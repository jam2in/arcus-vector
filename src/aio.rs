mod pool;

use std::os::raw::c_void;

use crate::error::Error;
use crate::guard;
use crate::memcached::server::{Connection, ResponseHandler};
use crate::protocol::Request;
pub use pool::start;
use pool::{Job, Op};

/// Hands `request`, with the vector it carries, to a lane, to be replied to from the io callback;
/// an error is for `execute` to reply with.
pub fn begin(
    conn: Connection,
    request: Request<'_>,
    vector: Option<Vec<f32>>,
) -> Result<(), Error> {
    let op = Op::try_from(request)?;
    let mut job = Job::new(op, vector);
    let waiter = conn.wait_for_io();
    let data: *mut Job = job.as_mut();
    // SAFETY: `wake` takes back the box, which the lane never frees, once the lane is done with it.
    unsafe { conn.on_io_complete(wake, data.cast()) };
    pool::submit(job, waiter);
    Ok(())
}

unsafe extern "C" fn wake(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool {
    guard(
        || false,
        || {
            // SAFETY: `data` is the box from `begin`; the core calls this once, after the lane.
            let job = unsafe { Box::from_raw(data.cast::<Job>()) };
            let reply = match job.result {
                Err(e) => e.reply(),
                // No command succeeds yet.
                Ok(()) => return false,
            };
            // SAFETY: the server is running the io callback for `cookie`, with `handler`.
            unsafe { Connection::new(cookie, handler).respond(reply) }
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
        let request = Request::Delete {
            index: "idx",
            vkey: "v",
        };
        assert_eq!(begin(conn.connection(), request, None), Ok(()));
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 7);
        assert!(conn.reply().is_empty());
        assert!(conn.run_callback());
        assert_eq!(conn.reply(), b"NOT_SUPPORTED\r\n");
    }
}
