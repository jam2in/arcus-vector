//! The engine functions over Rust types, with the handle and fixed arguments filled in.

use std::os::raw::{c_int, c_void};
use std::ptr;

use super::{Engine, ITEM_TYPE_MAP};
use crate::memcached::ffi::{
    ENGINE_ERROR_CODE, ENGINE_ITEM_ATTR, eitem, eitem_info, elems_result, field_t, item, item_attr,
    item_info,
};

type Cookie = *const c_void;
type Code = ENGINE_ERROR_CODE;

const VBUCKET: u16 = 0;

fn raw(key: &str) -> (*const c_void, c_int) {
    (key.as_ptr().cast(), key.len() as c_int)
}

fn field_of(field: &str) -> field_t {
    field_t {
        value: field.as_ptr().cast_mut().cast(),
        length: field.len(),
    }
}

impl Engine {
    /// # Safety
    ///
    /// `cookie` is a live connection or null.
    pub(super) unsafe fn getattr(
        &self,
        cookie: Cookie,
        key: &str,
        id: &mut ENGINE_ITEM_ATTR,
        attr: &mut item_attr,
    ) -> Code {
        let (key, nkey) = raw(key);
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe { (self.getattr)(self.handle(), cookie, key, nkey, id, 1, attr, VBUCKET) }
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null.
    pub(super) unsafe fn remove(&self, cookie: Cookie, key: &str) -> Code {
        let cas = 0;
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe {
            (self.remove)(
                self.handle(),
                cookie,
                key.as_ptr().cast(),
                key.len(),
                cas,
                VBUCKET,
            )
        }
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null.
    pub(super) unsafe fn map_create(
        &self,
        cookie: Cookie,
        key: &str,
        value: &[u8],
        attr: &mut item_attr,
    ) -> Code {
        let (key, nkey) = raw(key);
        let (value, nvalue) = (value.as_ptr().cast(), value.len() as c_int);
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe {
            (self.map_struct_create_with_value)(
                self.handle(),
                cookie,
                key,
                nkey,
                value,
                nvalue,
                attr,
                VBUCKET,
            )
        }
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null.
    pub(super) unsafe fn map_elem_alloc(
        &self,
        cookie: Cookie,
        key: &str,
        nfield: usize,
        nbytes: usize,
        elem: &mut *mut eitem,
    ) -> Code {
        let (key, nkey) = raw(key);
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe { (self.map_elem_alloc)(self.handle(), cookie, key, nkey, nfield, nbytes, elem) }
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null; `elem` is unlinked and ours.
    pub(super) unsafe fn map_elem_free(&self, cookie: Cookie, elem: *mut eitem) {
        // SAFETY: as documented.
        unsafe { (self.map_elem_free)(self.handle(), cookie, elem) };
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null; `elem` holds our reference.
    pub(super) unsafe fn map_elem_release(&self, cookie: Cookie, mut elem: *mut eitem) {
        // SAFETY: as documented.
        unsafe { (self.map_elem_release)(self.handle(), cookie, &raw mut elem, 1) };
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null; `elem` is unlinked and ours.
    pub(super) unsafe fn map_elem_insert(
        &self,
        cookie: Cookie,
        key: &str,
        elem: *mut eitem,
        replaced: &mut bool,
        created: &mut bool,
    ) -> Code {
        let (key, nkey) = raw(key);
        let replace_if_exist = true;
        let attr = ptr::null_mut();
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe {
            (self.map_elem_insert)(
                self.handle(),
                cookie,
                key,
                nkey,
                elem,
                replace_if_exist,
                attr,
                replaced,
                created,
                VBUCKET,
            )
        }
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null.
    pub(super) unsafe fn map_elem_update(
        &self,
        cookie: Cookie,
        key: &str,
        field: &str,
        value: &[u8],
    ) -> Code {
        let (key, nkey) = raw(key);
        let field = field_of(field);
        let (value, nvalue) = (value.as_ptr().cast(), value.len() as c_int);
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe {
            (self.map_elem_update)(
                self.handle(),
                cookie,
                key,
                nkey,
                &raw const field,
                value,
                nvalue,
                VBUCKET,
            )
        }
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null.
    pub(super) unsafe fn map_elem_delete(
        &self,
        cookie: Cookie,
        key: &str,
        field: &str,
        deleted: &mut u32,
        dropped: &mut bool,
    ) -> Code {
        let (key, nkey) = raw(key);
        let field = field_of(field);
        let drop_if_empty = false;
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe {
            (self.map_elem_delete)(
                self.handle(),
                cookie,
                key,
                nkey,
                1,
                &raw const field,
                drop_if_empty,
                deleted,
                dropped,
                VBUCKET,
            )
        }
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null.
    pub(super) unsafe fn map_elem_get(
        &self,
        cookie: Cookie,
        key: &str,
        field: &str,
        result: &mut elems_result,
    ) -> Code {
        let (key, nkey) = raw(key);
        let field = field_of(field);
        let (delete, drop_if_empty) = (false, false);
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe {
            (self.map_elem_get)(
                self.handle(),
                cookie,
                key,
                nkey,
                1,
                &raw const field,
                delete,
                drop_if_empty,
                result,
                VBUCKET,
            )
        }
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null; `elem` is live.
    pub(super) unsafe fn get_elem_info(
        &self,
        cookie: Cookie,
        elem: *const eitem,
        info: &mut eitem_info,
    ) {
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe { (self.get_elem_info)(self.handle(), cookie, ITEM_TYPE_MAP, elem, info) };
    }

    /// # Safety
    ///
    /// `cookie` is a live connection or null; `item` is live.
    pub(super) unsafe fn get_item_info(
        &self,
        cookie: Cookie,
        item: *const item,
        info: &mut item_info,
    ) -> bool {
        // SAFETY: as documented; the rest is borrowed for the call.
        unsafe { (self.get_item_info)(self.handle(), cookie, item, info) }
    }
}
