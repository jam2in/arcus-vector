//! The ASCII protocol extension that claims `vop` command lines.
//!
//! No command is implemented yet, so every `vop` line is answered with `NOT_SUPPORTED`.

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use crate::memcached::ffi::{AUTHZ_NONE, EXTENSION_ASCII_PROTOCOL_DESCRIPTOR, token_t};

const COMMAND: &[u8] = b"vop";
const NOT_SUPPORTED: &[u8] = b"NOT_SUPPORTED\r\n";

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
/// If `argc` is at least one and `argv` is non-null, `argv` must point to a valid `token_t`, and
/// that token's `value`, if non-null, must point to `length` readable bytes. Both must stay valid
/// for the call.
unsafe extern "C" fn accept(
    _cmd_cookie: *const c_void,
    _cookie: *mut c_void,
    argc: c_int,
    argv: *mut token_t,
    _ndata: *mut usize,
    _ptr: *mut *mut c_char,
) -> bool {
    if argc < 1 || argv.is_null() {
        return false;
    }
    // SAFETY: `argc` is at least one and `argv` is non-null, so the caller guarantees `argv` points
    // to a valid token for the call.
    let first = unsafe { &*argv };
    if first.value.is_null() {
        return false;
    }
    // SAFETY: `value` is non-null, so the caller guarantees it points to `length` readable bytes
    // for the call.
    let name = unsafe { std::slice::from_raw_parts(first.value.cast::<u8>(), first.length) };
    name == COMMAND
}

/// # Safety
///
/// `response_handler`, if present, must be safe to call with `cookie` and a pointer to bytes that
/// live for the call.
unsafe extern "C" fn execute(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
    _argc: c_int,
    _argv: *mut token_t,
    response_handler: ResponseHandler,
) -> bool {
    let Some(respond) = response_handler else {
        return false;
    };
    // SAFETY: the caller guarantees `respond` is safe to call with `cookie`, and `NOT_SUPPORTED`
    // is a `'static` byte string, so it outlives the call.
    unsafe {
        respond(
            cookie,
            NOT_SUPPORTED.len() as c_int,
            NOT_SUPPORTED.as_ptr().cast(),
        )
    }
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
        // SAFETY: when `line` is non-empty, `tokens[0]` is a valid token over a string that
        // outlives the call.
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
    fn accepts_only_vop_lines() {
        assert!(accepts(&["vop", "create", "idx", "2", "0"]));
        assert!(!accepts(&["get", "key"]));
        assert!(!accepts(&["vopx"]));
        assert!(!accepts(&[]));
    }

    /// # Safety
    ///
    /// `cookie` must point to a `Vec<u8>` that nothing else accesses during the call, and `dta`
    /// must point to `nbytes` readable bytes.
    unsafe extern "C" fn record(cookie: *const c_void, nbytes: c_int, dta: *const c_char) -> bool {
        // SAFETY: the caller guarantees `cookie` is an unaliased `Vec<u8>` and `dta` holds `nbytes`
        // bytes.
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
        // SAFETY: `record` gets `reply` back as its cookie, which nothing else touches during the
        // call, and `execute` passes it a `'static` reply.
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
