//! `persia verify` (ROADMAP 6.6.3, 9.5.3): checks a whole database read-only, with [`ReadOnlyDatabase::verify`]
//! before the blob pack is opened, then its blobs with [`ReadOnlyBlobDatabase::verify_blobs`] (SPEC §9.5, ADR-0053).
//! A document error is reported without opening the pack. The pack open and the pack scan both bound each frame's
//! length by its kind before reading it (ROADMAP 9.2.14).
//!
//! The output is for people: `key  value` lines, then `verify  ok`. A failed check is the command's error.
//!
//! A live writer may commit and collect blobs while this runs. A dangling reference is reported as such only if the
//! container's committed `seq` has not moved since the documents were opened. Otherwise the documents may be older
//! than the pack (a document deleted, then its blob collected), and the run fails with "changed during verify,
//! rerun" (exit status 1, not 3).

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use persia::internal::documents::ReadOnlyBlobDatabase;
use persia::internal::verify::{Scan, VerifyError};
use persia_engine::db::{DatabaseOptions, ReadOnlyDatabase, Tail};
use persia_storage::container::{Container, Mode};

/// Opens the database at `path` read-only, verifies it and its blobs as deep as `scan` asks, and writes what it found
/// to `out`.
pub fn verify(out: &mut dyn Write, path: &Path, scan: Scan) -> Result<()> {
    if !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    // The built-in analyzers: replay analyzes the `text` fields of the logged documents, and a schema names only
    // built-in analyzers (SPEC §6.2.2); one this build lacks, from a newer build, fails the open as unsupported.
    let options = DatabaseOptions::default();
    let docs = ReadOnlyDatabase::open(path, &options.registry)
        .with_context(|| format!("opening {}", path.display()))?;
    // Document checks before the pack is opened, so a damaged container is reported without opening the pack.
    let report = docs
        .verify()
        .with_context(|| format!("verifying {}", path.display()))?;
    let db = ReadOnlyBlobDatabase::with_pack_beside(docs, path)
        .with_context(|| format!("opening the blob pack of {}", path.display()))?;
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
    // The blob rows; a dangling reference (SPEC §9.5) or a damaged pack frame is the command's error.
    let blobs = match db.verify_blobs(scan) {
        Ok(blobs) => blobs,
        Err(e @ VerifyError::Dangling { .. }) => {
            changed_during_verify(&db, path)?;
            return Err(e).with_context(|| format!("verifying the blobs of {}", path.display()));
        }
        Err(e) => {
            return Err(e).with_context(|| format!("verifying the blobs of {}", path.display()));
        }
    };
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
    for (key, value) in rows {
        writeln!(out, "{key:<18}{value}")?;
    }
    writeln!(out, "verify            ok")?;
    Ok(())
}

/// After a dangling reference: fails (status 1) if the container's committed `seq` moved since the documents were
/// opened. A blob is collected only after the commit that dropped its last reference (SPEC §9.5), and that commit
/// precedes this read, so an unmoved `seq` means the pack this run opened lacks a blob documents it read reference.
/// The container at `path` is opened again rather than this handle's superblock re-read: a compaction renames a new
/// file over the path, and this handle keeps reading the old one.
fn changed_during_verify(db: &ReadOnlyBlobDatabase, path: &Path) -> Result<()> {
    let opened = db.database().container().superblock().seq;
    let now = Container::open(path, Mode::ReadOnly)
        .with_context(|| format!("opening {} again", path.display()))?
        .superblock()
        .seq;
    if now != opened {
        bail!(
            "{} changed during verify (seq {opened} at the start, {now} now): a writer committed while it ran, so a \
             blob its documents referenced may have been collected since; rerun",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)] // Tests: inputs are fixed.

    use persia::internal::documents::{Blob, BlobDatabase};
    use persia::internal::gc::GcOptions;
    use persia_engine::document::Document;
    use persia_engine::schema::SchemaBuilder;
    use persia_engine::wal::Durability;

    use super::*;

    /// A reference found dangling because the documents were opened before a writer deleted their document,
    /// committed and collected its blob, and the pack after: "changed during verify" (status 1), never a dangling
    /// reference (status 3). With no commit since the documents' open, it is a dangling reference.
    #[test]
    fn a_dangling_reference_after_a_commit_since_the_documents_open_asks_for_a_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let options = DatabaseOptions::default();
        let mut writer = BlobDatabase::create(&path, DatabaseOptions::default()).unwrap();
        writer
            .log_mut()
            .create_collection(SchemaBuilder::new("c").build().unwrap())
            .unwrap();
        writer
            .put(
                "c",
                &Document::new("a"),
                Blob::from_reader(&b"collected"[..]),
                Durability::Fsync,
            )
            .unwrap();
        writer.commit().unwrap();
        let docs = ReadOnlyDatabase::open(&path, &options.registry).unwrap();
        writer
            .log_mut()
            .delete("c", "a", Durability::Fsync)
            .unwrap();
        writer.commit().unwrap();
        assert_eq!(
            writer
                .gc(&GcOptions::default())
                .unwrap()
                .sweep
                .blobs_deleted,
            1
        );
        let db = ReadOnlyBlobDatabase::with_pack_beside(docs, &path).unwrap();
        assert!(matches!(
            db.verify_blobs(Scan::Quick),
            Err(VerifyError::Dangling { .. })
        ));
        let err = changed_during_verify(&db, &path).unwrap_err();
        assert!(
            format!("{err:#}").contains("changed during verify"),
            "{err:#}"
        );
        assert_eq!(crate::status_of(&err), 1);

        // The same reader with nothing committed since: the dangling reference stands.
        drop(writer);
        let docs = ReadOnlyDatabase::open(&path, &options.registry).unwrap();
        let db = ReadOnlyBlobDatabase::with_pack_beside(docs, &path).unwrap();
        changed_during_verify(&db, &path).unwrap();
    }
}
