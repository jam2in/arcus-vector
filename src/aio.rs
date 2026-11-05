mod ops;
mod pool;
mod sweeper;

use std::borrow::Cow;
use std::os::raw::c_void;
use std::sync::{Arc, OnceLock};

use crate::error::Error;
use crate::guard;
use crate::memcached::server::{Connection, IoWaiter, ResponseHandler};
pub use pool::{NUM_LANES, Op, start, submit as begin};

/// A connection waiting for a lane to answer it from the io callback; dropped unanswered, it
/// answers an internal error.
pub struct Waiter {
    reply: Arc<OnceLock<Cow<'static, [u8]>>>,
    _wait: IoWaiter,
}

impl Waiter {
    pub fn new(conn: Connection) -> Self {
        let reply = Arc::new(OnceLock::new());
        // SAFETY: its callers, `cmd::execute` and `event::offer`, run in `execute`.
        let wait = unsafe { conn.wait_for_io() };
        let data = Arc::into_raw(Arc::clone(&reply));
        // SAFETY: as above, and `wake` takes back the count `data` holds.
        unsafe { conn.on_io_complete(wake, data.cast_mut().cast()) };
        Self { reply, _wait: wait }
    }

    /// Leaves `reply` for the io callback, then wakes the connection.
    pub fn answer(self, reply: Cow<'static, [u8]>) {
        let _ = self.reply.set(reply);
    }
}

unsafe extern "C" fn wake(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool {
    guard(
        || false,
        || {
            // SAFETY: `data` is the count from `Waiter::new`, and the core calls this once.
            let reply = unsafe { Arc::from_raw(data.cast::<OnceLock<Cow<'static, [u8]>>>()) };
            let reply = reply.get().map_or(Error::Internal.reply(), AsRef::as_ref);
            // SAFETY: the server is running the io callback for `cookie`, with `handler`.
            unsafe { Connection::new(cookie, handler).respond(reply) }
        },
    )
}

#[cfg(test)]
pub fn start_once() {
    pool::start_once();
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::*;
    use crate::index;
    use crate::memcached::server::fake;
    use crate::protocol::{self, Tokens};
    use crate::quant::Vector;

    #[test]
    fn begin_replies_from_the_io_callback_after_a_lane_notifies() {
        fake::install();
        pool::start_once();
        let (tx, rx) = channel();
        let conn = fake::Conn::new(7, &tx);
        let config = index::test_config(4, "cos", "f32");
        assert_eq!(index::create("aio", 0, &config), Ok(()));
        let tokens = Tokens::from_line("vop search aio vector 16 3");
        let mut request = protocol::parse(tokens.words()).unwrap();
        request.set_vector(Vector::F32(vec![0.0; 4]));
        begin(
            Op::new(request).unwrap(),
            Some(Waiter::new(conn.connection())),
        );
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 7);
        assert!(conn.reply().is_empty());
        assert!(conn.run_callback());
        assert_eq!(conn.reply(), b"VECTORS 0\r\nEND\r\n");
    }
}
