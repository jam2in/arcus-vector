use std::cell::UnsafeCell;
use std::os::raw::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, MutexGuard, PoisonError};

const WORKERS: usize = 32;

/// A link embedded in a job, so queueing a job never allocates.
pub struct Entry {
    next: UnsafeCell<*const Entry>,
    run: unsafe fn(*const Entry),
}

// SAFETY: `next` is only accessed under the lane mutex and `run` is a plain fn pointer.
unsafe impl Send for Entry {}
// SAFETY: through `&Entry` only `run` is read freely; `next` is touched only under the lane mutex.
unsafe impl Sync for Entry {}

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

struct Pool {
    lanes: [Lane; WORKERS],
}

// Set at load, and again in the child of `-d`'s fork, where the lanes made at load are gone.
static POOL: AtomicPtr<Pool> = AtomicPtr::new(ptr::null_mut());

/// Starts the lanes; `false` if a thread could not be made.
pub fn start() -> bool {
    let pool: &'static Pool = Box::leak(Box::new(Pool {
        lanes: std::array::from_fn(|_| Lane {
            queue: Mutex::new(List::new()),
            wake: Condvar::new(),
            load: AtomicUsize::new(0),
        }),
    }));
    // Wait for every lane to settle, so a fork never catches one mid-start holding a lock.
    let ready = Arc::new(Barrier::new(WORKERS + 1));
    for (n, lane) in pool.lanes.iter().enumerate() {
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
    POOL.store(ptr::from_ref(pool).cast_mut(), Ordering::Release);
    true
}

unsafe extern "C" {
    fn pthread_atfork(
        prepare: Option<unsafe extern "C" fn()>,
        parent: Option<unsafe extern "C" fn()>,
        child: Option<unsafe extern "C" fn()>,
    ) -> c_int;
}

pub fn restart_after_fork() -> bool {
    // SAFETY: `restart` is a plain fn that catches its own panics.
    unsafe { pthread_atfork(None, None, Some(restart)) == 0 }
}

extern "C" fn restart() {
    if !crate::guard(|| false, start) {
        // A job queued on a lane with no thread would leave its connection waiting forever.
        std::process::exit(1);
    }
}

#[expect(clippy::expect_used, reason = "`start` runs at load, before any job")]
fn pool() -> &'static Pool {
    // SAFETY: a non-null `POOL` is a leaked pool, never freed.
    unsafe { POOL.load(Ordering::Acquire).as_ref() }.expect("the lanes start at load")
}

#[cfg(test)]
pub fn start_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| assert!(start()));
}

fn queue(lane: &Lane) -> MutexGuard<'_, List> {
    lane.queue.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn idlest() -> &'static Lane {
    let load = |lane: &Lane| lane.load.load(Ordering::Relaxed);
    let lanes = &pool().lanes;
    lanes.iter().fold(&lanes[0], |idlest, lane| {
        if load(lane) < load(idlest) {
            lane
        } else {
            idlest
        }
    })
}

/// # Safety
///
/// `entry` must stay alive until its `run` is called and must not be in another list meanwhile.
pub unsafe fn submit(lane: &Lane, entry: *const Entry) {
    let mut queue = queue(lane);
    // SAFETY: the caller guarantees `entry` is alive and unlinked; the lane mutex is held.
    unsafe { queue.push_back(entry) };
    lane.load.fetch_add(1, Ordering::Relaxed);
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
        unsafe { submit(&pool().lanes[lane], &raw const (*raw).entry) };
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
    fn the_idlest_lane_runs_the_job() {
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
