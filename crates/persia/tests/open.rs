//! `Db::open`, `OpenOptions` and `Db::close` (ROADMAP 10.1.1; SPEC §8.1, §10.2, §14).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::path::Path;

use persia::{Db, ErrorCode, OpenOptions};

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn open_creates_the_database_and_reopens_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let db = Db::open(&path).unwrap();
    assert!(path.is_file());
    assert_eq!(db.path(), path);
    assert!(!db.is_read_only());
    let debug = format!("{db:?}");
    assert!(
        debug.contains("app.persia") && debug.contains("read_only: false"),
        "{debug}"
    );
    db.close().unwrap();
    let created = std::fs::read(&path).unwrap();

    // An existing file is opened, never replaced: it still starts with the same header.
    let db = Db::open(&path).unwrap();
    db.close().unwrap();
    let reopened = std::fs::read(&path).unwrap();
    assert_eq!(
        created.get(..4096),
        reopened.get(..4096),
        "the header page, with its db_uuid"
    );
}

#[test]
fn close_and_drop_release_the_writer_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    for _ in 0..3 {
        let db = Db::open(&path).unwrap();
        db.close().unwrap();
    }
    for _ in 0..3 {
        let db = Db::open(&path).unwrap();
        drop(db);
    }
    Db::open(&path).unwrap().close().unwrap();
}

#[cfg(unix)]
#[test]
fn a_file_url_opens_the_file_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("my data");
    std::fs::create_dir(&sub).unwrap();
    let path = sub.join("app #1.persia");
    let text = path.to_str().unwrap();
    let encoded = text.replace(' ', "%20").replace('#', "%23");

    let db = Db::open(format!("file://{encoded}")).unwrap();
    assert_eq!(db.path(), path, "the decoded path");
    db.close().unwrap();
    assert!(path.is_file());
    for url in [
        format!("file://localhost{encoded}"),
        format!("FILE://{encoded}"),
    ] {
        let db = OpenOptions::new()
            .create_if_missing(false)
            .open(&url)
            .unwrap();
        assert_eq!(db.path(), path, "{url}");
        db.close().unwrap();
    }
    // The same text without a scheme is a path, taken as it is: no such file.
    let literal = OpenOptions::new()
        .create_if_missing(false)
        .open(&encoded)
        .unwrap_err();
    assert_eq!(literal.code(), ErrorCode::NotFound);
}

#[test]
fn schemes_without_a_backend_are_unsupported_and_others_invalid() {
    for url in [
        "s3://bucket/app",
        "gs://bucket/app",
        "az://account/container/app",
        "memory://",
        "http://example.com/app.persia",
        "sqlite://app.db",
    ] {
        let err = Db::open(url).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Unsupported, "{url}: {err}");
        let read_only = OpenOptions::new().read_only(true).open(url).unwrap_err();
        assert_eq!(read_only.code(), ErrorCode::Unsupported, "{url}");
    }
    for url in ["file://example.com/app.persia", "file:///a%zz", ""] {
        let err = Db::open(url).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{url:?}: {err}");
    }
}

#[test]
fn create_if_missing_false_requires_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let options = OpenOptions::new().create_if_missing(false);
    let err = options.open(&path).unwrap_err();
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert!(err.message().contains("app.persia"), "{err}");
    assert!(files_in(dir.path()).is_empty(), "nothing was created");

    Db::open(&path).unwrap().close().unwrap();
    options.open(&path).unwrap().close().unwrap();
}

#[test]
fn a_read_only_open_needs_the_file_and_writes_nothing_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let options = OpenOptions::new().read_only(true);
    let err = options.open(&path).unwrap_err();
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert!(files_in(dir.path()).is_empty(), "nothing was created");

    Db::open(&path).unwrap().close().unwrap();
    let before = std::fs::read(&path).unwrap();
    let reader = options.open(&path).unwrap();
    assert!(reader.is_read_only());
    assert!(format!("{reader:?}").contains("read_only: true"));
    // Beside a writer, and beside another reader.
    let writer = Db::open(&path).unwrap();
    let second = options.open(&path).unwrap();
    second.close().unwrap();
    writer.close().unwrap();
    reader.close().unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "no open here wrote to the container"
    );
}

#[test]
fn a_file_that_is_not_a_database_is_corrupt_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    let text = b"not a database, but somebody's notes".repeat(500);
    std::fs::write(&path, &text).unwrap();
    for options in [OpenOptions::new(), OpenOptions::new().read_only(true)] {
        let err = options.open(&path).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Corrupt, "{err}");
        assert!(err.message().contains("notes.txt"), "{err}");
        assert!(std::error::Error::source(&err).is_some());
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        text,
        "the file is not ours to change"
    );
}

#[test]
fn options_are_reusable_and_cloneable() {
    let dir = tempfile::tempdir().unwrap();
    let options = OpenOptions::new().merge_threads(0).memtable_bytes(1 << 20);
    let copy = options.clone();
    let a = options.open(dir.path().join("a.persia")).unwrap();
    let b = copy.open(dir.path().join("b.persia")).unwrap();
    a.close().unwrap();
    b.close().unwrap();
    assert!(format!("{options:?}").contains("merge_threads: 0"));
}

#[test]
fn a_memtable_of_zero_bytes_is_refused_before_anything_is_created() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("app.persia");
    for options in [
        OpenOptions::new().memtable_bytes(0),
        OpenOptions::new().read_only(true).memtable_bytes(0),
    ] {
        let err = options.open(&path).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
        assert!(err.message().starts_with("memtable_bytes"), "{err}");
    }
    assert!(files_in(dir.path()).is_empty(), "nothing was created");
    // The smallest size opens, and so do zero durations (tests/maintenance.rs: every round commits) and
    // `merge_threads(0)` (merges on the maintenance thread).
    OpenOptions::new()
        .memtable_bytes(1)
        .idle_flush(Duration::ZERO)
        .commit_interval(Duration::ZERO)
        .merge_threads(0)
        .open(&path)
        .unwrap()
        .close()
        .unwrap();
}
