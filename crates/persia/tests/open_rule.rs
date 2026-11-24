//! The SQLite-like open rule, the same for every language (ROADMAP 10.1.9; SPEC §8.1, §13.1).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::path::Path;
use std::sync::{Arc, Barrier};

use persia::{Db, ErrorCode, OpenOptions};

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The container header page, which holds its `db_uuid`: equal headers mean the file was not replaced.
fn header(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap().get(..4096).unwrap().to_vec()
}

#[test]
fn a_missing_path_gets_its_parents_and_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a").join("b").join("c").join("app.persia");
    let db = Db::open(&path).unwrap();
    assert!(path.is_file());
    db.close().unwrap();
    // An existing file is opened, never replaced, also through options that would create it.
    let before = header(&path);
    OpenOptions::new()
        .create_if_missing(true)
        .open(&path)
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(header(&path), before);
}

#[test]
fn must_exist_and_read_only_create_nothing_on_a_missing_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing").join("deeper").join("app.persia");
    for options in [
        OpenOptions::new().create_if_missing(false),
        OpenOptions::new().read_only(true),
        // Read-only wins over creation: not an error of its own, and a missing path fails.
        OpenOptions::new().read_only(true).create_if_missing(true),
        OpenOptions::new().create_if_missing(false).read_only(true),
    ] {
        let err = options.open(&path).unwrap_err();
        assert_eq!(err.code(), ErrorCode::NotFound, "{options:?}: {err}");
        assert!(
            files_in(dir.path()).is_empty(),
            "{options:?} created something"
        );
    }
}

#[test]
fn a_parent_that_is_a_file_is_invalid_in_every_mode() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, b"a file, not a directory").unwrap();
    for path in [file.join("app.persia"), file.join("sub").join("app.persia")] {
        for options in [
            OpenOptions::new(),
            OpenOptions::new().create_if_missing(false),
            OpenOptions::new().read_only(true),
        ] {
            let err = options.open(&path).unwrap_err();
            assert_eq!(
                err.code(),
                ErrorCode::InvalidArgument,
                "{}: {err}",
                path.display()
            );
            assert!(err.message().contains("notes.txt"), "{err}");
        }
    }
    assert_eq!(files_in(dir.path()), ["notes.txt"]);
    assert_eq!(std::fs::read(&file).unwrap(), b"a file, not a directory");
}

#[test]
fn a_leftover_pack_refuses_the_create_as_a_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    std::fs::write(dir.path().join("app.persia.blobs"), b"somebody's pack").unwrap();
    let err = Db::open(&path).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Conflict, "{err}");
    assert!(err.message().contains("app.persia.blobs"), "{err}");
    assert_eq!(
        files_in(dir.path()),
        ["app.persia.blobs"],
        "nothing created"
    );
}

#[test]
fn racing_opens_of_a_missing_path_agree_on_one_database() {
    for round in 0..8 {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("deep").join("app.persia"));
        let threads = 6;
        let barrier = Arc::new(Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (path, barrier) = (Arc::clone(&path), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    let opened = Db::open(&*path);
                    let header = opened.as_ref().ok().map(|_| header(&path));
                    (opened.map(Db::close), header)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut opened = 0;
        for (result, _) in &results {
            match result {
                Ok(closed) => {
                    closed.as_ref().unwrap();
                    opened += 1;
                }
                Err(err) => assert_eq!(err.code(), ErrorCode::Conflict, "round {round}: {err}"),
            }
        }
        assert!(opened >= 1, "round {round}: one open created the database");
        // Every open that succeeded saw the same database: it was created once and never replaced.
        let headers: Vec<_> = results.iter().filter_map(|(_, h)| h.clone()).collect();
        assert!(headers.windows(2).all(|w| w[0] == w[1]), "round {round}");
        assert_eq!(header(&path), headers[0], "round {round}");
    }
}

#[cfg(unix)]
#[test]
fn a_create_that_fails_reports_its_own_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = Db::open(locked.join("app.persia"));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    match result {
        // Run as root, the directory's mode does not apply: nothing to check here.
        Ok(db) => db.close().unwrap(),
        // Not retried as a race (that would end as `Transient`): the file system's own failure.
        Err(err) => assert_eq!(err.code(), ErrorCode::Io, "{err}"),
    }
}
