use std::ptr;

use super::{CRLF, Engine, ITEM_TYPE_MAP, Result, engine};
use crate::error::Error;
use crate::memcached::ffi::{eitem, eitem_info, item, item_info};

/// An item the engine hands the event callback this runs in, read in place without a lock.
#[derive(Clone, Copy)]
pub struct Item {
    info: item_info,
}

impl Item {
    /// `INTERNAL` for a null item, or one the engine reports nothing on.
    pub fn new(item_ptr: usize) -> Result<Self> {
        let engine = engine()?;
        let item: *const item = ptr::with_exposed_provenance(item_ptr);
        if item.is_null() {
            return Err(Error::Internal);
        }
        // SAFETY: `item_info` is a plain C struct, for which all-zero bytes are valid.
        let mut info: item_info = unsafe { std::mem::zeroed() };
        let (h, c) = (engine.handle(), ptr::null());
        // SAFETY: the engine handed the item to the event callback this runs in, which keeps it
        // allocated.
        let got = unsafe { (engine.get_item_info)(h, c, item, &raw mut info) };
        got.then_some(Self { info }).ok_or(Error::Internal)
    }

    pub fn key(&self) -> Option<&[u8]> {
        let info = &self.info;
        if info.key.is_null() {
            return None;
        }
        // SAFETY: the engine reports `nkey` bytes there, in the item the callback holds.
        Some(unsafe { std::slice::from_raw_parts(info.key.cast::<u8>(), usize::from(info.nkey)) })
    }

    /// The value without its CRLF.
    pub fn value(&self) -> Option<&[u8]> {
        let info = &self.info;
        if info.value.is_null() || info.naddnl != 0 {
            return None;
        }
        // SAFETY: the engine reports `nbytes` bytes there, in the item the callback holds.
        let value =
            unsafe { std::slice::from_raw_parts(info.value.cast::<u8>(), info.nbytes as usize) };
        // Every stored value ends in CRLF; one that does not is passed on whole.
        Some(value.strip_suffix(CRLF).unwrap_or(value))
    }
}

/// An element we hold a reference on, which keeps it allocated; read in place without a lock.
pub struct Elem {
    engine: &'static Engine,
    elem: *mut eitem,
    info: eitem_info,
}

impl Elem {
    /// Fails only without the engine, as an element whose reference cannot be given back would
    /// leak.
    pub fn new(elem_ptr: usize) -> Result<Self> {
        let engine = engine()?;
        let elem: *mut eitem = ptr::with_exposed_provenance_mut(elem_ptr);
        // SAFETY: `eitem_info` is a plain C struct, for which all-zero bytes are valid.
        let mut info: eitem_info = unsafe { std::mem::zeroed() };
        if !elem.is_null() {
            let (h, c) = (engine.handle(), ptr::null());
            // SAFETY: the reference we hold keeps the element allocated.
            unsafe { (engine.get_elem_info)(h, c, ITEM_TYPE_MAP, elem, &raw mut info) };
        }
        Ok(Self { engine, elem, info })
    }

    pub fn field(&self) -> Option<&[u8]> {
        let info = &self.info;
        if info.score.is_null() {
            return None;
        }
        // SAFETY: the engine reports `nscore` bytes there, in the element our reference holds
        // until `release` takes `self`.
        Some(unsafe { std::slice::from_raw_parts(info.score, usize::from(info.nscore)) })
    }

    /// The value without its CRLF.
    pub fn value(&self) -> Option<&[u8]> {
        let info = &self.info;
        if info.value.is_null() || info.naddnl != 0 {
            return None;
        }
        // SAFETY: as for `field`, with `nbytes` bytes.
        let value =
            unsafe { std::slice::from_raw_parts(info.value.cast::<u8>(), info.nbytes as usize) };
        // Every stored value ends in CRLF; one that does not is passed on whole.
        Some(value.strip_suffix(CRLF).unwrap_or(value))
    }

    /// The value without its CRLF, to change in place.
    ///
    /// # Safety
    ///
    /// Nothing else may read or write the element while the slice lives.
    pub unsafe fn value_mut(&mut self) -> Option<&mut [u8]> {
        let info = &self.info;
        if info.value.is_null() || info.naddnl != 0 {
            return None;
        }
        // SAFETY: as for `value`, and the caller's; the engine hands out a const pointer to the
        // element's own memory, which it allocated writable.
        let value = unsafe {
            std::slice::from_raw_parts_mut(info.value.cast::<u8>().cast_mut(), info.nbytes as usize)
        };
        let len = value.len() - if value.ends_with(CRLF) { CRLF.len() } else { 0 };
        Some(&mut value[..len])
    }

    /// Gives back the reference we hold.
    pub fn release(self) {
        if self.elem.is_null() {
            return;
        }
        // SAFETY: the reference is ours, given back only here, as `self` goes.
        unsafe { self.engine.release_elems(&mut [self.elem]) };
    }
}
