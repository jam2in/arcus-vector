//! A search filter: `<field> <op> <value>` conditions joined by `and` / `or`, taken left to right
//! with no precedence and no parentheses. Keywords are case-insensitive, and the spaces around an
//! operator are optional.

use std::cmp::Ordering;

use bson::{RawBsonRef, RawDocument};

use crate::error::Error;

/// `first`, then each condition joined to all before it: `a or b and c` is `(a or b) and c`.
#[derive(Debug, PartialEq)]
pub struct Filter {
    first: Cond,
    rest: Vec<(Join, Cond)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Join {
    And,
    Or,
}

#[derive(Debug, PartialEq)]
struct Cond {
    field: String,
    op: Op,
    value: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A quoted value is always a string; an unquoted one is `true`, `false`, `null`, an integer or a
/// finite double if it reads as one, and a string otherwise.
#[derive(Debug, PartialEq)]
enum Value {
    Int(i64),
    Double(f64),
    Str(String),
    Bool(bool),
    Null,
}

impl Filter {
    /// `BAD_VALUE` for anything that is not a whole filter.
    pub fn parse(src: &[u8]) -> Result<Self, Error> {
        let mut p = Parser { rest: src };
        let first = p.cond()?;
        let mut rest = Vec::new();
        loop {
            let join = if p.keyword(b"and") {
                Join::And
            } else if p.keyword(b"or") {
                Join::Or
            } else {
                break;
            };
            rest.push((join, p.cond()?));
        }
        p.skip_ws();
        if !p.rest.is_empty() {
            return Err(Error::BadValue);
        }
        Ok(Self { first, rest })
    }

    pub fn matches(&self, doc: &RawDocument) -> bool {
        self.rest
            .iter()
            .fold(self.first.holds(doc), |held, (join, cond)| match join {
                Join::And => held && cond.holds(doc),
                Join::Or => held || cond.holds(doc),
            })
    }
}

impl Cond {
    /// A missing field fails even `!=`. Anything without an order against the value (another
    /// type, a document, an array, NaN; booleans and null beyond `=`) is only unequal.
    fn holds(&self, doc: &RawDocument) -> bool {
        let Ok(Some(found)) = doc.get(&self.field) else {
            return false;
        };
        let equality = matches!(self.op, Op::Eq | Op::Ne);
        let order = match (found, &self.value) {
            (RawBsonRef::String(a), Value::Str(b)) => Some(a.cmp(b.as_str())),
            (RawBsonRef::Boolean(a), Value::Bool(b)) if equality => Some(a.cmp(b)),
            (RawBsonRef::Null, Value::Null) if equality => Some(Ordering::Equal),
            (RawBsonRef::Int32(a), Value::Int(b)) => Some(i64::from(a).cmp(b)),
            (RawBsonRef::Int64(a), Value::Int(b)) => Some(a.cmp(b)),
            (found, value) => double(found)
                .zip(value.double())
                .and_then(|(a, b)| a.partial_cmp(&b)),
        };
        order.map_or(self.op == Op::Ne, |order| self.op.holds(order))
    }
}

impl Op {
    fn holds(self, order: Ordering) -> bool {
        match self {
            Self::Eq => order.is_eq(),
            Self::Ne => order.is_ne(),
            Self::Lt => order.is_lt(),
            Self::Le => order.is_le(),
            Self::Gt => order.is_gt(),
            Self::Ge => order.is_ge(),
        }
    }
}

impl Value {
    fn double(&self) -> Option<f64> {
        match *self {
            Self::Int(n) => Some(n as f64),
            Self::Double(x) => Some(x),
            _ => None,
        }
    }
}

fn double(found: RawBsonRef<'_>) -> Option<f64> {
    match found {
        RawBsonRef::Double(x) => Some(x),
        RawBsonRef::Int32(n) => Some(f64::from(n)),
        RawBsonRef::Int64(n) => Some(n as f64),
        _ => None,
    }
}

struct Parser<'a> {
    rest: &'a [u8],
}

impl Parser<'_> {
    fn cond(&mut self) -> Result<Cond, Error> {
        self.skip_ws();
        let field = self.take_while(is_field_byte);
        if field.is_empty() {
            return Err(Error::BadValue);
        }
        let field = text(field)?;
        self.skip_ws();
        let op = self.op().ok_or(Error::BadValue)?;
        self.skip_ws();
        let value = self.value()?;
        Ok(Cond { field, op, value })
    }

    fn op(&mut self) -> Option<Op> {
        let (op, len) = match self.rest {
            [b'!', b'=', ..] => (Op::Ne, 2),
            [b'<', b'=', ..] => (Op::Le, 2),
            [b'>', b'=', ..] => (Op::Ge, 2),
            [b'=', ..] => (Op::Eq, 1),
            [b'<', ..] => (Op::Lt, 1),
            [b'>', ..] => (Op::Gt, 1),
            _ => return None,
        };
        self.rest = self.rest.get(len..)?;
        Some(op)
    }

    fn value(&mut self) -> Result<Value, Error> {
        if let [quote @ (b'\'' | b'"'), rest @ ..] = self.rest {
            let end = rest
                .iter()
                .position(|b| b == quote)
                .ok_or(Error::BadValue)?;
            let (inner, after) = rest.split_at(end);
            self.rest = after.get(1..).unwrap_or_default();
            return text(inner).map(Value::Str);
        }
        let word = text(self.take_while(|b| !b.is_ascii_whitespace()))?;
        Ok(match word.as_str() {
            "" => return Err(Error::BadValue),
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" => Value::Null,
            _ => match (word.parse::<i64>(), word.parse::<f64>()) {
                (Ok(n), _) => Value::Int(n),
                (_, Ok(x)) if x.is_finite() => Value::Double(x),
                _ => Value::Str(word),
            },
        })
    }

    /// Consumes `word` only as a whole word, so `android` is a field, not `and` + `roid`.
    fn keyword(&mut self, word: &[u8]) -> bool {
        self.skip_ws();
        let Some((head, tail)) = self.rest.split_at_checked(word.len()) else {
            return false;
        };
        if !head.eq_ignore_ascii_case(word) || tail.first().is_some_and(|&b| is_field_byte(b)) {
            return false;
        }
        self.rest = tail;
        true
    }

    fn skip_ws(&mut self) {
        self.take_while(|b| b.is_ascii_whitespace());
    }

    fn take_while(&mut self, keep: impl Fn(u8) -> bool) -> &[u8] {
        let n = self.rest.iter().take_while(|&&b| keep(b)).count();
        let (taken, rest) = self.rest.split_at(n);
        self.rest = rest;
        taken
    }
}

/// A field is a top-level key; `.` is part of the name, not a path. Bytes past ASCII let a key in
/// any script through, checked as UTF-8 after.
fn is_field_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.') || !b.is_ascii()
}

fn text(bytes: &[u8]) -> Result<String, Error> {
    str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| Error::BadValue)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{
        "cat": "tech", "ts": 1723248000, "score": -1.5, "ok": true, "nil": null, "sub": {"a": 1},
        "big": 9007199254740993
    }"#;

    fn parse(src: &str) -> Result<Filter, Error> {
        Filter::parse(src.as_bytes())
    }

    fn one(op: Op, value: Value) -> Result<Filter, Error> {
        let cond = Cond {
            field: "a".into(),
            op,
            value,
        };
        Ok(Filter {
            first: cond,
            rest: Vec::new(),
        })
    }

    fn on(src: &str, json: &str) -> bool {
        let doc = crate::bson::from_json(json.as_bytes()).unwrap();
        parse(src)
            .unwrap()
            .matches(RawDocument::from_bytes(&doc).unwrap())
    }

    #[test]
    fn each_operator_reads_with_or_without_spaces() {
        for (src, op) in [
            ("a=1", Op::Eq),
            ("a != 1", Op::Ne),
            ("a< 1", Op::Lt),
            ("a <=1", Op::Le),
            ("a > 1", Op::Gt),
            ("a>=1", Op::Ge),
        ] {
            assert_eq!(parse(src), one(op, Value::Int(1)), "{src}");
        }
    }

    #[test]
    fn a_value_takes_the_type_it_reads_as() {
        for (src, value) in [
            ("a = 5000000000", Value::Int(5_000_000_000)),
            ("a = -1.5", Value::Double(-1.5)),
            ("a = tech", Value::Str("tech".into())),
            ("a = 'a <= b'", Value::Str("a <= b".into())),
            (r#"a = "5""#, Value::Str("5".into())),
            ("a = true", Value::Bool(true)),
            ("a = 'true'", Value::Str("true".into())),
            ("a = null", Value::Null),
            ("a = nan", Value::Str("nan".into())),
        ] {
            assert_eq!(parse(src), one(Op::Eq, value), "{src}");
        }
    }

    #[test]
    fn a_malformed_filter_is_a_bad_value() {
        for src in [
            "",
            "   ",
            "a",
            "= 1",
            "a =",
            "a = 'x",
            "a = 1 b",
            "a = 1 and",
            "a = 1 or",
            "a ! 1",
        ] {
            assert_eq!(parse(src), Err(Error::BadValue), "{src:?}");
        }
        assert_eq!(Filter::parse(b"a = \xff"), Err(Error::BadValue));
    }

    #[test]
    fn and_and_or_take_no_precedence() {
        // (a=1 or b=2) and c=3
        let src = "a=1 or b=2 AND c=3";
        assert!(!on(src, r#"{"a": 1}"#));
        assert!(on(src, r#"{"a": 1, "c": 3}"#));
        assert!(on(src, r#"{"b": 2, "c": 3}"#));
        // (a=1 and b=2) or c=3
        let src = "a=1 and b=2 or c=3";
        assert!(on(src, r#"{"c": 3}"#));
        assert!(!on(src, r#"{"a": 1}"#));
    }

    #[test]
    fn a_field_is_any_whole_word_key() {
        assert!(on("android = 1", r#"{"android": 1}"#));
        assert!(on("a.b-c_d = 1", r#"{"a.b-c_d": 1}"#));
        assert!(on("가격 >= 100", r#"{"가격": 100}"#));
    }

    #[test]
    fn strings_and_numbers_compare_in_order() {
        assert!(on("cat = tech and cat < tg", DOC));
        assert!(!on("cat != tech", DOC));
        assert!(on("ts >= 1723248000 and ts < 1723248001", DOC));
        assert!(on("score <= -1.5 and score > -2", DOC));
        assert!(on("ts > 1.5", DOC));
    }

    #[test]
    fn integers_past_doubles_compare_exactly() {
        assert!(on("big = 9007199254740993", DOC));
        assert!(!on("big = 9007199254740992", DOC));
    }

    #[test]
    fn booleans_and_null_only_compare_for_equality() {
        assert!(on("ok = true and ok != false", DOC));
        assert!(!on("ok > false", DOC));
        assert!(on("nil = null", DOC));
        assert!(!on("nil >= null", DOC));
    }

    #[test]
    fn a_missing_field_fails_even_for_not_equal() {
        assert!(!on("nope = 1", DOC));
        assert!(!on("nope != 1", DOC));
        assert!(on("nope = 1 or cat = tech", DOC));
    }

    #[test]
    fn another_type_is_only_unequal() {
        assert!(!on("cat = 5", DOC));
        assert!(on("cat != 5", DOC));
        assert!(!on("ts = '1723248000'", DOC));
        assert!(on("sub != 1", DOC));
        assert!(!on("sub < 1", DOC));
    }
}
