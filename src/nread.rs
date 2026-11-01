use std::os::raw::c_char;

use crate::bson;
use crate::cmd;
use crate::error::Error;
use crate::index;
use crate::memcached::server::Connection;
use crate::protocol::{self, MAX_BODY_BYTES, MAX_ELEMENT_BYTES, Request, Tokens};

pub enum Body {
    None,
    Read(usize),
    /// Too long to read; the connection closes, as the core cannot skip it.
    Refuse,
}

/// What to do with the body `request` declares.
pub fn body(request: &Request<'_>) -> Body {
    let segments = request.segments();
    if segments == [None, None] {
        return Body::None;
    }
    let len = segments
        .into_iter()
        .flatten()
        .try_fold(0usize, |sum, n| sum.checked_add(n)?.checked_add(2));
    match len {
        Some(len) if len <= MAX_BODY_BYTES => Body::Read(len),
        _ => Body::Refuse,
    }
}

/// A line kept on its connection while the core reads its body.
pub struct Pending {
    line: String,
    body: Vec<u8>,
}

impl Pending {
    /// Parses the kept line again, checks each body segment ends in CRLF, and fills in the vector
    /// it carries, read as its index's.
    pub fn request(&self) -> Result<Request<'_>, Error> {
        let mut request = protocol::parse(Tokens::from_line(&self.line).words())?;
        let [vector, extra] = request.segments();
        let mut rest = self.body.as_slice();
        let mut cut = |n: Option<usize>| -> Result<&[u8], Error> {
            let Some(n) = n else { return Ok(&[]) };
            let (segment, tail) = rest.split_at_checked(n + 2).ok_or(Error::BadDataChunk)?;
            rest = tail;
            segment.strip_suffix(b"\r\n").ok_or(Error::BadDataChunk)
        };
        let bytes = cut(vector)?;
        let extra = cut(extra)?;
        match &mut request {
            Request::Upsert(u) if u.meta_bytes.is_some() => u.meta = bson::from_json(extra)?,
            Request::SetMeta(s) => s.meta = bson::from_json(extra)?,
            _ => {}
        }
        if let Some(format) = request.vector_format() {
            request.set_vector(cmd::read_vector(request.index(), bytes, format)?);
        }
        if let Some(meta) = request.meta() {
            check_element(request.index(), meta)?;
        }
        Ok(request)
    }
}

/// Refuses an element of `index` with `meta` that the core would refuse as too long.
fn check_element(index: &str, meta: &[u8]) -> Result<(), Error> {
    let dim = index::dimensions(index).ok_or(Error::NotFound)?;
    if meta.len() + dim * size_of::<f32>() + 2 > MAX_ELEMENT_BYTES {
        return Err(Error::TooLargeValue);
    }
    Ok(())
}

/// Keeps the line on `conn` with a `len`-byte body buffer, which it returns; `None` without memory,
/// or if `conn` already keeps something, which must not be lost.
pub fn expect(conn: Connection, words: &[&str], len: usize) -> Option<*mut c_char> {
    let mut line = String::new();
    line.try_reserve_exact(words.iter().map(|w| w.len() + 1).sum())
        .ok()?;
    for word in words {
        line.push_str(word);
        line.push(' ');
    }
    let mut body = Vec::new();
    body.try_reserve_exact(len).ok()?;
    body.resize(len, 0);
    // The buffer stays put as `body` moves onto `conn`, until it is taken back and dropped.
    let buffer = body.as_mut_ptr().cast();
    conn.keep(Pending { line, body }).ok()?;
    Some(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_of(line: &str) -> Body {
        body(&protocol::parse(Tokens::from_line(line).words()).unwrap())
    }

    #[test]
    fn an_element_past_the_core_limit_is_too_large() {
        let words = Tokens::from_line("vop create toolarge 4 0");
        let Ok(Request::Create(spec)) = protocol::parse(words.words()) else {
            panic!("a create line");
        };
        assert_eq!(index::create(&spec), Ok(()));
        let fits = vec![0; MAX_ELEMENT_BYTES - 2 - 4 * 4];
        assert_eq!(check_element("toolarge", &fits), Ok(()));
        let over = [fits, vec![0]].concat();
        assert_eq!(check_element("toolarge", &over), Err(Error::TooLargeValue));
        assert_eq!(check_element("missing", &[]), Err(Error::NotFound));
    }

    #[test]
    fn each_segment_counts_with_its_crlf() {
        assert!(matches!(body_of("vop setmeta idx k1 5"), Body::Read(7)));
        assert!(matches!(
            body_of("vop upsert idx k1 8 meta 3"),
            Body::Read(15)
        ));
        assert!(matches!(
            body_of("vop search idx vector 8 5 filter 4"),
            Body::Read(16)
        ));
        assert!(matches!(body_of("vop search idx vkey k1 5"), Body::None));
        assert!(matches!(body_of("vop drop idx"), Body::None));
    }

    #[test]
    fn a_body_over_the_limit_is_refused() {
        let line = format!("vop setmeta idx k1 {}", MAX_BODY_BYTES - 1);
        assert!(matches!(body_of(&line), Body::Refuse));
        let line = format!("vop setmeta idx k1 {}", MAX_BODY_BYTES - 2);
        assert!(matches!(body_of(&line), Body::Read(MAX_BODY_BYTES)));
    }
}
