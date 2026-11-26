//! A database opened read-write only to compact its container (SPEC §4.6 "From the command line", §8.6; ROADMAP
//! 10.4.4; ADR-0068): what `persia compact` opens, with the built-in analyzers only.
//!
//! The open is the writer's ([`Database::open`]): the writer lock, the container's recovery, the manifest and every
//! segment checked, the whole WAL replayed into memtables, a cut commit retired (SPEC §8.6). A `text` field whose
//! analyzer the registry lacks (one registered by the application, SPEC §6.2.2) is indexed with the no-token stand-in
//! of [`ReadOnlyDatabase::open_skipping_analysis`](super::ReadOnlyDatabase::open_skipping_analysis), and listed by
//! [`CompactOnlyDatabase::skipped_analysis`]. Such a memtable must never become a segment: a commit that would flush a
//! non-empty memtable of a collection with a skipped field fails with [`DatabaseError::AnalysisSkipped`] before
//! anything is written. Every flush flushes every memtable, so the open fails when its replay would flush any
//! collection while such a collection holds a replayed record: a memtable at the size threshold (for such a
//! collection, measured on the stand-in memtable, which holds none of the skipped field's terms) or out of docids.
//! Otherwise those records stay WAL records, which the compaction copies and the next writer, with the analyzer,
//! replays. After a refusal, open the database with the analyzer and commit (an open alone may flush nothing).
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
    /// ```
    /// use std::sync::Arc;
    ///
    /// use persia_engine::db::{CompactOnlyDatabase, Database, DatabaseOptions};
    /// use persia_storage::container::{Container, Mode};
    /// use persia_storage::vfs::sim::SimFs;
    ///
    /// let fs = Arc::new(SimFs::new());
    /// let created = Container::create_in(Arc::clone(&fs) as _, "/app.persia")?;
    /// drop(Database::create_container(created, DatabaseOptions::default())?);
    /// let container = Container::open_in(fs, "/app.persia", Mode::ReadWrite)?;
    /// let mut db = CompactOnlyDatabase::open_container(container, DatabaseOptions::default())?;
    /// db.compact()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`DatabaseError::Container`] with [`container::Error::ReadOnly`] for a container opened read-only;
    /// [`DatabaseError::AnalysisSkipped`] when the replay would flush (any collection: every flush flushes every
    /// memtable) while a collection with a skipped field holds a replayed record, before that commit writes anything;
    /// [`DatabaseError::UnregisteredAnalyzer`] for a version of a built-in analyzer this build lacks; otherwise as
    /// [`Database::open_container`].
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
    ///
    /// ```
    /// use persia_engine::analysis::{Analyzer, AnalyzerRegistry, WhitespaceTokenizer};
    /// use persia_engine::db::{CompactOnlyDatabase, Database, DatabaseOptions};
    /// use persia_engine::schema::CollectionSchema;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// // The application registers `tags@1` and names it in a schema.
    /// let mut registry = AnalyzerRegistry::builtin();
    /// registry.register("tags", 1, Analyzer::builder(WhitespaceTokenizer).build())?;
    /// let schema = CollectionSchema::from_json_in(
    ///     r#"{"name": "notes", "fields": {"tags": {"type": "text", "analyzer": "tags"}}}"#,
    ///     &registry,
    /// )?;
    /// let mut app = Database::create(&path, DatabaseOptions { registry, ..DatabaseOptions::default() })?;
    /// app.create_collection(schema)?;
    /// drop(app);
    /// // A tool with the built-ins only opens it to compact, without analyzing `tags`.
    /// let db = CompactOnlyDatabase::open(&path, DatabaseOptions::default())?;
    /// let skipped = db.skipped_analysis();
    /// assert_eq!(skipped.len(), 1);
    /// assert_eq!((skipped[0].collection.as_str(), skipped[0].field.as_str()), ("notes", "tags"));
    /// assert_eq!(skipped[0].analyzer, "tags@1");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn skipped_analysis(&self) -> &[SkippedAnalysis] {
        self.db.skipped_analysis()
    }

    /// The `seq` of the last commit: the open's, or the compaction's, which is one above it.
    ///
    /// ```
    /// use persia_engine::db::{CompactOnlyDatabase, Database, DatabaseOptions};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let created = Database::create(&path, DatabaseOptions::default())?;
    /// let seq = created.seq();
    /// drop(created);
    /// let mut db = CompactOnlyDatabase::open(&path, DatabaseOptions::default())?;
    /// assert_eq!(db.seq(), seq);
    /// db.compact()?;
    /// assert_eq!(db.seq(), seq + 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn seq(&self) -> u64 {
        self.db.seq()
    }

    /// The container's space (SPEC §4.6 "Space accounting").
    ///
    /// ```
    /// use persia_engine::db::{CompactOnlyDatabase, Database, DatabaseOptions};
    /// use persia_engine::schema::SchemaBuilder;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let mut app = Database::create(&path, DatabaseOptions::default())?;
    /// app.create_collection(SchemaBuilder::new("a").build()?)?;
    /// drop(app);
    /// let mut db = CompactOnlyDatabase::open(&path, DatabaseOptions::default())?;
    /// let before = db.space_usage()?;
    /// assert!(before.dead() > 0, "the first commit's manifest is dead");
    /// let done = db.compact()?;
    /// assert_eq!((done.before, db.space_usage()?), (before, done.after));
    /// assert_eq!(done.after.dead(), 0);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
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
    /// ```
    /// use persia_engine::db::{CompactOnlyDatabase, Database, DatabaseOptions};
    /// use persia_engine::document::Document;
    /// use persia_engine::schema::SchemaBuilder;
    /// use persia_engine::wal::Durability;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let mut app = Database::create(&path, DatabaseOptions::default())?;
    /// app.create_collection(SchemaBuilder::new("items").build()?)?;
    /// for _ in 0..3 {
    ///     app.put("items", &Document::new("a"), Durability::Fsync)?; // replaces "a"
    ///     app.commit()?;
    /// }
    /// app.put("items", &Document::new("b"), Durability::Fsync)?; // in the WAL only
    /// drop(app);
    /// let mut db = CompactOnlyDatabase::open(&path, DatabaseOptions::default())?;
    /// let done = db.compact()?;
    /// assert!(done.after.total < done.before.total);
    /// drop(db);
    /// let reopened = Database::open(&path, DatabaseOptions::default())?;
    /// let snapshot = reopened.snapshot();
    /// let items = snapshot.collection("items").ok_or("no items")?;
    /// assert!(items.get("a")?.is_some() && items.get("b")?.is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Database::compact`].
    pub fn compact(&mut self) -> Result<Compacted, DatabaseError> {
        self.db.compact()
    }
}
