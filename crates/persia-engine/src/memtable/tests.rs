use persia_analysis::{Analyzer, WhitespaceTokenizer};
use proptest::prelude::*;
use roaring::RoaringBitmap;

use super::*;
use crate::schema::{FieldId, SchemaBuilder, TextOptions};
use crate::segment::{MAX_TERM_LEN, ReadError};
use crate::view::{KeyReader, KeywordReader, NormReader, PostingsIterator, SearchView};

/// A memtable whose two `text` fields split on whitespace only, so a token can pass [`MAX_TERM_LEN`].
fn unguarded() -> (Memtable, FieldId, FieldId) {
    let schema = SchemaBuilder::new("c")
        .field("t", FieldKind::Text(TextOptions::default()))
        .field("u", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let (t, u) = (
        schema.field("t").unwrap().id(),
        schema.field("u").unwrap().id(),
    );
    let mem = Memtable::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    for field in &mut write(&mem.shared).unwrap().fields {
        if let MemField::Text { analyzer, .. } = field {
            *analyzer = Arc::new(Analyzer::builder(WhitespaceTokenizer).build());
        }
    }
    (mem, t, u)
}

fn docs(view: &MemtableView, field: FieldId, term: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    if let Some(mut p) = view.postings(field, term).unwrap() {
        while let Some(d) = p.next().unwrap() {
            out.push(d);
        }
    }
    out
}

#[test]
fn a_document_failing_mid_index_leaves_nothing() {
    let (mut mem, t, u) = unguarded();
    let doc = |id: &str, t_text: &str, u_text: String| {
        Document::new(id)
            .with(t, Value::String(t_text.into()))
            .with(u, Value::String(u_text))
    };
    mem.put(&doc("a", "shared", "shared".into())).unwrap();
    let long = "q".repeat(MAX_TERM_LEN + 1);
    // `t` is indexed first: its new and shared terms are rolled back when `u` fails.
    let bad = doc("bad", "fresh shared", format!("fresh {long}"));
    assert_eq!(
        mem.put(&bad),
        Err(MemtableError::Segment(SegmentError::TermTooLong {
            field: u,
            len: MAX_TERM_LEN + 1
        }))
    );
    {
        let inner = read(&mem.shared);
        assert_eq!(inner.ids.len(), 1, "the id is not kept");
        for field in &inner.fields {
            let index = field.index().unwrap();
            assert_eq!(index.inverted.terms.len(), 1);
            assert_eq!(index.inverted.postings.len(), 1);
            assert_eq!((index.heads.len(), index.next.len()), (1, 1));
        }
    }
    assert_eq!(mem.version(), 1);
    // The next document gets docid 1, and its postings chain after the kept ones.
    assert_eq!(mem.put(&doc("bad", "fresh shared", "fresh".into())), Ok(1));
    let view = mem.view();
    assert_eq!(docs(&view, t, b"shared"), [0, 1]);
    assert_eq!(docs(&view, t, b"fresh"), [1]);
    assert_eq!(docs(&view, u, b"fresh"), [1]);
    assert_eq!(view.docid_of("bad").unwrap(), Some(1));
    let stats = view.field_stats(t).unwrap();
    assert_eq!((stats.docs_with_field, stats.sum_field_len), (2, 3));
}

#[derive(Debug, Clone)]
enum Step {
    Put(u8, Vec<u8>),
    /// A put whose `u` value ends with a token over [`MAX_TERM_LEN`] bytes, after new and known terms in `t`.
    Bad(u8, Vec<u8>),
    Delete(u8),
}

const WORDS: [&str; 4] = ["a", "b", "c", "fresh"];

fn words(w: &[u8]) -> String {
    let w: Vec<&str> = w
        .iter()
        .filter_map(|&i| WORDS.get(usize::from(i)).copied())
        .collect();
    w.join(" ")
}

proptest! {
    /// Failed puts mixed with upserts and deletes leave the memtable as if they never came.
    #[test]
    fn failed_puts_change_nothing(steps in prop::collection::vec(
        prop_oneof![
            (0..5_u8, prop::collection::vec(0..4_u8, 0..4)).prop_map(|(i, w)| Step::Put(i, w)),
            (0..7_u8, prop::collection::vec(0..4_u8, 0..4)).prop_map(|(i, w)| Step::Bad(i, w)),
            (0..5_u8).prop_map(Step::Delete),
        ],
        0..24,
    )) {
        let (mut mem, t, u) = unguarded();
        let (mut clean, _, _) = unguarded();
        let long = "q".repeat(MAX_TERM_LEN + 1);
        let mut views = Vec::new();
        for step in &steps {
            match step {
                Step::Put(i, w) => {
                    let doc = Document::new(format!("d{i}"))
                        .with(t, Value::String(words(w)))
                        .with(u, Value::String(words(w)));
                    prop_assert_eq!(mem.put(&doc), clean.put(&doc));
                }
                Step::Bad(i, w) => {
                    let doc = Document::new(format!("d{i}"))
                        .with(t, Value::String(format!("{} fresh", words(w))))
                        .with(u, Value::String(format!("{} {long}", words(w))));
                    let rejected = matches!(
                        mem.put(&doc),
                        Err(MemtableError::Segment(SegmentError::TermTooLong { .. }))
                    );
                    prop_assert!(rejected);
                }
                Step::Delete(i) => {
                    let id = format!("d{i}");
                    prop_assert_eq!(mem.delete(&id), clean.delete(&id));
                }
            }
            views.push((mem.view(), clean.view()));
        }
        for (a, b) in &views {
            prop_assert_eq!(a.version(), b.version());
            prop_assert_eq!(a.doc_count(), b.doc_count());
            for field in [t, u] {
                prop_assert_eq!(a.field_stats(field), b.field_stats(field));
                for term in WORDS.iter().map(|w| w.as_bytes()).chain([long.as_bytes()]) {
                    prop_assert_eq!(docs(a, field, term), docs(b, field, term));
                }
            }
            for i in 0..7 {
                let id = format!("d{i}");
                prop_assert_eq!(a.lookup(&id), b.lookup(&id));
            }
        }
        let (a, b) = (read(&mem.shared), read(&clean.shared));
        prop_assert_eq!(a.ids.len(), b.ids.len());
        for (x, y) in a.fields.iter().zip(&b.fields) {
            let (x, y) = (x.index().unwrap(), y.index().unwrap());
            prop_assert_eq!(x.inverted.terms.len(), y.inverted.terms.len());
            prop_assert_eq!(&x.heads, &y.heads);
            prop_assert_eq!(&x.next, &y.next);
        }
    }
}

/// ROADMAP 6.3.9: an operation the memtable rejects never reaches the log, and one whose logging fails is not
/// visible.
#[test]
fn logged_writes_reach_the_log_only_when_accepted() {
    let schema = SchemaBuilder::new("c")
        .field("t", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let t = schema.field("t").unwrap().id();
    let doc = |id: &str| Document::new(id).with(t, Value::String("hello world".into()));
    let mut mem = Memtable::new(&schema, &AnalyzerRegistry::builtin())
        .unwrap()
        .with_max_docs(2);
    let mut log = Vec::new();
    let mut logging = |bytes: &[u8]| {
        log.push(bytes.to_vec());
        Ok::<(), &str>(())
    };
    assert_eq!(mem.put_logged(&doc("a"), &mut logging), Ok(0));
    // The failed log: the document is not there, and its id is unknown to every view.
    assert_eq!(
        mem.put_logged(&doc("b"), |_| Err("disk")),
        Err(Logged::Log("disk"))
    );
    let view = mem.view();
    assert_eq!((view.lookup("b"), view.live_count()), (None, 1));
    assert_eq!(docs(&view, t, b"hello"), [0]);
    // An invalid document is rejected before logging.
    let bad = Document::new("e").with(t, Value::I64(1));
    assert!(matches!(
        mem.put_logged(&bad, &mut logging),
        Err(Logged::Rejected(MemtableError::Segment(
            SegmentError::Document(_)
        )))
    ));
    assert_eq!(mem.put_logged(&doc("c"), &mut logging), Ok(1));
    // So is a put once every docid is used.
    assert_eq!(
        mem.put_logged(&doc("d"), &mut logging),
        Err(Logged::Rejected(MemtableError::Segment(
            SegmentError::Full { max_docs: 2 }
        )))
    );
    assert_eq!(log.len(), 2);
    // The payload is the document's full encoding, what replay decodes.
    assert_eq!(Document::decode(&log[1], &schema).unwrap(), doc("c"));
    // Deletes: a bad id is rejected unlogged; a failed log leaves the id live.
    let mut deletes = 0;
    assert!(matches!(
        mem.delete_logged("", || {
            deletes += 1;
            Ok::<(), &str>(())
        }),
        Err(Logged::Rejected(_))
    ));
    assert_eq!(
        mem.delete_logged("a", || Err("disk")),
        Err(Logged::Log("disk"))
    );
    assert_eq!(deletes, 0);
    assert_eq!(mem.view().lookup("a"), Some(IdState::Live(0)));
    assert_eq!(mem.delete_logged("a", || Ok::<(), &str>(())), Ok(true));
    assert_eq!(mem.view().lookup("a"), Some(IdState::Deleted));
}

/// The ids a flush tombstones (ROADMAP 6.4.3): every id an applied put or delete named, not one whose only operation
/// failed to log (its id stays interned).
#[test]
fn for_each_id_yields_the_ids_of_applied_operations() {
    let (mut mem, t, _) = unguarded();
    mem.put(&Document::new("put").with(t, Value::String("a".into())))
        .unwrap();
    mem.delete("deleted").unwrap();
    let failed = mem.put_logged(
        &Document::new("unlogged").with(t, Value::String("b".into())),
        |_| Err("log failed"),
    );
    assert!(matches!(failed, Err(Logged::Log("log failed"))));
    let failed = mem.delete_logged("unlogged-delete", || Err("log failed"));
    assert!(matches!(failed, Err(Logged::Log("log failed"))));
    let mut ids = Vec::new();
    mem.for_each_id(|id| {
        ids.push(String::from_utf8(id.to_vec()).unwrap());
        Ok::<(), ()>(())
    })
    .unwrap();
    ids.sort();
    assert_eq!(ids, ["deleted", "put"]);
}

/// The fields of [`chunked`]: `body` text, `tag` keyword, `n` i64, `flag` bool.
struct Chunked {
    schema: CollectionSchema,
    body: FieldId,
    tag: FieldId,
    n: FieldId,
    flag: FieldId,
}

fn chunked() -> Chunked {
    let schema = SchemaBuilder::new("c")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field(
            "tag",
            FieldKind::Keyword(crate::schema::KeywordOptions::default()),
        )
        .field("n", FieldKind::I64 { stored: false })
        .field("flag", FieldKind::Bool { stored: false })
        .build()
        .unwrap();
    let id = |name| schema.field(name).unwrap().id();
    Chunked {
        body: id("body"),
        tag: id("tag"),
        n: id("n"),
        flag: id("flag"),
        schema,
    }
}

/// Document `i` of [`chunked`]: `i % 50` words in `body` (none for 0), tag `t{1499 - i % 1500}` (interned in reverse byte order), `n = i`, `flag` for even `i`.
fn chunked_doc(f: &Chunked, id: &str, i: u32) -> Document {
    Document::new(id)
        .with(f.body, Value::String("w ".repeat((i % 50) as usize)))
        .with(f.tag, Value::String(format!("t{:05}", 1_499 - i % 1_500)))
        .with(f.n, Value::I64(i64::from(i)))
        .with(f.flag, Value::Bool(i.is_multiple_of(2)))
}

/// ROADMAP 7.6.6: the memtable's whole-field copies for a view (`deleted`, `present`, `trues`, `keys`, `keywords`,
/// `norms`) are taken in chunks, with a step per item (per chunk for norms) outside the lock: writers that land
/// between chunks (new documents, a failed put, a replace, a delete) change nothing the view sees.
#[test]
fn chunked_copies_are_the_view_while_writers_interleave() {
    let f = chunked();
    let mem =
        std::sync::Mutex::new(Memtable::new(&f.schema, &AnalyzerRegistry::builtin()).unwrap());
    // 2 500 documents, the view's cutoff inside the third chunk. Before the view: `d3` deleted and `d5` replaced
    // (by docid 2 500, as document 5).
    for i in 0..2_500 {
        mem.lock()
            .unwrap()
            .put(&chunked_doc(&f, &format!("d{i}"), i))
            .unwrap();
    }
    mem.lock().unwrap().delete("d3").unwrap();
    mem.lock().unwrap().put(&chunked_doc(&f, "d5", 5)).unwrap();
    let view = mem.lock().unwrap().view();
    let docs = 2_501_u32;
    let i_of = |docid: u32| if docid == 2_500 { 5 } else { docid };
    // Writes land in every phase of every copy: at its first step (norms take two), in its first chunks, and in the
    // keyword values after their 2 501 postings. A failed put interns new terms that are truncated, and the next put
    // interns other new terms under the same ids; then new documents, a replace and a delete, all after the view.
    let writes = std::cell::Cell::new(0_u32);
    let steps = std::cell::Cell::new(0_u32);
    let step = || {
        steps.set(steps.get() + 1);
        if matches!(steps.get(), 1 | 1_000 | 3_000) {
            let w = writes.get();
            writes.set(w + 1);
            let mut mem = mem.lock().unwrap();
            let fresh = |id: &str, word: &str| {
                Document::new(id)
                    .with(f.body, Value::String(format!("{word}{w}")))
                    .with(f.tag, Value::String(format!("{word}{w}")))
            };
            let failed = mem.put_logged(&fresh("x", "lost"), |_| Err("disk"));
            assert!(failed.is_err());
            mem.put(&fresh(&format!("y{w}"), "kept")).unwrap();
            for i in 0..300 {
                mem.put(&chunked_doc(&f, &format!("n{w}-{i}"), 4_000 + i))
                    .unwrap();
            }
            mem.put(&chunked_doc(&f, "d7", 8)).unwrap();
            mem.delete(&format!("d{}", 9 + w)).unwrap();
        }
        Ok(())
    };
    let counted = |expected: u32| {
        assert_eq!(steps.replace(0), expected);
    };

    assert_eq!(
        view.deleted_stepped(&step).unwrap(),
        RoaringBitmap::from_iter([3, 5])
    );
    counted(docs);
    // A document whose `i` is a multiple of 50 has no `body` token.
    let all: RoaringBitmap = (0..docs).collect();
    let worded: RoaringBitmap = (0..docs).filter(|&d| !i_of(d).is_multiple_of(50)).collect();
    assert_eq!(view.present_stepped(f.body, &step).unwrap(), worded);
    counted(u32::try_from(worded.len()).unwrap());
    for field in [f.tag, f.n, f.flag] {
        assert_eq!(view.present_stepped(field, &step).unwrap(), all);
        counted(docs);
    }
    let even: RoaringBitmap = (0..docs).filter(|&d| i_of(d).is_multiple_of(2)).collect();
    assert_eq!(view.trues_stepped(f.flag, &step).unwrap(), even);
    counted(docs);

    let mut keys = view.keys_stepped(f.n, &step).unwrap();
    counted(docs);
    for docid in 0..docs {
        let key = persia_format::for_codec::i64_to_key(i64::from(i_of(docid)));
        assert_eq!(keys.key(docid).unwrap(), Some(key), "{docid}");
    }
    assert_eq!(keys.key(docs).unwrap(), None);

    let mut words = view.keywords_stepped(f.tag, &step).unwrap();
    // A step per posting, then per distinct tag.
    counted(docs + 1_500);
    assert_eq!(words.len(), 1_500);
    for docid in 0..docs {
        let ords = words.ords(docid).unwrap().to_vec();
        assert_eq!(ords.len(), 1);
        let value = words.value(ords[0]).unwrap();
        assert_eq!(
            value,
            format!("t{:05}", 1_499 - i_of(docid) % 1_500).as_bytes()
        );
    }
    assert!(words.ords(docs).is_err());
    // Byte order: ord `o` is `t{o}`.
    assert_eq!(words.value(7), Some(&b"t00007"[..]));

    let mut norms = view.norms_stepped(f.body, &step).unwrap();
    // One step per full chunk: 1 024 and 2 048 of 2 501.
    counted(2);
    for docid in 0..docs {
        let expected = norms::encode(i_of(docid) % 50);
        assert_eq!(norms.get(docid).unwrap(), expected, "{docid}");
    }
    // Copied once per view and field: no step the second time.
    view.norms_stepped(f.body, &step).unwrap();
    counted(0);
    // Two writes per copy, three in the keywords copy (one in its values), one in the norms copy.
    assert_eq!(writes.get(), 18);
    // The plain calls are the stepped ones.
    assert_eq!(view.deleted().unwrap(), RoaringBitmap::from_iter([3, 5]));
    assert_eq!(view.trues(f.flag).unwrap(), even);
}

/// A step's error stops each copy, outside the lock: a writer can still write after it.
#[test]
fn a_step_error_stops_each_chunked_copy() {
    let f = chunked();
    let mut mem = Memtable::new(&f.schema, &AnalyzerRegistry::builtin()).unwrap();
    for i in 0..3_000 {
        mem.put(&chunked_doc(&f, &format!("d{i}"), i % 7)).unwrap();
    }
    let view = mem.view();
    let stop = |after: u32| {
        let steps = std::cell::Cell::new(0_u32);
        move || {
            steps.set(steps.get() + 1);
            if steps.get() >= after {
                Err(ReadError::Interrupted(crate::search::Interrupt::Cancelled))
            } else {
                Ok(())
            }
        }
    };
    let stopped = |r: Result<(), ReadError>| {
        assert!(matches!(r, Err(ReadError::Interrupted(_))), "{r:?}");
    };
    stopped(view.deleted_stepped(&stop(2_000)).map(drop));
    stopped(view.present_stepped(f.body, &stop(2_000)).map(drop));
    stopped(view.trues_stepped(f.flag, &stop(2_000)).map(drop));
    stopped(view.keys_stepped(f.n, &stop(2_000)).map(drop));
    // Past the postings, in the values.
    stopped(view.keywords_stepped(f.tag, &stop(3_001)).map(drop));
    stopped(view.norms_stepped(f.body, &stop(2)).map(drop));
    // A stopped copy is not kept: the next one copies again, whole.
    assert!(view.norms_stepped(f.body, &|| Ok(())).is_ok());
    let mut keys = view.keys_stepped(f.n, &|| Ok(())).unwrap();
    assert_eq!(
        keys.key(2_999).unwrap(),
        Some(persia_format::for_codec::i64_to_key(2_999 % 7))
    );
    // The whole column: `i % 7` gives tags `t01493` to `t01499`, and every document holds one.
    let mut words = view.keywords_stepped(f.tag, &|| Ok(())).unwrap();
    let values: Vec<Vec<u8>> = (0..words.len())
        .map(|o| words.value(o).unwrap().to_vec())
        .collect();
    let want: Vec<Vec<u8>> = (1_493..=1_499)
        .map(|t| format!("t{t:05}").into_bytes())
        .collect();
    assert_eq!(values, want);
    for docid in 0..3_000 {
        let tag = format!("t{:05}", 1_499 - docid % 7);
        let ords = words.ords(docid).unwrap().to_vec();
        assert_eq!(ords.len(), 1);
        assert_eq!(words.value(ords[0]), Some(tag.as_bytes()));
    }
    mem.put(&chunked_doc(&f, "after", 1)).unwrap();
}

/// While held, new views (the writer's and a reader's) see the memtable as at the hold, an upserted document
/// included; releasing shows every write, and a view taken during the hold keeps what it saw (ROADMAP 6.3.12).
#[test]
fn a_hold_keeps_new_views_at_the_writes_before_it() {
    let (mut mem, t, _) = unguarded();
    let doc = |id: &str, text: &str| Document::new(id).with(t, Value::String(text.into()));
    mem.put(&doc("a", "old")).unwrap();
    let reader = mem.reader();
    mem.hold();
    mem.hold();
    assert!(mem.is_held());
    mem.put(&doc("a", "new")).unwrap();
    mem.put(&doc("b", "new")).unwrap();
    for held in [mem.view(), reader.view()] {
        assert_eq!((held.version(), held.live_count()), (1, 1));
        assert_eq!(held.lookup("a"), Some(IdState::Live(0)));
        assert_eq!(held.lookup("b"), None);
        assert_eq!(docs(&held, t, b"old"), [0]);
        assert_eq!(docs(&held, t, b"new"), Vec::<u32>::new());
        let stats = held.field_stats(t).unwrap();
        assert_eq!((stats.docs_with_field, stats.sum_field_len), (1, 1));
    }
    let during = reader.view();
    mem.release();
    assert!(!mem.is_held());
    let after = reader.view();
    assert_eq!((after.version(), after.live_count()), (3, 2));
    assert_eq!(after.lookup("a"), Some(IdState::Live(1)));
    assert_eq!(docs(&after, t, b"new"), [1, 2]);
    assert_eq!(docs(&after, t, b"old"), Vec::<u32>::new());
    assert_eq!((during.version(), during.live_count()), (1, 1));
    assert_eq!(docs(&during, t, b"new"), Vec::<u32>::new());
    // A reader of a replaced memtable can end its hold too.
    mem.hold();
    assert!(reader.reads(&mem));
    reader.release();
    assert!(!mem.is_held());
}

/// A hold's copy of the statistics counts toward the flush threshold (`size_in_bytes`) while it lives.
#[test]
fn a_hold_counts_its_copy_of_the_statistics() {
    let (mut mem, t, _) = unguarded();
    mem.put(&Document::new("a").with(t, Value::String("x".into())))
        .unwrap();
    let before = mem.size_in_bytes();
    mem.hold();
    let held = mem.size_in_bytes();
    assert!(held > before, "{held} <= {before}");
    mem.release();
    assert_eq!(mem.size_in_bytes(), before);
}

/// ROADMAP 7.5.5: a view copies a field's column keys and keyword column once; later reads (a sort, a facet, a
/// filter check of the same view, or of a clone of it) share that copy and step no more.
#[test]
fn column_copies_are_kept_per_view() {
    let f = chunked();
    let mut mem = Memtable::new(&f.schema, &AnalyzerRegistry::builtin()).unwrap();
    for i in 0..3_000 {
        mem.put(&chunked_doc(&f, &format!("d{i}"), i % 7)).unwrap();
    }
    let view = mem.view();
    let steps = std::cell::Cell::new(0_u32);
    let step = || {
        steps.set(steps.get() + 1);
        Ok(())
    };
    let mut keys = view.keys_stepped(f.n, &step).unwrap();
    let mut words = view.keywords_stepped(f.tag, &step).unwrap();
    assert!(steps.replace(0) > 0);
    // Writes after the view change nothing it copied.
    mem.put(&chunked_doc(&f, "d1", 6)).unwrap();
    for view in [view.clone(), view] {
        let mut again = view.keys_stepped(f.n, &step).unwrap();
        let mut words_again = view.keywords_stepped(f.tag, &step).unwrap();
        assert_eq!(steps.get(), 0);
        assert_eq!(words_again.len(), words.len());
        for docid in 0..3_000 {
            assert_eq!(
                again.key(docid).unwrap(),
                keys.key(docid).unwrap(),
                "{docid}"
            );
            assert_eq!(
                words_again.ords(docid).unwrap(),
                words.ords(docid).unwrap(),
                "{docid}"
            );
        }
    }
}

/// ROADMAP 7.5.5: a filter scan of a column (`try_column_keys`) reads a view's kept copy of its keys, and keeps the
/// copy it makes when it reads the whole column, so a later sort or check of the view copies nothing.
#[test]
fn a_column_scan_shares_the_views_copy() {
    let f = chunked();
    let mut mem = Memtable::new(&f.schema, &AnalyzerRegistry::builtin()).unwrap();
    for i in 0..3_000 {
        mem.put(&chunked_doc(&f, &format!("d{i}"), i % 7)).unwrap();
    }
    let scan = |view: &MemtableView| {
        let mut seen = Vec::new();
        view.try_column_keys(f.n, |docid, key| {
            seen.push((docid, key));
            Ok(())
        })
        .unwrap();
        seen
    };
    let steps = std::cell::Cell::new(0_u32);
    let step = || {
        steps.set(steps.get() + 1);
        Ok(())
    };
    // A scan first: the sort after it steps no more.
    let view = mem.view();
    let scanned = scan(&view);
    assert_eq!(scanned.len(), 3_000);
    let mut keys = view.keys_stepped(f.n, &step).unwrap();
    assert_eq!(steps.replace(0), 0);
    for &(docid, key) in &scanned {
        assert_eq!(keys.key(docid).unwrap(), Some(key));
    }
    // A sort first: the scan after it reads its copy, not the memtable written since.
    let view = mem.view();
    view.keys_stepped(f.n, &step).unwrap();
    mem.put(&chunked_doc(&f, "late", 5)).unwrap();
    assert_eq!(scan(&view), scanned);
    // A scan stopped by `f` keeps nothing: the next one reads the whole column.
    let view = mem.view();
    let stopped = view.try_column_keys(f.n, |_, _| {
        Err(ReadError::Interrupted(crate::search::Interrupt::Cancelled))
    });
    assert!(stopped.is_err());
    assert_eq!(scan(&view).len(), 3_001);
}

/// The keyword column `words` as `(values in ord order, each document's values)`, to compare two copies.
fn keyword_rows(words: &mut MemtableKeywords, docs: u32) -> (Vec<Vec<u8>>, Vec<Vec<Vec<u8>>>) {
    use crate::view::KeywordReader;
    let values = (0..words.len())
        .map(|o| words.value(o).unwrap().to_vec())
        .collect();
    let rows = (0..docs)
        .map(|d| {
            let ords = words.ords(d).unwrap().to_vec();
            ords.iter()
                .map(|&o| words.value(o).unwrap().to_vec())
                .collect()
        })
        .collect();
    (values, rows)
}

/// A `put` that interns new `keyword` terms and then fails (a `text` token over `MAX_TERM_LEN`, analyzed after the
/// `keyword` field) leaves the documents as they were: the view after it shares the copy of the view before it, and
/// that copy is its own. The next `put` takes the docid and term ids the failed one rolled back; a view of it copies
/// again, and gets its value.
#[test]
fn a_failed_put_between_two_views_keeps_their_shared_copy() {
    use crate::schema::KeywordOptions;
    use crate::view::KeywordReader;

    // `sku` before `body`, so a put interns its `sku` value before `body` rejects it.
    let schema = SchemaBuilder::new("c")
        .field(
            "sku",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let (sku, body) = (
        schema.field("sku").unwrap().id(),
        schema.field("body").unwrap().id(),
    );
    let put = |id: &str, value: &str, text: &str| {
        Document::new(id)
            .with(sku, Value::String(value.into()))
            .with(body, Value::String(text.into()))
    };
    let mut mem = Memtable::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    // Whitespace only: the builtin analyzers drop a token over `MAX_TERM_LEN` instead of rejecting it.
    for field in &mut write(&mem.shared).unwrap().fields {
        if let MemField::Text { analyzer, .. } = field {
            *analyzer = Arc::new(Analyzer::builder(WhitespaceTokenizer).build());
        }
    }
    for i in 0..20 {
        mem.put(&put(&format!("d{i}"), &format!("v{:02}", i % 6), "w"))
            .unwrap();
    }
    let before = mem.view();
    let mut shared = before.keywords(sku).unwrap();
    let long = "q".repeat(MAX_TERM_LEN + 1);
    assert!(mem.put(&put("bad", "fresh-value", &long)).is_err());
    let after = mem.view();
    assert_eq!(after.doc_count(), before.doc_count());
    let mut read = after.keywords(sku).unwrap();
    assert!(std::ptr::eq(
        read.value(0).unwrap(),
        shared.value(0).unwrap()
    ));
    let mut own = after.unshared().keywords(sku).unwrap();
    assert_eq!(keyword_rows(&mut read, 20), keyword_rows(&mut own, 20));
    assert_eq!(keyword_rows(&mut shared, 20), keyword_rows(&mut own, 20));
    // The rolled-back docid and term id, taken by a value sorting first.
    mem.put(&put("good", "a-first", "w")).unwrap();
    let next = mem.view();
    let mut copied = next.keywords(sku).unwrap();
    assert!(!std::ptr::eq(
        copied.value(0).unwrap(),
        shared.value(0).unwrap()
    ));
    assert_eq!(copied.value(0), Some(&b"a-first"[..]));
    let mut own = next.unshared().keywords(sku).unwrap();
    assert_eq!(keyword_rows(&mut copied, 21), keyword_rows(&mut own, 21));
}

/// The cached copies' size is stored, not walked on each write (`flush_if_full` reads it after every `put` and
/// `delete`), and it stays the size a full walk of the copies counts, across puts, deletes and new copies.
#[test]
fn the_shared_copies_size_is_their_full_count_after_puts_and_deletes() {
    use crate::schema::KeywordOptions;

    let schema = SchemaBuilder::new("c")
        .field(
            "sku",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .build()
        .unwrap();
    let sku = schema.field("sku").unwrap().id();
    let mut mem = Memtable::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    let cached = |mem: &Memtable| {
        let inner = read(&mem.shared);
        (inner.columns.size_in_bytes(), inner.columns.count_bytes())
    };
    let empty = cached(&mem).0;
    for round in 0..6_u32 {
        for i in 0..40 {
            let value = "v".repeat(usize::try_from(1 + (i * 7 + round) % 13).unwrap());
            mem.put(&Document::new(format!("d{i}-{round}")).with(sku, Value::String(value)))
                .unwrap();
        }
        for i in (0..40).step_by(3) {
            mem.delete(&format!("d{i}-{}", round / 2)).unwrap();
        }
        let view = mem.view();
        let words = view.keywords(sku).unwrap();
        assert_eq!(words.size_in_bytes(), words.count_bytes());
        // Writes after the copy leave it, and its size, as they were.
        mem.put(&Document::new(format!("late-{round}")).with(sku, Value::String("zz".into())))
            .unwrap();
        mem.delete(&format!("d1-{round}")).unwrap();
        let (stored, counted) = cached(&mem);
        assert_eq!(stored, counted);
        assert!(stored >= empty + words.count_bytes());
    }
}
