use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::memcached::engine::Elem;

/// Run one at a time in the order sent, so a release waits for every search that began before it.
enum Sweep {
    Release(usize),
    /// Holds back the releases after it until its search drops the [`Reading`].
    Block(Receiver<()>),
    /// Wakes the sweeper to see it is closed.
    Shutdown,
}

struct Sweeper {
    events: Sender<Sweep>,
    closed: AtomicBool,
}

// Set with the lanes, at load and again in the child of `-d`'s fork.
static SWEEPER: AtomicPtr<Sweeper> = AtomicPtr::new(ptr::null_mut());

/// Held by a search until it is done with what it found.
pub struct Reading {
    _done: Sender<()>,
}

/// Keeps the elements a search may find from being released until the [`Reading`] drops; taken
/// before the search reads its graph. `None` if the sweeper has not started or has ended.
pub fn block() -> Option<Reading> {
    let (done, wait) = channel();
    send(Sweep::Block(wait)).then_some(Reading { _done: done })
}

/// Gives back the reference on an element taken out of its graph, once the searches under way
/// are done with it. Dropped if the sweeper has ended.
pub fn release(elem_ptr: usize) {
    send(Sweep::Release(elem_ptr));
}

fn send(sweep: Sweep) -> bool {
    // SAFETY: a non-null `SWEEPER` is leaked, never freed.
    unsafe { SWEEPER.load(Ordering::Acquire).as_ref() }
        .is_some_and(|sweeper| sweeper.events.send(sweep).is_ok())
}

fn new() -> (&'static Sweeper, Receiver<Sweep>) {
    let (events, rx) = channel();
    let sweeper = Box::leak(Box::new(Sweeper {
        events,
        closed: AtomicBool::new(false),
    }));
    (sweeper, rx)
}

/// Starts the sweeper's thread, which sends on `ready` first; `false` if it could not be made.
pub(super) fn spawn(ready: &Sender<()>) -> bool {
    let (sweeper, events) = new();
    let ready = ready.clone();
    let spawned = std::thread::Builder::new()
        .name("arcus-vector-sweeper".into())
        .spawn(move || {
            let _ = ready.send(());
            run(sweeper, &events);
        });
    if spawned.is_err() {
        return false;
    }
    SWEEPER.store(ptr::from_ref(sweeper).cast_mut(), Ordering::Release);
    true
}

/// Has the sweeper end once the sweep it runs is done; the queued releases drop.
pub(super) fn shutdown() {
    // SAFETY: a non-null `SWEEPER` is leaked, never freed.
    if let Some(sweeper) = unsafe { SWEEPER.load(Ordering::Acquire).as_ref() } {
        close(sweeper);
    }
}

fn close(sweeper: &Sweeper) {
    sweeper.closed.store(true, Ordering::Release);
    let _ = sweeper.events.send(Sweep::Shutdown);
}

fn run(sweeper: &Sweeper, events: &Receiver<Sweep>) {
    for sweep in events {
        if sweeper.closed.load(Ordering::Acquire) {
            break;
        }
        match sweep {
            Sweep::Release(elem_ptr) => {
                if let Ok(elem) = Elem::new(elem_ptr) {
                    elem.release();
                }
            }
            // Its search drops the sender when done, panicking or not.
            Sweep::Block(wait) => while wait.recv().is_ok() {},
            Sweep::Shutdown => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_block_holds_back_what_follows_until_its_search_is_done() {
        let (sweeper, rx) = new();
        let handle = std::thread::spawn(move || run(sweeper, &rx));
        let (done, wait) = channel();
        let _ = sweeper.events.send(Sweep::Block(wait));
        // A rendezvous: a send gets through only while the sweeper waits on it.
        let (probe, next) = sync_channel(0);
        let _ = sweeper.events.send(Sweep::Block(next));
        std::thread::sleep(Duration::from_millis(50));
        assert!(probe.try_send(()).is_err());
        drop(done);
        assert!(probe.send(()).is_ok());
        drop(probe);
        close(sweeper);
        handle.join().unwrap();
    }

    #[test]
    fn a_closed_sweeper_ends() {
        let (sweeper, rx) = new();
        close(sweeper);
        std::thread::spawn(move || run(sweeper, &rx))
            .join()
            .unwrap();
        assert!(sweeper.events.send(Sweep::Shutdown).is_err());
    }
}
