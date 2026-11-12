use std::os::raw::c_int;

use crate::error::Error;
use crate::memcached::ffi::token_t;

/// The words of a command line, in place.
pub struct Tokens<'a> {
    words: Vec<&'a str>,
}

impl<'a> Tokens<'a> {
    /// The tokens the core split a line into. A bad command line if one is not UTF-8.
    ///
    /// # Safety
    ///
    /// If `argc > 0`, `argv` must point to `argc` tokens with `length` readable bytes, for `'a`.
    pub unsafe fn new(argv: *const token_t, argc: c_int) -> Result<Self, Error> {
        let len = usize::try_from(argc).unwrap_or(0);
        if len == 0 || argv.is_null() {
            return Ok(Self { words: Vec::new() });
        }
        // SAFETY: `len > 0` and `argv` is non-null, so it points to `len` tokens valid for `'a`.
        let tokens = unsafe { std::slice::from_raw_parts(argv, len) };
        let words = tokens
            .iter()
            .map(|token| {
                if token.value.is_null() || token.length == 0 {
                    return Ok("");
                }
                // SAFETY: the caller guarantees a non-null `value` has `length` bytes for `'a`.
                let bytes =
                    unsafe { std::slice::from_raw_parts(token.value.cast::<u8>(), token.length) };
                str::from_utf8(bytes).map_err(|_| Error::BadCommandLine)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { words })
    }

    /// Splits `line` on spaces.
    pub fn from_line(line: &'a str) -> Self {
        let words = line.split(' ').filter(|w| !w.is_empty()).collect();
        Self { words }
    }

    pub fn words(&self) -> &[&'a str] {
        &self.words
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_that_is_not_utf8_makes_a_bad_line() {
        let token = |bytes: &[u8]| token_t {
            value: bytes.as_ptr().cast_mut().cast(),
            length: bytes.len(),
        };
        let raw = [token(b"vop"), token(b"drop"), token(&[0xff])];
        // SAFETY: `raw` points into byte literals, which live for the whole program.
        let tokens = unsafe { Tokens::new(raw.as_ptr(), raw.len() as c_int) };
        assert!(matches!(tokens, Err(Error::BadCommandLine)));
    }

    #[test]
    fn a_line_splits_on_any_run_of_spaces() {
        assert_eq!(
            Tokens::from_line(" vop  drop idx ").words(),
            ["vop", "drop", "idx"]
        );
    }
}
