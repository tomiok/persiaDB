//! A sorted search that reaches a column key no document can hold (SPEC §5.2.1, §6.4.1, §7.4; found in the review of
//! ROADMAP 10.1.29–10.1.31).
//!
//! Invariants:
//! 1. A segment whose `datetime` column holds a key out of range, or
//!    whose `f64` column holds a NaN, behind valid CRCs (only `verify` checked these before), fails a search sorted by
//!    that column as a corruption of the column, naming the field and the docid, plain and relaxed: the value is
//!    never stated as a hit's sort value or put in a cursor.
//! 2. The same segment sorted by another column, or by score, still answers. (A `bool` column is a bitmap, SPEC §5.3.5:
//!    its keys are 0 or 1 by construction; the check of its keys is a unit test of `search::sort`.)

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    missing_docs
)] // Tests: inputs are fixed.

use persia_analysis::AnalyzerRegistry;
use persia_engine::db::{Database, DatabaseOptions};
use persia_engine::document::{Document, MAX_DATETIME, Value};
use persia_engine::manifest::{
    CollectionDiff, Location, Manifest, ManifestDiff, SegmentEntry, SegmentId,
};
use persia_engine::relax::RelaxOptions;
use persia_engine::schema::{CollectionSchema, FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::request::{QueryRequest, QueryRequestError, RequestError, RunOptions};
use persia_engine::search::{Order, Sort, SortKey};
use persia_engine::segment::{
    ReadError, SectionKind, SegmentBuilder, SegmentCorruption, SegmentMeta, SegmentWriter,
};
use persia_format::block::Compression;
use persia_format::for_codec::{f64_to_key, i64_to_key};
use persia_format::frame::FrameKind;
use persia_format::reader::Reader;
use persia_format::writer::Writer;
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Commit, Container};

#[path = "support/split.rs"]
mod split;

const DOCS: usize = 4;

fn schema() -> CollectionSchema {
    SchemaBuilder::new("a")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("flag", FieldKind::Bool { stored: true })
        .field("at", FieldKind::Datetime { stored: true })
        .field("price", FieldKind::F64 { stored: true })
        .field("qty", FieldKind::I64 { stored: true })
        .build()
        .unwrap()
}

fn field(name: &str) -> persia_engine::schema::FieldId {
    schema().field(name).unwrap().id()
}

/// The bytes of a segment of `DOCS` documents, every one with every field.
fn segment_bytes() -> Vec<u8> {
    let s = schema();
    let registry = AnalyzerRegistry::builtin();
    let mut b = SegmentBuilder::new(&s, &registry).unwrap();
    for i in 0..DOCS {
        let n = i64::try_from(i).unwrap();
        let doc = Document::new(format!("d{i}"))
            .with(field("title"), Value::String("red shoes".into()))
            .with(field("flag"), Value::Bool(i % 2 == 0))
            .with(field("at"), Value::Datetime(n * 1_000_000))
            .with(
                field("price"),
                Value::F64(10.5 * f64::from(u32::try_from(i).unwrap() + 1)),
            )
            .with(field("qty"), Value::I64(n));
        b.add(&doc).unwrap();
    }
    let meta = SegmentMeta {
        segment_id: 1,
        created_at: 0,
        merge_generation: 0,
    };
    SegmentWriter::new(&b, meta).finish().unwrap()
}

/// A FOR column of `values` in blocks of 1024 (SPEC §5.2.1).
fn write_for(w: &mut Writer, values: &[u64]) {
    w.write_u64(values.len() as u64);
    let mut data = Writer::new();
    let mut ends = Vec::new();
    for block in values.chunks(1024) {
        data.write_for_block(block).unwrap();
        ends.push(data.len() as u64);
    }
    for e in ends {
        w.write_u64(e);
    }
    w.write_bytes(data.as_slice());
}

fn read_for(r: &mut Reader<'_>) -> Vec<u64> {
    let count = r.read_u64().unwrap() as usize;
    let ends: Vec<usize> = (0..count.div_ceil(1024))
        .map(|_| r.read_u64().unwrap() as usize)
        .collect();
    let data = r.read_bytes(ends.last().copied().unwrap_or(0)).unwrap();
    let mut values = Vec::new();
    let mut start = 0;
    for end in ends {
        values.extend(
            Reader::new(&data[start..end])
                .read_for_block()
                .unwrap()
                .iter(),
        );
        start = end;
    }
    values
}

/// The segment with doc `doc`'s key in the column of `name` set to `key`, every CRC valid.
fn with_key(name: &str, doc: usize, key: u64) -> Vec<u8> {
    let bytes = segment_bytes();
    let s = split::Split::new(&bytes);
    let id = field(name).get();
    let which = s
        .framed
        .iter()
        .position(|(i, kind, _, _)| {
            *kind == SectionKind::Columns
                && split::le_u32(&s.body, split::table_entry(&s.body, *i) + 4) == id
        })
        .unwrap();
    let stream = &s.framed[which].2;
    let mut r = Reader::new(stream);
    r.skip(8).unwrap();
    let presence_len = r.read_u64().unwrap();
    assert_eq!(presence_len, 0, "{name}: a dense column");
    let mut keys = read_for(&mut r);
    assert_eq!(keys.len(), DOCS);
    keys[doc] = key;
    let mut w = Writer::new();
    w.write_bytes(&stream[..8]);
    w.write_u64(0);
    write_for(&mut w, &keys);
    s.with_stream(which, w.as_slice())
}

/// A database whose collection `a` is the one segment `bytes`.
fn database(bytes: &[u8]) -> Database<SimFile> {
    let file = SimFile::new();
    let mut c = Container::create_on(file.clone(), [8; 16]).unwrap();
    let f = c
        .append(FrameKind::Segment, bytes, Compression::None)
        .unwrap();
    let entry = SegmentEntry {
        id: SegmentId::new(1),
        doc_count: DOCS as u32,
        size: bytes.len() as u64,
        location: Location::Frame {
            offset: f.offset,
            len: f.len,
        },
        tombstones: None,
    };
    c.sync().unwrap();
    let mut diff = ManifestDiff {
        wal_start: Some(c.end()),
        ..ManifestDiff::default()
    };
    diff.collections.insert(
        "a".into(),
        CollectionDiff::Create {
            schema: schema(),
            segments: vec![entry],
        },
    );
    let next = Manifest::empty().apply(&diff).unwrap();
    let wal_start = c.end();
    let m = c
        .append(
            FrameKind::Manifest,
            &next.encode().unwrap(),
            Compression::None,
        )
        .unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start,
        fence_token: next.fence_token(),
    })
    .unwrap();
    drop(c);
    Database::open_on(file, DatabaseOptions::default()).unwrap()
}

/// The search of `red` sorted by `by` (then by score), plain or relaxed.
fn search(db: &Database<SimFile>, by: &str, relax: bool) -> Result<usize, QueryRequestError> {
    let snapshot = db.snapshot();
    let c = snapshot.collection("a").unwrap();
    let sort = Sort::new(
        [SortKey::field(field(by), Order::Asc), SortKey::score()],
        &schema(),
    )
    .unwrap();
    let mut request = QueryRequest::new("red").with_sort(sort);
    if !relax {
        request = request.with_relax(RelaxOptions::disabled());
    }
    let response = request.run(c, &RunOptions::default())?;
    Ok(response.to_json(c.schema()).unwrap().len())
}

#[test]
fn a_column_key_no_document_can_hold_fails_a_sorted_search_as_corruption() {
    // The undamaged segment answers every sort.
    let good = database(&segment_bytes());
    for by in ["flag", "at", "price", "qty"] {
        for relax in [false, true] {
            search(&good, by, relax).unwrap();
        }
    }
    let cases = [
        (
            "at",
            i64_to_key(MAX_DATETIME + 1),
            "a datetime column key out of range",
        ),
        (
            "price",
            f64_to_key(f64::NAN),
            "an f64 column key that is not a finite number other than -0.0",
        ),
    ];
    for (name, key, expected) in cases {
        let db = database(&with_key(name, 1, key));
        for relax in [false, true] {
            let err = search(&db, name, relax).unwrap_err();
            assert!(
                matches!(
                    &err,
                    QueryRequestError::Request(RequestError::Read(e @ ReadError::Section {
                        kind: SectionKind::Columns,
                        field,
                        position: 1,
                        reason: SegmentCorruption::Invalid { what },
                    })) if e.is_corruption() && *field == schema().field(name).unwrap().id().get() && *what == expected
                ),
                "{name} relax {relax}: {err:?}"
            );
        }
        // Invariant 2: another column, and the score, still answer.
        let other = if name == "qty" { "flag" } else { "qty" };
        search(&db, other, false).unwrap();
    }
}
