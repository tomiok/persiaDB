use persia_analysis::{Analyzer, WhitespaceTokenizer};
use proptest::prelude::*;

use super::*;
use crate::schema::{FieldId, SchemaBuilder, TextOptions};
use crate::segment::MAX_TERM_LEN;
use crate::view::{PostingsIterator, SearchView};

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
