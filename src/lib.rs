//! arcus-vector: vector search as an extension to arcus-memcached.

mod aio;
mod ascii;
mod error;
mod index;
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
pub extern "C" fn memcached_extensions_initialize(
    _config: *const c_char,
    get_server_api: GET_SERVER_API,
) -> EXTENSION_ERROR_CODE {
    guard(
        || EXTENSION_ERROR_CODE_EXTENSION_FATAL,
        || initialize(get_server_api),
    )
}

fn initialize(get_server_api: GET_SERVER_API) -> EXTENSION_ERROR_CODE {
    let Some(get_server_api) = get_server_api else {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    };
    // SAFETY: the core's own function; it takes no arguments and returns its static handle.
    let Some(server) = (unsafe { get_server_api().as_ref() }) else {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    };
    if !memcached::server::set(server) || !aio::start() {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    }
    // SAFETY: `DESCRIPTOR` is static and only the core touches it from here on (it writes `next`).
    let registered = unsafe { memcached::server::register_ascii(&raw mut ascii::DESCRIPTOR) };
    if registered {
        EXTENSION_ERROR_CODE_EXTENSION_SUCCESS
    } else {
        EXTENSION_ERROR_CODE_EXTENSION_FATAL
    }
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
