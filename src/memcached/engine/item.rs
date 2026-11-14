use std::ptr;

use super::{CRLF, Result, engine};
use crate::error::Error;
use crate::memcached::ffi::{item, item_info};

/// An item the engine hands the event callback this runs in, read in place without a lock.
#[derive(Clone, Copy)]
pub struct Item {
    // engine: &'static Engine,
    // item: *mut item,
    info: item_info,
}

impl Item {
    pub fn new(item_ptr: usize) -> Result<Self> {
        let engine = engine()?;
        let item: *const item = ptr::with_exposed_provenance(item_ptr);

        let mut info = item_info::default();

        if !item.is_null() {
            let handle = engine.handle();
            let cookie = ptr::null();

            // SAFETY: the item is kept alive by its reference.
            unsafe { (engine.get_item_info)(handle, cookie, item, &raw mut info) };
        } else {
            Err(Error::Internal)?;
        }

        Ok(Self { info })
    }

    pub fn key(&self) -> Option<&[u8]> {
        let info = &self.info;

        if info.key.is_null() {
            return None;
        }

        // SAFETY: `nkey` bytes, kept alive by the callback.
        let key =
            unsafe { std::slice::from_raw_parts(info.key.cast::<u8>(), usize::from(info.nkey)) };

        Some(key)
    }

    /// The value without its CRLF.
    pub fn value(&self) -> Option<&[u8]> {
        let info = &self.info;

        if info.value.is_null() || info.naddnl != 0 {
            return None;
        }

        // SAFETY: `nbytes` bytes, kept alive by the callback.
        let value =
            unsafe { std::slice::from_raw_parts(info.value.cast::<u8>(), info.nbytes as usize) };

        Some(value.strip_suffix(CRLF).unwrap_or(value))
    }
}
