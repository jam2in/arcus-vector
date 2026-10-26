use std::cell::UnsafeCell;
use std::os::raw::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, MutexGuard, PoisonError};

const NUM_LANES: usize = 32;

/// A link embedded in a job, so queueing a job never allocates.
pub struct Entry {
    next: UnsafeCell<*const Self>,
    run: unsafe fn(*const Self),
}

impl Entry {
    pub const fn new(run: unsafe fn(*const Self)) -> Self {
        Self {
            next: UnsafeCell::new(ptr::null()),
            run,
        }
    }
}

struct List {
    head: *const Entry,
    tail: *const Entry,
}

// SAFETY: a `List` is only reached through its lane's mutex, and its entries stay alive until run.
unsafe impl Send for List {}

impl List {
    const fn new() -> Self {
        Self {
            head: ptr::null(),
            tail: ptr::null(),
        }
    }

    /// # Safety
    ///
    /// `entry` must stay alive until run and must not be in any list.
    unsafe fn push_back(&mut self, entry: *const Entry) {
        // SAFETY: `entry` is alive and unlinked, and `&mut self` holds the lane mutex.
        unsafe { *(*entry).next.get() = ptr::null() };
        if self.tail.is_null() {
            self.head = entry;
        } else {
            // SAFETY: `tail` is in this list, so it is alive, and the lane mutex is held.
            unsafe { *(*self.tail).next.get() = entry };
        }
        self.tail = entry;
    }

    fn pop_front(&mut self) -> Option<*const Entry> {
        if self.head.is_null() {
            return None;
        }
        let entry = self.head;
        // SAFETY: `entry` is this list's head, so it is alive, and the lane mutex is held.
        self.head = unsafe { *(*entry).next.get() };
        if self.head.is_null() {
            self.tail = ptr::null();
        }
        Some(entry)
    }
}

pub struct Lane {
    queue: Mutex<List>,
    wake: Condvar,
    /// Queued plus running jobs, read without the lock to pick the least busy lane.
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
    let lanes: &'static [Lane; NUM_LANES] = Box::leak(Box::new(std::array::from_fn(|_| Lane {
        queue: Mutex::new(List::new()),
        wake: Condvar::new(),
        load: AtomicUsize::new(0),
    })));
    // A fork must not catch a lane mid-start, holding a global lock (allocator, TLS) the child
    // needs; lane locks need no care, as the child builds a new pool.
    let ready = Arc::new(Barrier::new(NUM_LANES + 1));
    for (n, lane) in lanes.iter().enumerate() {
        let ready = Arc::clone(&ready);
        let spawned = std::thread::Builder::new()
            .name(format!("arcus-vector-{n}"))
            .spawn(move || {
                ready.wait();
                run(lane);
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

fn queue(lane: &Lane) -> MutexGuard<'_, List> {
    lane.queue.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn idlest() -> &'static Lane {
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

/// # Safety
///
/// `entry` must stay alive and in no other list until its `run` starts, which may free it.
pub unsafe fn submit(lane: &Lane, entry: *const Entry) {
    lane.load.fetch_add(1, Ordering::Relaxed);
    let mut queue = queue(lane);
    // SAFETY: the caller guarantees `entry` is alive and unlinked; the lane mutex is held.
    unsafe { queue.push_back(entry) };
    drop(queue);
    lane.wake.notify_one();
}

fn run(lane: &Lane) {
    loop {
        let entry = {
            let mut queue = queue(lane);
            loop {
                if let Some(entry) = queue.pop_front() {
                    break entry;
                }
                queue = lane
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        // A panicking job must not kill the lane, or its later jobs would never run.
        // SAFETY: `entry` was just popped, so its submitter keeps it alive until this `run`.
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { ((*entry).run)(entry) }));
        lane.load.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Sender, channel};
    use std::time::Duration;

    use super::*;

    #[repr(C)]
    struct Probe {
        entry: Entry,
        f: Box<dyn FnOnce() + Send>,
    }

    unsafe fn run_probe(entry: *const Entry) {
        // SAFETY: `entry` is the first field of a `#[repr(C)]` `Probe` leaked by `probe`, run once.
        let probe = unsafe { Box::from_raw(entry.cast_mut().cast::<Probe>()) };
        (probe.f)();
    }

    fn probe(lane: usize, f: impl FnOnce() + Send + 'static) {
        start_once();
        let raw = Box::into_raw(Box::new(Probe {
            entry: Entry::new(run_probe),
            f: Box::new(f),
        }));
        // SAFETY: the leaked `Probe` stays alive and unlinked until `run_probe` frees it.
        unsafe { submit(&lanes()[lane], &raw const (*raw).entry) };
    }

    fn send(tx: &Sender<usize>, n: usize) -> impl FnOnce() + Send + 'static {
        let tx = tx.clone();
        move || {
            let _ = tx.send(n);
        }
    }

    #[test]
    fn jobs_on_one_lane_run_in_order() {
        let (tx, rx) = channel();
        for n in 0..100 {
            probe(3, send(&tx, n));
        }
        let got: Vec<usize> = (0..100)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        assert_eq!(got, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn a_busy_lane_keeps_taking_jobs() {
        let (hold_tx, hold_rx) = channel::<()>();
        let (started_tx, started_rx) = channel::<()>();
        probe(5, move || {
            let _ = started_tx.send(());
            let _ = hold_rx.recv();
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        let (tx, rx) = channel();
        for n in 0..1000 {
            probe(5, send(&tx, n));
        }
        drop(hold_tx);
        for _ in 0..1000 {
            rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
    }

    #[test]
    fn a_submitted_job_runs() {
        let (tx, rx) = channel();
        let raw = Box::into_raw(Box::new(Probe {
            entry: Entry::new(run_probe),
            f: Box::new(send(&tx, 7)),
        }));
        start_once();
        let lane = idlest();
        // SAFETY: the leaked `Probe` stays alive and unlinked until `run_probe` frees it.
        unsafe { submit(lane, &raw const (*raw).entry) };
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 7);
    }

    #[test]
    fn the_list_pops_in_push_order() {
        unsafe fn nothing(_: *const Entry) {}
        let entries = [
            Entry::new(nothing),
            Entry::new(nothing),
            Entry::new(nothing),
        ];
        let mut list = List::new();
        for entry in &entries {
            // SAFETY: each entry outlives `list` and is pushed once.
            unsafe { list.push_back(entry) };
        }
        for entry in &entries {
            assert_eq!(list.pop_front(), Some(ptr::from_ref(entry)));
        }
        assert_eq!(list.pop_front(), None);
    }
}
