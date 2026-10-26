#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the commands that fail with the rest are not in yet"
    )
)]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    BadCommandLine,
    BadDataChunk,
    TooLargeValue,
    BadValue,
    NotFound,
    NotFoundElement,
    Exists,
    TypeMismatch,
    Overflowed,
    OutOfMemory,
    Internal,
    NotSupported,
}

impl Error {
    /// The reply line, worded as arcus-memcached words the same failure.
    pub const fn reply(self) -> &'static [u8] {
        match self {
            Self::BadCommandLine => b"CLIENT_ERROR bad command line format\r\n",
            Self::BadDataChunk => b"CLIENT_ERROR bad data chunk\r\n",
            Self::TooLargeValue => b"CLIENT_ERROR too large value\r\n",
            Self::BadValue => b"CLIENT_ERROR bad value\r\n",
            Self::NotFound => b"NOT_FOUND\r\n",
            Self::NotFoundElement => b"NOT_FOUND_ELEMENT\r\n",
            Self::Exists => b"EXISTS\r\n",
            Self::TypeMismatch => b"TYPE_MISMATCH\r\n",
            Self::Overflowed => b"OVERFLOWED\r\n",
            Self::OutOfMemory => b"SERVER_ERROR out of memory\r\n",
            Self::Internal => b"SERVER_ERROR internal\r\n",
            Self::NotSupported => b"NOT_SUPPORTED\r\n",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_arcus_lines() {
        for (error, line) in [
            (
                Error::BadCommandLine,
                "CLIENT_ERROR bad command line format",
            ),
            (Error::BadDataChunk, "CLIENT_ERROR bad data chunk"),
            (Error::TooLargeValue, "CLIENT_ERROR too large value"),
            (Error::BadValue, "CLIENT_ERROR bad value"),
            (Error::NotFound, "NOT_FOUND"),
            (Error::NotFoundElement, "NOT_FOUND_ELEMENT"),
            (Error::Exists, "EXISTS"),
            (Error::TypeMismatch, "TYPE_MISMATCH"),
            (Error::Overflowed, "OVERFLOWED"),
            (Error::OutOfMemory, "SERVER_ERROR out of memory"),
            (Error::Internal, "SERVER_ERROR internal"),
            (Error::NotSupported, "NOT_SUPPORTED"),
        ] {
            assert_eq!(error.reply(), format!("{line}\r\n").as_bytes());
        }
    }
}
