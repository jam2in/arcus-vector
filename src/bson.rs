//! Metadata: a JSON object on the wire, a BSON document in the engine.
//!
//! A field value may use the `bson` crate's extended JSON to pick its type: `{"$numberLong":"5"}`
//! is stored as int64 5. Top-level keys are always field names, or `{"$date":..,"a":..}` would be
//! read as a date and refused.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the commands that store metadata are not in yet")
)]

use bson::{Bson, Document, RawDocument};
use serde_json::Value as Json;

use crate::error::Error;

pub const MAX_DEPTH: usize = 32;

pub fn from_json(json: &[u8]) -> Result<Vec<u8>, Error> {
    let Ok(Json::Object(map)) = serde_json::from_slice(json) else {
        return Err(Error::BadValue);
    };
    if map.values().any(|v| depth(v) >= MAX_DEPTH) {
        return Err(Error::BadValue);
    }
    // Refuses a NUL in a key and a malformed reserved form such as `{"$numberLong":"x"}`.
    let doc = Document::try_from(map).map_err(|_| Error::BadValue)?;
    doc.to_vec().map_err(|_| Error::BadValue)
}

fn depth(v: &Json) -> usize {
    match v {
        Json::Object(m) => 1 + m.values().map(depth).max().unwrap_or(0),
        Json::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Compact JSON in relaxed extended JSON, so the types `from_json` stores come back as plain JSON.
pub fn to_json(doc: &[u8]) -> Result<String, Error> {
    let doc = RawDocument::from_bytes(doc)
        .and_then(Document::try_from)
        .map_err(|_| Error::Internal)?;
    serde_json::to_string(&Bson::Document(doc).into_relaxed_extjson()).map_err(|_| Error::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(json: &str) -> String {
        to_json(&from_json(json.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn every_scalar_type_round_trips() {
        let json = r#"{"a":1,"b":-5000000000,"c":1.5,"d":"x y","e":true,"f":null}"#;
        assert_eq!(round_trip(json), json);
    }

    #[test]
    fn nested_objects_and_arrays_round_trip() {
        let json = r#"{"o":{"p":[1,"two",{"q":false}]}}"#;
        assert_eq!(round_trip(json), json);
    }

    #[test]
    fn key_order_is_kept() {
        assert_eq!(round_trip(r#"{"z":1,"a":2}"#), r#"{"z":1,"a":2}"#);
    }

    #[test]
    fn a_double_keeps_its_fraction() {
        assert_eq!(round_trip(r#"{"a":2.0}"#), r#"{"a":2.0}"#);
    }

    #[test]
    fn the_empty_object_is_the_five_byte_document() {
        assert_eq!(from_json(b"{}").unwrap(), [5, 0, 0, 0, 0]);
        assert_eq!(to_json(&[5, 0, 0, 0, 0]).unwrap(), "{}");
    }

    #[test]
    fn only_a_json_object_is_metadata() {
        for json in ["[1,2]", "\"x\"", "1", "", "{"] {
            assert_eq!(from_json(json.as_bytes()), Err(Error::BadValue), "{json}");
        }
    }

    #[test]
    fn nesting_is_capped() {
        let nested = |n: usize| format!(r#"{{"a":{}1{}}}"#, "[".repeat(n), "]".repeat(n));
        assert!(from_json(nested(MAX_DEPTH - 1).as_bytes()).is_ok());
        assert_eq!(
            from_json(nested(MAX_DEPTH).as_bytes()),
            Err(Error::BadValue)
        );
    }

    #[test]
    fn an_extended_json_value_takes_its_type() {
        let doc = from_json(br#"{"price":{"$numberLong":"5"},"n":1}"#).unwrap();
        let doc = Document::from_reader(doc.as_slice()).unwrap();
        assert_eq!(doc.get("price"), Some(&Bson::Int64(5)));
        assert_eq!(doc.get("n"), Some(&Bson::Int32(1)));
    }

    #[test]
    fn top_level_keys_are_always_field_names() {
        let json = r#"{"$date":1,"a":2}"#;
        assert_eq!(round_trip(json), json);
    }

    #[test]
    fn a_malformed_value_or_key_is_refused() {
        for json in [r#"{"price":{"$numberLong":"x"}}"#, "{\"a\\u0000b\":1}"] {
            assert_eq!(from_json(json.as_bytes()), Err(Error::BadValue), "{json}");
        }
    }

    #[test]
    fn a_truncated_document_is_an_internal_error() {
        let doc = from_json(br#"{"a":1}"#).unwrap();
        assert_eq!(to_json(&doc[..doc.len() - 1]), Err(Error::Internal));
    }
}
