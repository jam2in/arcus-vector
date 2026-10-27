use std::cell::Cell;
use std::os::raw::c_void;

use crate::aio::{self, Job};
use crate::guard;
use crate::memcached::ffi::{
    ENGINE_EVENT_TYPE, event_data_t, event_type_t_EVENT_ELEM_DELETE,
    event_type_t_EVENT_ELEM_INSERT, event_type_t_EVENT_ELEM_REPLACE,
};
use crate::memcached::server;

thread_local! {
    /// The job of the write command running on this thread, for the event its write raises.
    static CURRENT: Cell<Option<*mut Job>> = const { Cell::new(None) };
}

/// Makes `job` this thread's current job; its connection must already wait on it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the write commands are not in yet")
)]
pub fn offer(job: *mut Job) {
    CURRENT.set(Some(job));
}

/// Takes the current job back; `None` if an event took it to a lane.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the write commands are not in yet")
)]
pub fn withdraw() -> Option<*mut Job> {
    CURRENT.take()
}

pub fn install() -> bool {
    server::register_item_event(on_item_event)
}

/// Runs under the engine's cache lock, on the thread that wrote the element.
unsafe extern "C" fn on_item_event(
    _cookie: *const c_void,
    _type: ENGINE_EVENT_TYPE,
    event_data: *const c_void,
    _cb_data: *const c_void,
) {
    guard(
        || (),
        || {
            // SAFETY: the engine passes its `event_data_t`, alive for this call.
            let Some(ev) = (unsafe { event_data.cast::<event_data_t>().as_ref() }) else {
                return;
            };
            let elements = [
                event_type_t_EVENT_ELEM_INSERT,
                event_type_t_EVENT_ELEM_DELETE,
                event_type_t_EVENT_ELEM_REPLACE,
            ];
            if !elements.contains(&ev.type_) {
                return;
            }
            // No current job: the write came from replication or persistence replay.
            let Some(job) = CURRENT.take().or_else(aio::background) else {
                return;
            };
            // SAFETY: an offered job is unqueued and its connection waits on it; a new one has none.
            unsafe { aio::submit(job) };
        },
    );
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::memcached::ffi::{
        ENGINE_EVENT_TYPE_ON_EVENT_ITEM, event_type_t, event_type_t_EVENT_ITEM_LINK,
    };

    fn raise(type_: event_type_t) {
        aio::start_once();
        let ev = event_data_t {
            type_,
            coll_item: ptr::null_mut(),
            target: ptr::null_mut(),
            source: ptr::null_mut(),
            target_ref: 0,
            source_ref: 0,
        };
        // SAFETY: `ev` outlives the call, as the engine's own event does.
        unsafe {
            on_item_event(
                ptr::null(),
                ENGINE_EVENT_TYPE_ON_EVENT_ITEM,
                (&raw const ev).cast(),
                ptr::null(),
            );
        }
    }

    #[test]
    fn an_element_event_takes_the_current_job() {
        offer(aio::background().unwrap());
        raise(event_type_t_EVENT_ELEM_INSERT);
        assert_eq!(withdraw(), None);
    }

    #[test]
    fn an_item_event_leaves_the_current_job() {
        let job = aio::background().unwrap();
        offer(job);
        raise(event_type_t_EVENT_ITEM_LINK);
        assert_eq!(withdraw(), Some(job));
        // SAFETY: `job` came back unqueued, and no connection waits on it.
        unsafe { aio::submit(job) };
    }

    #[test]
    fn a_null_event_is_ignored() {
        // SAFETY: a null `event_data` is never dereferenced.
        unsafe {
            on_item_event(
                ptr::null(),
                ENGINE_EVENT_TYPE_ON_EVENT_ITEM,
                ptr::null(),
                ptr::null(),
            );
        }
    }
}
