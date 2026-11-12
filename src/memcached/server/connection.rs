use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use super::api;
use crate::ascii::Pending;
use crate::memcached::ffi::{
    ENGINE_ERROR_CODE_ENGINE_SUCCESS, EXTENSION_LOG_LEVEL_EXTENSION_LOG_WARNING,
};

pub type ResponseHandler =
    Option<unsafe extern "C" fn(cookie: *const c_void, nbytes: c_int, dta: *const c_char) -> bool>;

pub type IoCallback = unsafe extern "C" fn(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool;

/// A client connection, as the server names it to the extension.
#[derive(Clone, Copy)]
pub struct Connection {
    cookie: *const c_void,
    handler: ResponseHandler,
}

impl Connection {
    /// # Safety
    ///
    /// The server must be running `accept`, `execute`, `abort` or the io callback for `cookie`,
    /// with the `handler` it passed (`None` for `accept` and `abort`), and the connection must not
    /// outlive that call.
    pub const unsafe fn new(cookie: *const c_void, handler: ResponseHandler) -> Self {
        Self { cookie, handler }
    }

    pub const fn cookie(self) -> *const c_void {
        self.cookie
    }

    pub fn respond(self, reply: &[u8]) -> bool {
        let Some(handler) = self.handler else {
            return false;
        };
        let Ok(len) = c_int::try_from(reply.len()) else {
            return false;
        };
        // SAFETY: `new`'s caller guaranteed the connection takes a reply through `handler` now;
        // `reply` outlives the call.
        unsafe { handler(self.cookie, len, reply.as_ptr().cast()) }
    }

    /// Logs `message` as a warning about the connection.
    pub fn warn(self, message: &CStr) {
        // SAFETY: the server's own function; it returns its logger, alive for the process.
        let Some(log) = (unsafe { (api().get_logger)().as_ref() }).and_then(|logger| logger.log)
        else {
            return;
        };
        let level = EXTENSION_LOG_LEVEL_EXTENSION_LOG_WARNING;
        // SAFETY: `%s` takes the one C string that follows, which outlives the call.
        unsafe { log(level, self.cookie, c"%s\n".as_ptr(), message.as_ptr()) };
    }

    /// Keeps `pending` on the connection until [`Connection::take_kept`]; gives it back if the
    /// connection already keeps one.
    pub fn keep(self, pending: Pending) -> Result<(), Pending> {
        if !self.kept().is_null() {
            return Err(pending);
        }
        self.store(Box::into_raw(Box::new(pending)).cast());
        Ok(())
    }

    pub fn take_kept(self) -> Option<Pending> {
        let kept = self.kept().cast::<Pending>();
        if kept.is_null() {
            return None;
        }
        self.store(ptr::null_mut());
        // SAFETY: only `keep` stores on a connection, a leaked box, taken back once here.
        Some(*unsafe { Box::from_raw(kept) })
    }

    fn store(self, data: *mut c_void) {
        // SAFETY: `new`'s caller guaranteed the connection is live.
        unsafe { (api().store_engine_specific)(self.cookie, data) };
    }

    fn kept(self) -> *mut c_void {
        // SAFETY: `new`'s caller guaranteed the connection is live.
        unsafe { (api().get_engine_specific)(self.cookie) }
    }

    /// Call before handing the work to another thread, so a notify that comes first still counts.
    ///
    /// # Safety
    ///
    /// The server must be running `execute` for the connection. It waits only when `execute`
    /// returns, so a wait from elsewhere leaves the notify to reach a connection that may be gone.
    pub unsafe fn wait_for_io(self) -> IoWaiter {
        // SAFETY: the caller guarantees the connection is running `execute`.
        unsafe { (api().waitfor_io_complete)(self.cookie) };
        IoWaiter(self.cookie)
    }

    /// # Safety
    ///
    /// The server must be running `execute` for the connection, and `data` must live until `cb`
    /// runs.
    pub unsafe fn on_io_complete(self, cb: IoCallback, data: *mut c_void) {
        // SAFETY: the caller guarantees the connection is running `execute` and `data` outlives
        // `cb`.
        unsafe { (api().on_io_complete)(self.cookie, Some(cb), data) };
    }
}

/// Wakes the connection that made it with [`Connection::wait_for_io`] when dropped, on whichever
/// thread holds it then, so no path that loses it can leave the connection waiting.
pub struct IoWaiter(*const c_void);

// SAFETY: its one use, the notify in `drop`, calls the core's `notify_io_complete`, which takes
// the connection's thread lock.
unsafe impl Send for IoWaiter {}

impl Drop for IoWaiter {
    fn drop(&mut self) {
        // SAFETY: the connection waits for this one notify, so the server keeps it alive until then.
        unsafe { (api().notify_io_complete)(self.0, ENGINE_ERROR_CODE_ENGINE_SUCCESS) };
    }
}
