use std::os::raw::c_void;
use std::{ptr, slice};

use super::{CRLF, Elem, Engine, Result, engine};
use crate::error::Error;
use crate::memcached::ffi::{
    ENGINE_ERROR_CODE, ENGINE_ERROR_CODE_ENGINE_E2BIG, ENGINE_ERROR_CODE_ENGINE_EBADTYPE,
    ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT, ENGINE_ERROR_CODE_ENGINE_ENOMEM,
    ENGINE_ERROR_CODE_ENGINE_EOVERFLOW, ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK,
    ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS, ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT,
    ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_ITEM_ATTR_ATTR_TYPE, eitem, eitem_info, elems_result,
    item_attr,
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
        let mut attr = item_attr {
            readable: 1,
            event: 1,
            maxcount: i32::try_from(maxcount).unwrap_or(i32::MAX),
            exptime: server::realtime(exptime),
            ..Default::default()
        };

        // SAFETY: the store's connection.
        ok(unsafe { self.engine.map_create(self.cookie, key, value, &mut attr) })
    }

    pub fn exist(&self, key: &str) -> Result<()> {
        let mut id = ENGINE_ITEM_ATTR_ATTR_TYPE;
        let mut attr = item_attr::default();

        // SAFETY: the store's connection.
        ok(unsafe { self.engine.getattr(self.cookie, key, &mut id, &mut attr) })
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        // SAFETY: the store's connection.
        ok(unsafe { self.engine.remove(self.cookie, key) })
    }

    pub fn upsert(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let elem = self.alloc(key, field, value)?;

        let mut replaced = false;
        let mut created = false;

        // SAFETY: the store's connection; `elem` is unlinked and ours.
        let ret = unsafe {
            self.engine
                .map_elem_insert(self.cookie, key, elem, &mut replaced, &mut created)
        };

        ok(ret).inspect_err(|_| {
            // SAFETY: not inserted, so `elem` is still unlinked and ours.
            unsafe { self.engine.map_elem_free(self.cookie, elem) };
        })
    }

    pub fn update(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let nbytes = value.len() + CRLF.len();

        let mut value_data = Vec::new();
        value_data
            .try_reserve_exact(nbytes)
            .map_err(|_| Error::OutOfMemory)?;
        value_data.extend_from_slice(value);
        value_data.extend_from_slice(CRLF);

        // SAFETY: the store's connection.
        ok(unsafe {
            self.engine
                .map_elem_update(self.cookie, key, field, &value_data)
        })
    }

    /// Removes the element at `field`; `NOT_FOUND_ELEMENT` if there is none.
    pub fn delete(&self, key: &str, field: &str) -> Result<()> {
        let mut deleted = 0u32;
        let mut dropped = false;

        // SAFETY: the store's connection.
        ok(unsafe {
            self.engine
                .map_elem_delete(self.cookie, key, field, &mut deleted, &mut dropped)
        })
    }

    pub fn get(&self, key: &str, field: &str) -> Result<Elem> {
        let mut result = elems_result::default();

        // SAFETY: the store's connection.
        ok(unsafe {
            self.engine
                .map_elem_get(self.cookie, key, field, &mut result)
        })?;

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

    fn alloc(&self, key: &str, field: &str, value: &[u8]) -> Result<*mut eitem> {
        let nfield = field.len();
        let nbytes = value.len() + CRLF.len();

        let mut elem = ptr::null_mut();

        // SAFETY: the store's connection.
        ok(unsafe {
            self.engine
                .map_elem_alloc(self.cookie, key, nfield, nbytes, &mut elem)
        })?;

        let mut info = eitem_info::default();

        // SAFETY: the store's connection; `elem` is sized `nfield` and `nbytes`.
        let (field_slice, value_slice) = unsafe {
            self.engine.get_elem_info(self.cookie, elem, &mut info);
            (
                slice::from_raw_parts_mut(info.score.cast_mut(), nfield),
                slice::from_raw_parts_mut(info.value.cast::<u8>().cast_mut(), nbytes),
            )
        };

        field_slice.copy_from_slice(field.as_bytes());
        value_slice[..value.len()].copy_from_slice(value);
        value_slice[value.len()..].copy_from_slice(CRLF);

        Ok(elem)
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
