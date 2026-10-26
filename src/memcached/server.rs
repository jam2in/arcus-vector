use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use super::ffi::{ENGINE_ERROR_CODE_ENGINE_SUCCESS, SERVER_CORE_API, SERVER_HANDLE_V1};

pub type ResponseHandler =
    Option<unsafe extern "C" fn(cookie: *const c_void, nbytes: c_int, dta: *const c_char) -> bool>;

pub type IoCallback = unsafe extern "C" fn(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool;

static SERVER: AtomicPtr<SERVER_HANDLE_V1> = AtomicPtr::new(ptr::null_mut());

pub fn set(server: *mut SERVER_HANDLE_V1) {
    SERVER.store(server, Ordering::Release);
}

fn core() -> Option<&'static SERVER_CORE_API> {
    let server = SERVER.load(Ordering::Acquire);
    if server.is_null() {
        return None;
    }
    // SAFETY: `server` is the core's static handle, set once at initialization.
    let core = unsafe { (*server).core };
    // SAFETY: a non-null `core` is the core's static API table.
    unsafe { core.as_ref() }
}

/// A client connection, as the server names it to the extension.
#[derive(Clone, Copy)]
pub struct Connection {
    cookie: *const c_void,
    handler: ResponseHandler,
}

impl Connection {
    /// # Safety
    ///
    /// `handler`, if present, must accept `cookie`.
    pub const unsafe fn new(cookie: *const c_void, handler: ResponseHandler) -> Self {
        Self { cookie, handler }
    }

    /// # Safety
    ///
    /// The connection must be running `execute` or its io callback.
    pub unsafe fn respond(self, reply: &[u8]) -> bool {
        let Some(handler) = self.handler else {
            return false;
        };
        let Ok(len) = c_int::try_from(reply.len()) else {
            return false;
        };
        // SAFETY: `new`'s caller guaranteed `handler` accepts the connection, which the caller
        // guarantees can take a reply now; `reply` outlives the call.
        unsafe { handler(self.cookie, len, reply.as_ptr().cast()) }
    }

    /// Call before handing the work to another thread, so a notify that comes first still counts.
    ///
    /// # Safety
    ///
    /// The connection must be running `execute`.
    #[expect(clippy::expect_used, reason = "the core API is to be checked at load")]
    pub unsafe fn wait_for_io(self) {
        let waitfor = core()
            .and_then(|core| core.waitfor_io_complete)
            .expect("the core supports waitfor_io_complete");
        // SAFETY: the caller guarantees the connection is live.
        unsafe { waitfor(self.cookie) };
    }

    /// # Safety
    ///
    /// The connection must be waiting from [`Self::wait_for_io`].
    #[expect(clippy::expect_used, reason = "the core API is to be checked at load")]
    pub unsafe fn notify_io(self) {
        let notify = core()
            .and_then(|core| core.notify_io_complete)
            .expect("the core supports notify_io_complete");
        // SAFETY: the caller guarantees the connection is waiting, so live.
        unsafe { notify(self.cookie, ENGINE_ERROR_CODE_ENGINE_SUCCESS) };
    }

    /// # Safety
    ///
    /// The connection must be running `execute`, and `data` must live until `cb` runs.
    #[expect(clippy::expect_used, reason = "the core API is to be checked at load")]
    pub unsafe fn on_io_complete(self, cb: IoCallback, data: *mut c_void) {
        let register = core()
            .and_then(|core| core.on_io_complete)
            .expect("the core supports on_io_complete");
        // SAFETY: the caller guarantees the connection is live and `data` outlives `cb`.
        unsafe { register(self.cookie, Some(cb), data) };
    }
}
