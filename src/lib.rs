//! arcus-vector: vector search as an extension to arcus-memcached.
//!
//! memcached loads this library with `-X` and calls [`memcached_extensions_initialize`], which
//! registers the ASCII protocol extension for `vop` commands.

mod ascii;
mod memcached;

use std::os::raw::c_char;

use crate::memcached::ffi::{
    EXTENSION_ERROR_CODE, EXTENSION_ERROR_CODE_EXTENSION_FATAL,
    EXTENSION_ERROR_CODE_EXTENSION_SUCCESS, GET_SERVER_API,
    extension_type_t_EXTENSION_ASCII_PROTOCOL,
};

/// The symbol memcached looks up when it loads this library with `-X`.
#[unsafe(no_mangle)]
pub extern "C" fn memcached_extensions_initialize(
    _config: *const c_char,
    get_server_api: GET_SERVER_API,
) -> EXTENSION_ERROR_CODE {
    let Some(get_server_api) = get_server_api else {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    };
    // SAFETY: `get_server_api` is the core's own function; it takes no arguments and returns its
    // static server handle.
    let server = unsafe { get_server_api() };
    if server.is_null() {
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

    // SAFETY: the core keeps the pointer for the life of the process and writes the descriptor's
    // `next` to link it into its list. `DESCRIPTOR` is a static, so it outlives that use, and no
    // other code touches it after it is handed over here.
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
