#![expect(dead_code, reason = "the commands that use the engine are not in yet")]

#[rustfmt::skip]
mod fns;

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

use super::ffi::{
    ENGINE_ERROR_CODE, ENGINE_ERROR_CODE_ENGINE_E2BIG, ENGINE_ERROR_CODE_ENGINE_EBADTYPE,
    ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT, ENGINE_ERROR_CODE_ENGINE_ENOMEM,
    ENGINE_ERROR_CODE_ENGINE_EOVERFLOW, ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK,
    ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS, ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT,
    ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_HANDLE, ENGINE_ITEM_TYPE_ITEM_TYPE_MAP, eitem,
    eitem_info, elems_result, engine_interface_v1, field_t, item, item_attr, item_info,
};
use super::server::{self, Connection};
use crate::error::Error;

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

type Result<T> = std::result::Result<T, Error>;

const ITEM_TYPE_MAP: c_int = ENGINE_ITEM_TYPE_ITEM_TYPE_MAP as c_int;
const CRLF: &[u8] = b"\r\n";

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

    /// For work no connection asked for: only the calls that never look at the cookie
    /// (`get_elem_info`, `get_item_info`, `map_elem_release`).
    pub fn background() -> Result<Self> {
        Ok(Self {
            engine: engine()?,
            cookie: ptr::null(),
        })
    }

    fn handle(&self) -> *mut ENGINE_HANDLE {
        ptr::from_ref(self.engine.interface).cast_mut().cast()
    }

    /// Creates a Map with `value` as the item's own value, which must end in CRLF.
    pub fn create_map(&self, key: &str, value: &[u8], mut attr: item_attr) -> Result<()> {
        let (h, c) = (self.handle(), self.cookie);
        let (k, nk) = (key.as_ptr().cast(), key_len(key)?);
        let (v, nv) = (value.as_ptr().cast(), c_len(value)?);
        // SAFETY: `key`, `value` and `attr` outlive the call, which copies them.
        let ret = unsafe {
            (self.engine.map_struct_create_with_value)(h, c, k, nk, v, nv, &raw mut attr, 0)
        };
        ok(ret)
    }

    /// Removes the item at `key`; a Map's elements go later, from the engine's own thread.
    pub fn remove(&self, key: &str) -> Result<()> {
        let (h, c) = (self.handle(), self.cookie);
        let (k, nk) = (key.as_ptr().cast(), key.len());
        // SAFETY: `key` outlives the call.
        let ret = unsafe { (self.engine.remove)(h, c, k, nk, 0, 0) };
        ok(ret)
    }

    /// Writes one element at `field`, replacing any there; whether it replaced one.
    pub fn upsert_elem(&self, key: &str, field: &str, value: &[u8]) -> Result<bool> {
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
        if let Err(e) = ok(ret) {
            // SAFETY: the insert failed, so `elem` was never linked and is still ours to free.
            unsafe { (self.engine.map_elem_free)(h, c, elem) };
            return Err(e);
        }
        Ok(replaced)
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
        if result.elem_array.is_null() {
            return Err(Error::Internal);
        }
        // SAFETY: the engine filled `elem_count` entries of the array it allocated, each with a
        // reference taken for us.
        let elems = unsafe {
            std::slice::from_raw_parts_mut(result.elem_array, result.elem_count as usize)
        };
        let mut copy = Vec::new();
        let copied = elems.first().and_then(|&elem| {
            // SAFETY: the reference the engine took keeps `elem` allocated for the copy.
            unsafe {
                self.with_elem_at(elem, |_, value| {
                    copy.try_reserve_exact(value.len())
                        .map(|()| copy.extend_from_slice(value))
                })
            }
        });
        // SAFETY: the references the engine took for us, given back once, after the last read.
        unsafe { self.release_elems(elems) };
        // SAFETY: the engine `malloc`ed the array for us; nothing reads it after this.
        unsafe { free(result.elem_array.cast()) };
        match copied {
            Some(Ok(())) => Ok(copy),
            Some(Err(_)) => Err(Error::OutOfMemory),
            None => Err(Error::Internal),
        }
    }

    /// Gives back element references.
    ///
    /// # Safety
    ///
    /// Each element must be one we hold a reference on, given back only here; nothing may read it
    /// afterwards. The engine hands out at most `max_map_size` at once, so they fit a `c_int`.
    pub unsafe fn release_elems(&self, elems: &mut [*mut eitem]) {
        let (h, c) = (self.handle(), self.cookie);
        let n = elems.len() as c_int;
        // SAFETY: the caller guarantees each is a reference we hold.
        unsafe { (self.engine.map_elem_release)(h, c, elems.as_mut_ptr(), n) };
    }

    /// Reads an element's field and value (without its CRLF) in place, without a lock.
    ///
    /// # Safety
    ///
    /// `elem` must stay allocated for the call: one we hold a reference on, or one the engine
    /// handed to the event callback this runs in.
    pub unsafe fn with_elem_at<T>(
        &self,
        elem: *const eitem,
        f: impl FnOnce(&[u8], &[u8]) -> T,
    ) -> Option<T> {
        if elem.is_null() {
            return None;
        }
        // SAFETY: `eitem_info` is a plain C struct, for which all-zero bytes are valid.
        let mut info: eitem_info = unsafe { std::mem::zeroed() };
        let (h, c) = (self.handle(), self.cookie);
        // SAFETY: the caller guarantees `elem` is allocated.
        unsafe { (self.engine.get_elem_info)(h, c, ITEM_TYPE_MAP, elem, &raw mut info) };
        if info.score.is_null() || info.value.is_null() || info.naddnl != 0 {
            return None;
        }
        // SAFETY: the engine reports `nscore` and `nbytes` bytes there; `elem` outlives `f`.
        let (field, value) = unsafe {
            (
                std::slice::from_raw_parts(info.score, usize::from(info.nscore)),
                std::slice::from_raw_parts(info.value.cast::<u8>(), info.nbytes as usize),
            )
        };
        // Every stored value ends in CRLF; one that does not is passed on whole.
        Some(f(field, value.strip_suffix(CRLF).unwrap_or(value)))
    }

    /// Reads an item's key in place, without a lock.
    ///
    /// # Safety
    ///
    /// `item` must stay allocated for the call, as one handed to the event callback this runs in.
    pub unsafe fn with_key_at<T>(
        &self,
        item: *const item,
        f: impl FnOnce(&[u8]) -> T,
    ) -> Option<T> {
        // SAFETY: the caller's guarantee.
        let info = unsafe { self.item_info(item) }?;
        if info.key.is_null() {
            return None;
        }
        // SAFETY: the engine reports `nkey` bytes there; `item` outlives `f`.
        let key =
            unsafe { std::slice::from_raw_parts(info.key.cast::<u8>(), usize::from(info.nkey)) };
        Some(f(key))
    }

    /// Reads an item's value (without its CRLF) in place, without a lock.
    ///
    /// # Safety
    ///
    /// Same as [`Store::with_key_at`].
    pub unsafe fn with_item_value_at<T>(
        &self,
        item: *const item,
        f: impl FnOnce(&[u8]) -> T,
    ) -> Option<T> {
        // SAFETY: the caller's guarantee.
        let info = unsafe { self.item_info(item) }?;
        if info.value.is_null() || info.naddnl != 0 {
            return None;
        }
        // SAFETY: the engine reports `nbytes` bytes there; `item` outlives `f`.
        let value =
            unsafe { std::slice::from_raw_parts(info.value.cast::<u8>(), info.nbytes as usize) };
        // Every stored value ends in CRLF; one that does not is passed on whole.
        Some(f(value.strip_suffix(CRLF).unwrap_or(value)))
    }

    /// # Safety
    ///
    /// `item` must stay allocated for the call.
    unsafe fn item_info(&self, item: *const item) -> Option<item_info> {
        if item.is_null() {
            return None;
        }
        // SAFETY: `item_info` is a plain C struct, for which all-zero bytes are valid.
        let mut info: item_info = unsafe { std::mem::zeroed() };
        let (h, c) = (self.handle(), self.cookie);
        // SAFETY: the caller guarantees `item` is allocated.
        unsafe { (self.engine.get_item_info)(h, c, item, &raw mut info) }.then_some(info)
    }
}

/// The engine functions arcus-vector calls.
struct Engine {
    interface: &'static engine_interface_v1,
    remove: fns::Remove,
    map_struct_create_with_value: fns::MapCreate,
    map_elem_alloc: fns::ElemAlloc,
    map_elem_free: fns::ElemFree,
    map_elem_release: fns::ElemRelease,
    map_elem_insert: fns::ElemInsert,
    map_elem_update: fns::ElemUpdate,
    map_elem_delete: fns::ElemDelete,
    map_elem_get: fns::ElemGet,
    get_elem_info: fns::GetElemInfo,
    get_item_info: fns::GetItemInfo,
}

impl Engine {
    /// `None` if the engine lacks a function arcus-vector calls.
    fn new(interface: &'static engine_interface_v1) -> Option<Self> {
        Some(Self {
            interface,
            remove: interface.remove?,
            map_struct_create_with_value: interface.map_struct_create_with_value?,
            map_elem_alloc: interface.map_elem_alloc?,
            map_elem_free: interface.map_elem_free?,
            map_elem_release: interface.map_elem_release?,
            map_elem_insert: interface.map_elem_insert?,
            map_elem_update: interface.map_elem_update?,
            map_elem_delete: interface.map_elem_delete?,
            map_elem_get: interface.map_elem_get?,
            get_elem_info: interface.get_elem_info?,
            get_item_info: interface.get_item_info?,
        })
    }
}

/// `None` once the engine turned out to lack a function arcus-vector calls.
static ENGINE: OnceLock<Option<Engine>> = OnceLock::new();

/// The engine, checked once; one lacking a function stays `NOT_SUPPORTED`.
fn engine() -> Result<&'static Engine> {
    if let Some(engine) = ENGINE.get() {
        return engine.as_ref().ok_or(Error::NotSupported);
    }
    let interface = server::engine().ok_or(Error::Internal)?;
    ENGINE
        .get_or_init(|| Engine::new(interface))
        .as_ref()
        .ok_or(Error::NotSupported)
}

/// `EWOULDBLOCK` is a write waiting on a replica's ack, which is already done here.
fn ok(ret: ENGINE_ERROR_CODE) -> Result<()> {
    match ret {
        ENGINE_ERROR_CODE_ENGINE_SUCCESS | ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK => Ok(()),
        ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT => Err(Error::NotFound),
        ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS => Err(Error::Exists),
        ENGINE_ERROR_CODE_ENGINE_EBADTYPE => Err(Error::TypeMismatch),
        ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT => Err(Error::NotFoundElement),
        ENGINE_ERROR_CODE_ENGINE_EOVERFLOW => Err(Error::Overflowed),
        ENGINE_ERROR_CODE_ENGINE_E2BIG => Err(Error::TooLargeValue),
        ENGINE_ERROR_CODE_ENGINE_ENOMEM => Err(Error::OutOfMemory),
        _ => Err(Error::Internal),
    }
}

fn key_len(key: &str) -> Result<c_int> {
    c_int::try_from(key.len()).map_err(|_| Error::BadCommandLine)
}

fn c_len(bytes: &[u8]) -> Result<c_int> {
    c_int::try_from(bytes.len()).map_err(|_| Error::TooLargeValue)
}

fn selector(field: &str) -> field_t {
    field_t {
        value: field.as_ptr().cast::<c_char>().cast_mut(),
        length: field.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memcached::ffi::ENGINE_ERROR_CODE_ENGINE_FAILED;

    #[test]
    fn engine_codes_become_arcus_replies() {
        assert_eq!(ok(ENGINE_ERROR_CODE_ENGINE_SUCCESS), Ok(()));
        assert_eq!(ok(ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK), Ok(()));
        assert_eq!(
            ok(ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT),
            Err(Error::NotFound)
        );
        assert_eq!(ok(ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS), Err(Error::Exists));
        assert_eq!(
            ok(ENGINE_ERROR_CODE_ENGINE_EBADTYPE),
            Err(Error::TypeMismatch)
        );
        assert_eq!(
            ok(ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT),
            Err(Error::NotFoundElement)
        );
        assert_eq!(
            ok(ENGINE_ERROR_CODE_ENGINE_EOVERFLOW),
            Err(Error::Overflowed)
        );
        assert_eq!(
            ok(ENGINE_ERROR_CODE_ENGINE_E2BIG),
            Err(Error::TooLargeValue)
        );
        assert_eq!(ok(ENGINE_ERROR_CODE_ENGINE_ENOMEM), Err(Error::OutOfMemory));
        assert_eq!(ok(ENGINE_ERROR_CODE_ENGINE_FAILED), Err(Error::Internal));
    }

    #[test]
    fn an_engine_without_maps_is_refused() {
        // SAFETY: all-zero is a valid interface: every function `None`.
        let engine: engine_interface_v1 = unsafe { std::mem::zeroed() };
        assert!(Engine::new(Box::leak(Box::new(engine))).is_none());
    }

    #[test]
    fn an_index_map_is_readable_and_raises_events() {
        let attr = map_attr(4000, 60);
        assert_eq!((attr.readable, attr.event), (1, 1));
        assert_eq!((attr.maxcount, attr.exptime), (4000, 60));
        assert_eq!(map_attr(u32::MAX, 0).maxcount, i32::MAX);
    }
}
