pub mod config;
mod graph;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use usearch::ScalarKind;

use crate::error::Error;
use config::options_of;
pub use config::{config_of, is_config};
pub use graph::Graph;

/// An index with the address of the Map that holds it, so that the late events of a dropped Map
/// never reach one created again under its name.
struct Entry {
    map: usize,
    graph: Arc<Graph>,
}

static INDEXES: LazyLock<RwLock<HashMap<String, Entry>>> = LazyLock::new(RwLock::default);

pub fn create(name: &str, map: usize, config: &[u8]) -> Result<(), Error> {
    let options = options_of(config).ok_or(Error::TypeMismatch)?;
    let taken =
        |indexes: &HashMap<String, Entry>| indexes.get(name).is_some_and(|e| e.graph.is_open());
    // A native index reserves per-thread buffers up front; skip building one for a taken name.
    if taken(&read()) {
        return Err(Error::Exists);
    }
    let graph = Arc::new(Graph::new(&options)?);
    let mut indexes = write();
    if taken(&indexes) {
        return Err(Error::Exists);
    }
    indexes.insert(name.to_owned(), Entry { map, graph });
    Ok(())
}

/// Takes out the index at `name` if `map` holds it.
pub fn remove(name: &str, map: usize) -> bool {
    // Closed outside the registry's lock, which a command in the graph may yet take.
    let Some(graph) = of_map(name, map) else {
        return false;
    };
    graph.close();
    let mut indexes = write();
    let held = indexes.get(name).is_some_and(|entry| entry.map == map);
    held && indexes.remove(name).is_some()
}

/// Closes the index at `name` to commands, as its Map is gone; its events still find it until its
/// drop.
pub fn retire(name: &str) {
    let graph = read().get(name).map(|entry| Arc::clone(&entry.graph));
    if let Some(graph) = graph {
        graph.close();
    }
}

/// The index at `name`, for a command.
pub fn get(name: &str) -> Option<Arc<Graph>> {
    let indexes = read();
    let entry = indexes.get(name).filter(|entry| entry.graph.is_open())?;
    Some(Arc::clone(&entry.graph))
}

/// The index at `name` if `map` holds it, for an event of that Map.
pub fn of_map(name: &str, map: usize) -> Option<Arc<Graph>> {
    let indexes = read();
    let entry = indexes.get(name).filter(|entry| entry.map == map)?;
    Some(Arc::clone(&entry.graph))
}

/// The dimensions and scalar kind of the index at `name`.
pub fn layout(name: &str) -> Option<(usize, ScalarKind)> {
    get(name).map(|graph| (graph.dimensions(), graph.scalar_kind()))
}

fn read() -> RwLockReadGuard<'static, HashMap<String, Entry>> {
    INDEXES.read().unwrap_or_else(PoisonError::into_inner)
}

fn write() -> RwLockWriteGuard<'static, HashMap<String, Entry>> {
    INDEXES.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A config for an index of `dim` dimensions, with the default graph parameters.
    pub fn test_config(dim: i32, metric: &str, quant: &str) -> Vec<u8> {
        bson::doc! {
            "type": "vector", "dim": dim, "metric": metric, "quant": quant, "M": 16, "efc": 128,
            "efs": 64,
        }
        .to_vec()
        .unwrap()
    }

    #[test]
    fn a_name_is_created_once() {
        let config = test_config(4, "cos", "f32");
        assert_eq!(create("once", 0, &config), Ok(()));
        assert_eq!(create("once", 0, &config), Err(Error::Exists));
        assert!(read().contains_key("once"));
    }

    #[test]
    fn every_metric_and_quant_builds() {
        for (n, (metric, quant)) in [
            ("cos", "f32"),
            ("l2", "f16"),
            ("ip", "i8"),
            ("hamming", "b1"),
            ("tanimoto", "b1"),
        ]
        .into_iter()
        .enumerate()
        {
            let config = test_config(8, metric, quant);
            assert_eq!(create(&format!("kind{n}"), 0, &config), Ok(()));
        }
    }

    #[test]
    fn a_map_value_builds_the_index_it_describes() {
        let config = bson::doc! {
            "type": "vector", "dim": 8, "metric": "l2", "quant": "f16", "M": 32, "efc": 200,
            "efs": 10,
        };
        assert_eq!(create("fromcfg", 0, &config.to_vec().unwrap()), Ok(()));
        assert_eq!(layout("fromcfg"), Some((8, ScalarKind::F16)));
    }

    #[test]
    fn a_marked_value_of_another_shape_is_not_an_index() {
        let config = bson::doc! { "type": "vector", "dim": 8 };
        assert_eq!(
            create("other", 0, &config.to_vec().unwrap()),
            Err(Error::TypeMismatch)
        );
    }

    #[test]
    fn an_event_reaches_only_the_index_its_map_holds() {
        assert_eq!(create("remade", 7, &test_config(4, "cos", "f32")), Ok(()));
        assert!(of_map("remade", 7).is_some());
        assert!(of_map("remade", 8).is_none());
        assert!(get("remade").is_some());
    }

    #[test]
    fn removing_takes_an_index_out_once() {
        assert_eq!(create("gone", 0, &test_config(4, "cos", "f32")), Ok(()));
        assert!(!remove("gone", 1));
        assert!(remove("gone", 0));
        assert!(!remove("gone", 0));
        assert!(get("gone").is_none());
    }

    #[test]
    fn a_retired_index_is_hidden_from_commands_until_its_drop() {
        let config = test_config(4, "cos", "f32");
        assert_eq!(create("retired", 1, &config), Ok(()));
        retire("retired");
        assert!(get("retired").is_none());
        assert!(of_map("retired", 1).is_some());
        assert!(remove("retired", 1));
        assert_eq!(create("retired", 2, &config), Ok(()));
        assert!(get("retired").is_some());
    }
}
