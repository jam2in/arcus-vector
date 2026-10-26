//! arcus-vector: vector search as an extension to arcus-memcached.

mod aio;
mod ascii;
mod error;
mod memcached;
mod protocol;

use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::memcached::ffi::{
    EXTENSION_ERROR_CODE, EXTENSION_ERROR_CODE_EXTENSION_FATAL,
    EXTENSION_ERROR_CODE_EXTENSION_SUCCESS, GET_SERVER_API,
    extension_type_t_EXTENSION_ASCII_PROTOCOL,
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
    let server = unsafe { get_server_api() };
    if server.is_null() {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    }
    memcached::server::set(server);
    if !aio::start() {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    }
    // SAFETY: `server` was null-checked above and points to the core's static handle.
    let extension = unsafe { (*server).extension };
    if extension.is_null() {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    }
    // SAFETY: `extension` was null-checked above and points to the core's static extension table.
    let Some(register) = (unsafe { (*extension).register_extension }) else {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    };

    // SAFETY: `DESCRIPTOR` is static and only the core touches it from here on (it writes `next`).
    let registered = unsafe {
        register(
            extension_type_t_EXTENSION_ASCII_PROTOCOL,
            (&raw mut ascii::DESCRIPTOR).cast(),
        )
    };
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
