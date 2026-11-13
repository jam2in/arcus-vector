use std::os::raw::{c_int, c_void};
use std::ptr;

use super::{
    CRLF, Elem, Engine, ITEM_TYPE_MAP, Result, engine, free, key_len, ok, selector, value_len,
};
use crate::error::Error;
use crate::memcached::ffi::{
    ENGINE_HANDLE, ENGINE_ITEM_ATTR_ATTR_TYPE, eitem, eitem_info, elems_result, item_attr,
};
use crate::memcached::server::Connection;

/// The attributes an index's Map is created with: `maxcount` 0 is the engine's default, and
/// `exptime` 0 never expires.
pub fn map_attr(maxcount: u32, exptime: u32) -> item_attr {
    // SAFETY: `item_attr` is a plain C struct of integers, for which all-zero bytes are valid.
    let mut attr: item_attr = unsafe { std::mem::zeroed() };
    attr.readable = 1;
    // Only a Map with this flag raises element events, and it carries over to replicas.
    attr.event = 1;
    attr.maxcount = i32::try_from(maxcount).unwrap_or(i32::MAX);
    attr.exptime = exptime;
    attr
}

/// The engine, making its calls for one connection.
#[derive(Clone, Copy)]
pub struct Store {
    engine: &'static Engine,
    cookie: *const c_void,
}

impl Store {
    pub fn of(conn: Connection) -> Result<Self> {
        Ok(Self {
            engine: engine()?,
            cookie: conn.cookie(),
        })
    }

    /// Creates a Map with `value` as the item's own value, which must end in CRLF.
    pub fn create_map(&self, key: &str, value: &[u8], mut attr: item_attr) -> Result<()> {
        let (h, c) = (self.handle(), self.cookie);
        let (k, nk) = (key.as_ptr().cast(), key_len(key)?);
        let (v, nv) = (value.as_ptr().cast(), value_len(value)?);
        // SAFETY: `key`, `value` and `attr` outlive the call, which copies them.
        let ret = unsafe {
            (self.engine.map_struct_create_with_value)(h, c, k, nk, v, nv, &raw mut attr, 0)
        };
        ok(ret)
    }

    /// Whether an item of any type is at `key`.
    pub fn exists(&self, key: &str) -> Result<bool> {
        let (h, c) = (self.handle(), self.cookie);
        let (k, nk) = (key.as_ptr().cast(), key_len(key)?);
        let mut id = ENGINE_ITEM_ATTR_ATTR_TYPE;
        // SAFETY: `item_attr` is a plain C struct, for which all-zero bytes are valid.
        let mut attr: item_attr = unsafe { std::mem::zeroed() };
        // SAFETY: `key`, `id` and `attr` outlive the call, which reads one id and fills `attr`.
        let ret = unsafe { (self.engine.getattr)(h, c, k, nk, &raw mut id, 1, &raw mut attr, 0) };
        match ok(ret) {
            Ok(()) => Ok(true),
            Err(Error::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Removes the item at `key`; a Map's elements go later, from the engine's own thread.
    pub fn remove(&self, key: &str) -> Result<()> {
        let (h, c) = (self.handle(), self.cookie);
        let (k, nk) = (key.as_ptr().cast(), key.len());
        // SAFETY: `key` outlives the call.
        let ret = unsafe { (self.engine.remove)(h, c, k, nk, 0, 0) };
        ok(ret)
    }

    /// Writes one element at `field`, replacing any there.
    pub fn upsert_elem(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let (h, c) = (self.handle(), self.cookie);
        let (k, nk) = (key.as_ptr().cast(), key_len(key)?);
        // The engine keeps these lengths in a `u8` and a `u16`.
        let (nfield, nbytes) = (field.len(), value.len() + CRLF.len());
        u8::try_from(nfield).map_err(|_| Error::BadCommandLine)?;
        u16::try_from(nbytes).map_err(|_| Error::TooLargeValue)?;
        let mut elem: *mut eitem = ptr::null_mut();
        // SAFETY: `key` outlives the call, and `elem` is a local out-pointer.
        let ret =
            unsafe { (self.engine.map_elem_alloc)(h, c, k, nk, nfield, nbytes, &raw mut elem) };
        ok(ret)?;
        // SAFETY: `eitem_info` is a plain C struct, for which all-zero bytes are valid.
        let mut info: eitem_info = unsafe { std::mem::zeroed() };
        // SAFETY: `elem` is the unlinked element just allocated.
        unsafe { (self.engine.get_elem_info)(h, c, ITEM_TYPE_MAP, elem, &raw mut info) };
        // SAFETY: the engine allocated `nfield` bytes at `score` and `nbytes` at `value`.
        unsafe {
            let dst = info.value.cast::<u8>().cast_mut();
            ptr::copy_nonoverlapping(field.as_ptr(), info.score.cast_mut(), nfield);
            ptr::copy_nonoverlapping(value.as_ptr(), dst, value.len());
            ptr::copy_nonoverlapping(CRLF.as_ptr(), dst.add(value.len()), CRLF.len());
        }
        let (mut replaced, mut created) = (false, false);
        let (r, cr) = (&raw mut replaced, &raw mut created);
        // SAFETY: `elem` is ours and unlinked; the out-params are locals.
        let ret = unsafe {
            (self.engine.map_elem_insert)(h, c, k, nk, elem, true, ptr::null_mut(), r, cr, 0)
        };
        match ok(ret) {
            Ok(()) => Ok(()),
            Err(e) => {
                // SAFETY: the insert failed, so `elem` was never linked and is still ours to free.
                unsafe { (self.engine.map_elem_free)(h, c, elem) };
                Err(e)
            }
        }
    }

    /// Replaces the value of the element at `field`; `NOT_FOUND_ELEMENT` if there is none.
    pub fn update_elem(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let (h, c) = (self.handle(), self.cookie);
        let (k, nk) = (key.as_ptr().cast(), key_len(key)?);
        // The engine keeps the length in a `u16`.
        let nbytes = u16::try_from(value.len() + CRLF.len()).map_err(|_| Error::TooLargeValue)?;
        let mut terminated = Vec::new();
        terminated
            .try_reserve_exact(usize::from(nbytes))
            .map_err(|_| Error::OutOfMemory)?;
        terminated.extend_from_slice(value);
        terminated.extend_from_slice(CRLF);
        let (v, nv) = (terminated.as_ptr().cast(), c_int::from(nbytes));
        let field = selector(field);
        // SAFETY: `key`, `field` and `terminated` outlive the call.
        let ret = unsafe { (self.engine.map_elem_update)(h, c, k, nk, &raw const field, v, nv, 0) };
        ok(ret)
    }

    /// Removes the element at `field`; `NOT_FOUND_ELEMENT` if there is none.
    pub fn delete_elem(&self, key: &str, field: &str) -> Result<()> {
        let (h, c) = (self.handle(), self.cookie);
        let (k, nk) = (key.as_ptr().cast(), key_len(key)?);
        let field = selector(field);
        let (mut deleted, mut dropped) = (0u32, false);
        let (d, dr) = (&raw mut deleted, &raw mut dropped);
        // SAFETY: `key`, `field` and the out-params outlive the call.
        let ret = unsafe {
            (self.engine.map_elem_delete)(h, c, k, nk, 1, &raw const field, false, d, dr, 0)
        };
        ok(ret)
    }

    /// A copy of the value at `field`, without its CRLF.
    pub fn get_elem(&self, key: &str, field: &str) -> Result<Vec<u8>> {
        let (h, c) = (self.handle(), self.cookie);
        let (k, nk) = (key.as_ptr().cast(), key_len(key)?);
        let field = selector(field);
        // SAFETY: `elems_result` is a plain C struct; all-zero bytes are an empty result.
        let mut result: elems_result = unsafe { std::mem::zeroed() };
        let r = &raw mut result;
        // SAFETY: `key`, `field` and `result` outlive the call.
        let ret = unsafe {
            (self.engine.map_elem_get)(h, c, k, nk, 1, &raw const field, false, false, r, 0)
        };
        ok(ret)?;
        // A success with no element leaves the array unset.
        if result.elem_array.is_null() {
            return Err(Error::NotFoundElement);
        }
        // SAFETY: the engine filled `elem_count` entries of the array it allocated, each with a
        // reference taken for us.
        let elems = unsafe {
            std::slice::from_raw_parts_mut(result.elem_array, result.elem_count as usize)
        };
        let mut copy = Vec::new();
        let copied = elems.first().and_then(|&elem| {
            let elem = Elem::new(elem.expose_provenance()).ok()?;
            let value = elem.value()?;
            Some(
                copy.try_reserve_exact(value.len())
                    .map(|()| copy.extend_from_slice(value)),
            )
        });
        // SAFETY: the references the engine took for us, given back once, after the last read.
        unsafe { self.engine.release_elems(elems) };
        // SAFETY: the engine `malloc`ed the array for us; nothing reads it after this.
        unsafe { free(result.elem_array.cast()) };
        match copied {
            Some(Ok(())) => Ok(copy),
            Some(Err(_)) => Err(Error::OutOfMemory),
            None => Err(Error::Internal),
        }
    }

    fn handle(&self) -> *mut ENGINE_HANDLE {
        self.engine.handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_map_is_readable_and_raises_events() {
        let attr = map_attr(4000, 60);
        assert_eq!((attr.readable, attr.event), (1, 1));
        assert_eq!((attr.maxcount, attr.exptime), (4000, 60));
        assert_eq!(map_attr(u32::MAX, 0).maxcount, i32::MAX);
    }
}
