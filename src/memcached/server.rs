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

#[expect(dead_code, reason = "the commands that use the rest are not in yet")]
struct Api {
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
pub fn set(server: &'static SERVER_HANDLE_V1) -> Result<(), &'static str> {
    // A second `-X` of this library fails here, before its descriptor is registered twice.
    API.set(Api::new(server)?).map_err(|_| "already loaded")
}

impl Api {
    fn new(server: &SERVER_HANDLE_V1) -> Result<Self, &'static str> {
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
    pub unsafe fn wait_for_io(self) -> IoWaiter {
        // SAFETY: the caller guarantees the connection is live.
        unsafe { (api().waitfor_io_complete)(self.cookie) };
        IoWaiter(self.cookie)
    }

    /// # Safety
    ///
    /// The connection must be running `execute`, and `data` must live until `cb` runs.
    pub unsafe fn on_io_complete(self, cb: IoCallback, data: *mut c_void) {
        // SAFETY: the caller guarantees the connection is live and `data` outlives `cb`.
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

/// A core API for unit tests, where each cookie is a [`fake::Conn`].
#[cfg(test)]
pub mod fake {
    use std::mem;
    use std::ptr;
    use std::sync::mpsc::Sender;
    use std::sync::{Mutex, Once};

    use super::*;
    use crate::memcached::ffi::{
        SERVER_CALLBACK_API, SERVER_CORE_API, SERVER_EXTENSION_API, SERVER_LOG_API,
    };

    pub struct Conn {
        id: usize,
        notified: Sender<usize>,
        callback: Mutex<(AIO_CALLBACK, usize)>,
        reply: Mutex<Vec<u8>>,
    }

    impl Conn {
        /// Sends `id` to `notified` when the connection is notified.
        pub fn new(id: usize, notified: &Sender<usize>) -> Self {
            Self {
                id,
                notified: notified.clone(),
                callback: Mutex::new((None, 0)),
                reply: Mutex::new(Vec::new()),
            }
        }

        pub fn connection(&self) -> Connection {
            // SAFETY: `record` takes any `Conn` cookie.
            unsafe { Connection::new(ptr::from_ref(self).cast(), Some(record)) }
        }

        /// Runs the io callback, as the core does once the connection is notified.
        pub fn run_callback(&self) -> bool {
            let (cb, data) = *self.callback.lock().unwrap();
            let cb = cb.unwrap();
            // SAFETY: `cb` and `data` are what `on_io_complete` registered for this connection.
            unsafe {
                cb(
                    ptr::from_ref(self).cast(),
                    data as *mut c_void,
                    Some(record),
                )
            }
        }

        pub fn reply(&self) -> Vec<u8> {
            self.reply.lock().unwrap().clone()
        }
    }

    /// # Safety
    ///
    /// `cookie` must be a live `Conn`.
    unsafe fn conn<'a>(cookie: *const c_void) -> &'a Conn {
        // SAFETY: the caller guarantees `cookie` is a live `Conn`.
        unsafe { &*cookie.cast::<Conn>() }
    }

    unsafe extern "C" fn record(cookie: *const c_void, nbytes: c_int, dta: *const c_char) -> bool {
        // SAFETY: the cookie is a `Conn`, and the caller passes `nbytes` bytes at `dta`.
        let (conn, bytes) = unsafe {
            (
                conn(cookie),
                std::slice::from_raw_parts(dta.cast::<u8>(), nbytes as usize),
            )
        };
        conn.reply.lock().unwrap().extend_from_slice(bytes);
        true
    }

    unsafe extern "C" fn waitfor_io_complete(_: *const c_void) {}

    unsafe extern "C" fn notify_io_complete(cookie: *const c_void, _: ENGINE_ERROR_CODE) {
        // SAFETY: the cookie is a `Conn`, alive until its notify.
        let conn = unsafe { conn(cookie) };
        let _ = conn.notified.send(conn.id);
    }

    unsafe extern "C" fn on_io_complete(
        cookie: *const c_void,
        cb: AIO_CALLBACK,
        data: *mut c_void,
    ) {
        // SAFETY: the cookie is a `Conn` running `execute`.
        let conn = unsafe { conn(cookie) };
        *conn.callback.lock().unwrap() = (cb, data as usize);
    }

    unsafe extern "C" fn get_logger() -> *mut EXTENSION_LOGGER_DESCRIPTOR {
        ptr::null_mut()
    }

    unsafe extern "C" fn realtime(_: time_t) -> rel_time_t {
        0
    }

    unsafe extern "C" fn store_engine_specific(_: *const c_void, _: *mut c_void) {}

    unsafe extern "C" fn get_engine_specific(_: *const c_void) -> *mut c_void {
        ptr::null_mut()
    }

    unsafe extern "C" fn register_extension(_: extension_type_t, _: *mut c_void) -> bool {
        false
    }

    unsafe extern "C" fn register_callback(
        _: *mut ENGINE_HANDLE,
        _: ENGINE_EVENT_TYPE,
        _: EVENT_CALLBACK,
        _: *const c_void,
    ) {
    }

    unsafe extern "C" fn server_version() -> *const c_char {
        VERSION.as_ptr().cast()
    }

    /// A server handle with every table and function arcus-vector uses, each table leaked.
    pub fn server() -> SERVER_HANDLE_V1 {
        // SAFETY: all-zero is a valid handle and API table: null pointers and `None` functions.
        let (mut server, core, log, extension, callback) = unsafe {
            (
                mem::zeroed::<SERVER_HANDLE_V1>(),
                Box::leak(Box::new(mem::zeroed::<SERVER_CORE_API>())),
                Box::leak(Box::new(mem::zeroed::<SERVER_LOG_API>())),
                Box::leak(Box::new(mem::zeroed::<SERVER_EXTENSION_API>())),
                Box::leak(Box::new(mem::zeroed::<SERVER_CALLBACK_API>())),
            )
        };
        core.server_version = Some(server_version);
        core.waitfor_io_complete = Some(waitfor_io_complete);
        core.notify_io_complete = Some(notify_io_complete);
        core.on_io_complete = Some(on_io_complete);
        core.realtime = Some(realtime);
        core.store_engine_specific = Some(store_engine_specific);
        core.get_engine_specific = Some(get_engine_specific);
        log.get_logger = Some(get_logger);
        extension.register_extension = Some(register_extension);
        callback.register_callback = Some(register_callback);
        server.core = core;
        server.log = log;
        server.extension = extension;
        server.callback = callback;
        server
    }

    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| assert_eq!(set(Box::leak(Box::new(server()))), Ok(())));
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;

    unsafe extern "C" fn other() -> *const c_char {
        c"0.0.0".as_ptr()
    }

    #[test]
    fn a_server_with_everything_is_taken() {
        assert!(Api::new(&fake::server()).is_ok());
    }

    #[test]
    fn a_server_of_another_version_is_refused() {
        let server = fake::server();
        // SAFETY: `core` is the fake's own leaked table, used by nothing else.
        unsafe { (*server.core).server_version = Some(other) };
        assert_eq!(
            Api::new(&server).err(),
            Some("the server is not the arcus-memcached version arcus-vector was built for")
        );
        // SAFETY: as above.
        unsafe { (*server.core).server_version = None };
        assert_eq!(
            Api::new(&server).err(),
            Some("the server lacks server_version")
        );
    }

    #[test]
    fn a_missing_table_is_named() {
        let mut server = fake::server();
        server.log = ptr::null_mut();
        assert_eq!(Api::new(&server).err(), Some("the server has no log API"));
    }

    #[test]
    fn a_missing_function_is_named() {
        let server = fake::server();
        // SAFETY: `core` is the fake's own leaked table, used by nothing else.
        unsafe { (*server.core).waitfor_io_complete = None };
        assert_eq!(
            Api::new(&server).err(),
            Some("the server lacks waitfor_io_complete")
        );
    }
}
