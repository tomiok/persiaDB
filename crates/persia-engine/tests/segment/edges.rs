//! Edge cases of the segment writer that the random model test reaches rarely or never: the footer's fixed layout
//! and locator (SPEC §5.1), presence rules (SPEC §5.3), positions across empty values (SPEC §5.3.3), keyword lists
//! (SPEC §5.3.5), numeric extremes and multi-block columns (SPEC §5.2.1, §5.3.5), builder atomicity and determinism
//! (ROADMAP 5.1, 5.2).
//!
//! Invariants:
//! 1. The footer's fixed part is 88 bytes at fixed offsets; `footer_len` covers fixed part, stats, table, ids and
//!    CRC; the CRC covers every footer byte before it; the footer is 8-aligned and preceded only by zero padding.
//! 2. Field statistics list every `text`/`keyword` field, zeros included, and nothing else.
//! 3. The section table is exactly the sections the presence rules call for, ascending `(field_id, kind)`, the
//!    segment-level ones (`0xFFFF_FFFF`) last.
//! 4. A value without tokens still moves the next value's base by `POSITION_GAP`.
//! 5. A keyword list's duplicates are one posting with `tf` = repetitions and one ord; `""` is a value; `[]` is
//!    missing in the column and in the statistics.
//! 6. Numeric keys round-trip at the type extremes, across FOR blocks; presence is length 0 iff every document has
//!    a value; an absent document stores its FOR block's minimum.
//! 7. A rejected document leaves the builder (bytes written and `size_in_bytes`) as if it had never been tried.
//! 8. Output depends only on the documents, meta and options: separate builders, repeated `finish`, `finish`
//!    between `add`s.

use std::collections::BTreeMap;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{
    Document, MAX_DATETIME, MAX_ID_LEN, MAX_KEYWORD_LEN, MIN_DATETIME, Value,
};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldKind, KeywordOptions, MAX_FIELDS, SchemaBuilder,
    TextOptions,
};
use persia_engine::segment::{
    POSITION_GAP, SegmentBuilder, SegmentError, SegmentMeta, SegmentWriter, WriteOptions,
};
use persia_format::checksum::crc32c;
use persia_format::for_codec::{f64_to_key, i64_to_key};
use proptest::prelude::*;

use super::model::Occurrence;
use super::{blob, build, check, decode, golden_docs, id, meta, schema};

const SEG: u32 = decode::SEGMENT_FIELD;

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn finish(b: &SegmentBuilder) -> Vec<u8> {
    SegmentWriter::new(b, meta()).finish().unwrap()
}

fn builder(schema: &CollectionSchema) -> SegmentBuilder {
    SegmentBuilder::new(schema, &AnalyzerRegistry::builtin()).unwrap()
}

/// The `(field, kind)` keys of the section table, in table order.
fn table(bytes: &[u8]) -> Vec<(u32, u16)> {
    let f = decode::parse(bytes);
    let mut keys: Vec<(u32, u16)> = f.sections.keys().map(|&(k, field)| (field, k)).collect();
    keys.sort_unstable();
    keys
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Footer layout (SPEC §5.1)
// ---------------------------------------------------------------------------------------------------------------

/// Reads the footer by fixed byte offsets (not by a sequential reader), so a field written in the wrong place or
/// with the wrong width fails here even if the decoder's read order happened to match the writer's.
#[test]
fn footer_fixed_part_is_88_bytes_at_spec_offsets() {
    let schema = schema();
    let docs = golden_docs(&schema);
    let meta = SegmentMeta {
        segment_id: u128::MAX - 1,
        created_at: i64::MIN,
        merge_generation: u32::MAX,
    };
    let mut b = builder(&schema);
    for d in &docs {
        b.add(d).unwrap();
    }
    let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
    let n = bytes.len();

    // Locator.
    assert_eq!(&bytes[n - 4..], b"PSEG");
    let footer_len = le_u32(&bytes, n - 8) as usize;
    let start = n - 8 - footer_len;
    assert_eq!(start % 8, 0);
    let footer = &bytes[start..n - 8];

    assert_eq!((le_u16(footer, 0), le_u16(footer, 2)), (1, 0));
    assert_eq!(le_u32(footer, 4), 0, "reserved");
    assert_eq!(le_u64(footer, 8), 0, "incompat_flags");
    assert_eq!(le_u64(footer, 16), 0, "rocompat_flags");
    assert_eq!(
        u128::from_le_bytes(footer[24..40].try_into().unwrap()),
        u128::MAX - 1
    );
    assert_eq!(
        u128::from_le_bytes(footer[40..56].try_into().unwrap()),
        schema.hash().get()
    );
    assert_eq!(
        footer[56],
        persia_engine::schema::SchemaHash::ENCODING_VERSION
    );
    assert_eq!(&footer[57..60], &[0, 0, 0]);
    assert_eq!(le_u32(footer, 60) as usize, schema.fields().len());
    assert_eq!(le_u32(footer, 64) as usize, docs.len());
    assert_eq!(le_u32(footer, 68), u32::MAX);
    assert_eq!(
        i64::from_le_bytes(footer[72..80].try_into().unwrap()),
        i64::MIN
    );
    let stats = le_u32(footer, 80) as usize;
    let sections = le_u32(footer, 84) as usize;

    // Exactly fixed part + stats + table + two ids + CRC.
    let ids_at = 88 + 16 * stats + 24 * sections;
    let min_len = usize::from(le_u16(footer, ids_at));
    let max_at = ids_at + 2 + min_len;
    let max_len = usize::from(le_u16(footer, max_at));
    assert_eq!(footer_len, max_at + 2 + max_len + 4);
    let mut ids: Vec<&str> = docs.iter().map(Document::id).collect();
    ids.sort_unstable();
    assert_eq!(&footer[ids_at + 2..max_at], ids[0].as_bytes());
    assert_eq!(
        &footer[max_at + 2..max_at + 2 + max_len],
        ids[ids.len() - 1].as_bytes()
    );

    // The CRC covers every footer byte before it.
    assert_eq!(
        crc32c(&footer[..footer_len - 4]),
        le_u32(footer, footer_len - 4)
    );

    // Every section and the footer lie within the bytes before the footer, and nothing else is there but padding.
    let mut covered = vec![false; start];
    for i in 0..sections {
        let e = 88 + 16 * stats + 24 * i;
        assert_eq!(le_u16(footer, e + 2), 0, "section reserved");
        let (off, len) = (
            le_u64(footer, e + 8) as usize,
            le_u64(footer, e + 16) as usize,
        );
        assert!(off + len <= start);
        for c in &mut covered[off..off + len] {
            assert!(!*c, "sections overlap");
            *c = true;
        }
    }
    for (i, c) in covered.iter().enumerate() {
        if !c {
            assert_eq!(bytes[i], 0, "uncovered byte {i} is not padding");
        }
    }
}

/// Each footer variable part moves with its count: one more `text`/`keyword` field adds exactly 16 bytes of stats,
/// a longer id adds exactly its bytes.
#[test]
fn footer_len_tracks_stats_table_and_ids() {
    let base = SchemaBuilder::new("c")
        .field("k", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let more = SchemaBuilder::new("c")
        .field("k", FieldKind::Keyword(KeywordOptions::default()))
        .field("unused", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let footer_len = |b: &[u8]| le_u32(b, b.len() - 8) as usize;
    let a = build(&base, &[Document::new("x")], WriteOptions::default());
    let b = build(&more, &[Document::new("x")], WriteOptions::default());
    assert_eq!(footer_len(&b), footer_len(&a) + 16);
    let c = build(&base, &[Document::new("xyz")], WriteOptions::default());
    // min_id and max_id both grow by 2 bytes.
    assert_eq!(footer_len(&c), footer_len(&a) + 4);
}

/// SPEC §5.1: a footer may exceed the 4096 bytes a reader fetches first; the most fields a schema has (each with
/// a value, so stats and table are both at their largest) still fits within `MAX_FOOTER_LEN`.
#[test]
fn footer_of_a_schema_with_the_most_fields() {
    let mut sb = SchemaBuilder::new("wide");
    for i in 0..MAX_FIELDS {
        sb = sb.field(
            format!("k{i:04}"),
            FieldKind::Keyword(KeywordOptions::default()),
        );
    }
    let schema = sb.build().unwrap();
    let mut doc = Document::new("only");
    for f in schema.fields() {
        doc.insert(f.id(), Value::String(f.name().to_owned()));
    }
    let docs = [doc];
    let bytes = build(&schema, &docs, WriteOptions::default());
    check(&schema, &docs, &bytes);
    let footer_len = le_u32(&bytes, bytes.len() - 8);
    assert!(footer_len > 4096, "{footer_len}");
    assert!(footer_len <= persia_engine::segment::MAX_FOOTER_LEN);
    let f = decode::parse(&bytes);
    assert_eq!(f.stats.len(), MAX_FIELDS);
    assert!(f.stats.values().all(|&s| s == (1, 1)));
    // TermDict, TermInfo, Postings, Freqs and Columns per field, then DocStore, IdMap, IdList.
    assert_eq!(f.sections.len(), MAX_FIELDS * 5 + 3);
}

/// `min_id`/`max_id` compare bytes, not chars or case: `"Z" < "a" < "z" < "é"`. A single document is both. The
/// longest id fits.
#[test]
fn min_and_max_id_are_byte_order() {
    let schema = schema();
    let long = "~".repeat(MAX_ID_LEN);
    for (ids, min, max) in [
        (vec!["a", "é", "Z", "z"], "Z", "é"),
        (vec!["solo"], "solo", "solo"),
        (vec!["b", long.as_str(), "a"], "a", long.as_str()),
    ] {
        let docs: Vec<Document> = ids.iter().map(|i| Document::new(*i)).collect();
        let bytes = build(&schema, &docs, WriteOptions::default());
        check(&schema, &docs, &bytes);
        let f = decode::parse(&bytes);
        assert_eq!(
            (f.min_id.as_slice(), f.max_id.as_slice()),
            (min.as_bytes(), max.as_bytes())
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 2–3. Statistics and presence (SPEC §5.1, §5.3)
// ---------------------------------------------------------------------------------------------------------------

fn presence_schema() -> CollectionSchema {
    let en = || AnalyzerRef::builtin("en").unwrap();
    SchemaBuilder::new("presence")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        // Only stopwords reach it: values, but no tokens.
        .field(
            "stops",
            FieldKind::Text(TextOptions {
                analyzer: en(),
                positions: true,
                ..TextOptions::default()
            }),
        )
        // No document has it.
        .field("never_text", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        // Only empty lists reach it.
        .field("empty_kw", FieldKind::Keyword(KeywordOptions::default()))
        .field("never_kw", FieldKind::Keyword(KeywordOptions::default()))
        .field("n", FieldKind::I64 { stored: true })
        .field("never_n", FieldKind::F64 { stored: true })
        .field("flag", FieldKind::Bool { stored: false })
        .field("never_flag", FieldKind::Bool { stored: false })
        .field("raw", FieldKind::Stored)
        .build()
        .unwrap()
}

fn presence_docs(schema: &CollectionSchema) -> Vec<Document> {
    let f = |n: &str| id(schema, n);
    vec![
        Document::new("d0")
            .with(f("title"), Value::String("Hello world".into()))
            .with(
                f("stops"),
                Value::Strings(vec!["the of".into(), String::new(), "and".into()]),
            )
            .with(f("tag"), Value::String("t".into()))
            .with(f("empty_kw"), Value::Strings(Vec::new()))
            .with(f("n"), Value::I64(7))
            .with(f("flag"), Value::Bool(false))
            .with(f("raw"), Value::Json(serde_json::json!([1, 2]))),
        Document::new("d1")
            .with(f("stops"), Value::String("a an the".into()))
            .with(f("empty_kw"), Value::Strings(Vec::new()))
            .with(f("raw"), Value::Json(serde_json::json!("s"))),
    ]
}

/// SPEC §5.3 presence: the exact section table, and SPEC §5.1 stats for every text/keyword field including zeros.
#[test]
fn presence_rules_give_exactly_these_sections() {
    let schema = presence_schema();
    let docs = presence_docs(&schema);
    let bytes = build(&schema, &docs, WriteOptions::default());
    check(&schema, &docs, &bytes);
    let f = |n: &str| id(&schema, n).get();

    // Field ids follow the schema's own order (not declaration order): sort into table order.
    let mut expected = vec![
        // title: TermDict, TermInfo, Postings, Freqs, Positions, Norms; no Skip (df < 128).
        (f("title"), 1),
        (f("title"), 2),
        (f("title"), 3),
        (f("title"), 4),
        (f("title"), 6),
        (f("title"), 7),
        // tag: TermDict, TermInfo, Postings, Freqs, Columns.
        (f("tag"), 1),
        (f("tag"), 2),
        (f("tag"), 3),
        (f("tag"), 4),
        (f("tag"), 8),
        (f("n"), 8),
        // An all-false bool column is still a column: every document that has the field has a value.
        (f("flag"), 8),
        // DocStore, IdMap, IdList; no BlobRefs without blobs.
        (SEG, 9),
        (SEG, 10),
        (SEG, 11),
    ];
    expected.sort_unstable();
    assert_eq!(table(&bytes), expected);

    let stats = decode::parse(&bytes).stats;
    let expected_stats = BTreeMap::from([
        (f("title"), (1, 2)),
        (f("stops"), (0, 0)),
        (f("never_text"), (0, 0)),
        (f("tag"), (1, 1)),
        (f("empty_kw"), (0, 0)),
        (f("never_kw"), (0, 0)),
    ]);
    assert_eq!(stats, expected_stats);
}

/// SPEC §5.3: BlobRefs exists iff a document has a blob, wherever that document is, and its records are empty for
/// the others.
#[test]
fn blob_refs_present_iff_some_document_has_a_blob() {
    let schema = schema();
    for with_blob in [None, Some(0), Some(4), Some(9)] {
        let docs: Vec<Document> = (0..10_u8)
            .map(|i| {
                let mut d = Document::new(format!("d{i}"));
                if Some(i) == with_blob {
                    d.set_blob_ref(Some(blob(i)));
                }
                d
            })
            .collect();
        let bytes = build(&schema, &docs, WriteOptions::default());
        check(&schema, &docs, &bytes);
        assert_eq!(
            table(&bytes).contains(&(SEG, 12)),
            with_blob.is_some(),
            "{with_blob:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Positions (SPEC §5.3.3)
// ---------------------------------------------------------------------------------------------------------------

fn positions_of(bytes: &[u8], field: u32, doc: u32) -> BTreeMap<Vec<u8>, Vec<u32>> {
    let f = decode::parse(bytes);
    let norms = decode::stream(&f.sections[&(7, field)]);
    decode::terms(&f, field, Some(&norms), true)
        .terms
        .into_iter()
        .filter_map(|(term, occ)| {
            occ.into_iter()
                .find(|o| o.doc == doc)
                .map(|o| (term, o.positions))
        })
        .collect()
}

/// Value `i + 1` starts `POSITION_GAP` after the end of value `i`, which is 0 for a value with no tokens: here
/// stopword-only and empty values sit between, before and after values with tokens. Positions are hand-computed
/// from SPEC §5.3.3, not from the model.
#[test]
fn position_gap_counts_values_without_tokens() {
    assert_eq!(POSITION_GAP, 100);
    let schema = presence_schema();
    let stops = id(&schema, "stops");
    let doc = Document::new("p").with(
        stops,
        Value::Strings(vec![
            // "cat" at 1 (the stopword keeps its position): base 0, end 2.
            "the cat".into(),
            // No tokens: base 102, end 0.
            "of the".into(),
            // No tokens: base 202, end 0.
            String::new(),
            // base 302: "dog" at 302, "cat" at 304; end 3.
            "dogs and cats".into(),
            // No tokens: base 405.
            "the".into(),
            // base 505.
            "cat".into(),
        ]),
    );
    let docs = [doc];
    let bytes = build(&schema, &docs, WriteOptions::default());
    check(&schema, &docs, &bytes);
    let got = positions_of(&bytes, stops.get(), 0);
    assert_eq!(
        got,
        BTreeMap::from([
            (b"cat".to_vec(), vec![1, 304, 505]),
            (b"dog".to_vec(), vec![302]),
        ])
    );
    // The field length counts tokens only: 4.
    assert_eq!(decode::parse(&bytes).stats[&stops.get()], (1, 4));
}

/// A single value that analyzes to nothing next to one that does not, as the only content of the field: the empty
/// value first still shifts the base.
#[test]
fn leading_empty_value_shifts_positions() {
    let schema = presence_schema();
    let title = id(&schema, "title");
    let docs = [Document::new("p").with(
        title,
        Value::Strings(vec![String::new(), "  ".into(), "x y x".into()]),
    )];
    let bytes = build(&schema, &docs, WriteOptions::default());
    check(&schema, &docs, &bytes);
    assert_eq!(
        positions_of(&bytes, title.get(), 0),
        BTreeMap::from([(b"x".to_vec(), vec![200, 202]), (b"y".to_vec(), vec![201])])
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 5. Keyword lists (SPEC §5.3.1, §5.3.4, §5.3.5)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn keyword_duplicates_empty_string_and_empty_list() {
    let schema = presence_schema();
    let tag = id(&schema, "tag");
    let docs = vec![
        Document::new("dups").with(
            tag,
            Value::Strings(vec![
                "b".into(),
                "a".into(),
                "b".into(),
                String::new(),
                "a".into(),
                "b".into(),
            ]),
        ),
        Document::new("empty-list").with(tag, Value::Strings(Vec::new())),
        Document::new("empty-string").with(tag, Value::String(String::new())),
        Document::new("absent"),
        Document::new("one").with(tag, Value::Strings(vec!["a".into()])),
    ];
    let bytes = build(&schema, &docs, WriteOptions::default());
    check(&schema, &docs, &bytes);
    let f = decode::parse(&bytes);

    let occ = |doc, tf| Occurrence {
        doc,
        tf,
        positions: Vec::new(),
    };
    let terms = decode::terms(&f, tag.get(), None, false).terms;
    assert_eq!(
        terms,
        BTreeMap::from([
            (Vec::new(), vec![occ(0, 1), occ(2, 1)]),
            (b"a".to_vec(), vec![occ(0, 2), occ(4, 1)]),
            (b"b".to_vec(), vec![occ(0, 3)]),
        ])
    );

    let column = decode::keyword_column(&decode::stream(&f.sections[&(8, tag.get())]), 5);
    assert_eq!(
        column,
        BTreeMap::from([
            (0, vec![Vec::new(), b"a".to_vec(), b"b".to_vec()]),
            (2, vec![Vec::new()]),
            (4, vec![b"a".to_vec()]),
        ]),
        "one ord per distinct value; an empty list reads as missing, an empty string does not"
    );
    // docs_with_field: dups, empty-string, one. sum_field_len counts every value, duplicates too: 6 + 1 + 1.
    assert_eq!(f.stats[&tag.get()], (3, 8));
}

// ---------------------------------------------------------------------------------------------------------------
// 6. Numeric columns (SPEC §5.2.1, §5.3.5)
// ---------------------------------------------------------------------------------------------------------------

fn numeric_schema() -> CollectionSchema {
    SchemaBuilder::new("nums")
        .field("i", FieldKind::I64 { stored: false })
        .field("f", FieldKind::F64 { stored: true })
        .field("t", FieldKind::Datetime { stored: true })
        .field("b", FieldKind::Bool { stored: false })
        .build()
        .unwrap()
}

const I64S: &[i64] = &[i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];
const F64S: &[f64] = &[
    f64::MIN,
    -1.0,
    -f64::MIN_POSITIVE,
    -5e-324,
    0.0,
    -0.0,
    5e-324,
    f64::MIN_POSITIVE,
    1.0,
    f64::MAX,
];
const DATETIMES: &[i64] = &[MIN_DATETIME, -1, 0, 1, MAX_DATETIME];

/// The presence bitmap's length field of a Numeric/Bool `Columns` stream (right after the 8-byte header).
fn presence_len(bytes: &[u8], field: u32) -> u64 {
    let f = decode::parse(bytes);
    let s = decode::stream(&f.sections[&(8, field)]);
    le_u64(&s, 8)
}

/// Every document has every column, over three FOR blocks (2100 docs): presence is written as length 0, and the
/// extremes of every type round-trip.
#[test]
fn dense_numeric_columns_at_extremes_over_several_for_blocks() {
    let schema = numeric_schema();
    let f = |n: &str| id(&schema, n);
    let docs: Vec<Document> = (0..2100_usize)
        .map(|d| {
            Document::new(format!("{d:05}"))
                .with(f("i"), Value::I64(I64S[d % I64S.len()]))
                .with(f("f"), Value::F64(F64S[d % F64S.len()]))
                .with(f("t"), Value::Datetime(DATETIMES[d % DATETIMES.len()]))
                .with(f("b"), Value::Bool(d % 3 == 0))
        })
        .collect();
    let bytes = build(&schema, &docs, WriteOptions::default());
    check(&schema, &docs, &bytes);
    for name in ["i", "f", "t", "b"] {
        assert_eq!(presence_len(&bytes, f(name).get()), 0, "{name}");
    }
    // -0.0 enters the document as 0.0 (SPEC §6.4.1), so its key is +0.0's.
    let col = decode::numeric_column(
        &decode::stream(&decode::parse(&bytes).sections[&(8, f("f").get())]),
        2100,
    );
    let neg_zero_doc = F64S
        .iter()
        .position(|v| v.to_bits() == (-0.0_f64).to_bits());
    assert_eq!(col[&(neg_zero_doc.unwrap() as u32)], f64_to_key(0.0));
    assert!(
        col.values().all(|&k| k != f64_to_key(-0.0)),
        "no -0.0 key is written"
    );
}

/// Sparse columns: a whole FOR block without values (docs 1024..2048), documents with a value only at block
/// edges, and extremes. Presence is a real bitmap; an absent document's key is its block's minimum
/// (SPEC §5.3.5, a writer convention that keeps it at zero bits).
#[test]
fn sparse_numeric_columns_over_several_for_blocks() {
    let schema = numeric_schema();
    let i = id(&schema, "i");
    let has = |d: usize| {
        matches!(d, 0 | 1 | 1023 | 2048 | 2049 | 2099) || (d < 1024 && d.is_multiple_of(5))
    };
    let docs: Vec<Document> = (0..2100_usize)
        .map(|d| {
            let mut doc = Document::new(format!("{d:05}"));
            if has(d) {
                doc.insert(i, Value::I64(I64S[d % I64S.len()]));
            }
            doc
        })
        .collect();
    let bytes = build(&schema, &docs, WriteOptions::default());
    check(&schema, &docs, &bytes);
    assert!(presence_len(&bytes, i.get()) > 0);

    // Read the dense keys, absent documents included.
    let stream = decode::stream(&decode::parse(&bytes).sections[&(8, i.get())]);
    let mut r = persia_format::Reader::new(&stream);
    r.read_bytes(8).unwrap();
    let len = r.read_u64().unwrap() as usize;
    r.read_bytes(len).unwrap();
    let keys = decode::for_column(&mut r);
    assert!(r.is_empty());
    assert_eq!(keys.len(), 2100);
    for (block, chunk) in keys.chunks(1024).enumerate() {
        let present: Vec<u64> = (block * 1024..block * 1024 + chunk.len())
            .filter(|&d| has(d))
            .map(|d| i64_to_key(I64S[d % I64S.len()]))
            .collect();
        let min = present.iter().copied().min().unwrap_or(0);
        for (k, &key) in chunk.iter().enumerate() {
            if !has(block * 1024 + k) {
                assert_eq!(key, min, "absent doc {} of block {block}", block * 1024 + k);
            }
        }
    }
}

/// A bool column whose values are all `true`, all `false`, or present in only one document.
#[test]
fn bool_column_shapes() {
    let schema = numeric_schema();
    let b = id(&schema, "b");
    for (values, all_present) in [
        (vec![Some(true); 3], true),
        (vec![Some(false); 3], true),
        (vec![None, Some(true), None], false),
        (vec![None, None, Some(false)], false),
    ] {
        let docs: Vec<Document> = values
            .iter()
            .enumerate()
            .map(|(d, v)| {
                let mut doc = Document::new(format!("{d}"));
                if let Some(v) = v {
                    doc.insert(b, Value::Bool(*v));
                }
                doc
            })
            .collect();
        let bytes = build(&schema, &docs, WriteOptions::default());
        check(&schema, &docs, &bytes);
        assert_eq!(
            presence_len(&bytes, b.get()) == 0,
            all_present,
            "{values:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 7. Builder atomicity (ROADMAP 5.1; SegmentBuilder::add docs)
// ---------------------------------------------------------------------------------------------------------------

/// Documents the builder must reject, each after a valid prefix of fields so a non-atomic builder would have
/// indexed something first. The first one reuses `taken`, an id already added.
fn rejected(schema: &CollectionSchema, taken: &str) -> Vec<(Document, &'static str)> {
    let f = |n: &str| id(schema, n);
    let full = |doc_id: &str| {
        Document::new(doc_id)
            .with(f("title"), Value::String("never indexed words".into()))
            .with(f("body"), Value::String("unseen".into()))
            .with(f("tag"), Value::String("ghost".into()))
            .with(f("price"), Value::F64(1.0))
    };
    vec![
        (
            full(taken).with(f("qty"), Value::I64(1)),
            "duplicate id with new content",
        ),
        (
            full("bad-type").with(f("qty"), Value::String("x".into())),
            "wrong type in a later field",
        ),
        (
            full("bad-float").with(f("price"), Value::F64(f64::NAN)),
            "non-finite f64",
        ),
        (
            full("bad-time").with(f("at"), Value::Datetime(MAX_DATETIME + 1)),
            "datetime out of range",
        ),
        (
            full("bad-kw").with(f("tag"), Value::String("k".repeat(MAX_KEYWORD_LEN + 1))),
            "keyword too long",
        ),
        (
            full("bad-raw").with(f("raw"), Value::Json(serde_json::Value::Null)),
            "null stored value",
        ),
        (full(""), "empty id"),
    ]
}

/// SPEC (SegmentBuilder::add): a rejected document leaves the builder as it was: same docids for later documents,
/// same `size_in_bytes`, byte-identical segment.
#[test]
fn rejected_documents_leave_output_byte_identical() {
    let schema = schema();
    let docs = golden_docs(&schema);
    let (head, tail) = docs.split_at(150);
    let reference = build(&schema, &docs, WriteOptions::default());

    let mut b = builder(&schema);
    for d in head {
        b.add(d).unwrap();
    }
    let size = b.size_in_bytes();
    for (bad, why) in rejected(&schema, head[3].id()) {
        let err = b.add(&bad).unwrap_err();
        match why {
            "duplicate id with new content" => assert_eq!(
                err,
                SegmentError::DuplicateId {
                    id: head[3].id().into()
                }
            ),
            _ => assert!(matches!(err, SegmentError::Document(_)), "{why}: {err}"),
        }
        assert_eq!(b.doc_count() as usize, head.len(), "{why}");
        assert_eq!(b.size_in_bytes(), size, "{why}");
    }
    for (i, d) in tail.iter().enumerate() {
        assert_eq!(b.add(d).unwrap() as usize, head.len() + i);
    }
    assert!(
        finish(&b) == reference,
        "a rejected document changed the segment"
    );
}

/// Ops for the atomicity property: add a fresh document, re-add an earlier id, or add an invalid one.
#[derive(Debug, Clone)]
enum Op {
    Add(u8, u8),
    Duplicate(usize),
    Invalid(usize),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::Add(a, b)),
        1 => any::<usize>().prop_map(Op::Duplicate),
        1 => any::<usize>().prop_map(Op::Invalid),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 512 } else { 32 }, ..ProptestConfig::default() })]

    /// Interleaving rejections with accepted documents gives the segment of the accepted ones alone;
    /// `size_in_bytes` never shrinks and does not move on a rejection.
    #[test]
    fn rejections_are_invisible(ops in prop::collection::vec(arb_op(), 1..80)) {
        let schema = schema();
        let f = |n: &str| id(&schema, n);
        let bad = rejected(&schema, "");
        let mut b = builder(&schema);
        let mut accepted: Vec<Document> = Vec::new();
        let mut last = b.size_in_bytes();
        for (n, op) in ops.iter().enumerate() {
            let before = b.size_in_bytes();
            let result = match *op {
                Op::Add(w, t) => {
                    let doc = Document::new(format!("n{n}"))
                        .with(f("title"), Value::String(format!("w{w} common")))
                        .with(f("tag"), Value::Strings(vec![format!("t{}", t % 7), "x".into()]))
                        .with(f("qty"), Value::I64(i64::from(w) - 100));
                    let r = b.add(&doc);
                    prop_assert_eq!(r.clone(), Ok(u32::try_from(accepted.len()).unwrap()));
                    accepted.push(doc);
                    r
                }
                Op::Duplicate(i) if !accepted.is_empty() => {
                    let id = accepted[i % accepted.len()].id().to_owned();
                    let r = b.add(&Document::new(id.clone()).with(f("tag"), Value::String("dup".into())));
                    prop_assert_eq!(r.clone(), Err(SegmentError::DuplicateId { id }));
                    r
                }
                Op::Duplicate(_) => continue,
                Op::Invalid(i) => {
                    let (doc, why) = &bad[1 + i % (bad.len() - 1)];
                    let r = b.add(doc);
                    prop_assert!(matches!(r, Err(SegmentError::Document(_))), "{}", why);
                    r
                }
            };
            let now = b.size_in_bytes();
            prop_assert!(now >= last);
            if result.is_err() {
                prop_assert_eq!(now, before);
            }
            last = now;
        }
        if accepted.is_empty() {
            prop_assert_eq!(SegmentWriter::new(&b, meta()).finish(), Err(SegmentError::Empty));
        } else {
            let expected = build(&schema, &accepted, WriteOptions::default());
            prop_assert!(finish(&b) == expected);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 8. Determinism (SPEC §5; segment module docs)
// ---------------------------------------------------------------------------------------------------------------

/// Two builders, each with its own registry and schema value, give the same bytes; `finish` does not consume or
/// change the builder, and a builder finished midway and then extended equals one that never was.
#[test]
fn output_depends_only_on_documents_meta_and_options() {
    let docs = golden_docs(&schema());
    let a = build(&schema(), &docs, WriteOptions::default());

    let schema = schema();
    let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    let mut midway = Vec::new();
    for (i, d) in docs.iter().enumerate() {
        b.add(d).unwrap();
        if i == 99 {
            midway = finish(&b);
            assert_eq!(finish(&b), midway, "finish twice");
        }
    }
    assert!(finish(&b) == a, "separate builder");
    assert!(finish(&b) == a, "finish is repeatable");
    assert_eq!(
        midway,
        build(&schema, &docs[..100], WriteOptions::default())
    );

    // Meta goes only into the footer: the bytes before it are the same.
    let other = SegmentWriter::new(
        &b,
        SegmentMeta {
            segment_id: 1,
            created_at: 2,
            merge_generation: 0,
        },
    )
    .finish()
    .unwrap();
    let footer_start = |x: &[u8]| x.len() - 8 - le_u32(x, x.len() - 8) as usize;
    assert_eq!(footer_start(&a), footer_start(&other));
    assert_eq!(a[..footer_start(&a)], other[..footer_start(&other)]);
    assert_ne!(a, other);
}

/// The zstd level reaches the compressed sections only (DocStore, IdList, BlobRefs); every block-framed section
/// still decodes, and the segment-level FST and the field sections do not change.
#[test]
fn zstd_level_changes_only_compressed_sections() {
    let schema = schema();
    let docs = golden_docs(&schema);
    let a = build(&schema, &docs, WriteOptions::default());
    let b = build(
        &schema,
        &docs,
        WriteOptions {
            zstd_level: 19,
            ..WriteOptions::default()
        },
    );
    check(&schema, &docs, &b);
    let (fa, fb) = (decode::parse(&a), decode::parse(&b));
    assert_eq!(
        fa.sections.keys().collect::<Vec<_>>(),
        fb.sections.keys().collect::<Vec<_>>()
    );
    for (key, sa) in &fa.sections {
        if !matches!(key, (9 | 11 | 12, SEG)) {
            assert!(
                sa == &fb.sections[key],
                "section {key:?} depends on zstd_level"
            );
        }
    }
}

/// The streamed writer (ROADMAP 5.1.6) at the list and column edges: lists of 127, 128 and 129 postings (one tail
/// block; one full block with a Skip entry; a full block and a one-posting tail) and document counts at the FOR
/// block edges (a keyword `starts` column of 1025 values holds a one-value second block). Each segment decodes to
/// its model, opens and verifies; with a 1-byte block size every framed section is cut into many blocks.
#[test]
fn writer_edges_of_lists_and_columns_round_trip() {
    let schema = schema();
    let s = &schema;
    for n in [1_u32, 127, 128, 129, 1023, 1024, 1025] {
        let docs: Vec<Document> = (0..n)
            .map(|i| {
                let mut d = Document::new(format!("d{i:05}"))
                    .with(
                        id(s, "title"),
                        Value::String(format!("all w{} w{}", i % 7, i % 129)),
                    )
                    .with(id(s, "qty"), Value::I64(i64::from(i) - 500));
                if i % 3 != 1 {
                    let tags = vec!["every".to_owned(), format!("k{}", i % 5), format!("z{i}")];
                    d = d.with(id(s, "tag"), Value::Strings(tags));
                }
                if i % 2 == 0 {
                    d = d.with(id(s, "flag"), Value::Bool(i % 4 == 0));
                }
                d
            })
            .collect();
        for block_size in [1_u32, 509, 64 << 10] {
            let options = WriteOptions {
                block_size,
                ..WriteOptions::default()
            };
            let bytes = build(&schema, &docs, options);
            check(&schema, &docs, &bytes);
            let seg = persia_engine::segment::Segment::open(
                persia_format::ByteSource::new(bytes.into()),
                &schema,
            )
            .unwrap();
            seg.verify().unwrap();
            let all = seg
                .terms(id(s, "title"))
                .unwrap()
                .get(b"all")
                .unwrap()
                .unwrap();
            assert_eq!(all.df(), n, "n {n} block_size {block_size}");
        }
    }
}
