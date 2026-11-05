use std::borrow::Cow;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::raw::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Barrier};

use super::{Waiter, ops};
use crate::error::Error;
use crate::protocol::{Request, Search, Source};
use crate::quant::Vector;

pub const NUM_LANES: usize = 32;

/// An op for a lane, and the connection it answers: none for a change from replication or replay.
struct Task {
    op: Op,
    waiter: Option<Waiter>,
}

#[rustfmt::skip]
pub enum Op {
    Search { index: String, vector: Vector, k: usize, with_meta: bool },
    Insert { index: String, map: usize, key: usize },
    Replace { index: String, map: usize, old: usize, new: usize },
    /// A replace from `vop setmeta`, answered as a store.
    SetMeta { index: String, map: usize, old: usize, new: usize },
    Delete { index: String, map: usize, key: usize },
    Create { index: String, map: usize, config: Vec<u8> },
    Drop { index: String, map: usize },
    /// Wakes an idle lane to see it is closed.
    Shutdown,
}

impl Op {
    /// A command's search; a write's op comes from the element event it raises.
    pub fn new(request: Request<'_>) -> Result<Self, Error> {
        match request {
            Request::Search(Search {
                index,
                source: Source::Vector { .. },
                vector,
                k,
                filter_bytes: None,
                with_meta,
            }) => Ok(Self::Search {
                index: index.into(),
                vector,
                k,
                with_meta,
            }),
            _ => Err(Error::NotSupported),
        }
    }

    /// A search goes to the least busy lane; a change goes to its index's lane, so that changes to
    /// one index run in the order the engine made them. A shutdown goes to every lane, from
    /// [`close`].
    fn lane(&self) -> Option<&'static Lane> {
        match self {
            Self::Search { .. } => Some(idlest()),
            Self::Insert { index, .. }
            | Self::Replace { index, .. }
            | Self::SetMeta { index, .. }
            | Self::Delete { index, .. }
            | Self::Create { index, .. }
            | Self::Drop { index, .. } => Some(lane_of(index)),
            Self::Shutdown => None,
        }
    }
}

impl Task {
    fn run(self) {
        let reply = ops::run(&self.op).unwrap_or_else(|e| Cow::Borrowed(e.reply()));
        if let Some(waiter) = self.waiter {
            waiter.answer(reply);
        }
    }
}

struct Lane {
    tasks: Sender<Task>,
    /// Queued plus running jobs, to pick the least busy lane.
    load: AtomicUsize,
    closed: AtomicBool,
}

// Set at load, and again in the child of `-d`'s fork, where the lanes made at load are gone.
static POOL: AtomicPtr<[Lane; NUM_LANES]> = AtomicPtr::new(ptr::null_mut());

/// Starts the lanes, and has them started again when `-d` forks to daemonize.
pub fn start() -> bool {
    // SAFETY: `restart` is a plain fn that catches its own panics.
    spawn() && unsafe { pthread_atfork(None, None, Some(restart)) == 0 }
}

/// Has each lane end once the job it runs is done; the queued ones drop unrun, waking their
/// connections to an internal error. Does not wait for the lanes to end.
#[expect(dead_code, reason = "nothing closes the lanes yet")]
pub fn shutdown() {
    // SAFETY: a non-null `POOL` is a leaked array, never freed.
    if let Some(lanes) = unsafe { POOL.load(Ordering::Acquire).as_ref() } {
        lanes.iter().for_each(close);
    }
}

fn close(lane: &Lane) {
    lane.closed.store(true, Ordering::Release);
    let _ = lane.tasks.send(Task {
        op: Op::Shutdown,
        waiter: None,
    });
}

/// `false` if a thread could not be made.
fn spawn() -> bool {
    let mut receivers = Vec::with_capacity(NUM_LANES);
    let lanes: &'static [Lane; NUM_LANES] = Box::leak(Box::new(std::array::from_fn(|_| {
        let (tasks, rx) = channel();
        receivers.push(rx);
        Lane {
            tasks,
            load: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        }
    })));
    // A fork must not catch a lane mid-start, holding a global lock (allocator, TLS) the child
    // needs; lane channels need no care, as the child builds a new pool.
    let ready = Arc::new(Barrier::new(NUM_LANES + 1));
    for (n, (lane, rx)) in lanes.iter().zip(receivers).enumerate() {
        let ready = Arc::clone(&ready);
        let spawned = std::thread::Builder::new()
            .name(format!("arcus-vector-{n}"))
            .spawn(move || {
                ready.wait();
                run(lane, &rx);
            });
        if spawned.is_err() {
            return false;
        }
    }
    ready.wait();
    POOL.store(ptr::from_ref(lanes).cast_mut(), Ordering::Release);
    true
}

unsafe extern "C" {
    fn pthread_atfork(
        prepare: Option<unsafe extern "C" fn()>,
        parent: Option<unsafe extern "C" fn()>,
        child: Option<unsafe extern "C" fn()>,
    ) -> c_int;
}

/// Runs in the child when `-d` daemonizes; the lanes started at load did not come with the fork.
extern "C" fn restart() {
    if !crate::guard(|| false, spawn) {
        // A job queued on a lane with no thread would leave its connection waiting forever.
        std::process::exit(1);
    }
}

#[expect(clippy::expect_used, reason = "`start` runs at load, before any job")]
fn lanes() -> &'static [Lane; NUM_LANES] {
    // SAFETY: a non-null `POOL` is a leaked array, never freed.
    unsafe { POOL.load(Ordering::Acquire).as_ref() }.expect("the lanes start at load")
}

#[cfg(test)]
pub fn start_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| assert!(spawn()));
}

fn idlest() -> &'static Lane {
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    let lanes = lanes();
    let start = NEXT.fetch_add(1, Ordering::Relaxed) % NUM_LANES;
    let (head, tail) = lanes.split_at(start);
    let mut idlest = &lanes[start];
    let mut min = idlest.load.load(Ordering::Relaxed);
    for lane in tail.iter().skip(1).chain(head) {
        if min == 0 {
            break;
        }
        let load = lane.load.load(Ordering::Relaxed);
        if load < min {
            idlest = lane;
            min = load;
        }
    }
    idlest
}

fn lane_of(index: &str) -> &'static Lane {
    let mut hasher = DefaultHasher::new();
    index.hash(&mut hasher);
    &lanes()[(hasher.finish() % NUM_LANES as u64) as usize]
}

/// Runs `op` on its lane, which then answers `waiter`.
pub fn submit(op: Op, waiter: Option<Waiter>) -> bool {
    let Some(lane) = op.lane() else {
        return false;
    };
    send(lane, Task { op, waiter })
}

/// `false` only if the lane has ended; the task then drops, answering its connection an internal
/// error.
fn send(lane: &Lane, task: Task) -> bool {
    lane.load.fetch_add(1, Ordering::Relaxed);
    match lane.tasks.send(task) {
        Ok(()) => true,
        Err(_) => {
            lane.load.fetch_sub(1, Ordering::Relaxed);
            false
        }
    }
}

fn run(lane: &Lane, tasks: &Receiver<Task>) {
    for task in tasks {
        if lane.closed.load(Ordering::Acquire) {
            break;
        }
        // A panicking job must not kill the lane, or its later jobs would never run.
        let _ = catch_unwind(AssertUnwindSafe(|| task.run()));
        lane.load.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use super::*;
    use crate::memcached::server::fake;

    #[test]
    fn ops_on_one_lane_run_in_submit_order() {
        fake::install();
        start_once();
        let (tx, rx) = channel();
        let conns: Vec<_> = (0..100).map(|id| fake::Conn::new(id, &tx)).collect();
        for conn in &conns {
            let op = Op::Search {
                index: "idx".into(),
                vector: Vector::default(),
                k: 1,
                with_meta: false,
            };
            let waiter = Some(Waiter::new(conn.connection()));
            send(&lanes()[3], Task { op, waiter });
        }
        let got: Vec<usize> = (0..100)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        assert_eq!(got, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn a_closed_lane_ends_and_drops_the_queued_jobs_unrun() {
        fake::install();
        let (tasks, rx) = channel();
        let lane: &'static Lane = Box::leak(Box::new(Lane {
            tasks,
            load: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        }));
        let (tx, woken) = channel();
        let conns: Vec<_> = (0..5).map(|id| fake::Conn::new(id, &tx)).collect();
        for conn in &conns {
            let task = Task {
                op: Op::Search {
                    index: "idx".into(),
                    vector: Vector::default(),
                    k: 1,
                    with_meta: false,
                },
                waiter: Some(Waiter::new(conn.connection())),
            };
            assert!(send(lane, task));
        }
        close(lane);
        std::thread::spawn(move || run(lane, &rx)).join().unwrap();
        let mut got: Vec<usize> = woken.try_iter().collect();
        got.sort_unstable();
        assert_eq!(got, (0..5).collect::<Vec<_>>());
        for conn in &conns {
            assert!(conn.run_callback());
            assert_eq!(conn.reply(), Error::Internal.reply());
        }
        assert!(!send(
            lane,
            Task {
                op: Op::Drop {
                    index: "idx".into(),
                    map: 0,
                },
                waiter: None,
            }
        ));
    }

    #[test]
    fn one_index_always_gets_one_lane() {
        start_once();
        assert!(ptr::eq(lane_of("idx"), lane_of("idx")));
        let spread = (0..1000)
            .map(|n| ptr::from_ref(lane_of(&format!("idx{n}"))))
            .collect::<HashSet<_>>();
        assert_eq!(spread.len(), NUM_LANES);
    }
}
