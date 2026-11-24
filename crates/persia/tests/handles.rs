//! `Send + Sync` handles, reader snapshots and single-writer errors (ROADMAP 10.2.1; SPEC §8.4, §8.6).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::sync::{Arc, Barrier};

use persia::{BlobReader, Collection, Db, Document, ErrorCode, OpenOptions};

#[test]
fn every_handle_is_send_sync_and_static() {
    fn assert_handle<T: Send + Sync + 'static>() {}
    fn assert_send<T: Send + 'static>() {}
    assert_handle::<Db>();
    assert_handle::<Collection>();
    assert_handle::<OpenOptions>();
    assert_handle::<persia::Error>();
    assert_send::<BlobReader>();
}

#[test]
fn a_second_writer_fails_at_once_until_the_first_closes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let first = Db::open(&path).unwrap();
    for options in [
        OpenOptions::new(),
        OpenOptions::new().create_if_missing(false),
    ] {
        let err = options.open(&path).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Conflict, "{err}");
        assert!(err.message().contains("app.persia"), "{err}");
    }
    // Readers open beside the writer.
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    first.close().unwrap();
    let second = Db::open(&path).unwrap();
    drop(reader);
    second.close().unwrap();
}

#[test]
fn one_db_serves_many_threads() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path().join("app.persia")).unwrap());
    db.create_collection_json(r#"{"name": "c", "fields": {}}"#)
        .unwrap();
    let threads = 4;
    let per_thread = 25;
    let barrier = Arc::new(Barrier::new(threads * 2));
    let mut handles = Vec::new();
    for t in 0..threads {
        let (writer, start) = (Arc::clone(&db), Arc::clone(&barrier));
        handles.push(std::thread::spawn(move || {
            let c = writer.collection("c").unwrap();
            start.wait();
            for i in 0..per_thread {
                c.put(&Document::new(format!("{t}-{i}"))).unwrap();
                // Read-your-writes from the writing thread itself.
                assert!(c.get(&format!("{t}-{i}")).unwrap().is_some());
            }
        }));
        let (reader, start) = (Arc::clone(&db), Arc::clone(&barrier));
        handles.push(std::thread::spawn(move || {
            let c = reader.collection("c").unwrap();
            start.wait();
            // Never blocked by the writer, and consistent with its order: once a write is seen, every write the
            // same thread made before it is seen too.
            for _ in 0..per_thread {
                let visible = |i: usize| c.get(&format!("{t}-{i}")).unwrap().is_some();
                if let Some(highest) = (0..per_thread).rev().find(|&i| visible(i)) {
                    assert!(
                        (0..=highest).all(visible),
                        "{t}: a write before {highest} is missing"
                    );
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let c = db.collection("c").unwrap();
    for t in 0..threads {
        for i in 0..per_thread {
            assert!(c.get(&format!("{t}-{i}")).unwrap().is_some(), "{t}-{i}");
        }
    }
    Arc::try_unwrap(db).unwrap().close().unwrap();
}

#[test]
fn a_read_only_database_shows_one_state_until_it_refreshes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = Db::open(&path).unwrap();
    let w = writer
        .create_collection_json(r#"{"name": "c", "fields": {}}"#)
        .unwrap();
    w.put(&Document::new("before")).unwrap();

    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    let r = reader.collection("c").unwrap();
    assert!(
        r.get("before").unwrap().is_some(),
        "a logged write, replayed"
    );

    w.put(&Document::new("logged")).unwrap();
    writer.commit().unwrap();
    w.delete("before").unwrap();
    writer
        .create_collection_json(r#"{"name": "later", "fields": {}}"#)
        .unwrap();
    // Unchanged until the refresh, from every handle and thread.
    let r2 = r.clone();
    let from_thread = std::thread::spawn(move || {
        (
            r2.get("logged").unwrap().is_some(),
            r2.get("before").unwrap().is_some(),
        )
    })
    .join()
    .unwrap();
    assert_eq!(from_thread, (false, true));
    assert_eq!(reader.collection_names().unwrap(), ["c"]);

    reader.refresh().unwrap();
    assert!(r.get("logged").unwrap().is_some());
    assert!(r.get("before").unwrap().is_none());
    assert_eq!(reader.collection_names().unwrap(), ["c", "later"]);
    // Nothing new: a refresh changes nothing, and is not an error.
    reader.refresh().unwrap();
    assert!(r.get("logged").unwrap().is_some());

    // On a read-write database a refresh does nothing: its reads are always the latest.
    writer.refresh().unwrap();
    writer.close().unwrap();
    reader.refresh().unwrap();
    assert!(r.get("logged").unwrap().is_some(), "the close's commit");
    reader.close().unwrap();
    assert_eq!(r.get("logged").unwrap_err().code(), ErrorCode::Closed);
}

#[test]
fn a_refresh_onto_another_database_is_a_conflict_and_keeps_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = Db::open(&path).unwrap();
    writer
        .create_collection_json(r#"{"name": "c", "fields": {}}"#)
        .unwrap()
        .put(&Document::new("old"))
        .unwrap();
    writer.close().unwrap();
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();

    // Another database takes the path: the reader refuses it.
    std::fs::remove_file(&path).unwrap();
    let other = Db::open(&path).unwrap();
    other.close().unwrap();
    let err = reader.refresh().unwrap_err();
    assert_eq!(err.code(), ErrorCode::Conflict, "{err}");
    let c = reader.collection("c").unwrap();
    assert!(c.get("old").unwrap().is_some(), "the state it had");
}

#[test]
fn concurrent_refreshes_around_commits_never_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = Db::open(&path).unwrap();
    let w = writer
        .create_collection_json(r#"{"name": "c", "fields": {}}"#)
        .unwrap();
    let reader = Arc::new(OpenOptions::new().read_only(true).open(&path).unwrap());
    let rounds = 20;
    let barrier = Arc::new(Barrier::new(3));
    let refreshers: Vec<_> = (0..2)
        .map(|_| {
            let (reader, barrier) = (Arc::clone(&reader), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..rounds {
                    // Without one refresh at a time, the later swap could find the other's newer state and refuse
                    // its own reopen as older (a false `Conflict`).
                    reader.refresh().unwrap();
                }
            })
        })
        .collect();
    barrier.wait();
    for i in 0..rounds {
        w.put(&Document::new(format!("d{i}"))).unwrap();
        writer.commit().unwrap();
    }
    for r in refreshers {
        r.join().unwrap();
    }
    reader.refresh().unwrap();
    let c = reader.collection("c").unwrap();
    assert!((0..rounds).all(|i| c.get(&format!("d{i}")).unwrap().is_some()));
}
