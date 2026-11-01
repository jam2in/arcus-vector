//! The engine functions as `engine_interface_v1` declares them.

use std::os::raw::{c_int, c_void};

use crate::memcached::ffi::{
    ENGINE_ERROR_CODE, ENGINE_HANDLE, eitem, eitem_info, elems_result, field_t, item, item_attr,
    item_info,
};

pub type Remove = unsafe extern "C" fn(Handle, Cookie, Key, usize, u64, u16) -> Code;
pub type MapCreate = unsafe extern "C" fn(Handle, Cookie, Key, c_int, *const c_void, c_int, *mut item_attr, u16) -> Code;
pub type ElemAlloc = unsafe extern "C" fn(Handle, Cookie, Key, c_int, usize, usize, *mut *mut eitem) -> Code;
pub type ElemFree = unsafe extern "C" fn(Handle, Cookie, *mut eitem);
pub type ElemRelease = unsafe extern "C" fn(Handle, Cookie, *mut *mut eitem, c_int);
pub type ElemInsert = unsafe extern "C" fn(Handle, Cookie, Key, c_int, *mut eitem, bool, *mut item_attr, *mut bool, *mut bool, u16) -> Code;
pub type ElemUpdate = unsafe extern "C" fn(Handle, Cookie, Key, c_int, *const field_t, *const c_void, c_int, u16) -> Code;
pub type ElemDelete = unsafe extern "C" fn(Handle, Cookie, Key, c_int, c_int, *const field_t, bool, *mut u32, *mut bool, u16) -> Code;
pub type ElemGet = unsafe extern "C" fn(Handle, Cookie, Key, c_int, c_int, *const field_t, bool, bool, *mut elems_result, u16) -> Code;
pub type GetElemInfo = unsafe extern "C" fn(Handle, Cookie, c_int, *const eitem, *mut eitem_info);
pub type GetItemInfo = unsafe extern "C" fn(Handle, Cookie, *const item, *mut item_info) -> bool;

type Handle = *mut ENGINE_HANDLE;
type Cookie = *const c_void;
type Key = *const c_void;
type Code = ENGINE_ERROR_CODE;
