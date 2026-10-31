use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::raw::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Barrier, OnceLock};

use crate::error::Error;
use crate::memcached::server::IoWaiter;
use crate::protocol::Request;

const NUM_LANES: usize = 32;

/// One command from a connection, which its lane runs and its io callback replies from.
pub struct Job {
    op: Op,
    /// Unset if its lane did not finish it.
    pub result: OnceLock<Result<Data, Error>>,
}

impl Job {
    pub fn new(op: Op) -> Arc<Self> {
        Arc::new(Self {
            op,
            result: OnceLock::new(),
        })
    }
}

/// What a command gives back for its reply; none does yet.
pub struct Data;

/// A job for a lane, and the connection to wake once it is done.
struct Task {
    job: Arc<Job>,
    waiter: IoWaiter,
}

pub enum Op {
    Search,
    Upsert { index: String, vkey: String },
    SetMeta { index: String, vkey: String },
    Delete { index: String, vkey: String },
}

impl TryFrom<Request<'_>> for Op {
    type Error = Error;

    fn try_from(request: Request<'_>) -> Result<Self, Error> {
        match request {
            Request::Search(_) => Ok(Self::Search),
            Request::Upsert(u) => Ok(Self::Upsert {
                index: u.index.into(),
                vkey: u.vkey.into(),
            }),
            Request::SetMeta(m) => Ok(Self::SetMeta {
                index: m.index.into(),
                vkey: m.vkey.into(),
            }),
            Request::Delete { index, vkey } => Ok(Self::Delete {
                index: index.into(),
                vkey: vkey.into(),
            }),
            _ => Err(Error::NotSupported),
        }
    }
}

impl Op {
    /// A search goes to the least busy lane; a write goes to its vkey's lane, so that writes on one
    /// vkey run in the order they came.
    fn lane(&self) -> &'static Lane {
        match self {
            Self::Search => idlest(),
            Self::Upsert { index, vkey }
            | Self::SetMeta { index, vkey }
            | Self::Delete { index, vkey } => lane_of(index, vkey),
        }
    }
}

impl Task {
    fn run(self) {
        let result = match self.job.op {
            Op::Search | Op::Upsert { .. } | Op::SetMeta { .. } | Op::Delete { .. } => {
                Err(Error::NotSupported)
            }
        };
        let _ = self.job.result.set(result);
        drop(self.waiter);
    }
}

struct Lane {
    tasks: Sender<Task>,
    /// Queued plus running jobs, to pick the least busy lane.
    load: AtomicUsize,
}

// Set at load, and again in the child of `-d`'s fork, where the lanes made at load are gone.
static POOL: AtomicPtr<[Lane; NUM_LANES]> = AtomicPtr::new(ptr::null_mut());

/// Starts the lanes, and has them started again when `-d` forks to daemonize.
pub fn start() -> bool {
    // SAFETY: `restart` is a plain fn that catches its own panics.
    spawn() && unsafe { pthread_atfork(None, None, Some(restart)) == 0 }
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

fn lane_of(index: &str, vkey: &str) -> &'static Lane {
    let mut hasher = DefaultHasher::new();
    (index, vkey).hash(&mut hasher);
    &lanes()[(hasher.finish() % NUM_LANES as u64) as usize]
}

/// Runs `job` on its lane, which leaves how it ended there, then drops `waiter`.
pub fn submit(job: Arc<Job>, waiter: IoWaiter) {
    send(job.op.lane(), Task { job, waiter });
}

fn send(lane: &Lane, task: Task) {
    lane.load.fetch_add(1, Ordering::Relaxed);
    // Fails only if the lane's thread is gone, and a lane never returns.
    let _ = lane.tasks.send(task);
}

fn run(lane: &Lane, tasks: &Receiver<Task>) {
    for task in tasks {
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
            let job = Job::new(Op::Search);
            let waiter = conn.connection().wait_for_io();
            send(&lanes()[3], Task { job, waiter });
        }
        let got: Vec<usize> = (0..100)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        assert_eq!(got, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn one_vkey_always_gets_one_lane() {
        start_once();
        assert!(ptr::eq(lane_of("idx", "a"), lane_of("idx", "a")));
        let spread = (0..1000)
            .map(|n| ptr::from_ref(lane_of("idx", &format!("v{n}"))))
            .collect::<HashSet<_>>();
        assert_eq!(spread.len(), NUM_LANES);
    }
}
