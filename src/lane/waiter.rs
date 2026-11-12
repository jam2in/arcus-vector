use std::borrow::Cow;
use std::os::raw::c_void;
use std::sync::{Arc, OnceLock};

use crate::error::Error;
use crate::guard;
use crate::memcached::server::{Connection, IoWaiter, ResponseHandler};

/// A connection waiting for a lane to answer it from the io callback; dropped unanswered, it
/// answers an internal error.
pub struct Waiter {
    reply: Arc<OnceLock<Cow<'static, [u8]>>>,
    _wait: IoWaiter,
}

impl Waiter {
    pub fn new(conn: Connection) -> Self {
        let reply = Arc::new(OnceLock::new());
        // SAFETY: its callers, `command::execute` and `event::offer`, run in `execute`.
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
