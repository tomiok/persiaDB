//! Read-your-writes (ROADMAP 10.2.2; SPEC §8.4): a read on a read-write `Db` that starts after a write returned
//! sees it, from any handle and any thread, at every durability, committed or not, across flush commits.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::mpsc;

use persia::{Blob, Collection, Db, Document, Durability, OpenOptions, Value};
use proptest::prelude::*;

const SCHEMA: &str = r#"{"name": "c", "fields": {"n": {"type": "i64", "stored": true}}}"#;

fn doc(c: &Collection, id: &str, n: i64) -> Document {
    let field = c.schema().unwrap().field("n").unwrap().id();
    Document::new(id).with(field, Value::I64(n))
}

fn value(c: &Collection, id: &str) -> Option<i64> {
    let field = c.schema().unwrap().field("n").unwrap().id();
    c.get(id).unwrap().map(|d| match d.get(field) {
        Some(Value::I64(n)) => *n,
        other => panic!("{other:?}"),
    })
}

#[test]
fn a_write_is_seen_by_the_next_read_at_every_durability() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let base = db.create_collection_json(SCHEMA).unwrap();
    for (n, durability) in [Durability::Buffered, Durability::Async, Durability::Fsync]
        .into_iter()
        .enumerate()
    {
        let n = i64::try_from(n).unwrap();
        let c = base.clone().with_durability(durability);
        c.put(&doc(&c, "x", n)).unwrap();
        assert_eq!(value(&c, "x"), Some(n), "{durability:?}");
        assert_eq!(value(&base, "x"), Some(n), "another handle, {durability:?}");
        c.put_batch(&[doc(&c, "y", n), doc(&c, "z", n)]).unwrap();
        assert_eq!((value(&c, "y"), value(&c, "z")), (Some(n), Some(n)));
        c.delete("y").unwrap();
        assert_eq!(value(&c, "y"), None, "{durability:?}");
    }
    // A commit in between changes nothing a read sees.
    db.commit().unwrap();
    assert_eq!(value(&base, "x"), Some(2));
    assert_eq!(value(&base, "y"), None);
    let blob = base
        .put_with_blob(&doc(&base, "b", 9), Blob::from_reader(&b"bytes"[..]))
        .unwrap();
    let mut bytes = Vec::new();
    db.get_blob(&blob).unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"bytes");
}

#[test]
fn a_write_is_seen_by_a_read_on_another_thread_that_starts_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = db.create_collection_json(SCHEMA).unwrap();
    let (wrote, written) = mpsc::channel::<i64>();
    let (checked, check) = mpsc::channel::<Option<i64>>();
    let reader = c.clone();
    let thread = std::thread::spawn(move || {
        for n in written {
            checked.send(value(&reader, &format!("k{n}"))).unwrap();
        }
    });
    for n in 0..20 {
        c.clone()
            .with_durability(Durability::Buffered)
            .put(&doc(&c, &format!("k{n}"), n))
            .unwrap();
        wrote.send(n).unwrap();
        assert_eq!(check.recv().unwrap(), Some(n));
    }
    drop(wrote);
    thread.join().unwrap();
}

#[test]
fn a_read_only_database_sees_a_write_after_a_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = Db::open(&path).unwrap();
    let w = writer.create_collection_json(SCHEMA).unwrap();
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    let r = reader.collection("c").unwrap();
    w.put(&doc(&w, "a", 1)).unwrap(); // Fsync: in the file when it returns
    assert_eq!(value(&r, "a"), None, "not before a refresh");
    reader.refresh().unwrap();
    assert_eq!(value(&r, "a"), Some(1));
}

#[derive(Debug, Clone)]
enum Op {
    Put(u8, i64),
    Batch(Vec<(u8, i64)>),
    Delete(u8),
    Commit,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..12u8, any::<i64>()).prop_map(|(id, n)| Op::Put(id, n)),
        1 => proptest::collection::vec((0..12u8, any::<i64>()), 0..4).prop_map(Op::Batch),
        2 => (0..12u8).prop_map(Op::Delete),
        1 => Just(Op::Commit),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]

    /// After every write, every id reads as a map of the writes so far says, with a memtable so small that puts
    /// flush and commit on their own.
    #[test]
    fn reads_follow_the_writes(ops in proptest::collection::vec(op(), 1..30)) {
        let dir = tempfile::tempdir().unwrap();
        let db = OpenOptions::new().memtable_bytes(4 << 10).open(dir.path().join("app.persia")).unwrap();
        let c = db.create_collection_json(SCHEMA).unwrap().with_durability(Durability::Buffered);
        let mut model: BTreeMap<String, i64> = BTreeMap::new();
        for op in ops {
            match op {
                Op::Put(id, n) => {
                    c.put(&doc(&c, &format!("d{id}"), n)).unwrap();
                    model.insert(format!("d{id}"), n);
                }
                Op::Batch(docs) => {
                    let batch: Vec<_> = docs.iter().map(|(id, n)| doc(&c, &format!("d{id}"), *n)).collect();
                    c.put_batch(&batch).unwrap();
                    for (id, n) in docs {
                        model.insert(format!("d{id}"), n);
                    }
                }
                Op::Delete(id) => {
                    c.delete(&format!("d{id}")).unwrap();
                    model.remove(&format!("d{id}"));
                }
                Op::Commit => db.commit().unwrap(),
            }
            for id in 0..12u8 {
                let id = format!("d{id}");
                prop_assert_eq!(value(&c, &id), model.get(&id).copied(), "{}", id);
            }
        }
    }
}
