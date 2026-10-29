use std::os::raw::c_char;

use crate::error::Error;
use crate::memcached::server::Connection;
use crate::protocol::{self, MAX_BODY_BYTES, Request, Tokens};

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
    /// Parses the kept line again and checks each body segment ends in CRLF.
    pub fn request(&self) -> Result<Request<'_>, Error> {
        let request = protocol::parse(Tokens::from_line(&self.line).words())?;
        let mut rest = self.body.as_slice();
        for n in request.segments().into_iter().flatten() {
            let (segment, tail) = rest.split_at_checked(n + 2).ok_or(Error::BadDataChunk)?;
            if !segment.ends_with(b"\r\n") {
                return Err(Error::BadDataChunk);
            }
            rest = tail;
        }
        Ok(request)
    }
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
    fn each_segment_counts_with_its_crlf() {
        assert!(matches!(body_of("vop setmeta idx k1 5"), Body::Read(7)));
        assert!(matches!(
            body_of("vop upsert idx k1 8 meta 3"),
            Body::Read(15)
        ));
        assert!(matches!(
            body_of("vop search idx vector 5 8 filter 4"),
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
