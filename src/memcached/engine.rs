#![expect(dead_code, reason = "the commands that use the engine are not in yet")]

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

use super::ffi::{
    ENGINE_ERROR_CODE, ENGINE_ERROR_CODE_ENGINE_EBADTYPE, ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT,
    ENGINE_ERROR_CODE_ENGINE_ENOMEM, ENGINE_ERROR_CODE_ENGINE_EOVERFLOW,
    ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK, ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS,
    ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT, ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_HANDLE,
    ENGINE_ITEM_TYPE_ITEM_TYPE_MAP, eitem, eitem_info, elems_result, engine_interface_v1, field_t,
    item, item_attr, item_info,
};
use super::server::{self, Connection};
use crate::error::Error;

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

type Result<T> = std::result::Result<T, Error>;

const ITEM_TYPE_MAP: c_int = ENGINE_ITEM_TYPE_ITEM_TYPE_MAP as c_int;
const CRLF: &[u8] = b"\r\n";

static ENGINE: OnceLock<&'static engine_interface_v1> = OnceLock::new();

fn engine() -> Result<&'static engine_interface_v1> {
    if let Some(engine) = ENGINE.get() {
        return Ok(engine);
    }
    let engine = server::engine().ok_or(Error::Internal)?;
    Ok(ENGINE.get_or_init(|| engine))
}

fn check(code: ENGINE_ERROR_CODE) -> Result<()> {
    match code {
        ENGINE_ERROR_CODE_ENGINE_SUCCESS => Ok(()),
        ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT => Err(Error::NotFound),
        ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS => Err(Error::Exists),
        ENGINE_ERROR_CODE_ENGINE_EBADTYPE => Err(Error::TypeMismatch),
        ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT => Err(Error::NotFoundElement),
        ENGINE_ERROR_CODE_ENGINE_EOVERFLOW => Err(Error::Overflowed),
        ENGINE_ERROR_CODE_ENGINE_ENOMEM => Err(Error::OutOfMemory),
        _ => Err(Error::Internal),
    }
}

/// A write that would block is done and waiting on a replica's ack.
fn written(code: ENGINE_ERROR_CODE) -> Result<()> {
    if code == ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK {
        return Ok(());
    }
    check(code)
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

/// The attributes an index's Map is created with; 0 for either means the engine's default.
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
    engine: &'static engine_interface_v1,
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
    /// (`get_elem_info`, `get_item_info`, `map_elem_release`, `get_config`).
    pub fn background() -> Result<Self> {
        Ok(Self {
            engine: engine()?,
            cookie: ptr::null(),
        })
    }

    fn handle(&self) -> *mut ENGINE_HANDLE {
        ptr::from_ref(self.engine).cast_mut().cast()
    }

    fn config(&self, key: &CStr) -> Option<u32> {
        let get_config = self.engine.get_config?;
        let mut value: u32 = 0;
        // SAFETY: `key` is NUL-terminated, and the engine writes a `uint32_t` for these keys.
        let code = unsafe {
            get_config(
                self.handle(),
                self.cookie,
                key.as_ptr(),
                (&raw mut value).cast(),
            )
        };
        (code == ENGINE_ERROR_CODE_ENGINE_SUCCESS).then_some(value)
    }

    pub fn max_element_bytes(&self) -> Option<u32> {
        self.config(c"max_element_bytes")
    }

    pub fn max_map_size(&self) -> Option<u32> {
        self.config(c"max_map_size")
    }

    /// Creates a Map with `value` as the item's own value, which must end in CRLF.
    pub fn create_map(&self, key: &str, value: &[u8], mut attr: item_attr) -> Result<()> {
        let create = self
            .engine
            .map_struct_create_with_value
            .ok_or(Error::NotSupported)?;
        // SAFETY: `key`, `value` and `attr` outlive the call, which copies them.
        written(unsafe {
            create(
                self.handle(),
                self.cookie,
                key.as_ptr().cast(),
                c_len(key.as_bytes())?,
                value.as_ptr().cast(),
                c_len(value)?,
                &raw mut attr,
                0,
            )
        })
    }

    /// Removes the item at `key`; a Map's elements go later, from the engine's own thread.
    pub fn remove(&self, key: &str) -> Result<()> {
        let remove = self.engine.remove.ok_or(Error::NotSupported)?;
        // SAFETY: `key` outlives the call.
        written(unsafe {
            remove(
                self.handle(),
                self.cookie,
                key.as_ptr().cast(),
                key.len(),
                0,
                0,
            )
        })
    }

    /// Writes one element at `field`, replacing any there; whether it replaced one.
    pub fn upsert_elem(&self, key: &str, field: &str, value: &[u8]) -> Result<bool> {
        let (Some(alloc), Some(free_elem), Some(elem_info), Some(insert)) = (
            self.engine.map_elem_alloc,
            self.engine.map_elem_free,
            self.engine.get_elem_info,
            self.engine.map_elem_insert,
        ) else {
            return Err(Error::NotSupported);
        };
        let nkey = c_len(key.as_bytes())?;
        let nbytes = value.len() + CRLF.len();
        let mut elem: *mut eitem = ptr::null_mut();
        // SAFETY: `key` outlives the call, and `elem` is a local out-pointer.
        check(unsafe {
            alloc(
                self.handle(),
                self.cookie,
                key.as_ptr().cast(),
                nkey,
                field.len(),
                nbytes,
                &raw mut elem,
            )
        })?;
        // SAFETY: `eitem_info` is a plain C struct, for which all-zero bytes are valid.
        let mut info: eitem_info = unsafe { std::mem::zeroed() };
        // SAFETY: `elem` is the unlinked element just allocated.
        unsafe {
            elem_info(
                self.handle(),
                self.cookie,
                ITEM_TYPE_MAP,
                elem,
                &raw mut info,
            );
        };
        if info.score.is_null()
            || info.value.is_null()
            || usize::from(info.nscore) != field.len()
            || info.nbytes as usize != nbytes
        {
            // SAFETY: `elem` was never linked, so it is still ours to free.
            unsafe { free_elem(self.handle(), self.cookie, elem) };
            return Err(Error::Internal);
        }
        // SAFETY: the engine reports room for `field` at `score` and `nbytes` at `value`.
        unsafe {
            let dst = info.value.cast::<u8>().cast_mut();
            ptr::copy_nonoverlapping(field.as_ptr(), info.score.cast_mut(), field.len());
            ptr::copy_nonoverlapping(value.as_ptr(), dst, value.len());
            ptr::copy_nonoverlapping(CRLF.as_ptr(), dst.add(value.len()), CRLF.len());
        }
        let (mut replaced, mut created) = (false, false);
        // SAFETY: `elem` is ours and unlinked; the out-params are locals.
        let code = unsafe {
            insert(
                self.handle(),
                self.cookie,
                key.as_ptr().cast(),
                nkey,
                elem,
                true,
                ptr::null_mut(),
                &raw mut replaced,
                &raw mut created,
                0,
            )
        };
        if let Err(e) = written(code) {
            // SAFETY: the insert failed, so `elem` was never linked and is still ours to free.
            unsafe { free_elem(self.handle(), self.cookie, elem) };
            return Err(e);
        }
        Ok(replaced)
    }

    /// Replaces the value of the element at `field`; `NOT_FOUND_ELEMENT` if there is none.
    pub fn update_elem(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let update = self.engine.map_elem_update.ok_or(Error::NotSupported)?;
        let mut terminated = Vec::new();
        terminated
            .try_reserve_exact(value.len() + CRLF.len())
            .map_err(|_| Error::OutOfMemory)?;
        terminated.extend_from_slice(value);
        terminated.extend_from_slice(CRLF);
        let selector = selector(field);
        // SAFETY: `key`, `selector` (borrowing `field`) and `terminated` outlive the call.
        written(unsafe {
            update(
                self.handle(),
                self.cookie,
                key.as_ptr().cast(),
                c_len(key.as_bytes())?,
                &raw const selector,
                terminated.as_ptr().cast(),
                c_len(&terminated)?,
                0,
            )
        })
    }

    /// Removes the element at `field`; `NOT_FOUND_ELEMENT` if there is none.
    pub fn delete_elem(&self, key: &str, field: &str) -> Result<()> {
        let delete = self.engine.map_elem_delete.ok_or(Error::NotSupported)?;
        let selector = selector(field);
        let (mut deleted, mut dropped) = (0u32, false);
        // SAFETY: `key`, `selector` and the out-params outlive the call.
        written(unsafe {
            delete(
                self.handle(),
                self.cookie,
                key.as_ptr().cast(),
                c_len(key.as_bytes())?,
                1,
                &raw const selector,
                false,
                &raw mut deleted,
                &raw mut dropped,
                0,
            )
        })?;
        if deleted == 0 {
            return Err(Error::NotFoundElement);
        }
        Ok(())
    }

    /// A copy of the value at `field`, without its CRLF.
    pub fn get_elem(&self, key: &str, field: &str) -> Result<Vec<u8>> {
        let get = self.engine.map_elem_get.ok_or(Error::NotSupported)?;
        let selector = selector(field);
        // SAFETY: `elems_result` is a plain C struct; all-zero bytes are an empty result.
        let mut result: elems_result = unsafe { std::mem::zeroed() };
        // SAFETY: `key`, `selector` and `result` outlive the call.
        let code = unsafe {
            get(
                self.handle(),
                self.cookie,
                key.as_ptr().cast(),
                c_len(key.as_bytes())?,
                1,
                &raw const selector,
                false,
                false,
                &raw mut result,
                0,
            )
        };
        if result.elem_array.is_null() {
            check(code)?;
            return Err(Error::NotFoundElement);
        }
        // SAFETY: the engine filled `elem_count` entries of the array it allocated, each with a
        // reference taken for us.
        let elems = unsafe {
            std::slice::from_raw_parts_mut(result.elem_array, result.elem_count as usize)
        };
        let mut copy = Vec::new();
        let copied = elems.first().map(|&elem| {
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
        check(code)?;
        match copied {
            None => Err(Error::NotFoundElement),
            Some(Some(Ok(()))) => Ok(copy),
            Some(Some(Err(_))) => Err(Error::OutOfMemory),
            Some(None) => Err(Error::Internal),
        }
    }

    /// Gives back element references.
    ///
    /// # Safety
    ///
    /// Each element must be one we hold a reference on, given back only here; nothing may read it
    /// afterwards.
    pub unsafe fn release_elems(&self, elems: &mut [*mut eitem]) {
        let Some(release) = self.engine.map_elem_release else {
            return;
        };
        for chunk in elems.chunks_mut(c_int::MAX as usize) {
            // SAFETY: the caller guarantees each is a reference we hold; `chunk` fits a `c_int`.
            unsafe {
                release(
                    self.handle(),
                    self.cookie,
                    chunk.as_mut_ptr(),
                    chunk.len() as c_int,
                );
            }
        }
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
        let elem_info = self.engine.get_elem_info?;
        if elem.is_null() {
            return None;
        }
        // SAFETY: `eitem_info` is a plain C struct, for which all-zero bytes are valid.
        let mut info: eitem_info = unsafe { std::mem::zeroed() };
        // SAFETY: the caller guarantees `elem` is allocated.
        unsafe {
            elem_info(
                self.handle(),
                self.cookie,
                ITEM_TYPE_MAP,
                elem,
                &raw mut info,
            );
        };
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
        Some(f(value.strip_suffix(CRLF).unwrap_or(value)))
    }

    /// # Safety
    ///
    /// `item` must stay allocated for the call.
    unsafe fn item_info(&self, item: *const item) -> Option<item_info> {
        let get_item_info = self.engine.get_item_info?;
        if item.is_null() {
            return None;
        }
        // SAFETY: `item_info` is a plain C struct, for which all-zero bytes are valid.
        let mut info: item_info = unsafe { std::mem::zeroed() };
        // SAFETY: the caller guarantees `item` is allocated.
        unsafe { get_item_info(self.handle(), self.cookie, item, &raw mut info) }.then_some(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memcached::ffi::ENGINE_ERROR_CODE_ENGINE_FAILED;

    #[test]
    fn engine_codes_become_arcus_replies() {
        assert_eq!(check(ENGINE_ERROR_CODE_ENGINE_SUCCESS), Ok(()));
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT),
            Err(Error::NotFound)
        );
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS),
            Err(Error::Exists)
        );
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_EBADTYPE),
            Err(Error::TypeMismatch)
        );
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT),
            Err(Error::NotFoundElement)
        );
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_EOVERFLOW),
            Err(Error::Overflowed)
        );
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_ENOMEM),
            Err(Error::OutOfMemory)
        );
        assert_eq!(check(ENGINE_ERROR_CODE_ENGINE_FAILED), Err(Error::Internal));
    }

    #[test]
    fn a_write_waiting_on_a_replica_is_done() {
        assert_eq!(written(ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK), Ok(()));
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK),
            Err(Error::Internal)
        );
    }

    #[test]
    fn an_index_map_is_readable_and_raises_events() {
        let attr = map_attr(4000, 60);
        assert_eq!((attr.readable, attr.event), (1, 1));
        assert_eq!((attr.maxcount, attr.exptime), (4000, 60));
        assert_eq!(map_attr(u32::MAX, 0).maxcount, i32::MAX);
    }
}
