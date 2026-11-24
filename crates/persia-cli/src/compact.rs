//! `persia compact` (ROADMAP 10.4.1): the container compaction of SPEC §4.6, on request.
//!
//! The command opens the database read-write, as any writer does (it takes the writer lock, and recovery truncates a
//! torn tail, SPEC §8.6), so it fails while another writer has the database open. It then runs
//! [`Database::compact`]: the live frames are rewritten into a new file that is renamed over the container, so the
//! path names a complete database at every instant, and a crash leaves the old file and a stale temporary file
//! (`persia container sweep` removes it). It commits no document of its own: the new file holds the last committed
//! snapshot, with `seq` one higher than it. That snapshot is the open's: as with any writer's open, recovery may flush
//! a WAL backlog larger than the default memtable (64 MiB; the command cannot know the application's) into a commit, or
//! retire a cut commit (SPEC §8.6), so `seq` may rise by more than one over the file's last commit. Otherwise the WAL
//! records after the last commit stay in the WAL.
//!
//! What the compaction leaves alone (SPEC §4.8, §9.5):
//! - **The blob pack.** `<name>.persia.blobs` is not opened, read or rewritten. Its dead space is reclaimed by the blob
//!   garbage collection of the writing application, not here.
//! - **Readers.** A read-only handle of another process keeps reading the old file until it refreshes (SPEC §4.7). The
//!   new `seq` puts it behind, which holds a writer's blob GC back until that reader refreshes or closes.
//! - **Copies in progress.** A copy taken with the files in use must not span a compaction: do not run this while
//!   such a backup runs.
//!
//! The command has the built-in analyzers only. A schema that names an analyzer registered by the application
//! (SPEC §6.2.2) fails the read-write open (exit status 1): such a database is compacted by its application.
//!
//! The text output is `key  value` rows. `--json` writes one object on one line, with these keys and no others:
//! `{"seq": .., "before": {"bytes": .., "live_bytes": .., "dead_bytes": ..}, "after": {..}, "reclaimed_bytes": ..}`,
//! where `bytes` is the end of the container's valid frames (SPEC §4.6 "Space accounting").

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use persia_engine::db::{Compacted, Database, DatabaseError, DatabaseOptions};
use persia_storage::container::{Error as ContainerError, SpaceUsage};
use serde_json::{Value, json};

/// Compacts the container at `path` and writes the space before and after to `out`, as JSON with `json`.
pub fn compact(out: &mut dyn Write, path: &Path, json: bool) -> Result<()> {
    let done = run(path)?;
    if json {
        writeln!(out, "{}", to_json(&done))?;
    } else {
        write_text(out, &done)?;
    }
    Ok(())
}

/// Opens the database at `path` read-write, compacts its container and closes it.
fn run(path: &Path) -> Result<Compacted> {
    // A read-write open of a missing path must not be mistaken for a database to create.
    if !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    let mut db = match Database::open(path, DatabaseOptions::default()) {
        Ok(db) => db,
        Err(DatabaseError::Container(ContainerError::Locked)) => {
            bail!(
                "{} is open by a writer; compact it once that is closed",
                path.display()
            )
        }
        Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
    };
    db.compact().map_err(|err| failed(path, err))
}

/// The error of a failed [`Database::compact`] of `path`. When only the directory sync after the rename failed, the
/// new file may already be what the path names: the message says what to do next.
fn failed(path: &Path, err: DatabaseError) -> anyhow::Error {
    let context = if matches!(
        err,
        DatabaseError::Container(ContainerError::CompactedNotDurable { .. })
    ) {
        format!(
            "compacting {} (the new file may already be in place; reopen it or run `persia verify`)",
            path.display()
        )
    } else {
        format!("compacting {}", path.display())
    };
    anyhow::Error::new(err).context(context)
}

fn reclaimed(done: &Compacted) -> u64 {
    done.before.total.saturating_sub(done.after.total)
}

fn space_json(space: SpaceUsage) -> Value {
    json!({"bytes": space.total, "live_bytes": space.live, "dead_bytes": space.dead()})
}

fn to_json(done: &Compacted) -> Value {
    json!({
        "seq": done.seq,
        "before": space_json(done.before),
        "after": space_json(done.after),
        "reclaimed_bytes": reclaimed(done),
    })
}

fn write_text(out: &mut dyn Write, done: &Compacted) -> Result<()> {
    let space = |s: SpaceUsage| format!("{} bytes ({} live, {} dead)", s.total, s.live, s.dead());
    let rows = [
        ("seq", done.seq.to_string()),
        ("before", space(done.before)),
        ("after", space(done.after)),
        ("reclaimed", format!("{} bytes", reclaimed(done))),
    ];
    for (key, value) in rows {
        writeln!(out, "{key:<18}{value}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done() -> Compacted {
        Compacted {
            seq: 9,
            before: SpaceUsage {
                total: 5000,
                live: 3000,
            },
            after: SpaceUsage {
                total: 3200,
                live: 3200,
            },
        }
    }

    #[test]
    fn the_json_object_has_its_documented_shape() {
        assert_eq!(
            to_json(&done()).to_string(),
            r#"{"after":{"bytes":3200,"dead_bytes":0,"live_bytes":3200},"before":{"bytes":5000,"dead_bytes":2000,"live_bytes":3000},"reclaimed_bytes":1800,"seq":9}"#
        );
    }

    #[test]
    fn the_text_rows_state_the_space_before_and_after() {
        let mut out = Vec::new();
        write_text(&mut out, &done()).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "seq               9\n\
             before            5000 bytes (3000 live, 2000 dead)\n\
             after             3200 bytes (3200 live, 0 dead)\n\
             reclaimed         1800 bytes\n"
        );
    }

    /// A compacted file that is not smaller (its new Manifest frame outweighs the dead bytes) reads as nothing
    /// reclaimed, not as an underflow.
    #[test]
    fn a_file_that_did_not_shrink_reclaims_nothing() {
        let mut same = done();
        same.after.total = 5008;
        assert_eq!(reclaimed(&same), 0);
    }
}
