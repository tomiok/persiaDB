//! The segment merger (SPEC §5.5, ADR-0031, ROADMAP 6.5.2). The oracle is the flush: merging segments with
//! deletions must write, byte for byte, the segment a builder writes from the same live documents in the same order
//! (same id, `created_at` and `merge_generation`). That covers the k-way term merge, postings and positions with
//! remapped docids, norms, columns, rows, ids, blob refs, and every recomputed statistic at once.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    missing_docs
)]

use std::collections::BTreeMap;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeError, MergeSource, merge};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::segment::{
    POSITION_GAP, Segment, SegmentBuilder, SegmentError, SegmentMeta, SegmentWriter, Tombstones,
    WriteOptions,
};
use persia_format::checksum::crc32c;
use persia_format::{BlobId, BlobRef, ByteSource};
use proptest::prelude::*;
use roaring::RoaringBitmap;

const CREATED_AT: i64 = 1_700_000_000_000_000;
const OUTPUT_ID: u128 = 0xfeed;

/// The text and keyword fields: `title` keeps positions, `body` is stemmed (`en`), `tag` is a multi-valued keyword.
fn old_schema() -> CollectionSchema {
    text_fields(SchemaBuilder::new("things")).build().unwrap()
}

/// Every field kind: [`old_schema`] evolved with columns and a stored field (SPEC §6.2.4), so ids of the old fields
/// stay.
fn schema() -> CollectionSchema {
    let proposed = text_fields(SchemaBuilder::new("things"))
        .field("price", FieldKind::F64 { stored: true })
        .field("qty", FieldKind::I64 { stored: false })
        .field("at", FieldKind::Datetime { stored: true })
        .field("flag", FieldKind::Bool { stored: true })
        .field("raw", FieldKind::Stored)
        .build()
        .unwrap();
    old_schema().evolve(&proposed).unwrap()
}

fn text_fields(b: SchemaBuilder) -> SchemaBuilder {
    b.field(
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
}

fn field(schema: &CollectionSchema, name: &str) -> persia_engine::schema::FieldId {
    schema.field(name).unwrap().id()
}

fn registry() -> AnalyzerRegistry {
    AnalyzerRegistry::builtin()
}

fn write(
    schema: &CollectionSchema,
    docs: &[Document],
    meta: SegmentMeta,
    options: WriteOptions,
) -> Vec<u8> {
    let mut b = SegmentBuilder::new(schema, &registry()).unwrap();
    for d in docs {
        b.add(d).unwrap();
    }
    SegmentWriter::new(&b, meta)
        .with_options(options)
        .finish()
        .unwrap()
}

fn open(schema: &CollectionSchema, bytes: Vec<u8>) -> Segment {
    Segment::open(ByteSource::new(bytes.into()), schema).unwrap()
}

fn input_meta(i: usize, merge_generation: u32) -> SegmentMeta {
    SegmentMeta {
        segment_id: 100 + i as u128,
        created_at: CREATED_AT - 1,
        merge_generation,
    }
}

fn output_meta(merge_generation: u32) -> SegmentMeta {
    SegmentMeta {
        segment_id: OUTPUT_ID,
        created_at: CREATED_AT,
        merge_generation,
    }
}

/// Segments opened from `bytes`, with their deletions.
struct Inputs {
    segments: Vec<Segment>,
    tombstones: Vec<Option<Tombstones>>,
}

impl Inputs {
    fn new(schema: &CollectionSchema, bytes: Vec<Vec<u8>>, deleted: &[Vec<u32>]) -> Self {
        let segments: Vec<_> = bytes.into_iter().map(|b| open(schema, b)).collect();
        let tombstones = segments
            .iter()
            .zip(deleted)
            .map(|(s, d)| {
                (!d.is_empty()).then(|| s.tombstones(d.iter().copied().collect()).unwrap())
            })
            .collect();
        Self {
            segments,
            tombstones,
        }
    }

    fn sources(&self) -> Vec<MergeSource<'_>> {
        self.segments
            .iter()
            .zip(&self.tombstones)
            .map(|(segment, t)| MergeSource {
                segment,
                tombstones: t.as_ref(),
            })
            .collect()
    }
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
    let meta = BTreeMap::from([("origin".to_owned(), format!("scan-{i}"))]);
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
    /// A document of [`schema`] without an id; `only_old` keeps to [`old_schema`]'s fields.
    fn arb_doc(only_old: bool)(
        title in proptest::option::of(prop::collection::vec(prop::collection::vec(prop::sample::select(WORDS), 0..6), 0..3)),
        body in proptest::option::of(prop::collection::vec(prop::sample::select(WORDS), 0..10)),
        tags in proptest::option::of(prop::collection::vec(prop::sample::select(TAGS), 0..4)),
        price in proptest::option::of(prop_oneof![Just(0.0), Just(-0.0), -1e6..1e6_f64]),
        qty in proptest::option::of(any::<i64>()),
        at in proptest::option::of(-62_000_000_000_000_000_i64..250_000_000_000_000_000),
        flag in proptest::option::of(any::<bool>()),
        raw in proptest::option::of(any::<u32>()),
        blob_seed in proptest::option::of(any::<u8>()),
    ) -> Document {
        let s = schema();
        let mut d = Document::new("");
        if let Some(t) = title {
            let values: Vec<String> = t.iter().map(|w| w.join(" ")).collect();
            d.insert(field(&s, "title"), if values.len() == 1 { Value::String(values[0].clone()) } else { Value::Strings(values) });
        }
        if let Some(b) = body { d.insert(field(&s, "body"), Value::String(b.join(" "))); }
        if let Some(t) = tags { d.insert(field(&s, "tag"), Value::Strings(t.iter().map(|s| (*s).to_owned()).collect())); }
        if !only_old {
            if let Some(p) = price { d.insert(field(&s, "price"), Value::F64(p)); }
            if let Some(q) = qty { d.insert(field(&s, "qty"), Value::I64(q)); }
            if let Some(a) = at { d.insert(field(&s, "at"), Value::Datetime(a)); }
            if let Some(f) = flag { d.insert(field(&s, "flag"), Value::Bool(f)); }
            if let Some(r) = raw { d.insert(field(&s, "raw"), Value::Json(serde_json::json!({ "n": r }))); }
        }
        d.set_blob_ref(blob_seed.map(blob));
        d
    }
}

/// `doc` with id `id`.
fn with_id(doc: &Document, id: String) -> Document {
    let mut out = Document::new(id);
    for (f, v) in doc.fields() {
        out.insert(f, v.clone());
    }
    out.set_blob_ref(doc.blob_ref().cloned());
    out
}

/// One input: its documents, the docids deleted, and its merge generation.
type Plan = Vec<(Vec<Document>, Vec<u32>, u32)>;

fn arb_inputs(only_old_first: bool) -> impl Strategy<Value = Plan> {
    let input = |only_old: bool| {
        prop::collection::vec((arb_doc(only_old), any::<bool>(), "[a-z]{0,2}"), 1..60)
            .prop_flat_map(move |docs| (Just(docs), 0u32..5))
    };
    (
        input(only_old_first),
        prop::collection::vec(input(false), 0..4),
    )
        .prop_map(|(first, rest)| {
            std::iter::once(first)
                .chain(rest)
                .enumerate()
                .map(|(i, (docs, generation))| {
                    let mut deleted = Vec::new();
                    let docs = docs
                        .into_iter()
                        .enumerate()
                        .map(|(d, (doc, gone, suffix))| {
                            if gone {
                                deleted.push(d as u32);
                            }
                            // Unique across inputs; the suffix varies id order against docid order.
                            with_id(&doc, format!("{suffix}-{i}-{d}"))
                        })
                        .collect();
                    (docs, deleted, generation)
                })
                .collect()
        })
}

/// Writes each input, merges them, and checks the bytes against a flush of the live documents.
fn check_merge(
    plan: &Plan,
    first_schema: &CollectionSchema,
    options: WriteOptions,
) -> Result<(), TestCaseError> {
    let schema = schema();
    let bytes = plan
        .iter()
        .enumerate()
        .map(|(i, (docs, _, generation))| {
            let s = if i == 0 { first_schema } else { &schema };
            write(s, docs, input_meta(i, *generation), options)
        })
        .collect();
    let deleted: Vec<_> = plan.iter().map(|(_, d, _)| d.clone()).collect();
    let inputs = Inputs::new(&schema, bytes, &deleted);
    let merged = merge(&schema, &registry(), &inputs.sources()).unwrap();
    let live: Vec<Document> = plan
        .iter()
        .flat_map(|(docs, deleted, _)| {
            docs.iter()
                .enumerate()
                .filter(|(d, _)| !deleted.contains(&(*d as u32)))
                .map(|(_, doc)| doc.clone())
        })
        .collect();
    let Some(merged) = merged else {
        prop_assert!(live.is_empty());
        return Ok(());
    };
    let generation = plan.iter().map(|(_, _, g)| *g).max().unwrap() + 1;
    prop_assert_eq!(merged.merge_generation(), generation);
    prop_assert_eq!(merged.builder().doc_count() as usize, live.len());
    let got = merged.write(OUTPUT_ID, CREATED_AT, options).unwrap();
    let want = write(&schema, &live, output_meta(generation), options);
    prop_assert!(
        got == want,
        "merged segment differs from a flush of the live documents"
    );
    // And it reads back: every live document by id.
    let segment = open(&schema, got);
    for (docid, doc) in live.iter().enumerate() {
        prop_assert_eq!(segment.docid_of(doc.id()).unwrap(), Some(docid as u32));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 256 } else { 32 }, ..ProptestConfig::default() })]

    /// A merge writes what a flush of the inputs' live documents, in input order, writes.
    #[test]
    fn merge_equals_flush_of_live_documents(
        plan in arb_inputs(false),
        block_size in prop::sample::select(vec![61_u32, 4096, 64 * 1024]),
    ) {
        check_merge(&plan, &schema(), WriteOptions { block_size, ..WriteOptions::default() })?;
    }

    /// The first input was written before the schema gained its column and stored fields (SPEC §6.2.4).
    #[test]
    fn merge_across_schema_evolution(plan in arb_inputs(true)) {
        let old = old_schema();
        let full = schema();
        prop_assert_eq!(old.fields(), &full.fields()[..3]);
        check_merge(&plan, &old, WriteOptions::default())?;
    }
}

fn doc(id: &str, title: &str) -> Document {
    Document::new(id).with(field(&schema(), "title"), Value::String(title.into()))
}

fn segment(i: usize, docs: &[Document]) -> Segment {
    open(
        &schema(),
        write(&schema(), docs, input_meta(i, 0), WriteOptions::default()),
    )
}

#[test]
fn nothing_live_merges_to_nothing() {
    let schema = schema();
    let a = segment(0, &[doc("a", "alpha"), doc("b", "beta")]);
    let all = a.tombstones((0..2).collect()).unwrap();
    let sources = [MergeSource {
        segment: &a,
        tombstones: Some(&all),
    }];
    assert!(merge(&schema, &registry(), &sources).unwrap().is_none());
    assert!(merge(&schema, &registry(), &[]).unwrap().is_none());
}

#[test]
fn terms_only_deleted_documents_had_are_dropped() {
    let schema = schema();
    let title = field(&schema, "title");
    let a = segment(0, &[doc("a", "alpha shared"), doc("b", "beta shared")]);
    let gone = a.tombstones(RoaringBitmap::from([1])).unwrap();
    let merged = merge(
        &schema,
        &registry(),
        &[MergeSource {
            segment: &a,
            tombstones: Some(&gone),
        }],
    )
    .unwrap()
    .unwrap();
    let out = open(
        &schema,
        merged
            .write(OUTPUT_ID, CREATED_AT, WriteOptions::default())
            .unwrap(),
    );
    let mut terms = out.terms(title).unwrap();
    assert!(terms.get(b"beta").unwrap().is_none());
    assert_eq!(terms.get(b"shared").unwrap().map(|t| t.df()), Some(1));
    assert_eq!(
        out.field_stats(title)
            .map(|s| (s.docs_with_field, s.sum_field_len)),
        Some((1, 2))
    );
}

#[test]
fn positions_and_gaps_survive_remapping() {
    let schema = schema();
    let title = field(&schema, "title");
    let a = segment(0, &[doc("a", "gone")]);
    let multi = Document::new("b").with(title, Value::Strings(vec!["x y".into(), "y".into()]));
    let b = segment(1, &[doc("c", "gone"), multi]);
    let gone_a = a.tombstones(RoaringBitmap::from([0])).unwrap();
    let gone_b = b.tombstones(RoaringBitmap::from([0])).unwrap();
    let sources = [
        MergeSource {
            segment: &a,
            tombstones: Some(&gone_a),
        },
        MergeSource {
            segment: &b,
            tombstones: Some(&gone_b),
        },
    ];
    let merged = merge(&schema, &registry(), &sources).unwrap().unwrap();
    let out = open(
        &schema,
        merged
            .write(OUTPUT_ID, CREATED_AT, WriteOptions::default())
            .unwrap(),
    );
    let terms = out.terms(title).unwrap();
    let mut index = terms;
    let info = index.get(b"y").unwrap().unwrap();
    let mut postings = index.postings(&info).unwrap();
    assert_eq!(postings.next().unwrap(), Some(0));
    assert_eq!(postings.freq().unwrap(), 2);
    assert_eq!(postings.positions().unwrap(), [1, 2 + POSITION_GAP]);
}

#[test]
fn an_id_live_in_two_inputs_is_an_error() {
    let schema = schema();
    let a = segment(0, &[doc("same", "alpha")]);
    let b = segment(1, &[doc("same", "beta")]);
    let sources = [
        MergeSource {
            segment: &a,
            tombstones: None,
        },
        MergeSource {
            segment: &b,
            tombstones: None,
        },
    ];
    assert_eq!(
        merge(&schema, &registry(), &sources).unwrap_err(),
        MergeError::Build(SegmentError::DuplicateId { id: "same".into() })
    );
    // Deleted in one: fine.
    let gone = a.tombstones(RoaringBitmap::from([0])).unwrap();
    let sources = [
        MergeSource {
            segment: &a,
            tombstones: Some(&gone),
        },
        MergeSource {
            segment: &b,
            tombstones: None,
        },
    ];
    assert_eq!(
        merge(&schema, &registry(), &sources)
            .unwrap()
            .unwrap()
            .builder()
            .doc_count(),
        1
    );
}

/// `bytes` with its footer body changed by `edit` and resealed.
fn with_footer(bytes: &[u8], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let n = bytes.len();
    let footer_len = u32::from_le_bytes(bytes[n - 8..n - 4].try_into().unwrap()) as usize;
    let start = n - 8 - footer_len;
    let mut body = bytes[start..n - 12].to_vec();
    edit(&mut body);
    let mut out = bytes[..start].to_vec();
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32c(&body).to_le_bytes());
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
    out.extend_from_slice(b"PSEG");
    out
}

#[test]
fn unknown_rocompat_flags_are_not_rewritten() {
    let schema = schema();
    let bytes = write(
        &schema,
        &[doc("a", "alpha")],
        input_meta(0, 0),
        WriteOptions::default(),
    );
    let a = open(&schema, with_footer(&bytes, |b| b[16] = 4));
    assert_eq!(
        merge(
            &schema,
            &registry(),
            &[MergeSource {
                segment: &a,
                tombstones: None
            }]
        )
        .unwrap_err(),
        MergeError::Unsupported {
            segment: 100,
            flags: 4
        }
    );
}
