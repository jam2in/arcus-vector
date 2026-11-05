use std::borrow::Cow;

use usearch::ScalarKind;

use crate::aio::{self, Op};
use crate::bson;
use crate::element;
use crate::error::Error;
use crate::event::{self, Command};
use crate::index;
use crate::memcached::engine::{self, Store};
use crate::memcached::server::{self, Connection};
use crate::protocol::{Format, Request, Source};
use crate::quant::{self, Vector};

/// A reply now, or `None` once a lane has the connection to reply to.
type Reply = Option<Cow<'static, [u8]>>;

/// Runs `request` on `conn`, replying now or once a lane is done.
pub fn run(conn: Connection, request: Result<Request<'_>, Error>) -> bool {
    match request.and_then(|request| execute(conn, request)) {
        Ok(None) => true,
        Ok(Some(reply)) => conn.respond(&reply),
        Err(e) => conn.respond(e.reply()),
    }
}

fn execute(conn: Connection, request: Request<'_>) -> Result<Reply, Error> {
    match request {
        Request::Create(c) => {
            if index::get(c.index).is_some() {
                return Err(Error::Exists);
            }
            // The config is the new map's value; its link event registers the index.
            let mut config = index::config_of(&c)?;
            config.extend_from_slice(b"\r\n");
            let attr = engine::map_attr(c.maxcount, server::realtime(c.exptime));
            let store = Store::of(conn)?;
            event::offer(conn, Command::Write, || {
                store.create_map(c.index, &config, attr)
            })?;
            Ok(None)
        }
        Request::Upsert(u) => {
            let (_, store) = open(conn, u.index)?;
            let value = element::join(&u.meta, &u.vector.to_bytes())?;
            event::offer(conn, Command::Write, || {
                store.upsert_elem(u.index, u.vkey, &value)
            })?;
            Ok(None)
        }
        Request::SetMeta(s) => {
            let (_, store) = open(conn, s.index)?;
            let old = store.get_elem(s.index, s.vkey)?;
            let (_, vector) = element::split(&old).ok_or(Error::Internal)?;
            let value = element::join(&s.meta, vector)?;
            event::offer(conn, Command::SetMeta, || {
                store.update_elem(s.index, s.vkey, &value)
            })?;
            Ok(None)
        }
        Request::GetMeta { index, vkey } => {
            let (_, store) = open(conn, index)?;
            let value = store.get_elem(index, vkey)?;
            let (doc, _) = element::split(&value).ok_or(Error::Internal)?;
            let json = bson::to_json(doc)?;
            let reply = format!("META {vkey} {}\r\n{json}\r\n", json.len());
            Ok(Some(Cow::Owned(reply.into_bytes())))
        }
        Request::Delete { index, vkey } => {
            let (_, store) = open(conn, index)?;
            event::offer(conn, Command::Write, || store.delete_elem(index, vkey))?;
            Ok(None)
        }
        Request::Drop { index } => {
            let (_, store) = open(conn, index)?;
            event::offer(conn, Command::Write, || store.remove(index))?;
            Ok(None)
        }
        Request::Search(mut search) => {
            let (kind, store) = open(conn, search.index)?;
            if let Source::Vkey(vkey) = search.source {
                let value = store.get_elem(search.index, vkey)?;
                let (_, bytes) = element::split(&value).ok_or(Error::Internal)?;
                search.vector = Vector::from_bytes(bytes, kind).ok_or(Error::Internal)?;
            }
            aio::begin(Op::new(Request::Search(search))?, Some(conn));
            Ok(None)
        }
    }
}

/// The scalar kind of the index at `index`, and the engine to run on; `NOT_FOUND` before the
/// engine if there is no such index.
fn open(conn: Connection, index: &str) -> Result<(ScalarKind, Store), Error> {
    let (_, kind) = index::layout(index).ok_or(Error::NotFound)?;
    Ok((kind, Store::of(conn)?))
}

/// `bytes` as a vector of `index`, as many `f32`s as it has dimensions, in its scalar kind: `bin`
/// is little-endian `f32`s, `str` finite decimals split by spaces. A missing index is not found,
/// and any other vector a bad value.
pub fn read_vector(index: &str, bytes: &[u8], format: Format) -> Result<Vector, Error> {
    let (dim, kind) = index::layout(index).ok_or(Error::NotFound)?;
    let vector: Vec<f32> = match format {
        Format::Bin => {
            let (words, rest) = bytes.as_chunks();
            if !rest.is_empty() {
                return Err(Error::BadValue);
            }
            words.iter().map(|&w| f32::from_le_bytes(w)).collect()
        }
        Format::Str => bytes
            .split(|&b| b == b' ')
            .filter(|w| !w.is_empty())
            .map(|w| {
                str::from_utf8(w)
                    .ok()?
                    .parse()
                    .ok()
                    .filter(|x: &f32| x.is_finite())
            })
            .collect::<Option<_>>()
            .ok_or(Error::BadValue)?,
    };
    if vector.len() == dim {
        quant::quantize(vector, kind)
    } else {
        Err(Error::BadValue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_index_is_not_found() {
        assert_eq!(
            read_vector("nowhere", &[0; 16], Format::Bin),
            Err(Error::NotFound)
        );
    }

    #[test]
    fn a_vector_is_read_as_its_index_dimensions() {
        let created = index::create_with("four", 0, &index::test_config(4, "cos", "f32"));
        assert_eq!(created, Ok(()));
        let bin: Vec<u8> = [1.0f32, -2.5, 0.0, 3.25]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let four = Ok(Vector::F32(vec![1.0, -2.5, 0.0, 3.25]));
        assert_eq!(read_vector("four", &bin, Format::Bin), four);
        let bad = Err(Error::BadValue);
        assert_eq!(read_vector("four", &bin[..12], Format::Bin), bad);
        assert_eq!(read_vector("four", &bin[..15], Format::Bin), bad);
        assert_eq!(read_vector("four", &[0; 20], Format::Bin), bad);
        assert_eq!(read_vector("four", b"1 -2.5  0 3.25", Format::Str), four);
        assert_eq!(read_vector("four", b"1 2 3", Format::Str), bad);
        assert_eq!(read_vector("four", b"1 2 x 4", Format::Str), bad);
        assert_eq!(read_vector("four", b"1 2 nan 4", Format::Str), bad);
        assert_eq!(read_vector("four", b"1 2 inf 4", Format::Str), bad);
        assert_eq!(read_vector("four", b"1 2 1e39 4", Format::Str), bad);
    }
}
