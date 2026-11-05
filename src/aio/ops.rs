use std::borrow::Cow;
use std::sync::Arc;

use super::pool::Op;
use super::sweeper;
use crate::bson;
use crate::error::Error;
use crate::index::{self, Graph};
use crate::memcached::engine::Elem;
use crate::quant::Vector;

pub fn run(op: &Op) -> Result<Cow<'static, [u8]>, Error> {
    match op {
        Op::Search {
            index,
            vector,
            k,
            with_meta,
        } => search(index, vector, *k, *with_meta).map(Cow::Owned),
        Op::Insert { index, map, key } => {
            insert(of_map(index, *map), *key).map(|()| Cow::Borrowed(&b"STORED\r\n"[..]))
        }
        Op::Replace {
            index,
            map,
            old,
            new,
        } => replace(of_map(index, *map), *old, *new).map(|()| Cow::Borrowed(&b"REPLACED\r\n"[..])),
        Op::SetMeta {
            index,
            map,
            old,
            new,
        } => replace(of_map(index, *map), *old, *new).map(|()| Cow::Borrowed(&b"STORED\r\n"[..])),
        Op::Delete { index, map, key } => {
            delete(of_map(index, *map), *key).map(|()| Cow::Borrowed(&b"DELETED\r\n"[..]))
        }
        Op::Create { index, map, config } => {
            index::create(index, *map, config).map(|()| Cow::Borrowed(&b"CREATED\r\n"[..]))
        }
        Op::Drop { index, map } => index::remove(index, *map)
            .then_some(Cow::Borrowed(&b"DROPPED\r\n"[..]))
            .ok_or(Error::NotFound),
        // A lane ends on it before it runs.
        Op::Shutdown => Err(Error::Internal),
    }
}

fn vector(elem: &Elem, graph: &Graph) -> Result<Vector, Error> {
    elem.vector(graph.scalar_kind()).ok_or(Error::Internal)
}

fn graph(index: &str) -> Result<Arc<Graph>, Error> {
    index::get(index).ok_or(Error::NotFound)
}

fn of_map(index: &str, map: usize) -> Result<Arc<Graph>, Error> {
    index::of_map(index, map).ok_or(Error::NotFound)
}

// An element a lane takes in stays held, in its graph or not, until a delete or a replace takes it
// out, which releases it exactly once.

fn insert(graph: Result<Arc<Graph>, Error>, key: usize) -> Result<(), Error> {
    let elem = Elem::new(key)?;
    let graph = graph?;
    graph.add(key as u64, &vector(&elem, &graph)?)
}

fn replace(graph: Result<Arc<Graph>, Error>, old: usize, new: usize) -> Result<(), Error> {
    let moved = graph.and_then(|graph| {
        let (old_elem, new_elem) = (Elem::new(old)?, Elem::new(new)?);
        let (from, to) = (old as u64, new as u64);
        let same = graph.contains(from) && old_elem.same_vector(&new_elem) == Some(true);
        if same && graph.rename(from, to)? {
            return Ok(());
        }
        graph.remove(from)?;
        vector(&new_elem, &graph).and_then(|v| graph.add(to, &v))
    });
    sweeper::release(old);
    moved
}

fn delete(graph: Result<Arc<Graph>, Error>, key: usize) -> Result<(), Error> {
    let removed = graph.and_then(|graph| graph.remove(key as u64));
    sweeper::release(key);
    removed.map(|_| ())
}

fn search(index: &str, vector: &Vector, k: usize, with_meta: bool) -> Result<Vec<u8>, Error> {
    let _reading = sweeper::block().ok_or(Error::Internal)?;
    graph(index)?.search_with(vector, k, |found| {
        let mut out = Vec::new();
        put(&mut out, format!("VECTORS {}\r\n", found.len()).as_bytes())?;
        for &(key, score) in found {
            let elem = Elem::new(usize::try_from(key).map_err(|_| Error::Internal)?)?;
            put(&mut out, elem.vkey().ok_or(Error::Internal)?)?;
            put(&mut out, format!(" {}", score_text(score)).as_bytes())?;
            if with_meta {
                let json = bson::to_json(elem.meta().ok_or(Error::Internal)?.as_bytes())?;
                put(&mut out, format!(" {}\r\n", json.len()).as_bytes())?;
                put(&mut out, json.as_bytes())?;
            }
            put(&mut out, b"\r\n")?;
        }
        put(&mut out, b"END\r\n")?;
        Ok(out)
    })
}

/// The shortest text that reads back as `score`, with an exponent where plain digits run long.
fn score_text(score: f32) -> String {
    let magnitude = score.abs();
    if magnitude != 0.0 && !(1e-4..1e16).contains(&magnitude) {
        format!("{score:e}")
    } else {
        format!("{score}")
    }
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    out.try_reserve(bytes.len())
        .map_err(|_| Error::OutOfMemory)?;
    out.extend_from_slice(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_score_far_from_one_takes_an_exponent() {
        assert_eq!(score_text(0.0), "0");
        assert_eq!(score_text(0.006_116_266), "0.006116266");
        assert_eq!(score_text(1.0), "1");
        assert_eq!(score_text(2.220_446e-16), "2.220446e-16");
        assert_eq!(score_text(-1.5e-5), "-1.5e-5");
        assert_eq!(score_text(3.4e20), "3.4e20");
    }

    #[test]
    fn a_missing_index_is_not_found() {
        crate::memcached::server::fake::install();
        let op = Op::Delete {
            index: "nowhere".into(),
            map: 0,
            key: 8,
        };
        assert_eq!(run(&op), Err(Error::NotFound));
    }
}
