use usearch::{MetricKind, ScalarKind};

use crate::data::filter::Filter;
use crate::data::vector::Vector;

/// `vop create <index> <dim> <expiretime> [<maxcount>] [metric m] [quant q] [M n] [efc c] [efs s]`
#[derive(Debug)]
pub struct Create<'a> {
    pub index: &'a str,
    pub dim: u16,
    pub exptime: u32,
    /// `DEFAULT_MAXCOUNT` when omitted or 0. The engine caps any count at `max_map_size`.
    pub maxcount: u32,
    pub metric: MetricKind,
    pub quant: ScalarKind,
    pub m: usize,
    pub efc: usize,
    pub efs: usize,
}

/// `vop upsert <index> <vkey> <vector_bytes> [meta <metabytes>] [bin|str]`
#[derive(Debug)]
pub struct Upsert<'a> {
    pub index: &'a str,
    pub vkey: &'a str,
    pub vector_bytes: usize,
    /// The vector the body carries, in its index's scalar kind; empty until the body is read.
    pub vector: Vector,
    pub meta_bytes: Option<usize>,
    /// The metadata the body carries, as BSON; an empty document if it carries none.
    pub meta: Vec<u8>,
    pub format: Format,
}

/// `vop setmeta <index> <vkey> <metabytes>`
#[derive(Debug)]
pub struct SetMeta<'a> {
    pub index: &'a str,
    pub vkey: &'a str,
    pub meta_bytes: usize,
    /// The metadata the body carries, as BSON; empty until the body is read.
    pub meta: Vec<u8>,
}

#[derive(Debug)]
pub enum Source<'a> {
    Vkey(&'a str),
    Vector { bytes: usize, format: Format },
}

/// `vop search <index> vkey <vkey> <num> [filter <f>] [withmeta]`, or
/// `vop search <index> vector <vector_bytes> <num> [filter <f>] [withmeta] [bin|str]`
#[derive(Debug)]
pub struct Search<'a> {
    pub index: &'a str,
    pub source: Source<'a>,
    /// The vector to search by, in its index's scalar kind: from the body for a vector source,
    /// empty until it is read.
    pub vector: Vector,
    pub k: usize,
    pub filter_bytes: Option<usize>,
    /// Parsed from the body, unset until it is read.
    pub filter: Option<Filter>,
    pub with_meta: bool,
}

#[derive(Debug)]
pub enum Request<'a> {
    Create(Create<'a>),
    Upsert(Upsert<'a>),
    SetMeta(SetMeta<'a>),
    Search(Search<'a>),
    /// `vop getmeta <index> <vkey>`
    GetMeta {
        index: &'a str,
        vkey: &'a str,
    },
    /// `vop delete <index> <vkey>`
    Delete {
        index: &'a str,
        vkey: &'a str,
    },
    /// `vop drop <index>`
    Drop {
        index: &'a str,
    },
}

impl<'a> Request<'a> {
    pub const fn index(&self) -> &'a str {
        match self {
            Self::Create(c) => c.index,
            Self::Upsert(u) => u.index,
            Self::SetMeta(s) => s.index,
            Self::Search(s) => s.index,
            Self::GetMeta { index, .. } | Self::Delete { index, .. } | Self::Drop { index } => {
                index
            }
        }
    }

    /// The format of the vector the body carries, if it carries one.
    pub const fn vector_format(&self) -> Option<Format> {
        match self {
            Self::Upsert(u) => Some(u.format),
            Self::Search(Search {
                source: Source::Vector { format, .. },
                ..
            }) => Some(*format),
            _ => None,
        }
    }

    /// The metadata of the element the request writes.
    pub const fn meta(&self) -> Option<&[u8]> {
        match self {
            Self::Upsert(u) => Some(u.meta.as_slice()),
            Self::SetMeta(s) => Some(s.meta.as_slice()),
            _ => None,
        }
    }

    /// Fills in the vector read from the body, for a request that carries one.
    pub fn set_vector(&mut self, read: Vector) {
        match self {
            Self::Upsert(u) => u.vector = read,
            Self::Search(s) => s.vector = read,
            _ => {}
        }
    }

    /// The lengths of the body segments the line declares, in the order they arrive: the vector,
    /// if any, then the metadata or filter.
    pub const fn segments(&self) -> [Option<usize>; 2] {
        match self {
            Self::Upsert(u) => [Some(u.vector_bytes), u.meta_bytes],
            Self::SetMeta(s) => [None, Some(s.meta_bytes)],
            Self::Search(s) => match s.source {
                Source::Vector { bytes, .. } => [Some(bytes), s.filter_bytes],
                Source::Vkey(_) => [None, s.filter_bytes],
            },
            _ => [None, None],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Bin,
    Str,
}

impl Format {
    pub(super) fn parse(s: &str) -> Option<Self> {
        match s {
            "bin" => Some(Self::Bin),
            "str" => Some(Self::Str),
            _ => None,
        }
    }
}
