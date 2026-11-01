//! arcus-vector: vector search as an extension to arcus-memcached.

mod aio;
mod ascii;
mod bson;
mod error;
mod memcached;
mod protocol;

use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::memcached::ffi::{
    EXTENSION_ERROR_CODE, EXTENSION_ERROR_CODE_EXTENSION_FATAL,
    EXTENSION_ERROR_CODE_EXTENSION_SUCCESS, GET_SERVER_API,
};

pub(crate) fn guard<T>(on_panic: impl FnOnce() -> T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| on_panic())
}

#[unsafe(no_mangle)]
#[expect(
    clippy::print_stderr,
    reason = "the server's logger is reachable only once its API passes the check"
)]
pub extern "C" fn memcached_extensions_initialize(
    _config: *const c_char,
    get_server_api: GET_SERVER_API,
) -> EXTENSION_ERROR_CODE {
    guard(
        || EXTENSION_ERROR_CODE_EXTENSION_FATAL,
        || match initialize(get_server_api) {
            Ok(()) => EXTENSION_ERROR_CODE_EXTENSION_SUCCESS,
            Err(why) => {
                eprintln!("arcus-vector: {why}");
                EXTENSION_ERROR_CODE_EXTENSION_FATAL
            }
        },
    )
}

fn initialize(get_server_api: GET_SERVER_API) -> Result<(), &'static str> {
    let get_server_api = get_server_api.ok_or("the server passed no get_server_api")?;
    // SAFETY: the core's own function; it takes no arguments and returns its static handle.
    let server = unsafe { get_server_api().as_ref() }.ok_or("the server has no server API")?;
    memcached::server::set(server)?;
    if !aio::start() {
        return Err("the lane threads could not start");
    }
    // SAFETY: `DESCRIPTOR` is static and only the core touches it from here on (it writes `next`).
    if !unsafe { memcached::server::register_ascii(&raw mut ascii::DESCRIPTOR) } {
        return Err("the server refused the ASCII protocol extension");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_turns_a_panic_into_the_fallback() {
        assert_eq!(guard(|| 0, || 1), 1);
        assert_eq!(guard(|| 0, || panic!("in a callback")), 0);
    }
}
