//! The `persia` binary's maintenance commands, end to end (ROADMAP 2.3.8, 2.4.19, 6.6.3; ADR-0019): output lines,
//! what is removed and kept, `verify` on sound and corrupt databases, and the errors for a missing path or a
//! container a writer holds.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use persia_storage::container::{Container, Mode};

/// Serializes the tests of this binary. A test that spawns `persia` forks, and a forked child holds copies of every
/// descriptor the parent had open until it execs, flock included: a container another test had open at that moment
/// would then look locked, to that test and to the `persia` it runs.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn persia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_persia"))
        .args(args)
        .output()
        .unwrap()
}

fn path_arg(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// `(action, kind, state, file name)` of each output line.
fn lines(out: &Output) -> Vec<(String, String, String, String)> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut v: Vec<_> = String::from_utf8(out.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            assert_eq!(f.len(), 4, "{l}");
            let name = Path::new(f[3]).file_name().unwrap().to_string_lossy();
            (f[0].into(), f[1].into(), f[2].into(), name.into_owned())
        })
        .collect();
    v.sort();
    v
}

fn row(action: &str, kind: &str, state: &str, name: &str) -> (String, String, String, String) {
    (action.into(), kind.into(), state.into(), name.into())
}

const TMP: &str = "app.persia.compact.0123456789abcdef.3";
const QUARANTINE: &str = "app.persia.quarantine.0000000000003028.0123456789abcdef.1";

/// A container with a stale compaction file and a quarantine copy beside it, canonical paths.
fn container() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("app.persia");
    drop(Container::create(&path).unwrap());
    std::fs::write(root.join(TMP), b"half a container").unwrap();
    std::fs::write(root.join(QUARANTINE), b"a corrupt tail").unwrap();
    std::fs::write(root.join("notes.txt"), b"not ours").unwrap();
    (dir, path)
}

#[test]
fn container_files_lists_and_removes_nothing() {
    let _serial = serial();
    let (_dir, path) = container();
    let out = persia(&["container", "files", path_arg(&path)]);
    assert_eq!(
        lines(&out),
        [
            row("found", "quarantine", "free", QUARANTINE),
            row("found", "temporary", "free", TMP),
        ]
    );
    assert!(path.with_file_name(TMP).exists());
}

#[test]
fn container_sweep_keeps_quarantine_unless_asked() {
    let _serial = serial();
    let (_dir, path) = container();
    let out = persia(&["container", "sweep", path_arg(&path)]);
    assert_eq!(
        lines(&out),
        [
            row("kept", "quarantine", "free", QUARANTINE),
            row("removed", "temporary", "free", TMP),
        ]
    );
    assert!(!path.with_file_name(TMP).exists());
    assert!(path.with_file_name(QUARANTINE).exists());

    let out = persia(&["container", "sweep", "--quarantine", path_arg(&path)]);
    assert_eq!(
        lines(&out),
        [row("removed", "quarantine", "free", QUARANTINE)]
    );
    assert!(!path.with_file_name(QUARANTINE).exists());
    assert!(path.with_file_name("notes.txt").exists());
    // The container itself is intact.
    Container::open(&path, Mode::ReadOnly).unwrap();
}

#[cfg(unix)]
#[test]
fn container_sweep_fails_while_a_writer_holds_the_container() {
    let _serial = serial();
    let (_dir, path) = container();
    let writer = Container::open(&path, Mode::ReadWrite).unwrap();
    let out = persia(&["container", "sweep", path_arg(&path)]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is open by a writer"), "{err}");
    assert!(path.with_file_name(TMP).exists());
    drop(writer);
}

#[test]
fn missing_paths_are_errors_and_create_nothing() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    for args in [
        vec!["container", "files", path_arg(&missing)],
        vec!["container", "sweep", path_arg(&missing)],
        vec!["store", "files", path_arg(&missing)],
        vec!["store", "sweep", path_arg(&missing)],
        vec!["verify", path_arg(&missing)],
    ] {
        let out = persia(&args);
        // Not a corruption: the generic failure status (ROADMAP 6.6.7).
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(!String::from_utf8_lossy(&out.stderr).is_empty(), "{args:?}");
        assert!(!missing.exists(), "{args:?} created {}", missing.display());
    }
}

#[test]
fn store_sweep_removes_stale_temporaries() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(root.join("segments@")).unwrap();
    let stale = [
        "@tmp.0123456789abcdef.0",
        "segments@/@tmp.fedcba9876543210.7",
    ];
    for s in stale {
        std::fs::write(root.join(s), b"crashed put").unwrap();
    }
    std::fs::write(root.join("segments@/1.seg"), b"an object").unwrap();

    let found = lines(&persia(&["store", "files", path_arg(&root)]));
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .all(|(a, k, ..)| a == "found" && k == "temporary")
    );
    let swept = lines(&persia(&["store", "sweep", path_arg(&root)]));
    if cfg!(unix) {
        assert_eq!(
            swept,
            [
                row("removed", "temporary", "free", "@tmp.0123456789abcdef.0"),
                row("removed", "temporary", "free", "@tmp.fedcba9876543210.7"),
            ]
        );
        for s in stale {
            assert!(!root.join(s).exists(), "{s}");
        }
    } else {
        // Windows: unknown whether in use, so only --force removes them.
        assert!(
            swept
                .iter()
                .all(|(a, _, s, _)| a == "kept" && s == "unknown")
        );
        let forced = lines(&persia(&["store", "sweep", "--force", path_arg(&root)]));
        assert_eq!(forced.len(), stale.len(), "{forced:?}");
        assert!(forced.iter().all(|(a, ..)| a == "removed"), "{forced:?}");
        for s in stale {
            assert!(!root.join(s).exists(), "{s}");
        }
    }
    assert!(root.join("segments@/1.seg").exists());
}

/// A quarantine copy the sweep's own open makes (of a corrupt tail) is never removed by that sweep: nobody has seen
/// it yet (ADR-0014). The copies that existed before are.
#[test]
fn container_sweep_keeps_a_quarantine_copy_its_own_open_made() {
    let _serial = serial();
    let (_dir, path) = container();
    // Bytes after the last frame that are neither zeros nor frames: a read-write open quarantines them.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(&[0xab; 100]);
    std::fs::write(&path, &bytes).unwrap();

    let out = persia(&["container", "sweep", "--quarantine", path_arg(&path)]);
    let got = lines(&out);
    assert!(
        got.contains(&row("removed", "quarantine", "free", QUARANTINE)),
        "{got:?}"
    );
    let new: Vec<_> = got
        .iter()
        .filter(|(a, k, _, n)| a == "kept" && k == "quarantine" && n != QUARANTINE)
        .collect();
    assert_eq!(new.len(), 1, "{got:?}");
    assert!(path.with_file_name(&new[0].3).exists());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("kept for inspection"), "{err}");
    // A second sweep removes it: by now it was listed.
    let again = lines(&persia(&[
        "container",
        "sweep",
        "--quarantine",
        path_arg(&path),
    ]));
    assert_eq!(again, [row("removed", "quarantine", "free", &new[0].3)]);
}

/// A relative path works like the absolute one: the removed files are matched against the listing whichever way
/// the container was named.
#[test]
fn container_sweep_with_a_relative_path() {
    let _serial = serial();
    let (dir, path) = container();
    let out = Command::new(env!("CARGO_BIN_EXE_persia"))
        .args(["container", "sweep", "--quarantine", "app.persia"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        lines(&out),
        [
            row("removed", "quarantine", "free", QUARANTINE),
            row("removed", "temporary", "free", TMP),
        ]
    );
    assert!(!path.with_file_name(TMP).exists());
}

/// A path with control characters stays one line of four fields: they are escaped.
#[cfg(unix)]
#[test]
fn control_characters_in_paths_are_escaped() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let odd = std::fs::canonicalize(dir.path()).unwrap().join("a\tb\nc");
    std::fs::create_dir(&odd).unwrap();
    let path = odd.join("app.persia");
    drop(Container::create(&path).unwrap());
    std::fs::write(odd.join(TMP), b"half a container").unwrap();
    let out = persia(&["container", "files", path_arg(&path)]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 1, "{text:?}");
    let fields: Vec<&str> = rows[0].split('\t').collect();
    assert_eq!(fields.len(), 4, "{text:?}");
    assert!(fields[3].contains(r"a\tb\nc"), "{text:?}");
}

/// A sweep that fails part-way prints what it removed before failing, then the error: here the root's stale file
/// goes first, and a read-only directory then makes the sweep fail.
#[cfg(unix)]
#[test]
fn a_failed_store_sweep_still_reports_what_it_removed() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let first = root.join("@tmp.0123456789abcdef.0");
    std::fs::write(&first, b"crashed put").unwrap();
    let locked = root.join("b@");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("@tmp.0123456789abcdef.1"), b"crashed put").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = persia(&["store", "sweep", path_arg(&root)]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if locked.join("@tmp.0123456789abcdef.1").exists() {
        // Not running as root, which could remove it anyway.
        assert!(!out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.lines().collect::<Vec<_>>(),
            [format!("removed\ttemporary\tfree\t{}", first.display())],
            "{stdout}"
        );
        assert!(!String::from_utf8_lossy(&out.stderr).is_empty());
    } else {
        // Root removes files in a read-only directory: the sweep succeeds and removes both.
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 2);
    }
    assert!(!first.exists());
}

/// A database with one committed document, one synced but uncommitted, and its writer, still open.
fn database(path: &Path) -> persia_engine::db::Database {
    use persia_engine::db::{Database, DatabaseOptions};
    use persia_engine::document::{Document, Value};
    use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    use persia_engine::wal::Durability;

    let schema = SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()
        .unwrap();
    let body = schema.field("body").unwrap().id();
    let mut db = Database::create(path, DatabaseOptions::default()).unwrap();
    db.create_collection(schema).unwrap();
    for (id, durability) in [("n1", Durability::Fsync), ("n2", Durability::Fsync)] {
        let doc = Document::new(id).with(body, Value::String(format!("note {id}")));
        db.put("notes", &doc, durability).unwrap();
        if id == "n1" {
            db.commit().unwrap();
        }
    }
    db
}

/// `key value` pairs of `persia verify`'s output, which must have succeeded.
fn report(out: &Output) -> Vec<(String, String)> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| {
            let (key, value) = l.split_once(' ').unwrap();
            (key.to_owned(), value.trim_start().to_owned())
        })
        .collect()
}

fn pair(key: &str, value: &str) -> (String, String) {
    (key.into(), value.into())
}

#[test]
fn verify_reports_a_sound_database_also_beside_its_writer() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = database(&path);
    let expected = [
        pair("seq", "3"),
        pair("collections", "1"),
        pair("segments", "1"),
        pair("documents", "1 (0 deleted)"),
        pair("replayed", "1 WAL records"),
        pair("uncommitted", "1 frames"),
        pair("tail", "clean"),
        pair("verify", "ok"),
    ];
    // Read-only: the writer keeps its lock and the file is unchanged.
    let before = std::fs::read(&path).unwrap();
    assert_eq!(report(&persia(&["verify", path_arg(&path)])), expected);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(writer);
    assert_eq!(report(&persia(&["verify", path_arg(&path)])), expected);
}

#[test]
fn verify_reports_a_torn_tail_and_fails_on_a_corrupt_one() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(database(&path));
    let mut bytes = std::fs::read(&path).unwrap();
    let end = bytes.len();
    bytes.resize(end + 4096, 0);
    std::fs::write(&path, &bytes).unwrap();
    let found = report(&persia(&["verify", path_arg(&path)]));
    let tail = format!("torn at offset {end} (a read-write open truncates it)");
    assert!(found.contains(&pair("tail", &tail)), "{found:?}");

    bytes[end + 100] = 0x5a;
    std::fs::write(&path, &bytes).unwrap();
    let out = persia(&["verify", path_arg(&path)]);
    // Corruption has its own exit status (ROADMAP 6.6.7).
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("corrupt after its last commit"), "{err}");
    assert_eq!(out.stdout, b"");
}

#[test]
fn a_file_that_is_not_a_container_exits_with_4() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.txt");
    std::fs::write(&notes, b"this is not a database").unwrap();
    let empty = dir.path().join("empty");
    std::fs::write(&empty, b"").unwrap();
    for path in [&notes, &empty] {
        for args in [
            vec!["verify", path_arg(path)],
            vec!["container", "sweep", path_arg(path)],
        ] {
            let out = persia(&args);
            assert_eq!(
                out.status.code(),
                Some(4),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    // The magic is intact and the header CRC is not: a damaged container, still 3.
    let db = dir.path().join("app.persia");
    drop(Container::create(&db).unwrap());
    let mut bytes = std::fs::read(&db).unwrap();
    bytes[12] ^= 0xff;
    std::fs::write(&db, &bytes).unwrap();
    let out = persia(&["verify", path_arg(&db)]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let help = persia(&["verify", "--help"]);
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(
        help.status.success() && text.contains("4 when the file is not a Persia container"),
        "{text}\n{}",
        String::from_utf8_lossy(&help.stderr)
    );
    let root = String::from_utf8(persia(&["--help"]).stdout).unwrap();
    assert!(
        root.contains("4 the file is not a Persia container"),
        "{root}"
    );
}

#[test]
fn verify_usage_errors_exit_with_2() {
    let _serial = serial();
    let out = persia(&["verify"]);
    assert_eq!(out.status.code(), Some(2));
}

/// One frame row of `persia inspect container`: `(offset, len, frame_seq, kind, state, note)`.
type FrameRow = (u64, u64, u64, String, String, String);

/// What [`inspected`] splits the output into.
type Inspected = (Vec<(String, String)>, Vec<FrameRow>, Vec<Vec<String>>);

/// The output of `persia inspect container`, which must have succeeded: its `key value` lines, its frame rows, and
/// the record rows after `records of id` (`frame record state op collection`, each split on whitespace).
fn inspected(out: &Output) -> Inspected {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    let mut sections = text.split("\n\n");
    let keys = sections
        .next()
        .unwrap()
        .lines()
        .map(|l| {
            let (key, value) = l.split_once(' ').unwrap();
            (key.to_owned(), value.trim_start().to_owned())
        })
        .collect();
    let frames = sections
        .next()
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let n = |i: usize| f[i].parse::<u64>().unwrap();
            (n(0), n(1), n(2), f[3].into(), f[4].into(), f[5..].join(" "))
        })
        .collect();
    let records = sections
        .next()
        .map(|s| {
            s.lines()
                .skip(2)
                .map(|l| l.split_whitespace().map(str::to_owned).collect())
                .collect()
        })
        .unwrap_or_default();
    (keys, frames, records)
}

/// ROADMAP 6.6.10: the superblock, every frame from `wal_start` with its state (a stray Manifest after `log_end`, as
/// a commit cut before its slot write leaves, with its `seq`), and by id the WAL records not yet flushed, beside a
/// live writer and without changing the file.
#[test]
fn inspect_container_lists_frames_and_the_records_of_an_id() {
    use persia_engine::wal::Durability;
    use persia_format::block::Compression;
    use persia_format::frame::FrameKind;

    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut writer = database(&path);
    // n1 is in a segment (its put was flushed); its delete, synced, is not.
    writer.delete("notes", "n1", Durability::Fsync).unwrap();
    drop(writer);
    // A copy of the committed manifest after `log_end`: a cut commit's frame.
    let committed_seq = {
        let mut c = Container::open(&path, Mode::ReadWrite).unwrap();
        let sb = *c.superblock();
        let manifest = c.read_frame(sb.manifest_off, 1 << 20).unwrap().payload;
        c.append(FrameKind::Manifest, &manifest, Compression::None)
            .unwrap();
        c.sync().unwrap();
        sb.seq
    };

    let c = Container::open(&path, Mode::ReadOnly).unwrap();
    let sb = *c.superblock();
    let before = std::fs::read(&path).unwrap();
    let (keys, frames, records) = inspected(&persia(&[
        "inspect",
        "container",
        path_arg(&path),
        "--id",
        "n1",
    ]));
    assert_eq!(std::fs::read(&path).unwrap(), before, "read-only");
    for (key, value) in [
        ("seq", sb.seq.to_string()),
        ("wal_start", sb.wal_start.to_string()),
        ("log_end", sb.log_end.to_string()),
        ("tail", format!("clean at {}", c.end())),
    ] {
        assert!(keys.contains(&pair(key, &value)), "{key}: {keys:?}");
    }
    let want: Vec<(u64, u64, u64, String)> = c
        .frames()
        .iter()
        .map(|f| (f.offset, f.len, f.frame_seq, format!("{:?}", f.kind)))
        .collect();
    let kind = |k: &str| match k {
        "wal" => "WalChunk",
        "manifest" => "Manifest",
        "segment" => "Segment",
        "tombstones" => "TombstoneSidecar",
        other => panic!("{other}"),
    };
    let got: Vec<(u64, u64, u64, String)> = frames
        .iter()
        .map(|(o, l, s, k, ..)| (*o, *l, *s, kind(k).to_owned()))
        .collect();
    assert_eq!(got, want);
    for (offset, _, _, kind, state, _) in &frames {
        let after = if *offset >= sb.log_end {
            "uncommitted"
        } else {
            "committed"
        };
        assert_eq!(state, after, "{offset} {kind}");
    }
    let manifests: Vec<(&str, &str)> = frames
        .iter()
        .filter(|f| f.3 == "manifest")
        .map(|f| (f.4.as_str(), f.5.as_str()))
        .collect();
    let seq = committed_seq.to_string();
    assert_eq!(
        manifests,
        [
            ("committed", format!("seq {seq}, the slot's").as_str()),
            ("uncommitted", format!("seq {seq}").as_str()),
        ]
    );
    // The delete of n1, logged after the commit: one record, after `log_end`.
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0][2..], ["uncommitted", "delete", "\"notes\""]);
    // n2's put, by its encoded id.
    let (_, _, n2) = inspected(&persia(&[
        "inspect",
        "container",
        path_arg(&path),
        "--id",
        "n2",
    ]));
    assert_eq!(n2.len(), 1, "{n2:?}");
    assert_eq!(n2[0][2..], ["uncommitted", "put", "\"notes\""]);
    let (_, _, none) = inspected(&persia(&[
        "inspect",
        "container",
        path_arg(&path),
        "--id",
        "zz",
    ]));
    assert!(none.is_empty(), "{none:?}");

    // Not a container: exit 4, like verify.
    let other = dir.path().join("other");
    std::fs::write(&other, b"not a persia file at all").unwrap();
    assert_eq!(
        persia(&["inspect", "container", path_arg(&other)])
            .status
            .code(),
        Some(4)
    );
}
