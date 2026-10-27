//! Fuzz the segment reader (ROADMAP 5.3; SPEC §5.1–§5.3.6, §16.1): no input makes `Segment::open` or any read of
//! an opened segment panic. Damage must come back as an `Err` (CLAUDE.md "Safety & errors").
//!
//! Raw bytes almost never get past the footer and section CRCs, so the input picks a mode by its first byte (mod 4):
//! - 0: the rest is opened as a segment, as it is;
//! - 1: the rest is a list of edits to the decoded streams of the block-framed sections of one fixed, valid
//!   segment (built once). Each edited section is re-framed (`Compression::None`, same block size), the sections are
//!   laid out again and the footer is rewritten with their offsets and lengths, its CRC and the locator. So every
//!   CRC holds and only the semantic checks of SPEC §5.3 stand between the edits and the reader's decoders;
//! - 2: edits to the raw sections (the TermDict and IdMap FSTs): their entries (keys, values, entries added,
//!   removed or swapped), rebuilt as a valid FST, or their payload bytes; the payload CRC is recomputed;
//! - 3: edits to the footer body (bytes, small deltas to `u64` fields, insertions, truncation) after the layout,
//!   with its CRC and length recomputed: the footer rules of SPEC §5.1 alone stand between them and the reader.
//!
//! In every mode an opened segment is read completely: every term with its postings, frequencies and positions,
//! `advance` and `shallow_advance` to a few targets, norms, columns, every document, id and blob reference, then
//! `Segment::verify`. Errors are ignored; the only failure is a panic (or a hang).
#![no_main]

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::segment::{
    ReadError, SectionKind, Segment, SegmentBuilder, SegmentMeta, SegmentWriter, TermIndex,
    WriteOptions,
};
use persia_format::block::{BlockReader, BlockWriter, Compression};
use persia_format::checksum::crc32c;
use persia_format::{BlobId, BlobRef, ByteSource};

/// One edit record of modes 1–3 (see the module docs): mode 1 reads section selector, stream offset (`u24` LE),
/// operation, byte; mode 2 section selector, entry index (`u16` LE), operation, two bytes; mode 3 ignores the first
/// byte and reads footer offset (`u16` LE), operation, byte.
const EDIT_LEN: usize = 6;
/// Edits applied per input at most, so an execution stays fast.
const MAX_EDITS: usize = 64;
/// Postings read per term at most: a corrupted `df` must not turn one execution into a long one.
const MAX_POSTINGS: usize = 4096;

const WORDS: &[&str] = &[
    "alpha",
    "beta",
    "the",
    "running",
    "runs",
    "Ünïcode",
    "x",
    "of",
    "Gamma",
];
const TAGS: &[&str] = &["red", "green", "", "blue", "Red"];

fn schema() -> CollectionSchema {
    SchemaBuilder::new("fuzz")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field(
            "body",
            FieldKind::Text(TextOptions {
                analyzer: AnalyzerRef::builtin("en").expect("builtin analyzer"),
                stored: false,
                ..TextOptions::default()
            }),
        )
        .field(
            "tag",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .field("price", FieldKind::F64 { stored: true })
        .field("qty", FieldKind::I64 { stored: false })
        .field("at", FieldKind::Datetime { stored: true })
        .field("flag", FieldKind::Bool { stored: true })
        .field("raw", FieldKind::Stored)
        .build()
        .expect("valid schema")
}

fn id(schema: &CollectionSchema, name: &str) -> FieldId {
    schema.field(name).expect("field exists").id()
}

/// 160 documents: `body` terms reach >128 docids (several postings blocks, so `Skip` entries), every field kind
/// has values and gaps, and some documents carry a blob reference.
fn documents(schema: &CollectionSchema) -> Vec<Document> {
    (0..160_u32)
        .map(|i| {
            let w = |k: u32| WORDS[((i + k) as usize) % WORDS.len()];
            let mut d = Document::new(format!("doc-{:03}", (i * 7) % 160));
            if i % 3 != 2 {
                let title = if i % 5 == 0 {
                    Value::Strings(vec![format!("{} {}", w(0), w(1)), w(2).to_owned()])
                } else {
                    Value::String(format!("the {} {} the", w(0), w(3)))
                };
                d.insert(id(schema, "title"), title);
            }
            d.insert(
                id(schema, "body"),
                Value::String(format!("running runs {}", WORDS[(i % 4) as usize])),
            );
            if i % 4 != 0 {
                d.insert(
                    id(schema, "tag"),
                    Value::Strings(vec![TAGS[(i % 5) as usize].to_owned(), "red".to_owned()]),
                );
            }
            if i % 2 == 0 {
                d.insert(id(schema, "price"), Value::F64(f64::from(i) * 1.5 - 100.0));
            }
            d.insert(id(schema, "qty"), Value::I64(i64::from(i) - 150));
            if i % 7 != 0 {
                d.insert(
                    id(schema, "at"),
                    Value::Datetime(1_600_000_000_000_000 + i64::from(i) * 1_000_000),
                );
            }
            if i % 3 == 0 {
                d.insert(id(schema, "flag"), Value::Bool(i % 2 == 0));
            }
            d.insert(
                id(schema, "raw"),
                Value::Json(serde_json::json!({ "i": i, "s": "x" })),
            );
            if i % 50 == 1 {
                let b = u8::try_from(i % 256).expect("fits");
                let meta = BTreeMap::from([("origin".to_owned(), format!("scan-{i}"))]);
                let blob = BlobRef::new(
                    BlobId::from_bytes([b; 32]),
                    1000 + u64::from(b),
                    "application/pdf",
                    4 << 20,
                    meta,
                )
                .expect("valid blob ref");
                d.set_blob_ref(Some(blob));
            }
            d
        })
        .collect()
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}
fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}
fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().expect("2 bytes"))
}

/// The `(key, value)` entries of an FST, in key order.
type Entries = Vec<(Vec<u8>, u64)>;

/// The fixed segment, split into its footer body (without CRC and locator) and its sections in table order
/// (SPEC §5.1). Only the harness's own, valid bytes are parsed here.
struct Fixed {
    schema: CollectionSchema,
    body: Vec<u8>,
    sections: Vec<Vec<u8>>,
    /// Indices into `sections` of the block-framed ones, with their decoded stream and block size.
    framed: Vec<(usize, Vec<u8>, u32)>,
    /// Indices into `sections` of the raw ones (the FSTs), with their entries.
    raw: Vec<(usize, Entries)>,
}

impl Fixed {
    fn build() -> Self {
        let schema = schema();
        let registry = AnalyzerRegistry::builtin();
        let mut builder = SegmentBuilder::new(&schema, &registry).expect("builder");
        for d in documents(&schema) {
            builder.add(&d).expect("valid document");
        }
        let meta = SegmentMeta {
            segment_id: 7,
            created_at: 1_700_000_000_000_000,
            merge_generation: 1,
        };
        // Small blocks: streams span many blocks, so edits reach block boundaries.
        let options = WriteOptions {
            block_size: 512,
            ..WriteOptions::default()
        };
        let bytes = SegmentWriter::new(&builder, meta)
            .with_options(options)
            .finish()
            .expect("segment");
        assert!(Segment::open(ByteSource::new(bytes.clone().into()), &schema).is_ok());

        let n = bytes.len();
        let footer_len = le_u32(&bytes, n - 8) as usize;
        let body = bytes[n - 8 - footer_len..n - 12].to_vec();
        let mut sections = Vec::new();
        let mut framed = Vec::new();
        let mut raw = Vec::new();
        for i in 0..le_u32(&body, 84) as usize {
            let e = entry(&body, i);
            let kind = SectionKind::from_code(le_u16(&body, e)).expect("known kind");
            let off = usize::try_from(le_u64(&body, e + 8)).expect("offset");
            let len = usize::try_from(le_u64(&body, e + 16)).expect("length");
            let section = bytes[off..off + len].to_vec();
            if kind.is_block_framed() {
                let r = BlockReader::open(&section, 0).expect("valid section");
                let mut stream = Vec::new();
                r.read_range(0, r.len(), &mut stream).expect("readable");
                framed.push((i, stream, r.block_size()));
            } else {
                let payload = section[..section.len() - 4].to_vec();
                let map = fst::Map::new(payload).expect("valid FST");
                let mut entries = Vec::new();
                let mut s = fst::IntoStreamer::into_stream(&map);
                while let Some((k, v)) = fst::Streamer::next(&mut s) {
                    entries.push((k.to_vec(), v));
                }
                raw.push((i, entries));
            }
            sections.push(section);
        }
        Self {
            schema,
            body,
            sections,
            framed,
            raw,
        }
    }

    /// The segment with its raw sections (TermDict and IdMap FSTs) edited, each payload CRC recomputed: as FST
    /// entries (key bytes, values, entries added or removed, rebuilt as a valid FST), or as payload bytes. `None` if
    /// no edit changed anything.
    fn edited_raw(&self, edits: &[u8]) -> Option<Vec<u8>> {
        let mut maps: BTreeMap<usize, BTreeMap<Vec<u8>, u64>> = BTreeMap::new();
        let mut bytes: BTreeMap<usize, Vec<(usize, u8)>> = BTreeMap::new();
        for e in edits.as_chunks::<EDIT_LEN>().0.iter().take(MAX_EDITS) {
            let which = usize::from(e[0]) % self.raw.len();
            let entries = maps
                .entry(which)
                .or_insert_with(|| self.raw[which].1.iter().cloned().collect());
            let index = usize::from(e[1]) | usize::from(e[2]) << 8;
            let value = e[5];
            let key = entries.keys().nth(index % entries.len().max(1)).cloned();
            match (e[3] % 6, key) {
                // A value: small (a near-valid ordinal or docid) or with high bits.
                (0, Some(k)) => {
                    entries.insert(k, u64::from(value) | u64::from(e[4]) << 32);
                }
                (1, Some(k)) => {
                    entries.remove(&k);
                }
                // A new key next to an existing one.
                (2, Some(mut k)) => {
                    k.push(value);
                    entries.insert(k, u64::from(e[4]));
                }
                (3, Some(k)) if !k.is_empty() => {
                    let v = entries.remove(&k).unwrap_or(0);
                    let mut k = k;
                    let at = usize::from(e[4]) % k.len();
                    k[at] = value;
                    entries.insert(k, v);
                }
                // Two keys swap values (an IdMap mapping two ids to each other's docids).
                (4, Some(k)) => {
                    let other = entries
                        .keys()
                        .nth(usize::from(e[4]) % entries.len())
                        .cloned();
                    if let Some(o) = other {
                        let (a, b) = (entries[&k], entries[&o]);
                        entries.insert(k, b);
                        entries.insert(o, a);
                    }
                }
                // A payload byte, on top of the rebuilt FST: the FST decoder sees damage behind a valid CRC.
                _ => bytes
                    .entry(which)
                    .or_default()
                    .push((index | usize::from(e[4]) << 16, value)),
            }
        }
        let mut sections = self.sections.clone();
        let mut changed = false;
        for (which, (index, original)) in self.raw.iter().enumerate() {
            let entries = maps.remove(&which);
            let patches = bytes.remove(&which).unwrap_or_default();
            let rebuilt = entries.as_ref().is_some_and(|m| {
                !m.iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .eq(original.iter().cloned())
            });
            if !rebuilt && patches.is_empty() {
                continue;
            }
            let mut payload = match &entries {
                Some(m) if rebuilt => fst::Map::from_iter(m.iter().map(|(k, v)| (k.clone(), *v)))
                    .expect("sorted keys")
                    .into_fst()
                    .into_inner(),
                _ => sections[*index][..sections[*index].len() - 4].to_vec(),
            };
            for (at, value) in patches {
                if !payload.is_empty() {
                    let at = at % payload.len();
                    payload[at] ^= value.max(1);
                }
            }
            let crc = crc32c(&payload);
            payload.extend_from_slice(&crc.to_le_bytes());
            changed |= payload != sections[*index];
            sections[*index] = payload;
        }
        changed.then(|| self.layout(&sections, &[]))
    }

    /// The segment with `edits` applied (see the module docs); `None` if no edit changed anything.
    fn edited(&self, edits: &[u8]) -> Option<Vec<u8>> {
        let mut streams: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
        for e in edits.as_chunks::<EDIT_LEN>().0.iter().take(MAX_EDITS) {
            let which = usize::from(e[0]) % self.framed.len();
            let stream = streams
                .entry(which)
                .or_insert_with(|| self.framed[which].1.clone());
            let offset = usize::from(e[1]) | usize::from(e[2]) << 8 | usize::from(e[3]) << 16;
            let at = offset % (stream.len() + 1);
            let value = e[5];
            match e[4] % 4 {
                0 if at < stream.len() => stream[at] = value,
                1 if at < stream.len() => stream[at] ^= value.max(1),
                2 => stream.truncate(at),
                _ => stream.insert(at, value),
            }
        }
        let mut sections = self.sections.clone();
        let mut changed = false;
        for (which, stream) in streams {
            let (index, original, block_size) = &self.framed[which];
            if stream == *original {
                continue;
            }
            changed = true;
            let mut w = BlockWriter::new(*block_size, Compression::None).expect("block size");
            w.write(&stream).expect("write");
            sections[*index] = w.finish().expect("finish");
        }
        changed.then(|| self.layout(&sections, &[]))
    }

    /// Lays `sections` out (8-aligned, zero padding), then the footer with their offsets and lengths, its CRC and
    /// the locator (SPEC §5.1). `footer_edits` (records of [`EDIT_LEN`] bytes: footer offset `u16` LE, operation,
    /// byte) then change the footer body, offsets and statistics included, before its CRC is computed: every footer
    /// rule of SPEC §5.1 stands alone between them and the reader.
    fn layout(&self, sections: &[Vec<u8>], footer_edits: &[u8]) -> Vec<u8> {
        let pad = |out: &mut Vec<u8>| out.resize(out.len().next_multiple_of(8), 0);
        let mut body = self.body.clone();
        let mut out = Vec::new();
        for (i, s) in sections.iter().enumerate() {
            pad(&mut out);
            let e = entry(&body, i);
            body[e + 8..e + 16].copy_from_slice(&(out.len() as u64).to_le_bytes());
            body[e + 16..e + 24].copy_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(s);
        }
        for e in footer_edits
            .as_chunks::<EDIT_LEN>()
            .0
            .iter()
            .take(MAX_EDITS)
        {
            let at = (usize::from(e[1]) | usize::from(e[2]) << 8) % (body.len() + 1);
            let value = e[4];
            match e[3] % 5 {
                0 if at < body.len() => body[at] = value,
                1 if at < body.len() => body[at] ^= value.max(1),
                // A small signed delta at a u64 field: offsets, lengths and sums stay near-valid.
                2 if at + 8 <= body.len() => {
                    let v = u64::from_le_bytes(body[at..at + 8].try_into().expect("8 bytes"));
                    let v = v.wrapping_add(i64::from(value as i8) as u64);
                    body[at..at + 8].copy_from_slice(&v.to_le_bytes());
                }
                3 => body.truncate(at),
                _ => body.insert(at, value),
            }
        }
        pad(&mut out);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32c(&body).to_le_bytes());
        let footer_len = u32::try_from(body.len() + 4).expect("footer fits");
        out.extend_from_slice(&footer_len.to_le_bytes());
        out.extend_from_slice(b"PSEG");
        out
    }
}

/// Offset of section-table entry `i` in the footer body (SPEC §5.1).
fn entry(body: &[u8], i: usize) -> usize {
    88 + 16 * le_u32(body, 80) as usize + 24 * i
}

fn fixed() -> &'static Fixed {
    static FIXED: OnceLock<Fixed> = OnceLock::new();
    FIXED.get_or_init(Fixed::build)
}

/// Every term of `terms`, with all its postings, then `advance` / `shallow_advance` to a few targets.
fn read_terms(terms: &mut TermIndex<'_>, doc_count: u32) -> Result<(), ReadError> {
    let _ = terms.len();
    let mut found = Vec::new();
    let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
    while let Some(next) = stream.next() {
        let (term, info) = next?;
        found.push((term.to_vec(), info));
    }
    let mut prefix = terms.prefix(b"r");
    while let Some(next) = prefix.next() {
        next?;
    }
    for (term, info) in &found {
        let _ = (info.df(), info.ttf(), info.max_tf(), info.min_norm());
        let _ = terms.get(term);
        let _ = terms.info(info.ordinal());
        let mut postings = terms.postings(info)?;
        let mut read = 0;
        while postings.next()?.is_some() && read < MAX_POSTINGS {
            postings.freq()?;
            let _ = postings.positions()?.len();
            let _ = postings.block_max();
            read += 1;
        }
        let mut postings = terms.postings(info)?;
        for target in [
            0,
            1,
            129,
            doc_count / 2,
            doc_count.saturating_sub(1),
            doc_count,
            u32::MAX,
        ] {
            let _ = postings.shallow_advance(target)?;
            let _ = postings.block_max();
            if postings.advance(target)?.is_some() {
                postings.freq()?;
                let _ = postings.positions()?.len();
            }
        }
    }
    let _ = terms.get(b"no such term");
    let _ = terms.info(terms.len());
    Ok(())
}

/// Reads `seg` completely through the public reader; every error is ignored.
fn read_all(seg: &Segment, schema: &CollectionSchema) {
    // A raw input with valid CRCs may claim any doc count; bound the per-document loops.
    let n = seg.doc_count().min(4096);
    let _ = (seg.segment_id(), seg.id_range(), seg.schema_field_count());
    for field in schema.fields() {
        let fid = field.id();
        let _ = seg.field_stats(fid);
        // Every accessor on every field: the wrong-kind ones are caller errors, also never panics.
        if let Ok(mut terms) = seg.terms(fid) {
            let _ = read_terms(&mut terms, n);
        }
        if let Ok(norms) = seg.norms(fid) {
            for d in 0..=n {
                let _ = (norms.get(d), norms.length(d));
            }
        }
        if let Ok(col) = seg.numeric_column(fid) {
            for d in 0..=n {
                let _ = (col.has_value(d), col.key(d), col.get(d));
            }
        }
        if let Ok(col) = seg.bool_column(fid) {
            let _ = col.trues().len();
            for d in 0..=n {
                let _ = col.get(d);
            }
        }
        if let Ok(col) = seg.keyword_column(fid) {
            for ord in 0..=col.len().min(u64::from(u16::MAX)) {
                if let Some(v) = col.value(ord) {
                    let _ = col.ord(v);
                }
            }
            for d in 0..=n {
                let _ = col.ords(d).map(|o| o.len());
                let _ = col.values(d).map(|v| v.len());
            }
        }
    }
    for d in 0..=n {
        let _ = seg.document(d);
        if let Ok(doc_id) = seg.id_of(d) {
            let _ = seg.docid_of(&doc_id);
        }
        let _ = seg.blob_ref(d);
    }
    let _ = seg.docid_of("no-such-doc");
    // Every whole-segment rule (ROADMAP 5.4.1). Its memory is bounded by the input: open checked that the record
    // arrays hold 8 bytes per document.
    let _ = seg.verify();
    let _ = seg.sections();
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let fixed = fixed();
    let bytes = match mode % 4 {
        0 => rest.to_vec(),
        1 => match fixed.edited(rest) {
            Some(bytes) => bytes,
            None => return,
        },
        2 => match fixed.edited_raw(rest) {
            Some(bytes) => bytes,
            None => return,
        },
        _ => fixed.layout(&fixed.sections, rest),
    };
    if let Ok(seg) = Segment::open(ByteSource::new(bytes.into()), &fixed.schema) {
        read_all(&seg, &fixed.schema);
    }
});
