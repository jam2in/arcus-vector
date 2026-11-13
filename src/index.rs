mod graph;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use usearch::{IndexOptions, ScalarKind};

use crate::error::Error;
use crate::protocol::{self, Create};
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
    // A native index reserves per-thread buffers up front; skip building one for a taken name.
    if read().contains_key(name) {
        return Err(Error::Exists);
    }
    let graph = Arc::new(Graph::new(&options)?);
    let mut indexes = write();
    if indexes.contains_key(name) {
        return Err(Error::Exists);
    }
    indexes.insert(name.to_owned(), Entry { map, graph });
    Ok(())
}

/// Takes out the index at `name` if `map` holds it.
pub fn remove(name: &str, map: usize) -> bool {
    let mut indexes = write();
    let held = indexes.get(name).is_some_and(|entry| entry.map == map);
    held && indexes.remove(name).is_some()
}

/// The index at `name`, for a command.
pub fn get(name: &str) -> Option<Arc<Graph>> {
    read().get(name).map(|entry| Arc::clone(&entry.graph))
}

/// The index at `name` if `map` holds it, for an event of that Map.
pub fn of_map(name: &str, map: usize) -> Option<Arc<Graph>> {
    let indexes = read();
    let entry = indexes.get(name).filter(|entry| entry.map == map)?;
    Some(Arc::clone(&entry.graph))
}

pub fn dimensions(name: &str) -> Option<usize> {
    get(name).map(|graph| graph.dimensions())
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

/// Whether a map value is marked as an index's config, `{ "type": "vector", ... }`.
pub fn is_config(value: &[u8]) -> bool {
    bson::RawDocument::from_bytes(value)
        .is_ok_and(|doc| doc.get_str("type").is_ok_and(|kind| kind == "vector"))
}

/// `{ "type": "vector", "dim", "metric", "quant", "M", "efc", "efs" }`
fn options_of(config: &[u8]) -> Option<IndexOptions> {
    let doc = bson::Document::from_reader(config).ok()?;
    let int = |key| usize::try_from(doc.get_i32(key).ok()?).ok();
    Some(IndexOptions {
        dimensions: int("dim")?,
        metric: protocol::parse_metric(doc.get_str("metric").ok()?)?,
        quantization: protocol::parse_quant(doc.get_str("quant").ok()?)?,
        connectivity: int("M")?,
        expansion_add: int("efc")?,
        expansion_search: int("efs")?,
        multi: false,
    })
}

/// The config `vop create` stores as its map's value.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "vop create does not store it on the engine yet")
)]
pub fn config_of(create: &Create<'_>) -> Result<Vec<u8>, Error> {
    let int = |n: usize| i32::try_from(n).map_err(|_| Error::BadCommandLine);
    bson::doc! {
        "type": "vector",
        "dim": i32::from(create.dim),
        "metric": protocol::metric_word(create.metric),
        "quant": protocol::quant_word(create.quant),
        "M": int(create.m)?,
        "efc": int(create.efc)?,
        "efs": int(create.efs)?,
    }
    .to_vec()
    .map_err(|_| Error::Internal)
}

/// A config for an index of `dim` dimensions, with the default graph parameters.
#[cfg(test)]
pub fn test_config(dim: i32, metric: &str, quant: &str) -> Vec<u8> {
    bson::doc! {
        "type": "vector", "dim": dim, "metric": metric, "quant": quant, "M": 16, "efc": 128,
        "efs": 64,
    }
    .to_vec()
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_line_reads_back_from_its_config() {
        let line = "vop create back 8 0 metric hamming quant b1 M 8 efc 40 efs 20";
        let Ok(protocol::Request::Create(create)) =
            protocol::parse(protocol::Tokens::from_line(line).words())
        else {
            panic!("a create line");
        };
        let config = config_of(&create).unwrap();
        assert!(is_config(&config));
        let options = options_of(&config).unwrap();
        assert_eq!((options.dimensions, options.connectivity), (8, 8));
        assert_eq!((options.expansion_add, options.expansion_search), (40, 20));
        assert_eq!(
            (options.metric, options.quantization),
            (create.metric, create.quant)
        );
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
    fn only_a_value_marked_as_vector_is_a_config() {
        assert!(!is_config(b"hello"));
        assert!(!is_config(&bson::doc! { "dim": 8 }.to_vec().unwrap()));
        assert!(!is_config(
            &bson::doc! { "type": "other" }.to_vec().unwrap()
        ));
        assert!(!is_config(&bson::doc! { "type": 1 }.to_vec().unwrap()));
        assert!(is_config(
            &bson::doc! { "type": "vector" }.to_vec().unwrap()
        ));
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
        assert_eq!(dimensions("gone"), None);
    }
}
