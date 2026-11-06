use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard};

use numkong::f16;
use usearch::{Index, IndexOptions, ScalarKind, b1x8};

use crate::aio::NUM_LANES;
use crate::error::Error;
use crate::quant::Vector;

const MIN_CAPACITY: usize = 1024;

pub struct Graph {
    index: RwLock<Index>,
    reserved: AtomicUsize,
    adding: AtomicUsize,
    populated: AtomicBool,
    dim: usize,
    kind: ScalarKind,
}

impl Graph {
    pub fn new(options: &IndexOptions) -> Result<Self, Error> {
        let index = Index::new(options).map_err(|_| Error::Internal)?;
        index
            .reserve_capacity_and_threads(MIN_CAPACITY, NUM_LANES)
            .map_err(|_| Error::OutOfMemory)?;
        Ok(Self {
            index: RwLock::new(index),
            reserved: AtomicUsize::new(MIN_CAPACITY),
            adding: AtomicUsize::new(0),
            populated: AtomicBool::new(false),
            dim: options.dimensions,
            kind: options.quantization,
        })
    }

    pub fn dimensions(&self) -> usize {
        self.dim
    }

    pub fn scalar_kind(&self) -> ScalarKind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.read().size()
    }

    pub fn add(&self, key: u64, vector: &Vector) -> Result<(), Error> {
        let adding = self.adding.fetch_add(1, Ordering::AcqRel) + 1;
        let added = self.reserve(self.len() + adding).and_then(|()| {
            let index = self.read();
            match vector {
                Vector::F32(v) => index.add(key, v),
                Vector::F16(v) => index.add(key, as_usearch_f16(v)),
                Vector::I8(v) => index.add(key, v),
                Vector::B1(v) => index.add(key, b1x8::from_u8s(v)),
            }
            .map_err(|_| Error::Internal)
        });
        self.adding.fetch_sub(1, Ordering::AcqRel);
        added?;
        self.populated.store(true, Ordering::Release);
        Ok(())
    }

    pub fn contains(&self, key: u64) -> bool {
        self.read().contains(key)
    }

    pub fn remove(&self, key: u64) -> Result<bool, Error> {
        Ok(self.read().remove(key).map_err(|_| Error::Internal)? > 0)
    }

    pub fn rename(&self, from: u64, to: u64) -> Result<bool, Error> {
        Ok(self.read().rename(from, to).map_err(|_| Error::Internal)? > 0)
    }

    /// Runs `f` on what a search finds.
    pub fn search_with<T>(
        &self,
        query: &Vector,
        count: usize,
        f: impl FnOnce(&[(u64, f32)]) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if !self.populated.load(Ordering::Acquire) {
            return f(&[]);
        }
        let index = self.read();
        let count = count.min(index.size().max(1));
        let matches = match query {
            Vector::F32(q) => index.search(q, count),
            Vector::F16(q) => index.search(as_usearch_f16(q), count),
            Vector::I8(q) => index.search(q, count),
            Vector::B1(q) => index.search(b1x8::from_u8s(q), count),
        }
        .map_err(|_| Error::Internal)?;
        let found: Vec<(u64, f32)> = matches.keys.into_iter().zip(matches.distances).collect();
        f(&found)
    }

    fn reserve(&self, needed: usize) -> Result<(), Error> {
        if needed <= self.reserved.load(Ordering::Acquire) {
            return Ok(());
        }
        let index = self.index.write().unwrap_or_else(PoisonError::into_inner);
        let reserved = self.reserved.load(Ordering::Acquire);
        if needed <= reserved {
            return Ok(());
        }
        let target = needed.max(reserved * 2);
        index
            .reserve_capacity_and_threads(target, NUM_LANES)
            .map_err(|_| Error::OutOfMemory)?;
        self.reserved.store(target, Ordering::Release);
        Ok(())
    }

    fn read(&self) -> RwLockReadGuard<'_, Index> {
        self.index.read().unwrap_or_else(PoisonError::into_inner)
    }
}

fn as_usearch_f16(v: &[f16]) -> &[usearch::f16] {
    // SAFETY: both are `repr(transparent)` over 16 bits, so the slices share their layout.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), v.len()) }
}

#[cfg(test)]
mod tests {
    use usearch::MetricKind;

    use super::*;
    use crate::quant;

    fn search(g: &Graph, query: &Vector, count: usize) -> Result<Vec<(u64, f32)>, Error> {
        g.search_with(query, count, |found| Ok(found.to_vec()))
    }

    fn graph(dim: usize, kind: ScalarKind) -> Graph {
        Graph::new(&IndexOptions {
            dimensions: dim,
            metric: MetricKind::Cos,
            quantization: kind,
            ..IndexOptions::default()
        })
        .unwrap()
    }

    fn vector(values: &[f32], kind: ScalarKind) -> Vector {
        quant::quantize(values.to_vec(), kind).unwrap()
    }

    #[test]
    fn a_search_before_any_add_finds_nothing() {
        let g = graph(4, ScalarKind::F32);
        let found = search(&g, &vector(&[1.0, 0.0, 0.0, 0.0], ScalarKind::F32), 3);
        assert_eq!(found, Ok(Vec::new()));
    }

    #[test]
    fn each_kind_finds_what_it_added() {
        for kind in [
            ScalarKind::F32,
            ScalarKind::F16,
            ScalarKind::I8,
            ScalarKind::B1,
        ] {
            let g = graph(8, kind);
            let near = [1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0];
            let far = [-1.0, -1.0, -1.0, -1.0, 1.0, 1.0, 1.0, 1.0];
            assert_eq!(g.add(10, &vector(&near, kind)), Ok(()));
            assert_eq!(g.add(20, &vector(&far, kind)), Ok(()));
            let found = search(&g, &vector(&near, kind), 1).unwrap();
            assert_eq!(found.first().map(|&(key, _)| key), Some(10), "{kind:?}");
        }
    }

    #[test]
    fn the_count_is_capped_by_what_it_holds() {
        let g = graph(4, ScalarKind::F32);
        g.add(1, &vector(&[1.0, 0.0, 0.0, 0.0], ScalarKind::F32))
            .unwrap();
        let found = search(
            &g,
            &vector(&[1.0, 0.0, 0.0, 0.0], ScalarKind::F32),
            usize::MAX,
        );
        assert_eq!(found.map(|f| f.len()), Ok(1));
    }

    #[test]
    fn adds_past_the_first_reservation_grow_it() {
        let g = graph(4, ScalarKind::F32);
        for key in 0..(MIN_CAPACITY as u64 + 10) {
            let v = [key as f32, 1.0, 0.0, 0.0];
            assert_eq!(g.add(key, &vector(&v, ScalarKind::F32)), Ok(()));
        }
        assert_eq!(g.len(), MIN_CAPACITY + 10);
        assert!(g.reserved.load(Ordering::Acquire) >= MIN_CAPACITY * 2);
    }

    #[test]
    fn a_key_is_renamed_then_removed() {
        let g = graph(4, ScalarKind::F32);
        let v = vector(&[0.0, 1.0, 0.0, 0.0], ScalarKind::F32);
        g.add(1, &v).unwrap();
        assert_eq!(g.rename(1, 2), Ok(true));
        assert_eq!(g.rename(1, 3), Ok(false));
        assert_eq!(search(&g, &v, 1).unwrap()[0].0, 2);
        assert_eq!(g.remove(2), Ok(true));
        assert_eq!(g.remove(2), Ok(false));
        assert_eq!(g.len(), 0);
    }

    #[test]
    fn lanes_add_and_search_at_once_while_it_grows() {
        let g = graph(4, ScalarKind::F32);
        std::thread::scope(|s| {
            for t in 0..4u64 {
                let g = &g;
                s.spawn(move || {
                    for n in 0..600u64 {
                        let v = vector(&[t as f32, n as f32, 1.0, 0.0], ScalarKind::F32);
                        assert_eq!(g.add(t * 1000 + n, &v), Ok(()));
                        assert!(search(g, &v, 3).is_ok());
                    }
                });
            }
        });
        assert_eq!(g.len(), 2400);
    }
}
