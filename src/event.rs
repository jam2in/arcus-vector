use std::cell::Cell;
use std::os::raw::c_void;

use crate::aio::{self, Op};
use crate::guard;
use crate::index;
use crate::memcached::engine::Item;
use crate::memcached::ffi::{
    ENGINE_EVENT_TYPE, ENGINE_EVENT_TYPE_ON_EVENT_ITEM, event_data_t,
    event_type_t_EVENT_ELEM_DELETE as EVENT_ELEM_DELETE,
    event_type_t_EVENT_ELEM_INSERT as EVENT_ELEM_INSERT,
    event_type_t_EVENT_ELEM_REPLACE as EVENT_ELEM_REPLACE,
    event_type_t_EVENT_ITEM_LINK as EVENT_ITEM_LINK,
    event_type_t_EVENT_ITEM_UNLINK as EVENT_ITEM_UNLINK,
};
use crate::memcached::server::{self, Connection};

/// The command a write runs for, where a replace it raises answers as that command.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the write commands are not in yet")
    )]
    Write,
    SetMeta,
}

thread_local! {
    static CONN: Cell<Option<(Connection, Command)>> = const { Cell::new(None) };
}

/// Runs `write` so that the lane its event goes to replies to `conn`; gives `conn` back if no
/// event took it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the write commands are not in yet")
)]
pub fn offer<T>(
    conn: Connection,
    command: Command,
    write: impl FnOnce() -> T,
) -> (T, Option<Connection>) {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            CONN.set(None);
        }
    }
    let _clear = Clear;
    CONN.set(Some((conn, command)));
    let out = write();
    (out, CONN.take().map(|(conn, _)| conn))
}

fn op_of(ev: &event_data_t) -> Option<Op> {
    let map = match ev.type_ {
        EVENT_ITEM_LINK | EVENT_ITEM_UNLINK => ev.target,
        _ => ev.coll_item,
    };
    let map = map.expose_provenance();
    let item = Item::new(map).ok()?;
    let index = owned(item.key()?)?;
    let (key, old) = (ev.target.expose_provenance(), ev.source.expose_provenance());
    match ev.type_ {
        EVENT_ITEM_LINK => {
            let config = item.value().filter(|v| index::is_config(v))?.to_vec();
            Some(Op::Create { index, map, config })
        }
        EVENT_ITEM_UNLINK => Some(Op::Drop { index, map }),
        EVENT_ELEM_INSERT => Some(Op::Insert { index, map, key }),
        EVENT_ELEM_REPLACE => Some(Op::Replace {
            index,
            map,
            old,
            new: key,
        }),
        EVENT_ELEM_DELETE => Some(Op::Delete { index, map, key }),
        _ => None,
    }
}

fn owned(bytes: &[u8]) -> Option<String> {
    str::from_utf8(bytes).ok().map(str::to_owned)
}

pub fn install() {
    // SAFETY: `on_item_event` takes the `event_data_t` the engine passes with `ON_EVENT_ITEM`.
    unsafe { server::register_callback(ENGINE_EVENT_TYPE_ON_EVENT_ITEM, on_item_event) };
}

/// # Safety
///
/// A non-null `data` must be an `event_data_t` whose item and elements live for the call.
unsafe extern "C" fn on_item_event(
    _cookie: *const c_void,
    _type: ENGINE_EVENT_TYPE,
    data: *const c_void,
    _cb_data: *const c_void,
) {
    guard(
        || (),
        || {
            // SAFETY: the caller guarantees a non-null `data` is a live `event_data_t`; the engine
            // reads back the reference counts we set in it.
            let Some(ev) = (unsafe { data.cast_mut().cast::<event_data_t>().as_mut() }) else {
                return;
            };
            let Some(op) = op_of(ev) else {
                return;
            };
            if let Op::Delete { index, map, .. } = &op
                && index::of_map(index, *map).is_none()
            {
                // No graph has the element; the engine frees it now.
                ev.target_ref = 1;
                return;
            }
            // `CONN` is none when the change came from replication or replay.
            let (conn, command) = CONN.take().unzip();
            let op = match op {
                Op::Replace {
                    index,
                    map,
                    old,
                    new,
                } if command == Some(Command::SetMeta) => Op::SetMeta {
                    index,
                    map,
                    old,
                    new,
                },
                op => op,
            };
            let holds = matches!(
                op,
                Op::Insert { .. } | Op::Replace { .. } | Op::SetMeta { .. }
            );
            if aio::begin(op, conn) && holds {
                // The graph keeps the inserted or new element, read in place, until a later event
                // takes it out.
                ev.target_ref = 1;
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use std::ptr;
    use std::sync::mpsc::channel;

    use super::*;
    use crate::memcached::ffi::event_type_t;
    use crate::memcached::server::fake;

    fn event(type_: event_type_t, coll_item: usize, target: usize, source: usize) -> event_data_t {
        event_data_t {
            type_,
            coll_item: ptr::without_provenance_mut(coll_item),
            target: ptr::without_provenance_mut(target),
            source: ptr::without_provenance_mut(source),
            target_ref: 0,
            source_ref: 0,
        }
    }

    #[test]
    fn the_callback_takes_an_event_and_a_null() {
        let mut ev = event(EVENT_ELEM_INSERT, 8, 16, 0);
        for data in [(&raw mut ev).cast_const().cast(), ptr::null()] {
            // SAFETY: `ev` outlives the call, and its item and elements are never read.
            unsafe {
                on_item_event(
                    ptr::null(),
                    ENGINE_EVENT_TYPE_ON_EVENT_ITEM,
                    data,
                    ptr::null(),
                );
            };
        }
    }

    fn raise(ev: &mut event_data_t) {
        // SAFETY: `ev` outlives the call, and its item and elements are never read.
        unsafe {
            on_item_event(
                ptr::null(),
                ENGINE_EVENT_TYPE_ON_EVENT_ITEM,
                ptr::from_mut(ev).cast_const().cast(),
                ptr::null(),
            );
        }
    }

    #[test]
    fn an_event_the_engine_cannot_name_leaves_the_conn() {
        fake::install();
        aio::start_once();
        let (tx, rx) = channel();
        let conn = fake::Conn::new(12, &tx);
        // The tests have no engine to read the index from.
        let ((), left) = offer(conn.connection(), Command::Write, || {
            raise(&mut event(EVENT_ELEM_INSERT, 8, 16, 0));
        });
        assert!(left.is_some());
        assert!(CONN.take().is_none());
        assert!(rx.try_recv().is_err());
    }
}
