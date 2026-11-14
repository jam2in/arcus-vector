use std::os::raw::{c_int, c_void};
use std::{ptr, slice};

use super::{CRLF, Elem, Engine, ITEM_TYPE_MAP, Result, engine};
use crate::error::Error;
use crate::memcached::ffi::{
    ENGINE_ERROR_CODE, ENGINE_ERROR_CODE_ENGINE_E2BIG, ENGINE_ERROR_CODE_ENGINE_EBADTYPE,
    ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT, ENGINE_ERROR_CODE_ENGINE_ENOMEM,
    ENGINE_ERROR_CODE_ENGINE_EOVERFLOW, ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK,
    ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS, ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT,
    ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_ITEM_ATTR_ATTR_TYPE, eitem, eitem_info, elems_result,
    field_t, item_attr,
};
use crate::memcached::server::{self, Connection};

unsafe extern "C" {
    fn free(ptr: *mut c_void);
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

    pub fn create(&self, key: &str, value: &[u8], maxcount: u32, exptime: u32) -> Result<()> {
        let handle = self.engine.handle();
        let cookie = self.cookie;

        let (key, nkey) = (key.as_ptr().cast(), key.len() as c_int);
        let (value, nvalue) = (value.as_ptr().cast(), value.len() as c_int);
        let mut attr = item_attr {
            readable: 1,
            event: 1,
            maxcount: i32::try_from(maxcount).unwrap_or(i32::MAX),
            exptime: server::realtime(exptime),
            ..Default::default()
        };

        // SAFETY: `key`, `value` and `attr` outlive the call, which copies them.
        let ret = unsafe {
            (self.engine.map_struct_create_with_value)(
                handle,
                cookie,
                key,
                nkey,
                value,
                nvalue,
                &raw mut attr,
                0,
            )
        };

        ok(ret)
    }

    pub fn exist(&self, key: &str) -> Result<()> {
        let handle = self.engine.handle();
        let cookie = self.cookie;

        let (key, nkey) = (key.as_ptr().cast(), key.len() as c_int);
        let mut id = ENGINE_ITEM_ATTR_ATTR_TYPE;
        let mut attr = item_attr::default();

        // SAFETY: `key`, `id` and `attr` outlive the call.
        let ret = unsafe {
            (self.engine.getattr)(handle, cookie, key, nkey, &raw mut id, 1, &raw mut attr, 0)
        };

        ok(ret)
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        let handle = self.engine.handle();
        let cookie = self.cookie;

        let (key, nkey) = (key.as_ptr().cast(), key.len());

        // SAFETY: `key` outlives the call.
        let ret = unsafe { (self.engine.remove)(handle, cookie, key, nkey, 0, 0) };

        ok(ret)
    }

    pub fn upsert(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let handle = self.engine.handle();
        let cookie = self.cookie;

        let elem = self.alloc(key, field, value)?;

        let (key, nkey) = (key.as_ptr().cast(), key.len() as c_int);
        let mut replaced = false;
        let mut created = false;

        // SAFETY: `elem` is ours and unlinked; `key` and the out-params outlive the call.
        let ret = unsafe {
            (self.engine.map_elem_insert)(
                handle,
                cookie,
                key,
                nkey,
                elem,
                true,
                ptr::null_mut(),
                &raw mut replaced,
                &raw mut created,
                0,
            )
        };

        match ok(ret) {
            Ok(()) => Ok(()),
            Err(e) => {
                // SAFETY: the insert failed, so `elem` was never linked and is still ours to free.
                unsafe { (self.engine.map_elem_free)(handle, cookie, elem) };
                Err(e)
            }
        }
    }

    pub fn update(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let handle = self.engine.handle();
        let cookie = self.cookie;

        let (key, nkey) = (key.as_ptr().cast(), key.len() as c_int);
        let nbytes = value.len() + CRLF.len();

        let mut value_data = Vec::new();
        value_data
            .try_reserve_exact(nbytes)
            .map_err(|_| Error::OutOfMemory)?;
        value_data.extend_from_slice(value);
        value_data.extend_from_slice(CRLF);

        let (value, nvalue) = (value_data.as_ptr().cast(), nbytes as c_int);
        let field = field_t {
            value: field.as_ptr().cast_mut().cast(),
            length: field.len(),
        };

        // SAFETY: `key`, `field` and `value` outlive the call.
        let ret = unsafe {
            (self.engine.map_elem_update)(
                handle,
                cookie,
                key,
                nkey,
                &raw const field,
                value,
                nvalue,
                0,
            )
        };

        ok(ret)
    }

    /// Removes the element at `field`; `NOT_FOUND_ELEMENT` if there is none.
    pub fn delete(&self, key: &str, field: &str) -> Result<()> {
        let handle = self.engine.handle();
        let cookie = self.cookie;

        let (key, nkey) = (key.as_ptr().cast(), key.len() as c_int);
        let field = field_t {
            value: field.as_ptr().cast_mut().cast(),
            length: field.len(),
        };

        let mut deleted = 0u32;
        let mut dropped = false;

        // SAFETY: `key`, `field` and the out-params outlive the call.
        let ret = unsafe {
            (self.engine.map_elem_delete)(
                handle,
                cookie,
                key,
                nkey,
                1,
                &raw const field,
                false,
                &raw mut deleted,
                &raw mut dropped,
                0,
            )
        };

        ok(ret)
    }

    pub fn get(&self, key: &str, field: &str) -> Result<Elem> {
        let handle = self.engine.handle();
        let cookie = self.cookie;

        let (key, nkey) = (key.as_ptr().cast(), key.len() as c_int);
        let field = field_t {
            value: field.as_ptr().cast_mut().cast(),
            length: field.len(),
        };

        let mut result = elems_result::default();

        // SAFETY: `key`, `field` and `result` outlive the call.
        let ret = unsafe {
            (self.engine.map_elem_get)(
                handle,
                cookie,
                key,
                nkey,
                1,
                &raw const field,
                false,
                false,
                &raw mut result,
                0,
            )
        };

        match ok(ret) {
            Ok(()) => {
                if result.elem_array.is_null() {
                    return Err(Error::NotFoundElement);
                }

                // SAFETY: an array of one, `malloc`ed for us.
                let elem_ptr = unsafe {
                    let elem_ptr = *result.elem_array;
                    free(result.elem_array.cast());
                    elem_ptr.expose_provenance()
                };

                Elem::new(elem_ptr)
            }
            Err(e) => Err(e),
        }
    }

    fn alloc(&self, key: &str, field: &str, value: &[u8]) -> Result<*mut eitem> {
        let handle = self.engine.handle();
        let cookie = self.cookie;

        let (key, nkey) = (key.as_ptr().cast(), key.len() as c_int);
        let (field, nfield) = (field.as_bytes(), field.len());
        let nbytes = value.len() + CRLF.len();

        let mut elem = ptr::null_mut();

        // SAFETY: `key` outlives the call, and `elem` is a local out-pointer.
        let ret = unsafe {
            (self.engine.map_elem_alloc)(handle, cookie, key, nkey, nfield, nbytes, &raw mut elem)
        };

        match ok(ret) {
            Ok(()) => {
                let mut info = eitem_info::default();

                // SAFETY: `elem` is unlinked, sized `nfield` and `nbytes`.
                let (field_slice, value_slice) = unsafe {
                    (self.engine.get_elem_info)(handle, cookie, ITEM_TYPE_MAP, elem, &raw mut info);
                    (
                        slice::from_raw_parts_mut(info.score.cast_mut(), nfield),
                        slice::from_raw_parts_mut(info.value.cast::<u8>().cast_mut(), nbytes),
                    )
                };

                field_slice.copy_from_slice(field);
                value_slice[..value.len()].copy_from_slice(value);
                value_slice[value.len()..].copy_from_slice(CRLF);

                Ok(elem)
            }
            Err(e) => Err(e),
        }
    }
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
}
