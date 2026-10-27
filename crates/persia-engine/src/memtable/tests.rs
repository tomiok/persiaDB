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
