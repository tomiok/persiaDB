//! The rest of the embedded API as async (ROADMAP 10.1.26; ADR-0064, amended 2026-11-27): collection management,
//! blobs, `refresh`, `backup` and the JSON and typed calls on the database's own pool, each driven by the `block_on`
//! of the tests' support (no runtime is a dev-dependency, ROADMAP 0.6.11), and what dropping each one's future does:
//! a queued job is skipped and leaves nothing, a started change runs to its end with its result discarded, a started
//! read has its token set.
//!
//! The pool has one thread, held where a job starts by the pool's test hook, so which job is queued and which runs is
//! decided by the test, not by timing. Each test ends by closing the database, which joins the pool, and opening the
//! file read-write again (the close released it); an owned blob reader holds a share of a counter, so a test sees that
//! no job kept one. No test sleeps.
#![cfg(feature = "async")]
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines // each test is one scenario over every call
)] // Tests: inputs are fixed.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use persia::{
    AsyncDb, BlobRef, Db, Document, ErrorCode, FieldKind, JobEvent, JobKind, OpenOptions,
    PutBlobOptions, SchemaBuilder, TextOptions,
};
use serde::{Deserialize, Serialize};

#[path = "support/asyncs.rs"]
mod asyncs;

use asyncs::{Probe, block_on, open, seeded};

/// The bytes the blob calls store.
const BYTES: &[u8] = b"the bytes of an async blob";

/// The reference [`BYTES`] gets with the default options. Blobs are content-addressed, so a scratch database gives it
/// without storing anything in the database under test.
fn reference_of_bytes(dir: &Path) -> BlobRef {
    let scratch = Db::open(dir.join("scratch.persia")).unwrap();
    let blob = scratch.put_blob(BYTES, &PutBlobOptions::new()).unwrap();
    scratch.close().unwrap();
    blob
}

/// An owned reader of [`BYTES`] that holds a share of a counter while it lives.
struct Tracked {
    bytes: &'static [u8],
    _alive: Arc<()>,
}

impl Tracked {
    fn new(alive: &Arc<()>) -> Self {
        Self {
            bytes: BYTES,
            _alive: Arc::clone(alive),
        }
    }
}

impl Read for Tracked {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.bytes.read(out)
    }
}

/// A typed document of the seeded `notes` collection.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Note {
    id: String,
    body: String,
}

fn note(id: &str, body: &str) -> Note {
    Note {
        id: id.to_owned(),
        body: body.to_owned(),
    }
}

/// The kind of the next job that ran to its end, and its error's code.
fn ran(probe: &Probe) -> (JobKind, Option<ErrorCode>) {
    let JobEvent::Started { kind, .. } = probe.next() else {
        panic!("a job starts");
    };
    let JobEvent::Finished {
        kind: finished,
        error,
        delivered: true,
    } = probe.next()
    else {
        panic!("the job {kind:?} finishes and its future takes the result");
    };
    assert_eq!(kind, finished);
    (kind, error)
}

/// A future of any output: what the loops hold and drop.
type AnyFuture = Box<dyn std::any::Any>;

/// What queues one job.
type Submit<'a> = Box<dyn Fn() -> AnyFuture + 'a>;

/// The read-write open of `path` that a close must allow: the pool joined, the file released.
fn released(path: &Path) {
    Db::open(path).unwrap().close().unwrap();
}

#[test]
fn every_new_call_runs_on_the_pool_as_its_sync_form_does() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let (probe, options) = Probe::new();
    let db = block_on(AsyncDb::open_with(options, &path)).unwrap();

    let schema = SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let notes = block_on(db.create_collection(schema)).unwrap();
    assert_eq!(ran(&probe), (JobKind::CreateCollection, None));
    let files = block_on(db.create_collection_json(r#"{"name": "files", "fields": {}}"#)).unwrap();
    assert_eq!(ran(&probe), (JobKind::CreateCollection, None));
    let err =
        block_on(db.create_collection_json(r#"{"name": "files", "fields": {}}"#)).unwrap_err();
    assert_eq!(err.code(), ErrorCode::AlreadyExists);
    assert_eq!(
        ran(&probe),
        (JobKind::CreateCollection, Some(ErrorCode::AlreadyExists))
    );
    // A lookup of the published state, on the caller's thread: no job.
    assert_eq!(db.collection_names().unwrap(), ["files", "notes"]);
    assert!(probe.quiet(), "collection_names queued no job");

    block_on(notes.put_json(r#"{"id": "n1", "body": "hello async json"}"#)).unwrap();
    assert_eq!(ran(&probe), (JobKind::PutJson, None));
    assert_eq!(
        block_on(notes.get_json("n1")).unwrap().as_deref(),
        Some(r#"{"id":"n1","body":"hello async json"}"#)
    );
    assert_eq!(ran(&probe), (JobKind::GetJson, None));
    block_on(notes.put_struct(&note("n2", "typed words"))).unwrap();
    assert_eq!(ran(&probe), (JobKind::PutJson, None));
    assert_eq!(
        block_on(notes.get_struct::<Note>("n2")).unwrap(),
        Some(note("n2", "typed words"))
    );
    assert_eq!(ran(&probe), (JobKind::GetJson, None));
    assert_eq!(block_on(notes.get_struct::<Note>("missing")).unwrap(), None);
    assert_eq!(ran(&probe), (JobKind::GetJson, None));
    let response = block_on(notes.search_json(r#"{"q": "typed", "relax": false}"#)).unwrap();
    assert_eq!(response.hits.len(), 1);
    assert_eq!(response.hits[0].document.id(), "n2");
    assert_eq!(ran(&probe), (JobKind::SearchJson, None));
    let err = block_on(notes.search_json(r#"{"query": "typed"}"#)).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument);
    assert_eq!(
        ran(&probe),
        (JobKind::SearchJson, Some(ErrorCode::InvalidArgument))
    );
    // A value that cannot be written as JSON (a key that is not a string) is refused on the caller's thread.
    let unwritable = BTreeMap::from([((1, 2), 3)]);
    let err = block_on(notes.put_struct(&unwritable)).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument);
    assert!(
        probe.quiet(),
        "a value that cannot be written queued no job"
    );

    let blob = block_on(db.put_blob(BYTES, PutBlobOptions::new().mime("text/plain"))).unwrap();
    assert_eq!((blob.size(), blob.mime()), (26, "text/plain"));
    assert_eq!(ran(&probe), (JobKind::PutBlob, None));
    assert_eq!(
        block_on(db.get_blob_range(blob.clone(), 4..9)).unwrap(),
        b"bytes"
    );
    assert_eq!(ran(&probe), (JobKind::GetBlobRange, None));
    let err = block_on(db.get_blob_range(blob.clone(), 0..1_000)).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument);
    assert_eq!(
        ran(&probe),
        (JobKind::GetBlobRange, Some(ErrorCode::InvalidArgument))
    );
    let attached =
        block_on(files.put_with_blob(Document::new("f1"), BYTES, PutBlobOptions::new())).unwrap();
    assert_eq!(ran(&probe), (JobKind::PutWithBlob, None));
    let f1 = block_on(files.get("f1")).unwrap().unwrap();
    assert_eq!(f1.blob_ref(), Some(&attached));
    assert_eq!(ran(&probe), (JobKind::Get, None));
    // A blob already stored goes with a document through `put`, as its reference.
    let mut f2 = Document::new("f2");
    f2.set_blob_ref(Some(blob.clone()));
    block_on(files.put(f2)).unwrap();
    assert_eq!(ran(&probe), (JobKind::Put, None));
    let f2 = block_on(files.get("f2")).unwrap().unwrap();
    assert_eq!(f2.blob_ref(), Some(&blob));
    assert_eq!(ran(&probe), (JobKind::Get, None));

    let dest = dir.path().join("backup.persia");
    block_on(db.backup(&dest)).unwrap();
    assert_eq!(ran(&probe), (JobKind::Backup, None));
    let err = block_on(db.backup(&dest)).unwrap_err();
    assert_eq!(err.code(), ErrorCode::AlreadyExists);
    assert_eq!(
        ran(&probe),
        (JobKind::Backup, Some(ErrorCode::AlreadyExists))
    );
    // A read-write database has nothing to refresh.
    block_on(db.refresh()).unwrap();
    assert_eq!(ran(&probe), (JobKind::Refresh, None));

    block_on(db.drop_collection("files")).unwrap();
    assert_eq!(ran(&probe), (JobKind::DropCollection, None));
    assert_eq!(db.collection_names().unwrap(), ["notes"]);
    assert_eq!(
        block_on(files.get("f1")).unwrap_err().code(),
        ErrorCode::NotFound
    );
    let err = block_on(db.drop_collection("files")).unwrap_err();
    assert_eq!(err.code(), ErrorCode::NotFound);

    block_on(db.close()).unwrap();
    released(&path);
    // The backup holds what was there when it ran, the dropped collection and its blob included.
    let copy = OpenOptions::new().read_only(true).open(&dest).unwrap();
    let notes = copy.collection("notes").unwrap();
    assert!(notes.get("n1").unwrap().is_some() && notes.get("n2").unwrap().is_some());
    let f1 = copy
        .collection("files")
        .unwrap()
        .get("f1")
        .unwrap()
        .unwrap();
    assert_eq!(
        copy.get_blob_range(f1.blob_ref().unwrap(), 0..26).unwrap(),
        BYTES
    );
}

#[test]
fn jobs_dropped_before_they_start_are_skipped_and_leave_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let expected = reference_of_bytes(dir.path());
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);
    let alive = Arc::new(());
    let dest = dir.path().join("backup.persia");

    probe.hold_next();
    let holder = notes.get("seed");
    assert!(matches!(
        probe.next(),
        JobEvent::Started {
            kind: JobKind::Get,
            ..
        }
    ));
    // The pool's one thread is held in `holder`: every job below is queued behind it, then its future dropped.
    let typed = SchemaBuilder::new("typed").build().unwrap();
    let queued: Vec<(JobKind, AnyFuture)> = vec![
        (
            JobKind::CreateCollection,
            Box::new(db.create_collection_json(r#"{"name": "made", "fields": {}}"#)),
        ),
        (
            JobKind::CreateCollection,
            Box::new(db.create_collection(typed)),
        ),
        (
            JobKind::DropCollection,
            Box::new(db.drop_collection("notes")),
        ),
        (JobKind::PutJson, Box::new(notes.put_json(r#"{"id": "j"}"#))),
        (
            JobKind::PutJson,
            Box::new(notes.put_struct(&note("s", "typed"))),
        ),
        (
            JobKind::PutBlob,
            Box::new(db.put_blob(Tracked::new(&alive), PutBlobOptions::new())),
        ),
        (
            JobKind::PutWithBlob,
            Box::new(notes.put_with_blob(
                Document::new("w"),
                Tracked::new(&alive),
                PutBlobOptions::new(),
            )),
        ),
        (JobKind::Backup, Box::new(db.backup(&dest))),
        (JobKind::Refresh, Box::new(db.refresh())),
        (JobKind::GetJson, Box::new(notes.get_json("seed"))),
        (JobKind::GetJson, Box::new(notes.get_struct::<Note>("seed"))),
        (
            JobKind::SearchJson,
            Box::new(notes.search_json(r#"{"q": "fox"}"#)),
        ),
        (
            JobKind::GetBlobRange,
            Box::new(db.get_blob_range(expected.clone(), 0..1)),
        ),
    ];
    let kinds: Vec<JobKind> = queued.iter().map(|(kind, _)| *kind).collect();
    drop(queued);
    probe.release();
    assert!(matches!(
        probe.next(),
        JobEvent::Finished {
            kind: JobKind::Get,
            error: None,
            delivered: true
        }
    ));
    for kind in kinds {
        let skipped = probe.next();
        assert!(
            matches!(&skipped, JobEvent::Skipped { kind: k } if *k == kind),
            "{kind:?}: {skipped:?}"
        );
    }
    assert!(block_on(holder).unwrap().is_some());

    // None of them left anything.
    assert_eq!(db.collection_names().unwrap(), ["notes"]);
    for id in ["j", "s", "w"] {
        assert!(block_on(notes.get(id)).unwrap().is_none(), "{id}");
    }
    assert_eq!(
        block_on(db.get_blob_range(expected, 0..1))
            .unwrap_err()
            .code(),
        ErrorCode::NotFound,
        "no blob was stored"
    );
    assert!(!dest.exists() && !persia_blob::pack_path(&dest).exists());
    // The database still works, and closes cleanly.
    block_on(notes.put_json(r#"{"id": "after"}"#)).unwrap();
    block_on(db.close()).unwrap();
    assert_eq!(
        Arc::strong_count(&alive),
        1,
        "the skipped jobs dropped their readers"
    );
    released(&path);
    assert!(asyncs::has(&path, "after"));
}

#[test]
fn a_started_change_runs_to_its_end_and_its_result_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let expected = reference_of_bytes(dir.path());
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);
    block_on(db.create_collection_json(r#"{"name": "gone", "fields": {}}"#)).unwrap();
    assert_eq!(ran(&probe), (JobKind::CreateCollection, None));
    let alive = Arc::new(());
    let dest = dir.path().join("backup.persia");

    let changes: Vec<(JobKind, Submit<'_>)> = vec![
        (
            JobKind::CreateCollection,
            Box::new(|| Box::new(db.create_collection_json(r#"{"name": "made", "fields": {}}"#))),
        ),
        (
            JobKind::CreateCollection,
            Box::new(|| {
                Box::new(db.create_collection(SchemaBuilder::new("typed").build().unwrap()))
            }),
        ),
        (
            JobKind::DropCollection,
            Box::new(|| Box::new(db.drop_collection("gone"))),
        ),
        (
            JobKind::PutJson,
            Box::new(|| Box::new(notes.put_json(r#"{"id": "j"}"#))),
        ),
        (
            JobKind::PutJson,
            Box::new(|| Box::new(notes.put_struct(&note("s", "typed")))),
        ),
        (
            JobKind::PutBlob,
            Box::new(|| Box::new(db.put_blob(Tracked::new(&alive), PutBlobOptions::new()))),
        ),
        (
            JobKind::PutWithBlob,
            Box::new(|| {
                Box::new(notes.put_with_blob(
                    Document::new("w"),
                    Tracked::new(&alive),
                    PutBlobOptions::new(),
                ))
            }),
        ),
        (JobKind::Backup, Box::new(|| Box::new(db.backup(&dest)))),
        (JobKind::Refresh, Box::new(|| Box::new(db.refresh()))),
    ];
    for (kind, submit) in changes {
        probe.hold_next();
        let future = submit();
        let started = probe.next();
        assert!(
            matches!(&started, JobEvent::Started { kind: k, token: None } if *k == kind),
            "{started:?}"
        );
        drop(future);
        probe.release();
        let finished = probe.next();
        assert!(
            matches!(&finished, JobEvent::Finished { kind: k, error: None, delivered: false } if *k == kind),
            "{finished:?}"
        );
    }

    // Each change happened whole.
    assert_eq!(db.collection_names().unwrap(), ["made", "notes", "typed"]);
    for id in ["j", "s"] {
        assert!(block_on(notes.get(id)).unwrap().is_some(), "{id}");
    }
    let w = block_on(notes.get("w")).unwrap().unwrap();
    assert_eq!(w.blob_ref(), Some(&expected));
    assert_eq!(block_on(db.get_blob_range(expected, 0..26)).unwrap(), BYTES);
    block_on(db.close()).unwrap();
    assert_eq!(
        Arc::strong_count(&alive),
        1,
        "the uploads dropped their readers"
    );
    released(&path);
    let copy = OpenOptions::new().read_only(true).open(&dest).unwrap();
    let w = copy.collection("notes").unwrap().get("w").unwrap().unwrap();
    assert_eq!(
        copy.get_blob_range(w.blob_ref().unwrap(), 0..26).unwrap(),
        BYTES,
        "the backup is whole"
    );
}

#[test]
fn a_started_read_has_its_token_set_when_its_future_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);
    let blob = block_on(db.put_blob(BYTES, PutBlobOptions::new())).unwrap();
    assert_eq!(ran(&probe), (JobKind::PutBlob, None));

    let reads: Vec<(JobKind, Submit<'_>)> = vec![
        (
            JobKind::GetJson,
            Box::new(|| Box::new(notes.get_json("seed"))),
        ),
        (
            JobKind::GetJson,
            Box::new(|| Box::new(notes.get_struct::<Note>("seed"))),
        ),
        (
            JobKind::SearchJson,
            Box::new(|| Box::new(notes.search_json(r#"{"q": "fox"}"#))),
        ),
        (
            JobKind::GetBlobRange,
            Box::new(|| Box::new(db.get_blob_range(blob.clone(), 0..1))),
        ),
    ];
    for (kind, submit) in reads {
        probe.hold_next();
        let future = submit();
        let JobEvent::Started {
            kind: started,
            token: Some(token),
        } = probe.next()
        else {
            panic!("a read starts with a token");
        };
        assert_eq!(started, kind);
        assert!(!token.is_cancelled());
        drop(future);
        assert!(token.is_cancelled(), "dropping the future set the token");
        probe.release();
        let finished = probe.next();
        assert!(
            matches!(
                &finished,
                JobEvent::Finished { kind: k, error: Some(ErrorCode::Cancelled), delivered: false } if *k == kind
            ),
            "the read saw the token: {finished:?}"
        );
    }
    // The database still works.
    assert!(block_on(notes.get_json("seed")).unwrap().is_some());
    block_on(db.close()).unwrap();
    released(&path);
}

#[test]
fn jobs_still_queued_at_close_complete_with_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let (probe, options) = Probe::new();
    let (db, notes) = open(&path, options);
    let alive = Arc::new(());
    let dest = dir.path().join("backup.persia");

    probe.hold_next();
    let running = notes.put_json(r#"{"id": "running"}"#);
    assert!(matches!(probe.next(), JobEvent::Started { .. }));
    let backup = db.backup(&dest);
    let upload = db.put_blob(Tracked::new(&alive), PutBlobOptions::new());
    let created = db.create_collection_json(r#"{"name": "made", "fields": {}}"#);
    let search = notes.search_json(r#"{"q": "fox"}"#);
    let closing = db.close();
    // Completed by the close itself, while the running job is still held.
    assert_eq!(block_on(backup).unwrap_err().code(), ErrorCode::Closed);
    assert_eq!(block_on(upload).unwrap_err().code(), ErrorCode::Closed);
    assert_eq!(block_on(created).unwrap_err().code(), ErrorCode::Closed);
    assert_eq!(block_on(search).unwrap_err().code(), ErrorCode::Closed);
    // A call after the close is refused at once, and drops what it was given.
    let late = notes.put_with_blob(
        Document::new("late"),
        Tracked::new(&alive),
        PutBlobOptions::new(),
    );
    assert_eq!(block_on(late).unwrap_err().code(), ErrorCode::Closed);
    assert_eq!(
        block_on(notes.get_struct::<Note>("seed"))
            .unwrap_err()
            .code(),
        ErrorCode::Closed
    );
    probe.release();
    block_on(closing).unwrap();
    block_on(running).unwrap();
    assert_eq!(Arc::strong_count(&alive), 1);
    assert!(!dest.exists());
    let reopened = Db::open(&path).unwrap();
    assert_eq!(reopened.collection_names().unwrap(), ["notes"]);
    assert!(
        reopened
            .collection("notes")
            .unwrap()
            .get("running")
            .unwrap()
            .is_some()
    );
    reopened.close().unwrap();
}

/// `refresh` on a read-only database: a queued one dropped is skipped (the reader keeps its state), a started one
/// dropped runs to its end (the reader shows the new state), and one awaited shows what the writer made durable.
#[test]
fn a_read_only_database_refreshes_on_the_pool() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    seeded(&path);
    let writer = Db::open(&path).unwrap();
    let (probe, options) = Probe::new();
    let (reader, notes) = open(&path, options.read_only(true));

    writer
        .create_collection_json(r#"{"name": "first", "fields": {}}"#)
        .unwrap();
    probe.hold_next();
    let holder = notes.get("seed");
    assert!(matches!(probe.next(), JobEvent::Started { .. }));
    drop(reader.refresh());
    probe.release();
    assert!(matches!(probe.next(), JobEvent::Finished { .. }));
    assert!(matches!(
        probe.next(),
        JobEvent::Skipped {
            kind: JobKind::Refresh
        }
    ));
    assert!(block_on(holder).unwrap().is_some());
    assert_eq!(
        reader.collection("first").unwrap_err().code(),
        ErrorCode::NotFound
    );

    probe.hold_next();
    let refresh = reader.refresh();
    assert!(matches!(
        probe.next(),
        JobEvent::Started {
            kind: JobKind::Refresh,
            token: None
        }
    ));
    drop(refresh);
    probe.release();
    assert!(matches!(
        probe.next(),
        JobEvent::Finished {
            kind: JobKind::Refresh,
            error: None,
            delivered: false
        }
    ));
    assert!(
        reader.collection("first").is_ok(),
        "the dropped refresh ran"
    );

    writer
        .create_collection_json(r#"{"name": "second", "fields": {}}"#)
        .unwrap();
    block_on(reader.refresh()).unwrap();
    assert_eq!(
        reader.collection_names().unwrap(),
        ["first", "notes", "second"]
    );
    block_on(reader.close()).unwrap();
    writer.close().unwrap();
    released(&path);
}
