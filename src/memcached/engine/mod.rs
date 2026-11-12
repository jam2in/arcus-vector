#[rustfmt::skip]
mod fns;

mod item;
mod store;

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

use super::ffi::{
    ENGINE_ERROR_CODE, ENGINE_ERROR_CODE_ENGINE_E2BIG, ENGINE_ERROR_CODE_ENGINE_EBADTYPE,
    ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT, ENGINE_ERROR_CODE_ENGINE_ENOMEM,
    ENGINE_ERROR_CODE_ENGINE_EOVERFLOW, ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK,
    ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS, ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT,
    ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_HANDLE, ENGINE_ITEM_TYPE_ITEM_TYPE_MAP, eitem,
    engine_interface_v1, field_t,
};
use super::server;
use crate::error::Error;
pub use item::{Elem, Item};
pub use store::{Store, map_attr};

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

type Result<T> = std::result::Result<T, Error>;

const ITEM_TYPE_MAP: c_int = ENGINE_ITEM_TYPE_ITEM_TYPE_MAP as c_int;
const CRLF: &[u8] = b"\r\n";

/// The engine functions arcus-vector calls.
struct Engine {
    interface: &'static engine_interface_v1,
    getattr: fns::GetAttr,
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
    fn handle(&self) -> *mut ENGINE_HANDLE {
        ptr::from_ref(self.interface).cast_mut().cast()
    }

    /// Gives back element references.
    ///
    /// # Safety
    ///
    /// Each element must be one we hold a reference on, given back only here; nothing may read it
    /// afterwards. The engine hands out at most `max_map_size` at once, so they fit a `c_int`.
    unsafe fn release_elems(&self, elems: &mut [*mut eitem]) {
        let (h, c) = (self.handle(), ptr::null());
        let n = elems.len() as c_int;
        // SAFETY: the caller guarantees each is a reference we hold.
        unsafe { (self.map_elem_release)(h, c, elems.as_mut_ptr(), n) };
    }

    /// `None` if the engine lacks a function arcus-vector calls.
    fn new(interface: &'static engine_interface_v1) -> Option<Self> {
        Some(Self {
            interface,
            getattr: interface.getattr?,
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

fn value_len(bytes: &[u8]) -> Result<c_int> {
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
}
