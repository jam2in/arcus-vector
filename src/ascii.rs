use std::cell::Cell;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use crate::aio;
use crate::error::Error;
use crate::guard;
use crate::index;
use crate::memcached::ffi::{AUTHZ_NONE, EXTENSION_ASCII_PROTOCOL_DESCRIPTOR, token_t};
use crate::memcached::server::{Connection, ResponseHandler};
use crate::protocol::{self, Request, Tokens};

const COMMAND: &[u8] = b"vop";

thread_local! {
    /// Lets the `execute` the core calls right after a panicked `accept` close the connection.
    static ACCEPT_PANICKED: Cell<bool> = const { Cell::new(false) };
}

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
/// `argv`/`argc` must meet [`Tokens::new`]'s contract; `response_handler` must accept `cookie`.
unsafe extern "C" fn execute(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    response_handler: ResponseHandler,
) -> bool {
    guard(
        || false,
        || {
            if ACCEPT_PANICKED.replace(false) {
                return false;
            }
            // SAFETY: the caller guarantees `Tokens::new`'s contract.
            let tokens = unsafe { Tokens::new(argv, argc) };
            // SAFETY: the caller guarantees `response_handler` accepts `cookie`.
            let conn = unsafe { Connection::new(cookie, response_handler) };
            let reply = match protocol::parse(&tokens) {
                Ok(Request::Search(_)) => {
                    let reply = Error::NotSupported.reply();
                    // SAFETY: `conn` is running `execute`.
                    unsafe { aio::begin(conn, reply) };
                    return true;
                }
                Ok(Request::Create(spec)) => match index::create(&spec) {
                    Ok(()) => b"CREATED\r\n",
                    Err(e) => e.reply(),
                },
                Ok(_) => Error::NotSupported.reply(),
                Err(e) => e.reply(),
            };
            // SAFETY: `conn` is running `execute`.
            unsafe { conn.respond(reply) }
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

    fn reply_to(line: &str) -> String {
        let mut reply: Vec<u8> = Vec::new();
        let (_buf, mut tokens) = protocol::tokenize(line);
        // SAFETY: `tokens` point into `_buf`, and `record` gets the unshared `reply` back.
        let ok = unsafe {
            execute(
                ptr::null(),
                (&raw mut reply).cast(),
                tokens.len() as c_int,
                tokens.as_mut_ptr(),
                Some(record),
            )
        };
        assert!(ok);
        String::from_utf8(reply).unwrap()
    }

    #[test]
    fn well_formed_lines_are_not_supported_yet() {
        assert_eq!(reply_to("vop drop idx"), "NOT_SUPPORTED\r\n");
    }

    #[test]
    fn create_registers_the_index_once() {
        assert_eq!(reply_to("vop create ascii 2 0"), "CREATED\r\n");
        assert_eq!(reply_to("vop create ascii 2 0"), "EXISTS\r\n");
    }

    #[test]
    fn malformed_lines_get_the_reason() {
        assert_eq!(
            reply_to("vop create idx"),
            "CLIENT_ERROR bad command line format\r\n"
        );
        assert_eq!(
            reply_to("vop list"),
            "CLIENT_ERROR bad command line format\r\n"
        );
    }
}
