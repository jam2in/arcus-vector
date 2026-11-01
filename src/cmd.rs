use crate::aio;
use crate::error::Error;
use crate::index;
use crate::memcached::server::Connection;
use crate::protocol::{Format, Request};

/// Runs `request`, whose body carried `vector`, on `conn`, replying now or once a lane is done.
pub fn run(conn: Connection, request: Result<Request<'_>, Error>, vector: &[u8]) -> bool {
    let reply = match request {
        Ok(Request::Create(spec)) => match index::create(&spec) {
            Ok(()) => b"CREATED\r\n",
            Err(e) => e.reply(),
        },
        Ok(
            request @ (Request::Upsert(_)
            | Request::SetMeta(_)
            | Request::Search(_)
            | Request::Delete { .. }),
        ) => match read_vector(&request, vector).and_then(|v| aio::begin(conn, request, v)) {
            Ok(()) => return true,
            Err(e) => e.reply(),
        },
        Ok(request @ (Request::GetMeta { .. } | Request::Drop { .. })) => {
            match index::dimensions(request.index()) {
                Some(_) => Error::NotSupported.reply(),
                None => Error::NotFound.reply(),
            }
        }
        Err(e) => e.reply(),
    };
    conn.respond(reply)
}

/// The vector `request` carries, as many `f32`s as its index has dimensions: `bin` is
/// little-endian `f32`s, `str` decimals split by spaces. A missing index is not found, and any other
/// vector a bad value.
fn read_vector(request: &Request<'_>, bytes: &[u8]) -> Result<Option<Vec<f32>>, Error> {
    let dim = index::dimensions(request.index()).ok_or(Error::NotFound)?;
    let vector: Vec<f32> = match request.vector_format() {
        None => return Ok(None),
        Some(Format::Bin) => {
            let (words, rest) = bytes.as_chunks();
            if !rest.is_empty() {
                return Err(Error::BadValue);
            }
            words.iter().map(|&w| f32::from_le_bytes(w)).collect()
        }
        Some(Format::Str) => bytes
            .split(|&b| b == b' ')
            .filter(|w| !w.is_empty())
            .map(|w| str::from_utf8(w).ok()?.parse().ok())
            .collect::<Option<_>>()
            .ok_or(Error::BadValue)?,
    };
    if vector.len() == dim {
        Ok(Some(vector))
    } else {
        Err(Error::BadValue)
    }
}

#[cfg(test)]
mod tests {
    use std::os::raw::c_int;

    use super::*;
    use crate::protocol::{self, Tokens};

    fn on_line<T>(line: &str, f: impl FnOnce(Request<'_>) -> T) -> T {
        let (_buf, tokens) = protocol::tokenize(line);
        // SAFETY: `tokens` point into `_buf`, which outlives the call.
        let tokens = unsafe { Tokens::new(tokens.as_ptr(), tokens.len() as c_int) };
        f(protocol::parse(&tokens).unwrap())
    }

    fn read_line(line: &str, vector: &[u8]) -> Result<Option<Vec<f32>>, Error> {
        on_line(line, |request| read_vector(&request, vector))
    }

    #[test]
    fn a_missing_index_is_not_found() {
        assert_eq!(read_line("vop delete nowhere v", b""), Err(Error::NotFound));
        assert_eq!(
            read_line("vop upsert nowhere v 16", &[0; 16]),
            Err(Error::NotFound)
        );
    }

    #[test]
    fn a_vector_is_read_as_its_index_dimensions() {
        let created = on_line("vop create four 4 0", |request| match request {
            Request::Create(spec) => index::create(&spec),
            _ => unreachable!(),
        });
        assert_eq!(created, Ok(()));
        let bin: Vec<u8> = [1.0f32, -2.5, 0.0, 3.25]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let four = Ok(Some(vec![1.0, -2.5, 0.0, 3.25]));
        assert_eq!(read_line("vop upsert four v 16", &bin), four);
        assert_eq!(
            read_line("vop upsert four v 12", &bin[..12]),
            Err(Error::BadValue)
        );
        assert_eq!(
            read_line("vop upsert four v 15", &bin[..15]),
            Err(Error::BadValue)
        );
        assert_eq!(
            read_line("vop upsert four v 16 str", b"1 -2.5  0 3.25"),
            four
        );
        assert_eq!(
            read_line("vop upsert four v 5 str", b"1 2 3"),
            Err(Error::BadValue)
        );
        assert_eq!(
            read_line("vop upsert four v 7 str", b"1 2 x 4"),
            Err(Error::BadValue)
        );
        assert_eq!(read_line("vop search four vector 3 16", &bin), four);
        assert_eq!(
            read_line("vop search four vector 3 20", &[0; 20]),
            Err(Error::BadValue)
        );
        assert_eq!(read_line("vop search four vkey v 3", b""), Ok(None));
        assert_eq!(read_line("vop delete four v", b""), Ok(None));
    }
}
