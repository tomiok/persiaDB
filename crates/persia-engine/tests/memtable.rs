//! The memtable (ROADMAP 6.1): differential against the segment its live documents make, views as of every
//! point in a history, upserts and deletes, and readers running beside the writer.
#![allow(
    clippy::unwrap_used,
    clippy::unreachable,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::single_match_else,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::ops::Bound;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use fst::Automaton;
use fst::automaton::{AlwaysMatch, Str, Subsequence};
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, DocumentError, Value};
use persia_engine::memtable::{IdState, Memtable, MemtableError, MemtableView};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::segment::{
    MAX_TERM_LEN, ReadError, Segment, SegmentBuilder, SegmentError, SegmentMeta, SegmentWriter,
};
use persia_engine::view::{NormReader, PostingsIterator, SearchView, SegmentView, TermEnum};
use persia_format::ByteSource;
use proptest::prelude::*;
use roaring::RoaringBitmap;

const WORDS: [&str; 6] = ["alpha", "beta", "gamma", "delta", "common", "omega"];
const TAGS: [&str; 3] = ["x", "y", "z"];

struct Fields {
    body: FieldId,
    title: FieldId,
    tag: FieldId,
    n: FieldId,
    flag: FieldId,
}

fn schema() -> (CollectionSchema, Fields) {
    let schema = SchemaBuilder::new("c")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field(
            "title",
            FieldKind::Text(TextOptions {
                positions: false,
                stored: false,
                ..TextOptions::default()
            }),
        )
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .field("n", FieldKind::I64 { stored: true })
        .field("flag", FieldKind::Bool { stored: false })
        .build()
        .unwrap();
    let id = |name: &str| schema.field(name).unwrap().id();
    let fields = Fields {
        body: id("body"),
        title: id("title"),
        tag: id("tag"),
        n: id("n"),
        flag: id("flag"),
    };
    (schema, fields)
}

fn memtable(schema: &CollectionSchema) -> Memtable {
    Memtable::new(schema, &AnalyzerRegistry::builtin()).unwrap()
}

#[derive(Debug, Clone)]
enum Op {
    Put(u8, Vec<Vec<u8>>, Vec<u8>, Vec<u8>, i64),
    Delete(u8),
}

fn op() -> impl Strategy<Value = Op> {
    let words = || prop::collection::vec(prop::collection::vec(0..WORDS.len() as u8, 0..6), 0..3);
    prop_oneof![
        4 => (0..8_u8, words(), prop::collection::vec(0..WORDS.len() as u8, 0..4),
              prop::collection::vec(0..TAGS.len() as u8, 0..3), any::<i64>())
            .prop_map(|(id, body, title, tags, n)| Op::Put(id, body, title, tags, n)),
        1 => (0..8_u8).prop_map(Op::Delete),
    ]
}

fn text(words: &[u8]) -> String {
    let words: Vec<&str> = words.iter().map(|&w| WORDS[w as usize]).collect();
    words.join(" ")
}

fn document(f: &Fields, op: &Op) -> Option<Document> {
    let Op::Put(id, body, title, tags, n) = op else {
        return None;
    };
    let mut doc = Document::new(format!("d{id}")).with(f.n, Value::I64(*n));
    // Some documents lack it, so the column has a presence bitmap.
    if n % 3 != 0 {
        drop(doc.insert(f.flag, Value::Bool(n % 2 == 0)));
    }
    match body.as_slice() {
        [] => {}
        [one] => doc
            .insert(f.body, Value::String(text(one)))
            .map_or((), drop),
        many => doc
            .insert(
                f.body,
                Value::Strings(many.iter().map(|w| text(w)).collect()),
            )
            .map_or((), drop),
    }
    if !title.is_empty() {
        doc.insert(f.title, Value::String(text(title)));
    }
    match tags.as_slice() {
        [] => {}
        [one] => drop(doc.insert(f.tag, Value::String(TAGS[*one as usize].into()))),
        many => drop(doc.insert(
            f.tag,
            Value::Strings(many.iter().map(|&t| TAGS[t as usize].into()).collect()),
        )),
    }
    Some(doc)
}

/// The live documents after each prefix of `ops`, in the order the memtable gave them docids, and the ids whose
/// latest operation is a delete.
#[derive(Debug, Clone, Default)]
struct Model {
    live: Vec<Document>,
    deleted: Vec<String>,
}

impl Model {
    fn apply(&mut self, f: &Fields, op: &Op) {
        let id = match op {
            Op::Put(id, ..) | Op::Delete(id) => format!("d{id}"),
        };
        self.live.retain(|d| d.id() != id);
        self.deleted.retain(|d| *d != id);
        match document(f, op) {
            Some(doc) => self.live.push(doc),
            None => self.deleted.push(id),
        }
    }
}

fn segment(schema: &CollectionSchema, docs: &[Document]) -> Option<Segment> {
    if docs.is_empty() {
        return None;
    }
    let mut b = SegmentBuilder::new(schema, &AnalyzerRegistry::builtin()).unwrap();
    for d in docs {
        b.add(d).unwrap();
    }
    let meta = SegmentMeta {
        segment_id: 1,
        created_at: 0,
        merge_generation: 0,
    };
    let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
    Some(Segment::open(ByteSource::new(bytes.into()), schema).unwrap())
}

type Listed = Vec<(u32, u32, Vec<u32>)>;

/// Every posting of `term`: docid, frequency and positions; and the block bounds of the first.
fn list<P: PostingsIterator>(mut p: P) -> (Listed, Option<(u32, u8)>) {
    let shallow = p.shallow_advance(0).unwrap();
    assert_eq!(p.doc(), None, "a new iterator is before its first document");
    let mut out = Vec::new();
    let mut bounds = None;
    while let Some(d) = p.next().unwrap() {
        assert_eq!(p.doc(), Some(d));
        let b = p.block_max().unwrap();
        bounds.get_or_insert((b.max_tf, b.min_norm));
        out.push((d, p.freq().unwrap(), p.positions().unwrap().to_vec()));
    }
    assert_eq!(p.next().unwrap(), None, "stays exhausted");
    assert!(shallow.is_some() || out.is_empty());
    assert!(u32::try_from(out.len()).unwrap() <= p.df());
    (out, bounds)
}

fn all_terms(segment: &Segment, field: FieldId) -> Vec<Vec<u8>> {
    let terms = segment.terms(field).unwrap();
    let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
    let mut out = Vec::new();
    while let Some(entry) = stream.next() {
        out.push(entry.unwrap().0.to_vec());
    }
    out
}

/// Every term `terms` lists, with its `df`.
fn enumerate<V: SearchView, A: Automaton>(
    view: &V,
    field: FieldId,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
    automaton: A,
) -> Vec<(Vec<u8>, u32)> {
    let mut terms = view.terms(field, lower, upper, automaton).unwrap();
    let mut out = Vec::new();
    while let Some((term, df)) = terms.next().unwrap() {
        out.push((term.to_vec(), df));
    }
    out
}

/// The memtable lists the terms (and `df`s) the segment of its live documents does, by range and by automaton.
fn assert_enumerations_match(f: &Fields, view: &MemtableView, seg: Option<&SegmentView<'_>>) {
    use Bound::{Excluded, Included, Unbounded};
    type Range<'a> = (Bound<&'a [u8]>, Bound<&'a [u8]>);
    let ranges: [Range<'_>; 5] = [
        (Unbounded, Unbounded),
        (Included(b"c"), Excluded(b"d")),
        (Excluded(b"beta"), Included(b"gamma")),
        (Included(b"y"), Unbounded),
        (Included(b"z"), Excluded(b"a")),
    ];
    for field in [f.body, f.title, f.tag] {
        for (lower, upper) in ranges {
            let mem = enumerate(view, field, lower, upper, AlwaysMatch);
            let expected =
                seg.map_or_else(Vec::new, |s| enumerate(s, field, lower, upper, AlwaysMatch));
            assert_eq!(mem, expected, "{field:?} {lower:?}..{upper:?}");
            let prefix = || Str::new("a").starts_with();
            let mem = enumerate(view, field, lower, upper, prefix());
            let expected =
                seg.map_or_else(Vec::new, |s| enumerate(s, field, lower, upper, prefix()));
            assert_eq!(mem, expected, "{field:?} {lower:?}..{upper:?} prefix a");
            let mem = enumerate(view, field, lower, upper, Subsequence::new("ma"));
            let expected = seg.map_or_else(Vec::new, |s| {
                enumerate(s, field, lower, upper, Subsequence::new("ma"))
            });
            assert_eq!(
                mem, expected,
                "{field:?} {lower:?}..{upper:?} subsequence ma"
            );
        }
        // Exactly the terms with postings.
        for (term, _) in enumerate(view, field, Unbounded, Unbounded, AlwaysMatch) {
            assert!(view.postings(field, &term).unwrap().is_some());
        }
    }
}

/// `view` holds exactly what a segment of `model.live` would.
fn assert_matches(schema: &CollectionSchema, f: &Fields, view: &MemtableView, model: &Model) {
    let seg = segment(schema, &model.live);
    let seg_view = seg.as_ref().map(|s| SegmentView::new(s, None));
    let live: Vec<u32> = (0..view.doc_count()).filter(|&d| view.is_live(d)).collect();
    assert_eq!(live.len(), model.live.len());
    assert_eq!(view.live_count() as usize, model.live.len());
    // Memtable docid of segment docid `i`.
    let to_seg = |d: u32| u32::try_from(live.binary_search(&d).unwrap()).unwrap();

    for field in [f.body, f.title, f.tag] {
        let expected_stats = seg_view
            .as_ref()
            .and_then(|s| s.field_stats(field))
            .unwrap_or_default();
        assert_eq!(view.field_stats(field), Some(expected_stats), "{field:?}");
        let mut terms: Vec<Vec<u8>> = seg
            .as_ref()
            .map(|s| all_terms(s, field))
            .unwrap_or_default();
        terms.extend(WORDS.iter().chain(&TAGS).map(|w| w.as_bytes().to_vec()));
        terms.push(b"absent".to_vec());
        for term in terms {
            let mem = view.postings(field, &term).unwrap().map(list);
            let expected = seg_view
                .as_ref()
                .and_then(|s| s.postings(field, &term).unwrap())
                .map(list);
            let mem = mem.map(|(l, b)| {
                let l: Listed = l.into_iter().map(|(d, tf, p)| (to_seg(d), tf, p)).collect();
                (l, b)
            });
            assert_eq!(
                mem,
                expected,
                "{field:?} {:?}",
                String::from_utf8_lossy(&term)
            );
        }
    }
    assert_enumerations_match(f, view, seg_view.as_ref());
    assert_eq!(
        view.deleted(),
        (0..view.doc_count())
            .filter(|&d| !view.is_live(d))
            .collect()
    );
    // Values and column keys, live documents only, in segment docids.
    let live_only = |b: RoaringBitmap| -> Vec<u32> {
        b.iter().filter(|&d| view.is_live(d)).map(to_seg).collect()
    };
    for field in [f.body, f.title, f.tag, f.n, f.flag] {
        let expected: Vec<u32> = seg_view
            .as_ref()
            .map(|s| s.present(field).unwrap().iter().collect())
            .unwrap_or_default();
        let present = view.present(field).unwrap();
        assert!(
            present.max().is_none_or(|d| d < view.doc_count()),
            "only the view's documents"
        );
        assert_eq!(live_only(present), expected, "{field:?}");
    }
    for field in [f.n, f.flag] {
        let mut mem = Vec::new();
        view.column_keys(field, |d, k| mem.push((d, k))).unwrap();
        assert!(mem.windows(2).all(|w| w[0].0 < w[1].0), "docid order");
        assert!(
            mem.last().is_none_or(|&(d, _)| d < view.doc_count()),
            "only the view's documents"
        );
        let mem: Vec<(u32, u64)> = mem
            .into_iter()
            .filter(|&(d, _)| view.is_live(d))
            .map(|(d, k)| (to_seg(d), k))
            .collect();
        let mut expected = Vec::new();
        if let Some(s) = &seg_view {
            s.column_keys(field, |d, k| expected.push((d, k))).unwrap();
        }
        assert_eq!(mem, expected, "{field:?}");
    }
    for (i, &d) in (0_u32..).zip(&live) {
        let s = seg_view.as_ref().unwrap();
        for field in [f.body, f.title] {
            assert_eq!(
                view.norms(field).unwrap().get(d).unwrap(),
                s.norms(field).unwrap().get(i).unwrap()
            );
        }
        assert_eq!(view.document(d).unwrap(), s.document(i).unwrap());
    }
    for id in (0..8).map(|i| format!("d{i}")) {
        let expected = seg_view.as_ref().and_then(|s| s.docid_of(&id).unwrap());
        assert_eq!(view.docid_of(&id).unwrap().map(to_seg), expected, "{id}");
        let state = view.lookup(&id);
        if let Some(seg_docid) = expected {
            let Some(IdState::Live(d)) = state else {
                panic!("{id} is live: {state:?}");
            };
            assert_eq!(to_seg(d), seg_docid);
        } else if model.deleted.contains(&id) {
            assert_eq!(state, Some(IdState::Deleted));
        } else if expected.is_none() {
            assert_eq!(state, None);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 512 } else { 64 }))]

    /// A view taken after any prefix of the operations is the segment of the live documents then, however many
    /// operations follow it.
    #[test]
    fn views_match_the_segment_of_their_live_documents(ops in prop::collection::vec(op(), 0..32)) {
        let (schema, f) = schema();
        let mut mem = memtable(&schema);
        let mut model = Model::default();
        let mut views = vec![(mem.view(), model.clone())];
        for op in &ops {
            match document(&f, op) {
                Some(doc) => drop(mem.put(&doc).unwrap()),
                None => {
                    let Op::Delete(id) = op else { unreachable!() };
                    let id = format!("d{id}");
                    let had = model.live.iter().any(|d| d.id() == id);
                    prop_assert_eq!(mem.delete(&id).unwrap(), had);
                }
            }
            model.apply(&f, op);
            views.push((mem.view(), model.clone()));
        }
        prop_assert_eq!(mem.version(), ops.len() as u64);
        prop_assert_eq!(mem.live_count() as usize, model.live.len());
        for (view, model) in &views {
            assert_matches(&schema, &f, view, model);
        }
    }
}

#[test]
fn upsert_replaces_within_the_memtable() {
    let (schema, f) = schema();
    let mut mem = memtable(&schema);
    let doc = |body: &str| Document::new("a").with(f.body, Value::String(body.into()));
    assert_eq!(mem.put(&doc("one two")).unwrap(), 0);
    let first = mem.view();
    assert_eq!(mem.put(&doc("two three")).unwrap(), 1);
    let view = mem.view();
    assert_eq!((view.doc_count(), view.live_count()), (2, 1));
    assert!(!view.is_live(0) && view.is_live(1));
    assert_eq!(view.docid_of("a").unwrap(), Some(1));
    assert!(view.postings(f.body, b"one").unwrap().is_none());
    let stats = view.field_stats(f.body).unwrap();
    assert_eq!((stats.docs_with_field, stats.sum_field_len), (1, 2));
    // The replaced document can still be fetched by docid, as a deleted one of a segment.
    assert_eq!(view.document(0).unwrap().id(), "a");
    // The view before is unchanged.
    assert_eq!(first.docid_of("a").unwrap(), Some(0));
    assert_eq!(first.lookup("a"), Some(IdState::Live(0)));
    assert!(first.postings(f.body, b"one").unwrap().is_some());
    assert!(first.postings(f.body, b"three").unwrap().is_none());
}

#[test]
fn deletes_are_recorded_for_any_id() {
    let (schema, f) = schema();
    let mut mem = memtable(&schema);
    assert!(mem.is_empty());
    assert!(!mem.delete("elsewhere").unwrap(), "not in the memtable");
    assert!(!mem.is_empty(), "a delete is something to flush");
    let view = mem.view();
    assert_eq!(view.lookup("elsewhere"), Some(IdState::Deleted));
    assert_eq!(view.lookup("never"), None);
    assert_eq!(view.docid_of("elsewhere").unwrap(), None);

    mem.put(&Document::new("elsewhere").with(f.n, Value::I64(1)))
        .unwrap();
    assert_eq!(mem.view().lookup("elsewhere"), Some(IdState::Live(0)));
    assert_eq!(view.lookup("elsewhere"), Some(IdState::Deleted), "MVCC");
    assert!(mem.delete("elsewhere").unwrap());
    assert!(!mem.delete("elsewhere").unwrap(), "already deleted");
    assert_eq!(mem.live_count(), 0);
    assert_eq!(mem.version(), 4);
}

#[test]
fn rejects_invalid_operations_without_changing() {
    let (schema, f) = schema();
    let mut mem = memtable(&schema);
    mem.put(&Document::new("a").with(f.body, Value::String("alpha".into())))
        .unwrap();
    let (size, version) = (mem.size_in_bytes(), mem.version());
    for id in [String::new(), "x".repeat(513)] {
        assert_eq!(
            mem.delete(&id),
            Err(MemtableError::Segment(SegmentError::Document(
                DocumentError::IdLen { len: id.len() }
            )))
        );
    }
    let wrong = Document::new("b").with(f.n, Value::String("not a number".into()));
    assert!(matches!(
        mem.put(&wrong),
        Err(MemtableError::Segment(SegmentError::Document(_)))
    ));
    let long_tag = Document::new("c").with(f.tag, Value::String("t".repeat(MAX_TERM_LEN + 1)));
    assert!(mem.put(&long_tag).is_err());
    assert_eq!(mem.version(), version);
    assert!(mem.size_in_bytes() >= size);
    let view = mem.view();
    assert_eq!((view.doc_count(), view.live_count()), (1, 1));
    assert_eq!(view.lookup("b"), None);
}

#[test]
fn field_errors_match_a_segment() {
    let (schema, f) = schema();
    let mut mem = memtable(&schema);
    let doc = Document::new("a")
        .with(f.body, Value::String("alpha".into()))
        .with(f.tag, Value::String("x".into()))
        .with(f.n, Value::I64(1));
    mem.put(&doc).unwrap();
    let view = mem.view();
    let seg = segment(&schema, &[doc]).unwrap();
    let seg_view = SegmentView::new(&seg, None);
    let mut big = SchemaBuilder::new("big");
    for i in 0..20 {
        big = big.field(format!("f{i}"), FieldKind::I64 { stored: false });
    }
    let unknown = big.build().unwrap().field("f19").unwrap().id();
    let mem_errors = (
        view.postings(f.n, b"1").err(),
        view.postings(unknown, b"1").err(),
        view.norms(f.tag).err(),
        view.document(1).err(),
    );
    let seg_errors = (
        seg_view.postings(f.n, b"1").err(),
        seg_view.postings(unknown, b"1").err(),
        seg_view.norms(f.tag).err(),
        seg_view.document(1).err(),
    );
    assert_eq!(mem_errors, seg_errors);
    assert!(matches!(mem_errors.0, Some(ReadError::FieldType { .. })));
    assert!(matches!(mem_errors.1, Some(ReadError::UnknownField { .. })));
    assert!(matches!(
        mem_errors.3,
        Some(ReadError::DocOutOfRange { .. })
    ));
    assert_eq!(view.field_stats(f.n), None);

    // Enumeration and columns refuse the same fields.
    let all = || (Bound::Unbounded, Bound::Unbounded);
    let column_errors =
        |v: &dyn Fn(FieldId) -> Option<ReadError>| (v(f.body), v(f.tag), v(unknown));
    let mem_columns = column_errors(&|field| view.column_keys(field, |_, _| ()).err());
    let seg_columns = column_errors(&|field| seg_view.column_keys(field, |_, _| ()).err());
    assert_eq!(mem_columns, seg_columns);
    assert!(matches!(mem_columns.0, Some(ReadError::FieldType { .. })));
    let (lower, upper) = all();
    assert_eq!(
        view.terms(f.n, lower, upper, AlwaysMatch).err(),
        seg_view.terms(f.n, lower, upper, AlwaysMatch).err()
    );
    assert!(view.terms(f.n, lower, upper, AlwaysMatch).is_err());
    assert_eq!(view.present(unknown).err(), seg_view.present(unknown).err());
}

#[test]
fn segment_views_list_deleted_documents_terms() {
    let (schema, f) = schema();
    let docs: Vec<Document> = ["alpha", "beta"]
        .iter()
        .enumerate()
        .map(|(i, w)| Document::new(format!("d{i}")).with(f.body, Value::String((*w).into())))
        .collect();
    let seg = segment(&schema, &docs).unwrap();
    let tombstones = seg.tombstones(RoaringBitmap::from_iter([0])).unwrap();
    let view = SegmentView::new(&seg, Some(&tombstones));
    assert_eq!(view.deleted(), RoaringBitmap::from_iter([0]));
    // `alpha` only holds a deleted document: still listed, with the df its postings report.
    let listed = enumerate(
        &view,
        f.body,
        Bound::Unbounded,
        Bound::Unbounded,
        AlwaysMatch,
    );
    assert_eq!(listed, [(b"alpha".to_vec(), 1), (b"beta".to_vec(), 1)]);
    let mut alpha = view.postings(f.body, b"alpha").unwrap().unwrap();
    assert_eq!(alpha.next().unwrap(), None);
    // Present counts the deleted document too.
    assert_eq!(
        view.present(f.body).unwrap(),
        RoaringBitmap::from_iter([0, 1])
    );
    assert_eq!(SegmentView::new(&seg, None).deleted(), RoaringBitmap::new());
}

#[test]
fn advance_skips_and_never_moves_back() {
    let (schema, f) = schema();
    let mut mem = memtable(&schema);
    for i in 0..10 {
        let body = if i % 3 == 0 { "alpha beta" } else { "beta" };
        mem.put(&Document::new(format!("d{i}")).with(f.body, Value::String(body.into())))
            .unwrap();
    }
    let view = mem.view();
    let mut p = view.postings(f.body, b"alpha").unwrap().unwrap();
    assert_eq!(p.df(), 4);
    let mut exact = view.postings(f.body, b"alpha").unwrap().unwrap();
    assert_eq!(
        exact.advance(3).unwrap(),
        Some(3),
        "a target that is a document"
    );
    assert_eq!(p.advance(4).unwrap(), Some(6));
    assert_eq!(p.positions().unwrap(), [0]);
    assert_eq!(p.advance(2).unwrap(), Some(6), "never back");
    assert_eq!(p.advance(6).unwrap(), Some(6));
    assert_eq!(p.next().unwrap(), Some(9));
    assert_eq!(p.shallow_advance(10).unwrap(), None);
    assert_eq!(p.shallow_advance(0).unwrap().map(|b| b.last_docid), Some(9));
    assert_eq!(p.advance(10).unwrap(), None);
    assert_eq!((p.doc(), p.freq().unwrap(), p.block_max()), (None, 0, None));
    assert_eq!(p.positions().unwrap(), [0_u32; 0]);
}

/// Readers take views while the writer upserts and deletes: each view is consistent on its own (every live
/// document has the common term once; ids and docids agree), and views only move forward.
#[test]
fn readers_see_consistent_views_during_writes() {
    let (schema, f) = schema();
    let mut mem = memtable(&schema);
    let published = Mutex::new(mem.view());
    // The last operation on each id: the `i` of its live version, or `None` after a delete.
    let mut last: Vec<Option<Option<u32>>> = vec![None; 150];
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let (done, published, f) = (&done, &published, &f);
                scope.spawn(move || {
                    let mut checked = 0_u32;
                    let mut previous = 0;
                    loop {
                        let finished = done.load(Ordering::Acquire);
                        let view = published.lock().unwrap().clone();
                        assert!(view.version() >= previous);
                        previous = view.version();
                        check_consistent(&view, f);
                        checked += 1;
                        if finished {
                            return checked;
                        }
                    }
                })
            })
            .collect();
        for i in 0..2000_u32 {
            let id = format!("d{}", i % 150);
            if i % 7 == 6 {
                mem.delete(&id).unwrap();
                last[(i % 150) as usize] = Some(None);
            } else {
                last[(i % 150) as usize] = Some(Some(i));
                let body = format!("common w{i}");
                mem.put(&Document::new(id).with(f.body, Value::String(body)))
                    .unwrap();
            }
            // Readers also hold views from before this write while it runs.
            *published.lock().unwrap() = mem.view();
        }
        done.store(true, Ordering::Release);
        for r in readers {
            assert!(r.join().unwrap() > 0);
        }
    });
    check_consistent(&mem.view(), &f);
    // Only live versions keep their terms; ids whose last operation was a delete read as deleted.
    let view = mem.view();
    for (k, op) in last.iter().enumerate() {
        let id = format!("d{k}");
        match op {
            Some(Some(_)) => {
                let docid = view.docid_of(&id).unwrap();
                assert!(docid.is_some(), "{id}");
                assert_eq!(view.lookup(&id), docid.map(IdState::Live));
            }
            Some(None) => assert_eq!(view.lookup(&id), Some(IdState::Deleted)),
            None => assert_eq!(view.lookup(&id), None),
        }
    }
    for i in 0..2000_u32 {
        let live = last[(i % 150) as usize] == Some(Some(i));
        let term = format!("w{i}");
        assert_eq!(
            view.postings(f.body, term.as_bytes()).unwrap().is_some(),
            live,
            "{term}"
        );
    }
}

fn check_consistent(view: &MemtableView, f: &Fields) {
    let mut live = 0_u32;
    if let Some(mut p) = view.postings(f.body, b"common").unwrap() {
        while let Some(d) = p.next().unwrap() {
            assert!(view.is_live(d));
            assert_eq!(p.freq().unwrap(), 1);
            let id = view.document(d).unwrap().id().to_owned();
            assert_eq!(view.docid_of(&id).unwrap(), Some(d));
            live += 1;
        }
    }
    assert_eq!(live, view.live_count());
    let stats = view.field_stats(f.body).unwrap();
    assert_eq!(stats.docs_with_field, live);
    assert_eq!(stats.sum_field_len, 2 * u64::from(live));
}

#[test]
fn size_grows_until_the_policy_flushes() {
    use persia_engine::memtable::{FlushPolicy, FlushReason};
    use std::time::{Duration, Instant};

    let (schema, f) = schema();
    let mut mem = memtable(&schema);
    let policy = FlushPolicy {
        memtable_bytes: 64 << 10,
        idle: Duration::from_secs(10),
    };
    let start = Instant::now();
    let mut last = None;
    let mut size = mem.size_in_bytes();
    for i in 0_u64.. {
        let now = start + Duration::from_millis(i);
        if let Some(reason) = policy.due(mem.size_in_bytes(), last, now) {
            assert_eq!(reason, FlushReason::Size);
            break;
        }
        let body = format!("common w{i} {}", WORDS[(i % 6) as usize]);
        mem.put(&Document::new(format!("d{i}")).with(f.body, Value::String(body)))
            .unwrap();
        last = Some(now);
        assert!(mem.size_in_bytes() >= size);
        size = mem.size_in_bytes();
        assert!(i < 10_000, "64 KiB is reached");
    }
    assert!(mem.size_in_bytes() >= policy.memtable_bytes);
    let quiet = last.unwrap() + policy.idle;
    let small = FlushPolicy {
        memtable_bytes: usize::MAX,
        ..policy
    };
    assert_eq!(
        small.due(mem.size_in_bytes(), last, quiet),
        Some(FlushReason::Idle)
    );
}
