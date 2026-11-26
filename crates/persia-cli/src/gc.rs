//! `persia gc` (ROADMAP 10.4.5): blob garbage collection of SPEC §9.5 on request, through the embedded API's internal
//! layer (`persia::internal::gc`, [`BlobDatabase::gc`]): the mark, the sweep, and the pack rewrite. No GC logic lives
//! here.
//!
//! **The open.** GC runs in the writer (SPEC §9.5), so the command opens the database read-write with its blob pack
//! (`BlobDatabase::open_mark_only`, the writer's open for GC only, ADR-0069): it takes the container's writer lock and
//! the pack's, so it fails (status 1) while another writer has the database open, and recovery runs first (SPEC §8.6:
//! as with any writer's open, it may flush a WAL backlog larger than the default memtable, or retire a cut commit). It
//! writes no document. A database without a pack (`<name>.persia.blobs`, created by the first blob) has nothing to
//! collect: the report says so (`rewrite` `no_pack`, `pack.present` false), with status 0. No pack is created then,
//! but the pack's writer lock is taken, as by any writer that opens the database with its blob store, so its lock file
//! `<name>.persia.blobs.lock` appears (never removed, SPEC §9.3).
//!
//! **Analyzers.** The command has the built-in analyzers only, and collects a database whose schema names an analyzer
//! its application registered (SPEC §6.2.2; ROADMAP 10.4.7, ADR-0069): as `persia compact` (ADR-0068), its open
//! indexes such a `text` field with a stand-in that emits no token, and the mark reads no term (it lists the blob
//! references of the stored documents and of every WAL put from `wal_start` on, so a WAL record of such a collection is
//! marked). What the open cannot do is flush such a collection: when its recovery would flush any collection (every
//! flush flushes every memtable) while such a collection holds a replayed record (a memtable at the default 64 MiB, the
//! stand-in's, or out of docids), it fails with status 1 before flushing any of them, and nothing is marked or swept.
//! Open the database with its application and commit (closing a `persia::Db` commits), then collect.
//!
//! **The grace period.** `--grace <N>{s,m,h,d}`, 24 h by default (SPEC §9.5), is checked by the GC's own rule
//! (`GcOptions::validate`), which refuses one below its floor, twice the longest bind budget (2 h, SPEC §9.4): before
//! anything is opened or read (status 1), so a refused grace leaves every file as it was. It ages the objects of a
//! remote store only: the local pack keeps no age, and the sweep drops what the mark does not keep at once (SPEC §9.3,
//! §9.5). So the report's `young` is 0 for a local database; the grace is checked and reported all the same.
//!
//! **Readers.** A read-only handle of another process holds a reader lease beside the pack (SPEC §9.3). The sweep does
//! not run while any live lease is behind the committed seq, opening, ahead of it or unreadable (SPEC §9.5): the
//! report then counts the leases by state and says the sweep was held back (status 0: nothing was wrong, nothing was
//! deleted). The GC logs each such lease's path; the report has the counts only. Every commit, this command's open
//! included when its recovery commits, puts the read-only handles of other processes behind until they refresh.
//!
//! The text output is `key  value` rows. `--json` writes one object on one line, with these keys and no others:
//! - `grace_secs`, `held_back` (the sweep did not run: a reader lease or a backup's hold), `held_by_copy`;
//! - `pack` `{present, bytes_before, bytes_after}`: the pack file's size around the command, `null` without a pack;
//!   `bytes_after` is also `null` when the size could not be read after a GC that succeeded (said on stderr: the
//!   report is still written, status 0);
//! - `marked` `{blobs, uploads}`, `missing` (the hex ids of referenced blobs the pack lacks: dangling references,
//!   which `verify` reports and GC never creates);
//! - `deleted` `{blobs, uploads, chunks}`;
//! - `kept` `{pinned, young, unknown_age, changed, unreadable, uploads_skipped}` (`SweepReport`'s counts of unmarked
//!   objects the sweep kept, and whether it left the staged chunks alone);
//! - `rewrite` `{outcome, dead_bytes, frame_bytes, generation, frame_bytes_before, frame_bytes_after}`: `outcome` is
//!   `no_pack`, `below_ratio` (fewer than half the frames' bytes to drop: nothing rewritten), `deferred` (a pin taken
//!   after the plan: unchanged), `done` (the pack was replaced), `not_run` (the sweep was held back); the
//!   other keys are `null` where the outcome has none;
//! - `readers`: `null` without a pack, else `{live, current, opening, behind, ahead, unreadable, stale_removed,
//!   oldest_seq}` (SPEC §9.5).

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use persia::internal::documents::{BlobDatabase, Error as BlobDatabaseError};
use persia::internal::gc::{GcError, GcOptions, GcReport, Rewrite};
use persia_engine::db::{DatabaseError, DatabaseOptions};
use persia_storage::container::Error as ContainerError;
use serde_json::{Value, json};

/// What a `persia gc` run did: the GC's report and the pack file's size around it.
#[derive(Debug)]
struct Collected {
    grace: Duration,
    report: GcReport,
    /// The pack file's size before the open; `None` without a pack.
    pack_before: Option<u64>,
    /// Its size after the close.
    pack_after: PackAfter,
}

/// The pack file's size after a GC that succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackAfter {
    /// Read: `None` without a pack.
    Read(Option<u64>),
    /// It could not be read (said on stderr): the report gives it as unknown, `null`.
    Unknown,
}

impl PackAfter {
    /// The size the report writes: `None` without a pack, or when it is unknown.
    fn size(self) -> Option<u64> {
        match self {
            Self::Read(size) => size,
            Self::Unknown => None,
        }
    }
}

/// Collects the blob store of the database at `path` and writes what it did to `out`, as JSON with `json`.
pub fn gc(out: &mut dyn Write, path: &Path, grace: Duration, json: bool) -> Result<()> {
    let done = run(path, grace)?;
    if json {
        writeln!(out, "{}", to_json(&done))?;
    } else {
        write_text(out, &done)?;
    }
    Ok(())
}

/// The size of the file at `pack`, `None` when there is none.
fn pack_size(pack: &Path) -> Result<Option<u64>> {
    match std::fs::metadata(pack) {
        Ok(m) => Ok(Some(m.len())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading the size of {}", pack.display())),
    }
}

/// Opens the database at `path` read-write with its pack, runs the GC and closes it.
fn run(path: &Path, grace: Duration) -> Result<Collected> {
    let options = GcOptions {
        grace,
        ..GcOptions::default()
    };
    // Checked before anything is opened: options the GC would refuse must not cost an open, whose recovery may
    // commit or truncate (SPEC §8.6).
    options
        .validate()
        .with_context(|| format!("collecting the blobs of {}", path.display()))?;
    // A read-write open of a missing path must not be mistaken for a database to create.
    if !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    let pack = persia::internal::pack_path(path);
    let pack_before = pack_size(&pack)?;
    let db = BlobDatabase::open_mark_only(path, DatabaseOptions::default())
        .map_err(|err| opening(path, err))?;
    let report = db
        .gc(&options)
        .with_context(|| format!("collecting the blobs of {}", path.display()))?;
    drop(db);
    // The GC is done: a size that cannot be read now is reported as unknown, not as a failed collection.
    let pack_after = match pack_size(&pack) {
        Ok(size) => PackAfter::Read(size),
        Err(err) => {
            eprintln!("persia: {err:#} (the report gives the pack's size after as unknown)");
            PackAfter::Unknown
        }
    };
    Ok(Collected {
        grace,
        report,
        pack_before,
        pack_after,
    })
}

/// The error of a failed open of `path`: a writer holding the lock, or an analyzer this command lacks, said plainly.
fn opening(path: &Path, err: BlobDatabaseError) -> anyhow::Error {
    let database = match &err {
        BlobDatabaseError::Database(e) => Some(&**e),
        _ => None,
    };
    let context = match database {
        Some(DatabaseError::Container(ContainerError::Locked)) => format!(
            "{} is open by a writer; collect it once that is closed",
            path.display()
        ),
        // The mark-only open skips a registered analyzer (ADR-0069); what it still refuses is a version of a built-in
        // one this build lacks.
        Some(DatabaseError::UnregisteredAnalyzer { .. }) => format!(
            "opening {} (an analyzer version this build of persia gc lacks: collect it with a newer build)",
            path.display()
        ),
        Some(DatabaseError::AnalysisSkipped { .. }) => format!(
            "opening {} (persia gc has the built-in analyzers only and cannot flush that collection: open the \
             database with its application and commit, then collect it)",
            path.display()
        ),
        _ => format!("opening {}", path.display()),
    };
    anyhow::Error::new(err).context(context)
}

/// Whether the sweep did not run: held back by a reader lease or a backup's hold.
fn held_back(report: &GcReport) -> bool {
    report.held_by_copy || report.readers.as_ref().is_some_and(|r| r.held_back)
}

/// The pack rewrite's outcome and its numbers, as the report writes them (`None` where the outcome has none).
#[derive(Debug, Default, PartialEq, Eq)]
struct RewriteFields {
    outcome: &'static str,
    dead_bytes: Option<u64>,
    frame_bytes: Option<u64>,
    generation: Option<u64>,
    frame_bytes_before: Option<u64>,
    frame_bytes_after: Option<u64>,
}

fn rewrite_fields(done: &Collected) -> RewriteFields {
    let only = |outcome| RewriteFields {
        outcome,
        ..RewriteFields::default()
    };
    if held_back(&done.report) {
        return only("not_run");
    }
    match done.report.sweep.rewrite {
        None => only("not_run"),
        Some(Rewrite::NoPack) => only("no_pack"),
        Some(Rewrite::BelowRatio {
            dead_bytes,
            frame_bytes,
        }) => RewriteFields {
            outcome: "below_ratio",
            dead_bytes: Some(dead_bytes),
            frame_bytes: Some(frame_bytes),
            ..RewriteFields::default()
        },
        Some(Rewrite::Deferred) => only("deferred"),
        Some(Rewrite::Done {
            generation,
            frame_bytes_before,
            frame_bytes_after,
        }) => RewriteFields {
            outcome: "done",
            generation: Some(generation),
            frame_bytes_before: Some(frame_bytes_before),
            frame_bytes_after: Some(frame_bytes_after),
            ..RewriteFields::default()
        },
    }
}

fn to_json(done: &Collected) -> Value {
    let r = &done.report;
    let s = &r.sweep;
    let rewrite = rewrite_fields(done);
    let readers = r.readers.as_ref().map(|l| {
        json!({
            "live": l.live,
            "current": l.current,
            "opening": l.opening,
            "behind": l.behind,
            "ahead": l.ahead,
            "unreadable": l.unreadable,
            "stale_removed": l.stale_removed,
            "oldest_seq": l.oldest_seq,
        })
    });
    let missing: Vec<String> = r.missing.iter().map(ToString::to_string).collect();
    json!({
        "grace_secs": done.grace.as_secs(),
        "held_back": held_back(r),
        "held_by_copy": r.held_by_copy,
        "pack": {
            "present": done.pack_before.is_some() || done.pack_after.size().is_some(),
            "bytes_before": done.pack_before,
            "bytes_after": done.pack_after.size(),
        },
        "marked": {"blobs": r.marked_blobs, "uploads": r.marked_uploads},
        "missing": missing,
        "deleted": {"blobs": s.blobs_deleted, "uploads": s.uploads_deleted, "chunks": s.chunks_deleted},
        "kept": {
            "pinned": s.pinned,
            "young": s.young,
            "unknown_age": s.unknown_age,
            "changed": s.changed,
            "unreadable": s.unreadable,
            "uploads_skipped": s.uploads_skipped,
        },
        "rewrite": {
            "outcome": rewrite.outcome,
            "dead_bytes": rewrite.dead_bytes,
            "frame_bytes": rewrite.frame_bytes,
            "generation": rewrite.generation,
            "frame_bytes_before": rewrite.frame_bytes_before,
            "frame_bytes_after": rewrite.frame_bytes_after,
        },
        "readers": readers,
    })
}

fn bytes(size: Option<u64>) -> String {
    size.map_or_else(|| "none".to_owned(), |b| format!("{b} bytes"))
}

fn write_text(out: &mut dyn Write, done: &Collected) -> Result<()> {
    let r = &done.report;
    let s = &r.sweep;
    let f = rewrite_fields(done);
    let rewrite = match f.outcome {
        "below_ratio" => format!(
            "below the ratio ({} of {} frame bytes dead): not rewritten",
            f.dead_bytes.unwrap_or(0),
            f.frame_bytes.unwrap_or(0)
        ),
        "done" => format!(
            "done (generation {}): {} -> {} frame bytes",
            f.generation.unwrap_or(0),
            f.frame_bytes_before.unwrap_or(0),
            f.frame_bytes_after.unwrap_or(0)
        ),
        "no_pack" => "no pack: nothing to collect".to_owned(),
        "deferred" => "deferred (a pin taken after the plan): unchanged".to_owned(),
        "not_run" => "not run".to_owned(),
        other => other.to_owned(),
    };
    let sweep = if r.held_by_copy {
        "held back by a backup in progress: nothing marked or swept".to_owned()
    } else if held_back(r) {
        "held back by reader leases: nothing deleted".to_owned()
    } else {
        "ran".to_owned()
    };
    let mut rows = vec![
        ("grace", format!("{} s", done.grace.as_secs())),
        (
            "pack",
            format!(
                "{} -> {}",
                bytes(done.pack_before),
                match done.pack_after {
                    PackAfter::Read(size) => bytes(size),
                    PackAfter::Unknown => "unknown".to_owned(),
                }
            ),
        ),
        ("sweep", sweep),
        (
            "marked",
            format!("{} blobs, {} uploads", r.marked_blobs, r.marked_uploads),
        ),
        (
            "deleted",
            format!(
                "{} blobs, {} uploads, {} chunks",
                s.blobs_deleted, s.uploads_deleted, s.chunks_deleted
            ),
        ),
        (
            "kept",
            format!(
                "{} pinned, {} young, {} of unknown age, {} changed, {} unreadable{}",
                s.pinned,
                s.young,
                s.unknown_age,
                s.changed,
                s.unreadable,
                if s.uploads_skipped {
                    ", staged chunks left alone"
                } else {
                    ""
                }
            ),
        ),
        ("rewrite", rewrite),
    ];
    if let Some(l) = &r.readers {
        rows.push((
            "readers",
            format!(
                "{} live: {} current, {} opening, {} behind, {} ahead, {} unreadable; {} stale removed",
                l.live, l.current, l.opening, l.behind, l.ahead, l.unreadable, l.stale_removed
            ),
        ));
    }
    if !r.missing.is_empty() {
        let ids: Vec<String> = r.missing.iter().map(ToString::to_string).collect();
        rows.push(("missing", ids.join(", ")));
    }
    for (key, value) in rows {
        writeln!(out, "{key:<18}{value}")?;
    }
    Ok(())
}

/// `--grace`: a whole number of seconds, minutes, hours or days (`90m`, `24h`, `2d`). Its floor is the GC's to check.
pub fn parse_grace(text: &str) -> Result<Duration, String> {
    let split = text
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map_or(text.len(), |(i, _)| i);
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{text:?} is not <number><unit>, such as 24h (units: s, m, h, d)"))?;
    let per: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("{text:?} has no unit s, m, h or d (such as 24h)")),
    };
    n.checked_mul(per)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{text:?} is too long"))
}

/// Whether a GC failure is corrupt stored bytes (exit status 3): the store's or the engine's own judgement.
pub fn is_corruption(err: &GcError) -> bool {
    match err {
        GcError::Blob(e) => e.is_corruption(),
        GcError::Database(e) => e.is_corruption(),
        GcError::GraceBelowBindBudget { .. } | GcError::InvalidRewriteRatio(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use persia::internal::gc::ReaderLeaseReport;

    use super::*;

    #[test]
    fn a_grace_is_a_whole_number_and_a_unit() {
        assert_eq!(parse_grace("24h"), Ok(Duration::from_hours(24)));
        assert_eq!(parse_grace("90m"), Ok(Duration::from_mins(90)));
        assert_eq!(parse_grace("2d"), Ok(Duration::from_hours(48)));
        assert_eq!(parse_grace("7200s"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_grace("0s"), Ok(Duration::ZERO));
        for bad in [
            "", "h", "24", "24H", "24 h", " 24h", "1.5h", "-1h", "+1h", "24hh", "24w",
        ] {
            assert!(parse_grace(bad).is_err(), "{bad:?}");
        }
        // The largest that fits, and one past it.
        assert_eq!(
            parse_grace(&format!("{}s", u64::MAX)),
            Ok(Duration::from_secs(u64::MAX))
        );
        assert!(parse_grace(&format!("{}d", u64::MAX / 86_400 + 1)).is_err());
        assert!(parse_grace("99999999999999999999s").is_err());
    }

    fn collected(report: GcReport) -> Collected {
        Collected {
            grace: Duration::from_hours(2),
            report,
            pack_before: Some(9000),
            pack_after: PackAfter::Read(Some(9000)),
        }
    }

    /// A sweep held back by a reader lease: the readers' counts, nothing deleted, the rewrite not run.
    #[test]
    fn a_held_back_sweep_says_so() {
        let mut report = GcReport::default();
        report.marked_blobs = 2;
        let mut readers = ReaderLeaseReport::default();
        readers.live = 2;
        readers.current = 1;
        readers.behind = 1;
        readers.oldest_seq = Some(4);
        readers.held_back = true;
        report.readers = Some(readers);
        let done = collected(report);
        assert!(held_back(&done.report));
        assert_eq!(
            to_json(&done).to_string(),
            r#"{"deleted":{"blobs":0,"chunks":0,"uploads":0},"grace_secs":7200,"held_back":true,"held_by_copy":false,"kept":{"changed":0,"pinned":0,"unknown_age":0,"unreadable":0,"uploads_skipped":false,"young":0},"marked":{"blobs":2,"uploads":0},"missing":[],"pack":{"bytes_after":9000,"bytes_before":9000,"present":true},"readers":{"ahead":0,"behind":1,"current":1,"live":2,"oldest_seq":4,"opening":0,"stale_removed":0,"unreadable":0},"rewrite":{"dead_bytes":null,"frame_bytes":null,"frame_bytes_after":null,"frame_bytes_before":null,"generation":null,"outcome":"not_run"}}"#
        );
        let mut out = Vec::new();
        write_text(&mut out, &done).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "grace             7200 s\n\
             pack              9000 bytes -> 9000 bytes\n\
             sweep             held back by reader leases: nothing deleted\n\
             marked            2 blobs, 0 uploads\n\
             deleted           0 blobs, 0 uploads, 0 chunks\n\
             kept              0 pinned, 0 young, 0 of unknown age, 0 changed, 0 unreadable\n\
             rewrite           not run\n\
             readers           2 live: 1 current, 0 opening, 1 behind, 0 ahead, 0 unreadable; 0 stale removed\n"
        );
    }

    /// A pack whose size could not be read after the GC: the report is still written, `bytes_after` `null`.
    #[test]
    fn an_unknown_size_after_is_null_and_unknown() {
        let done = Collected {
            pack_after: PackAfter::Unknown,
            ..collected(GcReport::default())
        };
        let json = to_json(&done);
        assert_eq!(
            json["pack"],
            serde_json::json!({"present": true, "bytes_before": 9000, "bytes_after": null})
        );
        let mut out = Vec::new();
        write_text(&mut out, &done).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("pack              9000 bytes -> unknown\n"),
            "{text}"
        );
    }

    /// A backup's hold: nothing marked or swept, and the text says why.
    #[test]
    fn a_backups_hold_is_reported() {
        let mut report = GcReport::default();
        report.held_by_copy = true;
        let done = collected(report);
        assert!(held_back(&done.report));
        assert_eq!(to_json(&done)["held_by_copy"], true);
        assert_eq!(rewrite_fields(&done).outcome, "not_run");
        let mut out = Vec::new();
        write_text(&mut out, &done).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "sweep             held back by a backup in progress: nothing marked or swept\n"
            ),
            "{text}"
        );
    }

    /// Each rewrite outcome writes its own numbers and `null` for the others; the kept counts and a skipped upload
    /// listing are written as the sweep reported them.
    #[test]
    fn each_rewrite_outcome_writes_its_numbers() {
        let mut report = GcReport::default();
        report.sweep.rewrite = Some(Rewrite::BelowRatio {
            dead_bytes: 10,
            frame_bytes: 100,
        });
        report.sweep.pinned = 1;
        report.sweep.young = 2;
        report.sweep.unknown_age = 3;
        report.sweep.changed = 4;
        report.sweep.unreadable = 5;
        report.sweep.uploads_skipped = true;
        report.sweep.blobs_deleted = 6;
        report.sweep.uploads_deleted = 7;
        report.sweep.chunks_deleted = 8;
        let done = collected(report);
        assert!(!held_back(&done.report));
        assert_eq!(
            rewrite_fields(&done),
            RewriteFields {
                outcome: "below_ratio",
                dead_bytes: Some(10),
                frame_bytes: Some(100),
                ..RewriteFields::default()
            }
        );
        let json = to_json(&done);
        assert_eq!(
            json["kept"],
            serde_json::json!({"pinned": 1, "young": 2, "unknown_age": 3, "changed": 4, "unreadable": 5, "uploads_skipped": true})
        );
        assert_eq!(
            json["deleted"],
            serde_json::json!({"blobs": 6, "uploads": 7, "chunks": 8})
        );
        let mut out = Vec::new();
        write_text(&mut out, &done).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(
                "rewrite           below the ratio (10 of 100 frame bytes dead): not rewritten\n"
            ),
            "{text}"
        );
        assert!(text.contains("kept              1 pinned, 2 young, 3 of unknown age, 4 changed, 5 unreadable, staged chunks left alone\n"), "{text}");
        assert!(text.contains("sweep             ran\n"), "{text}");

        let mut deferred = GcReport::default();
        deferred.sweep.rewrite = Some(Rewrite::Deferred);
        let done = collected(deferred);
        assert_eq!(rewrite_fields(&done).outcome, "deferred");
        let mut out = Vec::new();
        write_text(&mut out, &done).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("rewrite           deferred (a pin taken after the plan): unchanged\n")
        );

        let mut swept = GcReport::default();
        swept.sweep.rewrite = Some(Rewrite::Done {
            generation: 3,
            frame_bytes_before: 500,
            frame_bytes_after: 200,
        });
        let done = collected(swept);
        assert_eq!(
            rewrite_fields(&done),
            RewriteFields {
                outcome: "done",
                generation: Some(3),
                frame_bytes_before: Some(500),
                frame_bytes_after: Some(200),
                ..RewriteFields::default()
            }
        );
        // A sweep that reported no rewrite at all.
        assert_eq!(
            rewrite_fields(&collected(GcReport::default())).outcome,
            "not_run"
        );
        assert_eq!(
            to_json(&Collected {
                pack_before: None,
                pack_after: PackAfter::Read(None),
                ..collected(GcReport::default())
            })["pack"],
            serde_json::json!({"present": false, "bytes_before": null, "bytes_after": null})
        );
    }

    /// Only a missing file is "no pack": any other failure to read its size is an error, never taken for no pack.
    #[test]
    fn only_a_missing_pack_has_no_size() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.persia.blobs");
        assert_eq!(pack_size(&file).unwrap(), None);
        std::fs::write(&file, b"12345").unwrap();
        assert_eq!(pack_size(&file).unwrap(), Some(5));
        // A path through a regular file: not a directory, not missing.
        assert!(pack_size(&file.join("inside")).is_err());
    }

    #[test]
    fn only_corrupt_stored_bytes_are_corruption() {
        let floor = GcError::GraceBelowBindBudget {
            grace: Duration::ZERO,
            floor: Duration::from_hours(2),
        };
        assert!(!is_corruption(&floor));
        assert!(!is_corruption(&GcError::InvalidRewriteRatio(f64::NAN)));
        assert!(is_corruption(&GcError::from(DatabaseError::CorruptTail {
            offset: 1
        })));
        assert!(!is_corruption(&GcError::from(DatabaseError::Poisoned)));
        assert!(!is_corruption(&GcError::Blob(
            persia::internal::gc::BlobError::PackLocked
        )));
    }
}
