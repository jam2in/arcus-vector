mod pool;

use std::borrow::Cow;
use std::os::raw::c_void;
use std::sync::{Arc, OnceLock};

use crate::error::Error;
use crate::guard;
use crate::memcached::server::{Connection, ResponseHandler};
use crate::protocol::Request;
use pool::Op;
pub use pool::start;

/// Hands `request` to a lane, to be replied to from the io callback; an error is for `execute` to
/// reply with.
pub fn begin(conn: Connection, request: Request<'_>) -> Result<(), Error> {
    let op = Op::new(request)?;
    let reply = Arc::new(OnceLock::new());
    // SAFETY: the only caller, `cmd::run`, runs in `execute`.
    let waiter = unsafe { conn.wait_for_io() };
    let data = Arc::into_raw(Arc::clone(&reply));
    // SAFETY: as above, and `wake` takes back the count `data` holds.
    unsafe { conn.on_io_complete(wake, data.cast_mut().cast()) };
    pool::submit(op, reply, waiter);
    Ok(())
}

unsafe extern "C" fn wake(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool {
    guard(
        || false,
        || {
            // SAFETY: `data` is the count from `begin`, and the core calls this once.
            let reply = unsafe { Arc::from_raw(data.cast::<OnceLock<Cow<'static, [u8]>>>()) };
            let reply = reply.get().map_or(Error::Internal.reply(), AsRef::as_ref);
            // SAFETY: the server is running the io callback for `cookie`, with `handler`.
            unsafe { Connection::new(cookie, handler).respond(reply) }
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::*;
    use crate::index;
    use crate::memcached::server::fake;
    use crate::protocol::{self, Tokens};

    #[test]
    fn begin_replies_from_the_io_callback_after_a_lane_notifies() {
        fake::install();
        pool::start_once();
        let (tx, rx) = channel();
        let conn = fake::Conn::new(7, &tx);
        let Ok(Request::Create(spec)) =
            protocol::parse(Tokens::from_line("vop create aio 4 0").words())
        else {
            unreachable!()
        };
        assert_eq!(index::create(&spec), Ok(()));
        let tokens = Tokens::from_line("vop search aio vector 16 3");
        let mut request = protocol::parse(tokens.words()).unwrap();
        request.set_vector(vec![0.0; 4]);
        assert_eq!(begin(conn.connection(), request), Ok(()));
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 7);
        assert!(conn.reply().is_empty());
        assert!(conn.run_callback());
        assert_eq!(conn.reply(), b"NOT_SUPPORTED\r\n");
    }
}
