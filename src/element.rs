//! A vector element: the vkey is its field, and its value is a BSON document of metadata, whose
//! first four bytes give its length, then the vector's bytes.

use bson::RawDocument;
use usearch::ScalarKind;

use crate::memcached::engine::Elem;
use crate::quant::Vector;

impl Elem {
    pub fn vkey(&self) -> Option<&[u8]> {
        self.field()
    }

    /// Read in place; a field that does not parse turns up as an error on access.
    pub fn meta(&self) -> Option<&RawDocument> {
        RawDocument::from_bytes(split(self.value()?)?.0).ok()
    }

    /// A copy, as the bytes may not be aligned for `kind`.
    pub fn vector(&self, kind: ScalarKind) -> Option<Vector> {
        Vector::from_bytes(split(self.value()?)?.1, kind)
    }

    pub fn same_vector(&self, other: &Self) -> Option<bool> {
        Some(split(self.value()?)?.1 == split(other.value()?)?.1)
    }
}

fn split(value: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = u32::from_le_bytes(*value.first_chunk()?);
    value.split_at_checked(usize::try_from(len).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_splits_after_its_document() {
        let doc = [5, 0, 0, 0, 0];
        let value = [&doc[..], &[1, 2, 3]].concat();
        assert_eq!(split(&value), Some((&doc[..], &[1, 2, 3][..])));
        assert_eq!(split(&[9, 0, 0, 0, 0]), None);
        assert_eq!(split(&[1, 0]), None);
    }
}
