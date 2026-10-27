//! Size edge cases of segments (ROADMAP 5.5.4; SPEC §5, §5.4, §6.4.1): the largest document the limits allow, an
//! empty collection, a single document, and 2^20 documents (with a cheaper ~70 000-document variant).
//!
//! Every segment written here passes `check` (the strict decoder against the model, and `Segment::verify`) and
//! `check_all` (everything read back through the public reader equals the model).
//!
//! Invariants:
//! 1. A document whose encoding is exactly `MAX_DOCUMENT_LEN` bytes is accepted, stored as one record spanning
//!    thousands of blocks, and reads back byte-exact; one byte more is `DocumentError::TooLarge` and leaves the
//!    builder unchanged (SPEC §6.4.1, §5.3.6).
//! 2. One document's term frequency and its positions run are not limited to a 128-value group: a `tf` and a run
//!    longer than 128 round-trip (SPEC §5.3.2, §5.3.3).
//! 3. 512-byte ids (the longest, SPEC §6.4.1) are the footer's `min_id`/`max_id` (SPEC §5.1).
//! 4. No documents is `SegmentError::Empty`, whatever the schema. A schema without indexed fields, or documents
//!    without values, give exactly DocStore, IdMap and IdList (SPEC §5.3 presence rules).
//! 5. One document gives exactly the sections its values call for, footer stats of one document, and postings
//!    iterators that yield docid 0 once, under `next`, `advance` and `shallow_advance` (SPEC §5.3.2).
//! 6. Many documents: lists of thousands of full blocks with a Skip entry each, FOR columns of many 1024-value
//!    blocks, presence and `true` bitmaps past the 65 536-docid roaring container boundary, and the id map, all read
//!    back exactly (SPEC §5.2.1, §5.3.5, §5.4).

use std::collections::BTreeSet;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{
    Document, DocumentError, FieldSet, MAX_DOCUMENT_LEN, MAX_ID_LEN, Value,
};
use persia_engine::schema::{
    CollectionSchema, FieldKind, FieldType, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::segment::{
    BlockMax, SEGMENT_FIELD, SectionKind, Segment, SegmentBuilder, SegmentError, SegmentWriter,
    WriteOptions,
};

use super::read::{expected, open, read_back};
use super::{blob, build, check, id, meta, schema};

// ---------------------------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------------------------

/// `check` (decoder + `verify`), then everything the public reader returns must equal the model.
fn check_all(schema: &CollectionSchema, docs: &[Document], bytes: &[u8]) {
    check(schema, docs, bytes);
    let got = read_back(schema, bytes).unwrap();
    let want = expected(schema, docs);
    assert_eq!(got.terms, want.terms);
    assert_eq!(got.norms, want.norms);
    assert_eq!(got.columns, want.columns);
    assert_eq!(got.keywords, want.keywords);
    assert_eq!(got.ids, want.ids);
    assert_eq!(got.blobs, want.blobs);
    assert!(got.docs == want.docs, "documents differ");

    let seg = open(schema, bytes).unwrap();
    assert_eq!(seg.doc_count() as usize, docs.len());
    let ids: BTreeSet<&str> = docs.iter().map(Document::id).collect();
    assert_eq!(
        seg.id_range(),
        (
            ids.first().unwrap().as_bytes(),
            ids.last().unwrap().as_bytes()
        )
    );
    for field in schema.fields() {
        let stats = seg.field_stats(field.id());
        if matches!(field.field_type(), FieldType::Text | FieldType::Keyword) {
            let stats = stats.unwrap();
            let index = &want.terms[&field.id().get()];
            let with: BTreeSet<u32> = index.terms.values().flatten().map(|o| o.doc).collect();
            let sum: u64 = index
                .terms
                .values()
                .flatten()
                .map(|o| u64::from(o.tf))
                .sum();
            assert_eq!(
                (stats.docs_with_field as usize, stats.sum_field_len),
                (with.len(), sum),
                "stats of {}",
                field.name()
            );
        } else {
            assert_eq!(stats, None, "stats of {}", field.name());
        }
    }
}

/// The `(field, kind)` pairs of the section table, in table order, through the public reader.
fn table(seg: &Segment) -> Vec<(u32, SectionKind)> {
    seg.sections()
        .unwrap()
        .iter()
        .map(|s| (s.field, s.kind))
        .collect()
}

fn stream_blocks(seg: &Segment, kind: SectionKind, field: u32) -> u32 {
    seg.sections()
        .unwrap()
        .iter()
        .find(|s| (s.kind, s.field) == (kind, field))
        .and_then(|s| s.stream)
        .unwrap()
        .block_count
}

const SEGMENT_LEVEL: [(u32, SectionKind); 3] = [
    (SEGMENT_FIELD, SectionKind::DocStore),
    (SEGMENT_FIELD, SectionKind::IdMap),
    (SEGMENT_FIELD, SectionKind::IdList),
];

/// SplitMix64: a fixed, seedable generator, so the generated documents never depend on a crate's version.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 1. The largest document (SPEC §6.4.1, §5.3.6)
// ---------------------------------------------------------------------------------------------------------------

fn large_schema() -> CollectionSchema {
    SchemaBuilder::new("large")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .field("raw", FieldKind::Stored)
        .build()
        .unwrap()
}

/// 4000 tokens: `alpha` every other one (tf 2000, a positions run of 2000), `beta` every fourth, the rest spread
/// over a small vocabulary; then an empty value (base gap only) and a third value.
fn large_title(seed: u64) -> Value {
    const VOCAB: &[&str] = &["gamma", "delta", "eps", "zeta", "eta", "theta", "iota"];
    let mut rng = SplitMix(seed);
    let words: Vec<&str> = (0..4000)
        .map(|i| match i % 4 {
            0 | 2 => "alpha",
            1 => "beta",
            _ => VOCAB[(rng.next() % VOCAB.len() as u64) as usize],
        })
        .collect();
    Value::Strings(vec![
        words.join(" "),
        String::new(),
        "alpha beta omega".to_owned(),
    ])
}

/// A document with the given id whose full encoding is exactly `len` bytes: the stored `raw` string is the filler.
fn document_of_len(schema: &CollectionSchema, doc_id: &str, len: usize) -> Document {
    let make = |filler: usize| {
        Document::new(doc_id)
            .with(id(schema, "title"), large_title(7))
            .with(
                id(schema, "tag"),
                Value::Strings(vec!["big".into(), "x".repeat(300)]),
            )
            .with(
                id(schema, "raw"),
                Value::Json(serde_json::json!({ "pad": "p".repeat(filler) })),
            )
    };
    // Far past 65 535 bytes the string's CBOR head has a fixed width, so the length is linear in the filler.
    let probe = 1 << 20;
    let probe_len = make(probe).encode(schema, FieldSet::All).unwrap().len();
    let doc = make(probe + len - probe_len);
    // `encode` refuses a document over the limit; its error states the length instead.
    let encoded = match doc.encode(schema, FieldSet::All) {
        Ok(bytes) => bytes.len(),
        Err(DocumentError::TooLarge { len }) => len,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(encoded, len);
    doc
}

/// SPEC §6.4.1: "the binary encoding of the whole document" is at most 16 MiB. One at exactly the limit, between
/// two documents with 512-byte ids, at a small block size (thousands of DocStore blocks) and at the default.
#[test]
fn largest_document_spans_many_blocks_and_reads_back() {
    let schema = large_schema();
    let title = id(&schema, "title");
    // The smallest and largest ids are both 512 bytes; '\u{10FFFF}' is 4 bytes of UTF-8.
    let low = "\u{1}".repeat(MAX_ID_LEN);
    let high = "\u{10FFFF}".repeat(MAX_ID_LEN / 4);
    assert_eq!((low.len(), high.len()), (MAX_ID_LEN, MAX_ID_LEN));
    let big = document_of_len(&schema, &"m".repeat(MAX_ID_LEN), MAX_DOCUMENT_LEN);
    // Every field is stored, so the DocStore row is the whole document: a record of exactly the reader's cap.
    assert_eq!(
        big.encode(&schema, FieldSet::Stored).unwrap().len(),
        MAX_DOCUMENT_LEN
    );
    let docs = vec![
        Document::new(high.clone()).with(title, Value::String("alpha".into())),
        big,
        Document::new(low.clone()).with(title, Value::String("omega".into())),
    ];

    // One byte more is rejected, and the builder is as before.
    let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    b.add(&docs[0]).unwrap();
    let size = b.size_in_bytes();
    let too_big = document_of_len(&schema, &"n".repeat(MAX_ID_LEN), MAX_DOCUMENT_LEN + 1);
    assert_eq!(
        b.add(&too_big),
        Err(SegmentError::Document(DocumentError::TooLarge {
            len: MAX_DOCUMENT_LEN + 1
        }))
    );
    assert_eq!((b.doc_count(), b.size_in_bytes()), (1, size));

    for block_size in [4096, WriteOptions::default().block_size] {
        let bytes = build(
            &schema,
            &docs,
            WriteOptions {
                block_size,
                ..WriteOptions::default()
            },
        );
        check_all(&schema, &docs, &bytes);
        let seg = open(&schema, &bytes).unwrap();
        assert_eq!(seg.id_range(), (low.as_bytes(), high.as_bytes()));
        let blocks = stream_blocks(&seg, SectionKind::DocStore, SEGMENT_FIELD);
        assert!(
            u64::from(blocks) * u64::from(block_size) > MAX_DOCUMENT_LEN as u64,
            "{blocks} blocks of {block_size}"
        );
        assert_eq!(seg.document(1).unwrap(), docs[1]);
        assert_eq!(seg.docid_of(&low).unwrap(), Some(2));
        assert_eq!(seg.id_of(0).unwrap(), high);

        // tf above 128 and a positions run above 128 within one document.
        let mut terms = seg.terms(title).unwrap();
        let info = terms.get(b"alpha").unwrap().unwrap();
        assert_eq!((info.df(), info.max_tf()), (2, 2001));
        let mut p = terms.postings(&info).unwrap();
        assert_eq!(p.advance(1).unwrap(), Some(1));
        assert_eq!(p.freq().unwrap(), 2001);
        let pos = p.positions().unwrap().to_vec();
        let mut want: Vec<u32> = (0..4000).filter(|i| i % 2 == 0).collect();
        // Value 1 is empty (end 0): value 2's base is 4000 + 100 + 0 + 100.
        want.push(4000 + 100 + 100);
        assert_eq!(pos, want);
        assert_eq!(p.next().unwrap(), None);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 2. Empty collection (SPEC §5.3 presence, §5.4)
// ---------------------------------------------------------------------------------------------------------------

fn no_fields(strict: bool) -> CollectionSchema {
    SchemaBuilder::new("bare").strict(strict).build().unwrap()
}

fn stored_only() -> CollectionSchema {
    SchemaBuilder::new("stored")
        .field("a", FieldKind::Stored)
        .field("b", FieldKind::Stored)
        .build()
        .unwrap()
}

#[test]
fn no_documents_is_empty_for_every_schema() {
    for schema in [schema(), large_schema(), no_fields(true), stored_only()] {
        let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        assert_eq!(
            SegmentWriter::new(&b, meta()).finish(),
            Err(SegmentError::Empty)
        );
        // A rejected document does not make it non-empty.
        assert!(b.add(&Document::new("")).is_err());
        assert_eq!(
            SegmentWriter::new(&b, meta())
                .with_options(WriteOptions {
                    block_size: 1,
                    ..WriteOptions::default()
                })
                .finish(),
            Err(SegmentError::Empty)
        );
    }
}

/// Schemas with nothing to index: only the three segment-level sections, no field statistics.
#[test]
fn schema_without_indexed_fields_has_only_segment_sections() {
    let lax = no_fields(false);
    let ids_only: Vec<Document> = (0..300).map(|i| Document::new(format!("i{i}"))).collect();
    let extras: Vec<Document> = (0..300_u32)
        .map(|i| {
            let mut d = Document::new(format!("e{i}"));
            if i % 3 != 0 {
                d.insert_extra("note", serde_json::json!({ "i": i, "s": ["x", null] }));
            }
            d
        })
        .collect();
    let stored = stored_only();
    let (a, b) = (id(&stored, "a"), id(&stored, "b"));
    let stored_docs: Vec<Document> = (0..300_u32)
        .map(|i| {
            let mut d = Document::new(format!("s{i}"));
            if i % 2 == 0 {
                d.insert(a, Value::Json(serde_json::json!(i)));
            }
            if i % 5 == 0 {
                d.insert(b, Value::Json(serde_json::json!("ü".repeat(i as usize))));
            }
            d
        })
        .collect();
    let cases = [
        (no_fields(true), ids_only.clone()),
        (no_fields(true), ids_only[..1].to_vec()),
        (lax.clone(), extras),
        (stored, stored_docs),
    ];
    for (schema, docs) in &cases {
        for block_size in [1, 61, 16 * 1024] {
            let bytes = build(
                schema,
                docs,
                WriteOptions {
                    block_size,
                    ..WriteOptions::default()
                },
            );
            check_all(schema, docs, &bytes);
            let seg = open(schema, &bytes).unwrap();
            assert_eq!(table(&seg), SEGMENT_LEVEL, "{}", schema.name());
        }
    }
}

/// Documents with no field values at all, under the full schema: nothing but the segment-level sections; and next
/// to documents that do have values, they are absent everywhere (norm 0, no column value, no postings).
#[test]
fn documents_without_values() {
    let schema = schema();
    let empty: Vec<Document> = (0..200).map(|i| Document::new(format!("{i:03}"))).collect();
    let bytes = build(&schema, &empty, WriteOptions::default());
    check_all(&schema, &empty, &bytes);
    let seg = open(&schema, &bytes).unwrap();
    assert_eq!(table(&seg), SEGMENT_LEVEL);
    // Text and keyword fields still have statistics, of zero.
    let title = seg.field_stats(id(&schema, "title")).unwrap();
    assert_eq!((title.docs_with_field, title.sum_field_len), (0, 0));
    assert!(seg.terms(id(&schema, "title")).unwrap().is_empty());
    assert_eq!(seg.norms(id(&schema, "title")).unwrap().get(199), 0);
    assert_eq!(
        seg.numeric_column(id(&schema, "qty"))
            .unwrap()
            .key(0)
            .unwrap(),
        None
    );
    assert_eq!(seg.bool_column(id(&schema, "flag")).unwrap().get(0), None);

    // One document with every value among valueless ones, first and last in docid order.
    let mut docs = empty.clone();
    docs[0] = full_document(&schema, "000");
    docs[199] = full_document(&schema, "199");
    let bytes = build(&schema, &docs, WriteOptions::default());
    check_all(&schema, &docs, &bytes);
}

// ---------------------------------------------------------------------------------------------------------------
// 3. A single document (SPEC §5.1, §5.3)
// ---------------------------------------------------------------------------------------------------------------

/// A value in every field of `schema()`, and a blob.
fn full_document(schema: &CollectionSchema, doc_id: &str) -> Document {
    let mut d = Document::new(doc_id)
        .with(
            id(schema, "title"),
            Value::Strings(vec!["The quick fox".into(), "fox fox".into()]),
        )
        .with(id(schema, "body"), Value::String("running runs".into()))
        .with(
            id(schema, "tag"),
            Value::Strings(vec!["red".into(), String::new(), "red".into()]),
        )
        .with(id(schema, "price"), Value::F64(-0.5))
        .with(id(schema, "qty"), Value::I64(i64::MIN))
        .with(id(schema, "at"), Value::Datetime(0))
        .with(id(schema, "flag"), Value::Bool(false))
        .with(id(schema, "raw"), Value::Json(serde_json::json!([1, "2"])));
    d.set_blob_ref(Some(blob(9)));
    d
}

/// Every postings iterator of a one-document segment: docid 0 once, under each way of moving.
fn assert_single_postings(seg: &Segment, field: persia_engine::schema::FieldId) {
    let terms = seg.terms(field).unwrap();
    assert!(!terms.is_empty());
    let mut stream = terms.range(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded);
    let mut infos = Vec::new();
    while let Some(next) = stream.next() {
        let (term, info) = next.unwrap();
        infos.push((term.to_vec(), info));
    }
    for (term, info) in infos {
        let at = String::from_utf8_lossy(&term).into_owned();
        assert_eq!(
            (info.df(), u64::from(info.max_tf())),
            (1, info.ttf()),
            "{at}"
        );
        let bound = BlockMax {
            last_docid: 0,
            max_tf: info.max_tf(),
            min_norm: info.min_norm(),
        };

        let mut p = terms.postings(&info).unwrap();
        assert_eq!((p.doc(), p.freq().unwrap(), p.block_max()), (None, 0, None));
        assert!(p.positions().unwrap().is_empty());
        assert_eq!(p.next().unwrap(), Some(0), "{at}");
        assert_eq!(p.doc(), Some(0));
        assert_eq!(p.freq().unwrap(), info.max_tf());
        assert_eq!(p.block_max(), Some(bound));
        assert_eq!(p.next().unwrap(), None);
        assert_eq!((p.doc(), p.next().unwrap()), (None, None));

        let mut p = terms.postings(&info).unwrap();
        assert_eq!(p.advance(0).unwrap(), Some(0), "{at}");
        assert_eq!(p.advance(0).unwrap(), Some(0), "never moves back");
        assert_eq!(p.advance(1).unwrap(), None);
        assert_eq!(p.advance(0).unwrap(), None, "exhausted stays exhausted");

        let mut p = terms.postings(&info).unwrap();
        assert_eq!(p.advance(u32::MAX).unwrap(), None);

        let mut p = terms.postings(&info).unwrap();
        assert_eq!(p.shallow_advance(0).unwrap(), Some(bound), "{at}");
        assert_eq!(p.doc(), None, "shallow_advance does not move");
        assert_eq!(p.shallow_advance(1).unwrap(), None);
        assert_eq!(p.shallow_advance(u32::MAX).unwrap(), None);
        assert_eq!(
            p.shallow_advance(0).unwrap(),
            Some(bound),
            "a falling target"
        );
        assert_eq!(p.next().unwrap(), Some(0));
        assert_eq!(p.shallow_advance(0).unwrap(), Some(bound));
    }
}

#[test]
fn single_document_with_every_field_type() {
    use SectionKind::{
        BlobRefs, Columns, DocStore, Freqs, IdList, IdMap, Norms, Positions, Postings, TermDict,
        TermInfo,
    };
    let schema = schema();
    let docs = vec![full_document(&schema, "only")];
    for block_size in [1, 16 * 1024] {
        let bytes = build(
            &schema,
            &docs,
            WriteOptions {
                block_size,
                ..WriteOptions::default()
            },
        );
        check_all(&schema, &docs, &bytes);
        let seg = open(&schema, &bytes).unwrap();
        assert_eq!(seg.id_range(), (&b"only"[..], &b"only"[..]));

        let f = |name: &str| id(&schema, name).get();
        let mut want = vec![
            (f("title"), TermDict),
            (f("title"), TermInfo),
            (f("title"), Postings),
            (f("title"), Freqs),
            (f("title"), Positions),
            (f("title"), Norms),
            (f("body"), TermDict),
            (f("body"), TermInfo),
            (f("body"), Postings),
            (f("body"), Freqs),
            (f("body"), Norms),
            (f("tag"), TermDict),
            (f("tag"), TermInfo),
            (f("tag"), Postings),
            (f("tag"), Freqs),
            (f("tag"), Columns),
            (f("price"), Columns),
            (f("qty"), Columns),
            (f("at"), Columns),
            (f("flag"), Columns),
            (SEGMENT_FIELD, DocStore),
            (SEGMENT_FIELD, IdMap),
            (SEGMENT_FIELD, IdList),
            (SEGMENT_FIELD, BlobRefs),
        ];
        want.sort_by_key(|&(field, kind)| (field, kind.code()));
        // One document is one tail block: no list reaches 128, so no Skip.
        assert_eq!(table(&seg), want);

        // "The quick fox" + "fox fox": 5 tokens; the tag list ["red", "", "red"]: 3 values.
        let stats = |name: &str| {
            let s = seg.field_stats(id(&schema, name)).unwrap();
            (s.docs_with_field, s.sum_field_len)
        };
        assert_eq!(stats("title"), (1, 5));
        assert_eq!(stats("tag"), (1, 3));
        assert_eq!(stats("body"), (1, 2));
        assert_eq!(seg.field_stats(id(&schema, "qty")), None);

        let mut terms = seg.terms(id(&schema, "title")).unwrap();
        let fox = terms.get(b"fox").unwrap().unwrap();
        let mut p = terms.postings(&fox).unwrap();
        assert_eq!(p.next().unwrap(), Some(0));
        // "fox" at 2 in value 0 (end 3), then at 0 and 1 of value 1, whose base is 3 + 100.
        assert_eq!(p.positions().unwrap(), &[2, 103, 104]);
        for name in ["title", "body", "tag"] {
            assert_single_postings(&seg, id(&schema, name));
        }
    }
}

/// A one-field schema of each field type, one document with a value (and one without any field: no field sections).
#[test]
fn single_document_per_field_type() {
    let kinds: Vec<(FieldKind, Value, Vec<SectionKind>)> = {
        use SectionKind::{Columns, Freqs, Norms, Positions, Postings, TermDict, TermInfo};
        vec![
            (
                FieldKind::Text(TextOptions::default()),
                Value::String("one two two".into()),
                vec![TermDict, TermInfo, Postings, Freqs, Norms],
            ),
            (
                FieldKind::Text(TextOptions {
                    positions: true,
                    stored: false,
                    ..TextOptions::default()
                }),
                Value::Strings(vec!["one".into(), "two two".into()]),
                vec![TermDict, TermInfo, Postings, Freqs, Positions, Norms],
            ),
            (
                FieldKind::Keyword(KeywordOptions::default()),
                Value::String(String::new()),
                vec![TermDict, TermInfo, Postings, Freqs, Columns],
            ),
            (
                FieldKind::I64 { stored: true },
                Value::I64(i64::MAX),
                vec![Columns],
            ),
            (
                FieldKind::F64 { stored: false },
                Value::F64(f64::MIN),
                vec![Columns],
            ),
            (
                FieldKind::Bool { stored: true },
                Value::Bool(true),
                vec![Columns],
            ),
            (
                FieldKind::Datetime { stored: true },
                Value::Datetime(persia_engine::document::MIN_DATETIME),
                vec![Columns],
            ),
            (
                FieldKind::Stored,
                Value::Json(serde_json::json!({})),
                vec![],
            ),
        ]
    };
    for (kind, value, sections) in kinds {
        let schema = SchemaBuilder::new("one")
            .field("f", kind.clone())
            .build()
            .unwrap();
        let f = id(&schema, "f");
        for doc in [
            Document::new("x").with(f, value.clone()),
            Document::new("x"),
        ] {
            let has_value = doc.get(f).is_some();
            let docs = vec![doc];
            let bytes = build(&schema, &docs, WriteOptions::default());
            check_all(&schema, &docs, &bytes);
            let seg = open(&schema, &bytes).unwrap();
            let mut want: Vec<(u32, SectionKind)> = if has_value {
                sections.iter().map(|&k| (f.get(), k)).collect()
            } else {
                Vec::new()
            };
            want.extend(SEGMENT_LEVEL);
            want.sort_by_key(|&(field, kind)| (field, kind.code()));
            assert_eq!(table(&seg), want, "{kind:?}, value: {has_value}");
            if has_value && matches!(kind, FieldKind::Text(_) | FieldKind::Keyword(_)) {
                assert_single_postings(&seg, f);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Many documents (SPEC §5.2.1, §5.3.2, §5.3.5, §5.4)
// ---------------------------------------------------------------------------------------------------------------

fn tiny_schema() -> CollectionSchema {
    SchemaBuilder::new("tiny")
        .field(
            "tag",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .field("n", FieldKind::I64 { stored: true })
        .field("flag", FieldKind::Bool { stored: false })
        .build()
        .unwrap()
}

const SMALL_SET: [&str; 5] = ["k0", "k1", "k2", "k3", "k4"];

/// `n` tiny documents: every one has the keyword `all` plus one of a small set (and `sparse` every 1000th); an i64
/// missing every 3rd (so a presence bitmap), with bit widths that vary between FOR blocks; a bool missing every
/// 5th. Ids are a bijection of the docid (an odd multiplier mod 2^32), so id order is not docid order.
fn tiny_docs(schema: &CollectionSchema, n: u32, seed: u64) -> Vec<Document> {
    let (tag, num, flag) = (id(schema, "tag"), id(schema, "n"), id(schema, "flag"));
    let mut rng = SplitMix(seed);
    (0..n)
        .map(|i| {
            let mut tags = vec!["all".to_owned(), SMALL_SET[(i % 5) as usize].to_owned()];
            if i % 1000 == 0 {
                tags.push("sparse".to_owned());
            }
            let mut d = Document::new(format!("{:08x}", i.wrapping_mul(0x9E37_79B1)))
                .with(tag, Value::Strings(tags));
            if i % 3 != 0 {
                // The width grows with the FOR block, so blocks differ in bit width (0 bits for block 0's tail).
                let width = (i / 1024) % 64;
                let v = rng.next() & ((1_u64 << width) - 1);
                d.insert(num, Value::I64(v.cast_signed() - (1 << 20)));
            }
            if i % 5 != 0 {
                d.insert(flag, Value::Bool(rng.next() & 1 == 0));
            }
            d
        })
        .collect()
}

fn check_many(n: u32) {
    let schema = tiny_schema();
    let docs = tiny_docs(&schema, n, 0x5EED_0000 + u64::from(n));
    let bytes = build(&schema, &docs, WriteOptions::default());
    check_all(&schema, &docs, &bytes);

    let seg = open(&schema, &bytes).unwrap();
    assert_eq!(seg.doc_count(), n);
    let tag = id(&schema, "tag");
    // Skip: one entry per block of each list with df >= 128.
    let mut terms = seg.terms(tag).unwrap();
    let all = terms.get(b"all").unwrap().unwrap();
    assert_eq!((all.df(), all.ttf()), (n, u64::from(n)));

    // A dense list: advance by strides that land on, before and after block boundaries; shallow bounds agree.
    let mut p = terms.postings(&all).unwrap();
    let mut target = 0_u32;
    let mut stride = 1_u32;
    while target < n {
        let m = p.shallow_advance(target).unwrap().unwrap();
        assert_eq!(
            m.last_docid,
            (target / 128 * 128 + 127).min(n - 1),
            "target {target}"
        );
        assert_eq!(m.max_tf, 1);
        assert_eq!(p.advance(target).unwrap(), Some(target));
        assert_eq!(p.freq().unwrap(), 1);
        target += stride;
        stride = stride * 3 % 1021 + 1;
    }
    assert_eq!(p.advance(n).unwrap(), None);

    // A list of every 5th docid, walked with next.
    let k3 = terms.get(b"k3").unwrap().unwrap();
    let mut p = terms.postings(&k3).unwrap();
    let mut want = (0..n).filter(|i| i % 5 == 3);
    while let Some(d) = p.next().unwrap() {
        assert_eq!(Some(d), want.next());
    }
    assert_eq!(want.next(), None);

    // Columns past the roaring container boundary (65 536).
    let flags = seg.bool_column(id(&schema, "flag")).unwrap();
    let nums = seg.numeric_column(id(&schema, "n")).unwrap();
    for d in [65_535, 65_536, 65_537, n - 1] {
        assert_eq!(flags.get(d).is_some(), d % 5 != 0, "docid {d}");
        assert_eq!(nums.has_value(d), d % 3 != 0, "docid {d}");
        assert_eq!(
            nums.get(d).unwrap().as_ref(),
            docs[d as usize].get(id(&schema, "n"))
        );
    }
    let trues_high = flags.trues().iter().filter(|&d| d >= 65_536).count();
    assert!(trues_high > 0);

    // The id map both ways, for every docid.
    for (d, doc) in (0_u32..).zip(&docs) {
        assert_eq!(seg.docid_of(doc.id()).unwrap(), Some(d));
    }
}

/// Crosses 65 536 docids (a second roaring container in the presence and `true` bitmaps), 69 FOR blocks per
/// column, 547 Skip entries for `all`.
#[test]
fn seventy_thousand_documents() {
    check_many(70_000);
}

/// 2^20 documents: 8192 full postings blocks for `all`, 1024 FOR blocks per column, 16 roaring containers.
#[cfg(feature = "slow")]
#[test]
fn two_to_the_twenty_documents() {
    check_many(1 << 20);
}
