//! Semantic section checks of the segment reader (SPEC §5.1–§5.3.6, ROADMAP 5.3): corruptions that keep every CRC
//! valid. Each case decodes one section's logical stream (or a raw section's payload), edits it, re-frames it
//! (`Compression::None`, same block size, raw CRC recomputed) and rebuilds the segment with a fresh layout and
//! footer, so only the rule under test is broken. The reader must report it as a corruption of that section, at
//! open or when the part is read, never `Ok` and never a panic.
//!
//! Invariants:
//! 1) A no-op edit (decode + re-frame every section) reads back exactly as the original (the helper is sound).
//! 2) Every rule listed in SPEC §5.3.1–§5.3.6 that the reader can check, broken on its own, is a
//!    `ReadError::Section { kind, .. }` (or `ReadError::Document` for a DocStore row) with `is_corruption()`.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use persia_engine::document::{Document, Value};
use persia_engine::schema::CollectionSchema;
use persia_engine::segment::{
    ReadError, SEGMENT_FIELD, SectionKind, SegmentCorruption, WriteOptions,
};
use persia_format::block::{BlockReader, BlockWriter, Compression};
use persia_format::checksum::crc32c;
use persia_format::{Corruption, Reader, Writer};
use roaring::RoaringBitmap;

use super::read::{open, read_back};
use super::{blob, build, id, schema};

// ---------------------------------------------------------------------------------------------------------------
// The segment editor.
// ---------------------------------------------------------------------------------------------------------------

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

#[derive(Debug, Clone)]
struct Sec {
    kind: SectionKind,
    field: u32,
    bytes: Vec<u8>,
}

/// A segment split into its footer (without CRC) and its sections, in table order.
#[derive(Debug, Clone)]
struct Seg {
    body: Vec<u8>,
    sections: Vec<Sec>,
}

impl Seg {
    fn parse(bytes: &[u8]) -> Self {
        let n = bytes.len();
        assert_eq!(&bytes[n - 4..], b"PSEG");
        let footer_len = le_u32(bytes, n - 8) as usize;
        let start = n - 8 - footer_len;
        let body = bytes[start..n - 12].to_vec();
        let sections = (0..le_u32(&body, 84) as usize)
            .map(|i| {
                let e = Self::entry(&body, i);
                let kind = SectionKind::from_code(le_u16(&body, e)).unwrap();
                let field = le_u32(&body, e + 4);
                let off = le_u64(&body, e + 8) as usize;
                let len = le_u64(&body, e + 16) as usize;
                Sec {
                    kind,
                    field,
                    bytes: bytes[off..off + len].to_vec(),
                }
            })
            .collect();
        Self { body, sections }
    }

    /// Offset of section-table entry `i` in the footer body (SPEC §5.1).
    fn entry(body: &[u8], i: usize) -> usize {
        88 + 16 * le_u32(body, 80) as usize + 24 * i
    }

    /// Lays the sections out again (8-aligned, zero padding) and writes a footer with their new offsets and
    /// lengths, its CRC and the locator (SPEC §5.1).
    fn to_bytes(&self) -> Vec<u8> {
        let pad = |out: &mut Vec<u8>| out.resize(out.len().next_multiple_of(8), 0);
        let mut body = self.body.clone();
        let mut out = Vec::new();
        for (i, s) in self.sections.iter().enumerate() {
            pad(&mut out);
            let e = Self::entry(&body, i);
            put_u64(&mut body, e + 8, out.len() as u64);
            put_u64(&mut body, e + 16, s.bytes.len() as u64);
            out.extend_from_slice(&s.bytes);
        }
        pad(&mut out);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32c(&body).to_le_bytes());
        out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
        out.extend_from_slice(b"PSEG");
        out
    }

    fn section(&self, kind: SectionKind, field: u32) -> &Sec {
        self.sections
            .iter()
            .find(|s| (s.kind, s.field) == (kind, field))
            .unwrap_or_else(|| panic!("no {kind:?} section for field {field}"))
    }

    fn section_mut(&mut self, kind: SectionKind, field: u32) -> &mut Sec {
        self.sections
            .iter_mut()
            .find(|s| (s.kind, s.field) == (kind, field))
            .unwrap_or_else(|| panic!("no {kind:?} section for field {field}"))
    }

    /// The decoded logical stream of a block-framed section (SPEC §5.2).
    fn stream(&self, kind: SectionKind, field: u32) -> Vec<u8> {
        let r = BlockReader::open(&self.section(kind, field).bytes, 0).unwrap();
        let mut out = Vec::new();
        r.read_range(0, r.len(), &mut out).unwrap();
        out
    }

    /// Hands the stream of `(kind, field)` to `f`, then re-frames it at the same block size, uncompressed. The
    /// edit must change the stream: a no-op edit would make a case pass or fail for the wrong reason.
    fn edit_stream(&mut self, kind: SectionKind, field: u32, f: impl FnOnce(&mut Vec<u8>)) {
        self.reframe(kind, field, |s| {
            let before = s.clone();
            f(s);
            assert_ne!(
                *s, before,
                "the edit of {kind:?} (field {field}) changed nothing"
            );
        });
    }

    fn reframe(&mut self, kind: SectionKind, field: u32, f: impl FnOnce(&mut Vec<u8>)) {
        let mut stream = self.stream(kind, field);
        let block_size = BlockReader::open(&self.section(kind, field).bytes, 0)
            .unwrap()
            .block_size();
        f(&mut stream);
        let mut w = BlockWriter::new(block_size, Compression::None).unwrap();
        w.write(&stream).unwrap();
        self.section_mut(kind, field).bytes = w.finish().unwrap();
    }

    /// The payload of a raw section (its CRC checked).
    fn raw(&self, kind: SectionKind, field: u32) -> Vec<u8> {
        let b = &self.section(kind, field).bytes;
        let (payload, crc) = b.split_at(b.len() - 4);
        assert_eq!(crc32c(payload), le_u32(crc, 0));
        payload.to_vec()
    }

    /// Hands a raw section's payload to `f`, then recomputes its CRC (SPEC §5.3 "Framing").
    fn edit_raw(&mut self, kind: SectionKind, field: u32, f: impl FnOnce(&mut Vec<u8>)) {
        let mut payload = self.raw(kind, field);
        let before = payload.clone();
        f(&mut payload);
        assert_ne!(
            payload, before,
            "the edit of {kind:?} (field {field}) changed nothing"
        );
        let crc = crc32c(&payload);
        payload.extend_from_slice(&crc.to_le_bytes());
        self.section_mut(kind, field).bytes = payload;
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Stream codecs used by the edits (layouts from SPEC, as in `decode.rs`).
// ---------------------------------------------------------------------------------------------------------------

/// A `TermInfo` record (SPEC §5.3.1): byte offsets of its fields.
mod ti {
    pub const LEN: usize = 56;
    pub const POSTINGS: usize = 0;
    pub const FREQS: usize = 8;
    pub const SKIP: usize = 16;
    pub const POSITIONS: usize = 24;
    pub const TTF: usize = 32;
    pub const DF: usize = 40;
    pub const MAX_TF: usize = 44;
    pub const MIN_NORM: usize = 48;
}

/// A `Skip` entry (SPEC §5.3.2): byte offsets of its fields.
mod sk {
    pub const LEN: usize = 40;
    pub const LAST: usize = 0;
    pub const MAX_TF: usize = 4;
    pub const POSTINGS: usize = 8;
    pub const FREQS: usize = 16;
    pub const POSITIONS: usize = 24;
}

/// The ordinal of `term` in a field's TermDict.
fn ordinal(seg: &Seg, field: u32, term: &str) -> usize {
    let dict = fst::Map::new(seg.raw(SectionKind::TermDict, field)).unwrap();
    dict.get(term).unwrap_or_else(|| panic!("no term {term}")) as usize
}

/// Stream position of a field in TermInfo record `term`.
fn ti_at(seg: &Seg, field: u32, term: &str, offset: usize) -> usize {
    ordinal(seg, field, term) * ti::LEN + offset
}

fn read_varints(bytes: &[u8]) -> Vec<u32> {
    let mut r = Reader::new(bytes);
    let mut out = Vec::new();
    while !r.is_empty() {
        out.push(r.read_varint_u32().unwrap());
    }
    out
}

fn write_varints(values: &[u32]) -> Vec<u8> {
    let mut w = Writer::new();
    for &v in values {
        w.write_varint_u32(v);
    }
    w.into_inner()
}

/// A FOR column (SPEC §5.3.5): `count u64`, the block ends, the blocks; decoded as its blocks' values.
fn read_for(r: &mut Reader<'_>) -> Vec<u64> {
    let count = r.read_u64().unwrap() as usize;
    let blocks = count.div_ceil(1024);
    let ends: Vec<usize> = (0..blocks)
        .map(|_| r.read_u64().unwrap() as usize)
        .collect();
    let data = r.read_bytes(ends.last().copied().unwrap_or(0)).unwrap();
    let mut values = Vec::new();
    let mut start = 0;
    for end in ends {
        let mut br = Reader::new(&data[start..end]);
        values.extend(br.read_for_block().unwrap().iter());
        start = end;
    }
    assert_eq!(values.len(), count);
    values
}

/// A FOR column whose blocks hold the given values (not necessarily 1024 each).
fn write_for_blocks(w: &mut Writer, blocks: &[&[u64]]) {
    let count: usize = blocks.iter().map(|b| b.len()).sum();
    w.write_u64(count as u64);
    let mut data = Writer::new();
    let mut ends = Vec::new();
    for b in blocks {
        data.write_for_block(b).unwrap();
        ends.push(data.len() as u64);
    }
    for e in ends {
        w.write_u64(e);
    }
    w.write_bytes(data.as_slice());
}

fn write_for(w: &mut Writer, values: &[u64]) {
    let blocks: Vec<&[u64]> = values.chunks(1024).collect();
    write_for_blocks(w, &blocks);
}

fn serialize(bitmap: &RoaringBitmap) -> Vec<u8> {
    let mut out = Vec::new();
    bitmap.serialize_into(&mut out).unwrap();
    out
}

/// A length-prefixed bitmap: its raw bytes, `None` for length 0.
fn read_bitmap(r: &mut Reader<'_>) -> Option<Vec<u8>> {
    let len = r.read_u64().unwrap() as usize;
    (len > 0).then(|| r.read_bytes(len).unwrap().to_vec())
}

fn write_bitmap(w: &mut Writer, bytes: Option<&[u8]>) {
    match bytes {
        None => w.write_u64(0),
        Some(b) => {
            w.write_u64(b.len() as u64);
            w.write_bytes(b);
        }
    }
}

/// A keyword column (SPEC §5.3.5), decoded.
#[derive(Debug, Clone)]
struct KeywordCol {
    values: Vec<Vec<u8>>,
    starts: Vec<u64>,
    ords: Vec<u64>,
}

impl KeywordCol {
    fn decode(stream: &[u8]) -> Self {
        let mut r = Reader::new(stream);
        assert_eq!(r.read_u8().unwrap(), 3);
        r.expect_zeros(7).unwrap();
        let (_starts_off, _ords_off) = (r.read_u64().unwrap(), r.read_u64().unwrap());
        let count = r.read_u64().unwrap() as usize;
        let ends: Vec<usize> = (0..count).map(|_| r.read_u64().unwrap() as usize).collect();
        let data = r.read_bytes(*ends.last().unwrap()).unwrap();
        let mut values = Vec::new();
        let mut start = 0;
        for end in ends {
            values.push(data[start..end].to_vec());
            start = end;
        }
        let starts = read_for(&mut r);
        let ords = read_for(&mut r);
        assert!(r.is_empty());
        Self {
            values,
            starts,
            ords,
        }
    }

    /// Encodes with correct `starts_off`/`ords_off` for whatever the fields now hold.
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.write_u8(3);
        w.write_zeros(7);
        w.write_u64(0);
        w.write_u64(0);
        w.write_u64(self.values.len() as u64);
        let mut end = 0;
        for v in &self.values {
            end += v.len() as u64;
            w.write_u64(end);
        }
        for v in &self.values {
            w.write_bytes(v);
        }
        let starts_off = w.len() as u64;
        write_for(&mut w, &self.starts);
        let ords_off = w.len() as u64;
        write_for(&mut w, &self.ords);
        let mut out = w.into_inner();
        put_u64(&mut out, 8, starts_off);
        put_u64(&mut out, 16, ords_off);
        out
    }
}

/// A record array (SPEC §5.3.6) of `n` records.
fn read_records(stream: &[u8], n: usize) -> Vec<Vec<u8>> {
    let mut r = Reader::new(stream);
    let ends: Vec<usize> = (0..n).map(|_| r.read_u64().unwrap() as usize).collect();
    let data = r.rest();
    let mut start = 0;
    ends.iter()
        .map(|&end| {
            let rec = data[start..end].to_vec();
            start = end;
            rec
        })
        .collect()
}

fn write_records(records: &[Vec<u8>]) -> Vec<u8> {
    let mut w = Writer::new();
    let mut end = 0;
    for r in records {
        end += r.len() as u64;
        w.write_u64(end);
    }
    for r in records {
        w.write_bytes(r);
    }
    w.into_inner()
}

fn edit_records(seg: &mut Seg, kind: SectionKind, f: impl FnOnce(&mut Vec<Vec<u8>>)) {
    seg.edit_stream(kind, SEGMENT_FIELD, |s| {
        let mut recs = read_records(s, DOCS as usize);
        f(&mut recs);
        *s = write_records(&recs);
    });
}

/// An FST map of `entries` (sorted by key).
fn fst_bytes(entries: &BTreeMap<Vec<u8>, u64>) -> Vec<u8> {
    fst::Map::from_iter(entries.iter().map(|(k, &v)| (k.clone(), v)))
        .unwrap()
        .into_fst()
        .into_inner()
}

fn fst_entries(bytes: Vec<u8>) -> BTreeMap<Vec<u8>, u64> {
    let map = fst::Map::new(bytes).unwrap();
    let mut s = fst::IntoStreamer::into_stream(&map);
    let mut out = BTreeMap::new();
    while let Some((k, v)) = fst::Streamer::next(&mut s) {
        out.insert(k.to_vec(), v);
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------------------------------------------

const DOCS: u32 = 300;

/// 300 documents shaped so every structure a rule talks about exists, at a known place:
/// - `tag` (keyword) terms `all` (df 300: 2 full blocks + tail, 3 Skip entries), `half` (df 150: 1 full + tail,
///   2 Skip entries) and `zrare` (df 9, tail only, last in byte order, so its lists end every stream);
/// - `title` (text, positions) terms `alpha`/`beta` (df 300, tf 2..8 for alpha), `gamma` and `zeta` (df 6, tf 2,
///   last in byte order: its positions run ends the Positions stream);
/// - `body` (text, no positions), `price` (sparse f64: presence bitmap), `qty` (dense i64: presence length 0),
///   `flag` (sparse bool), `at` (sparse datetime), `raw` (stored only), blobs on docs 1, 51, ...
fn fixture() -> (CollectionSchema, Vec<u8>) {
    let schema = schema();
    let s = &schema;
    let docs: Vec<Document> = (0..DOCS)
        .map(|i| {
            let mut tags = vec!["all".to_owned()];
            if i % 2 == 0 {
                tags.push("half".to_owned());
            }
            if i % 37 == 0 {
                tags.push("zrare".to_owned());
            }
            let mut title = "alpha beta alpha ".repeat((i % 4) as usize + 1);
            if i % 50 == 0 {
                title.push_str("zeta gamma zeta");
            }
            let mut d = Document::new(format!("d{i:04}"))
                .with(id(s, "tag"), Value::Strings(tags))
                .with(id(s, "title"), Value::String(title))
                .with(
                    id(s, "body"),
                    Value::String(format!(
                        "running runs {}",
                        ["alpha", "beta", "x", "of"][(i % 4) as usize]
                    )),
                )
                .with(id(s, "qty"), Value::I64(i64::from(i) - 150))
                .with(id(s, "raw"), Value::Json(serde_json::json!({ "i": i })));
            if i % 2 == 0 {
                d.insert(id(s, "price"), Value::F64(f64::from(i) * 1.5));
            }
            if i % 3 == 0 {
                d.insert(id(s, "flag"), Value::Bool(i % 2 == 0));
            }
            if i % 7 != 0 {
                d.insert(
                    id(s, "at"),
                    Value::Datetime(1_600_000_000_000_000 + i64::from(i)),
                );
            }
            if i % 50 == 1 {
                d.set_blob_ref(Some(blob(u8::try_from(i % 256).unwrap())));
            }
            d
        })
        .collect();
    let bytes = build(&schema, &docs, WriteOptions::default());
    (schema, bytes)
}

/// 1100 documents with only `qty`: a numeric column of two FOR blocks (1024 + 76).
fn two_for_blocks() -> (CollectionSchema, Vec<u8>) {
    let schema = schema();
    let qty = id(&schema, "qty");
    let docs: Vec<Document> = (0..1100)
        .map(|i| Document::new(format!("q{i:05}")).with(qty, Value::I64(i * 3)))
        .collect();
    let bytes = build(&schema, &docs, WriteOptions::default());
    (schema, bytes)
}

// ---------------------------------------------------------------------------------------------------------------
// The case runner.
// ---------------------------------------------------------------------------------------------------------------

/// Where the corruption must surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// `Segment::open` fails.
    Open,
    /// `Segment::open` succeeds, reading everything back fails.
    Read,
}

type Edit = Box<dyn Fn(&mut Seg)>;
type Want = Box<dyn Fn(&SegmentCorruption) -> bool>;

struct Case {
    name: &'static str,
    edit: Edit,
    at: At,
    kind: SectionKind,
    want: Want,
}

fn case(
    name: &'static str,
    at: At,
    kind: SectionKind,
    want: Want,
    edit: impl Fn(&mut Seg) + 'static,
) -> Case {
    Case {
        name,
        edit: Box::new(edit),
        at,
        kind,
        want,
    }
}

/// A `SegmentCorruption::Invalid` whose rule mentions `what`.
fn invalid(what: &'static str) -> Want {
    Box::new(move |c| matches!(c, SegmentCorruption::Invalid { what: w } if w.contains(what)))
}

fn trailing() -> Want {
    Box::new(|c| *c == SegmentCorruption::TrailingBytes)
}

fn fst_err() -> Want {
    Box::new(|c| *c == SegmentCorruption::Fst)
}

fn format(f: fn(&Corruption) -> bool) -> Want {
    Box::new(move |c| matches!(c, SegmentCorruption::Format(inner) if f(inner)))
}

fn any() -> Want {
    Box::new(|_| true)
}

/// Checks one edited segment; `Err` describes how the reader got it wrong.
fn run(schema: &CollectionSchema, bytes: &[u8], c: &Case) -> Result<(), String> {
    let mut seg = Seg::parse(bytes);
    (c.edit)(&mut seg);
    let edited = seg.to_bytes();
    assert_ne!(edited, bytes, "{}: the edit changed nothing", c.name);
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let opened = open(schema, &edited).map(drop);
        (opened, read_back(schema, &edited).map(drop))
    }));
    let (opened, read) = match outcome {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            return Err(format!("panicked: {msg}"));
        }
    };
    let err = match (c.at, opened, read) {
        (At::Open, Ok(()), _) => return Err("opened".into()),
        (At::Open, Err(e), _) | (At::Read, Ok(()), Err(e)) => e,
        (At::Read, Err(e), _) => return Err(format!("expected open to succeed, got {e}")),
        (At::Read, Ok(()), Ok(())) => return Err("read back without an error".into()),
    };
    if !err.is_corruption() {
        return Err(format!("not a corruption: {err:?}"));
    }
    match &err {
        ReadError::Section { kind, reason, .. } if *kind == c.kind && (c.want)(reason) => Ok(()),
        other => Err(format!(
            "expected a corrupt {:?} section, got {other:?}",
            c.kind
        )),
    }
}

fn run_all(schema: &CollectionSchema, bytes: &[u8], cases: &[Case]) {
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|c| {
            run(schema, bytes, c)
                .err()
                .map(|e| format!("{}: {e}", c.name))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} cases failed:\n  {}",
        failures.len(),
        cases.len(),
        failures.join("\n  ")
    );
}

// ---------------------------------------------------------------------------------------------------------------
// The helper itself.
// ---------------------------------------------------------------------------------------------------------------

/// Parsing and rebuilding is the identity, and re-framing every block-framed section uncompressed still reads
/// back exactly as the original: failures below come from the edits, not from the editor.
#[test]
fn editor_round_trips() {
    let (schema, bytes) = fixture();
    let seg = Seg::parse(&bytes);
    assert_eq!(seg.to_bytes(), bytes);
    let mut reframed = seg.clone();
    let keys: Vec<(SectionKind, u32)> = seg.sections.iter().map(|s| (s.kind, s.field)).collect();
    for (kind, field) in keys {
        if kind.is_block_framed() {
            reframed.reframe(kind, field, |_| {});
        }
    }
    let reframed = reframed.to_bytes();
    assert_ne!(reframed, bytes, "DocStore is zstd in the original");
    assert_eq!(
        read_back(&schema, &reframed).unwrap(),
        read_back(&schema, &bytes).unwrap()
    );
    let (schema, bytes) = two_for_blocks();
    assert_eq!(Seg::parse(&bytes).to_bytes(), bytes);
    read_back(&schema, &bytes).unwrap();
}

// ---------------------------------------------------------------------------------------------------------------
// TermInfo (SPEC §5.3.1).
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn term_info_rules() {
    use SectionKind::{Skip, TermInfo};
    let (schema, bytes) = fixture();
    let tag = id(&schema, "tag").get();
    let title = id(&schema, "title").get();
    let body = id(&schema, "body").get();
    let info_u32 = |field: u32, term: &'static str, off: usize, v: u32| {
        move |s: &mut Seg| {
            let at = ti_at(s, field, term, off);
            s.edit_stream(TermInfo, field, |b| put_u32(b, at, v));
        }
    };
    let info_u64 = |field: u32, term: &'static str, off: usize, f: fn(&Seg, u64) -> u64| {
        move |s: &mut Seg| {
            let at = ti_at(s, field, term, off);
            let snapshot = s.clone();
            s.edit_stream(TermInfo, field, |b| {
                let v = f(&snapshot, le_u64(b, at));
                put_u64(b, at, v);
            });
        }
    };
    let cases = vec![
        case(
            "df 0",
            At::Read,
            TermInfo,
            invalid("a df of"),
            info_u32(tag, "half", ti::DF, 0),
        ),
        case(
            "df above doc_count",
            At::Read,
            TermInfo,
            invalid("a df of"),
            info_u32(tag, "zrare", ti::DF, DOCS + 1),
        ),
        case(
            "postings_off of record i+1 below record i's",
            At::Read,
            TermInfo,
            invalid("postings offsets"),
            info_u64(tag, "zrare", ti::POSTINGS, |s, _| {
                let half = ti_at(s, id(&super::schema(), "tag").get(), "half", ti::POSTINGS);
                le_u64(&s.stream(TermInfo, id(&super::schema(), "tag").get()), half) - 1
            }),
        ),
        case(
            "freqs_off of record i+1 equal to record i's (an empty list)",
            At::Read,
            TermInfo,
            invalid("freqs offsets"),
            info_u64(tag, "half", ti::FREQS, |_, _| 0),
        ),
        case(
            "postings_off past the stream",
            At::Read,
            TermInfo,
            invalid("postings offsets"),
            info_u64(tag, "zrare", ti::POSTINGS, |s, _| {
                s.stream(SectionKind::Postings, id(&super::schema(), "tag").get())
                    .len() as u64
                    + 1
            }),
        ),
        case(
            "positions_off past the stream",
            At::Read,
            TermInfo,
            invalid("positions offsets"),
            info_u64(title, "zeta", ti::POSITIONS, |s, _| {
                s.stream(SectionKind::Positions, id(&super::schema(), "title").get())
                    .len() as u64
                    + 1
            }),
        ),
        case(
            "skip_off with df < 128",
            At::Read,
            TermInfo,
            invalid("skip_off for a list shorter than 128"),
            info_u64(tag, "zrare", ti::SKIP, |_, _| 40),
        ),
        case(
            "skip_off past Skip",
            At::Read,
            TermInfo,
            invalid("skip_off"),
            info_u64(tag, "half", ti::SKIP, |s, _| {
                s.stream(Skip, id(&super::schema(), "tag").get()).len() as u64 - 40
            }),
        ),
        case(
            "positions_off without Positions",
            At::Read,
            TermInfo,
            invalid("positions_off without Positions"),
            info_u64(body, "run", ti::POSITIONS, |_, _| 8),
        ),
        case(
            "non-zero reserved byte",
            At::Read,
            TermInfo,
            format(|c| matches!(c, Corruption::NonZeroReserved { value: 1 })),
            move |s: &mut Seg| {
                let at = ti_at(s, tag, "half", ti::LEN - 1);
                s.edit_stream(TermInfo, tag, |b| b[at] = 1);
            },
        ),
        case(
            "max_tf 0",
            At::Read,
            TermInfo,
            invalid("max_tf"),
            info_u32(tag, "all", ti::MAX_TF, 0),
        ),
        case(
            "max_tf above ttf",
            At::Read,
            TermInfo,
            invalid("max_tf"),
            info_u32(title, "zeta", ti::MAX_TF, 13),
        ),
        case(
            "ttf below df",
            At::Read,
            TermInfo,
            invalid("ttf"),
            info_u64(tag, "half", ti::TTF, |_, _| 149),
        ),
        case(
            "keyword min_norm",
            At::Read,
            TermInfo,
            invalid("min_norm in a keyword field"),
            move |s: &mut Seg| {
                let at = ti_at(s, tag, "all", ti::MIN_NORM);
                s.edit_stream(TermInfo, tag, |b| b[at] = 3);
            },
        ),
        case(
            "one record more than the FST has",
            At::Open,
            TermInfo,
            fst_err(),
            move |s: &mut Seg| s.edit_stream(TermInfo, tag, |b| b.extend_from_slice(&[0; ti::LEN])),
        ),
        case(
            "one record less than the FST has",
            At::Open,
            TermInfo,
            fst_err(),
            move |s: &mut Seg| s.edit_stream(TermInfo, title, |b| b.truncate(b.len() - ti::LEN)),
        ),
        case(
            "a length that is not a multiple of 56",
            At::Open,
            TermInfo,
            fst_err(),
            move |s: &mut Seg| s.edit_stream(TermInfo, tag, |b| b.push(0)),
        ),
    ];
    run_all(&schema, &bytes, &cases);
}

// ---------------------------------------------------------------------------------------------------------------
// Postings, Freqs and Skip (SPEC §5.2.1, §5.3.2).
// ---------------------------------------------------------------------------------------------------------------

/// Stream position of field `off` of Skip entry `k` of `term`.
fn skip_at(s: &Seg, field: u32, term: &str, k: usize, off: usize) -> usize {
    let at = ti_at(s, field, term, ti::SKIP);
    le_u64(&s.stream(SectionKind::TermInfo, field), at) as usize + k * sk::LEN + off
}

fn term_u64(s: &Seg, field: u32, term: &str, off: usize) -> u64 {
    le_u64(
        &s.stream(SectionKind::TermInfo, field),
        ti_at(s, field, term, off),
    )
}

#[test]
fn postings_and_skip_rules() {
    use SectionKind::{Freqs, Postings, Skip, TermInfo};
    let (schema, bytes) = fixture();
    let tag = id(&schema, "tag").get();
    let title = id(&schema, "title").get();
    let skip_u32 = |field: u32, term: &'static str, k: usize, off: usize, f: fn(u32) -> u32| {
        move |s: &mut Seg| {
            let at = skip_at(s, field, term, k, off);
            s.edit_stream(Skip, field, |b| {
                let v = f(le_u32(b, at));
                put_u32(b, at, v);
            });
        }
    };
    let cases = vec![
        case(
            "tail count other than df mod 128 (df 9 -> 8)",
            At::Read,
            Postings,
            invalid("tail count"),
            move |s: &mut Seg| {
                // max_tf 2 keeps ttf (9) within df · max_tf, so the TermInfo still passes.
                let at = ti_at(s, tag, "zrare", ti::DF);
                let tf_at = ti_at(s, tag, "zrare", ti::MAX_TF);
                s.edit_stream(TermInfo, tag, |b| {
                    put_u32(b, at, 8);
                    put_u32(b, tf_at, 2);
                });
            },
        ),
        case(
            "tail count other than df mod 128 after full blocks (df 300 -> 299)",
            At::Read,
            Postings,
            invalid("tail count"),
            move |s: &mut Seg| {
                let at = ti_at(s, tag, "all", ti::DF);
                let tf_at = ti_at(s, tag, "all", ti::MAX_TF);
                s.edit_stream(TermInfo, tag, |b| {
                    put_u32(b, at, 299);
                    put_u32(b, tf_at, 2);
                });
            },
        ),
        case(
            "a docid at doc_count",
            At::Read,
            Postings,
            invalid("docid at or above doc_count"),
            move |s: &mut Seg| {
                let off = term_u64(s, tag, "zrare", ti::POSTINGS) as usize;
                s.edit_stream(Postings, tag, |b| {
                    let mut docs = [0_u32; 128];
                    let n = Reader::new(&b[off..])
                        .read_postings_tail(None, &mut docs)
                        .unwrap();
                    assert_eq!(n, 9);
                    docs[n - 1] = DOCS;
                    let mut w = Writer::new();
                    w.write_postings_tail(&docs[..n], None).unwrap();
                    b.truncate(off);
                    b.extend_from_slice(w.as_slice());
                });
            },
        ),
        case(
            "a full block's last_docid other than its decoded last",
            At::Read,
            Postings,
            format(|c| matches!(c, Corruption::LastDocidMismatch { .. })),
            move |s: &mut Seg| {
                let off = term_u64(s, tag, "all", ti::POSTINGS) as usize;
                s.edit_stream(Postings, tag, |b| {
                    assert_eq!(le_u32(b, off), 127);
                    put_u32(b, off, 200);
                });
            },
        ),
        case(
            "a full block's last_docid below prev + 128",
            At::Read,
            Postings,
            format(|c| matches!(c, Corruption::LastDocidOutOfRange { .. })),
            move |s: &mut Seg| {
                let off = term_u64(s, tag, "all", ti::POSTINGS) as usize;
                s.edit_stream(Postings, tag, |b| put_u32(b, off, 100));
            },
        ),
        case(
            "a skip entry's last_docid other than its block's",
            At::Read,
            Postings,
            invalid("differs from its skip entry"),
            skip_u32(tag, "half", 0, sk::LAST, |v| {
                assert_eq!(v, 254);
                255
            }),
        ),
        case(
            "the last skip entry's last_docid other than its tail's",
            At::Read,
            Postings,
            invalid("differs from its skip entry"),
            skip_u32(tag, "half", 1, sk::LAST, |v| {
                assert_eq!(v, 298);
                299
            }),
        ),
        case(
            "a skip entry's last_docid at doc_count",
            At::Read,
            Skip,
            invalid("last_docid"),
            skip_u32(tag, "all", 2, sk::LAST, |_| DOCS),
        ),
        case(
            "skip postings offsets that do not increase",
            At::Read,
            Skip,
            invalid("do not increase"),
            move |s: &mut Seg| {
                let (e0, e1) = (
                    skip_at(s, tag, "all", 0, sk::POSTINGS),
                    skip_at(s, tag, "all", 1, sk::POSTINGS),
                );
                s.edit_stream(Skip, tag, |b| {
                    let v = le_u64(b, e0);
                    put_u64(b, e1, v);
                });
            },
        ),
        case(
            "skip freqs offsets that decrease",
            At::Read,
            Skip,
            invalid("do not increase"),
            move |s: &mut Seg| {
                let (e1, e2) = (
                    skip_at(s, tag, "all", 1, sk::FREQS),
                    skip_at(s, tag, "all", 2, sk::FREQS),
                );
                s.edit_stream(Skip, tag, |b| {
                    let v = le_u64(b, e1);
                    put_u64(b, e2, v - 1);
                });
            },
        ),
        case(
            "first skip entry's postings_off other than the TermInfo's",
            At::Read,
            Skip,
            invalid("first skip entry's offsets"),
            move |s: &mut Seg| {
                let at = skip_at(s, tag, "half", 0, sk::POSTINGS);
                s.edit_stream(Skip, tag, |b| {
                    let v = le_u64(b, at);
                    put_u64(b, at, v + 1);
                });
            },
        ),
        case(
            "first skip entry's freqs_off other than the TermInfo's",
            At::Read,
            Skip,
            invalid("first skip entry's offsets"),
            move |s: &mut Seg| {
                let at = skip_at(s, title, "beta", 0, sk::FREQS);
                s.edit_stream(Skip, title, |b| {
                    let v = le_u64(b, at);
                    put_u64(b, at, v + 1);
                });
            },
        ),
        case(
            "skip max_tf above the TermInfo's",
            At::Read,
            Skip,
            invalid("bounds outside the term's"),
            skip_u32(tag, "all", 0, sk::MAX_TF, |v| v + 1),
        ),
        case(
            "skip max_tf 0",
            At::Read,
            Skip,
            invalid("bounds outside the term's"),
            skip_u32(tag, "all", 1, sk::MAX_TF, |_| 0),
        ),
        case(
            "a frequency above its block's skip max_tf",
            At::Read,
            Freqs,
            invalid("frequency above its block's max_tf"),
            skip_u32(title, "alpha", 0, sk::MAX_TF, |v| {
                assert!(v > 2);
                2
            }),
        ),
        case(
            "a frequency above the TermInfo max_tf of a list without Skip",
            At::Read,
            Freqs,
            invalid("frequency above its block's max_tf"),
            move |s: &mut Seg| {
                // zeta: tf 2 in each of its 6 documents. max_tf 1 with ttf 6 keeps df <= ttf <= df · max_tf.
                let at = ti_at(s, title, "zeta", ti::MAX_TF);
                let ttf_at = ti_at(s, title, "zeta", ti::TTF);
                s.edit_stream(TermInfo, title, |b| {
                    put_u32(b, at, 1);
                    put_u64(b, ttf_at, 6);
                });
            },
        ),
        case(
            "trailing bytes after a tail block (end of stream)",
            At::Read,
            Postings,
            trailing(),
            move |s: &mut Seg| s.edit_stream(Postings, tag, |b| b.push(0)),
        ),
        case(
            "trailing bytes after the last block of a skip list (range from the next TermInfo)",
            At::Read,
            Postings,
            trailing(),
            move |s: &mut Seg| {
                // A byte between half's tail and zrare's list; zrare's postings_off moves past it.
                let at = ti_at(s, tag, "zrare", ti::POSTINGS);
                let off = term_u64(s, tag, "zrare", ti::POSTINGS) as usize;
                s.edit_stream(Postings, tag, |b| b.insert(off, 0));
                s.edit_stream(TermInfo, tag, |b| put_u64(b, at, off as u64 + 1));
            },
        ),
        case(
            "trailing bytes after a frequency run",
            At::Read,
            Freqs,
            trailing(),
            move |s: &mut Seg| s.edit_stream(Freqs, tag, |b| b.push(0)),
        ),
        case(
            "a frequency run shorter than df",
            At::Read,
            Freqs,
            any(),
            move |s: &mut Seg| s.edit_stream(Freqs, tag, |b| b.truncate(b.len() - 1)),
        ),
    ];
    run_all(&schema, &bytes, &cases);
}

// ---------------------------------------------------------------------------------------------------------------
// Positions (SPEC §5.3.3) and Norms (§5.3.4).
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn positions_and_norms_rules() {
    use SectionKind::{Norms, Positions, Skip};
    let (schema, bytes) = fixture();
    let title = id(&schema, "title").get();
    let zeta_run = |f: fn(&mut Vec<u32>)| {
        move |s: &mut Seg| {
            let off = term_u64(s, title, "zeta", ti::POSITIONS) as usize;
            s.edit_stream(Positions, title, |b| {
                let mut run = read_varints(&b[off..]);
                assert_eq!(run.len(), 12, "6 documents, tf 2");
                f(&mut run);
                b.truncate(off);
                b.extend_from_slice(&write_varints(&run));
            });
        }
    };
    let cases = vec![
        case(
            "a positions run shorter than its frequencies",
            At::Read,
            Positions,
            any(),
            zeta_run(|r| {
                r.pop();
            }),
        ),
        case(
            "a positions run longer than its frequencies",
            At::Read,
            Positions,
            trailing(),
            zeta_run(|r| r.push(1)),
        ),
        case(
            "a position sum above u32::MAX",
            At::Read,
            Positions,
            invalid("position above u32::MAX"),
            zeta_run(|r| {
                r[0] = u32::MAX;
                r[1] = 1;
            }),
        ),
        case(
            "a block's positions range too short for its Σtf (from the next Skip entry)",
            At::Read,
            Positions,
            invalid("positions run whose length does not fit"),
            move |s: &mut Seg| {
                // alpha's first block holds 128 documents with tf 2..8 (Σtf ≥ 256): a 1-byte range cannot hold
                // it. Entry 1's positions_off still grows and stays inside alpha's run.
                let e0 = skip_at(s, title, "alpha", 0, sk::POSITIONS);
                let e1 = skip_at(s, title, "alpha", 1, sk::POSITIONS);
                s.edit_stream(Skip, title, |b| {
                    let v = le_u64(b, e0);
                    put_u64(b, e1, v + 1);
                });
            },
        ),
        case(
            "Norms one byte longer than doc_count",
            At::Read,
            Norms,
            invalid("Norms stream"),
            move |s: &mut Seg| s.edit_stream(Norms, title, |b| b.push(0)),
        ),
        case(
            "Norms one byte shorter than doc_count",
            At::Read,
            Norms,
            invalid("Norms stream"),
            move |s: &mut Seg| s.edit_stream(Norms, title, |b| b.truncate(b.len() - 1)),
        ),
    ];
    run_all(&schema, &bytes, &cases);
}

// ---------------------------------------------------------------------------------------------------------------
// Columns (SPEC §5.3.5).
// ---------------------------------------------------------------------------------------------------------------

/// Re-encodes a Bool column with `presence`/`trues` replaced by `f`.
fn edit_bool(s: &mut Seg, field: u32, f: impl FnOnce(&mut Option<Vec<u8>>, &mut Option<Vec<u8>>)) {
    s.edit_stream(SectionKind::Columns, field, |b| {
        let mut r = Reader::new(b);
        let header = r.read_bytes(8).unwrap().to_vec();
        let mut presence = read_bitmap(&mut r);
        let mut trues = read_bitmap(&mut r);
        assert!(r.is_empty());
        f(&mut presence, &mut trues);
        let mut w = Writer::new();
        w.write_bytes(&header);
        write_bitmap(&mut w, presence.as_deref());
        write_bitmap(&mut w, trues.as_deref());
        *b = w.into_inner();
    });
}

/// Re-encodes a numeric column with its presence bitmap bytes replaced by `f`.
fn edit_presence(s: &mut Seg, field: u32, f: impl FnOnce(&mut Option<Vec<u8>>)) {
    s.edit_stream(SectionKind::Columns, field, |b| {
        let mut r = Reader::new(b);
        let header = r.read_bytes(8).unwrap().to_vec();
        let mut presence = read_bitmap(&mut r);
        let rest = r.rest().to_vec();
        f(&mut presence);
        let mut w = Writer::new();
        w.write_bytes(&header);
        write_bitmap(&mut w, presence.as_deref());
        w.write_bytes(&rest);
        *b = w.into_inner();
    });
}

fn edit_keyword(s: &mut Seg, field: u32, f: impl FnOnce(&mut KeywordCol)) {
    s.edit_stream(SectionKind::Columns, field, |b| {
        let mut col = KeywordCol::decode(b);
        f(&mut col);
        *b = col.encode();
    });
}

#[test]
fn column_rules() {
    use SectionKind::Columns;
    let (schema, bytes) = fixture();
    let tag = id(&schema, "tag").get();
    let qty = id(&schema, "qty").get();
    let price = id(&schema, "price").get();
    let at = id(&schema, "at").get();
    let flag = id(&schema, "flag").get();
    let cases = vec![
        case(
            "a numeric column with the bool tag",
            At::Read,
            Columns,
            invalid("column tag"),
            move |s: &mut Seg| s.edit_stream(Columns, qty, |b| b[0] = 2),
        ),
        case(
            "a keyword column with the numeric tag",
            At::Read,
            Columns,
            invalid("column tag"),
            move |s: &mut Seg| s.edit_stream(Columns, tag, |b| b[0] = 1),
        ),
        case(
            "an unknown tag",
            At::Read,
            Columns,
            invalid("column tag"),
            move |s: &mut Seg| s.edit_stream(Columns, flag, |b| b[0] = 9),
        ),
        case(
            "non-zero reserved bytes after the tag",
            At::Read,
            Columns,
            format(|c| matches!(c, Corruption::NonZeroReserved { value: 1 })),
            move |s: &mut Seg| s.edit_stream(Columns, at, |b| b[7] = 1),
        ),
        case(
            "a FOR column count other than doc_count",
            At::Read,
            Columns,
            invalid("FOR column count"),
            move |s: &mut Seg| {
                s.edit_stream(Columns, qty, |b| {
                    assert_eq!(le_u64(b, 8), 0, "dense: presence length 0");
                    assert_eq!(le_u64(b, 16), u64::from(DOCS));
                    put_u64(b, 16, u64::from(DOCS) - 1);
                });
            },
        ),
        case(
            "a FOR block end of 0 (ends that do not increase)",
            At::Read,
            Columns,
            invalid("FOR block ends"),
            move |s: &mut Seg| s.edit_stream(Columns, qty, |b| put_u64(b, 24, 0)),
        ),
        case(
            "bytes after a FOR block inside its range",
            At::Read,
            Columns,
            invalid("bytes after a FOR block"),
            move |s: &mut Seg| {
                s.edit_stream(Columns, qty, |b| {
                    let end = le_u64(b, 24);
                    put_u64(b, 24, end + 1);
                    b.push(0);
                });
            },
        ),
        case(
            "a presence bitmap that does not deserialize",
            At::Read,
            Columns,
            invalid("does not deserialize"),
            move |s: &mut Seg| {
                edit_presence(s, price, |p| {
                    let p = p.as_mut().expect("price is sparse");
                    p[..4].copy_from_slice(&[0; 4]);
                });
            },
        ),
        case(
            "a presence bitmap with a docid at doc_count",
            At::Read,
            Columns,
            invalid("bitmap docid at or above doc_count"),
            move |s: &mut Seg| {
                edit_presence(s, price, |p| {
                    let mut bm = RoaringBitmap::deserialize_from(&p.as_ref().unwrap()[..]).unwrap();
                    bm.insert(DOCS);
                    *p = Some(serialize(&bm));
                });
            },
        ),
        case(
            "a presence bitmap with bytes after it inside its length",
            At::Read,
            Columns,
            invalid("bytes after a bitmap"),
            move |s: &mut Seg| edit_presence(s, price, |p| p.as_mut().unwrap().push(0)),
        ),
        case(
            "bool trues not a subset of presence",
            At::Read,
            Columns,
            invalid("true value for a document without one"),
            move |s: &mut Seg| {
                edit_bool(s, flag, |presence, trues| {
                    assert!(presence.is_some(), "flag is sparse");
                    *trues = Some(serialize(&RoaringBitmap::from_iter([1])));
                });
            },
        ),
        case(
            "bool without its trues bitmap (length 0)",
            At::Read,
            Columns,
            invalid("without its true bitmap"),
            move |s: &mut Seg| edit_bool(s, flag, |_, trues| *trues = None),
        ),
        case(
            "bool trues with a docid at doc_count",
            At::Read,
            Columns,
            invalid("bitmap docid at or above doc_count"),
            move |s: &mut Seg| {
                edit_bool(s, flag, |presence, trues| {
                    *presence = None;
                    *trues = Some(serialize(&RoaringBitmap::from_iter([0, DOCS])));
                });
            },
        ),
        case(
            "trailing bytes after a numeric column",
            At::Read,
            Columns,
            trailing(),
            move |s: &mut Seg| s.edit_stream(Columns, qty, |b| b.push(0)),
        ),
        case(
            "trailing bytes after a bool column",
            At::Read,
            Columns,
            trailing(),
            move |s: &mut Seg| s.edit_stream(Columns, flag, |b| b.push(0)),
        ),
        case(
            "trailing bytes after a keyword column",
            At::Read,
            Columns,
            trailing(),
            move |s: &mut Seg| s.edit_stream(Columns, tag, |b| b.push(0)),
        ),
        case(
            "keyword starts_off off by one",
            At::Read,
            Columns,
            invalid("starts_off"),
            move |s: &mut Seg| {
                s.edit_stream(Columns, tag, |b| {
                    let v = le_u64(b, 8);
                    put_u64(b, 8, v + 1);
                });
            },
        ),
        case(
            "keyword ords_off off by one",
            At::Read,
            Columns,
            invalid("ords_off"),
            move |s: &mut Seg| {
                s.edit_stream(Columns, tag, |b| {
                    let v = le_u64(b, 16);
                    put_u64(b, 16, v - 1);
                });
            },
        ),
        case(
            "keyword dictionary count 0",
            At::Read,
            Columns,
            invalid("keyword dictionary count"),
            move |s: &mut Seg| s.edit_stream(Columns, tag, |b| put_u64(b, 24, 0)),
        ),
        case(
            "keyword values out of order",
            At::Read,
            Columns,
            invalid("not strictly increasing"),
            move |s: &mut Seg| edit_keyword(s, tag, |c| c.values.swap(0, 1)),
        ),
        case(
            "keyword values duplicated",
            At::Read,
            Columns,
            invalid("not strictly increasing"),
            move |s: &mut Seg| {
                edit_keyword(s, tag, |c| {
                    assert_eq!(c.values, [&b"all"[..], b"half", b"zrare"]);
                    c.values[1] = b"all".to_vec();
                });
            },
        ),
        case(
            "keyword value ends that decrease",
            At::Read,
            Columns,
            invalid("value ends that decrease"),
            move |s: &mut Seg| {
                // ends 3, 7, 12 -> 3, 2, 12
                s.edit_stream(Columns, tag, |b| {
                    assert_eq!(le_u64(b, 40), 7);
                    put_u64(b, 40, 2);
                });
            },
        ),
        case(
            "keyword starts that decrease",
            At::Read,
            Columns,
            invalid("starts that decrease"),
            move |s: &mut Seg| {
                edit_keyword(s, tag, |c| {
                    // doc 0 has 3 ords (all, half, zrare): starts 0, 3, 4, ...
                    assert_eq!(&c.starts[..3], [0, 3, 4]);
                    c.starts[1] = 5;
                });
            },
        ),
        case(
            "keyword first start other than 0",
            At::Read,
            Columns,
            invalid("starts that decrease"),
            move |s: &mut Seg| edit_keyword(s, tag, |c| c.starts[0] = 1),
        ),
        case(
            "keyword ord at the dictionary count",
            At::Read,
            Columns,
            invalid("past the dictionary"),
            move |s: &mut Seg| edit_keyword(s, tag, |c| c.ords[2] = 3),
        ),
        case(
            "keyword ords not increasing within a document",
            At::Read,
            Columns,
            invalid("not increasing"),
            move |s: &mut Seg| edit_keyword(s, tag, |c| c.ords.swap(0, 1)),
        ),
        case(
            "keyword ords repeated within a document",
            At::Read,
            Columns,
            invalid("not increasing"),
            move |s: &mut Seg| edit_keyword(s, tag, |c| c.ords[1] = c.ords[0]),
        ),
        case(
            "keyword starts column count other than doc_count + 1",
            At::Read,
            Columns,
            invalid("FOR column count"),
            move |s: &mut Seg| {
                edit_keyword(s, tag, |c| {
                    c.starts.pop();
                });
            },
        ),
        case(
            "keyword ords column count other than the last start",
            At::Read,
            Columns,
            invalid("FOR column count"),
            move |s: &mut Seg| {
                edit_keyword(s, tag, |c| {
                    c.ords.push(2);
                });
            },
        ),
    ];
    run_all(&schema, &bytes, &cases);
}

#[test]
fn multi_block_for_column_rules() {
    use SectionKind::Columns;
    let (schema, bytes) = two_for_blocks();
    let qty = id(&schema, "qty").get();
    let with_keys = |f: fn(&[u64], &mut Writer)| {
        move |s: &mut Seg| {
            s.edit_stream(Columns, qty, |b| {
                let mut r = Reader::new(b);
                let header = r.read_bytes(8).unwrap().to_vec();
                assert_eq!(read_bitmap(&mut r), None);
                let keys = read_for(&mut r);
                assert!(r.is_empty());
                let mut w = Writer::new();
                w.write_bytes(&header);
                w.write_u64(0);
                f(&keys, &mut w);
                *b = w.into_inner();
            });
        }
    };
    let cases = vec![
        case(
            "a first FOR block of 1000 values",
            At::Read,
            Columns,
            invalid("FOR block count"),
            with_keys(|k, w| write_for_blocks(w, &[&k[..1000], &k[1000..]])),
        ),
        case(
            "a last FOR block longer than the rest",
            At::Read,
            Columns,
            invalid("FOR block count"),
            with_keys(|k, w| {
                // The count stays doc_count (1100), but the last block holds 86 values instead of 76.
                let mut v = k.to_vec();
                v.extend_from_slice(&[0; 10]);
                let mut t = Writer::new();
                write_for_blocks(&mut t, &[&v[..1024], &v[1024..]]);
                let mut t = t.into_inner();
                put_u64(&mut t, 0, k.len() as u64);
                w.write_bytes(&t);
            }),
        ),
        case(
            "FOR block ends that do not increase",
            At::Read,
            Columns,
            invalid("FOR block ends"),
            move |s: &mut Seg| {
                s.edit_stream(Columns, qty, |b| {
                    // header 8, presence len 8, count 8, ends at 24 and 32.
                    let first = le_u64(b, 24);
                    put_u64(b, 32, first);
                });
            },
        ),
        case(
            "FOR block ends that decrease",
            At::Read,
            Columns,
            invalid("FOR block ends"),
            move |s: &mut Seg| {
                s.edit_stream(Columns, qty, |b| {
                    let first = le_u64(b, 24);
                    put_u64(b, 32, first - 1);
                });
            },
        ),
    ];
    run_all(&schema, &bytes, &cases);
}

// ---------------------------------------------------------------------------------------------------------------
// Record arrays and raw sections (SPEC §5.3.6, §5.3.1, §5.3 "Framing").
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn record_array_rules() {
    use SectionKind::{BlobRefs, DocStore, IdList};
    let (schema, bytes) = fixture();
    let last_end_plus_one = |kind: SectionKind| {
        move |s: &mut Seg| {
            s.edit_stream(kind, SEGMENT_FIELD, |b| {
                let at = (DOCS as usize - 1) * 8;
                let v = le_u64(b, at);
                put_u64(b, at, v + 1);
            });
        }
    };
    // end[k + 1] = end[k − 1] < end[k]: record k + 1 starts after it ends.
    let decreasing = |kind: SectionKind, k: usize| {
        move |s: &mut Seg| {
            s.edit_stream(kind, SEGMENT_FIELD, |b| {
                let before = le_u64(b, (k - 1) * 8);
                assert!(before < le_u64(b, k * 8), "record {k} is not empty");
                put_u64(b, (k + 1) * 8, before);
            });
        }
    };
    let cases = vec![
        case(
            "DocStore last end other than the data length",
            At::Open,
            DocStore,
            invalid("last record end"),
            last_end_plus_one(DocStore),
        ),
        case(
            "IdList last end other than the data length",
            At::Open,
            IdList,
            invalid("last record end"),
            last_end_plus_one(IdList),
        ),
        case(
            "BlobRefs last end other than the data length",
            At::Open,
            BlobRefs,
            invalid("last record end"),
            last_end_plus_one(BlobRefs),
        ),
        case(
            "a record array shorter than its ends",
            At::Open,
            IdList,
            invalid("shorter than its ends"),
            move |s: &mut Seg| {
                s.edit_stream(IdList, SEGMENT_FIELD, |b| b.truncate(DOCS as usize * 8 - 8));
            },
        ),
        case(
            "DocStore ends that decrease",
            At::Read,
            DocStore,
            invalid("decrease"),
            decreasing(DocStore, 4),
        ),
        case(
            "IdList ends that decrease",
            At::Read,
            IdList,
            invalid("decrease"),
            decreasing(IdList, 4),
        ),
        case(
            "BlobRefs ends that decrease",
            At::Read,
            BlobRefs,
            invalid("decrease"),
            decreasing(BlobRefs, 51),
        ),
        case(
            "an empty IdList record",
            At::Read,
            IdList,
            invalid("empty or non-UTF-8"),
            move |s: &mut Seg| edit_records(s, IdList, |r| r[3].clear()),
        ),
        case(
            "an IdList record longer than any id (513 bytes)",
            At::Read,
            IdList,
            invalid("longer than any valid one"),
            move |s: &mut Seg| edit_records(s, IdList, |r| r[3] = vec![b'x'; 513]),
        ),
        case(
            "a BlobRefs record longer than any BlobRef encoding",
            At::Read,
            BlobRefs,
            invalid("longer than any valid one"),
            move |s: &mut Seg| {
                edit_records(s, BlobRefs, |r| {
                    r[1] = vec![0; persia_format::BlobRef::MAX_ENCODED_LEN + 1];
                });
            },
        ),
        case(
            "a non-UTF-8 IdList record",
            At::Read,
            IdList,
            invalid("empty or non-UTF-8"),
            move |s: &mut Seg| edit_records(s, IdList, |r| r[3] = vec![b'd', 0xFF]),
        ),
        case(
            "a BlobRefs record with a trailing byte",
            At::Read,
            BlobRefs,
            trailing(),
            move |s: &mut Seg| {
                edit_records(s, BlobRefs, |r| {
                    assert!(!r[1].is_empty(), "doc 1 has a blob");
                    r[1].push(0);
                });
            },
        ),
        case(
            "a BlobRefs record that is garbage",
            At::Read,
            BlobRefs,
            any(),
            move |s: &mut Seg| {
                // Version 1, then not enough bytes for a blob id.
                edit_records(s, BlobRefs, |r| r[1] = [&[1_u8][..], &[0xFF; 11]].concat());
            },
        ),
        case(
            "a BlobRefs record cut short",
            At::Read,
            BlobRefs,
            any(),
            move |s: &mut Seg| {
                edit_records(s, BlobRefs, |r| {
                    r[1].pop();
                });
            },
        ),
        case(
            "a BlobRefs record for a document without a blob",
            At::Read,
            BlobRefs,
            any(),
            move |s: &mut Seg| {
                edit_records(s, BlobRefs, |r| {
                    assert!(r[0].is_empty());
                    r[0] = vec![1];
                });
            },
        ),
    ];
    run_all(&schema, &bytes, &cases);
}
/// A BlobRefs record of a newer `BlobRef` version (SPEC §9.1, ADR-0011) is not readable by this build: an
/// `Unsupported` error (upgrade, do not repair), never `Ok` and never a panic.
#[test]
fn blob_ref_of_a_newer_version_is_unsupported() {
    let (schema, bytes) = fixture();
    let mut s = Seg::parse(&bytes);
    edit_records(&mut s, SectionKind::BlobRefs, |r| r[1][0] = 2);
    let edited = s.to_bytes();
    let seg = open(&schema, &edited).unwrap();
    let err = seg.blob_ref(1).unwrap_err();
    assert!(
        matches!(
            &err,
            ReadError::Block {
                kind: SectionKind::BlobRefs,
                source: persia_format::Error::Unsupported { .. },
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.is_corruption());
    assert_eq!(seg.blob_ref(0).unwrap(), None);
}

type RowEdit = (&'static str, fn(&mut Vec<u8>));

/// A DocStore row that does not decode is `ReadError::Document` (SPEC §6.4.2), a corruption, never a panic.
#[test]
fn undecodable_doc_store_rows() {
    let (schema, bytes) = fixture();
    let edits: Vec<RowEdit> = vec![
        ("empty", Vec::clear),
        ("cut short", |r| {
            r.pop();
        }),
        ("a trailing byte", |r| r.push(0)),
        ("garbage", |r| *r = vec![0xFF; 9]),
        ("first byte flipped", |r| r[0] ^= 0x20),
    ];
    let mut failures = Vec::new();
    for (name, f) in edits {
        let mut s = Seg::parse(&bytes);
        edit_records(&mut s, SectionKind::DocStore, |r| f(&mut r[2]));
        let edited = s.to_bytes();
        let got = catch_unwind(AssertUnwindSafe(|| {
            let seg = open(&schema, &edited).unwrap();
            (
                seg.document(2).map(drop),
                read_back(&schema, &edited).map(drop),
            )
        }));
        match got {
            Err(_) => failures.push(format!("{name}: panicked")),
            Ok((Err(e @ ReadError::Document { docid: 2, .. }), Err(all)))
                if e.is_corruption() && all == e => {}
            Ok(other) => failures.push(format!("{name}: {other:?}")),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn raw_section_rules() {
    use SectionKind::{IdMap, TermDict, TermInfo};
    let (schema, bytes) = fixture();
    let tag = id(&schema, "tag").get();
    let title = id(&schema, "title").get();
    let id_map = |f: fn(&mut BTreeMap<Vec<u8>, u64>)| {
        move |s: &mut Seg| {
            s.edit_raw(IdMap, SEGMENT_FIELD, |p| {
                let mut entries = fst_entries(p.clone());
                assert_eq!(entries.len(), DOCS as usize);
                f(&mut entries);
                *p = fst_bytes(&entries);
            });
        }
    };
    let cases = vec![
        case(
            "a TermDict that is not an FST (version 99)",
            At::Open,
            TermDict,
            fst_err(),
            move |s: &mut Seg| {
                s.edit_raw(TermDict, tag, |p| {
                    p[..8].copy_from_slice(&99_u64.to_le_bytes());
                });
            },
        ),
        case(
            "a TermDict cut short",
            At::Open,
            TermDict,
            fst_err(),
            move |s: &mut Seg| s.edit_raw(TermDict, title, |p| p.truncate(p.len() / 2)),
        ),
        case(
            "a TermDict with fewer terms than TermInfo records",
            At::Open,
            TermInfo,
            fst_err(),
            move |s: &mut Seg| {
                s.edit_raw(TermDict, tag, |p| {
                    let mut entries = fst_entries(p.clone());
                    entries.remove(&b"zrare"[..]);
                    *p = fst_bytes(&entries);
                });
            },
        ),
        case(
            "a TermDict ordinal at the record count",
            At::Open,
            TermDict,
            invalid("rank"),
            move |s: &mut Seg| {
                s.edit_raw(TermDict, tag, |p| {
                    let mut entries = fst_entries(p.clone());
                    entries.insert(b"zrare".to_vec(), 3);
                    *p = fst_bytes(&entries);
                });
            },
        ),
        case(
            "an IdMap that is not an FST",
            At::Open,
            IdMap,
            fst_err(),
            move |s: &mut Seg| {
                s.edit_raw(IdMap, SEGMENT_FIELD, |p| {
                    p[..8].copy_from_slice(&99_u64.to_le_bytes());
                });
            },
        ),
        case(
            "an IdMap with one id less than doc_count",
            At::Open,
            IdMap,
            fst_err(),
            id_map(|e| {
                e.remove(&b"d0007"[..]);
            }),
        ),
        case(
            "an IdMap with one id more than doc_count",
            At::Open,
            IdMap,
            invalid("distinct docid"),
            id_map(|e| {
                e.insert(b"zzz".to_vec(), 0);
            }),
        ),
        case(
            "an IdMap value at doc_count",
            At::Open,
            IdMap,
            invalid("distinct docid"),
            id_map(|e| {
                e.insert(b"d0007".to_vec(), u64::from(DOCS));
            }),
        ),
        case(
            "an IdMap value above u32::MAX",
            At::Open,
            IdMap,
            invalid("distinct docid"),
            id_map(|e| {
                e.insert(b"d0007".to_vec(), 1 << 40);
            }),
        ),
    ];
    run_all(&schema, &bytes, &cases);

    // Open walks the IdMap, so a docid at doc_count never reaches `Segment::docid_of`.
    let mut s = Seg::parse(&bytes);
    id_map(|e| {
        e.insert(b"d0007".to_vec(), u64::from(DOCS));
    })(&mut s);
    let edited = s.to_bytes();
    let err = open(&schema, &edited).unwrap_err();
    assert!(err.is_corruption());
    assert!(
        matches!(
            err,
            ReadError::Section {
                kind: IdMap,
                field: SEGMENT_FIELD,
                ..
            }
        ),
        "{err:?}"
    );
    let seg = open(&schema, &bytes).unwrap();
    assert_eq!(seg.docid_of("d0008").unwrap(), Some(8));
}

// ---------------------------------------------------------------------------------------------------------------
// Rules of SPEC §5.3.1 that span a whole section (consistent segments with unreferenced bytes): corruption by SPEC,
// checked by `Segment::verify` (ROADMAP 5.4.1) rather than by every lookup.
// ---------------------------------------------------------------------------------------------------------------

/// §5.3.1: "the `skip_off` of each term with df ≥ 128 is where the previous such term's entries end". A 40-byte
/// gap between `all`'s and `half`'s entries, with `half`'s `skip_off` moved past it, is otherwise consistent.
#[test]
#[ignore = "ROADMAP 5.4.1: the skip_off chain spans every term, so Segment::verify checks it, not a lookup"]
fn skip_entries_not_back_to_back_are_corruption() {
    use SectionKind::{Skip, TermInfo};
    let (schema, bytes) = fixture();
    let tag = id(&schema, "tag").get();
    let cases = vec![
        case(
            "a gap between two terms' Skip entries",
            At::Read,
            Skip,
            any(),
            move |s: &mut Seg| {
                let at = ti_at(s, tag, "half", ti::SKIP);
                let off = term_u64(s, tag, "half", ti::SKIP) as usize;
                assert_eq!(off, 3 * sk::LEN);
                s.edit_stream(Skip, tag, |b| {
                    let gap = b[..sk::LEN].to_vec();
                    b.splice(off..off, gap);
                });
                s.edit_stream(TermInfo, tag, |b| put_u64(b, at, (off + sk::LEN) as u64));
            },
        ),
        case(
            "bytes after the last term's Skip entries",
            At::Read,
            Skip,
            any(),
            move |s: &mut Seg| {
                s.edit_stream(Skip, tag, |b| b.extend_from_slice(&[0; sk::LEN]));
            },
        ),
    ];
    run_all(&schema, &bytes, &cases);
}

/// §5.3.1: lists are written back to back from the start of each stream. Leading bytes before the first list, with
/// every offset (TermInfo and Skip) moved past them, are otherwise consistent.
#[test]
fn bytes_before_the_first_list_are_corruption() {
    use SectionKind::{Postings, Skip, TermInfo};
    let (schema, bytes) = fixture();
    let tag = id(&schema, "tag").get();
    let cases = vec![case(
        "8 bytes before the first postings list",
        At::Read,
        TermInfo,
        invalid("bytes before the first list"),
        move |s: &mut Seg| {
            s.edit_stream(Postings, tag, |b| {
                b.splice(0..0, [0_u8; 8]);
            });
            s.edit_stream(TermInfo, tag, |b| {
                for r in 0..b.len() / ti::LEN {
                    let at = r * ti::LEN + ti::POSTINGS;
                    let v = le_u64(b, at);
                    put_u64(b, at, v + 8);
                }
            });
            s.edit_stream(Skip, tag, |b| {
                for e in 0..b.len() / sk::LEN {
                    let at = e * sk::LEN + sk::POSTINGS;
                    let v = le_u64(b, at);
                    put_u64(b, at, v + 8);
                }
            });
        },
    )];
    run_all(&schema, &bytes, &cases);
}
