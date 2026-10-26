mod pool;

use std::alloc::{Layout, alloc};
use std::os::raw::c_void;

use crate::error::Error;
use crate::guard;
use crate::memcached::server::{Connection, ResponseHandler};
use pool::Entry;
pub use pool::start;

/// Reached from both the lane queue and the callback data; `wake` frees it after the lane.
#[repr(C)]
struct Job {
    /// The first field, so its address is the job's.
    entry: Entry,
    conn: Connection,
    reply: &'static [u8],
}

impl Job {
    /// Like `Box::new`, but a failed allocation is `None` instead of an abort.
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "`alloc` aligns the memory for the layout it is given"
    )]
    fn new(conn: Connection, reply: &'static [u8]) -> Option<*mut Self> {
        // SAFETY: `Job` is not zero-sized.
        let raw = unsafe { alloc(Layout::new::<Self>()) }.cast::<Self>();
        if raw.is_null() {
            return None;
        }
        let job = Self {
            entry: Entry::new(run),
            conn,
            reply,
        };
        // SAFETY: `raw` is a fresh allocation with `Job`'s layout.
        unsafe { raw.write(job) };
        Some(raw)
    }
}

/// # Safety
///
/// `conn` must be running `execute`.
pub unsafe fn begin(conn: Connection, reply: &'static [u8]) -> bool {
    let Some(job) = Job::new(conn, reply) else {
        // SAFETY: the caller guarantees `conn` is running `execute`.
        return unsafe { conn.respond(Error::OutOfMemory.reply()) };
    };
    // Before queueing, so a lane that finishes first is still counted.
    // SAFETY: `conn` is running `execute`.
    unsafe { conn.wait_for_io() };
    // SAFETY: `conn` is running `execute`, and `wake` frees `job`.
    unsafe { conn.on_io_complete(wake, job.cast()) };
    // SAFETY: `job` stays alive until `wake`, which runs only after the lane's notify.
    unsafe { pool::submit(pool::idlest(), &raw const (*job).entry) };
    true
}

unsafe fn run(entry: *const Entry) {
    // SAFETY: `entry` is the first field of a `#[repr(C)]` `Job`, alive until `wake`.
    let conn = unsafe { (*entry.cast::<Job>()).conn };
    // SAFETY: the connection waits for this notify, so the server keeps it alive until then.
    unsafe { conn.notify_io() };
}

unsafe extern "C" fn wake(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool {
    guard(
        || false,
        || {
            // SAFETY: `data` is the job from `begin`; the core calls this once, after the lane.
            let job = unsafe { Box::from_raw(data.cast::<Job>()) };
            // SAFETY: the server passes the handler for `cookie`, which is in its io callback.
            unsafe { Connection::new(cookie, handler).respond(job.reply) }
        },
    )
}

#[cfg(test)]
mod tests {
    use std::os::raw::{c_char, c_int};

    use super::*;

    /// # Safety
    ///
    /// `cookie` must be an unshared `Vec<u8>`, and `dta` must hold `nbytes` bytes.
    unsafe extern "C" fn record(cookie: *const c_void, nbytes: c_int, dta: *const c_char) -> bool {
        // SAFETY: the caller guarantees both, per `# Safety`.
        let (reply, bytes) = unsafe {
            (
                &mut *cookie.cast_mut().cast::<Vec<u8>>(),
                std::slice::from_raw_parts(dta.cast::<u8>(), nbytes as usize),
            )
        };
        reply.extend_from_slice(bytes);
        true
    }

    #[test]
    fn wake_writes_the_reply_and_frees_the_job() {
        let mut reply: Vec<u8> = Vec::new();
        let cookie = (&raw mut reply).cast_const().cast();
        // SAFETY: `record` takes any `Vec<u8>` cookie, as `reply` is.
        let conn = unsafe { Connection::new(cookie, Some(record)) };
        let job = Job::new(conn, b"NOT_SUPPORTED\r\n").unwrap();
        // SAFETY: `job` is ours, and `record` gets `reply` back as its cookie.
        let ok = unsafe { wake(cookie, job.cast(), Some(record)) };
        assert!(ok);
        assert_eq!(reply, b"NOT_SUPPORTED\r\n");
    }
}
