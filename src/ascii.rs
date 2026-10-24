use std::cell::Cell;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use crate::guard;
use crate::memcached::ffi::{AUTHZ_NONE, EXTENSION_ASCII_PROTOCOL_DESCRIPTOR, token_t};

const COMMAND: &[u8] = b"vop";
const NOT_SUPPORTED: &[u8] = b"NOT_SUPPORTED\r\n";

thread_local! {
    /// Lets the `execute` the core calls right after a panicked `accept` close the connection.
    static ACCEPT_PANICKED: Cell<bool> = const { Cell::new(false) };
}

type ResponseHandler =
    Option<unsafe extern "C" fn(cookie: *const c_void, nbytes: c_int, dta: *const c_char) -> bool>;

pub(crate) static mut DESCRIPTOR: EXTENSION_ASCII_PROTOCOL_DESCRIPTOR =
    EXTENSION_ASCII_PROTOCOL_DESCRIPTOR {
        get_name: Some(get_name),
        get_auth_flag: Some(get_auth_flag),
        accept: Some(accept),
        execute: Some(execute),
        abort: Some(abort),
        cookie: ptr::null(),
        next: ptr::null_mut(),
    };

extern "C" fn get_name(_cmd_cookie: *const c_void) -> *const c_char {
    c"arcus_vector".as_ptr()
}

extern "C" fn get_auth_flag() -> u16 {
    AUTHZ_NONE as u16
}

/// # Safety
///
/// Non-null `argv` must be a token with `length` readable bytes; non-null `ndata` must be writable.
unsafe extern "C" fn accept(
    _cmd_cookie: *const c_void,
    _cookie: *mut c_void,
    argc: c_int,
    argv: *mut token_t,
    ndata: *mut usize,
    _ptr: *mut *mut c_char,
) -> bool {
    // Clear a flag whose `execute` never ran (refused by SASL), or it closes another connection.
    ACCEPT_PANICKED.set(false);
    let claim_and_close = || {
        if !ndata.is_null() {
            // SAFETY: `ndata` is non-null, so the caller guarantees it is valid to write.
            unsafe { *ndata = 0 };
        }
        ACCEPT_PANICKED.set(true);
        true
    };
    guard(claim_and_close, || {
        if argc < 1 || argv.is_null() {
            return false;
        }
        // SAFETY: `argc >= 1` and `argv` is non-null, so it points to a valid token.
        let first = unsafe { &*argv };
        if first.value.is_null() {
            return false;
        }
        // SAFETY: `value` is non-null, so it points to `length` readable bytes.
        let name = unsafe { std::slice::from_raw_parts(first.value.cast::<u8>(), first.length) };
        name == COMMAND
    })
}

/// # Safety
///
/// `response_handler`, if present, must accept `cookie` and bytes that live for the call.
unsafe extern "C" fn execute(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
    _argc: c_int,
    _argv: *mut token_t,
    response_handler: ResponseHandler,
) -> bool {
    guard(
        || false,
        || {
            if ACCEPT_PANICKED.replace(false) {
                return false;
            }
            let Some(respond) = response_handler else {
                return false;
            };
            // SAFETY: `respond` accepts `cookie`, and `NOT_SUPPORTED` is `'static`.
            unsafe {
                respond(
                    cookie,
                    NOT_SUPPORTED.len() as c_int,
                    NOT_SUPPORTED.as_ptr().cast(),
                )
            }
        },
    )
}

extern "C" fn abort(_cmd_cookie: *const c_void, _cookie: *const c_void) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(text: &str) -> token_t {
        token_t {
            value: text.as_ptr().cast_mut().cast(),
            length: text.len(),
        }
    }

    fn accepts(line: &[&str]) -> bool {
        let mut tokens: Vec<token_t> = line.iter().map(|t| token(t)).collect();
        // SAFETY: `tokens` point into `line`, which outlives the call.
        unsafe {
            accept(
                ptr::null(),
                ptr::null_mut(),
                line.len() as c_int,
                tokens.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        }
    }

    #[test]
    fn a_panicked_accept_closes_the_connection() {
        ACCEPT_PANICKED.set(true);
        let mut reply: Vec<u8> = Vec::new();
        let mut tokens = vec![token("vop")];
        // SAFETY: `record` gets the unshared `reply` back as its cookie.
        let ok = unsafe {
            execute(
                ptr::null(),
                (&raw mut reply).cast(),
                1,
                tokens.as_mut_ptr(),
                Some(record),
            )
        };
        assert!(!ok);
        assert!(reply.is_empty());
        assert!(!ACCEPT_PANICKED.get());
    }

    #[test]
    fn accept_clears_a_flag_left_behind() {
        ACCEPT_PANICKED.set(true);
        assert!(!accepts(&["get", "key"]));
        assert!(!ACCEPT_PANICKED.get());
    }

    #[test]
    fn accepts_only_vop_lines() {
        assert!(accepts(&["vop", "create", "idx", "2", "0"]));
        assert!(!accepts(&["get", "key"]));
        assert!(!accepts(&["vopx"]));
        assert!(!accepts(&[]));
    }

    /// # Safety
    ///
    /// `cookie` must be an unshared `Vec<u8>`, and `dta` must hold `nbytes` bytes.
    unsafe extern "C" fn record(cookie: *const c_void, nbytes: c_int, dta: *const c_char) -> bool {
        // SAFETY: the caller guarantees both, per `# Safety`.
        let (reply, bytes) = unsafe {
            (
                &mut *cookie.cast_mut().cast::<Vec<u8>>(),
                std::slice::from_raw_parts(dta.cast::<u8>(), nbytes as usize),
            )
        };
        reply.extend_from_slice(bytes);
        true
    }

    #[test]
    fn every_vop_line_is_not_supported() {
        let mut reply: Vec<u8> = Vec::new();
        let mut tokens = vec![token("vop"), token("create")];
        // SAFETY: `record` gets the unshared `reply` back as its cookie.
        let ok = unsafe {
            execute(
                ptr::null(),
                (&raw mut reply).cast(),
                2,
                tokens.as_mut_ptr(),
                Some(record),
            )
        };
        assert!(ok);
        assert_eq!(reply, b"NOT_SUPPORTED\r\n");
    }
}
