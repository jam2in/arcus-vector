use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use super::ffi::{
    ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_EVENT_TYPE, ENGINE_EVENT_TYPE_ON_EVENT_ITEM,
    SERVER_CORE_API, SERVER_HANDLE_V1,
};

pub type ResponseHandler =
    Option<unsafe extern "C" fn(cookie: *const c_void, nbytes: c_int, dta: *const c_char) -> bool>;

pub type IoCallback = unsafe extern "C" fn(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool;

pub type EventCallback = unsafe extern "C" fn(
    cookie: *const c_void,
    type_: ENGINE_EVENT_TYPE,
    event_data: *const c_void,
    cb_data: *const c_void,
);

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

/// # Safety
///
/// `handler`, if present, must accept `cookie`.
pub unsafe fn respond(handler: ResponseHandler, cookie: *const c_void, reply: &[u8]) -> bool {
    let Some(handler) = handler else {
        return false;
    };
    let Ok(len) = c_int::try_from(reply.len()) else {
        return false;
    };
    // SAFETY: the caller guarantees `handler` accepts `cookie`; `reply` outlives the call.
    unsafe { handler(cookie, len, reply.as_ptr().cast()) }
}

/// Call before handing the work to another thread, so a notify that comes first still counts.
///
/// # Safety
///
/// `cookie` must be the connection of the running `execute`.
#[expect(clippy::expect_used, reason = "the core API is to be checked at load")]
pub unsafe fn waitfor_io_complete(cookie: *const c_void) {
    let waitfor = core()
        .and_then(|core| core.waitfor_io_complete)
        .expect("the core supports waitfor_io_complete");
    // SAFETY: the caller guarantees `cookie` is a live connection.
    unsafe { waitfor(cookie) };
}

/// Ends one wait; also withdraws a wait that turned out not to be needed.
///
/// # Safety
///
/// `cookie` must be a connection with a wait from [`waitfor_io_complete`].
#[expect(clippy::expect_used, reason = "the core API is to be checked at load")]
pub unsafe fn notify_io_complete(cookie: *const c_void) {
    let notify = core()
        .and_then(|core| core.notify_io_complete)
        .expect("the core supports notify_io_complete");
    // SAFETY: the caller guarantees `cookie` is a waiting, so live, connection.
    unsafe { notify(cookie, ENGINE_ERROR_CODE_ENGINE_SUCCESS) };
}

/// # Safety
///
/// `cookie` must be the running command's connection, and `data` must live until `cb` runs.
#[expect(clippy::expect_used, reason = "the core API is to be checked at load")]
pub unsafe fn on_io_complete(cookie: *const c_void, cb: IoCallback, data: *mut c_void) {
    let register = core()
        .and_then(|core| core.on_io_complete)
        .expect("the core supports on_io_complete");
    // SAFETY: the caller guarantees `cookie` is a live connection and `data` outlives `cb`.
    unsafe { register(cookie, Some(cb), data) };
}

/// Registers `cb` for the engine's item and element lifecycle events.
pub fn register_item_event(cb: EventCallback) -> bool {
    let server = SERVER.load(Ordering::Acquire);
    if server.is_null() {
        return false;
    }
    // SAFETY: `server` is the core's static handle, set once at initialization.
    let callback = unsafe { (*server).callback };
    // SAFETY: a non-null `callback` is the core's static callback table.
    let Some(register) = unsafe { callback.as_ref() }.and_then(|api| api.register_callback) else {
        return false;
    };
    // SAFETY: `cb` has the `EVENT_CALLBACK` signature; the core takes a null engine and data.
    unsafe {
        register(
            ptr::null_mut(),
            ENGINE_EVENT_TYPE_ON_EVENT_ITEM,
            Some(cb),
            ptr::null(),
        );
    }
    true
}
