use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::ptr;
use std::sync::OnceLock;

mod connection;
#[cfg(test)]
pub mod fake;

use super::ffi::{
    AIO_CALLBACK, ENGINE_ERROR_CODE, ENGINE_EVENT_TYPE, ENGINE_HANDLE, EVENT_CALLBACK,
    EXTENSION_ASCII_PROTOCOL_DESCRIPTOR, EXTENSION_LOGGER_DESCRIPTOR, SERVER_HANDLE_V1, VERSION,
    engine_interface_v1, extension_type_t, extension_type_t_EXTENSION_ASCII_PROTOCOL, rel_time_t,
    time_t,
};
pub use connection::{Connection, IoWaiter, ResponseHandler};

pub type GetServerApi = unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1;

pub type EventCallback = unsafe extern "C" fn(
    cookie: *const c_void,
    type_: ENGINE_EVENT_TYPE,
    data: *const c_void,
    cb_data: *const c_void,
);

struct Api {
    get_server_api: GetServerApi,
    get_logger: unsafe extern "C" fn() -> *mut EXTENSION_LOGGER_DESCRIPTOR,
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

/// Fails, saying why, if this server is not the version arcus-vector was built for or lacks what
/// it uses.
pub fn set(get_server_api: GetServerApi) -> Result<(), &'static str> {
    // SAFETY: the server's own function; it takes no arguments and returns its static handle.
    let server = unsafe { get_server_api().as_ref() }.ok_or("the server has no server API")?;
    // A second `-X` of this library fails here, before its descriptor is registered twice.
    API.set(Api::new(server, get_server_api)?)
        .map_err(|_| "already loaded")
}

/// `exptime` as the engine keeps it: memcached's rule, where a small value is seconds from now.
pub fn realtime(exptime: u32) -> u32 {
    // SAFETY: the server's own function, which only reads its clock.
    unsafe { (api().realtime)(time_t::from(exptime)) }
}

/// The loaded engine; `None` before the server loads it.
pub fn engine() -> Option<&'static engine_interface_v1> {
    // SAFETY: the server's own function; it returns its static handle, whose engine it fills in
    // only when asked after the engine has loaded.
    let server = unsafe { (api().get_server_api)().as_ref() }?;
    // SAFETY: a non-null engine handle is the loaded engine's interface, alive for the process.
    unsafe { server.engine.cast::<engine_interface_v1>().as_ref() }
}

impl Api {
    fn new(server: &SERVER_HANDLE_V1, get_server_api: GetServerApi) -> Result<Self, &'static str> {
        // SAFETY: a non-null `core` of the server handle is its static core API.
        let core = unsafe { server.core.as_ref() }.ok_or("the server has no core API")?;
        let server_version = core
            .server_version
            .ok_or("the server lacks server_version")?;
        if !check_version(server_version) {
            return Err("the server is not the arcus-memcached version arcus-vector was built for");
        }
        // SAFETY: the other non-null tables of the server handle are its static APIs.
        let (log, extension, callback) = unsafe {
            (
                server.log.as_ref().ok_or("the server has no log API")?,
                server
                    .extension
                    .as_ref()
                    .ok_or("the server has no extension API")?,
                server
                    .callback
                    .as_ref()
                    .ok_or("the server has no callback API")?,
            )
        };
        Ok(Self {
            get_server_api,
            get_logger: log.get_logger.ok_or("the server lacks get_logger")?,
            waitfor_io_complete: core
                .waitfor_io_complete
                .ok_or("the server lacks waitfor_io_complete")?,
            notify_io_complete: core
                .notify_io_complete
                .ok_or("the server lacks notify_io_complete")?,
            on_io_complete: core
                .on_io_complete
                .ok_or("the server lacks on_io_complete")?,
            realtime: core.realtime.ok_or("the server lacks realtime")?,
            store_engine_specific: core
                .store_engine_specific
                .ok_or("the server lacks store_engine_specific")?,
            get_engine_specific: core
                .get_engine_specific
                .ok_or("the server lacks get_engine_specific")?,
            register_extension: extension
                .register_extension
                .ok_or("the server lacks register_extension")?,
            register_callback: callback
                .register_callback
                .ok_or("the server lacks register_callback")?,
        })
    }
}

fn check_version(server_version: unsafe extern "C" fn() -> *const c_char) -> bool {
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

/// # Safety
///
/// `cb` must take what the server passes with a `type_` event.
pub unsafe fn register_callback(type_: ENGINE_EVENT_TYPE, cb: EventCallback) {
    // SAFETY: the caller guarantees `cb` takes `type_` events; the server ignores the engine and
    // keeps no pointer of ours.
    unsafe { (api().register_callback)(ptr::null_mut(), type_, Some(cb), ptr::null()) };
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;

    unsafe extern "C" fn no_server() -> *mut SERVER_HANDLE_V1 {
        ptr::null_mut()
    }

    unsafe extern "C" fn other() -> *const c_char {
        c"0.0.0".as_ptr()
    }

    #[test]
    fn a_server_with_everything_is_taken() {
        assert!(Api::new(&fake::server(), no_server).is_ok());
    }

    #[test]
    fn a_server_of_another_version_is_refused() {
        let server = fake::server();
        // SAFETY: `core` is the fake's own leaked table, used by nothing else.
        unsafe { (*server.core).server_version = Some(other) };
        assert_eq!(
            Api::new(&server, no_server).err(),
            Some("the server is not the arcus-memcached version arcus-vector was built for")
        );
        // SAFETY: as above.
        unsafe { (*server.core).server_version = None };
        assert_eq!(
            Api::new(&server, no_server).err(),
            Some("the server lacks server_version")
        );
    }

    #[test]
    fn a_missing_table_is_named() {
        let mut server = fake::server();
        server.log = ptr::null_mut();
        assert_eq!(
            Api::new(&server, no_server).err(),
            Some("the server has no log API")
        );
    }

    #[test]
    fn a_missing_function_is_named() {
        let server = fake::server();
        // SAFETY: `core` is the fake's own leaked table, used by nothing else.
        unsafe { (*server.core).waitfor_io_complete = None };
        assert_eq!(
            Api::new(&server, no_server).err(),
            Some("the server lacks waitfor_io_complete")
        );
    }
}
