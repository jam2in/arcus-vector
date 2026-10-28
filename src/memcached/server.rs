use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;

use super::ffi::{
    AIO_CALLBACK, ENGINE_ERROR_CODE, ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_EVENT_TYPE,
    ENGINE_HANDLE, EVENT_CALLBACK, EXTENSION_ASCII_PROTOCOL_DESCRIPTOR,
    EXTENSION_LOGGER_DESCRIPTOR, SERVER_HANDLE_V1, VERSION, extension_type_t,
    extension_type_t_EXTENSION_ASCII_PROTOCOL, rel_time_t, time_t,
};

pub type ResponseHandler =
    Option<unsafe extern "C" fn(cookie: *const c_void, nbytes: c_int, dta: *const c_char) -> bool>;

pub type IoCallback = unsafe extern "C" fn(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool;

type GetLogger = unsafe extern "C" fn() -> *mut EXTENSION_LOGGER_DESCRIPTOR;

#[expect(dead_code, reason = "the commands that use the rest are not in yet")]
struct Api {
    get_logger: GetLogger,
    waitfor_io_complete: unsafe extern "C" fn(cookie: *const c_void),
    notify_io_complete: unsafe extern "C" fn(cookie: *const c_void, status: ENGINE_ERROR_CODE),
    on_io_complete:
        unsafe extern "C" fn(cookie: *const c_void, cb: AIO_CALLBACK, cb_data: *mut c_void),
    realtime: unsafe extern "C" fn(exptime: time_t) -> rel_time_t,
    store_engine_specific: unsafe extern "C" fn(cookie: *const c_void, engine_data: *mut c_void),
    get_engine_specific: unsafe extern "C" fn(cookie: *const c_void) -> *mut c_void,
    register_extension:
        unsafe extern "C" fn(type_: extension_type_t, extension: *mut c_void) -> bool,
    register_callback: unsafe extern "C" fn(
        engine: *mut ENGINE_HANDLE,
        type_: ENGINE_EVENT_TYPE,
        cb: EVENT_CALLBACK,
        cb_data: *const c_void,
    ),
}

static API: OnceLock<Api> = OnceLock::new();

/// `false` if this server is not the version arcus-vector was built for or lacks what it uses.
pub fn set(server: &'static SERVER_HANDLE_V1) -> bool {
    // A second `-X` of this library fails here, before its descriptor is registered twice.
    Api::new(server).is_some_and(|api| API.set(api).is_ok())
}

impl Api {
    fn new(server: &SERVER_HANDLE_V1) -> Option<Self> {
        // SAFETY: a non-null `core` of the server handle is its static core API.
        let core = unsafe { server.core.as_ref() }?;
        if !built_for(core.server_version?) {
            return None;
        }
        // SAFETY: the other non-null tables of the server handle are its static APIs.
        let (log, extension, callback) = unsafe {
            (
                server.log.as_ref()?,
                server.extension.as_ref()?,
                server.callback.as_ref()?,
            )
        };
        Some(Self {
            get_logger: log.get_logger?,
            waitfor_io_complete: core.waitfor_io_complete?,
            notify_io_complete: core.notify_io_complete?,
            on_io_complete: core.on_io_complete?,
            realtime: core.realtime?,
            store_engine_specific: core.store_engine_specific?,
            get_engine_specific: core.get_engine_specific?,
            register_extension: extension.register_extension?,
            register_callback: callback.register_callback?,
        })
    }
}

fn built_for(server_version: unsafe extern "C" fn() -> *const c_char) -> bool {
    // SAFETY: the server's own function; it returns its static version string.
    let running = unsafe { server_version() };
    // SAFETY: a non-null version is the server's static NUL-terminated string.
    !running.is_null() && unsafe { CStr::from_ptr(running) }.to_bytes_with_nul() == VERSION
}

#[expect(
    clippy::expect_used,
    reason = "`set` checks the API at load, before any command"
)]
fn api() -> &'static Api {
    API.get().expect("the API is checked at load")
}

/// # Safety
///
/// `descriptor` must stay valid while registered; the server links it through `next`.
pub unsafe fn register_ascii(descriptor: *mut EXTENSION_ASCII_PROTOCOL_DESCRIPTOR) -> bool {
    let kind = extension_type_t_EXTENSION_ASCII_PROTOCOL;
    // SAFETY: the caller guarantees `descriptor` outlives its registration.
    unsafe { (api().register_extension)(kind, descriptor.cast()) }
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
    pub unsafe fn wait_for_io(self) {
        // SAFETY: the caller guarantees the connection is live.
        unsafe { (api().waitfor_io_complete)(self.cookie) };
    }

    /// # Safety
    ///
    /// The connection must be waiting from [`Self::wait_for_io`].
    pub unsafe fn notify_io(self) {
        // SAFETY: the caller guarantees the connection is waiting, so live.
        unsafe { (api().notify_io_complete)(self.cookie, ENGINE_ERROR_CODE_ENGINE_SUCCESS) };
    }

    /// # Safety
    ///
    /// The connection must be running `execute`, and `data` must live until `cb` runs.
    pub unsafe fn on_io_complete(self, cb: IoCallback, data: *mut c_void) {
        // SAFETY: the caller guarantees the connection is live and `data` outlives `cb`.
        unsafe { (api().on_io_complete)(self.cookie, Some(cb), data) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memcached::ffi::SERVER_CORE_API;

    unsafe extern "C" fn built() -> *const c_char {
        VERSION.as_ptr().cast()
    }

    unsafe extern "C" fn other() -> *const c_char {
        c"0.0.0".as_ptr()
    }

    fn check(version: Option<unsafe extern "C" fn() -> *const c_char>) -> Option<Api> {
        // SAFETY: all-zero is a valid handle and API table: null pointers and `None` functions.
        let (mut server, core) = unsafe {
            (
                std::mem::zeroed::<SERVER_HANDLE_V1>(),
                Box::leak(Box::new(std::mem::zeroed::<SERVER_CORE_API>())),
            )
        };
        core.server_version = version;
        server.core = core;
        Api::new(&server)
    }

    #[test]
    fn a_server_of_another_version_is_refused() {
        assert!(check(None).is_none());
        assert!(check(Some(other)).is_none());
    }

    #[test]
    fn the_built_version_is_recognized() {
        assert!(built_for(built));
        assert!(!built_for(other));
    }
}
