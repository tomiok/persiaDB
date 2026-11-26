//! `persia dump documents` (ROADMAP 10.4.1): the live documents of one collection as JSON Lines, read from a
//! read-only open (module `open`: no lock, nothing written, a live writer may have the database open).
//!
//! Each line is one document in the output form of SPEC §6.4.1 ([`Document::to_json`]): an object with `id`, then
//! `blob` if the document references one, then its fields and extra fields in ascending byte order of their names.
//! A line holds no raw line break (JSON escapes them inside strings), so the output splits on `\n`, and
//! [`Document::from_json`] with the collection's schema reads each line back.
//!
//! **Which documents.** Those a reader of that snapshot sees (SPEC §8.3): the committed segments' documents their
//! tombstones leave live, without the copies a WAL record replaces or deletes, then the documents of the WAL records
//! not yet in a segment (acknowledged writes, those after the last commit included).
//!
//! **Order.** Storage order, which is the same for every dump of one file state: the segments oldest first, as the
//! manifest lists them, each in ascending docid; then the WAL's documents in the order they were logged. A merge or a
//! commit by a writer changes the order of a later dump, not the set.
//!
//! **What a line holds.** A document as `get` returns it: the id, the blob reference (not the blob's bytes, which stay
//! in `<name>.persia.blobs`), the fields with `stored: true` and the extra fields. A field that is indexed but not
//! stored is not in a segment's documents and cannot be dumped.
//!
//! The segments' documents are read and written one at a time, so memory does not grow with them. The documents of
//! the WAL records are in memory already: the open replayed them into the memtable.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use persia_engine::schema::CollectionSchema;
use persia_engine::view::SearchView;

use crate::open;

/// Opens the database at `path` read-only and writes the live documents of `collection` to `out`, one JSON object
/// per line.
pub fn dump(out: &mut dyn Write, path: &Path, collection: &str) -> Result<()> {
    let db = open::read_only(path)?;
    let snapshot = db.snapshot();
    let Some(found) = snapshot.collection(collection) else {
        let names: Vec<String> = snapshot
            .collections()
            .map(|(name, _)| format!("{name:?}"))
            .collect();
        bail!(
            "{} has no collection {collection:?} (it has: {})",
            path.display(),
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            }
        );
    };
    let schema = found.schema();
    let memtable = found.memtable();
    let reading = || {
        format!(
            "reading the collection {collection:?} of {}",
            path.display()
        )
    };
    let views = found.views(&memtable).with_context(reading)?;
    for view in views.segments() {
        write_live(out, &view, schema).with_context(reading)?;
    }
    write_live(out, views.memtable(), schema).with_context(reading)
}

/// Writes every live document of `view`, in ascending docid.
fn write_live<V: SearchView>(
    out: &mut dyn Write,
    view: &V,
    schema: &CollectionSchema,
) -> Result<()> {
    let mut from = 0;
    while let Some(docid) = view.next_live(from)? {
        let document = view.document(docid)?;
        writeln!(out, "{}", document.to_json(schema)?)?;
        let Some(next) = docid.checked_add(1) else {
            break;
        };
        from = next;
    }
    Ok(())
}

/// Its strategies and helpers are shared with the property test of `import` (the round trip of a dump).
#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use persia_engine::db::{Database, DatabaseOptions};
    use persia_engine::document::{Document, MAX_DATETIME, MIN_DATETIME, Value};
    use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
    use persia_engine::wal::Durability;
    use proptest::collection::{btree_map, vec};
    use proptest::prelude::*;
    use serde_json::Value as Json;

    use super::*;

    /// Every field type of SPEC §6.1, all stored, and not strict, so documents carry extra fields too.
    pub(crate) fn schema() -> CollectionSchema {
        SchemaBuilder::new("c")
            .field("title", FieldKind::Text(TextOptions::default()))
            .field("tags", FieldKind::Keyword(KeywordOptions::default()))
            .field("n", FieldKind::I64 { stored: true })
            .field("x", FieldKind::F64 { stored: true })
            .field("ok", FieldKind::Bool { stored: true })
            .field("at", FieldKind::Datetime { stored: true })
            .field("meta", FieldKind::Stored)
            .strict(false)
            .build()
            .unwrap()
    }

    /// Strings with what a line format must survive: line breaks, quotes, separators Unicode calls line ends.
    fn text() -> impl Strategy<Value = String> {
        prop_oneof![
            4 => ".{0,12}",
            1 => Just("two\nlines\r\n\"quoted\" \\ \u{2028}\u{2029}\u{85} end".to_owned()),
            1 => any::<String>(),
        ]
    }

    fn finite() -> impl Strategy<Value = f64> {
        any::<f64>().prop_filter("finite", |x| x.is_finite())
    }

    /// Any JSON value a `stored` field holds: not `null` at the top, `null` allowed inside.
    fn stored() -> impl Strategy<Value = Json> {
        let leaf = prop_oneof![
            Just(Json::Null),
            any::<bool>().prop_map(Json::from),
            any::<i64>().prop_map(Json::from),
            any::<u64>().prop_map(Json::from),
            finite().prop_map(Json::from),
            text().prop_map(Json::from),
        ];
        leaf.prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                vec(inner.clone(), 0..4).prop_map(Json::Array),
                btree_map("[a-z]{1,4}", inner, 0..4)
                    .prop_map(|m| Json::Object(m.into_iter().collect())),
            ]
        })
        .prop_filter("a stored value is not null", |v| !v.is_null())
    }

    /// A valid document of [`schema`], its id one of six so that puts replace and deletes hit.
    fn document() -> impl Strategy<Value = Document> {
        let strings = prop_oneof![
            text().prop_map(Value::String),
            vec(text(), 0..3).prop_map(Value::Strings),
        ];
        let typed = (
            proptest::option::of(text().prop_map(Value::String)),
            proptest::option::of(strings),
            proptest::option::of(any::<i64>().prop_map(Value::I64)),
            proptest::option::of(finite().prop_map(Value::F64)),
            proptest::option::of(any::<bool>().prop_map(Value::Bool)),
            proptest::option::of((MIN_DATETIME..=MAX_DATETIME).prop_map(Value::Datetime)),
            proptest::option::of(stored().prop_map(Value::Json)),
        );
        let extras = btree_map("extra_[a-c]", stored(), 0..3);
        ("d[0-5]", typed, extras).prop_map(|(id, typed, extras)| {
            let schema = schema();
            let (title, tags, n, x, ok, at, meta) = typed;
            let values = [
                ("title", title),
                ("tags", tags),
                ("n", n),
                ("x", x),
                ("ok", ok),
                ("at", at),
                ("meta", meta),
            ];
            let mut doc = Document::new(id);
            for (name, value) in values {
                if let Some(value) = value {
                    doc.insert(schema.field(name).unwrap().id(), value);
                }
            }
            for (name, value) in extras {
                doc.insert_extra(name, value);
            }
            doc
        })
    }

    #[derive(Debug, Clone)]
    pub(crate) enum Op {
        Put(Document),
        Delete(String),
        Commit,
    }

    pub(crate) fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            6 => document().prop_map(Op::Put),
            2 => "d[0-5]".prop_map(Op::Delete),
            1 => Just(Op::Commit),
        ]
    }

    /// Runs `ops` on a new database at `path` and returns the documents left live, by id. Writes reach the file
    /// (`Async`) without a sync, so a read-only open sees those after the last commit as WAL records.
    pub(crate) fn apply(path: &Path, ops: Vec<Op>) -> (Database, BTreeMap<String, Document>) {
        let mut db = Database::create(path, DatabaseOptions::default()).unwrap();
        db.create_collection(schema()).unwrap();
        let mut live = BTreeMap::new();
        for op in ops {
            match op {
                Op::Put(doc) => {
                    db.put("c", &doc, Durability::Async).unwrap();
                    live.insert(doc.id().to_owned(), doc);
                }
                Op::Delete(id) => {
                    db.delete("c", &id, Durability::Async).unwrap();
                    live.remove(&id);
                }
                Op::Commit => {
                    db.commit().unwrap();
                }
            }
        }
        (db, live)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 512 } else { 48 }))]

        /// The round trip `import` relies on (ROADMAP 10.4.2): after any puts, deletes and commits, the dump holds
        /// each live document on one line, exactly once, and `Document::from_json` reads the line back as the
        /// document that was put, which writes the same line again.
        #[test]
        fn every_dumped_line_reads_back_as_the_live_document(ops in vec(op(), 0..24)) {
            let schema = schema();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("app.persia");
            let (writer, live) = apply(&path, ops);
            let mut out = Vec::new();
            dump(&mut out, &path, "c").unwrap();
            let text = String::from_utf8(out).unwrap();
            prop_assert!(text.is_empty() || text.ends_with('\n'));
            let mut read = BTreeMap::new();
            for line in text.split_terminator('\n') {
                let doc = Document::from_json(line, &schema).unwrap();
                prop_assert_eq!(doc.to_json(&schema).unwrap(), line);
                let id = doc.id().to_owned();
                prop_assert!(read.insert(id.clone(), doc).is_none(), "{} is dumped twice", id);
            }
            prop_assert_eq!(read, live);
            drop(writer);
        }
    }

    /// A writer that accepts `lines` lines, then fails as a closed pipe does.
    struct ClosesAfter {
        lines: usize,
        written: Vec<u8>,
    }

    impl ClosesAfter {
        fn lines_written(&self) -> usize {
            self.written
                .split(|b| *b == b'\n')
                .count()
                .saturating_sub(1)
        }
    }

    impl Write for ClosesAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.lines_written() >= self.lines {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Documents are written as they are read, not collected first: a reader that goes away after two lines got
    /// exactly those two, and the dump ends there with the pipe's error (which `main` reads as a quiet end).
    #[test]
    fn a_dump_streams_and_stops_at_a_closed_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let puts = ["d0", "d1", "d2", "d3"].map(|id| Op::Put(Document::new(id)));
        let (writer, _) = apply(&path, [&puts[..2], &[Op::Commit], &puts[2..]].concat());
        let mut out = ClosesAfter {
            lines: 2,
            written: Vec::new(),
        };
        let err = dump(&mut out, &path, "c").unwrap_err();
        assert!(crate::is_broken_pipe(&err), "{err:#}");
        assert_eq!(out.written, b"{\"id\":\"d0\"}\n{\"id\":\"d1\"}\n");
        // The same through the segment and into the WAL's documents.
        let mut out = ClosesAfter {
            lines: 3,
            written: Vec::new(),
        };
        let err = dump(&mut out, &path, "c").unwrap_err();
        assert!(crate::is_broken_pipe(&err), "{err:#}");
        assert_eq!(
            out.written,
            b"{\"id\":\"d0\"}\n{\"id\":\"d1\"}\n{\"id\":\"d2\"}\n"
        );
        drop(writer);
    }
}
