//! `LocalStorage` under concurrency, failing filesystems and hostile trees (SPEC §10.1, §10.1.1, ADR-0012).
//!
//! Invariants checked here:
//! 1. `put` is atomic: readers of the same root, through this or another store instance, see one whole version.
//! 2. No `@tmp.*` file survives a `put`/`put_if_absent`, whether it succeeded or failed.
//! 3. `put_if_absent` whose collided object vanishes before it is read retries; it never misreports the race.
//! 4. A failing filesystem (read-only directory, root replaced by a file, foreign entries where the layout expects
//!    another type) yields `ErrorKind::Io`, never a panic, and leaves the previous object intact.
//! 5. `list` over a hostile tree (deep nesting, non-UTF-8 names, symlink loops, names decoding to invalid keys)
//!    terminates, never panics, and returns only valid keys, sorted.
//!
//! Durability across a machine crash is ROADMAP 2.4.11 and is not simulated here.

// Test-only code: unwraps and indexing are the point.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::string_slice)]

use std::path::Path;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use persia_storage::{Error, ErrorKind, Key, KeyPrefix, LocalStorage, PutOutcome, Storage};

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

/// Every temporary file below `dir`, at any depth; names need not be UTF-8 and symlinks are not followed.
fn temp_files(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("@tmp") {
                out.push(entry.path().display().to_string());
            }
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

fn assert_no_temp_files(dir: &Path) {
    let left = temp_files(dir);
    assert!(left.is_empty(), "temporary files left behind: {left:?}");
}

fn io_source(err: &Error) -> Option<&std::io::Error> {
    match err {
        Error::Io { source, .. } => Some(source),
        _ => None,
    }
}

/// Sets the flag when dropped, also when the thread holding it panics, so the threads polling it stop and the
/// failure is reported instead of hanging the test.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Atomic `put`, seen from a second store instance (another process would hold its own instance).
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn readers_of_another_instance_never_see_a_torn_put() {
    const ROUNDS: usize = 200;
    let dir = tempfile::tempdir().unwrap();
    let writer = LocalStorage::open(dir.path()).unwrap();
    let reader = LocalStorage::open(dir.path()).unwrap();
    let k = key("segments/c/1.seg");
    // Different lengths and contents: a torn read shows up as a wrong length or a mixed byte.
    let versions = [
        Bytes::from(vec![0xA1u8; 64 * 1024]),
        Bytes::from(vec![0xB2u8; 1000]),
        Bytes::from((0..=255u8).cycle().take(3000).collect::<Vec<u8>>()),
    ];
    writer.put(&k, versions[0].clone()).unwrap();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _stop = StopOnDrop(&done);
            for i in 0..ROUNDS {
                writer
                    .put(&k, versions[i % versions.len()].clone())
                    .unwrap();
            }
        });
        scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                let got = reader.get(&k).unwrap();
                assert!(versions.contains(&got), "torn get: {} bytes", got.len());
            }
        });
        scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                let got = reader.get_range(&k, 0..1000).unwrap();
                assert_eq!(got.base_offset(), 0);
                assert!(versions.iter().any(|v| *got == v[..1000]), "torn get_range");
            }
        });
        scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                let listed = reader.list(&KeyPrefix::new("segments/").unwrap()).unwrap();
                assert_eq!(listed.len(), 1, "{listed:?}");
                let size = usize::try_from(listed[0].size).unwrap();
                assert!(
                    versions.iter().any(|v| v.len() == size),
                    "listed a partial size {size}"
                );
            }
        });
    });
    assert_no_temp_files(dir.path());
}

// SPEC §10.1: `put_if_absent` that collides with an object deleted before it can be read must retry, so the name
// is free again; reporting the vanished file as an error (or as `AlreadyExists`) would be wrong. Another process
// makes the object appear and vanish atomically (link, unlink) in a tight loop, so the window between the failed
// link and the read is hit often.
#[test]
fn put_if_absent_racing_an_appearing_and_vanishing_object_never_misreports() {
    const ROUNDS: usize = 1000;
    let dir = tempfile::tempdir().unwrap();
    let creator = LocalStorage::open(dir.path()).unwrap();
    let k = key("manifests/00000000000000000007.mf");
    let mine = Bytes::from_static(b"mine");
    // The object's directory and a complete file to publish under its name, outside the store's layout.
    creator.put(&key("manifests/other"), Bytes::new()).unwrap();
    let object = dir.path().join("manifests@/00000000000000000007.mf");
    let theirs = dir.path().join("@theirs"); // not a decodable name: never listed
    std::fs::write(&theirs, b"theirs").unwrap();
    let done = AtomicBool::new(false);
    let (created, existed, exhausted) = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                match std::fs::hard_link(&theirs, &object) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(err) => panic!("{err}"),
                }
                std::fs::remove_file(&object).unwrap();
            }
        });
        let outcomes = scope.spawn(|| {
            let (mut created, mut existed, mut exhausted) = (0, 0, 0);
            let _stop = StopOnDrop(&done);
            for _ in 0..ROUNDS {
                match creator.put_if_absent(&k, mine.clone()) {
                    Ok(PutOutcome::Created) => created += 1,
                    Ok(PutOutcome::AlreadyExists) => existed += 1,
                    // Only after LINK_ATTEMPTS collisions, each with a vanished object: allowed, but never a raw
                    // NotFound from reading the vanished file.
                    Err(err) => {
                        assert_eq!(err.kind(), ErrorKind::Io, "{err}");
                        let source = io_source(&err).unwrap();
                        assert_ne!(
                            source.kind(),
                            std::io::ErrorKind::NotFound,
                            "a vanished object was reported instead of retried: {err}"
                        );
                        exhausted += 1;
                    }
                }
            }
            (created, existed, exhausted)
        });
        outcomes.join().unwrap()
    });
    assert_eq!(created + existed + exhausted, ROUNDS);
    assert_no_temp_files(dir.path());
}

// Equal bytes found is `Created` (SPEC §10.1), also when the earlier attempt came from another instance.
#[test]
fn put_if_absent_with_equal_bytes_is_created_across_instances() {
    const WRITERS: usize = 6;
    let dir = tempfile::tempdir().unwrap();
    let a = LocalStorage::open(dir.path()).unwrap();
    let b = LocalStorage::open(dir.path()).unwrap();
    let k = key("manifests/00000000000000000001.mf");
    let data = Bytes::from_static(b"manifest");
    let barrier = Barrier::new(WRITERS);
    let outcomes: Vec<PutOutcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..WRITERS)
            .map(|w| {
                let (s, k, data, barrier) = (if w % 2 == 0 { &a } else { &b }, &k, &data, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    s.put_if_absent(k, data.clone()).unwrap()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(
        outcomes.iter().all(|o| *o == PutOutcome::Created),
        "{outcomes:?}"
    );
    assert_eq!(
        b.put_if_absent(&k, Bytes::from_static(b"different"))
            .unwrap(),
        PutOutcome::AlreadyExists
    );
    assert_eq!(a.get(&k).unwrap(), data);
    assert_no_temp_files(dir.path());
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Failing filesystems.
// ---------------------------------------------------------------------------------------------------------------

/// Whether permission bits bind this process (they do not for root, or with `CAP_DAC_OVERRIDE`).
#[cfg(unix)]
fn permissions_are_enforced(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let probe = dir.join("probe");
    std::fs::create_dir(&probe).unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o555)).unwrap();
    let enforced = std::fs::write(probe.join("x"), b"").is_err();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&probe).unwrap();
    enforced
}

#[cfg(unix)]
#[test]
fn writes_into_a_read_only_directory_are_io_and_leave_no_trace() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, s) = store();
    if !permissions_are_enforced(dir.path()) {
        eprintln!("skipped: permission bits are not enforced for this user (root?)");
        return;
    }
    s.put(&key("d/x"), Bytes::from_static(b"old")).unwrap();
    s.put(&key("top"), Bytes::from_static(b"old top")).unwrap();
    let mode =
        |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    mode(&s.root().join("d@"), 0o555);
    mode(s.root(), 0o555);

    let checks: Vec<(&str, persia_storage::Result<()>)> = vec![
        (
            "put replaces",
            s.put(&key("d/x"), Bytes::from_static(b"new")),
        ),
        (
            "put creates",
            s.put(&key("d/y"), Bytes::from_static(b"new")),
        ),
        (
            "put in new dir",
            s.put(&key("e/y"), Bytes::from_static(b"new")),
        ),
        (
            "put at root",
            s.put(&key("top"), Bytes::from_static(b"new")),
        ),
        (
            "put_if_absent creates",
            s.put_if_absent(&key("d/z"), Bytes::from_static(b"new"))
                .map(drop),
        ),
        (
            "put_if_absent in new dir",
            s.put_if_absent(&key("f/z"), Bytes::from_static(b"new"))
                .map(drop),
        ),
        ("delete", s.delete(&key("d/x"))),
    ];
    // Reads still work, and every write left the previous state.
    let after = (
        s.get(&key("d/x")).unwrap(),
        s.get(&key("top")).unwrap(),
        keys(&s, ""),
    );
    let leftovers = temp_files(s.root());
    mode(&s.root().join("d@"), 0o755);
    mode(s.root(), 0o755);

    for (what, result) in checks {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Io, "{what}");
    }
    assert_eq!(&after.0[..], b"old");
    assert_eq!(&after.1[..], b"old top");
    assert_eq!(after.2, ["d/x", "top"]);
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Writable again: the store recovers without reopening.
    s.put(&key("d/y"), Bytes::from_static(b"ok")).unwrap();
    s.put(&key("e/y"), Bytes::from_static(b"ok")).unwrap();
    assert_eq!(keys(&s, ""), ["d/x", "d/y", "e/y", "top"]);
    assert_no_temp_files(s.root());
}

/// The kind of every operation on `k`, labelled; `None` for success.
fn op_kinds(s: &LocalStorage, k: &Key) -> Vec<(&'static str, Option<ErrorKind>)> {
    let kind = |r: persia_storage::Result<()>| r.err().map(|e| e.kind());
    vec![
        ("put", kind(s.put(k, Bytes::from_static(b"x")))),
        (
            "put_if_absent",
            kind(s.put_if_absent(k, Bytes::from_static(b"x")).map(drop)),
        ),
        ("get", kind(s.get(k).map(drop))),
        ("get_range", kind(s.get_range(k, 0..1).map(drop))),
        ("delete", kind(s.delete(k))),
    ]
}

/// SPEC §10.1.1: writes through an entry the layout does not expect fail with `Io`; reads of an entry `list` would
/// skip are `NotFound`. `delete` is not specified for such entries: it must not panic, and either succeeds or is `Io`.
fn assert_foreign_entry_kinds(s: &LocalStorage, k: &Key) {
    for (what, kind) in op_kinds(s, k) {
        match what {
            "put" | "put_if_absent" => assert_eq!(kind, Some(ErrorKind::Io), "{what} {k}"),
            "get" | "get_range" => assert_eq!(kind, Some(ErrorKind::NotFound), "{what} {k}"),
            _ => assert!(
                matches!(kind, None | Some(ErrorKind::Io)),
                "{what} {k}: {kind:?}"
            ),
        }
    }
}

// The root, a directory of every path, replaced by a file after `open`.
#[test]
fn root_replaced_by_a_file_fails_writes_and_list() {
    let (_dir, s) = store();
    s.put(&key("a/b"), Bytes::from_static(b"x")).unwrap();
    std::fs::remove_dir_all(s.root()).unwrap();
    std::fs::write(s.root(), b"not a directory").unwrap();
    for k in [key("a/b"), key("x")] {
        assert_foreign_entry_kinds(&s, &k);
    }
    for p in ["", "a/", "a"] {
        assert_eq!(
            s.list(&KeyPrefix::new(p).unwrap()).unwrap_err().kind(),
            ErrorKind::Io,
            "list {p:?}"
        );
    }
    // Reopening reports it too.
    assert!(LocalStorage::open(s.root()).is_err());
}

// A foreign directory sits where the layout puts an object file.
#[test]
fn a_directory_in_place_of_an_object_is_not_an_object() {
    let (_dir, s) = store();
    s.put(&key("d/sibling"), Bytes::from_static(b"keep"))
        .unwrap();
    std::fs::create_dir_all(s.root().join("d@/obj/inner")).unwrap();
    std::fs::create_dir(s.root().join("top")).unwrap();
    for k in [key("d/obj"), key("top")] {
        assert_foreign_entry_kinds(&s, &k);
    }
    // The foreign directories are not objects, survive, and nothing was written beside them.
    assert_eq!(keys(&s, ""), ["d/sibling"]);
    assert!(s.root().join("d@/obj/inner").is_dir());
    assert_eq!(&s.get(&key("d/sibling")).unwrap()[..], b"keep");
    assert_no_temp_files(s.root());
}

// A foreign file sits where the layout puts a directory.
#[test]
fn a_file_in_place_of_a_directory_is_not_a_directory() {
    let (_dir, s) = store();
    std::fs::write(s.root().join("d@"), b"file named like a directory").unwrap();
    std::fs::create_dir_all(s.root().join("e@")).unwrap();
    std::fs::write(s.root().join("e@/f@"), b"nested").unwrap();
    for k in [key("d/obj"), key("e/f/obj")] {
        assert_foreign_entry_kinds(&s, &k);
    }
    assert_eq!(keys(&s, ""), Vec::<String>::new());
    assert_eq!(
        std::fs::read(s.root().join("d@")).unwrap(),
        b"file named like a directory"
    );
    assert_no_temp_files(s.root());
}

// SPEC §10.1.1: a symbolic link on the write path is an `Io` error, so a link planted in the root cannot redirect
// writes outside it; a symbolic link or socket with an object's name reads as `NotFound`, as `list` skips it.
#[cfg(unix)]
#[test]
fn symlinks_and_sockets_are_not_objects_or_directories() {
    use std::os::unix::fs::symlink;

    let (dir, s) = store();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("obj"), b"outside the root").unwrap();
    // A symlinked directory leaving the root, and a symlinked object file.
    symlink(&outside, s.root().join("out@")).unwrap();
    symlink(outside.join("obj"), s.root().join("link")).unwrap();
    // A dangling symlink with an object's name.
    symlink(dir.path().join("missing"), s.root().join("dangling")).unwrap();
    // A socket with an object's name (no FIFO without libc; a socket is non-regular too).
    let _socket = std::os::unix::net::UnixListener::bind(s.root().join("sock")).unwrap();

    for k in ["link", "dangling", "sock"] {
        let k = key(k);
        for what in ["get", "get_range"] {
            let kind = match what {
                "get" => s.get(&k).unwrap_err().kind(),
                _ => s.get_range(&k, 0..1).unwrap_err().kind(),
            };
            assert_eq!(kind, ErrorKind::NotFound, "{what} {k}");
        }
    }
    for (what, kind) in op_kinds(&s, &key("out/obj")) {
        match what {
            "put" | "put_if_absent" => assert_eq!(kind, Some(ErrorKind::Io), "{what}"),
            "get" | "get_range" => assert_eq!(kind, Some(ErrorKind::NotFound), "{what}"),
            _ => {}
        }
    }
    // Nothing reached the directory outside the root.
    let mut names: Vec<_> = std::fs::read_dir(&outside)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["obj"]);
    assert_eq!(
        std::fs::read(outside.join("obj")).unwrap(),
        b"outside the root"
    );
    assert_eq!(keys(&s, ""), Vec::<String>::new());
    assert_no_temp_files(s.root());
}

// SPEC §10.1.1: `list` skips entries that are not Persia's and treats a missing prefix directory as "nothing
// matches"; a foreign file where the prefix's directory should be is not Persia's either, so `list(p)` must equal
// `list("")` filtered by `p` (here: nothing), not fail.
#[test]
fn list_through_a_file_in_place_of_a_directory_matches_the_full_listing() {
    let (_dir, s) = store();
    s.put(&key("dz"), Bytes::from_static(b"1")).unwrap();
    std::fs::write(s.root().join("d@"), b"file named like a directory").unwrap();
    assert_eq!(keys(&s, ""), ["dz"]);
    assert_eq!(keys(&s, "d/"), Vec::<String>::new());
    assert_eq!(keys(&s, "d/x"), Vec::<String>::new());
}

// ---------------------------------------------------------------------------------------------------------------
// 5. Hostile trees for `list`.
// ---------------------------------------------------------------------------------------------------------------

// `a@/a@/…` far deeper than any key: only the keys within `Key::MAX_LEN` are listed, and the walk stops.
#[test]
fn list_over_a_tree_deeper_than_any_key_returns_the_valid_keys() {
    let (_dir, s) = store();
    // 511 directories `a@` (keys `a/…`) and a last segment: `bb` makes the longest valid key (1024 bytes).
    let longest = format!("{}bb", "a/".repeat(Key::MAX_LEN / 2 - 1));
    let short = format!("{}a", "a/".repeat(Key::MAX_LEN / 2 - 1));
    assert_eq!(longest.len(), Key::MAX_LEN);
    s.put(&key(&longest), Bytes::from_static(b"deep")).unwrap();
    s.put(&key(&short), Bytes::from_static(b"deep")).unwrap();
    // Extend the tree to 700 levels by hand, with objects that would be keys over 1024 bytes.
    let mut path = s.root().to_path_buf();
    for _ in 0..700 {
        path.push("a@");
    }
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("z"), b"too deep").unwrap();
    let mut mid = s.root().to_path_buf();
    for _ in 0..(Key::MAX_LEN / 2) {
        mid.push("a@");
    }
    std::fs::write(mid.join("z"), b"1025 bytes").unwrap();

    let mut want = vec![longest.clone(), short.clone()];
    want.sort();
    assert_eq!(keys(&s, ""), want);
    assert_eq!(keys(&s, "a/a/"), want);
    assert_eq!(keys(&s, &longest[..1000]), want);
    assert_eq!(&s.get(&key(&longest)).unwrap()[..], b"deep");
}

#[cfg(unix)]
#[test]
fn list_skips_non_utf8_names_and_symlinks_without_looping() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    let (dir, s) = store();
    s.put(&key("d/obj"), Bytes::from_static(b"1")).unwrap();
    s.put(&key("e"), Bytes::from_static(b"2")).unwrap();
    let root = s.root();
    // Non-UTF-8 file and directory names (only possible on Unix), with valid-looking content inside.
    std::fs::write(root.join(OsStr::from_bytes(b"\xFFbad")), b"x").unwrap();
    let bad_dir = root.join(OsStr::from_bytes(b"\xC3@"));
    std::fs::create_dir(&bad_dir).unwrap();
    std::fs::write(bad_dir.join("x"), b"x").unwrap();
    std::fs::write(root.join("d@").join(OsStr::from_bytes(b"ok\x80")), b"x").unwrap();
    // Symlinked directories, including loops back to the root and to an ancestor, and one leaving the root.
    symlink(root, root.join("loop@")).unwrap();
    symlink(root.join("d@"), root.join("d@/self@")).unwrap();
    symlink("..", root.join("d@/up@")).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), b"outside the root").unwrap();
    symlink(&outside, root.join("out@")).unwrap();
    // A dangling symlink with an object's name.
    symlink(root.join("missing"), root.join("dangling")).unwrap();

    assert_eq!(keys(&s, ""), ["d/obj", "e"]);
    assert_eq!(keys(&s, "d/"), ["d/obj"]);
}

// SPEC §10.1.1: "Symbolic links and other non-regular entries are skipped." That holds for the directory a prefix
// names too: otherwise `list(p)` disagrees with `list("")` filtered by `p`, and a symlink planted in the root lists
// files outside it.
#[cfg(unix)]
#[test]
fn list_does_not_follow_a_symlinked_prefix_directory() {
    use std::os::unix::fs::symlink;

    let (dir, s) = store();
    s.put(&key("e"), Bytes::from_static(b"2")).unwrap();
    let root = s.root();
    symlink(root, root.join("loop@")).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), b"outside the root").unwrap();
    symlink(&outside, root.join("out@")).unwrap();

    assert_eq!(keys(&s, ""), ["e"]);
    assert_eq!(keys(&s, "loop/"), Vec::<String>::new(), "followed loop@");
    assert_eq!(
        keys(&s, "out/"),
        Vec::<String>::new(),
        "listed outside the root"
    );
}

// Canonical names whose segment is not a valid key segment (forbidden characters, dot segments) are skipped,
// together with everything under them.
#[test]
fn list_skips_names_that_decode_to_invalid_keys() {
    let (_dir, s) = store();
    s.put(&key("ok"), Bytes::from_static(b"1")).unwrap();
    let root = s.root();
    for file in ["%3A", "%00", "%5C", "%7F", "a%3Ab"] {
        std::fs::write(root.join(file), b"x").unwrap();
    }
    for dir in ["..@", ".@", "%3A@", "%0A@"] {
        std::fs::create_dir(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join("x"), b"x").unwrap();
    }
    // An empty directory with a valid name holds no object.
    std::fs::create_dir(root.join("empty@")).unwrap();
    assert_eq!(keys(&s, ""), ["ok"]);
    assert_eq!(keys(&s, "empty/"), Vec::<String>::new());
}
