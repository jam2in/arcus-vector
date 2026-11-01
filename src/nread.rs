use std::alloc::{Layout, alloc};
use std::os::raw::{c_char, c_int};
use std::ptr;

use crate::error::Error;
use crate::memcached::server::Connection;
use crate::protocol::{self, MAX_BODY_BYTES, Request, Tokens};

const MAX_TOKENS: usize = 16;

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
    line: Vec<u8>,
    body: Vec<u8>,
}

impl Pending {
    /// Parses the kept line again and checks each body segment ends in CRLF.
    pub fn request(&self) -> Result<Request<'_>, Error> {
        let (raw, n) = protocol::split::<MAX_TOKENS>(&self.line).ok_or(Error::BadCommandLine)?;
        // SAFETY: the tokens point into `self.line`, which outlives the borrow of `self`.
        let tokens = unsafe { Tokens::new(raw.as_ptr(), c_int::try_from(n).unwrap_or(0)) };
        let request = protocol::parse(&tokens)?;
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

    /// The vector `request` declared, without its CRLF; empty if it declared none.
    pub fn vector(&self, request: &Request<'_>) -> &[u8] {
        let [first, vector] = request.segments();
        let start = first.map_or(0, |n| n + 2);
        vector
            .and_then(|n| self.body.get(start..start + n))
            .unwrap_or_default()
    }
}

/// Keeps the line on `conn` with a `len`-byte body buffer, which it returns; `None` without memory.
///
/// # Safety
///
/// `conn` must keep nothing else.
pub unsafe fn expect(conn: Connection, tokens: &Tokens<'_>, len: usize) -> Option<*mut c_char> {
    let words = (0..tokens.len()).map(|i| tokens.text(i).ok());
    let mut line = Vec::new();
    line.try_reserve_exact(words.clone().map(|w| w.map_or(0, str::len) + 1).sum())
        .ok()?;
    for word in words {
        line.extend_from_slice(word?.as_bytes());
        line.push(b' ');
    }
    let mut body = Vec::new();
    body.try_reserve_exact(len).ok()?;
    body.resize(len, 0);
    let pending = try_box(Pending { line, body })?;
    // SAFETY: `pending` is a live box; its body buffer stays put while the box is kept.
    let buffer = unsafe { (*pending).body.as_mut_ptr() }.cast();
    conn.keep(pending.cast());
    Some(buffer)
}

/// # Safety
///
/// `conn` must keep nothing but what [`expect`] kept.
pub unsafe fn take(conn: Connection) -> Option<Box<Pending>> {
    let raw = conn.kept().cast::<Pending>();
    if raw.is_null() {
        return None;
    }
    conn.keep(ptr::null_mut());
    // SAFETY: a non-null kept value is the box `expect` leaked, taken back once.
    Some(unsafe { Box::from_raw(raw) })
}

/// Like `Box::into_raw(Box::new(..))`, but a failed allocation is `None` instead of an abort.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "`alloc` aligns the memory for the layout it is given"
)]
fn try_box(pending: Pending) -> Option<*mut Pending> {
    // SAFETY: `Pending` is not zero-sized.
    let raw = unsafe { alloc(Layout::new::<Pending>()) }.cast::<Pending>();
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` is a fresh allocation with `Pending`'s layout.
    unsafe { raw.write(pending) };
    Some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tokenize;

    fn body_of(line: &str) -> Body {
        let (_buf, raw) = tokenize(line);
        // SAFETY: `raw` points into `_buf`, which outlives the call.
        let tokens = unsafe { Tokens::new(raw.as_ptr(), c_int::try_from(raw.len()).unwrap()) };
        body(&protocol::parse(&tokens).unwrap())
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
