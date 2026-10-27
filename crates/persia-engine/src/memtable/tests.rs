use persia_analysis::{Analyzer, WhitespaceTokenizer};

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
