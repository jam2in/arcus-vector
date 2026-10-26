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
    pub unsafe fn wait_for_io(self) -> IoWaiter {
        let waitfor = core()
            .and_then(|core| core.waitfor_io_complete)
            .expect("the core supports waitfor_io_complete");
        // SAFETY: the caller guarantees the connection is live.
        unsafe { waitfor(self.cookie) };
        IoWaiter(self.cookie)
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

/// Wakes the connection that made it with [`Connection::wait_for_io`] when dropped, on whichever
/// thread holds it then, so no path that loses it can leave the connection waiting.
pub struct IoWaiter(*const c_void);

// SAFETY: its one use, the notify in `drop`, calls the core's `notify_io_complete`, which takes
// the connection's thread lock.
unsafe impl Send for IoWaiter {}

impl Drop for IoWaiter {
    #[expect(clippy::expect_used, reason = "the core API is to be checked at load")]
    fn drop(&mut self) {
        let notify = core()
            .and_then(|core| core.notify_io_complete)
            .expect("the core supports notify_io_complete");
        // SAFETY: the connection waits for this one notify, so the server keeps it alive until then.
        unsafe { notify(self.0, ENGINE_ERROR_CODE_ENGINE_SUCCESS) };
    }
}

/// A core API for unit tests, where each cookie is a [`fake::Conn`].
#[cfg(test)]
pub mod fake {
    use std::sync::mpsc::Sender;
    use std::sync::{Mutex, Once};

    use super::*;
    use crate::memcached::ffi::{AIO_CALLBACK, ENGINE_ERROR_CODE};

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

    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // SAFETY: all-zero is a valid handle and API table: null pointers and `None` functions.
            let (server, core) = unsafe {
                (
                    Box::leak(Box::new(std::mem::zeroed::<SERVER_HANDLE_V1>())),
                    Box::leak(Box::new(std::mem::zeroed::<SERVER_CORE_API>())),
                )
            };
            core.waitfor_io_complete = Some(waitfor_io_complete);
            core.notify_io_complete = Some(notify_io_complete);
            core.on_io_complete = Some(on_io_complete);
            server.core = core;
            set(server);
        });
    }
}
