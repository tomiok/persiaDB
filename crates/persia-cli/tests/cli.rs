//! The `persia` binary's maintenance commands, end to end (ROADMAP 2.3.8, 2.4.19; ADR-0018): output lines, what is
//! removed and kept, and the errors for a missing path or a container a writer holds.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use persia_storage::container::{Container, Mode};

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
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    for args in [
        vec!["container", "sweep", path_arg(&missing)],
        vec!["store", "files", path_arg(&missing)],
        vec!["store", "sweep", path_arg(&missing)],
    ] {
        let out = persia(&args);
        assert!(!out.status.success(), "{args:?}");
        assert!(!String::from_utf8_lossy(&out.stderr).is_empty(), "{args:?}");
        assert!(!missing.exists(), "{args:?} created {}", missing.display());
    }
}

#[test]
fn store_sweep_removes_stale_temporaries() {
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
        assert!(forced.iter().all(|(a, ..)| a == "removed"), "{forced:?}");
    }
    assert!(root.join("segments@/1.seg").exists());
}

/// A quarantine copy the sweep's own open makes (of a corrupt tail) is never removed by that sweep: nobody has seen
/// it yet (ADR-0014). The copies that existed before are.
#[test]
fn container_sweep_keeps_a_quarantine_copy_its_own_open_made() {
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
