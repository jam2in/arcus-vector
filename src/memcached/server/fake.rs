//! A core API for unit tests, where each cookie is a [`Conn`].

use std::cell::Cell;
use std::mem;
use std::os::raw::c_int;
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
        // SAFETY: `record` takes any `Conn` cookie, which takes a reply or a wait at any time.
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
    // The test may drop `conn` once the id arrives, so the send must not use its sender.
    let (notified, id) = (conn.notified.clone(), conn.id);
    let _ = notified.send(id);
}

unsafe extern "C" fn on_io_complete(cookie: *const c_void, cb: AIO_CALLBACK, data: *mut c_void) {
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

thread_local! {
    static KEPT: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
}

/// Keeps one value per thread, whatever the cookie.
unsafe extern "C" fn store_engine_specific(_: *const c_void, data: *mut c_void) {
    KEPT.set(data);
}

unsafe extern "C" fn get_engine_specific(_: *const c_void) -> *mut c_void {
    KEPT.get()
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

static SERVER: OnceLock<usize> = OnceLock::new();

unsafe extern "C" fn get_server_api() -> *mut SERVER_HANDLE_V1 {
    *SERVER.get_or_init(|| ptr::from_mut(Box::leak(Box::new(server()))) as usize)
        as *mut SERVER_HANDLE_V1
}

pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| assert_eq!(set(get_server_api), Ok(())));
}
