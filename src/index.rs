use std::collections::HashMap;
use std::sync::{LazyLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use usearch::{Index, IndexOptions};

use crate::error::Error;
use crate::protocol::Create;

static INDEXES: LazyLock<RwLock<HashMap<String, Index>>> = LazyLock::new(RwLock::default);

/// Builds the index `spec` describes and registers it under its name.
pub fn create(spec: &Create<'_>) -> Result<(), Error> {
    // A native index reserves per-thread buffers up front; skip building one for a taken name.
    if read().contains_key(spec.index) {
        return Err(Error::Exists);
    }
    let index = Index::new(&options(spec)).map_err(|_| Error::Internal)?;
    let mut indexes = write();
    if indexes.contains_key(spec.index) {
        return Err(Error::Exists);
    }
    indexes.insert(spec.index.to_owned(), index);
    Ok(())
}

pub fn exists(name: &str) -> bool {
    read().contains_key(name)
}

fn read() -> RwLockReadGuard<'static, HashMap<String, Index>> {
    INDEXES.read().unwrap_or_else(PoisonError::into_inner)
}

fn write() -> RwLockWriteGuard<'static, HashMap<String, Index>> {
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
    use std::os::raw::c_int;

    use super::*;
    use crate::protocol::{self, Request};

    fn create_line(line: &str) -> Result<(), Error> {
        let (_buf, tokens) = protocol::tokenize(line);
        // SAFETY: `tokens` point into `_buf`, which outlives the call.
        let tokens = unsafe { protocol::Tokens::new(tokens.as_ptr(), tokens.len() as c_int) };
        let Ok(Request::Create(spec)) = protocol::parse(&tokens) else {
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
