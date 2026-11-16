use std::borrow::Cow;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::raw::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use super::{Waiter, ops, sweeper};
use crate::error::Error;
use crate::filter::Filter;
use crate::memcached::engine::Elem;
use crate::protocol::{Request, Search};
use crate::quant::Vector;

pub const NUM_LANES: usize = 32;

/// An op for a lane, and the connection it answers: none for a change from replication or replay.
struct Task {
    op: Op,
    waiter: Option<Waiter>,
}

#[rustfmt::skip]
pub enum Op {
    Search { index: String, vector: Vector, k: usize, filter: Option<Filter>, with_meta: bool },
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
                vector,
                k,
                filter,
                with_meta,
                ..
            }) => Ok(Self::Search {
                index: index.into(),
                vector,
                k,
                filter,
                with_meta,
            }),
            _ => Err(Error::NotSupported),
        }
    }

    /// A search goes to the least busy lane. A change to an element goes to its vkey's lane, so
    /// that the changes to one vkey run in the order the engine made them; a create or a drop to
    /// its index's. A shutdown goes to every lane, from [`close`].
    fn lane(&self) -> Option<&'static Lane> {
        match self {
            Self::Search { .. } => Some(idlest()),
            Self::Insert { index, key, .. } | Self::Delete { index, key, .. } => {
                Some(lane_of(index, vkey_of(*key).as_deref()))
            }
            Self::Replace { index, new, .. } | Self::SetMeta { index, new, .. } => {
                Some(lane_of(index, vkey_of(*new).as_deref()))
            }
            Self::Create { index, .. } | Self::Drop { index, .. } => Some(lane_of(index, None)),
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

/// What a lane runs: a task, or a stop that keeps it in step with a create on another lane.
enum Job {
    Task(Task),
    /// Holds the lane until the create it waits on has run.
    Wait(Arc<Gate>),
    /// Lets the lanes waiting on its create go, once it runs or drops unrun.
    Open(Opener),
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn wait(&self) {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let _open = self
            .opened
            .wait_while(open, |open| !*open)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

struct Opener(Arc<Gate>);

impl Drop for Opener {
    fn drop(&mut self) {
        *self.0.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.0.opened.notify_all();
    }
}

struct Lane {
    jobs: Sender<Job>,
    /// Queued plus running jobs, to pick the least busy lane.
    load: AtomicUsize,
    closed: AtomicBool,
}

// Set at load, and again in the child of `-d`'s fork, where the lanes made at load are gone.
static POOL: AtomicPtr<[Lane; NUM_LANES]> = AtomicPtr::new(ptr::null_mut());

/// Starts the lanes, and has them started again when `-d` forks to daemonize; on failure, the
/// threads that did start are shut down.
pub fn start() -> bool {
    // SAFETY: `restart` is a plain fn that catches its own panics.
    let started = spawn() && unsafe { pthread_atfork(None, None, Some(restart)) == 0 };
    if !started {
        shutdown();
    }
    started
}

/// Has each lane end once the job it runs is done; the queued ones drop unrun, waking their
/// connections to an internal error. Does not wait for the lanes to end.
pub fn shutdown() {
    // SAFETY: a non-null `POOL` is a leaked array, never freed.
    if let Some(lanes) = unsafe { POOL.load(Ordering::Acquire).as_ref() } {
        lanes.iter().for_each(close);
    }
    sweeper::shutdown();
}

fn close(lane: &Lane) {
    lane.closed.store(true, Ordering::Release);
    let _ = lane.jobs.send(Job::Task(Task {
        op: Op::Shutdown,
        waiter: None,
    }));
}

/// Starts the lanes and the sweeper; `false` if a thread could not be made, leaving those that
/// were for [`shutdown`].
fn spawn() -> bool {
    let mut receivers = Vec::with_capacity(NUM_LANES);
    let lanes: &'static [Lane; NUM_LANES] = Box::leak(Box::new(std::array::from_fn(|_| {
        let (jobs, rx) = channel();
        receivers.push(rx);
        Lane {
            jobs,
            load: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        }
    })));
    // A fork must not catch a thread mid-start, holding a global lock (allocator, TLS) the child
    // needs; lane channels need no care, as the child builds a new pool.
    let (ready, started) = channel();
    let mut spawned = 0;
    for (n, (lane, rx)) in lanes.iter().zip(receivers).enumerate() {
        let ready = ready.clone();
        let thread = std::thread::Builder::new()
            .name(format!("arcus-vector-{n}"))
            .spawn(move || {
                let _ = ready.send(());
                run(lane, &rx);
            });
        if thread.is_err() {
            break;
        }
        spawned += 1;
    }
    let all = spawned == NUM_LANES && sweeper::spawn(&ready);
    for _ in 0..spawned + usize::from(all) {
        let _ = started.recv();
    }
    POOL.store(ptr::from_ref(lanes).cast_mut(), Ordering::Release);
    all
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

fn lane_of(index: &str, vkey: Option<&[u8]>) -> &'static Lane {
    let mut hasher = DefaultHasher::new();
    index.hash(&mut hasher);
    vkey.hash(&mut hasher);
    &lanes()[(hasher.finish() % NUM_LANES as u64) as usize]
}

/// Read while the engine raises the element's event, which keeps it alive.
fn vkey_of(elem: usize) -> Option<Vec<u8>> {
    Elem::new(elem).ok()?.field().map(<[u8]>::to_vec)
}

/// Runs `op` on its lane, which then answers `waiter`.
pub fn submit(op: Op, waiter: Option<Waiter>) -> bool {
    let Some(lane) = op.lane() else {
        return false;
    };
    let task = Task { op, waiter };
    if matches!(task.op, Op::Create { .. }) {
        create(lane, task)
    } else {
        send(lane, task)
    }
}

/// Holds every other lane until the create has run, so that the changes the engine makes after
/// it, on any lane, find its index.
fn create(lane: &'static Lane, task: Task) -> bool {
    // One create at a time, or two could each wait on a lane that waits on the other.
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(PoisonError::into_inner);
    let gate = Arc::new(Gate::default());
    for other in lanes().iter().filter(|other| !ptr::eq(*other, lane)) {
        post(other, Job::Wait(Arc::clone(&gate)));
    }
    let sent = send(lane, task);
    post(lane, Job::Open(Opener(gate)));
    sent
}

/// `false` only if the lane has ended; the task then drops, answering its connection an internal
/// error.
fn send(lane: &Lane, task: Task) -> bool {
    post(lane, Job::Task(task))
}

fn post(lane: &Lane, job: Job) -> bool {
    lane.load.fetch_add(1, Ordering::Relaxed);
    match lane.jobs.send(job) {
        Ok(()) => true,
        Err(_) => {
            lane.load.fetch_sub(1, Ordering::Relaxed);
            false
        }
    }
}

fn run(lane: &Lane, jobs: &Receiver<Job>) {
    for job in jobs {
        if lane.closed.load(Ordering::Acquire) {
            break;
        }
        match job {
            // A panicking job must not kill the lane, or its later jobs would never run.
            Job::Task(task) => drop(catch_unwind(AssertUnwindSafe(|| task.run()))),
            Job::Wait(gate) => gate.wait(),
            Job::Open(opener) => drop(opener),
        }
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
                filter: None,
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
        let (jobs, rx) = channel();
        let lane: &'static Lane = Box::leak(Box::new(Lane {
            jobs,
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
                    filter: None,
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
    fn one_vkey_always_gets_one_lane_and_the_vkeys_of_an_index_spread() {
        start_once();
        assert!(ptr::eq(
            lane_of("idx", Some(b"v")),
            lane_of("idx", Some(b"v"))
        ));
        let spread = (0..1000)
            .map(|n| ptr::from_ref(lane_of("idx", Some(format!("v{n}").as_bytes()))))
            .collect::<HashSet<_>>();
        assert_eq!(spread.len(), NUM_LANES);
    }

    #[test]
    fn a_lane_waits_until_the_create_before_it_has_run() {
        let (jobs, rx) = channel();
        let lane: &'static Lane = Box::leak(Box::new(Lane {
            jobs,
            load: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        }));
        let gate = Arc::new(Gate::default());
        post(lane, Job::Wait(Arc::clone(&gate)));
        let (done, ran) = channel();
        let waiting = std::thread::spawn(move || {
            run(lane, &rx);
            done.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(50));
        close(lane);
        assert!(ran.try_recv().is_err());
        drop(Opener(gate));
        ran.recv_timeout(Duration::from_secs(10)).unwrap();
        waiting.join().unwrap();
    }
}
