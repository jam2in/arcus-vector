mod graph;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use usearch::{IndexOptions, ScalarKind};

use crate::error::Error;
use crate::protocol::Create;
pub use graph::Graph;

static INDEXES: LazyLock<RwLock<HashMap<String, Arc<Graph>>>> = LazyLock::new(RwLock::default);

/// Builds the index `spec` describes and registers it under its name.
pub fn create(spec: &Create<'_>) -> Result<(), Error> {
    // A native index reserves per-thread buffers up front; skip building one for a taken name.
    if read().contains_key(spec.index) {
        return Err(Error::Exists);
    }
    let graph = Arc::new(Graph::new(&options(spec))?);
    let mut indexes = write();
    if indexes.contains_key(spec.index) {
        return Err(Error::Exists);
    }
    indexes.insert(spec.index.to_owned(), graph);
    Ok(())
}

#[expect(dead_code, reason = "the lanes run no op yet")]
pub fn get(name: &str) -> Option<Arc<Graph>> {
    read().get(name).cloned()
}

pub fn dimensions(name: &str) -> Option<usize> {
    read().get(name).map(|graph| graph.dimensions())
}

/// The dimensions and scalar kind of the index at `name`.
pub fn layout(name: &str) -> Option<(usize, ScalarKind)> {
    read()
        .get(name)
        .map(|graph| (graph.dimensions(), graph.scalar_kind()))
}

fn read() -> RwLockReadGuard<'static, HashMap<String, Arc<Graph>>> {
    INDEXES.read().unwrap_or_else(PoisonError::into_inner)
}

fn write() -> RwLockWriteGuard<'static, HashMap<String, Arc<Graph>>> {
    INDEXES.write().unwrap_or_else(PoisonError::into_inner)
}

fn options(spec: &Create<'_>) -> IndexOptions {
    IndexOptions {
        dimensions: spec.dim.into(),
        metric: spec.metric,
        quantization: spec.quant,
        connectivity: spec.m,
        expansion_add: spec.efc,
        expansion_search: spec.efs,
        multi: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{self, Request, Tokens};

    fn create_line(line: &str) -> Result<(), Error> {
        let Ok(Request::Create(spec)) = protocol::parse(Tokens::from_line(line).words()) else {
            panic!("not a create line: {line}");
        };
        create(&spec)
    }

    #[test]
    fn a_name_is_created_once() {
        assert_eq!(create_line("vop create once 4 0"), Ok(()));
        assert_eq!(create_line("vop create once 4 0"), Err(Error::Exists));
        assert!(read().contains_key("once"));
    }

    #[test]
    fn every_metric_and_quant_builds() {
        for (n, line) in [
            "metric cos quant f32",
            "metric l2 quant f16",
            "metric ip quant i8",
            "metric hamming quant b1",
            "metric tanimoto quant b1",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(
                create_line(&format!("vop create kind{n} 8 0 {line}")),
                Ok(())
            );
        }
    }
}
