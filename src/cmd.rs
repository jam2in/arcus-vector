use crate::aio::{self, Op, Waiter};
use crate::error::Error;
use crate::index;
use crate::memcached::server::Connection;
use crate::protocol::{Format, Request};
use crate::quant::{self, Vector};

/// Runs `request` on `conn`, replying now or once a lane is done.
pub fn run(conn: Connection, request: Result<Request<'_>, Error>) -> bool {
    let reply = match request {
        Ok(Request::Create(_)) => Error::NotSupported.reply(),
        Ok(request) if index::dimensions(request.index()).is_none() => Error::NotFound.reply(),
        Ok(request) => match Op::new(request) {
            Ok(op) => {
                aio::begin(op, Some(Waiter::new(conn)));
                return true;
            }
            Err(e) => e.reply(),
        },
        Err(e) => e.reply(),
    };
    conn.respond(reply)
}

/// `bytes` as a vector of `index`, as many finite `f32`s as it has dimensions, in its scalar kind:
/// `bin` is little-endian `f32`s, `str` decimals split by spaces. A missing index is not found, and
/// any other vector a bad value.
pub fn read_vector(index: &str, bytes: &[u8], format: Format) -> Result<Vector, Error> {
    let (dim, kind) = index::layout(index).ok_or(Error::NotFound)?;
    // NaN or infinity makes a distance NaN, which the graph cannot order.
    let finite = |x: f32| x.is_finite().then_some(x);
    let vector: Vec<f32> = match format {
        Format::Bin => {
            let (words, rest) = bytes.as_chunks();
            if !rest.is_empty() {
                return Err(Error::BadValue);
            }
            words
                .iter()
                .map(|&w| finite(f32::from_le_bytes(w)))
                .collect::<Option<_>>()
        }
        Format::Str => bytes
            .split(|&b| b == b' ')
            .filter(|w| !w.is_empty())
            .map(|w| finite(str::from_utf8(w).ok()?.parse().ok()?))
            .collect::<Option<_>>(),
    }
    .ok_or(Error::BadValue)?;
    if vector.len() == dim {
        quant::quantize(vector, kind)
    } else {
        Err(Error::BadValue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_index_is_not_found() {
        assert_eq!(
            read_vector("nowhere", &[0; 16], Format::Bin),
            Err(Error::NotFound)
        );
    }

    #[test]
    fn a_vector_is_read_as_its_index_dimensions() {
        let created = index::create("four", 0, &index::test_config(4, "cos", "f32"));
        assert_eq!(created, Ok(()));
        let bin: Vec<u8> = [1.0f32, -2.5, 0.0, 3.25]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let four = Ok(Vector::F32(vec![1.0, -2.5, 0.0, 3.25]));
        assert_eq!(read_vector("four", &bin, Format::Bin), four);
        let bad = Err(Error::BadValue);
        assert_eq!(read_vector("four", &bin[..12], Format::Bin), bad);
        assert_eq!(read_vector("four", &bin[..15], Format::Bin), bad);
        assert_eq!(read_vector("four", &[0; 20], Format::Bin), bad);
        assert_eq!(read_vector("four", b"1 -2.5  0 3.25", Format::Str), four);
        assert_eq!(read_vector("four", b"1 2 3", Format::Str), bad);
        assert_eq!(read_vector("four", b"1 2 x 4", Format::Str), bad);
        assert_eq!(read_vector("four", b"1 2 nan 4", Format::Str), bad);
        assert_eq!(read_vector("four", b"1 2 inf 4", Format::Str), bad);
        assert_eq!(read_vector("four", b"1 2 1e39 4", Format::Str), bad);
        for x in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let bin: Vec<u8> = [1.0, x, 0.0, 3.25]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            assert_eq!(read_vector("four", &bin, Format::Bin), bad);
        }
    }
}
