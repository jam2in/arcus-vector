use crate::aio;
use crate::error::Error;
use crate::index;
use crate::memcached::server::Connection;
use crate::protocol::Request;

/// Runs `request` on `conn`, replying now or once a lane is done.
///
/// # Safety
///
/// `conn` must be running `execute`.
pub unsafe fn run(conn: Connection, request: Result<Request<'_>, Error>) -> bool {
    let reply = match request {
        Ok(Request::Create(spec)) => match index::create(&spec) {
            Ok(()) => b"CREATED\r\n",
            Err(e) => e.reply(),
        },
        Ok(request) if !index::exists(request.index()) => Error::NotFound.reply(),
        Ok(Request::Search(_)) => {
            let reply = Error::NotSupported.reply();
            // SAFETY: the caller guarantees `conn` is running `execute`.
            return unsafe { aio::begin(conn, reply) };
        }
        Ok(_) => Error::NotSupported.reply(),
        Err(e) => e.reply(),
    };
    // SAFETY: the caller guarantees `conn` is running `execute`.
    unsafe { conn.respond(reply) }
}
