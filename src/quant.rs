use numkong::{EachScale, cast, f16};
use usearch::ScalarKind;

use crate::error::Error;

/// A vector in its index's scalar kind: what usearch takes and the engine stores, so neither
/// converts it again.
#[derive(Clone, Debug, PartialEq)]
pub enum Vector {
    F32(Vec<f32>),
    F16(Vec<f16>),
    /// Turned by a fixed rotation, then scaled so its largest magnitude is 127: only cosine
    /// distance keeps its meaning.
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
pub fn quantize(mut vector: Vec<f32>, kind: ScalarKind) -> Result<Vector, Error> {
    match kind {
        ScalarKind::F32 => Ok(Vector::F32(vector)),
        ScalarKind::F16 => to_f16(&vector).map(Vector::F16),
        ScalarKind::I8 => {
            rotate(&mut vector);
            to_i8(&vector).map(Vector::I8)
        }
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

/// Rounds to nearest even in `cast`; no finite magnitude, or a NaN, gives all zeros.
fn to_i8(vector: &[f32]) -> Result<Vec<i8>, Error> {
    let mut out = vec![0; vector.len()];
    let max = vector
        .iter()
        .try_fold(0.0_f32, |max, x| (!x.is_nan()).then(|| max.max(x.abs())));
    let Some(max) = max.filter(|max| max.is_finite() && *max > 0.0) else {
        return Ok(out);
    };
    let mut scaled = vec![0.0; vector.len()];
    f32::each_scale(vector, 127.0 / max, 0.0, &mut scaled).ok_or(Error::Internal)?;
    cast(&scaled, &mut out).ok_or(Error::Internal)?;
    Ok(out)
}

/// Turns `vector` by a fixed rotation that spreads its length over all of its dimensions, so a few
/// large values do not leave the rest of an i8 only a few steps: rounds of random sign flips, each
/// followed by a Hadamard transform of the leading and the trailing power-of-two block.
fn rotate(vector: &mut [f32]) {
    const ROUNDS: u64 = 2;
    let dim = vector.len();
    if dim < 2 {
        return;
    }
    let block = 1 << dim.ilog2();
    for round in 0..ROUNDS {
        flip(vector, 2 * round);
        hadamard(&mut vector[..block]);
        if block != dim {
            flip(vector, 2 * round + 1);
            hadamard(&mut vector[dim - block..]);
        }
    }
}

fn flip(vector: &mut [f32], seed: u64) {
    for (i, x) in (0_u64..).zip(vector) {
        if splitmix((seed << 32) | i) >> 63 == 1 {
            *x = -*x;
        }
    }
}

/// SplitMix64's finalizer: fixed, as every version must turn a vector the same way.
const fn splitmix(x: u64) -> u64 {
    let x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The orthonormal Walsh-Hadamard transform of `block`, whose length is a power of two.
fn hadamard(block: &mut [f32]) {
    let mut half = 1;
    while half < block.len() {
        for pair in block.chunks_exact_mut(2 * half) {
            let (low, high) = pair.split_at_mut(half);
            for (a, b) in low.iter_mut().zip(high) {
                (*a, *b) = (*a + *b, *a - *b);
            }
        }
        half *= 2;
    }
    let scale = (block.len() as f32).sqrt().recip();
    block.iter_mut().for_each(|x| *x *= scale);
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
    fn i8_scales_the_largest_magnitude_to_127_and_rounds() {
        // 3/4 * 127 = 95.25.
        assert_eq!(to_i8(&[3.0, -4.0]), Ok(vec![95, -127]));
        assert_eq!(to_i8(&[0.0, 9.0, 0.0]), Ok(vec![0, 127, 0]));
    }

    #[test]
    fn i8_of_no_finite_magnitude_is_zero() {
        assert_eq!(to_i8(&[0.0, 0.0]), Ok(vec![0, 0]));
        assert_eq!(to_i8(&[f32::NAN, 1.0]), Ok(vec![0, 0]));
        assert_eq!(to_i8(&[f32::INFINITY, 1.0]), Ok(vec![0, 0]));
    }

    fn sample(dim: usize, seed: u64) -> Vec<f32> {
        (0..dim as u64)
            .map(|i| (splitmix((seed << 32) | i) >> 40) as f32 / (1 << 24) as f32 - 0.5)
            .collect()
    }

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn rotation_keeps_lengths_and_angles() {
        for dim in [1, 2, 3, 8, 100, 768, 1536] {
            let (a, b) = (sample(dim, 1), sample(dim, 2));
            let (mut ra, mut rb) = (a.clone(), b.clone());
            rotate(&mut ra);
            rotate(&mut rb);
            for (before, after) in [(dot(&a, &a), dot(&ra, &ra)), (dot(&a, &b), dot(&ra, &rb))] {
                assert!(
                    (before - after).abs() < 1e-4 * dim as f32,
                    "{dim}: {before} {after}"
                );
            }
        }
    }

    #[test]
    fn rotation_spreads_one_axis_over_all() {
        for dim in [768, 1024, 1536] {
            let mut axis = vec![0.0; dim];
            axis[dim - 1] = 1.0;
            rotate(&mut axis);
            let max = axis.iter().fold(0.0_f32, |max, x| max.max(x.abs()));
            // Spread evenly, each would be 1/√dim.
            assert!(max < 4.0 / (dim as f32).sqrt(), "{dim}: {max}");
        }
    }

    #[test]
    fn i8_turns_every_vector_the_same_way() {
        // Pinned: a master and its slave, or two versions, must quantize alike.
        assert_eq!(
            quantize(vec![1.0, 2.0, 3.0, 4.0, 5.0], ScalarKind::I8),
            Ok(Vector::I8(vec![71, -127, 5, -15, 36]))
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
