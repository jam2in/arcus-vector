#[rustfmt::skip]
mod alias;

mod elem;
mod item;
mod store;

use std::os::raw::c_int;
use std::ptr;
use std::sync::OnceLock;

use super::ffi::{ENGINE_HANDLE, ENGINE_ITEM_TYPE_ITEM_TYPE_MAP, engine_interface_v1};
use super::server;
use crate::error::Error;
pub use elem::Elem;
pub use item::Item;
pub use store::Store;

type Result<T> = std::result::Result<T, Error>;

const ITEM_TYPE_MAP: c_int = ENGINE_ITEM_TYPE_ITEM_TYPE_MAP as c_int;
const CRLF: &[u8] = b"\r\n";

/// The engine functions arcus-vector calls.
struct Engine {
    interface: &'static engine_interface_v1,
    getattr: alias::GetAttr,
    remove: alias::Remove,
    map_struct_create_with_value: alias::MapCreate,
    map_elem_alloc: alias::ElemAlloc,
    map_elem_free: alias::ElemFree,
    map_elem_release: alias::ElemRelease,
    map_elem_insert: alias::ElemInsert,
    map_elem_update: alias::ElemUpdate,
    map_elem_delete: alias::ElemDelete,
    map_elem_get: alias::ElemGet,
    get_elem_info: alias::GetElemInfo,
    get_item_info: alias::GetItemInfo,
}

impl Engine {
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

    fn handle(&self) -> *mut ENGINE_HANDLE {
        ptr::from_ref(self.interface).cast_mut().cast()
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_engine_without_maps_is_refused() {
        // Every function `None`.
        let engine = engine_interface_v1::default();
        assert!(Engine::new(Box::leak(Box::new(engine))).is_none());
    }
}
