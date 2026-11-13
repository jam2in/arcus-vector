use std::ops::RangeInclusive;

use usearch::{IndexOptions, MetricKind, ScalarKind};

use crate::ascii::Create;
use crate::error::Error;

pub const DEFAULT_M: usize = 16;
pub const M_RANGE: RangeInclusive<usize> = 2..=1024;
pub const DEFAULT_EFC: usize = 128;
pub const DEFAULT_EFS: usize = 64;
pub const EF_RANGE: RangeInclusive<usize> = 1..=65536;
pub const DEFAULT_MAXCOUNT: u32 = 50_000;
pub const MAX_MAXCOUNT: u32 = 1_000_000;

pub fn parse_metric(word: &str) -> Option<MetricKind> {
    match word {
        "cos" => Some(MetricKind::Cos),
        "l2" => Some(MetricKind::L2sq),
        "ip" => Some(MetricKind::IP),
        "hamming" => Some(MetricKind::Hamming),
        "tanimoto" => Some(MetricKind::Tanimoto),
        _ => None,
    }
}

pub const fn metric_word(metric: MetricKind) -> &'static str {
    match metric {
        MetricKind::L2sq => "l2",
        MetricKind::IP => "ip",
        MetricKind::Hamming => "hamming",
        MetricKind::Tanimoto => "tanimoto",
        _ => "cos",
    }
}

pub fn parse_quant(word: &str) -> Option<ScalarKind> {
    match word {
        "f32" => Some(ScalarKind::F32),
        "f16" => Some(ScalarKind::F16),
        "i8" => Some(ScalarKind::I8),
        "b1" => Some(ScalarKind::B1),
        _ => None,
    }
}

pub const fn quant_word(quant: ScalarKind) -> &'static str {
    match quant {
        ScalarKind::F16 => "f16",
        ScalarKind::I8 => "i8",
        ScalarKind::B1 => "b1",
        _ => "f32",
    }
}

/// Whether a map value is marked as an index's config, `{ "type": "vector", ... }`.
pub fn is_config(value: &[u8]) -> bool {
    bson::RawDocument::from_bytes(value)
        .is_ok_and(|doc| doc.get_str("type").is_ok_and(|kind| kind == "vector"))
}

/// `{ "type": "vector", "dim", "metric", "quant", "M", "efc", "efs" }`
pub(super) fn options_of(config: &[u8]) -> Option<IndexOptions> {
    let doc = bson::Document::from_reader(config).ok()?;
    let int = |key| usize::try_from(doc.get_i32(key).ok()?).ok();
    Some(IndexOptions {
        dimensions: int("dim")?,
        metric: parse_metric(doc.get_str("metric").ok()?)?,
        quantization: parse_quant(doc.get_str("quant").ok()?)?,
        connectivity: int("M")?,
        expansion_add: int("efc")?,
        expansion_search: int("efs")?,
        multi: false,
    })
}

/// The config `vop create` stores as its map's value.
pub fn config_of(create: &Create<'_>) -> Result<Vec<u8>, Error> {
    let int = |n: usize| i32::try_from(n).map_err(|_| Error::BadCommandLine);
    bson::doc! {
        "type": "vector",
        "dim": i32::from(create.dim),
        "metric": metric_word(create.metric),
        "quant": quant_word(create.quant),
        "M": int(create.m)?,
        "efc": int(create.efc)?,
        "efs": int(create.efs)?,
    }
    .to_vec()
    .map_err(|_| Error::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ascii;

    #[test]
    fn a_create_line_reads_back_from_its_config() {
        let line = "vop create back 8 0 metric hamming quant b1 M 8 efc 40 efs 20";
        let Ok(ascii::Request::Create(create)) =
            ascii::parse(ascii::Tokens::from_line(line).words())
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
}
