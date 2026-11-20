//! `persia verify` (ROADMAP 6.6.3, 9.5.3): checks a whole database read-only, with [`ReadOnlyDatabase::verify`], then
//! its blobs with [`ReadOnlyBlobDatabase::verify_blobs`] (SPEC §9.5, ADR-0053).
//!
//! The output is for people: `key  value` lines, then `verify  ok`. A failed check is the command's error.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use persia::internal::documents::ReadOnlyBlobDatabase;
use persia::internal::verify::Scan;
use persia_engine::db::{DatabaseOptions, ReadOnlyDatabase, Tail};

/// Opens the database at `path` read-only, verifies it and its blobs as deep as `scan` asks, and writes what it found
/// to `out`.
pub fn verify(out: &mut dyn Write, path: &Path, scan: Scan) -> Result<()> {
    if !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    // The built-in analyzers: replay analyzes the `text` fields of the logged documents, and a schema names only
    // built-in analyzers (SPEC §6.2.2); one this build lacks, from a newer build, fails the open as unsupported.
    let options = DatabaseOptions::default();
    let db = ReadOnlyDatabase::open(path, &options.registry)
        .with_context(|| format!("opening {}", path.display()))?;
    let report = db
        .verify()
        .with_context(|| format!("verifying {}", path.display()))?;
    let tail = match report.tail {
        Tail::Clean => "clean".to_owned(),
        Tail::Torn { offset } => {
            format!("torn at offset {offset} (a read-write open truncates it)")
        }
        Tail::Stale { offset } => {
            format!("stale frames from offset {offset} on (a read-write open truncates them)")
        }
        other => format!("{other:?}"),
    };
    let mut rows = vec![
        ("seq", report.seq.to_string()),
        ("collections", report.collections.to_string()),
        ("segments", report.segments.to_string()),
        (
            "documents",
            format!("{} ({} deleted)", report.documents, report.deleted),
        ),
        ("replayed", format!("{} WAL records", report.replayed)),
        (
            "uncommitted",
            format!("{} frames", report.uncommitted_frames),
        ),
        ("tail", tail),
    ];
    // -- wiring (9.5.3, 9.5.6) --
    // The blob rows; a dangling reference (SPEC §9.5) or a damaged pack frame is the command's error.
    let db = ReadOnlyBlobDatabase::with_pack_beside(db, path)
        .with_context(|| format!("opening the blob pack of {}", path.display()))?;
    let blobs = db
        .verify_blobs(scan)
        .with_context(|| format!("verifying the blobs of {}", path.display()))?;
    rows.push((
        "references",
        format!("{} to {} blobs", blobs.references, blobs.blobs),
    ));
    rows.push((
        "pack",
        if blobs.pack {
            format!("{} blobs bound", blobs.bound)
        } else {
            "none".to_owned()
        },
    ));
    rows.push((
        "scan",
        match (blobs.scan, scan) {
            (Some(s), _) => format!(
                "{} frames: {} chunks ({} orphans), {} indexes",
                s.frames, s.chunks, s.orphan_chunks, s.indexes
            ),
            (None, Scan::Quick) => "skipped (--quick)".to_owned(),
            (None, _) => "no pack".to_owned(),
        },
    ));
    rows.push((
        "rehashed",
        match blobs.rehashed {
            Some(n) => format!("{n} blobs"),
            None => "skipped (--deep re-hashes every referenced blob)".to_owned(),
        },
    ));
    // -- end wiring --
    for (key, value) in rows {
        writeln!(out, "{key:<18}{value}")?;
    }
    writeln!(out, "verify            ok")?;
    Ok(())
}
