//! A database opened read-write only to compact its container (SPEC §4.6 "From the command line", §8.6; ROADMAP
//! 10.4.4; ADR-0068): what `persia compact` opens, with the built-in analyzers only.
//!
//! The open is the writer's ([`Database::open`]): the writer lock, the container's recovery, the manifest and every
//! segment checked, the whole WAL replayed into memtables, a cut commit retired (SPEC §8.6). A `text` field whose
//! analyzer the registry lacks (one registered by the application, SPEC §6.2.2) is indexed with the no-token stand-in
//! of [`ReadOnlyDatabase::open_skipping_analysis`](super::ReadOnlyDatabase::open_skipping_analysis), and listed by
//! [`CompactOnlyDatabase::skipped_analysis`]. Such a memtable must never become a segment: a commit that would flush a
//! non-empty memtable of a collection with a skipped field fails with [`DatabaseError::AnalysisSkipped`] before
//! anything is written. So the open fails when its replay would flush such a collection's WAL backlog (one over the
//! memtable size, or out of docids); otherwise those records stay WAL records, which the compaction copies and the next
//! writer, with the analyzer, replays.
//!
//! The handle compacts, and does nothing else: it has no call that writes a document, commits, merges or changes a
//! schema.
//!
//! ```compile_fail
//! use persia_engine::db::{CompactOnlyDatabase, DatabaseOptions};
//! use persia_engine::document::Document;
//! use persia_engine::wal::Durability;
//!
//! fn put(db: &mut CompactOnlyDatabase) {
//!     db.put("notes", &Document::new("a"), Durability::Fsync); // no such method: it only compacts
//! }
//! ```

use std::fs::File;
use std::path::Path;

use persia_storage::container::{self, Container, ContainerFile, Mode, SpaceUsage};

use super::read_only::{Analysis, SkippedAnalysis};
use super::{Compacted, Database, DatabaseError, DatabaseOptions};

/// A database opened only to compact its container (module docs): the writer's open, with stand-ins for the analyzers
/// its registry lacks, and no flush of what they indexed.
#[derive(Debug)]
pub struct CompactOnlyDatabase<F = File> {
    db: Database<F>,
}

impl CompactOnlyDatabase<File> {
    /// Opens the database in the container file at `path` read-write, to compact it (module docs).
    ///
    /// ```
    /// use persia_engine::db::{CompactOnlyDatabase, Database, DatabaseOptions};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// drop(Database::create(&path, DatabaseOptions::default())?);
    /// let mut db = CompactOnlyDatabase::open(&path, DatabaseOptions::default())?;
    /// assert!(db.skipped_analysis().is_empty());
    /// let done = db.compact()?;
    /// assert_eq!(done.after.dead(), 0);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Container::open`] ([`container::Error::Locked`] while another writer holds the lock) and
    /// [`CompactOnlyDatabase::open_container`].
    pub fn open(path: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self, DatabaseError> {
        Self::open_container(Container::open(path, Mode::ReadWrite)?, options)
    }
}

impl<F: ContainerFile + Sync> CompactOnlyDatabase<F> {
    /// [`CompactOnlyDatabase::open`] on a container the caller opened read-write and recovered (on a simulated file
    /// system in crash tests). `options.registry` names the analyzers it has; a `text` field whose analyzer it lacks is
    /// skipped (module docs).
    ///
    /// # Errors
    /// [`DatabaseError::Container`] with [`container::Error::ReadOnly`] for a container opened read-only;
    /// [`DatabaseError::AnalysisSkipped`] when the replay would flush a memtable of a collection with a skipped field,
    /// before anything is written; [`DatabaseError::UnregisteredAnalyzer`] for a version of a built-in analyzer this
    /// build lacks; otherwise as [`Database::open_container`].
    pub fn open_container(
        container: Container<F>,
        options: DatabaseOptions,
    ) -> Result<Self, DatabaseError> {
        if container.mode() != Mode::ReadWrite {
            return Err(container::Error::ReadOnly {
                reason: container::ReadOnlyReason::OpenedReadOnly,
            }
            .into());
        }
        let db = Database::recover_as(container, options, Analysis::SkipUnregistered)?;
        Ok(Self { db })
    }

    /// The `text` fields whose analysis is skipped, in collection then field order: those whose analyzer the
    /// registry lacks. Empty when it has every one: the handle is then the writer's open, and refuses nothing.
    pub fn skipped_analysis(&self) -> &[SkippedAnalysis] {
        self.db.skipped_analysis()
    }

    /// The `seq` of the last commit: the open's, or the compaction's.
    pub fn seq(&self) -> u64 {
        self.db.seq()
    }

    /// The container's space (SPEC §4.6 "Space accounting").
    ///
    /// # Errors
    /// As [`Database::space_usage`].
    pub fn space_usage(&self) -> Result<SpaceUsage, DatabaseError> {
        self.db.space_usage()
    }
}

impl<F: ContainerFile + Sync + 'static> CompactOnlyDatabase<F> {
    /// [`Database::compact`]: rewrites the container without its dead frames (SPEC §4.6). It copies frames without
    /// reading their payloads, so it needs no analyzer; the WAL from `wal_start` on is kept as it is.
    ///
    /// # Errors
    /// As [`Database::compact`].
    pub fn compact(&mut self) -> Result<Compacted, DatabaseError> {
        self.db.compact()
    }
}
