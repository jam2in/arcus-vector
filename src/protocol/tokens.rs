use std::os::raw::c_int;

use crate::error::Error;
use crate::memcached::ffi::token_t;

#[derive(Clone, Copy)]
pub struct Tokens<'a> {
    tokens: &'a [token_t],
}

impl<'a> Tokens<'a> {
    /// # Safety
    ///
    /// If `argc > 0`, `argv` must point to `argc` tokens with `length` readable bytes, for `'a`.
    pub unsafe fn new(argv: *const token_t, argc: c_int) -> Self {
        let len = usize::try_from(argc).unwrap_or(0);
        if len == 0 || argv.is_null() {
            return Self { tokens: &[] };
        }
        // SAFETY: `len > 0` and `argv` is non-null, so it points to `len` tokens valid for `'a`.
        let tokens = unsafe { std::slice::from_raw_parts(argv, len) };
        Self { tokens }
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn text(&self, i: usize) -> Result<&'a str, Error> {
        let token = self.tokens.get(i).ok_or(Error::BadCommandLine)?;
        if token.value.is_null() || token.length == 0 {
            return Ok("");
        }
        // SAFETY: `value` is non-null, so it points to `length` bytes valid for `'a`.
        let bytes = unsafe { std::slice::from_raw_parts(token.value.cast::<u8>(), token.length) };
        std::str::from_utf8(bytes).map_err(|_| Error::BadCommandLine)
    }
}

/// The returned buffer owns the bytes the tokens point into, so it must outlive them.
#[cfg(test)]
pub fn tokenize(line: &str) -> (Vec<u8>, Vec<token_t>) {
    let mut buf = line.as_bytes().to_vec();
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, byte) in buf.iter_mut().enumerate() {
        if *byte == b' ' {
            if start != i {
                spans.push((start, i - start));
            }
            *byte = 0;
            start = i + 1;
        }
    }
    if start != buf.len() {
        spans.push((start, buf.len() - start));
    }
    let base = buf.as_mut_ptr();
    let tokens = spans
        .into_iter()
        .map(|(off, len)| token_t {
            // SAFETY: every span starts inside `buf`, so `base.add(off)` stays in bounds.
            value: unsafe { base.add(off) }.cast(),
            length: len,
        })
        .collect();
    (buf, tokens)
}
