//! Segment reader (SPEC §5, ROADMAP 5.3): everything a segment holds reads back as the model of its documents;
//! postings iteration (next, advance, shallow advance, block-max data, tombstones); term enumeration; the footer
//! rules of SPEC §5.1, each broken on its own; caller errors.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, FieldSet, Value};
use persia_engine::schema::{
    CollectionSchema, FieldKind, FieldType, KeywordOptions, SchemaBuilder,
};
use persia_engine::segment::{
    BlockCache, CacheStats, ReadError, Segment, SegmentCorruption, TermIndex, Tombstones,
    UnsupportedSegment, WriteOptions,
};
use persia_format::checksum::crc32c;
use persia_format::for_codec::{f64_to_key, i64_to_key};
use persia_format::{BlobRef, ByteSource};
use proptest::prelude::*;
use roaring::RoaringBitmap;

use super::model::{self, Index, Occurrence};
use super::{arb_doc, build, fixture_path, golden_docs, id, meta, schema, with_ids};

/// Everything a segment holds, read through the public reader.
#[derive(Debug, Default, PartialEq)]
pub struct ReadBack {
    pub terms: BTreeMap<u32, Index>,
    pub norms: BTreeMap<u32, Vec<u8>>,
    /// Numeric and bool columns: doc -> key (0 | 1 for bool).
    pub columns: BTreeMap<u32, BTreeMap<u32, u64>>,
    pub keywords: BTreeMap<u32, BTreeMap<u32, Vec<Vec<u8>>>>,
    pub docs: Vec<Document>,
    pub ids: Vec<String>,
    pub blobs: Vec<Option<BlobRef>>,
}

pub fn open(schema: &CollectionSchema, bytes: &[u8]) -> Result<Segment, ReadError> {
    Segment::open(ByteSource::new(bytes.to_vec().into()), schema)
}

/// Every term with all its postings, frequencies and positions.
fn read_index(terms: &TermIndex<'_>, positions: bool) -> Result<Index, ReadError> {
    let mut index = Index::default();
    let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
    let mut ordinal = 0;
    while let Some(next) = stream.next() {
        let (term, info) = next?;
        assert_eq!(info.ordinal(), ordinal);
        ordinal += 1;
        let term = term.to_vec();
        let mut postings = terms.postings(&info)?;
        let mut occ = Vec::new();
        while let Some(doc) = postings.next()? {
            let tf = postings.freq()?;
            let pos = postings.positions()?.to_vec();
            assert_eq!(pos.is_empty(), !positions);
            occ.push(Occurrence {
                doc,
                tf,
                positions: pos,
            });
        }
        assert_eq!(occ.len() as u32, info.df());
        assert_eq!(occ.iter().map(|o| u64::from(o.tf)).sum::<u64>(), info.ttf());
        index.terms.insert(term, occ);
    }
    assert_eq!(ordinal, terms.len());
    Ok(index)
}

/// Reads everything, then checks the whole-segment rules too (`Segment::verify`, ROADMAP 5.4.1); an `Err` as soon
/// as anything is corrupt.
pub fn read_back(schema: &CollectionSchema, bytes: &[u8]) -> Result<ReadBack, ReadError> {
    let out = read_lookups(schema, bytes)?;
    open(schema, bytes)?.verify()?;
    Ok(out)
}

/// Reads everything through the lookups alone (no `verify`): what a query path checks.
pub fn read_lookups(schema: &CollectionSchema, bytes: &[u8]) -> Result<ReadBack, ReadError> {
    let seg = open(schema, bytes)?;
    let n = seg.doc_count();
    let mut out = ReadBack::default();
    for field in schema.fields() {
        let fid = field.id();
        match field.kind() {
            FieldKind::Text(o) => {
                out.terms
                    .insert(fid.get(), read_index(&seg.terms(fid)?, o.positions)?);
                let norms = seg.norms(fid)?;
                out.norms
                    .insert(fid.get(), (0..n).map(|d| norms.get(d)).collect());
            }
            FieldKind::Keyword(_) => {
                out.terms
                    .insert(fid.get(), read_index(&seg.terms(fid)?, false)?);
                let col = seg.keyword_column(fid)?;
                let mut values = BTreeMap::new();
                for d in 0..n {
                    let v: Vec<Vec<u8>> = col.values(d)?.into_iter().map(<[u8]>::to_vec).collect();
                    assert_eq!(v.len(), col.ords(d)?.len());
                    if !v.is_empty() {
                        values.insert(d, v);
                    }
                }
                out.keywords.insert(fid.get(), values);
            }
            FieldKind::Stored => {}
            FieldKind::Bool { .. } => {
                let col = seg.bool_column(fid)?;
                let values = (0..n)
                    .filter_map(|d| col.get(d).map(|v| (d, u64::from(v))))
                    .collect();
                out.columns.insert(fid.get(), values);
            }
            _ => {
                let col = seg.numeric_column(fid)?;
                let mut values = BTreeMap::new();
                for d in 0..n {
                    if let Some(key) = col.key(d)? {
                        let typed = col.get(d)?.unwrap();
                        let typed_key = match (field.field_type(), typed) {
                            (FieldType::F64, Value::F64(v)) => f64_to_key(v),
                            (FieldType::I64, Value::I64(v))
                            | (FieldType::Datetime, Value::Datetime(v)) => i64_to_key(v),
                            other => panic!("{other:?}"),
                        };
                        assert_eq!(typed_key, key);
                        values.insert(d, key);
                    }
                }
                out.columns.insert(fid.get(), values);
            }
        }
    }
    for d in 0..n {
        out.docs.push(seg.document(d)?);
        let doc_id = seg.id_of(d)?;
        // `Segment::verify` (in `read_back`) checks that the IdMap maps the id back to `d`.
        out.ids.push(doc_id);
        out.blobs.push(seg.blob_ref(d)?);
    }
    Ok(out)
}

/// What [`read_back`] must return for `docs`, from the model.
pub fn expected(schema: &CollectionSchema, docs: &[Document]) -> ReadBack {
    let registry = AnalyzerRegistry::builtin();
    let n = docs.len() as u32;
    let mut out = ReadBack::default();
    for field in schema.fields() {
        let fid = field.id();
        match field.kind() {
            FieldKind::Text(o) => {
                let analyzer = registry
                    .get(o.analyzer.name(), o.analyzer.version())
                    .unwrap();
                let (index, lengths) = model::index(docs, fid, Some(analyzer), o.positions);
                out.terms.insert(fid.get(), index);
                out.norms.insert(
                    fid.get(),
                    (0..n)
                        .map(|d| lengths.get(&d).map_or(0, |&l| model::norm(l)))
                        .collect(),
                );
            }
            FieldKind::Keyword(_) => {
                let (index, _) = model::index(docs, fid, None, false);
                let mut values: BTreeMap<u32, Vec<Vec<u8>>> = BTreeMap::new();
                for (term, occ) in &index.terms {
                    for o in occ {
                        values.entry(o.doc).or_default().push(term.clone());
                    }
                }
                out.keywords.insert(fid.get(), values);
                out.terms.insert(fid.get(), index);
            }
            FieldKind::Stored => {}
            _ => {
                let values = (0_u32..)
                    .zip(docs)
                    .filter_map(|(d, doc)| {
                        let key = match *doc.get(fid)? {
                            Value::I64(v) | Value::Datetime(v) => i64_to_key(v),
                            Value::F64(v) => f64_to_key(v),
                            Value::Bool(v) => u64::from(v),
                            _ => unreachable!(),
                        };
                        Some((d, key))
                    })
                    .collect();
                out.columns.insert(fid.get(), values);
            }
        }
    }
    for d in docs {
        let row = d.encode(schema, FieldSet::Stored).unwrap();
        out.docs.push(Document::decode(&row, schema).unwrap());
        out.ids.push(d.id().to_owned());
        out.blobs.push(d.blob_ref().cloned());
    }
    out
}

pub fn check_reads(schema: &CollectionSchema, docs: &[Document], bytes: &[u8]) {
    let got = read_back(schema, bytes).unwrap();
    let want = expected(schema, docs);
    assert_eq!(got.terms, want.terms);
    assert_eq!(got.norms, want.norms);
    assert_eq!(got.columns, want.columns);
    assert_eq!(got.keywords, want.keywords);
    assert_eq!(got.ids, want.ids);
    assert_eq!(got.blobs, want.blobs);
    assert_eq!(got.docs, want.docs);

    let seg = open(schema, bytes).unwrap();
    assert_eq!(seg.segment_id(), meta().segment_id);
    assert_eq!(seg.created_at(), meta().created_at);
    assert_eq!(seg.merge_generation(), meta().merge_generation);
    assert_eq!(seg.schema_field_count() as usize, schema.fields().len());
    assert_eq!((seg.format_minor(), seg.rocompat_flags()), (0, 0));
    let ids: BTreeSet<&str> = docs.iter().map(Document::id).collect();
    let (lo, hi) = seg.id_range();
    assert_eq!(
        (lo, hi),
        (
            ids.first().unwrap().as_bytes(),
            ids.last().unwrap().as_bytes()
        )
    );
    assert_eq!(seg.docid_of("no such id").unwrap(), None);
    for field in schema.fields() {
        if let Some(stats) = seg.field_stats(field.id()) {
            let index = &want.terms[&field.id().get()];
            let docs_with: BTreeSet<u32> = index.terms.values().flatten().map(|o| o.doc).collect();
            assert_eq!(stats.docs_with_field as usize, docs_with.len());
        } else {
            assert!(!matches!(
                field.field_type(),
                FieldType::Text | FieldType::Keyword
            ));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 256 } else { 24 }, ..ProptestConfig::default() })]

    /// Everything reads back as the model, at block sizes that make records and lists span blocks.
    #[test]
    fn reader_matches_model(
        docs in prop::collection::vec(arb_doc(schema()), 1..400),
        block_size in prop::sample::select(vec![1_u32, 61, 4096, 16 * 1024]),
    ) {
        let schema = schema();
        let docs = with_ids(docs);
        let bytes = build(&schema, &docs, WriteOptions { block_size, ..WriteOptions::default() });
        check_reads(&schema, &docs, &bytes);
    }
}

#[test]
fn golden_segment_reads_as_its_documents() {
    let schema = schema();
    let stored = std::fs::read(fixture_path()).unwrap();
    check_reads(&schema, &golden_docs(&schema), &stored);
}

/// `Segment::sections` is the section table (SPEC §5.1) as `decode` parses it independently: kinds, fields, offsets,
/// lengths, and for each block-framed section its stream length, block count and block size.
#[test]
fn section_table_matches_the_decoder() {
    let schema = schema();
    let stored = std::fs::read(fixture_path()).unwrap();
    let seg = open(&schema, &stored).unwrap();
    let sections = seg.sections().unwrap();
    let decoded = super::decode::parse(&stored);
    assert_eq!(sections.len(), decoded.sections.len());
    let mut end = 0;
    for s in &sections {
        assert!(s.offset >= end && s.offset % 8 == 0, "{s:?}");
        end = s.offset + s.len;
        let section = &stored[s.offset as usize..end as usize];
        assert!(
            decoded.sections.contains_key(&(s.kind.code(), s.field)),
            "{s:?}"
        );
        match s.stream {
            Some(st) => {
                assert!(s.kind.is_block_framed());
                let r = persia_format::block::BlockReader::open(section, 0).unwrap();
                assert_eq!(
                    (st.len, st.block_count, st.block_size),
                    (r.len(), r.block_count(), r.block_size())
                );
            }
            None => assert!(!s.kind.is_block_framed(), "{s:?}"),
        }
    }
}

/// `n` documents with tags at different densities: lists of many full blocks, a tail, and single blocks.
fn dense(n: u32) -> (CollectionSchema, Vec<Document>, Vec<u8>) {
    let schema = schema();
    let tag = id(&schema, "tag");
    let title = id(&schema, "title");
    let docs: Vec<Document> = (0..n)
        .map(|i| {
            let mut tags = vec!["all".to_owned()];
            for (k, name) in [(2, "half"), (3, "third"), (97, "rare"), (1000, "sparse")] {
                if i % k == 0 {
                    tags.push(name.to_owned());
                }
            }
            let words = "alpha beta alpha ".repeat((i % 4) as usize + 1);
            Document::new(format!("d{i:05}"))
                .with(tag, Value::Strings(tags))
                .with(title, Value::String(words))
        })
        .collect();
    let bytes = build(
        &schema,
        &docs,
        WriteOptions {
            block_size: 997,
            ..WriteOptions::default()
        },
    );
    (schema, docs, bytes)
}

fn model_list(docs: &[Document], schema: &CollectionSchema, term: &str) -> Vec<(u32, u32)> {
    let (index, _) = model::index(docs, id(schema, "tag"), None, false);
    index.terms[term.as_bytes()]
        .iter()
        .map(|o| (o.doc, o.tf))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 256 } else { 32 }, ..ProptestConfig::default() })]

    /// Interleaved `next`, `advance` and `shallow_advance` agree with the model at every step.
    #[test]
    fn advance_matches_model(
        term in prop::sample::select(vec!["all", "half", "third", "rare", "sparse"]),
        steps in prop::collection::vec((0_u8..3, 0_u32..2200), 1..60),
        deleted in prop::collection::btree_set(0_u32..2100, 0..300),
    ) {
        static DENSE: std::sync::OnceLock<(CollectionSchema, Vec<Document>, Vec<u8>)> = std::sync::OnceLock::new();
        let (schema, docs, bytes) = DENSE.get_or_init(|| dense(2100));
        let seg = open(schema, bytes).unwrap();
        let tombstones = seg.tombstones(deleted.iter().copied().collect()).unwrap();
        let mut terms = seg.terms(id(schema, "tag")).unwrap();
        let info = terms.get(term.as_bytes()).unwrap().unwrap();
        let all = model_list(docs, schema, term);
        let live: Vec<(u32, u32)> = all.iter().copied().filter(|(d, _)| !deleted.contains(d)).collect();
        let mut p = terms.postings(&info).unwrap().with_tombstones(&tombstones);
        prop_assert_eq!(p.df() as usize, all.len());
        // Index into `live` of the current document; None before the first.
        let mut at: Option<usize> = None;
        for (op, target) in steps {
            match op {
                0 => {
                    let next = at.map_or(0, |i| i + 1);
                    let got = p.next().unwrap();
                    prop_assert_eq!(got, live.get(next).map(|e| e.0));
                    at = Some(next.min(live.len()));
                }
                1 => {
                    let from = at.unwrap_or(0);
                    let i = from + live[from.min(live.len())..].iter().position(|e| e.0 >= target).unwrap_or(live.len() - from.min(live.len()));
                    let got = p.advance(target).unwrap();
                    prop_assert_eq!(got, live.get(i).map(|e| e.0), "advance({})", target);
                    at = Some(i.min(live.len()));
                }
                _ => {
                    let got = p.shallow_advance(target).unwrap();
                    let first = all.iter().find(|e| e.0 >= target);
                    match (got, first) {
                        (None, None) => {}
                        (Some(m), Some(&(d, tf))) => {
                            prop_assert!(m.last_docid >= d, "{m:?} for {target}");
                            prop_assert!(tf <= m.max_tf);
                            prop_assert_eq!(m.min_norm, 0);
                        }
                        other => prop_assert!(false, "shallow_advance({}): {:?}", target, other),
                    }
                }
            }
            if at.is_some_and(|i| i >= live.len()) {
                prop_assert_eq!(p.doc(), None);
                prop_assert_eq!(p.next().unwrap(), None);
                break;
            }
            if let Some(i) = at {
                prop_assert_eq!(p.doc(), Some(live[i].0));
                prop_assert_eq!(p.freq().unwrap(), live[i].1);
                let m = p.block_max().unwrap();
                prop_assert!(m.last_docid >= live[i].0 && m.max_tf >= live[i].1);
            }
        }
    }
}

#[test]
fn block_max_bounds_every_block() {
    let (schema, docs, bytes) = dense(2100);
    let seg = open(&schema, &bytes).unwrap();
    let title = id(&schema, "title");
    let norms = seg.norms(title).unwrap();
    let mut terms = seg.terms(title).unwrap();
    let info = terms.get(b"alpha").unwrap().unwrap();
    assert_eq!(info.df(), 2100);
    let mut p = terms.postings(&info).unwrap();
    let mut blocks = BTreeSet::new();
    while let Some(d) = p.next().unwrap() {
        let m = p.block_max().unwrap();
        let tf = p.freq().unwrap();
        assert!(d <= m.last_docid && tf <= m.max_tf && norms.get(d) >= m.min_norm);
        assert!(info.max_tf() >= m.max_tf && info.min_norm() <= m.min_norm);
        // Positions: "alpha beta alpha" repeated, so alpha sits at 0, 2, 3, 5, ...
        let pos = p.positions().unwrap();
        assert_eq!(pos.len() as u32, tf);
        assert!(pos.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(pos[0], 0);
        blocks.insert(m.last_docid);
    }
    assert_eq!(blocks.len(), 2100_usize.div_ceil(128));
    assert_eq!(docs.len(), 2100);
    assert_eq!(norms.length(5), 6);
}

#[test]
fn tombstones_out_of_range_are_rejected() {
    let (schema, _, bytes) = dense(300);
    let seg = open(&schema, &bytes).unwrap();
    let err = seg
        .tombstones(RoaringBitmap::from_iter([1, 300]))
        .unwrap_err();
    assert_eq!(
        err,
        ReadError::Tombstone {
            docid: 300,
            doc_count: 300
        }
    );
    assert!(
        !err.is_corruption(),
        "the sidecar is corrupt, not the segment"
    );
    let all = Tombstones::new((0..300).collect(), 300).unwrap();
    assert_eq!(all.len(), 300);
    let mut terms = seg.terms(id(&schema, "tag")).unwrap();
    let info = terms.get(b"all").unwrap().unwrap();
    let mut p = terms.postings(&info).unwrap().with_tombstones(&all);
    assert_eq!(p.next().unwrap(), None);
    let mut p = terms.postings(&info).unwrap().with_tombstones(&all);
    assert_eq!(p.advance(10).unwrap(), None);
}

#[test]
fn term_enumeration() {
    let schema = schema();
    let docs = golden_docs(&schema);
    let bytes = build(&schema, &docs, WriteOptions::default());
    let seg = open(&schema, &bytes).unwrap();
    let title = id(&schema, "title");
    let terms = seg.terms(title).unwrap();
    let registry = AnalyzerRegistry::builtin();
    let analyzer = registry.get("standard", 1).unwrap();
    let (index, _) = model::index(&docs, title, Some(analyzer), true);
    let all: Vec<Vec<u8>> = index.terms.keys().cloned().collect();
    let collect = |s| collect_terms(s, &index);
    for prefix in [&b""[..], b"r", b"ru", b"run", b"\xff", b"alpha", b"zzz"] {
        let want: Vec<_> = all
            .iter()
            .filter(|t| t.starts_with(prefix))
            .cloned()
            .collect();
        assert_eq!(collect(terms.prefix(prefix)), want, "prefix {prefix:?}");
    }
    let want: Vec<_> = all
        .iter()
        .filter(|t| t.as_slice() > &b"beta"[..] && t.as_slice() <= &b"run"[..])
        .cloned()
        .collect();
    assert_eq!(
        collect(terms.range(Bound::Excluded(b"beta"), Bound::Included(b"run"))),
        want
    );
    // The automaton hook M8's fuzzy matching uses (a Levenshtein automaton there).
    let sub = fst::automaton::Subsequence::new("rn");
    let want: Vec<_> = all
        .iter()
        .filter(|t| {
            let s = std::str::from_utf8(t).unwrap();
            s.split_once('r')
                .is_some_and(|(_, rest)| rest.contains('n'))
        })
        .cloned()
        .collect();
    assert!(!want.is_empty());
    assert_eq!(collect_terms(terms.search(sub), &index), want);

    // A field without terms in the segment reads as empty.
    let tag = id(&schema, "tag");
    let single = build(
        &schema,
        &[Document::new("x").with(id(&schema, "qty"), Value::I64(1))],
        WriteOptions::default(),
    );
    let seg = open(&schema, &single).unwrap();
    let mut t = seg.terms(tag).unwrap();
    assert!(t.is_empty());
    assert_eq!(t.get(b"red").unwrap(), None);
    assert!(collect_terms(t.prefix(b""), &Index::default()).is_empty());
    assert!(seg.keyword_column(tag).unwrap().is_empty());
    assert_eq!(seg.norms(title).unwrap().get(0), 0);
    assert_eq!(seg.bool_column(id(&schema, "flag")).unwrap().get(0), None);
    assert_eq!(
        seg.numeric_column(id(&schema, "price"))
            .unwrap()
            .get(0)
            .unwrap(),
        None
    );
    assert_eq!(seg.blob_ref(0).unwrap(), None);
}

#[test]
fn caller_errors() {
    let schema = schema();
    let bytes = build(&schema, &golden_docs(&schema), WriteOptions::default());
    let seg = open(&schema, &bytes).unwrap();
    let qty = id(&schema, "qty");
    for err in [
        seg.terms(qty).map(drop).unwrap_err(),
        seg.norms(id(&schema, "tag")).map(drop).unwrap_err(),
        seg.numeric_column(id(&schema, "flag"))
            .map(drop)
            .unwrap_err(),
        seg.bool_column(qty).map(drop).unwrap_err(),
        seg.keyword_column(id(&schema, "title"))
            .map(drop)
            .unwrap_err(),
        seg.terms(id(&schema, "raw")).map(drop).unwrap_err(),
    ] {
        assert!(matches!(err, ReadError::FieldType { .. }), "{err}");
        assert!(!err.is_corruption());
    }
    let mut big = SchemaBuilder::new("big");
    for i in 0..20 {
        big = big.field(format!("f{i}"), FieldKind::I64 { stored: false });
    }
    let unknown = big.build().unwrap().field("f19").unwrap().id();
    assert_eq!(
        seg.terms(unknown).map(drop).unwrap_err(),
        ReadError::UnknownField { field: unknown }
    );
    for err in [
        seg.document(300).map(drop).unwrap_err(),
        seg.id_of(300).map(drop).unwrap_err(),
        seg.blob_ref(300).map(drop).unwrap_err(),
    ] {
        assert_eq!(
            err,
            ReadError::DocOutOfRange {
                docid: 300,
                doc_count: 300
            }
        );
    }
    assert_eq!(seg.document_by_id("nope").unwrap(), None);
    let (docid, doc) = seg.document_by_id("doc-007").unwrap().unwrap();
    assert_eq!((docid, doc.id()), (1, "doc-007"));
}

/// A segment written before fields were added opens with the evolved schema; another schema does not.
#[test]
fn schema_hash_is_checked_at_the_recorded_field_count() {
    let schema = schema();
    let bytes = build(&schema, &golden_docs(&schema), WriteOptions::default());
    let mut evolved = SchemaBuilder::new("things");
    for f in schema.fields() {
        evolved = evolved.field(f.name(), f.kind().clone());
    }
    let evolved = schema
        .evolve(
            &evolved
                .field("added", FieldKind::Keyword(KeywordOptions::default()))
                .build()
                .unwrap(),
        )
        .unwrap();
    let seg = open(&evolved, &bytes).unwrap();
    let added = evolved.field("added").unwrap().id();
    assert_eq!(seg.schema_field_count() as usize, schema.fields().len());
    assert!(seg.terms(added).unwrap().is_empty());
    assert_eq!(seg.field_stats(added), None);
    assert!(seg.keyword_column(added).unwrap().is_empty());
    // The whole-segment rules hold with the evolved schema too: only the first `schema_field_count` fields count.
    seg.verify().unwrap();

    let other = SchemaBuilder::new("other").build().unwrap();
    let err = open(&other, &bytes).unwrap_err();
    assert!(
        matches!(
            err,
            ReadError::Corrupt {
                reason: SegmentCorruption::SchemaFieldCount { .. },
                ..
            }
        ),
        "{err}"
    );
    let mut renamed = SchemaBuilder::new("renamed");
    for f in schema.fields() {
        renamed = renamed.field(f.name(), f.kind().clone());
    }
    let err = open(&renamed.build().unwrap(), &bytes).unwrap_err();
    assert!(
        matches!(
            err,
            ReadError::Corrupt {
                reason: SegmentCorruption::SchemaHash { .. },
                ..
            }
        ),
        "{err}"
    );
}

/// The footer of `bytes` without its CRC, handed to `edit`; CRC and locator are recomputed.
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

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Offset of section-table entry `i` in the footer body.
fn entry(body: &[u8], i: usize) -> usize {
    88 + 16 * u32_at(body, 80) as usize + 24 * i
}

/// Offset of `min_id`'s length in the footer body.
fn ids_at(body: &[u8]) -> usize {
    entry(body, u32_at(body, 84) as usize)
}

fn corruption(err: &ReadError) -> Option<SegmentCorruption> {
    match err {
        ReadError::Corrupt { reason, .. } | ReadError::Section { reason, .. } => Some(*reason),
        _ => None,
    }
}

type Edit = fn(&mut Vec<u8>);
type Want = fn(&SegmentCorruption) -> bool;

/// Each rule of SPEC §5.1, broken on its own with a valid CRC.
#[test]
fn footer_rules() {
    use SegmentCorruption as C;
    use persia_format::Corruption as F;
    let schema = schema();
    let bytes = build(&schema, &golden_docs(&schema), WriteOptions::default());
    let cases: Vec<(&str, Edit, Want)> = vec![
        (
            "reserved",
            |b| b[4] = 1,
            |c| *c == C::Format(F::NonZeroReserved { value: 1 }),
        ),
        (
            "schema reserved",
            |b| b[57] = 1,
            |c| *c == C::Format(F::NonZeroReserved { value: 1 }),
        ),
        (
            "field count",
            |b| put_u32(b, 60, 99),
            |c| matches!(c, C::SchemaFieldCount { recorded: 99, .. }),
        ),
        (
            "fewer fields",
            |b| put_u32(b, 60, 3),
            |c| matches!(c, C::SchemaHash { .. }),
        ),
        (
            "hash",
            |b| b[40] ^= 1,
            |c| matches!(c, C::SchemaHash { .. }),
        ),
        (
            "doc count 0",
            |b| put_u32(b, 64, 0),
            |c| *c == C::DocCount { count: 0 },
        ),
        (
            "doc count above 2^31",
            |b| put_u32(b, 64, (1 << 31) + 1),
            |c| matches!(c, C::DocCount { .. }),
        ),
        (
            "stats count",
            |b| put_u32(b, 80, 4),
            |c| matches!(c, C::FieldStats { .. }),
        ),
        (
            "stats of another field type",
            |b| put_u32(b, 88, 0),
            |c| matches!(c, C::FieldStats { .. }),
        ),
        (
            "docs_with_field",
            |b| put_u32(b, 92, 301),
            |c| matches!(c, C::FieldStats { .. }),
        ),
        (
            "unknown kind",
            |b| {
                let e = entry(b, 0);
                b[e] = 13;
            },
            |c| *c == C::UnknownSectionKind { kind: 13 },
        ),
        (
            "section reserved",
            |b| {
                let e = entry(b, 0);
                b[e + 2] = 1;
            },
            |c| *c == C::Format(F::NonZeroReserved { value: 1 }),
        ),
        (
            "kind under the wrong field type",
            |b| {
                let e = entry(b, 0);
                b[e] = 1;
            },
            |c| matches!(c, C::SectionField { kind: 1, field: 0 }),
        ),
        (
            "field at the field count",
            |b| {
                let e = entry(b, 0);
                put_u32(b, e + 4, 8);
            },
            |c| matches!(c, C::SectionField { .. }),
        ),
        (
            "segment kind under a field",
            |b| {
                let e = entry(b, 0);
                b[e] = 9;
            },
            |c| matches!(c, C::SectionField { kind: 9, .. }),
        ),
        (
            "order",
            |b| {
                let (e0, e1) = (entry(b, 0), entry(b, 1));
                let a = b[e0..e0 + 24].to_vec();
                b.copy_within(e1..e1 + 24, e0);
                b[e1..e1 + 24].copy_from_slice(&a);
            },
            |c| matches!(c, C::SectionOrder | C::SectionBounds { .. }),
        ),
        (
            "misaligned",
            |b| {
                let e = entry(b, 1);
                add_u64(b, e + 8, 1);
            },
            |c| matches!(c, C::SectionBounds { .. }),
        ),
        (
            "gap",
            |b| {
                let e = entry(b, 1);
                add_u64(b, e + 8, 8);
            },
            |c| matches!(c, C::SectionBounds { .. }),
        ),
        (
            "overlap",
            |b| {
                let e = entry(b, 0);
                add_u64(b, e + 16, 8);
            },
            |c| matches!(c, C::SectionBounds { .. }),
        ),
        (
            "past the footer",
            |b| {
                let n = u32_at(b, 84) as usize;
                let e = entry(b, n - 1);
                add_u64(b, e + 16, 64);
            },
            |c| matches!(c, C::SectionBounds { .. }),
        ),
        (
            "min_id empty",
            |b| {
                let i = ids_at(b);
                let len = usize::from(b[i]);
                b[i] = 0;
                b.drain(i + 2..i + 2 + len);
            },
            |c| *c == C::IdRange,
        ),
        (
            "min_id above max_id",
            |b| {
                let i = ids_at(b);
                b[i + 2] = b'z';
            },
            |c| *c == C::IdRange,
        ),
        ("trailing bytes", |b| b.push(0), |c| *c == C::TrailingBytes),
        (
            "positions for a field without",
            |b| {
                // body (field 1): its Norms (kind 7) relabelled Positions (6) keeps the table ordered.
                let n = u32_at(b, 84) as usize;
                let e = (0..n)
                    .map(|i| entry(b, i))
                    .find(|&e| u32_at(b, e + 4) == 1 && b[e] == 7)
                    .unwrap();
                b[e] = 6;
            },
            |c| {
                matches!(
                    c,
                    C::Presence {
                        kind: 6,
                        field: 1,
                        present: true
                    }
                )
            },
        ),
        (
            "stats without sections",
            |b| put_u32(b, 92, 0),
            |c| matches!(c, C::FieldStats { field: 1 }),
        ),
    ];
    for (name, edit, want) in cases {
        let Err(err) = open(&schema, &with_footer(&bytes, edit)) else {
            panic!("{name}: opened");
        };
        assert!(err.is_corruption(), "{name}: {err}");
        let reason = corruption(&err).unwrap();
        assert!(want(&reason), "{name}: {err}");
    }

    let unsupported: Vec<(&str, Edit, UnsupportedSegment)> = vec![
        (
            "major",
            |b| b[0] = 2,
            UnsupportedSegment::FormatMajor { major: 2 },
        ),
        (
            "incompat",
            |b| b[8] = 1,
            UnsupportedSegment::IncompatFlags { flags: 1 },
        ),
        (
            "schema encoding",
            |b| b[56] = 2,
            UnsupportedSegment::SchemaEncoding { version: 2 },
        ),
    ];
    for (name, edit, want) in unsupported {
        match open(&schema, &with_footer(&bytes, edit)).unwrap_err() {
            ReadError::Unsupported { reason, .. } => assert_eq!(reason, want, "{name}"),
            other => panic!("{name}: {other}"),
        }
    }
    // Read-only compatible flags are readable, and reported.
    let seg = open(&schema, &with_footer(&bytes, |b| b[16] = 4)).unwrap();
    assert_eq!(seg.rocompat_flags(), 4);
    // A different format minor version reads the same.
    let seg = open(&schema, &with_footer(&bytes, |b| b[2] = 9)).unwrap();
    assert_eq!(seg.format_minor(), 9);
}

#[test]
fn locator_and_padding() {
    use SegmentCorruption as C;
    let schema = schema();
    let bytes = build(&schema, &golden_docs(&schema), WriteOptions::default());
    let n = bytes.len();
    let reason = |b: &[u8]| corruption(&open(&schema, b).unwrap_err()).unwrap();

    let mut b = bytes.clone();
    b[n - 1] = b'X';
    assert_eq!(reason(&b), C::BadMagic);
    assert_eq!(reason(b"PSEG"), C::BadMagic);
    assert_eq!(reason(b""), C::BadMagic);
    for len in [0_u32, 97, (1 << 20) + 1, u32::try_from(n).unwrap(), 100] {
        let mut b = bytes.clone();
        put_u32(&mut b, n - 8, len);
        assert!(
            matches!(reason(&b), C::FooterLen { .. } | C::FooterChecksum { .. }),
            "footer_len {len}"
        );
    }
    let mut b = bytes.clone();
    b[n - 9] ^= 1;
    assert!(matches!(reason(&b), C::FooterChecksum { .. }));

    // Non-zero padding after the first section that does not end on a multiple of 8.
    let footer_len = u32_at(&bytes, n - 8) as usize;
    let body = &bytes[n - 8 - footer_len..n - 12];
    let count = u32_at(body, 84) as usize;
    let end = (0..count)
        .map(|i| {
            let e = entry(body, i);
            (u64::from_le_bytes(body[e + 8..e + 16].try_into().unwrap())
                + u64::from_le_bytes(body[e + 16..e + 24].try_into().unwrap())) as usize
        })
        .find(|end| end % 8 != 0)
        .expect("a padded section");
    let mut b = bytes.clone();
    b[end] = 1;
    assert_eq!(
        reason(&b),
        C::Format(persia_format::Corruption::NonZeroReserved { value: 1 })
    );
}

/// A segment inside a larger buffer: error offsets are absolute.
#[test]
fn offsets_are_absolute() {
    let schema = schema();
    let bytes = build(&schema, &golden_docs(&schema), WriteOptions::default());
    let mut b = with_footer(&bytes, |b| b[4] = 1);
    let base = 4096_u64;
    let err = Segment::open(
        ByteSource::with_base_offset(std::mem::take(&mut b).into(), base),
        &schema,
    )
    .unwrap_err();
    let n = bytes.len() as u64;
    let footer_len = u64::from(u32_at(&bytes, bytes.len() - 8));
    match err {
        ReadError::Corrupt { offset, .. } => assert_eq!(offset, base + n - 8 - footer_len + 4),
        other => panic!("{other}"),
    }
}

fn collect_terms<A: fst::Automaton>(
    mut s: persia_engine::segment::TermStream<'_, A>,
    index: &Index,
) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(next) = s.next() {
        let (term, info) = next.unwrap();
        assert_eq!(info.df() as usize, index.terms[term].len());
        out.push(term.to_vec());
    }
    out
}

fn add_u64(b: &mut [u8], at: usize, delta: u64) {
    let v = u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    b[at..at + 8].copy_from_slice(&(v + delta).to_le_bytes());
}

/// Block-max bounds when targets go down between calls: the block of the first document at or after each target,
/// with its own `max_tf` (found in the 5.3 review: a later block's bounds were returned).
#[test]
fn shallow_advance_with_falling_targets() {
    let schema = schema();
    let title = id(&schema, "title");
    // Docs 0..128 hold `w` 5 times, the rest once: the first block's max_tf is 5, every other block's 1.
    let docs: Vec<Document> = (0..1000_u32)
        .map(|i| {
            let text = if i < 128 { "w w w w w" } else { "w" };
            Document::new(format!("d{i:04}")).with(title, Value::String(text.into()))
        })
        .collect();
    let bytes = build(&schema, &docs, WriteOptions::default());
    let seg = open(&schema, &bytes).unwrap();
    let mut terms = seg.terms(title).unwrap();
    let info = terms.get(b"w").unwrap().unwrap();
    let mut p = terms.postings(&info).unwrap();
    let tf = |d: u32| if d < 128 { 5 } else { 1 };
    for target in [700, 10, 999, 0, 128, 127, 500, 1000] {
        let got = p.shallow_advance(target).unwrap();
        if target >= 1000 {
            assert_eq!(got, None);
            continue;
        }
        let m = got.unwrap();
        // The block holding `target`: blocks are 128 documents here.
        assert_eq!(
            m.last_docid,
            ((target / 128) * 128 + 127).min(999),
            "target {target}"
        );
        assert_eq!(m.max_tf, tf(target), "target {target}");
    }
    assert_eq!(p.next().unwrap(), Some(0));
    assert_eq!(p.freq().unwrap(), 5);
    // After `advance` moved past the first block, a target in it still gets its bounds (PR #26 review).
    assert_eq!(p.advance(700).unwrap(), Some(700));
    for target in [10, 0, 127] {
        let m = p.shallow_advance(target).unwrap().unwrap();
        assert_eq!(
            (m.last_docid, m.max_tf),
            (127, 5),
            "target {target} after advance"
        );
    }
    assert_eq!(p.shallow_advance(700).unwrap().unwrap().last_docid, 767);
    assert_eq!(p.doc(), Some(700));
}

/// Reader-side list lengths around the 128-docid block: tail only, one full block, full blocks then a tail.
#[test]
fn advance_across_block_boundaries() {
    let schema = schema();
    let tag = id(&schema, "tag");
    for n in [1_u32, 127, 128, 129, 255, 256, 257] {
        let docs: Vec<Document> = (0..n)
            .map(|i| Document::new(format!("{i:04}")).with(tag, Value::String("same".into())))
            .collect();
        let bytes = build(&schema, &docs, WriteOptions::default());
        let seg = open(&schema, &bytes).unwrap();
        let mut terms = seg.terms(tag).unwrap();
        let info = terms.get(b"same").unwrap().unwrap();
        for target in [0, 1, 126, 127, 128, 129, 255, 256, 257, 300] {
            let mut p = terms.postings(&info).unwrap();
            let want = (target < n).then_some(target);
            assert_eq!(p.advance(target).unwrap(), want, "n {n}, target {target}");
            assert_eq!(
                p.next().unwrap(),
                want.and_then(|d| (d + 1 < n).then_some(d + 1))
            );
        }
        let mut p = terms.postings(&info).unwrap();
        let mut count = 0;
        while p.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, n);
    }
}

#[test]
fn ordinals_and_term_infos_are_checked() {
    let schema = schema();
    let bytes = build(&schema, &golden_docs(&schema), WriteOptions::default());
    let seg = open(&schema, &bytes).unwrap();
    let mut tags = seg.terms(id(&schema, "tag")).unwrap();
    let len = tags.len();
    let err = tags.info(len).unwrap_err();
    assert_eq!(err, ReadError::OrdinalOutOfRange { ordinal: len, len });
    assert!(!err.is_corruption());
    assert_eq!(tags.info(0).unwrap().ordinal(), 0);

    // A TermInfo of another field, or of another segment, is refused.
    let red = tags.get(b"red").unwrap().unwrap();
    let titles = seg.terms(id(&schema, "title")).unwrap();
    assert_eq!(
        titles.postings(&red).map(drop),
        Err(ReadError::ForeignTermInfo)
    );
    let mut other = meta();
    other.segment_id += 1;
    let mut b =
        persia_engine::segment::SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    for d in golden_docs(&schema) {
        b.add(&d).unwrap();
    }
    let other = persia_engine::segment::SegmentWriter::new(&b, other)
        .finish()
        .unwrap();
    let other = open(&schema, &other).unwrap();
    let other_tags = other.terms(id(&schema, "tag")).unwrap();
    assert_eq!(
        other_tags.postings(&red).map(drop),
        Err(ReadError::ForeignTermInfo)
    );
    assert!(tags.postings(&red).is_ok());
    // The same bytes opened twice are two segments: their term infos are not interchangeable.
    let again = open(&schema, &bytes).unwrap();
    let again_tags = again.terms(id(&schema, "tag")).unwrap();
    assert_eq!(again.segment_id(), seg.segment_id());
    assert_eq!(
        again_tags.postings(&red).map(drop),
        Err(ReadError::ForeignTermInfo)
    );
}

/// Every document and frequency of `term`'s list in field `tag`.
fn list_of(seg: &Segment, tag: persia_engine::schema::FieldId, term: &str) -> Vec<(u32, u32)> {
    let terms = seg.terms(tag).unwrap();
    let info = terms.clone().get(term.as_bytes()).unwrap().unwrap();
    let mut postings = terms.postings(&info).unwrap();
    let mut out = Vec::new();
    while let Some(d) = postings.next().unwrap() {
        out.push((d, postings.freq().unwrap()));
    }
    out
}

#[test]
fn decoded_blocks_are_shared_across_lookups_and_iterators() {
    let (schema, docs, bytes) = dense(3000);
    let tag = id(&schema, "tag");
    let cache = Arc::new(BlockCache::new(64 << 20));
    let source = || ByteSource::new(bytes.clone().into());
    let seg = Segment::open_with_cache(source(), &schema, Arc::clone(&cache)).unwrap();
    let first = list_of(&seg, tag, "all");
    assert_eq!(first, model_list(&docs, &schema, "all"));
    let before = cache.stats();
    assert!(before.misses > 0);
    // A new `terms()` and a new iterator: every block they read is cached already.
    assert_eq!(list_of(&seg, tag, "all"), first);
    let after = cache.stats();
    assert_eq!(after.misses, before.misses, "nothing decoded again");
    assert!(after.hits > before.hits);

    // A second segment shares the cache under its own keys; dropping it evicts only its blocks.
    let other = Segment::open_with_cache(source(), &schema, Arc::clone(&cache)).unwrap();
    assert_eq!(list_of(&other, tag, "all"), first);
    assert!(cache.stats().misses > after.misses);
    drop(other);
    assert_eq!(cache.stats().blocks, after.blocks);
    drop(seg);
    assert_eq!(
        cache.stats(),
        CacheStats {
            blocks: 0,
            bytes: 0,
            ..cache.stats()
        }
    );
}

#[test]
fn a_small_cache_stays_within_capacity_and_reads_the_same() {
    let (schema, docs, bytes) = dense(3000);
    let tag = id(&schema, "tag");
    for capacity in [0, 997, 4000] {
        let cache = Arc::new(BlockCache::new(capacity));
        let seg = Segment::open_with_cache(
            ByteSource::new(bytes.clone().into()),
            &schema,
            Arc::clone(&cache),
        )
        .unwrap();
        for term in ["all", "half", "third", "rare", "sparse"] {
            assert_eq!(
                list_of(&seg, tag, term),
                model_list(&docs, &schema, term),
                "{capacity} {term}"
            );
            assert!(cache.stats().bytes <= capacity);
        }
        for d in 0..seg.doc_count() {
            assert_eq!(seg.document(d).unwrap(), docs[d as usize].clone());
        }
        assert!(cache.stats().bytes <= capacity);
    }
}

/// `(doc, positions)` of `term` in field `title`, for the documents `want` picks; the others are passed over.
fn positions_of(
    seg: &Segment,
    schema: &CollectionSchema,
    term: &str,
    want: impl Fn(u32) -> bool,
) -> Vec<(u32, Vec<u32>)> {
    let terms = seg.terms(id(schema, "title")).unwrap();
    let info = terms.clone().get(term.as_bytes()).unwrap().unwrap();
    let mut postings = terms.postings(&info).unwrap();
    let mut out = Vec::new();
    while let Some(d) = postings.next().unwrap() {
        if want(d) {
            let first = postings.positions().unwrap().to_vec();
            assert_eq!(postings.positions().unwrap(), first, "asked twice");
            out.push((d, first));
        }
    }
    out
}

#[test]
fn positions_are_read_per_document() {
    // "alpha" is in every document 2 to 8 times: each block's run has several packed groups and a varint tail.
    let (schema, _, bytes) = dense(3000);
    let seg = open(&schema, &bytes).unwrap();
    let all = positions_of(&seg, &schema, "alpha", |_| true);
    assert_eq!(all.len(), 3000);
    for (d, p) in &all {
        let k = (d % 4 + 1) * 3;
        let want: Vec<u32> = (0..k).filter(|i| i % 3 != 1).collect();
        assert_eq!(p, &want, "doc {d}");
    }
    for stride in [2, 7, 50, 127, 128, 129, 1000] {
        let sparse = positions_of(&seg, &schema, "alpha", |d| d % stride == stride - 1);
        let expected: Vec<_> = all
            .iter()
            .filter(|(d, _)| d % stride == stride - 1)
            .cloned()
            .collect();
        assert_eq!(sparse, expected, "stride {stride}");
    }
    // Advancing skips whole postings blocks; positions still line up with the document.
    let terms = seg.terms(id(&schema, "title")).unwrap();
    let info = terms.clone().get(b"alpha").unwrap().unwrap();
    let mut postings = terms.postings(&info).unwrap();
    for target in [5, 300, 301, 2999] {
        let d = postings.advance(target).unwrap().unwrap();
        assert_eq!(postings.positions().unwrap(), all[d as usize].1, "doc {d}");
    }
    seg.verify().unwrap();
}

// ---------------------------------------------------------------------------------------------------------------
// Lazy reads equal eager ones (ROADMAP 5.3.11-5.3.13): positions decoded one document at a time with documents
// skipped, FOR columns read in any docid order, and the shared block cache's byte bound, under random access.
// ---------------------------------------------------------------------------------------------------------------

/// A deterministic shuffle (xorshift64*, seeded by proptest) of `0..n`.
fn shuffled(n: u32, seed: u64) -> Vec<u32> {
    let mut state = seed | 1;
    let mut next = || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let mut v: Vec<u32> = (0..n).collect();
    for i in (1..v.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
    v
}

/// Adds `alpha beta` `k` times to every third document's title, so lists reach full blocks whose positions runs
/// hold several packed groups (SPEC §5.3.3) as well as the varint tail.
fn heavy_titles(schema: &CollectionSchema, docs: &mut [Document], k: usize) {
    let title = id(schema, "title");
    for (i, d) in docs.iter_mut().enumerate() {
        if i % 3 != 0 {
            continue;
        }
        let mut values = match d.get(title) {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Strings(v)) => v.clone(),
            _ => Vec::new(),
        };
        values.push("alpha beta ".repeat(k + i % 5));
        d.insert(title, Value::Strings(values));
    }
}

/// `(doc, tf, positions)` of every posting of every `title` term, read sequentially, positions of every document.
type FullRead = Vec<(Vec<u8>, Vec<(u32, u32, Vec<u32>)>)>;

fn full_title_read(seg: &Segment, schema: &CollectionSchema) -> FullRead {
    let terms = seg.terms(id(schema, "title")).unwrap();
    let mut out = Vec::new();
    let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
    while let Some(next) = stream.next() {
        let (term, info) = next.unwrap();
        let mut p = terms.postings(&info).unwrap();
        let mut list = Vec::new();
        while let Some(d) = p.next().unwrap() {
            let tf = p.freq().unwrap();
            list.push((d, tf, p.positions().unwrap().to_vec()));
        }
        out.push((term.to_vec(), list));
    }
    out
}

/// A segment over `docs`, read through a cache of `capacity` bytes of its own.
fn open_cached(
    schema: &CollectionSchema,
    bytes: &[u8],
    capacity: u64,
) -> (Segment, Arc<BlockCache>) {
    let cache = Arc::new(BlockCache::new(capacity));
    let seg = Segment::open_with_cache(
        ByteSource::new(bytes.to_vec().into()),
        schema,
        Arc::clone(&cache),
    )
    .unwrap();
    (seg, cache)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 256 } else { 16 }, ..ProptestConfig::default() })]

    /// Positions read for an arbitrary subset of documents, reached by `next` (some tombstoned) or `advance`, with
    /// `shallow_advance` in between and some asked for twice, equal those of a full sequential read; so do the
    /// documents and frequencies the iterator stops on (SPEC §5.3.3, ROADMAP 5.3.13).
    #[test]
    fn positions_with_skipped_documents_equal_a_full_read(
        docs in prop::collection::vec(arb_doc(schema()), 1..400),
        heavy in proptest::option::of(1_usize..10),
        block_size in prop::sample::select(vec![1_u32, 61, 4096]),
        capacity in prop::sample::select(vec![0_u64, 1500, BlockCache::DEFAULT_CAPACITY]),
        steps in prop::collection::vec((0_u8..4, 0_u32..300, any::<bool>()), 1..64),
        deleted in prop::collection::btree_set(0_u32..400, 0..120),
    ) {
        let schema = schema();
        let mut docs = with_ids(docs);
        if let Some(k) = heavy {
            heavy_titles(&schema, &mut docs, k);
        }
        let bytes = build(&schema, &docs, WriteOptions { block_size, ..WriteOptions::default() });
        let full = full_title_read(&open(&schema, &bytes).unwrap(), &schema);

        let (seg, cache) = open_cached(&schema, &bytes, capacity);
        let n = seg.doc_count();
        let deleted: RoaringBitmap = deleted.into_iter().filter(|&d| d < n).collect();
        let tombstones = seg.tombstones(deleted.clone()).unwrap();
        let terms = seg.terms(id(&schema, "title")).unwrap();
        let mut k = 0_usize;
        for (term, list) in &full {
            let info = terms.clone().get(term).unwrap().unwrap();
            let live: Vec<&(u32, u32, Vec<u32>)> =
                list.iter().filter(|e| !deleted.contains(e.0)).collect();
            let mut p = terms.postings(&info).unwrap().with_tombstones(&tombstones);
            let mut i = 0_usize;
            let mut cur = p.next().unwrap();
            loop {
                prop_assert_eq!(cur, live.get(i).map(|e| e.0), "term {:?}", term);
                let Some(d) = cur else { break };
                let (op, jump, read) = steps[k % steps.len()];
                k += 1;
                let (_, tf, positions) = live[i];
                if read {
                    prop_assert_eq!(p.positions().unwrap(), positions.as_slice(), "doc {}", d);
                    prop_assert_eq!(p.freq().unwrap(), *tf);
                }
                match op {
                    0 => {
                        cur = p.next().unwrap();
                        i += 1;
                    }
                    1 => {
                        let target = d.saturating_add(1).saturating_add(jump);
                        cur = p.advance(target).unwrap();
                        i += live[i..].iter().position(|e| e.0 >= target).unwrap_or(live.len() - i);
                    }
                    2 => {
                        // Block-max lookups do not move the iterator, nor its positions.
                        p.shallow_advance(d.saturating_add(jump)).unwrap();
                        prop_assert_eq!(p.doc(), Some(d));
                        prop_assert_eq!(p.positions().unwrap(), positions.as_slice(), "doc {}", d);
                        cur = p.next().unwrap();
                        i += 1;
                    }
                    _ => {
                        // Frequencies only: the block's run is entered later, past this document.
                        prop_assert_eq!(p.freq().unwrap(), *tf);
                        cur = p.next().unwrap();
                        i += 1;
                    }
                }
            }
            prop_assert_eq!(p.positions().unwrap(), &[] as &[u32]);
        }
        prop_assert!(cache.stats().bytes <= capacity);
    }
}

proptest! {
    // Fewer cases: each builds and reads a segment of up to 2200 documents.
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 128 } else { 8 }, ..ProptestConfig::default() })]

    /// Keyword `ords`/`values` and numeric `key`/`get` read in an arbitrary docid order (each docid twice, docids
    /// past the segment too) equal a sequential read, which equals the model (SPEC §5.3.5, ROADMAP 5.3.12). The
    /// documents repeat a few random ones up to 2200 times, so columns span up to three FOR blocks; with `many_tags`
    /// the keyword ords do too.
    #[test]
    fn columns_in_any_docid_order_equal_a_sequential_read(
        base in prop::collection::vec(arb_doc(schema()), 1..40),
        n in 1_usize..2200,
        block_size in prop::sample::select(vec![509_u32, 4096, 16 * 1024]), // 509: a FOR block spans several frames
        capacity in prop::sample::select(vec![0_u64, 3000, BlockCache::DEFAULT_CAPACITY]),
        seed in any::<u64>(),
        many_tags in any::<bool>(),
    ) {
        let schema = schema();
        let mut docs = with_ids((0..n).map(|i| base[i % base.len()].clone()).collect());
        if many_tags {
            more_tags(&schema, &mut docs);
        }
        let bytes = build(&schema, &docs, WriteOptions { block_size, ..WriteOptions::default() });
        let want = expected(&schema, &docs);
        let n = u32::try_from(n).unwrap();
        let tag = id(&schema, "tag");
        let numeric: Vec<_> = ["price", "qty", "at"].iter().map(|f| id(&schema, f)).collect();

        // Sequential, through the default cache.
        let seg = open(&schema, &bytes).unwrap();
        let col = seg.keyword_column(tag).unwrap();
        let seq_ords: Vec<Vec<u64>> = (0..n + 2).map(|d| col.ords(d).unwrap()).collect();
        let seq_values: Vec<Vec<Vec<u8>>> = (0..n + 2)
            .map(|d| col.values(d).unwrap().into_iter().map(<[u8]>::to_vec).collect())
            .collect();
        let model_values: Vec<Vec<Vec<u8>>> = (0..n + 2)
            .map(|d| want.keywords[&tag.get()].get(&d).cloned().unwrap_or_default())
            .collect();
        prop_assert_eq!(&seq_values, &model_values);
        let mut seq_keys = Vec::new();
        for &f in &numeric {
            let col = seg.numeric_column(f).unwrap();
            let keys: Vec<Option<u64>> = (0..n + 2).map(|d| col.key(d).unwrap()).collect();
            let model: Vec<Option<u64>> =
                (0..n + 2).map(|d| want.columns[&f.get()].get(&d).copied()).collect();
            prop_assert_eq!(&keys, &model, "field {}", f.get());
            let gets: Vec<Option<Value>> = (0..n + 2).map(|d| col.get(d).unwrap()).collect();
            seq_keys.push((keys, gets));
        }

        // Shuffled (up to 700 docids, plus the FOR block boundaries and docids past the segment), each twice, through
        // a cache of `capacity`, with the columns opened again.
        let (seg, cache) = open_cached(&schema, &bytes, capacity);
        let mut order: Vec<u32> = shuffled(n + 2, seed).into_iter().take(700).collect();
        order.extend([0, 1023, 1024, 1025, 2047, 2048, n - 1, n, n + 1].into_iter().filter(|&d| d < n + 2));
        let again = shuffled(u32::try_from(order.len()).unwrap(), seed.rotate_left(17));
        order.extend(again.into_iter().map(|j| order[j as usize]).collect::<Vec<_>>());
        let col = seg.keyword_column(tag).unwrap();
        let cols: Vec<_> = numeric.iter().map(|&f| seg.numeric_column(f).unwrap()).collect();
        for &d in &order {
            let i = d as usize;
            prop_assert_eq!(&col.ords(d).unwrap(), &seq_ords[i], "ords of {}", d);
            let values: Vec<Vec<u8>> = col.values(d).unwrap().into_iter().map(<[u8]>::to_vec).collect();
            prop_assert_eq!(&values, &seq_values[i], "values of {}", d);
            for (c, (keys, gets)) in cols.iter().zip(&seq_keys) {
                prop_assert_eq!(c.key(d).unwrap(), keys[i], "key of {}", d);
                prop_assert_eq!(c.has_value(d), keys[i].is_some(), "has_value of {}", d);
                let got = c.get(d).unwrap();
                // f64 values compare by key: -0.0 and NaN payloads stay exact.
                prop_assert_eq!(got.as_ref().map(value_key), gets[i].as_ref().map(value_key), "get of {}", d);
            }
            prop_assert!(cache.stats().bytes <= capacity);
        }
    }
}

/// A typed numeric value's order-preserving key.
fn value_key(v: &Value) -> u64 {
    match *v {
        Value::I64(x) | Value::Datetime(x) => i64_to_key(x),
        Value::F64(x) => f64_to_key(x),
        ref other => panic!("not numeric: {other:?}"),
    }
}

/// One read of [`cache_capacity_holds_across_two_segments`], as comparable output.
#[derive(Debug, PartialEq)]
enum Got {
    Keys(Vec<Option<u64>>),
    Ords(Vec<u64>),
    List(Vec<(u32, u32, Vec<u32>)>),
    Doc(Document),
}

/// Read `op` with argument `arg` from `seg`.
fn read_op(seg: &Segment, schema: &CollectionSchema, op: u8, arg: u32) -> Got {
    let n = seg.doc_count();
    match op {
        0 => Got::Keys(
            ["price", "qty", "at"]
                .iter()
                .map(|f| {
                    seg.numeric_column(id(schema, f))
                        .unwrap()
                        .key(arg % (n + 1))
                        .unwrap()
                })
                .collect(),
        ),
        1 => Got::Ords(
            seg.keyword_column(id(schema, "tag"))
                .unwrap()
                .ords(arg % (n + 1))
                .unwrap(),
        ),
        2 | 3 => {
            let field = id(schema, if op == 2 { "title" } else { "tag" });
            let terms = seg.terms(field).unwrap();
            if terms.is_empty() {
                return Got::List(Vec::new());
            }
            let ordinal = u64::from(arg) % terms.len();
            let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
            let mut info = None;
            for _ in 0..=ordinal {
                info = Some(stream.next().unwrap().unwrap().1);
            }
            let mut p = terms.postings(&info.unwrap()).unwrap();
            let mut out = Vec::new();
            // Starts at an argument-picked docid, and reads every other document's positions.
            let mut cur = p.advance(arg % (n + 1)).unwrap();
            while let Some(d) = cur {
                let tf = p.freq().unwrap();
                let pos = if d % 2 == 0 {
                    p.positions().unwrap().to_vec()
                } else {
                    Vec::new()
                };
                out.push((d, tf, pos));
                cur = p.next().unwrap();
            }
            Got::List(out)
        }
        _ => Got::Doc(seg.document(arg % n).unwrap()),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(feature = "slow") { 256 } else { 24 }, ..ProptestConfig::default() })]

    /// Two segments reading through one shared [`BlockCache`] in a random interleaving: after every read the cache
    /// holds at most its capacity, every read equals the same read through a segment with a cache of its own, and
    /// dropping both segments empties the cache (SPEC §10.6, ROADMAP 5.3.11).
    #[test]
    fn cache_capacity_holds_across_two_segments(
        docs_a in prop::collection::vec(arb_doc(schema()), 1..300),
        docs_b in prop::collection::vec(arb_doc(schema()), 1..300),
        heavy in 1_usize..6,
        capacity in prop::sample::select(vec![0_u64, 1, 700, 4096, 32 * 1024]),
        ops in prop::collection::vec((any::<bool>(), 0_u8..5, any::<u32>()), 1..150),
    ) {
        let schema = schema();
        let mut docs_a = with_ids(docs_a);
        heavy_titles(&schema, &mut docs_a, heavy);
        let docs_b = with_ids(docs_b);
        // Small blocks: many cached entries, so eviction runs often.
        let opts = WriteOptions { block_size: 61, ..WriteOptions::default() };
        let bytes = [build(&schema, &docs_a, opts), build(&schema, &docs_b, WriteOptions::default())];
        let reference = [open(&schema, &bytes[0]).unwrap(), open(&schema, &bytes[1]).unwrap()];

        let cache = Arc::new(BlockCache::new(capacity));
        let shared: Vec<Segment> = bytes
            .iter()
            .map(|b| Segment::open_with_cache(ByteSource::new(b.clone().into()), &schema, Arc::clone(&cache)).unwrap())
            .collect();
        let mut reads = 0_u64;
        for (which, op, arg) in ops {
            let s = usize::from(which);
            let got = read_op(&shared[s], &schema, op, arg);
            prop_assert_eq!(&got, &read_op(&reference[s], &schema, op, arg), "segment {} op {} arg {}", s, op, arg);
            let stats = cache.stats();
            prop_assert!(stats.bytes <= capacity, "{:?} over {}", stats, capacity);
            if capacity == 0 {
                prop_assert_eq!(stats.blocks, 0);
            }
            prop_assert!(stats.hits + stats.misses >= reads);
            reads = stats.hits + stats.misses;
        }
        let mut shared = shared;
        let b = shared.pop().unwrap();
        let a = shared.pop().unwrap();
        drop(a);
        prop_assert!(cache.stats().bytes <= capacity);
        drop(b);
        let stats = cache.stats();
        prop_assert_eq!((stats.blocks, stats.bytes), (0, 0), "{:?}", stats);
    }
}

/// Gives documents 0, 1, 2 or 3 more tags (cycling by docid): the ords column passes 1024 values, and documents'
/// ords straddle its FOR block boundaries.
fn more_tags(schema: &CollectionSchema, docs: &mut [Document]) {
    let tag = id(schema, "tag");
    for (i, d) in docs.iter_mut().enumerate() {
        let mut values = match d.get(tag) {
            Some(Value::Strings(v)) => v.clone(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        values.extend(
            ["blue", "green", "red"][..i % 4]
                .iter()
                .map(|s| (*s).to_owned()),
        );
        d.insert(tag, Value::Strings(values));
    }
}
