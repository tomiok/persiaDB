//! `LocalStorage` beyond the shared conformance suite: the on-disk layout (SPEC §10.1.1, ADR-0012), path safety
//! (ROADMAP 2.3.4), and what other processes and earlier runs leave on disk.

// Test-only code: unwraps and indexing are the point.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Barrier;

use bytes::Bytes;
use persia_storage::{ErrorKind, Key, KeyDefect, KeyPrefix, LocalStorage, PutOutcome, Storage};

fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

fn store() -> (tempfile::TempDir, LocalStorage) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStorage::open(dir.path().join("store")).unwrap();
    (dir, store)
}

fn keys(s: &dyn Storage, prefix: &str) -> Vec<String> {
    s.list(&KeyPrefix::new(prefix).unwrap())
        .unwrap()
        .into_iter()
        .map(|m| m.key.as_str().to_owned())
        .collect()
}

/// Every file below `root`, as `/`-separated paths relative to it, sorted.
fn tree(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let rel = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &rel, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out.sort();
    out
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/local/v1")
}

// Golden: the files a store writes for these keys (ADR-0012). A change here breaks every existing store.
#[test]
fn layout_is_pinned() {
    let (_dir, s) = store();
    for k in [
        "a",
        "a/b",
        "manifests/00000000000000000001.mf",
        "Émile/CON",
        "x y/100%",
    ] {
        s.put(&key(k), Bytes::from_static(b"x")).unwrap();
    }
    assert_eq!(
        s.put_if_absent(&key("segments/c/1.seg"), Bytes::from_static(b"x"))
            .unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        tree(s.root()),
        [
            "%C3%89mile@/%43%4F%4E",
            "a",
            "a@/b",
            "manifests@/00000000000000000001.mf",
            "segments@/c@/1.seg",
            "x%20y@/100%25",
        ],
        "no temporary file is left behind either"
    );
}

// The committed fixture is a store written by an earlier build; every later build must read it the same way.
#[test]
fn reads_the_committed_v1_fixture() {
    let s = LocalStorage::open(fixture_root()).unwrap();
    // The temporary file, the non-canonical name `%62` and the foreign `README` are skipped.
    assert_eq!(
        keys(&s, ""),
        ["a", "a/b", "manifests/00000000000000000001.mf", "Émile/CON"]
    );
    for (k, body) in [
        ("a", &b"object a"[..]),
        ("a/b", b"object a/b"),
        ("manifests/00000000000000000001.mf", b"manifest 1"),
        ("Émile/CON", b"uppercase and non-ASCII"),
    ] {
        assert_eq!(&s.get(&key(k)).unwrap()[..], body, "{k}");
    }
    assert_eq!(s.get(&key("b")).unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(keys(&s, "a/"), ["a/b"]);
}

#[test]
fn objects_and_directories_with_one_name_coexist() {
    let (_dir, s) = store();
    s.put(&key("a/b"), Bytes::from_static(b"under")).unwrap();
    s.put(&key("a"), Bytes::from_static(b"object")).unwrap();
    s.put(&key("a/b/c"), Bytes::from_static(b"deeper")).unwrap();
    assert_eq!(keys(&s, ""), ["a", "a/b", "a/b/c"]);
    s.delete(&key("a/b")).unwrap();
    assert_eq!(keys(&s, ""), ["a", "a/b/c"]);
    assert_eq!(&s.get(&key("a/b/c")).unwrap()[..], b"deeper");
}

#[test]
fn keys_differing_in_case_or_normalization_stay_distinct() {
    let (_dir, s) = store();
    let ks = ["Key", "key", "KEY", "é", "e\u{301}", "ß", "ss"];
    for k in ks {
        s.put(&key(k), Bytes::from(k.to_owned())).unwrap();
    }
    for k in ks {
        assert_eq!(s.get(&key(k)).unwrap(), Bytes::from(k.to_owned()), "{k}");
    }
    let mut want: Vec<&str> = ks.to_vec();
    want.sort_unstable();
    assert_eq!(keys(&s, ""), want);
}

#[test]
fn data_survives_reopening() {
    let (dir, s) = store();
    s.put(&key("segments/c/1.seg"), Bytes::from_static(b"seg"))
        .unwrap();
    drop(s);
    let s = LocalStorage::open(dir.path().join("store")).unwrap();
    assert_eq!(&s.get(&key("segments/c/1.seg")).unwrap()[..], b"seg");
    assert_eq!(
        s.put_if_absent(&key("segments/c/1.seg"), Bytes::from_static(b"other"))
            .unwrap(),
        PutOutcome::AlreadyExists
    );
}

#[test]
fn open_creates_missing_parents_and_rejects_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("x/y/z");
    LocalStorage::open(&nested).unwrap();
    assert!(nested.is_dir());
    let file = dir.path().join("file");
    std::fs::write(&file, b"not a directory").unwrap();
    assert!(LocalStorage::open(&file).is_err());
}

// ROADMAP 2.3.4: keys that try to leave the root never reach the filesystem.
#[test]
fn traversal_attempts_are_invalid_input() {
    for text in [
        "../escape",
        "a/../../escape",
        "/etc/passwd",
        "a/./b",
        "a//b",
        "C:/Windows",
        "a\\..\\b",
        "..",
        "a/..",
        "nul\0byte",
    ] {
        let err = Key::new(text).unwrap_err();
        assert_eq!(
            persia_storage::Error::from(err).kind(),
            ErrorKind::InvalidInput,
            "{text:?}"
        );
    }
    let (dir, s) = store();
    // Segments that look like paths are encoded into one file name each.
    for k in ["...", "..x/y", ".hidden", "%2E%2E/x"] {
        s.put(&key(k), Bytes::from_static(b"in")).unwrap();
    }
    assert_eq!(
        tree(s.root()),
        ["%252%45%252%45@/x", "...", "..x@/y", ".hidden"]
    );
    // Nothing was written beside the root.
    let mut outside: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    outside.sort();
    assert_eq!(outside, ["store"]);
}

#[test]
fn prefixes_are_never_resolved_as_paths() {
    let (_dir, s) = store();
    s.put(&key("a/x"), Bytes::from_static(b"1")).unwrap();
    s.put(&key("b"), Bytes::from_static(b"2")).unwrap();
    for p in ["a/..", "a/.", "..", "."] {
        assert_eq!(keys(&s, p), Vec::<String>::new(), "{p:?}");
    }
}

#[test]
fn names_over_the_limit_are_invalid_input() {
    let (_dir, s) = store();
    let long_last = key(&"x".repeat(256));
    let long_dir = key(&format!("{}/x", "x".repeat(255)));
    let escaped = key(&"X".repeat(86)); // 258 bytes once escaped
    for k in [&long_last, &long_dir, &escaped] {
        let err = s.put(k, Bytes::new()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
        assert!(
            matches!(
                &err,
                persia_storage::Error::InvalidKey(e) if matches!(e.defect, KeyDefect::NameTooLong { .. })
            ),
            "{err}"
        );
        for kind in [
            s.get(k).unwrap_err().kind(),
            s.get_range(k, 0..0).unwrap_err().kind(),
            s.put_if_absent(k, Bytes::new()).unwrap_err().kind(),
            s.delete(k).unwrap_err().kind(),
        ] {
            assert_eq!(kind, ErrorKind::InvalidInput);
        }
    }
    // A prefix through a directory that cannot exist matches nothing.
    assert_eq!(
        keys(&s, &format!("{}/", "x".repeat(255))),
        Vec::<String>::new()
    );
}

#[cfg(windows)]
#[test]
fn windows_reserved_names_are_invalid_input() {
    let (_dir, s) = store();
    for k in ["con", "nul.txt", "a/com1", "trailing."] {
        assert_eq!(
            s.put(&key(k), Bytes::new()).unwrap_err().kind(),
            ErrorKind::InvalidInput,
            "{k}"
        );
    }
    // Uppercase is escaped, so `CON` is an ordinary file name.
    s.put(&key("CON"), Bytes::from_static(b"ok")).unwrap();
}

#[test]
fn foreign_entries_are_skipped() {
    let (_dir, s) = store();
    s.put(&key("d/obj"), Bytes::from_static(b"1")).unwrap();
    let d = s.root().join("d@");
    std::fs::write(d.join("@tmp.1.1"), b"stale temporary file").unwrap();
    std::fs::write(d.join("Upper"), b"not canonical").unwrap();
    std::fs::write(d.join("%6F"), b"escaped plain byte").unwrap();
    std::fs::create_dir(d.join("nomarker")).unwrap();
    std::fs::write(d.join("nomarker/x"), b"directory without marker").unwrap();
    std::fs::write(d.join("file@"), b"a file named like a directory").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(s.root().join("d@/obj"), d.join("link")).unwrap();
    assert_eq!(keys(&s, ""), ["d/obj"]);
    assert_eq!(keys(&s, "d/"), ["d/obj"]);
}

// Two stores on one root, as two processes would have: the link is the only arbiter.
#[test]
fn put_if_absent_has_one_winner_across_store_instances() {
    const WRITERS: usize = 6;
    let dir = tempfile::tempdir().unwrap();
    let stores: Vec<LocalStorage> = (0..WRITERS)
        .map(|_| LocalStorage::open(dir.path()).unwrap())
        .collect();
    for round in 0..10 {
        let k = key(&format!("manifests/{round:020}.mf"));
        let barrier = Barrier::new(WRITERS);
        let outcomes: Vec<PutOutcome> = std::thread::scope(|scope| {
            let handles: Vec<_> = stores
                .iter()
                .enumerate()
                .map(|(w, s)| {
                    let (k, barrier) = (&k, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        s.put_if_absent(k, Bytes::from(format!("writer {w}")))
                            .unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners: Vec<usize> = (0..WRITERS)
            .filter(|&w| outcomes[w] == PutOutcome::Created)
            .collect();
        assert_eq!(winners.len(), 1, "round {round}: {outcomes:?}");
        assert_eq!(
            stores[0].get(&k).unwrap(),
            Bytes::from(format!("writer {}", winners[0]))
        );
    }
    assert!(
        tree(dir.path()).iter().all(|p| !p.contains("@tmp")),
        "temporary files are removed"
    );
}

#[test]
fn a_range_past_the_end_reads_nothing() {
    let (_dir, s) = store();
    s.put(&key("k"), Bytes::from_static(b"0123456789")).unwrap();
    // Checked against the file length before any allocation: a corrupt offset cannot ask for u64::MAX bytes.
    let err = s.get_range(&key("k"), 4..u64::MAX).unwrap_err();
    assert!(
        matches!(err, persia_storage::Error::ShortRead { returned: 6, .. }),
        "{err}"
    );
}

// A directory removed behind the store's back (an operator cleaning up) is recreated, not a permanent `Io` error.
#[test]
fn a_directory_removed_externally_is_recreated() {
    let (_dir, s) = store();
    s.put(&key("segments/c/1.seg"), Bytes::from_static(b"1"))
        .unwrap();
    std::fs::remove_dir_all(s.root().join("segments@")).unwrap();
    s.put(&key("segments/c/2.seg"), Bytes::from_static(b"2"))
        .unwrap();
    assert_eq!(keys(&s, "segments/"), ["segments/c/2.seg"]);
}

// An unmounted or removed root must not look like an empty store ("no manifests").
#[test]
fn listing_a_missing_root_is_an_error() {
    let (_dir, s) = store();
    std::fs::remove_dir(s.root()).unwrap();
    let err = s.list(&KeyPrefix::default()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Io, "{err}");
}

/// An object whose path, or its temporary file's, would exceed `PATH_MAX` is `InvalidInput` (`PathTooLong`) before
/// anything is written; a path of exactly the limit works (SPEC §10.1.1).
#[cfg(unix)]
#[test]
fn paths_over_the_platform_limit_are_invalid_input() {
    let max = if cfg!(target_vendor = "apple") {
        1023
    } else {
        4095
    };
    // Canonical: on macOS `/var` is a symbolic link, and the kernel counts the expanded path against `PATH_MAX`.
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    // A root of exactly `max - 256` bytes, so `<root>/<255-byte name>` is exactly `max` bytes.
    let target = max - 256;
    let mut root = base;
    while root.as_os_str().len() < target {
        let left = target - root.as_os_str().len() - 1;
        root.push("d".repeat(left.clamp(1, 200)));
    }
    assert_eq!(root.as_os_str().len(), target);
    let s = LocalStorage::open(&root).unwrap();
    let fits = key(&"x".repeat(255));
    s.put(&fits, Bytes::from_static(b"ok")).unwrap();
    assert_eq!(&s.get(&fits).unwrap()[..], b"ok");
    // `<root>/a@/<255 bytes>`: four bytes over.
    let long = key(&format!("a/{}", "x".repeat(255)));
    let err = s.put(&long, Bytes::new()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
    assert!(
        matches!(&err, persia_storage::Error::InvalidKey(e)
            if e.defect == (KeyDefect::PathTooLong { len: max + 3, max })),
        "{err}"
    );
    assert_eq!(s.get(&long).unwrap_err().kind(), ErrorKind::InvalidInput);
    assert_eq!(
        tree(&root),
        ["x".repeat(255)],
        "nothing was written for the long key"
    );
}
