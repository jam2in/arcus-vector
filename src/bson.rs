//! Metadata: a JSON object on the wire, a BSON document in the engine.
//!
//! The document holds only what JSON has: every integer is an int64, and a `$` key such as
//! `{"$numberLong":"5"}` is a plain field, not extended JSON.

use serde_json::Value as Json;

use crate::error::Error;

pub const MAX_DEPTH: usize = 32;
/// `{}`: its length, 5, then the terminating zero.
pub const EMPTY: [u8; 5] = [5, 0, 0, 0, 0];

pub fn from_json(json: &[u8]) -> Result<Vec<u8>, Error> {
    let Ok(json @ Json::Object(_)) = serde_json::from_slice(json) else {
        return Err(Error::BadValue);
    };
    if depth(&json) > MAX_DEPTH {
        return Err(Error::BadValue);
    }
    // Refuses a NUL in a key and an integer past int64.
    bson::serialize_to_vec(&json).map_err(|_| Error::BadValue)
}

fn depth(v: &Json) -> usize {
    match v {
        Json::Object(m) => 1 + m.values().map(depth).max().unwrap_or(0),
        Json::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

pub fn to_json(doc: &[u8]) -> Result<String, Error> {
    let json: Json = bson::deserialize_from_slice(doc).map_err(|_| Error::Internal)?;
    serde_json::to_string(&json).map_err(|_| Error::Internal)
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
    fn an_empty_object_is_the_empty_document() {
        assert_eq!(from_json(b"{}"), Ok(EMPTY.to_vec()));
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
    fn every_integer_is_an_int64() {
        let doc = from_json(br#"{"a":1,"b":-5000000000}"#).unwrap();
        let doc = bson::RawDocument::from_bytes(&doc).unwrap();
        assert_eq!(doc.get_i64("a").unwrap(), 1);
        assert_eq!(doc.get_i64("b").unwrap(), -5_000_000_000);
    }

    #[test]
    fn a_dollar_key_is_a_plain_field() {
        let json = r#"{"$date":1,"a":{"$numberLong":"5"},"b":{"$numberLong":"x","c":1}}"#;
        assert_eq!(round_trip(json), json);
    }

    #[test]
    fn the_int64_range_round_trips() {
        let json = r#"{"a":9223372036854775807,"b":-9223372036854775808}"#;
        assert_eq!(round_trip(json), json);
    }

    #[test]
    fn a_nul_key_or_an_integer_past_int64_is_refused() {
        for json in ["{\"a\\u0000b\":1}", r#"{"a":9223372036854775808}"#] {
            assert_eq!(from_json(json.as_bytes()), Err(Error::BadValue), "{json}");
        }
    }

    #[test]
    fn a_truncated_document_is_an_internal_error() {
        let doc = from_json(br#"{"a":1}"#).unwrap();
        assert_eq!(to_json(&doc[..doc.len() - 1]), Err(Error::Internal));
    }
}
