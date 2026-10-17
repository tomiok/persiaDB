//! Renaming over a mapped file (ROADMAP 2.4.5.3).

#![allow(
    clippy::unwrap_used,
    clippy::print_stderr,
    unsafe_code // maps temporary files this test owns (ADR-0010)
)]

use std::fs::File;
use std::io::Write;

use persia_format::mmap::MmapFile;

/// Maps `old`, optionally closes its handle, renames `new` over it, and reports the rename's outcome; the mapping
/// must keep the old bytes either way.
fn rename_over_mapped(close_handle: bool) -> std::io::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("app.persia");
    let new = dir.path().join("app.persia.compact");
    std::fs::write(&old, b"old contents").unwrap();
    File::create(&new)
        .unwrap()
        .write_all(b"new contents")
        .unwrap();
    let file = File::open(&old).unwrap();
    // SAFETY: both files belong to this test, and neither is written or truncated while mapped (the rename
    // replaces the name, not the mapped file's bytes).
    let map = unsafe { MmapFile::map(&file, 0..12) }.unwrap();
    if close_handle {
        drop(file);
    }
    let renamed = std::fs::rename(&new, &old);
    assert_eq!(
        &*map, b"old contents",
        "a mapping keeps the old file's bytes"
    );
    if renamed.is_ok() {
        assert_eq!(std::fs::read(&old).unwrap(), b"new contents");
    }
    renamed
}

/// Whether a live mapping of a file blocks renaming another file over it (ROADMAP 2.4.5.3; SPEC §4.6, §4.7;
/// ADR-0010): container compaction renames the new file over the container while readers may have it mapped. It does
/// not, on Unix nor on Windows (where the standard library's rename falls back to POSIX semantics), with the mapped
/// file's handle open or closed; the mapping keeps the old file's bytes.
#[test]
fn a_mapping_never_blocks_a_rename_over_the_file() {
    for close_handle in [false, true] {
        rename_over_mapped(close_handle).unwrap();
    }
}
