use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard};

use numkong::f16;
use usearch::{Index, IndexOptions, ScalarKind, VectorType, b1x8};

use crate::data::vector::Vector;
use crate::error::Error;
use crate::lane::NUM_LANES;

const MIN_CAPACITY: usize = 1024;
/// The key usearch writes into a removed slot: `default_free_value`, the largest key.
const FREE_KEY: u64 = u64::MAX;

pub struct Graph {
    index: RwLock<Index>,
    /// Held by the commands in the graph, and alone by [`Graph::close`].
    entry: RwLock<()>,
    closed: AtomicBool,
    reserved: AtomicUsize,
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
            entry: RwLock::new(()),
            closed: AtomicBool::new(false),
            reserved: AtomicUsize::new(MIN_CAPACITY),
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

    pub fn layout(&self) -> (usize, ScalarKind) {
        (self.dim, self.kind)
    }

    /// Keeps the graph's name from passing to another until the guard drops, so a command reads
    /// the graph and writes its Map as one; `None` once closed.
    pub fn enter(&self) -> Option<RwLockReadGuard<'_, ()>> {
        let entered = self.entry.read().unwrap_or_else(PoisonError::into_inner);
        self.is_open().then_some(entered)
    }

    /// Bars commands from the graph, once those that entered it are done.
    pub fn close(&self) {
        let _alone = self.entry.write().unwrap_or_else(PoisonError::into_inner);
        self.closed.store(true, Ordering::Release);
    }

    /// Never waits, unlike [`Graph::enter`], so the registry's lock may be held around it.
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    pub fn len(&self) -> usize {
        self.read().size()
    }

    pub fn add(&self, key: u64, vector: &Vector) -> Result<(), Error> {
        self.reserve(self.len() + 1)?;
        let index = self.read();
        match vector {
            Vector::F32(v) => index.add(key, v),
            Vector::F16(v) => index.add(key, as_usearch_f16(v)),
            Vector::I8(v) => index.add(key, v),
            Vector::B1(v) => index.add(key, b1x8::from_u8s(v)),
        }
        .map_err(|_| Error::Internal)?;
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

    /// Runs `f` on what a search finds, among the keys `accept` takes if given.
    pub fn search_with<T>(
        &self,
        query: &Vector,
        count: usize,
        accept: Option<impl Fn(u64) -> bool>,
        f: impl FnOnce(&[(u64, f32)]) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if !self.populated.load(Ordering::Acquire) {
            return f(&[]);
        }
        let index = self.read();
        let count = count.min(index.size().max(1));
        let accept = accept.as_ref();
        let found = match query {
            Vector::F32(q) => find(&index, q, count, accept),
            Vector::F16(q) => find(&index, as_usearch_f16(q), count, accept),
            Vector::I8(q) => find(&index, q, count, accept),
            Vector::B1(q) => find(&index, b1x8::from_u8s(q), count, accept),
        }?;
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

fn find<T: VectorType>(
    index: &Index,
    query: &[T],
    count: usize,
    accept: Option<&impl Fn(u64) -> bool>,
) -> Result<Vec<(u64, f32)>, Error> {
    let matches = match accept {
        None => index.search(query, count),
        // usearch calls it from C++, which a panic must not unwind into.
        Some(accept) => {
            index.filtered_search(query, count, |key| crate::guard(|| false, || accept(key)))
        }
    }
    .map_err(|_| Error::Internal)?;
    // usearch reads a result's key from its slot only as it hands the result out; a remove
    // meanwhile leaves the free key there, which names no element.
    let found = matches.keys.into_iter().zip(matches.distances);
    Ok(found.filter(|&(key, _)| key != FREE_KEY).collect())
}

fn as_usearch_f16(v: &[f16]) -> &[usearch::f16] {
    // SAFETY: both are `repr(transparent)` over 16 bits, so the slices share their layout.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), v.len()) }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use usearch::MetricKind;

    use super::*;
    use crate::data::vector;

    fn search(g: &Graph, query: &Vector, count: usize) -> Result<Vec<(u64, f32)>, Error> {
        g.search_with(query, count, None::<fn(u64) -> bool>, |found| {
            Ok(found.to_vec())
        })
    }

    fn graph(dim: usize, kind: ScalarKind) -> Graph {
        let metric = if kind == ScalarKind::B1 {
            MetricKind::Hamming
        } else {
            MetricKind::Cos
        };
        Graph::new(&IndexOptions {
            dimensions: dim,
            metric,
            quantization: kind,
            ..IndexOptions::default()
        })
        .unwrap()
    }

    fn vector(values: &[f32], kind: ScalarKind) -> Vector {
        vector::quantize(values.to_vec(), kind).unwrap()
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
    fn a_filtered_search_finds_only_what_it_accepts() {
        let g = graph(4, ScalarKind::F32);
        for key in 0..10u64 {
            let v = vector(&[key as f32, 1.0, 0.0, 0.0], ScalarKind::F32);
            g.add(key, &v).unwrap();
        }
        let query = vector(&[1.0, 1.0, 0.0, 0.0], ScalarKind::F32);
        let even = |key: u64| key.is_multiple_of(2);
        let keys = g.search_with(&query, 3, Some(even), |found| {
            Ok(found.iter().map(|&(key, _)| key).collect::<Vec<_>>())
        });
        let keys = keys.unwrap();
        assert_eq!(keys.len(), 3);
        assert!(keys.iter().all(|&key| even(key)), "{keys:?}");
    }

    #[test]
    fn a_panicking_filter_rejects_rather_than_unwinding() {
        let g = graph(4, ScalarKind::F32);
        let v = vector(&[1.0, 0.0, 0.0, 0.0], ScalarKind::F32);
        g.add(1, &v).unwrap();
        let panics = |_: u64| -> bool { panic!("in a filter") };
        let found = g.search_with(&v, 1, Some(panics), |found| Ok(found.len()));
        assert_eq!(found, Ok(0));
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
    fn its_lane_adds_while_others_search_as_it_grows() {
        let g = graph(4, ScalarKind::F32);
        thread::scope(|s| {
            let g = &g;
            for t in 0..3u64 {
                s.spawn(move || {
                    for n in 0..600u64 {
                        let q = vector(&[t as f32, n as f32, 1.0, 0.0], ScalarKind::F32);
                        assert!(search(g, &q, 3).is_ok());
                    }
                });
            }
            for n in 0..2400u64 {
                let v = vector(&[(n % 4) as f32, (n / 4) as f32, 1.0, 0.0], ScalarKind::F32);
                assert_eq!(g.add(n, &v), Ok(()));
            }
        });
        assert_eq!(g.len(), 2400);
    }

    #[test]
    fn a_close_waits_for_the_commands_in_the_graph_then_bars_the_rest() {
        let g = Arc::new(graph(4, ScalarKind::F32));
        let entered = g.enter().unwrap();
        let closer = thread::spawn({
            let g = Arc::clone(&g);
            move || g.close()
        });
        thread::sleep(Duration::from_millis(50));
        assert!(!closer.is_finished());
        drop(entered);
        closer.join().unwrap();
        assert!(!g.is_open());
        assert!(g.enter().is_none());
    }
}
