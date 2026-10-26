//! Segment writer (SPEC §5, ROADMAP 5.1–5.2): every segment written is decoded by a strict, independent decoder
//! (`decode`) and compared with a naive model of its documents (`model`); determinism; the golden segment.
//!
//! The golden file `tests/fixtures/segment/v1.0/basic.pseg` pins the bytes of format 1.0: it is never regenerated
//! once released (SPEC §15); `PERSIA_WRITE_SEGMENT_FIXTURES=1` writes it when it is missing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::single_match_else,
    clippy::explicit_iter_loop,
    clippy::doc_markdown,
    clippy::assert_is_empty,
    missing_docs
)]

mod decode;
mod model;

use std::collections::BTreeMap;
use std::path::PathBuf;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, FieldSet, Value};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldKind, FieldType, KeywordOptions, SchemaBuilder, SchemaHash,
    TextOptions,
};
use persia_engine::segment::{
    MAX_DOCS, SegmentBuilder, SegmentError, SegmentMeta, SegmentWriter, WriteOptions,
};
use persia_format::{BlobId, BlobRef, Reader};
use proptest::prelude::*;

fn schema() -> CollectionSchema {
    SchemaBuilder::new("things")
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
                analyzer: AnalyzerRef::builtin("en").unwrap(),
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
        .unwrap()
}

fn id(schema: &CollectionSchema, name: &str) -> persia_engine::schema::FieldId {
    schema.field(name).unwrap().id()
}

fn meta() -> SegmentMeta {
    SegmentMeta {
        segment_id: 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
        created_at: 1_700_000_000_000_000,
        merge_generation: 2,
    }
}

fn build(schema: &CollectionSchema, docs: &[Document], options: WriteOptions) -> Vec<u8> {
    let mut b = SegmentBuilder::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    for (i, d) in docs.iter().enumerate() {
        assert_eq!(b.add(d).unwrap() as usize, i);
    }
    SegmentWriter::new(&b, meta())
        .with_options(options)
        .finish()
        .unwrap()
}

/// Decodes `bytes` completely and compares every section with the model of `docs`.
fn check(schema: &CollectionSchema, docs: &[Document], bytes: &[u8]) {
    let f = decode::parse(bytes);
    let n = u32::try_from(docs.len()).unwrap();
    assert_eq!(f.format, (1, 0));
    assert_eq!(f.segment_id, meta().segment_id);
    assert_eq!(f.schema_hash, schema.hash().get());
    assert_eq!(f.schema_encoding_version, SchemaHash::ENCODING_VERSION);
    assert_eq!(f.field_count as usize, schema.fields().len());
    assert_eq!(
        (f.doc_count, f.merge_generation, f.created_at),
        (n, 2, meta().created_at)
    );
    let mut ids: Vec<&str> = docs.iter().map(Document::id).collect();
    ids.sort_unstable();
    assert_eq!(
        (f.min_id.as_slice(), f.max_id.as_slice()),
        (ids[0].as_bytes(), ids[ids.len() - 1].as_bytes())
    );

    let registry = AnalyzerRegistry::builtin();
    let mut expected_stats = BTreeMap::new();
    for field in schema.fields() {
        let fid = field.id().get();
        let section = |kind: u16| f.sections.get(&(kind, fid));
        match field.kind() {
            FieldKind::Text(o) => {
                let analyzer = registry
                    .get(o.analyzer.name(), o.analyzer.version())
                    .unwrap();
                let (index, lengths) = model::index(docs, field.id(), Some(analyzer), o.positions);
                expected_stats.insert(
                    fid,
                    (
                        lengths.len() as u32,
                        lengths.values().map(|&l| u64::from(l)).sum(),
                    ),
                );
                let norms = section(7).map(|s| decode::stream(s));
                if let Some(norms) = &norms {
                    let expected: Vec<u8> = (0..n)
                        .map(|d| lengths.get(&d).map_or(0, |&l| model::norm(l)))
                        .collect();
                    assert_eq!(norms, &expected, "norms of {}", field.name());
                } else {
                    assert!(index.terms.is_empty());
                }
                assert_eq!(
                    decode::terms(&f, fid, norms.as_deref(), o.positions),
                    index,
                    "{}",
                    field.name()
                );
                assert!(section(8).is_none());
            }
            FieldKind::Keyword(_) => {
                let (index, lengths) = model::index(docs, field.id(), None, false);
                expected_stats.insert(
                    fid,
                    (
                        lengths.len() as u32,
                        lengths.values().map(|&l| u64::from(l)).sum(),
                    ),
                );
                assert!(section(7).is_none());
                assert_eq!(
                    decode::terms(&f, fid, None, false),
                    index,
                    "{}",
                    field.name()
                );
                let column = section(8)
                    .map(|s| decode::keyword_column(&decode::stream(s), n))
                    .unwrap_or_default();
                let mut expected: BTreeMap<u32, Vec<Vec<u8>>> = BTreeMap::new();
                for (term, occ) in &index.terms {
                    for o in occ {
                        expected.entry(o.doc).or_default().push(term.clone());
                    }
                }
                assert_eq!(column, expected, "column of {}", field.name());
            }
            FieldKind::Stored => assert!((1..=8).all(|k| section(k).is_none())),
            _ => {
                assert!((1..=7).all(|k| section(k).is_none()));
                let expected: BTreeMap<u32, u64> = (0_u32..)
                    .zip(docs)
                    .filter_map(|(d, doc)| {
                        let key = match *doc.get(field.id())? {
                            Value::I64(v) | Value::Datetime(v) => {
                                persia_format::for_codec::i64_to_key(v)
                            }
                            Value::F64(v) => persia_format::for_codec::f64_to_key(v),
                            Value::Bool(v) => u64::from(v),
                            _ => unreachable!(),
                        };
                        Some((d, key))
                    })
                    .collect();
                let column = section(8).map(|s| {
                    let s = decode::stream(s);
                    if field.field_type() == FieldType::Bool {
                        decode::bool_column(&s, n)
                    } else {
                        decode::numeric_column(&s, n)
                    }
                });
                assert_eq!(
                    column.unwrap_or_default(),
                    expected,
                    "column of {}",
                    field.name()
                );
            }
        }
    }
    assert_eq!(f.stats, expected_stats);

    let seg = decode::SEGMENT_FIELD;
    let rows = decode::records(&decode::stream(&f.sections[&(9, seg)]), docs.len());
    for (row, doc) in rows.iter().zip(docs) {
        assert_eq!(row, &doc.encode(schema, FieldSet::Stored).unwrap());
    }
    let id_map = fst::Map::new(decode::raw(&f.sections[&(10, seg)]).to_vec()).unwrap();
    assert_eq!(id_map.len(), docs.len());
    let id_list = decode::records(&decode::stream(&f.sections[&(11, seg)]), docs.len());
    for (d, doc) in (0_u64..).zip(docs) {
        assert_eq!(id_map.get(doc.id()), Some(d));
        assert_eq!(id_list[d as usize], doc.id().as_bytes());
    }
    match f.sections.get(&(12, seg)) {
        None => assert!(docs.iter().all(|d| d.blob_ref().is_none())),
        Some(s) => {
            let blobs = decode::records(&decode::stream(s), docs.len());
            for (rec, doc) in blobs.iter().zip(docs) {
                match doc.blob_ref() {
                    None => assert!(rec.is_empty()),
                    Some(b) => {
                        let mut r = Reader::new(rec);
                        assert_eq!(&BlobRef::decode(&mut r).unwrap(), b);
                        assert!(r.is_empty());
                    }
                }
            }
        }
    }
    let expected_sections = f.sections.keys().filter(|(_, field)| *field == seg).count();
    assert!(expected_sections >= 3);
}

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

fn blob(i: u8) -> BlobRef {
    let meta = BTreeMap::from([
        ("origin".to_owned(), format!("scan-{i}")),
        ("lang".to_owned(), "es".to_owned()),
    ]);
    BlobRef::new(
        BlobId::from_bytes([i; 32]),
        1000 + u64::from(i),
        "application/pdf",
        4 << 20,
        meta,
    )
    .unwrap()
}

prop_compose! {
    fn arb_doc(schema: CollectionSchema)(
        title in proptest::option::of(prop::collection::vec(prop::collection::vec(prop::sample::select(WORDS), 0..6), 0..3)),
        body in proptest::option::of(prop::collection::vec(prop::sample::select(WORDS), 0..10)),
        tags in proptest::option::of(prop::collection::vec(prop::sample::select(TAGS), 0..4)),
        price in proptest::option::of(prop_oneof![Just(0.0), Just(-0.0), -1e6..1e6_f64]),
        qty in proptest::option::of(any::<i64>()),
        at in proptest::option::of(-62_000_000_000_000_000_i64..250_000_000_000_000_000),
        flag in proptest::option::of(any::<bool>()),
        raw in proptest::option::of(any::<u32>()),
        blob_seed in proptest::option::of(any::<u8>()),
        suffix in "[a-z]{0,3}",
    ) -> (Document, String) {
        let mut d = Document::new("");
        if let Some(t) = title {
            let values: Vec<String> = t.iter().map(|w| w.join(" ")).collect();
            d.insert(id(&schema, "title"), if values.len() == 1 { Value::String(values[0].clone()) } else { Value::Strings(values) });
        }
        if let Some(b) = body { d.insert(id(&schema, "body"), Value::String(b.join(" "))); }
        if let Some(t) = tags { d.insert(id(&schema, "tag"), Value::Strings(t.iter().map(|s| (*s).to_owned()).collect())); }
        if let Some(p) = price { d.insert(id(&schema, "price"), Value::F64(p)); }
        if let Some(q) = qty { d.insert(id(&schema, "qty"), Value::I64(q)); }
        if let Some(a) = at { d.insert(id(&schema, "at"), Value::Datetime(a)); }
        if let Some(f) = flag { d.insert(id(&schema, "flag"), Value::Bool(f)); }
        if let Some(r) = raw { d.insert(id(&schema, "raw"), Value::Json(serde_json::json!({ "n": r }))); }
        d.set_blob_ref(blob_seed.map(blob));
        (d, suffix)
    }
}

/// Gives each document a unique id; the suffix varies the id order against the docid order.
fn with_ids(docs: Vec<(Document, String)>) -> Vec<Document> {
    docs.into_iter()
        .enumerate()
        .map(|(i, (d, suffix))| {
            let mut out = Document::new(format!("{suffix}-{i}"));
            for (f, v) in d.fields() {
                out.insert(f, v.clone());
            }
            out.set_blob_ref(d.blob_ref().cloned());
            out
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 256 } else { 24 }, ..ProptestConfig::default() })]

    /// Every section decodes to what the documents hold, at block sizes that make records span blocks.
    #[test]
    fn segment_matches_model(
        docs in prop::collection::vec(arb_doc(schema()), 1..400),
        block_size in prop::sample::select(vec![1_u32, 61, 4096, 16 * 1024]),
    ) {
        let schema = schema();
        let docs = with_ids(docs);
        let bytes = build(&schema, &docs, WriteOptions { block_size, ..WriteOptions::default() });
        check(&schema, &docs, &bytes);
    }
}

#[test]
fn identical_input_gives_identical_bytes() {
    let schema = schema();
    let docs = golden_docs(&schema);
    let a = build(&schema, &docs, WriteOptions::default());
    let b = build(&schema, &docs, WriteOptions::default());
    assert_eq!(a, b);
    let c = build(
        &schema,
        &docs,
        WriteOptions {
            block_size: 512,
            ..WriteOptions::default()
        },
    );
    assert_ne!(a, c);
    check(&schema, &docs, &c);
}

#[test]
fn builder_rejects_bad_documents_without_changing() {
    let schema = schema();
    let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    assert_eq!(
        SegmentWriter::new(&b, meta()).finish(),
        Err(SegmentError::Empty)
    );
    let doc = Document::new("a").with(id(&schema, "tag"), Value::String("x".into()));
    assert_eq!(b.add(&doc), Ok(0));
    let size = b.size_in_bytes();
    assert_eq!(
        b.add(&doc),
        Err(SegmentError::DuplicateId { id: "a".into() })
    );
    let wrong = Document::new("b").with(id(&schema, "qty"), Value::String("x".into()));
    assert!(matches!(b.add(&wrong), Err(SegmentError::Document(_))));
    assert_eq!((b.doc_count(), b.size_in_bytes()), (1, size));
    assert_eq!(b.add(&Document::new("b")), Ok(1));
    let bytes = SegmentWriter::new(&b, meta()).finish().unwrap();
    check(&schema, &[doc, Document::new("b")], &bytes);
    assert_eq!(MAX_DOCS, 1 << 31);
}

#[test]
fn unknown_analyzer_is_an_error() {
    let schema = schema();
    let err = SegmentBuilder::new(&schema, &AnalyzerRegistry::new()).unwrap_err();
    assert!(matches!(err, SegmentError::UnknownAnalyzer { .. }), "{err}");
}

#[test]
fn invalid_block_size_is_an_error() {
    let schema = schema();
    let docs = golden_docs(&schema);
    let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    b.add(&docs[0]).unwrap();
    let err = SegmentWriter::new(&b, meta())
        .with_options(WriteOptions {
            block_size: 0,
            ..WriteOptions::default()
        })
        .finish();
    assert!(matches!(err, Err(SegmentError::Encode(_))));
}

#[test]
fn size_in_bytes_grows_with_documents() {
    let schema = schema();
    let docs = golden_docs(&schema);
    let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    let mut last = b.size_in_bytes();
    let mut grew = 0;
    for d in &docs {
        b.add(d).unwrap();
        let now = b.size_in_bytes();
        assert!(now >= last);
        grew += usize::from(now > last);
        last = now;
    }
    assert!(grew > 10);
    // At least the raw bytes it must hold: rows and ids.
    let rows: usize = docs
        .iter()
        .map(|d| d.encode(&schema, FieldSet::Stored).unwrap().len() + d.id().len())
        .sum();
    assert!(last >= rows, "{last} < {rows}");
}

/// 300 documents exercising every section: full and tail postings blocks (so Skip), positions across values,
/// every column type with gaps, and blob references with `user_meta` (ROADMAP 5.2.6).
fn golden_docs(schema: &CollectionSchema) -> Vec<Document> {
    (0..300_u32)
        .map(|i| {
            let mut d = Document::new(format!("doc-{:03}", (i * 7) % 300));
            if i % 3 != 2 {
                let w = |k: u32| WORDS[((i + k) % WORDS.len() as u32) as usize];
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
                d.set_blob_ref(Some(blob(u8::try_from(i % 256).unwrap())));
            }
            d
        })
        .collect()
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/segment/v1.0/basic.pseg")
}

#[test]
fn golden_segment() {
    let schema = schema();
    let docs = golden_docs(&schema);
    let bytes = build(&schema, &docs, WriteOptions::default());
    check(&schema, &docs, &bytes);
    let path = fixture_path();
    if std::env::var_os("PERSIA_WRITE_SEGMENT_FIXTURES").is_some() && !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();
    }
    let stored = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (PERSIA_WRITE_SEGMENT_FIXTURES=1 writes it)",
            path.display()
        )
    });
    check(&schema, &docs, &stored);
    assert!(
        bytes == stored,
        "segment encoding changed: format 1.0 is pinned by {}",
        path.display()
    );
    let f = decode::parse(&stored);
    assert!(
        f.sections.contains_key(&(5, id(&schema, "title").get())),
        "the golden segment has skip entries"
    );
    assert!(
        f.sections.contains_key(&(12, decode::SEGMENT_FIELD)),
        "and blob references"
    );
}

/// Lists of 127, 128, 129 and 256 documents: the boundaries between a tail block, full blocks and Skip entries.
#[test]
fn block_boundaries() {
    let schema = schema();
    let tag = id(&schema, "tag");
    for n in [1_u32, 127, 128, 129, 256, 257] {
        let docs: Vec<Document> = (0..n)
            .map(|i| Document::new(format!("{i}")).with(tag, Value::String("same".into())))
            .collect();
        let bytes = build(&schema, &docs, WriteOptions::default());
        check(&schema, &docs, &bytes);
        let f = decode::parse(&bytes);
        assert_eq!(
            f.sections.contains_key(&(5, tag.get())),
            n >= 128,
            "{n} docs"
        );
    }
}
