use numkong::{Dot, EachScale, cast, f16};
use usearch::ScalarKind;

use crate::error::Error;

/// A vector in its index's scalar kind: what usearch takes and the engine stores, so neither
/// converts it again.
#[derive(Clone, Debug, PartialEq)]
pub enum Vector {
    F32(Vec<f32>),
    F16(Vec<f16>),
    /// Scaled to unit length, then by 127, as usearch does: only cosine distance keeps its meaning.
    I8(Vec<i8>),
    /// A bit per dimension, set where the value is positive; the first dimension is the high bit.
    B1(Vec<u8>),
}

impl Default for Vector {
    fn default() -> Self {
        Self::F32(Vec::new())
    }
}

impl Vector {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            Self::F16(v) => v.iter().flat_map(|x| x.0.to_le_bytes()).collect(),
            Self::I8(v) => v.iter().map(|&x| x as u8).collect(),
            Self::B1(v) => v.clone(),
        }
    }

    pub fn from_bytes(bytes: &[u8], kind: ScalarKind) -> Option<Self> {
        Some(match kind {
            ScalarKind::F32 => {
                let (words, rest) = bytes.as_chunks();
                rest.is_empty()
                    .then(|| Self::F32(words.iter().map(|&w| f32::from_le_bytes(w)).collect()))?
            }
            ScalarKind::F16 => {
                let (halves, rest) = bytes.as_chunks();
                rest.is_empty().then(|| {
                    Self::F16(halves.iter().map(|&h| f16(u16::from_le_bytes(h))).collect())
                })?
            }
            ScalarKind::I8 => Self::I8(bytes.iter().map(|&b| b as i8).collect()),
            ScalarKind::B1 => Self::B1(bytes.to_vec()),
            _ => return None,
        })
    }
}

/// `vector` as `kind`, which must be one an index can be created with.
pub fn quantize(vector: Vec<f32>, kind: ScalarKind) -> Result<Vector, Error> {
    match kind {
        ScalarKind::F32 => Ok(Vector::F32(vector)),
        ScalarKind::F16 => to_f16(&vector).map(Vector::F16),
        ScalarKind::I8 => to_i8(&vector).map(Vector::I8),
        ScalarKind::B1 => Ok(Vector::B1(to_b1(&vector))),
        _ => Err(Error::Internal),
    }
}

/// The bytes a vector of `dim` dimensions takes as `kind`.
pub const fn byte_len(dim: usize, kind: ScalarKind) -> usize {
    match kind {
        ScalarKind::F16 => dim * 2,
        ScalarKind::I8 => dim,
        ScalarKind::B1 => dim.div_ceil(8),
        _ => dim * 4,
    }
}

fn to_f16(vector: &[f32]) -> Result<Vec<f16>, Error> {
    let mut out = vec![f16::default(); vector.len()];
    cast(vector, &mut out).ok_or(Error::Internal)?;
    Ok(out)
}

/// Rounds to nearest even and saturates in `cast`; a zero or NaN length gives all zeros, as usearch
/// does.
fn to_i8(vector: &[f32]) -> Result<Vec<i8>, Error> {
    let mut out = vec![0; vector.len()];
    let norm = f32::dot(vector, vector).ok_or(Error::Internal)?.sqrt();
    if norm.is_nan() || norm <= 0.0 {
        return Ok(out);
    }
    let mut scaled = vec![0.0; vector.len()];
    let scale = (127.0 / norm) as f32;
    f32::each_scale(vector, scale, 0.0, &mut scaled).ok_or(Error::Internal)?;
    cast(&scaled, &mut out).ok_or(Error::Internal)?;
    Ok(out)
}

fn to_b1(vector: &[f32]) -> Vec<u8> {
    vector
        .chunks(8)
        .map(|bits| {
            bits.iter()
                .enumerate()
                .filter(|&(_, &x)| x > 0.0)
                .fold(0, |byte, (i, _)| byte | (0x80 >> i))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_takes_its_bytes() {
        let kinds = [
            ScalarKind::F32,
            ScalarKind::F16,
            ScalarKind::I8,
            ScalarKind::B1,
        ];
        assert_eq!(kinds.map(|kind| byte_len(9, kind)), [36, 18, 9, 2]);
    }

    #[test]
    fn f32_is_kept_as_is() {
        assert_eq!(
            quantize(vec![1.0, -2.5], ScalarKind::F32),
            Ok(Vector::F32(vec![1.0, -2.5]))
        );
    }

    #[test]
    fn f16_rounds_to_half_precision() {
        let Ok(Vector::F16(half)) = quantize(vec![1.0, -2.5, 0.1], ScalarKind::F16) else {
            panic!("not f16");
        };
        let back: Vec<f32> = half.iter().map(|h| h.to_f32()).collect();
        assert_eq!(back[..2], [1.0, -2.5]);
        assert!((back[2] - 0.1).abs() < 1e-3);
    }

    #[test]
    fn i8_scales_to_unit_length_and_rounds() {
        // Length 5: 3/5 * 127 = 76.2 and 4/5 * 127 = 101.6.
        assert_eq!(
            quantize(vec![3.0, -4.0], ScalarKind::I8),
            Ok(Vector::I8(vec![76, -102]))
        );
        // One axis takes all of the length.
        assert_eq!(
            quantize(vec![0.0, 9.0, 0.0], ScalarKind::I8),
            Ok(Vector::I8(vec![0, 127, 0]))
        );
    }

    #[test]
    fn i8_of_no_length_is_zero() {
        assert_eq!(
            quantize(vec![0.0, 0.0], ScalarKind::I8),
            Ok(Vector::I8(vec![0, 0]))
        );
        assert_eq!(
            quantize(vec![f32::NAN, 1.0], ScalarKind::I8),
            Ok(Vector::I8(vec![0, 0]))
        );
    }

    #[test]
    fn b1_sets_a_bit_per_positive_value_from_the_high_bit() {
        let v = [1.0, 0.0, -1.0, 2.0, 0.5, 0.0, 0.0, 3.0, 1.0];
        assert_eq!(
            quantize(v.to_vec(), ScalarKind::B1),
            Ok(Vector::B1(vec![0b1001_1001, 0b1000_0000]))
        );
    }

    #[test]
    fn bytes_come_back_as_the_same_vector() {
        let values = vec![1.0, -2.5, 0.25, 7.0, -1.0, 0.0, 3.0, 2.0, -0.5];
        for kind in [
            ScalarKind::F32,
            ScalarKind::F16,
            ScalarKind::I8,
            ScalarKind::B1,
        ] {
            let vector = quantize(values.clone(), kind).unwrap();
            assert_eq!(Vector::from_bytes(&vector.to_bytes(), kind), Some(vector));
        }
        assert_eq!(Vector::from_bytes(&[0; 3], ScalarKind::F32), None);
    }
}
