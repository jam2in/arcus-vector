use std::cell::Cell;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use crate::cmd;
use crate::guard;
use crate::memcached::ffi::{AUTHZ_NONE, EXTENSION_ASCII_PROTOCOL_DESCRIPTOR, token_t};
use crate::memcached::server::{Connection, ResponseHandler};
use crate::nread::{self, Body};
use crate::protocol::{self, Tokens};

const COMMAND: &[u8] = b"vop";

thread_local! {
    /// Set by [`close`] for the `execute` the core calls right after `accept`.
    static CLOSE: Cell<bool> = const { Cell::new(false) };
}

/// Claims the line without its body and has the `execute` right after close the connection, the
/// only way to keep the stream in step when the body cannot be read. The core starts `ndata` at
/// 0, and `accept` sets it last, so it is still 0 here.
fn close() -> bool {
    CLOSE.set(true);
    true
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
/// `argv`/`argc` must meet [`Tokens::new`]'s contract, `cookie` must be the live connection, and
/// non-null `ndata`/`ptr` must be writable.
unsafe extern "C" fn accept(
    _cmd_cookie: *const c_void,
    cookie: *mut c_void,
    argc: c_int,
    argv: *mut token_t,
    ndata: *mut usize,
    ptr: *mut *mut c_char,
) -> bool {
    // Clear a flag whose `execute` never ran (refused by SASL), or it closes another connection.
    CLOSE.set(false);
    guard(close, || {
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
        if name != COMMAND {
            return false;
        }
        // SAFETY: the caller guarantees `Tokens::new`'s contract.
        let tokens = unsafe { Tokens::new(argv, argc) };
        // `execute` answers a bad line.
        let Ok(request) = protocol::parse(&tokens) else {
            return true;
        };
        let len = match nread::body(&request) {
            Body::None => return true,
            Body::Read(len) if !ndata.is_null() && !ptr.is_null() => len,
            Body::Read(_) | Body::Refuse => return close(),
        };
        // SAFETY: the server is running `accept` for `cookie`, which keeps nothing else.
        let Some(buffer) = (unsafe { nread::expect(Connection::new(cookie, None), &tokens, len) })
        else {
            return close();
        };
        // SAFETY: both are non-null, so the caller guarantees they are writable.
        unsafe {
            *ndata = len;
            *ptr = buffer;
        }
        true
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
            if CLOSE.replace(false) {
                return false;
            }
            // SAFETY: the server is running `execute` for `cookie`, with `response_handler`.
            let conn = unsafe { Connection::new(cookie, response_handler) };
            if argc == 0 {
                // SAFETY: without a line, the core is done reading the body `accept` kept on `conn`.
                let Some(pending) = (unsafe { nread::take(conn) }) else {
                    return false;
                };
                let request = pending.request();
                let vector = request.as_ref().map_or(&[][..], |r| pending.vector(r));
                return cmd::run(conn, request, vector);
            }
            // SAFETY: the caller guarantees `Tokens::new`'s contract.
            let tokens = unsafe { Tokens::new(argv, argc) };
            cmd::run(conn, protocol::parse(&tokens), &[])
        },
    )
}

/// # Safety
///
/// `cookie` must be a live connection whose body the core stopped reading.
unsafe extern "C" fn abort(_cmd_cookie: *const c_void, cookie: *const c_void) {
    guard(
        || (),
        || {
            // SAFETY: the server is running `abort` for `cookie`, and only `accept` keeps on it.
            drop(unsafe { nread::take(Connection::new(cookie, None)) });
        },
    );
}

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
        CLOSE.set(true);
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
        assert!(!CLOSE.get());
    }

    #[test]
    fn accept_clears_a_flag_left_behind() {
        CLOSE.set(true);
        assert!(!accepts(&["get", "key"]));
        assert!(!CLOSE.get());
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
        assert_eq!(reply_to("vop create unsupported 2 0"), "CREATED\r\n");
        assert_eq!(reply_to("vop drop unsupported"), "NOT_SUPPORTED\r\n");
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

    /// Runs a body-carrying line through `accept`, the core's read, and `execute`.
    fn reply_to_body(line: &str, body: &[u8]) -> Result<String, &'static str> {
        crate::memcached::server::fake::install();
        let mut reply: Vec<u8> = Vec::new();
        let cookie: *mut c_void = (&raw mut reply).cast();
        let (_buf, mut tokens) = protocol::tokenize(line);
        let (mut ndata, mut buffer) = (0, ptr::null_mut());
        // SAFETY: `tokens` point into `_buf`; `ndata`/`buffer` are writable.
        let claimed = unsafe {
            accept(
                ptr::null(),
                cookie,
                tokens.len() as c_int,
                tokens.as_mut_ptr(),
                &raw mut ndata,
                &raw mut buffer,
            )
        };
        assert!(claimed);
        if ndata == 0 {
            // SAFETY: as above, and `record` gets `reply` back as its cookie.
            let open = unsafe {
                execute(
                    ptr::null(),
                    cookie,
                    tokens.len() as c_int,
                    tokens.as_mut_ptr(),
                    Some(record),
                )
            };
            return if open {
                Ok(String::from_utf8(reply).unwrap())
            } else {
                Err("closed")
            };
        }
        assert_eq!(ndata, body.len());
        // SAFETY: `buffer` is the `ndata`-byte buffer `accept` handed out.
        unsafe { ptr::copy_nonoverlapping(body.as_ptr(), buffer.cast(), body.len()) };
        // SAFETY: the body is in; `record` gets `reply` back as its cookie.
        let open = unsafe { execute(ptr::null(), cookie, 0, ptr::null_mut(), Some(record)) };
        assert!(open);
        Ok(String::from_utf8(reply).unwrap())
    }

    #[test]
    fn a_body_is_read_then_answered() {
        assert_eq!(
            reply_to_body("vop setmeta absent k1 5", b"hello\r\n"),
            Ok("NOT_FOUND\r\n".into())
        );
        assert_eq!(
            reply_to_body("vop upsert absent k1 4 meta 2", b"{}\r\nabcd\r\n"),
            Ok("NOT_FOUND\r\n".into())
        );
    }

    #[test]
    fn a_vector_of_other_dimensions_is_a_bad_value() {
        assert_eq!(reply_to("vop create twodim 2 0"), "CREATED\r\n");
        assert_eq!(
            reply_to_body("vop upsert twodim k1 4", b"abcd\r\n"),
            Ok("CLIENT_ERROR bad value\r\n".into())
        );
        assert_eq!(
            reply_to_body("vop upsert twodim k1 3 meta 2 str", b"{}\r\n1.5\r\n"),
            Ok("CLIENT_ERROR bad value\r\n".into())
        );
    }

    #[test]
    fn a_segment_without_its_crlf_is_a_bad_data_chunk() {
        assert_eq!(
            reply_to_body("vop setmeta idx k1 5", b"hello!!"),
            Ok("CLIENT_ERROR bad data chunk\r\n".into())
        );
    }

    #[test]
    fn a_refused_body_closes_the_connection() {
        let line = format!("vop setmeta idx k1 {}", protocol::MAX_BODY_BYTES);
        assert_eq!(reply_to_body(&line, b""), Err("closed"));
    }

    #[test]
    fn a_missing_index_is_not_found() {
        for line in [
            "vop drop missing",
            "vop getmeta missing k1",
            "vop delete missing k1",
            "vop search missing vkey k1 5",
        ] {
            assert_eq!(reply_to(line), "NOT_FOUND\r\n", "{line}");
        }
        assert_eq!(
            reply_to_body("vop setmeta missing k1 5", b"hello\r\n"),
            Ok("NOT_FOUND\r\n".into())
        );
    }

    #[test]
    fn a_bad_line_is_answered_without_its_body() {
        assert_eq!(
            reply_to_body("vop setmeta idx k1", b""),
            Ok("CLIENT_ERROR bad command line format\r\n".into())
        );
    }

    #[test]
    fn abort_drops_the_kept_body() {
        crate::memcached::server::fake::install();
        let mut reply: Vec<u8> = Vec::new();
        let cookie: *mut c_void = (&raw mut reply).cast();
        let (_buf, mut tokens) = protocol::tokenize("vop setmeta idx k1 5");
        let (mut ndata, mut buffer) = (0, ptr::null_mut());
        // SAFETY: `tokens` point into `_buf`; `ndata`/`buffer` are writable.
        unsafe {
            accept(
                ptr::null(),
                cookie,
                tokens.len() as c_int,
                tokens.as_mut_ptr(),
                &raw mut ndata,
                &raw mut buffer,
            );
            abort(ptr::null(), cookie);
        }
        assert_eq!(ndata, 7);
        // SAFETY: `cookie` stands for a live connection in the fake server.
        assert!(unsafe { nread::take(Connection::new(cookie, None)) }.is_none());
    }
}
