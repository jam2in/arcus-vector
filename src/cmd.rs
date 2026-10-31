use crate::aio;
use crate::error::Error;
use crate::index;
use crate::memcached::server::Connection;
use crate::protocol::Request;

/// Runs `request` on `conn`, replying now or once a lane is done.
pub fn run(conn: Connection, request: Result<Request<'_>, Error>) -> bool {
    let reply = match request {
        Ok(Request::Create(spec)) => match index::create(&spec) {
            Ok(()) => b"CREATED\r\n",
            Err(e) => e.reply(),
        },
        Ok(request) if !index::exists(request.index()) => Error::NotFound.reply(),
        Ok(request) => match aio::begin(conn, request) {
            Ok(()) => return true,
            Err(e) => e.reply(),
        },
        Err(e) => e.reply(),
    };
    conn.respond(reply)
}
