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
use std::ops::Bound;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::merge::{MergeError, MergeSource, merge};
use persia_engine::schema::{
    AnalyzerRef, CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::segment::{
    POSITION_GAP, Segment, SegmentBuilder, SegmentError, SegmentMeta, SegmentWriter, Tombstones,
    WriteOptions,
};
use persia_format::checksum::crc32c;
use persia_format::{BlobId, BlobRef, ByteSource};
use proptest::prelude::*;
use roaring::RoaringBitmap;

#[path = "support/split.rs"]
mod split;
use split::Split;

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

fn field(schema: &CollectionSchema, name: &str) -> FieldId {
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

/// One input's documents (each with whether it is deleted and an id suffix) and its merge generation.
type RawInput = (Vec<(Document, bool, String)>, u32);

fn arb_input(only_old: bool) -> impl Strategy<Value = RawInput> {
    prop::collection::vec((arb_doc(only_old), any::<bool>(), "[a-z]{0,2}"), 1..60)
        .prop_flat_map(move |docs| (Just(docs), 0u32..5))
}

fn to_plan(inputs: Vec<RawInput>) -> Plan {
    inputs
        .into_iter()
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
}

fn arb_inputs(only_old_first: bool) -> impl Strategy<Value = Plan> {
    (
        arb_input(only_old_first),
        prop::collection::vec(arb_input(false), 0..4),
    )
        .prop_map(|(first, rest)| to_plan(std::iter::once(first).chain(rest).collect()))
}

/// Inputs of which any may have been written under [`old_schema`] (the `bool`s, one per input), in any position.
fn arb_mixed_inputs() -> impl Strategy<Value = (Plan, Vec<bool>)> {
    prop::collection::vec(
        any::<bool>().prop_flat_map(|old| (Just(old), arb_input(old))),
        1..5,
    )
    .prop_map(|inputs| {
        let (old, raw): (Vec<bool>, Vec<RawInput>) = inputs.into_iter().unzip();
        (to_plan(raw), old)
    })
}

/// The live postings of one field, by term: `(docid, tf, positions)` in docid order.
type FieldPostings = BTreeMap<Vec<u8>, Vec<(u32, u32, Vec<u32>)>>;

/// Every posting of `field` in `segment` whose docid `remap` keeps, with the docid `remap` gives it. Checks each
/// term's `df` against its postings when `remap` keeps everything.
fn postings_of(
    segment: &Segment,
    field: FieldId,
    remap: impl Fn(u32) -> Option<u32>,
) -> FieldPostings {
    let terms = segment.terms(field).unwrap();
    let mut out = FieldPostings::new();
    let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
    while let Some(next) = stream.next() {
        let (term, info) = next.unwrap();
        let mut postings = terms.postings(&info).unwrap();
        let mut list = Vec::new();
        let mut df = 0;
        while let Some(old) = postings.next().unwrap() {
            df += 1;
            if let Some(new) = remap(old) {
                let tf = postings.freq().unwrap();
                list.push((new, tf, postings.positions().unwrap().to_vec()));
            }
        }
        assert_eq!(info.df(), df, "df of {term:?}");
        if !list.is_empty() {
            out.entry(term.to_vec()).or_default().extend(list);
        }
    }
    for list in out.values_mut() {
        list.sort_unstable_by_key(|p| p.0);
    }
    out
}

/// Every stored and indexed value of document `docid` in `segment`, read back through the public reader.
fn values_of(schema: &CollectionSchema, segment: &Segment, docid: u32) -> Vec<String> {
    let mut out = vec![
        format!("{:?}", segment.document(docid).unwrap()),
        format!("{:?}", segment.blob_ref(docid).unwrap()),
    ];
    for f in schema.fields() {
        let id = f.id();
        let v = match f.kind() {
            FieldKind::Text(_) => format!("norm {:?}", segment.norms(id).unwrap().get(docid)),
            FieldKind::Keyword(_) => format!(
                "keyword {:?}",
                segment.keyword_column(id).unwrap().values(docid).unwrap()
            ),
            FieldKind::Bool { .. } => {
                format!("bool {:?}", segment.bool_column(id).unwrap().get(docid))
            }
            FieldKind::Stored => continue,
            _ => format!(
                "numeric {:?}",
                segment.numeric_column(id).unwrap().get(docid).unwrap()
            ),
        };
        out.push(format!("{}: {v}", f.name()));
    }
    out
}

/// The merged segment, read back through the public reader, against its inputs: every live document is found by
/// its id at its merged docid, with the same stored document, blob ref, column values and norms; no deleted id is
/// found; every field's postings are exactly the inputs' live postings, remapped (so no term only deleted documents
/// had survives).
fn check_read_back(
    schema: &CollectionSchema,
    inputs: &Inputs,
    deleted: &[Vec<u32>],
    out: &Segment,
) -> Result<(), TestCaseError> {
    out.verify().unwrap();
    let mut remaps = Vec::new();
    let mut next = 0_u32;
    for (seg, gone) in inputs.segments.iter().zip(deleted) {
        let mut remap = Vec::new();
        for old in 0..seg.doc_count() {
            let id = seg.id_of(old).unwrap();
            if gone.contains(&old) {
                prop_assert_eq!(
                    out.docid_of(&id).unwrap(),
                    None,
                    "deleted {} is in the merge",
                    id
                );
                remap.push(None);
                continue;
            }
            prop_assert_eq!(out.docid_of(&id).unwrap(), Some(next));
            prop_assert_eq!(out.id_of(next).unwrap(), id);
            prop_assert_eq!(values_of(schema, out, next), values_of(schema, seg, old));
            remap.push(Some(next));
            next += 1;
        }
        remaps.push(remap);
    }
    prop_assert_eq!(out.doc_count(), next);
    for f in schema.fields() {
        if !matches!(f.kind(), FieldKind::Text(_) | FieldKind::Keyword(_)) {
            continue;
        }
        let mut want = FieldPostings::new();
        for (seg, remap) in inputs.segments.iter().zip(&remaps) {
            for (term, list) in postings_of(seg, f.id(), |d| remap[d as usize]) {
                want.entry(term).or_default().extend(list);
            }
        }
        let got = postings_of(out, f.id(), Some);
        prop_assert!(
            got == want,
            "postings of {} differ: got {:?}, want {:?}",
            f.name(),
            got,
            want
        );
    }
    Ok(())
}

/// Writes each input (under [`old_schema`] where `old` says so, [`schema`] otherwise), merges them, and checks the
/// bytes against a flush of the live documents, then reads the merge back against its inputs.
fn check_merge(plan: &Plan, old: &[bool], options: WriteOptions) -> Result<(), TestCaseError> {
    let schema = schema();
    let old_schema = old_schema();
    let bytes = plan
        .iter()
        .enumerate()
        .map(|(i, (docs, _, generation))| {
            let s = if old.get(i).copied().unwrap_or(false) {
                &old_schema
            } else {
                &schema
            };
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
    prop_assert_eq!(merged.doc_count() as usize, live.len());
    let got = merged.write(OUTPUT_ID, CREATED_AT, options).unwrap();
    let want = write(&schema, &live, output_meta(generation), options);
    prop_assert!(
        got == want,
        "merged segment differs from a flush of the live documents"
    );
    // And it reads back: every live document by id, with its values; no deleted one.
    let segment = open(&schema, got);
    for (docid, doc) in live.iter().enumerate() {
        prop_assert_eq!(segment.docid_of(doc.id()).unwrap(), Some(docid as u32));
    }
    check_read_back(&schema, &inputs, &deleted, &segment)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 256 } else { 32 }, ..ProptestConfig::default() })]

    /// A merge writes what a flush of the inputs' live documents, in input order, writes.
    #[test]
    fn merge_equals_flush_of_live_documents(
        plan in arb_inputs(false),
        block_size in prop::sample::select(vec![61_u32, 4096, 64 * 1024]),
    ) {
        check_merge(&plan, &[], WriteOptions { block_size, ..WriteOptions::default() })?;
    }

    /// The first input was written before the schema gained its column and stored fields (SPEC §6.2.4).
    #[test]
    fn merge_across_schema_evolution(plan in arb_inputs(true)) {
        let old = old_schema();
        let full = schema();
        prop_assert_eq!(old.fields(), &full.fields()[..3]);
        check_merge(&plan, &[true], WriteOptions::default())?;
    }

    /// Any input, in any position, may have been written before the schema gained fields (SPEC §5.5, §6.2.4).
    #[test]
    fn merge_with_old_inputs_anywhere(
        (plan, old) in arb_mixed_inputs(),
        block_size in prop::sample::select(vec![61_u32, 4096]),
    ) {
        check_merge(&plan, &old, WriteOptions { block_size, ..WriteOptions::default() })?;
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
        MergeError::DuplicateLiveId {
            segment: 101,
            id: "same".into()
        }
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

/// A document of [`schema`] with every field kind set from `i`, and a blob ref on every third.
fn full_doc(id: &str, i: u32) -> Document {
    let s = schema();
    let w = |k: u32| WORDS[((i + k) as usize) % WORDS.len()];
    let mut d = Document::new(id)
        .with(
            field(&s, "title"),
            Value::String(format!("{} {}", w(0), w(1))),
        )
        .with(
            field(&s, "body"),
            Value::String(format!("running {}", w(2))),
        )
        .with(
            field(&s, "tag"),
            Value::Strings(vec![TAGS[(i % 5) as usize].to_owned()]),
        )
        .with(field(&s, "price"), Value::F64(f64::from(i) - 0.5))
        .with(field(&s, "qty"), Value::I64(i64::from(i) * -3))
        .with(
            field(&s, "at"),
            Value::Datetime(1_600_000_000_000_000 + i64::from(i)),
        )
        .with(field(&s, "flag"), Value::Bool(i.is_multiple_of(2)))
        .with(field(&s, "raw"), Value::Json(serde_json::json!({ "i": i })));
    if i.is_multiple_of(3) {
        d.set_blob_ref(Some(blob(i as u8)));
    }
    d
}

/// `n` documents of [`full_doc`], ids `{prefix}{i}`.
fn full_docs(prefix: &str, n: u32) -> Vec<Document> {
    (0..n)
        .map(|i| full_doc(&format!("{prefix}{i}"), i))
        .collect()
}

/// [`check_merge`] at a small and the default block size.
fn check_plan(plan: &Plan, old: &[bool]) {
    for block_size in [61, 4096] {
        let options = WriteOptions {
            block_size,
            ..WriteOptions::default()
        };
        check_merge(plan, old, options).unwrap();
    }
}

#[test]
fn deletions_that_leave_one_document_per_input() {
    // The survivor first, in the middle, last.
    let plan: Plan = vec![
        (full_docs("a", 5), vec![1, 2, 3, 4], 0),
        (full_docs("b", 5), vec![0, 1, 3, 4], 2),
        (full_docs("c", 5), vec![0, 1, 2, 3], 1),
    ];
    check_plan(&plan, &[]);
    // One input entirely deleted between them, and its generation still counts (SPEC §5.1: "one more than the
    // highest input").
    let mut plan = plan;
    plan.insert(1, (full_docs("d", 3), vec![0, 1, 2], 4));
    check_plan(&plan, &[]);
}

#[test]
fn single_document_inputs() {
    let plan: Plan = (0..4)
        .map(|i| {
            (
                full_docs(&format!("s{i}-"), 1),
                if i == 2 { vec![0] } else { vec![] },
                0,
            )
        })
        .collect();
    check_plan(&plan, &[]);
    check_plan(&plan[..1].to_vec(), &[]);
}

#[test]
fn inputs_without_text_values() {
    let s = schema();
    let columns_only = |id: &str, i: i64| {
        Document::new(id)
            .with(field(&s, "qty"), Value::I64(i))
            .with(field(&s, "flag"), Value::Bool(i > 0))
            .with(field(&s, "raw"), Value::Json(serde_json::json!(i)))
    };
    let bare = vec![Document::new("bare-0"), Document::new("bare-1")];
    let columns = vec![
        columns_only("col-0", -1),
        columns_only("col-1", 7),
        columns_only("col-2", 9),
    ];
    // Text-less inputs alone, then around an input with text, with deletions.
    check_plan(&vec![(bare.clone(), vec![], 0)], &[]);
    check_plan(&vec![(columns.clone(), vec![1], 0)], &[]);
    let plan: Plan = vec![
        (bare, vec![0], 0),
        (full_docs("t", 4), vec![2], 0),
        (columns, vec![], 0),
    ];
    check_plan(&plan, &[]);
    check_plan(&plan, &[true, false, false]);
}

#[test]
fn keyword_empty_and_repeated_values() {
    let s = schema();
    let tag = field(&s, "tag");
    let tags = |v: &[&str]| Value::Strings(v.iter().map(|&t| t.to_owned()).collect());
    let a = vec![
        Document::new("k0").with(tag, tags(&["gone-only", ""])),
        Document::new("k1").with(tag, tags(&["", "red", "red", ""])),
        Document::new("k2").with(tag, tags(&[])),
        Document::new("k3").with(tag, Value::String(String::new())),
    ];
    let b = vec![Document::new("k4").with(tag, tags(&["red", "", "red", "red"]))];
    let plan: Plan = vec![(a, vec![0], 0), (b, vec![], 0)];
    check_plan(&plan, &[]);

    // And explicitly: tf counts repeats, the empty string is a term, a deleted-only value is gone.
    let inputs = Inputs::new(
        &s,
        plan.iter()
            .enumerate()
            .map(|(i, (docs, _, g))| write(&s, docs, input_meta(i, *g), WriteOptions::default()))
            .collect(),
        &[vec![0], vec![]],
    );
    let merged = merge(&s, &registry(), &inputs.sources()).unwrap().unwrap();
    let out = open(
        &s,
        merged
            .write(OUTPUT_ID, CREATED_AT, WriteOptions::default())
            .unwrap(),
    );
    let got = postings_of(&out, tag, Some);
    let tfs = |t: &[u8]| -> Vec<(u32, u32)> { got[t].iter().map(|p| (p.0, p.1)).collect() };
    assert!(!got.contains_key(b"gone-only".as_slice()));
    assert_eq!(tfs(b""), [(0, 2), (2, 1), (3, 1)]);
    assert_eq!(tfs(b"red"), [(0, 2), (3, 3)]);
}

#[test]
fn multi_valued_text_with_empty_values() {
    let s = schema();
    let (title, body) = (field(&s, "title"), field(&s, "body"));
    let multi = |id: &str, v: &[&str]| {
        Document::new(id)
            .with(
                title,
                Value::Strings(v.iter().map(|&t| t.to_owned()).collect()),
            )
            .with(
                body,
                Value::Strings(v.iter().map(|&t| t.to_owned()).collect()),
            )
    };
    let a = vec![
        multi("m0", &["", "alpha beta", "", "beta"]),
        multi("m1", &["gone words"]),
        multi("m2", &["", ""]),
        multi("m3", &["x", "x x", "runs running"]),
    ];
    let b = vec![multi("m4", &["the of", "", "Ünïcode Gamma gamma"])];
    let plan: Plan = vec![(a, vec![1], 0), (b, vec![], 3)];
    check_plan(&plan, &[]);
    check_plan(&plan, &[false, true]);
}

#[test]
fn old_inputs_in_every_position() {
    let s = schema();
    let title_only = |p: &str| -> Vec<Document> {
        (0..3_usize)
            .map(|i| doc(&format!("{p}{i}"), WORDS[i]))
            .collect()
    };
    for old_at in 0..3 {
        let plan: Plan = (0..3)
            .map(|i| {
                let docs = if i == old_at {
                    title_only(&format!("o{i}-"))
                } else {
                    full_docs(&format!("n{i}-"), 4)
                };
                (docs, vec![1], i as u32)
            })
            .collect();
        let mut old = vec![false; 3];
        old[old_at] = true;
        check_plan(&plan, &old);
        // The columns added after the old input was written read as missing for its documents.
        let first_old_docid = (0..old_at).map(|_| 3).sum::<u32>();
        let inputs = Inputs::new(
            &s,
            plan.iter()
                .enumerate()
                .map(|(i, (docs, _, g))| {
                    let schema = if old[i] { old_schema() } else { s.clone() };
                    write(&schema, docs, input_meta(i, *g), WriteOptions::default())
                })
                .collect(),
            &[vec![1], vec![1], vec![1]],
        );
        let merged = merge(&s, &registry(), &inputs.sources()).unwrap().unwrap();
        let out = open(
            &s,
            merged
                .write(OUTPUT_ID, CREATED_AT, WriteOptions::default())
                .unwrap(),
        );
        for d in first_old_docid..first_old_docid + 2 {
            assert_eq!(
                out.numeric_column(field(&s, "qty"))
                    .unwrap()
                    .get(d)
                    .unwrap(),
                None
            );
            assert_eq!(out.bool_column(field(&s, "flag")).unwrap().get(d), None);
        }
    }
}

/// Inputs with these merge generations, one document each, the `deleted`-th input's deleted.
fn generation_inputs(generations: &[u32], deleted: Option<usize>) -> Inputs {
    let s = schema();
    let bytes = generations
        .iter()
        .enumerate()
        .map(|(i, &g)| {
            write(
                &s,
                &[doc(&format!("g{i}"), "alpha")],
                input_meta(i, g),
                WriteOptions::default(),
            )
        })
        .collect();
    let gone: Vec<Vec<u32>> = (0..generations.len())
        .map(|i| if Some(i) == deleted { vec![0] } else { vec![] })
        .collect();
    Inputs::new(&s, bytes, &gone)
}

#[test]
fn merge_generation_is_one_more_than_the_highest_input() {
    let s = schema();
    for (generations, deleted, want) in [
        (vec![0], None, 1),
        (vec![3, 0, 7], None, 8),
        (vec![7, 3, 0], None, 8),
        // A fully deleted input is still an input (SPEC §5.1).
        (vec![1, 9, 2], Some(1), 10),
        (vec![u32::MAX - 1, 0], None, u32::MAX),
    ] {
        let inputs = generation_inputs(&generations, deleted);
        let merged = merge(&s, &registry(), &inputs.sources()).unwrap().unwrap();
        assert_eq!(merged.merge_generation(), want, "{generations:?}");
        let out = open(
            &s,
            merged
                .write(OUTPUT_ID, CREATED_AT, WriteOptions::default())
                .unwrap(),
        );
        assert_eq!(out.merge_generation(), want);
    }
}

#[test]
fn merge_generation_overflow_is_an_error() {
    let s = schema();
    let too_large = MergeError::Build(SegmentError::TooLarge {
        what: "merge generations",
    });
    for generations in [
        vec![u32::MAX],
        vec![0, u32::MAX],
        vec![u32::MAX, u32::MAX - 1],
    ] {
        let inputs = generation_inputs(&generations, None);
        assert_eq!(
            merge(&s, &registry(), &inputs.sources()).unwrap_err(),
            too_large,
            "{generations:?}"
        );
    }
    // Nothing live: nothing to write, so no generation to compute.
    let inputs = generation_inputs(&[u32::MAX], Some(0));
    assert!(merge(&s, &registry(), &inputs.sources()).unwrap().is_none());
}

#[test]
fn all_deleted_inputs_merge_to_nothing() {
    let s = schema();
    let bytes = (0..3)
        .map(|i| {
            write(
                &s,
                &full_docs(&format!("x{i}-"), 3 + i as u32),
                input_meta(i, 0),
                WriteOptions::default(),
            )
        })
        .collect();
    let inputs = Inputs::new(
        &s,
        bytes,
        &[vec![0, 1, 2], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4]],
    );
    assert!(merge(&s, &registry(), &inputs.sources()).unwrap().is_none());
}

/// Opens `bytes` and merges it alone and with every other document deleted; never panics. Returns whether a merge
/// succeeded, or why it broke the contract: a merge of damaged input returns an error, or a segment that writes and
/// opens, and that also verifies when the input does (CLAUDE.md "Safety & errors"). When the input fails
/// [`Segment::verify`] (damage only the whole-segment checks see, not `open`), the merge may copy that damage into
/// a merged segment that fails `verify` too (still detected, never a panic).
fn merge_damaged(
    schema: &CollectionSchema,
    registry: &AnalyzerRegistry,
    bytes: Vec<u8>,
) -> Result<bool, String> {
    let Ok(seg) = Segment::open(ByteSource::new(bytes.into()), schema) else {
        return Ok(false);
    };
    let input_verifies = seg.verify().is_ok();
    let odd = seg
        .tombstones((1..seg.doc_count()).step_by(2).collect())
        .unwrap();
    let mut merged_any = false;
    for tombstones in [None, Some(&odd)] {
        let sources = [MergeSource {
            segment: &seg,
            tombstones,
        }];
        match merge(schema, registry, &sources) {
            Err(MergeError::Unsupported { .. }) => {
                return Err("Unsupported for damaged sections".into());
            }
            Err(_) | Ok(None) => {}
            Ok(Some(merged)) => {
                merged_any = true;
                let out = merged
                    .write(OUTPUT_ID, CREATED_AT, WriteOptions::default())
                    .map_err(|e| format!("merge accepted input it cannot write: {e}"))?;
                let out = Segment::open(ByteSource::new(out.into()), schema)
                    .map_err(|e| format!("merged segment does not open: {e}"))?;
                match (out.verify(), input_verifies) {
                    (Ok(()), _) | (Err(_), false) => {}
                    (Err(e), true) => {
                        return Err(format!("input verifies, merged segment does not: {e}"));
                    }
                }
            }
        }
    }
    Ok(merged_any)
}

/// Every block-framed section of a small segment damaged throughout its decoded stream, behind valid CRCs: a byte
/// flipped or set, the stream cut, a byte inserted. A deterministic cousin of the segment fuzz target's mode 1. By
/// default every 7th offset gets one edit (rotating through them); with the `slow` feature every offset gets all.
#[test]
fn damage_behind_valid_crcs_never_panics_a_merge() {
    type Edit = fn(&mut Vec<u8>, usize);
    const EDITS: [(&str, Edit); 6] = [
        ("xor 0x01", |s, at| s[at] ^= 0x01),
        ("xor 0x80", |s, at| s[at] ^= 0x80),
        ("set 0xff", |s, at| s[at] = 0xff),
        ("set 0x00", |s, at| s[at] = 0x00),
        ("truncate", |s, at| s.truncate(at)),
        ("insert 0x7f", |s, at| s.insert(at, 0x7f)),
    ];
    let s = schema();
    let registry = registry();
    let mut docs = full_docs("c", 10);
    docs[4].insert(
        field(&s, "title"),
        Value::Strings(vec!["alpha alpha".into(), String::new(), "beta".into()]),
    );
    docs[5].insert(
        field(&s, "tag"),
        Value::Strings(vec!["red".into(), String::new(), "red".into()]),
    );
    docs.push(Document::new("c-bare"));
    let options = WriteOptions {
        block_size: 64,
        ..WriteOptions::default()
    };
    let bytes = write(&s, &docs, input_meta(0, 0), options);
    let split = Split::new(&bytes);
    // The harness is faithful: re-laying out an unchanged stream gives a segment that merges.
    for (which, (_, _, stream, _)) in split.framed.iter().enumerate() {
        assert_eq!(
            merge_damaged(&s, &registry, split.with_stream(which, stream)),
            Ok(true)
        );
    }
    let slow = cfg!(feature = "slow");
    let mut failures = Vec::new();
    let (mut cases, mut merged) = (0, 0);
    for (which, (_, kind, stream, _)) in split.framed.iter().enumerate() {
        for at in (0..=stream.len()).step_by(if slow { 1 } else { 7 }) {
            let edits: &[(&str, Edit)] = if slow {
                &EDITS
            } else {
                let k = (at / 7 + which) % EDITS.len();
                &EDITS[k..=k]
            };
            for (name, edit) in edits {
                let mut edited = stream.clone();
                // Byte edits need a byte; at the end of the stream, insert instead.
                if at < stream.len() || *name == "insert 0x7f" {
                    edit(&mut edited, at);
                } else {
                    edited.push(0x7f);
                }
                if edited == *stream {
                    continue;
                }
                cases += 1;
                let damaged = split.with_stream(which, &edited);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    merge_damaged(&s, &registry, damaged)
                }));
                match outcome {
                    Ok(Ok(m)) => merged += usize::from(m),
                    Ok(Err(e)) => failures.push(format!("{kind:?} @{at} {name}: {e}")),
                    Err(_) => failures.push(format!("{kind:?} @{at} {name}: panicked")),
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {cases} damaged inputs broke the merge contract:\n{}",
        failures.len(),
        failures.join("\n")
    );
    // The edits reach the merge, not only the reader's open checks.
    assert!(merged > 0, "no damaged input opened and merged");
}

#[test]
fn an_input_opened_with_another_schema_is_refused() {
    let old = old_schema();
    let bytes = write(
        &old,
        &[doc("a", "alpha")],
        input_meta(0, 0),
        WriteOptions::default(),
    );
    let a = open(&old, bytes);
    assert_eq!(
        merge(
            &schema(),
            &registry(),
            &[MergeSource {
                segment: &a,
                tombstones: None
            }]
        )
        .unwrap_err(),
        MergeError::SchemaMismatch { segment: 100 }
    );
}
