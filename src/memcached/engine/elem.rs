use std::ptr;

use super::{CRLF, Engine, Result, engine};
use crate::error::Error;
use crate::memcached::ffi::{eitem, eitem_info};

pub struct Elem {
    engine: &'static Engine,
    elem: *mut eitem,
    info: eitem_info,
}

impl Elem {
    pub fn new(elem_ptr: usize) -> Result<Self> {
        let engine = engine()?;

        let elem: *mut eitem = ptr::with_exposed_provenance_mut(elem_ptr);
        let mut info = eitem_info::default();

        if !elem.is_null() {
            // SAFETY: no connection; the element is kept alive by its reference.
            unsafe { engine.get_elem_info(ptr::null(), elem, &mut info) };
        } else {
            Err(Error::Internal)?;
        }

        Ok(Self { engine, elem, info })
    }

    pub fn field(&self) -> Option<&[u8]> {
        let info = &self.info;

        if info.score.is_null() {
            return None;
        }

        // SAFETY: `nscore` bytes, kept alive by the element's reference.
        let field = unsafe { std::slice::from_raw_parts(info.score, usize::from(info.nscore)) };

        Some(field)
    }

    pub fn value(&self) -> Option<&[u8]> {
        let info = &self.info;

        if info.value.is_null() || info.naddnl != 0 {
            return None;
        }

        // SAFETY: `nbytes` bytes, kept alive by the element's reference.
        let value =
            unsafe { std::slice::from_raw_parts(info.value.cast::<u8>(), info.nbytes as usize) };

        Some(value.strip_suffix(CRLF).unwrap_or(value))
    }

    /// # Safety
    ///
    /// No other thread may access the element while the slice lives.
    pub unsafe fn value_mut(&mut self) -> Option<&mut [u8]> {
        let info = &self.info;

        if info.value.is_null() || info.naddnl != 0 {
            return None;
        }

        // SAFETY: as in `value`; the caller ensures exclusive access.
        let value = unsafe {
            std::slice::from_raw_parts_mut(info.value.cast::<u8>().cast_mut(), info.nbytes as usize)
        };

        let len = value.len() - if value.ends_with(CRLF) { CRLF.len() } else { 0 };
        Some(&mut value[..len])
    }

    pub fn release(self) {
        // SAFETY: no connection; our reference, given back once by consuming `self`.
        unsafe { self.engine.map_elem_release(ptr::null(), self.elem) };
    }
}
