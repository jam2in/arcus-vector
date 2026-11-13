#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the parsed fields are read by the commands built on this parser"
    )
)]

mod tokens;

use std::ops::RangeInclusive;
use std::str::FromStr;

use usearch::{MetricKind, ScalarKind};

use crate::bson;
use crate::error::Error;
use crate::memcached::ffi::MAX_FIELD_LENG;
use crate::quant::Vector;

pub use tokens::Tokens;
#[cfg(test)]
pub use tokens::tokenize;

pub const DEFAULT_M: usize = 16;
pub const M_RANGE: RangeInclusive<usize> = 2..=1024;
pub const DEFAULT_EFC: usize = 128;
pub const DEFAULT_EFS: usize = 64;
pub const EF_RANGE: RangeInclusive<usize> = 1..=65536;
pub const DEFAULT_MAXCOUNT: u32 = 50_000;
pub const MAX_MAXCOUNT: u32 = 1_000_000;
pub const MAX_RESULTS: usize = i32::MAX as usize;
pub const MAX_FILTER_BYTES: usize = 4096;
pub const MAX_BODY_BYTES: usize = 1 << 20;
/// arcus-memcached's `KEY_MAX_LENGTH`, which is in `memcached.h`, not the headers bound here.
pub const KEY_MAX_LENGTH: usize = 16000;
/// arcus-memcached's default `max_element_bytes`, set in `memcached.c`: the core refuses a longer
/// element value, CRLF included, before it reaches the engine.
pub const MAX_ELEMENT_BYTES: usize = 16 * 1024;

pub fn parse_metric(word: &str) -> Option<MetricKind> {
    match word {
        "cos" => Some(MetricKind::Cos),
        "l2" => Some(MetricKind::L2sq),
        "ip" => Some(MetricKind::IP),
        "hamming" => Some(MetricKind::Hamming),
        "tanimoto" => Some(MetricKind::Tanimoto),
        _ => None,
    }
}

pub const fn metric_word(metric: MetricKind) -> &'static str {
    match metric {
        MetricKind::L2sq => "l2",
        MetricKind::IP => "ip",
        MetricKind::Hamming => "hamming",
        MetricKind::Tanimoto => "tanimoto",
        _ => "cos",
    }
}

pub fn parse_quant(word: &str) -> Option<ScalarKind> {
    match word {
        "f32" => Some(ScalarKind::F32),
        "f16" => Some(ScalarKind::F16),
        "i8" => Some(ScalarKind::I8),
        "b1" => Some(ScalarKind::B1),
        _ => None,
    }
}

pub const fn quant_word(quant: ScalarKind) -> &'static str {
    match quant {
        ScalarKind::F16 => "f16",
        ScalarKind::I8 => "i8",
        ScalarKind::B1 => "b1",
        _ => "f32",
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Bin,
    Str,
}

impl Format {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "bin" => Some(Self::Bin),
            "str" => Some(Self::Str),
            _ => None,
        }
    }
}

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

fn in_range<T: PartialOrd>(n: T, range: &RangeInclusive<T>) -> Result<T, Error> {
    if range.contains(&n) {
        Ok(n)
    } else {
        Err(Error::BadCommandLine)
    }
}

/// Reads the words of a line in order.
struct Args<'w, 'a> {
    words: &'w [&'a str],
    at: usize,
}

impl<'a> Args<'_, 'a> {
    fn is_empty(&self) -> bool {
        self.at >= self.words.len()
    }

    fn peek(&self) -> Option<&'a str> {
        self.words.get(self.at).copied()
    }

    fn next(&mut self) -> Result<&'a str, Error> {
        let word = self.peek().ok_or(Error::BadCommandLine)?;
        self.at += 1;
        Ok(word)
    }

    /// An index names an item, so it is no longer than a key.
    fn index(&mut self) -> Result<&'a str, Error> {
        self.next_within(KEY_MAX_LENGTH)
    }

    /// A vkey names a field of the index's map.
    fn vkey(&mut self) -> Result<&'a str, Error> {
        self.next_within(MAX_FIELD_LENG as usize)
    }

    fn next_within(&mut self, max: usize) -> Result<&'a str, Error> {
        let text = self.next()?;
        if text.len() > max {
            return Err(Error::BadCommandLine);
        }
        Ok(text)
    }

    fn number<T: FromStr>(&mut self) -> Result<T, Error> {
        self.next()?.parse().map_err(|_| Error::BadCommandLine)
    }

    /// Reads the next word only if it is a number.
    fn optional_number<T: FromStr>(&mut self) -> Option<T> {
        let n = self.peek()?.parse().ok()?;
        self.at += 1;
        Some(n)
    }
}

pub fn parse<'a>(words: &[&'a str]) -> Result<Request<'a>, Error> {
    let mut args = Args { words, at: 1 };
    let request = match args.next()? {
        "create" => Request::Create(create(&mut args)?),
        "upsert" => Request::Upsert(upsert(&mut args)?),
        "setmeta" => Request::SetMeta(SetMeta {
            index: args.index()?,
            vkey: args.vkey()?,
            meta_bytes: args.number()?,
            meta: Vec::new(),
        }),
        "search" => Request::Search(search(&mut args)?),
        "getmeta" => Request::GetMeta {
            index: args.index()?,
            vkey: args.vkey()?,
        },
        "delete" => Request::Delete {
            index: args.index()?,
            vkey: args.vkey()?,
        },
        "drop" => Request::Drop {
            index: args.index()?,
        },
        _ => return Err(Error::BadCommandLine),
    };
    if !args.is_empty() {
        return Err(Error::BadCommandLine);
    }
    Ok(request)
}

fn create<'a>(args: &mut Args<'_, 'a>) -> Result<Create<'a>, Error> {
    let index = args.index()?;
    let dim = in_range(args.number()?, &(1..=u16::MAX))?;
    let exptime = args.number()?;
    let maxcount = match args.optional_number() {
        None | Some(0) => DEFAULT_MAXCOUNT,
        Some(n) => in_range(n, &(1..=MAX_MAXCOUNT))?,
    };

    let (mut metric, mut quant) = (MetricKind::Cos, ScalarKind::F32);
    let (mut m, mut efc, mut efs) = (DEFAULT_M, DEFAULT_EFC, DEFAULT_EFS);
    while !args.is_empty() {
        let key = args.next()?;
        let value = args.next()?;
        match key {
            "metric" => metric = parse_metric(value).ok_or(Error::BadCommandLine)?,
            "quant" => quant = parse_quant(value).ok_or(Error::BadCommandLine)?,
            "M" => m = value.parse().map_err(|_| Error::BadCommandLine)?,
            "efc" => efc = value.parse().map_err(|_| Error::BadCommandLine)?,
            "efs" => efs = value.parse().map_err(|_| Error::BadCommandLine)?,
            _ => return Err(Error::BadCommandLine),
        }
    }

    let bitwise = [MetricKind::Hamming, MetricKind::Tanimoto].contains(&metric);
    if bitwise != (quant == ScalarKind::B1) {
        return Err(Error::BadCommandLine);
    }
    Ok(Create {
        index,
        dim,
        exptime,
        maxcount,
        metric,
        quant,
        m: in_range(m, &M_RANGE)?,
        efc: in_range(efc, &EF_RANGE)?,
        efs: in_range(efs, &EF_RANGE)?,
    })
}

fn upsert<'a>(args: &mut Args<'_, 'a>) -> Result<Upsert<'a>, Error> {
    let index = args.index()?;
    let vkey = args.vkey()?;
    let vector_bytes = in_range(args.number()?, &(1..=usize::MAX))?;

    let (mut meta_bytes, mut format) = (None, None);
    while !args.is_empty() {
        let word = args.next()?;
        match word {
            "meta" if meta_bytes.is_none() => meta_bytes = Some(args.number()?),
            _ if format.is_none() => {
                format = Some(Format::parse(word).ok_or(Error::BadCommandLine)?);
            }
            _ => return Err(Error::BadCommandLine),
        }
    }
    Ok(Upsert {
        index,
        vkey,
        vector_bytes,
        vector: Vector::default(),
        meta_bytes,
        meta: bson::EMPTY.to_vec(),
        format: format.unwrap_or_default(),
    })
}

fn search<'a>(args: &mut Args<'_, 'a>) -> Result<Search<'a>, Error> {
    let index = args.index()?;
    let from = args.next()?;
    let (mut source, k) = match from {
        "vkey" => {
            let vkey = args.vkey()?;
            (Source::Vkey(vkey), args.number()?)
        }
        "vector" => {
            let bytes = in_range(args.number()?, &(1..=usize::MAX))?;
            let k = args.number()?;
            let format = Format::default();
            (Source::Vector { bytes, format }, k)
        }
        _ => return Err(Error::BadCommandLine),
    };

    let (mut filter_bytes, mut with_meta, mut format) = (None, false, None);
    while !args.is_empty() {
        let word = args.next()?;
        match word {
            "filter" if filter_bytes.is_none() => {
                let n = args.number()?;
                if n > MAX_FILTER_BYTES {
                    return Err(Error::TooLargeValue);
                }
                filter_bytes = Some(n);
            }
            "withmeta" if !with_meta => with_meta = true,
            _ if format.is_none() && matches!(source, Source::Vector { .. }) => {
                format = Some(Format::parse(word).ok_or(Error::BadCommandLine)?);
            }
            _ => return Err(Error::BadCommandLine),
        }
    }
    if let (Source::Vector { format: slot, .. }, Some(format)) = (&mut source, format) {
        *slot = format;
    }

    Ok(Search {
        index,
        source,
        vector: Vector::default(),
        k: in_range(k, &(1..=MAX_RESULTS))?,
        filter_bytes,
        with_meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &str) -> Result<String, Error> {
        parse(Tokens::from_line(line).words()).map(|request| describe(&request))
    }

    fn describe(request: &Request<'_>) -> String {
        match request {
            Request::Create(c) => format!(
                "create {} {} {} {} {:?} {:?} {} {} {}",
                c.index, c.dim, c.exptime, c.maxcount, c.metric, c.quant, c.m, c.efc, c.efs
            ),
            Request::Upsert(u) => format!(
                "upsert {} {} {} {:?} {:?}",
                u.index, u.vkey, u.vector_bytes, u.meta_bytes, u.format
            ),
            Request::SetMeta(s) => format!("setmeta {} {} {}", s.index, s.vkey, s.meta_bytes),
            Request::Search(s) => {
                let source = match &s.source {
                    Source::Vkey(vkey) => format!("vkey {vkey}"),
                    Source::Vector { bytes, format, .. } => format!("vector {bytes} {format:?}"),
                };
                format!(
                    "search {} {} {} {:?} {}",
                    s.index, source, s.k, s.filter_bytes, s.with_meta
                )
            }
            Request::GetMeta { index, vkey } => format!("getmeta {index} {vkey}"),
            Request::Delete { index, vkey } => format!("delete {index} {vkey}"),
            Request::Drop { index } => format!("drop {index}"),
        }
    }

    #[test]
    fn create_takes_defaults_and_options() {
        assert_eq!(
            parse_line("vop create idx 128 0").unwrap(),
            "create idx 128 0 50000 Cos F32 16 128 64"
        );
        assert_eq!(
            parse_line("vop create idx 2 60 100 metric l2 quant f16 M 32 efc 200 efs 10").unwrap(),
            "create idx 2 60 100 L2sq F16 32 200 10"
        );
        assert_eq!(
            parse_line("vop create idx 2 60 efs 10 M 32 quant f16 efc 200 metric l2").unwrap(),
            "create idx 2 60 50000 L2sq F16 32 200 10"
        );
        assert_eq!(
            parse_line("vop create idx 8 0 metric hamming quant b1").unwrap(),
            "create idx 8 0 50000 Hamming B1 16 128 64"
        );
        assert_eq!(
            parse_line("vop create idx 2 0 0").unwrap(),
            "create idx 2 0 50000 Cos F32 16 128 64"
        );
    }

    #[test]
    fn a_repeated_create_option_takes_the_last() {
        assert_eq!(
            parse_line("vop create idx 2 0 metric l2 metric cos efs 1 efs 2").unwrap(),
            "create idx 2 0 50000 Cos F32 16 128 2"
        );
    }

    #[test]
    fn create_rejects_bad_values() {
        for (line, error) in [
            ("vop create idx", Error::BadCommandLine),
            ("vop create idx 0 0", Error::BadCommandLine),
            ("vop create idx 65536 0", Error::BadCommandLine),
            ("vop create idx 2 -1", Error::BadCommandLine),
            ("vop create idx 2 0 1000001", Error::BadCommandLine),
            ("vop create idx 2 0 metric", Error::BadCommandLine),
            ("vop create idx 2 0 metric foo", Error::BadCommandLine),
            ("vop create idx 2 0 metric cosine", Error::BadCommandLine),
            ("vop create idx 2 0 metric l2sq", Error::BadCommandLine),
            ("vop create idx 2 0 metric euclidean", Error::BadCommandLine),
            ("vop create idx 2 0 metric dot", Error::BadCommandLine),
            ("vop create idx 2 0 metric jaccard", Error::BadCommandLine),
            ("vop create idx 2 0 size 3", Error::BadCommandLine),
            ("vop CREATE idx 2 0", Error::BadCommandLine),
            ("vop create idx 2 0 METRIC l2", Error::BadCommandLine),
            ("vop create idx 2 0 metric L2", Error::BadCommandLine),
            ("vop create idx 2 0 m 32", Error::BadCommandLine),
            ("vop create idx 2 0 M 1", Error::BadCommandLine),
            ("vop create idx 2 0 quant b1", Error::BadCommandLine),
        ] {
            assert_eq!(parse_line(line), Err(error), "{line}");
        }
    }

    #[test]
    fn a_too_long_index_or_vkey_is_refused() {
        let named = |index: &str, vkey: &str| {
            [
                format!("vop upsert {index} {vkey} 8"),
                format!("vop setmeta {index} {vkey} 5"),
                format!("vop getmeta {index} {vkey}"),
                format!("vop delete {index} {vkey}"),
                format!("vop search {index} vkey {vkey} 5"),
            ]
        };
        let indexed = |index: &str| {
            [
                format!("vop create {index} 2 0"),
                format!("vop drop {index}"),
                format!("vop search {index} vector 8 5"),
            ]
        };
        let (index, vkey) = (
            "i".repeat(KEY_MAX_LENGTH),
            "v".repeat(MAX_FIELD_LENG as usize),
        );
        let (long_index, long_vkey) = (format!("{index}i"), format!("{vkey}v"));
        for line in named(&index, &vkey).into_iter().chain(indexed(&index)) {
            assert!(parse_line(&line).is_ok(), "{line:.40}");
        }
        for line in named(&long_index, &vkey)
            .into_iter()
            .chain(named(&index, &long_vkey))
            .chain(indexed(&long_index))
        {
            assert_eq!(parse_line(&line), Err(Error::BadCommandLine), "{line:.40}");
        }
    }

    #[test]
    fn upsert_and_setmeta_read_their_lengths() {
        assert_eq!(
            parse_line("vop upsert idx k1 8 meta 5 str").unwrap(),
            "upsert idx k1 8 Some(5) Str"
        );
        assert_eq!(
            parse_line("vop upsert idx k1 8 str meta 5").unwrap(),
            "upsert idx k1 8 Some(5) Str"
        );
        assert_eq!(
            parse_line("vop upsert idx k1 8").unwrap(),
            "upsert idx k1 8 None Bin"
        );
        assert_eq!(
            parse_line("vop setmeta idx k1 5").unwrap(),
            "setmeta idx k1 5"
        );
        assert_eq!(
            parse_line("vop upsert idx k1 8 withmeta"),
            Err(Error::BadCommandLine)
        );
        assert_eq!(parse_line("vop setmeta idx k1"), Err(Error::BadCommandLine));
        for line in [
            "vop upsert idx k1 0",
            "vop upsert idx k1 8 meta",
            "vop upsert idx k1 8 meta 1 meta 2",
            "vop upsert idx k1 8 bin str",
            "vop upsert idx k1 8 str str",
        ] {
            assert_eq!(parse_line(line), Err(Error::BadCommandLine), "{line}");
        }
    }

    #[test]
    fn search_reads_either_source() {
        assert_eq!(
            parse_line("vop search idx vkey k1 10 filter 12 withmeta").unwrap(),
            "search idx vkey k1 10 Some(12) true"
        );
        assert_eq!(
            parse_line("vop search idx vector 8 5 str").unwrap(),
            "search idx vector 8 Str 5 None false"
        );
        assert_eq!(
            parse_line("vop search idx vkey k1 10 withmeta filter 12").unwrap(),
            "search idx vkey k1 10 Some(12) true"
        );
        assert_eq!(
            parse_line("vop search idx vector 8 5 str withmeta filter 3").unwrap(),
            "search idx vector 8 Str 5 Some(3) true"
        );
        assert_eq!(
            parse_line("vop search idx vector 8 5 filter 3 str withmeta").unwrap(),
            "search idx vector 8 Str 5 Some(3) true"
        );
        assert_eq!(
            parse_line("vop search idx vkey k1 0"),
            Err(Error::BadCommandLine)
        );
        assert_eq!(
            parse_line("vop search idx vector 8 5 filter 4097"),
            Err(Error::TooLargeValue)
        );
        for line in [
            "vop search idx key k1 5",
            "vop search idx vector 0 5",
            "vop search idx vkey k1 10 filter",
            "vop search idx vkey k1 10 str",
            "vop search idx vkey k1 10 withmeta withmeta",
            "vop search idx vkey k1 10 filter 1 filter 2",
            "vop search idx vector 8 5 bin str",
        ] {
            assert_eq!(parse_line(line), Err(Error::BadCommandLine), "{line}");
        }
    }

    #[test]
    fn keyed_commands_take_exact_arguments() {
        assert_eq!(parse_line("vop getmeta idx k1").unwrap(), "getmeta idx k1");
        assert_eq!(parse_line("vop delete idx k1").unwrap(), "delete idx k1");
        assert_eq!(parse_line("vop drop idx").unwrap(), "drop idx");
        assert_eq!(parse_line("vop drop idx extra"), Err(Error::BadCommandLine));
        assert_eq!(parse_line("vop"), Err(Error::BadCommandLine));
        assert_eq!(parse_line("vop list"), Err(Error::BadCommandLine));
    }
}
