use std::borrow::Cow;
use std::sync::Arc;

use usearch::ScalarKind;

use crate::ascii::{Format, Request, Source};
use crate::data::element;
use crate::data::meta;
use crate::data::vector::{self, Vector};
use crate::error::Error;
use crate::exec::event::{self, Expect};
use crate::index::{self, Graph};
use crate::lane::{self, Op, Waiter};
use crate::memcached::engine::{self, Store};
use crate::memcached::server::{self, Connection};

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
            // The config is the new map's value; its link event registers the index.
            let mut config = index::config_of(&c)?;
            config.extend_from_slice(b"\r\n");
            let attr = engine::map_attr(c.maxcount, server::realtime(c.exptime));
            let store = Store::of(conn)?;
            if index::get(c.index).is_some() {
                if store.exists(c.index)? {
                    return Err(Error::Exists);
                }
                // Dropped or expired, with its unlink not yet run on its lane; waits out the commands
                // in it.
                index::retire(c.index);
            }
            event::offer(conn, Expect::Create, || {
                store.create_map(c.index, &config, attr)
            });
            Ok(None)
        }
        Request::Upsert(u) => {
            let (graph, store) = open(conn, u.index)?;
            let _entered = graph.enter().ok_or(Error::NotFound)?;
            if !fits(&u.vector, graph.layout()) {
                return Err(Error::NotFound);
            }
            let value = element::join(&u.meta, &u.vector.to_bytes())?;
            event::offer(conn, Expect::Upsert, || {
                store.upsert_elem(u.index, u.vkey, &value)
            });
            Ok(None)
        }
        Request::SetMeta(s) => {
            let (graph, store) = open(conn, s.index)?;
            let _entered = graph.enter().ok_or(Error::NotFound)?;
            let (dim, kind) = graph.layout();
            // The replace fills the vector from the element it replaces, as the engine holds it.
            let value = element::with_blank_vector(&s.meta, vector::byte_len(dim, kind))?;
            event::offer(conn, Expect::SetMeta, || {
                store.update_elem(s.index, s.vkey, &value)
            });
            Ok(None)
        }
        Request::GetMeta { index, vkey } => {
            let (_, store) = open(conn, index)?;
            let value = store.get_elem(index, vkey)?;
            let (doc, _) = element::split(&value).ok_or(Error::Internal)?;
            let json = meta::to_json(doc)?;
            let reply = format!("META {vkey} {}\r\n{json}\r\n", json.len());
            Ok(Some(Cow::Owned(reply.into_bytes())))
        }
        Request::Delete { index, vkey } => {
            let (_, store) = open(conn, index)?;
            event::offer(conn, Expect::Delete, || store.delete_elem(index, vkey));
            Ok(None)
        }
        Request::Drop { index } => {
            let (_, store) = open(conn, index)?;
            event::offer(conn, Expect::Drop, || store.remove(index));
            Ok(None)
        }
        Request::Search(mut search) => {
            let (graph, store) = open(conn, search.index)?;
            let entered = graph.enter().ok_or(Error::NotFound)?;
            let (dim, kind) = graph.layout();
            if let Source::Vkey(vkey) = search.source {
                let value = store.get_elem(search.index, vkey)?;
                let (_, bytes) = element::split(&value).ok_or(Error::Internal)?;
                search.vector = Vector::from_bytes(bytes, kind).ok_or(Error::Internal)?;
            } else if !fits(&search.vector, (dim, kind)) {
                return Err(Error::NotFound);
            }
            drop(entered);
            lane::begin(Op::new(Request::Search(search))?, Some(Waiter::new(conn)));
            Ok(None)
        }
    }
}

/// The index at `index`, and the engine to run on. Without such an index, a key the engine has
/// there is of another type.
fn open(conn: Connection, index: &str) -> Result<(Arc<Graph>, Store), Error> {
    let Some(graph) = index::get(index) else {
        let other = Store::of(conn).and_then(|store| store.exists(index));
        return Err(if other == Ok(true) {
            Error::TypeMismatch
        } else {
            Error::NotFound
        });
    };
    Ok((graph, Store::of(conn)?))
}

/// Whether `vector`, read as the body came in, is one of an index of `layout`: a create may have
/// come since, under the name, with another or none, leaving nothing read.
fn fits(vector: &Vector, (dim, kind): (usize, ScalarKind)) -> bool {
    let (of, len) = match vector {
        Vector::F32(v) => (ScalarKind::F32, v.len() * 4),
        Vector::F16(v) => (ScalarKind::F16, v.len() * 2),
        Vector::I8(v) => (ScalarKind::I8, v.len()),
        Vector::B1(v) => (ScalarKind::B1, v.len()),
    };
    of == kind && len == vector::byte_len(dim, kind)
}

/// `bytes` as a vector of `index`, as many finite `f32`s as it has dimensions, in its scalar kind:
/// `bin` is little-endian `f32`s, `str` decimals split by spaces. A missing index is not found, and
/// any other vector a bad value.
pub fn read_vector(index: &str, bytes: &[u8], format: Format) -> Result<Vector, Error> {
    let (dim, kind) = index::layout(index).ok_or(Error::NotFound)?;
    // NaN or infinity makes a distance NaN, which the graph cannot order.
    let finite = |x: f32| x.is_finite().then_some(x);
    let vector: Vec<f32> = match format {
        Format::Bin => {
            let (words, rest) = bytes.as_chunks();
            if !rest.is_empty() {
                return Err(Error::BadValue);
            }
            words
                .iter()
                .map(|&w| finite(f32::from_le_bytes(w)))
                .collect::<Option<_>>()
        }
        Format::Str => bytes
            .split(|&b| b == b' ')
            .filter(|w| !w.is_empty())
            .map(|w| finite(str::from_utf8(w).ok()?.parse().ok()?))
            .collect::<Option<_>>(),
    }
    .ok_or(Error::BadValue)?;
    if vector.len() == dim {
        vector::quantize(vector, kind)
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
        let created = index::create("four", 0, &index::tests::test_config(4, "cos", "f32"));
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
        for x in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let bin: Vec<u8> = [1.0, x, 0.0, 3.25]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            assert_eq!(read_vector("four", &bin, Format::Bin), bad);
        }
    }
}
