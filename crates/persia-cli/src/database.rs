//! `persia verify` (ROADMAP 6.6.3): checks a whole database read-only, with [`ReadOnlyDatabase::verify`].
//!
//! The output is for people: `key  value` lines, then `verify  ok`. A failed check is the command's error.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use persia_engine::db::{DatabaseOptions, ReadOnlyDatabase, Tail};

/// Opens the database at `path` read-only, verifies it, and writes what it found to `out`.
pub fn verify(out: &mut dyn Write, path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    // The built-in analyzers: replay analyzes the `text` fields of the logged documents.
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
    let rows = [
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
    for (key, value) in rows {
        writeln!(out, "{key:<18}{value}")?;
    }
    writeln!(out, "verify            ok")?;
    Ok(())
}
