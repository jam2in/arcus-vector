use std::borrow::Cow;
use std::cell::Cell;
use std::os::raw::c_void;

use crate::aio::{self, Op, Waiter};
use crate::error::Error;
use crate::guard;
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

/// The event a command's write raises, which its lane answers the command from.
#[derive(Clone, Copy, PartialEq, Eq)]
#[expect(dead_code, reason = "the write commands are not in yet")]
pub enum Expect {
    Create,
    Drop,
    /// An insert, or a replace of the element at the vkey.
    Upsert,
    /// A replace, answered as a store.
    SetMeta,
    Delete,
}

/// A command waiting for the event of its write.
struct Pending {
    expect: Expect,
    waiter: Waiter,
}

impl Pending {
    /// Whether `op` is the event of this command's write. Another is from an eviction the write
    /// made room by, which raises only unlinks and the deletes of their elements: never the event
    /// of a write that allocates, while a write that does not evicts nothing.
    fn awaits(&self, op: &Op) -> bool {
        matches!(
            (op, self.expect),
            (Op::Create { .. }, Expect::Create)
                | (Op::Drop { .. }, Expect::Drop)
                | (Op::Insert { .. } | Op::Replace { .. }, Expect::Upsert)
                | (Op::Replace { .. }, Expect::SetMeta)
                | (Op::Delete { .. }, Expect::Delete)
        )
    }
}

thread_local! {
    static PENDING: Cell<Option<Pending>> = const { Cell::new(None) };
}

/// Runs `write`, whose `expect` event hands `conn` to the lane that answers it. A write that fails,
/// or raises no such event, is answered here.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the write commands are not in yet")
)]
pub fn offer(conn: Connection, expect: Expect, write: impl FnOnce() -> Result<(), Error>) {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            drop(PENDING.take());
        }
    }
    let _clear = Clear;
    PENDING.set(Some(Pending {
        expect,
        waiter: Waiter::new(conn),
    }));
    let written = write();
    // Unclaimed after a write that did not fail, the waiter drops, answering an internal error.
    if let Some(Pending { waiter, .. }) = PENDING.take()
        && let Err(e) = written
    {
        waiter.answer(Cow::Borrowed(e.reply()));
    }
}

/// The waiter `op` answers, if it is the event of the write on this thread.
fn claim(op: Op) -> (Op, Option<Waiter>) {
    let Some(pending) = PENDING.take() else {
        return (op, None);
    };
    if !pending.awaits(&op) {
        PENDING.set(Some(pending));
        return (op, None);
    }
    let op = match op {
        Op::Replace { index, old, new } if pending.expect == Expect::SetMeta => {
            Op::SetMeta { index, old, new }
        }
        op => op,
    };
    (op, Some(pending.waiter))
}

fn op_of(ev: &event_data_t) -> Option<Op> {
    let map = match ev.type_ {
        EVENT_ITEM_LINK | EVENT_ITEM_UNLINK => ev.target,
        _ => ev.coll_item,
    };
    let map = Item::new(map.expose_provenance()).ok()?;
    let index = owned(map.key()?)?;
    let (key, old) = (ev.target.expose_provenance(), ev.source.expose_provenance());
    match ev.type_ {
        EVENT_ITEM_LINK => {
            let config = map.value()?.to_vec();
            Some(Op::Create { index, config })
        }
        EVENT_ITEM_UNLINK => Some(Op::Drop { index }),
        EVENT_ELEM_INSERT => Some(Op::Insert { index, key }),
        EVENT_ELEM_REPLACE => Some(Op::Replace {
            index,
            old,
            new: key,
        }),
        EVENT_ELEM_DELETE => Some(Op::Delete { index, key }),
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
            // SAFETY: the caller guarantees a non-null `data` is a live `event_data_t`.
            let Some(ev) = (unsafe { data.cast::<event_data_t>().as_ref() }) else {
                return;
            };
            let Some(op) = op_of(ev) else {
                return;
            };
            // None for a change from replication or replay, or one the write did not ask for.
            let (op, waiter) = claim(op);
            aio::begin(op, waiter);
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
        let ev = event(EVENT_ELEM_INSERT, 8, 16, 0);
        for data in [(&raw const ev).cast(), ptr::null()] {
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

    fn raise(ev: &event_data_t) {
        // SAFETY: `ev` outlives the call, and its item and elements are never read.
        unsafe {
            on_item_event(
                ptr::null(),
                ENGINE_EVENT_TYPE_ON_EVENT_ITEM,
                ptr::from_ref(ev).cast(),
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
        offer(conn.connection(), Expect::Upsert, || {
            raise(&event(EVENT_ELEM_INSERT, 8, 16, 0));
            Ok(())
        });
        assert!(PENDING.take().is_none());
        assert_eq!(rx.try_recv(), Ok(12));
        assert!(conn.run_callback());
        assert_eq!(conn.reply(), Error::Internal.reply());
    }

    #[test]
    fn a_failed_write_is_answered_its_error() {
        fake::install();
        let (tx, rx) = channel();
        let conn = fake::Conn::new(13, &tx);
        offer(conn.connection(), Expect::Upsert, || Err(Error::NotFound));
        assert_eq!(rx.try_recv(), Ok(13));
        assert!(conn.run_callback());
        assert_eq!(conn.reply(), Error::NotFound.reply());
    }

    #[test]
    fn a_write_claims_only_its_own_event() {
        fake::install();
        let (tx, _rx) = channel();
        let conn = fake::Conn::new(14, &tx);
        let pending = || Pending {
            expect: Expect::SetMeta,
            waiter: Waiter::new(conn.connection()),
        };
        let replace = Op::Replace {
            index: "idx".into(),
            old: 2,
            new: 3,
        };
        PENDING.set(Some(pending()));
        assert!(matches!(claim(replace), (Op::SetMeta { .. }, Some(_))));
        let evicted = Op::Delete {
            index: "other".into(),
            key: 6,
        };
        let insert = Op::Insert {
            index: "idx".into(),
            key: 5,
        };
        PENDING.set(Some(pending()));
        for op in [evicted, insert] {
            assert!(claim(op).1.is_none());
        }
        assert!(PENDING.take().is_some());
    }
}
