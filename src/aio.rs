mod pool;

use std::os::raw::c_void;

use crate::guard;
use crate::memcached::server::{Connection, ResponseHandler};
use pool::Task;
pub use pool::start;

/// The callback data; `wake` frees it.
struct Job {
    reply: &'static [u8],
}

/// # Safety
///
/// `conn` must be running `execute`.
pub unsafe fn begin(conn: Connection, reply: &'static [u8]) {
    let job = Box::into_raw(Box::new(Job { reply }));
    // SAFETY: `conn` is running `execute`.
    let waiter = unsafe { conn.wait_for_io() };
    // SAFETY: `conn` is running `execute`, and `wake` frees `job`.
    unsafe { conn.on_io_complete(wake, job.cast()) };
    pool::submit(pool::idlest(), Task::Notify(waiter));
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
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::*;
    use crate::memcached::server::fake;

    #[test]
    fn begin_replies_from_the_io_callback_after_a_lane_notifies() {
        fake::install();
        pool::start_once();
        let (tx, rx) = channel();
        let conn = fake::Conn::new(7, &tx);
        // SAFETY: a fake connection is always running `execute`.
        unsafe { begin(conn.connection(), b"NOT_SUPPORTED\r\n") };
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 7);
        assert!(conn.reply().is_empty());
        assert!(conn.run_callback());
        assert_eq!(conn.reply(), b"NOT_SUPPORTED\r\n");
    }
}
