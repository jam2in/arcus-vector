//! The interface memcached gives an extension and its engine.

pub mod engine;
pub mod ffi;
pub mod server;

/// arcus-memcached's `KEY_MAX_LENGTH`, which is in `memcached.h`, not the headers bound here.
pub const KEY_MAX_LENGTH: usize = 16000;
/// arcus-memcached's default `max_element_bytes`, set in `memcached.c`: the core refuses a longer
/// element value, CRLF included, before it reaches the engine.
pub const MAX_ELEMENT_BYTES: usize = 16 * 1024;
