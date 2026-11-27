//! A database opened read-write only for the blob GC's mark (SPEC §9.5 "From the command line", §8.6; ROADMAP 10.4.7;
//! ADR-0069): what `persia gc` opens, with the built-in analyzers only.
//!
//! The open is [`CompactOnlyDatabase`](super::CompactOnlyDatabase)'s, the same function (ADR-0068): the writer's open
//! ([`Database::open`]), in which a `text` field whose analyzer the registry lacks is indexed with a no-token stand-in
//! and listed by [`MarkOnlyDatabase::skipped_analysis`], and whose pre-scan refuses the open with
//! [`DatabaseError::AnalysisSkipped`], before the replay and before anything but the tail repair is written, when the
//! WAL from `wal_start` holds a record the replay would apply to such a field's collection (a pending record, ROADMAP
//! 10.4.8, SPEC §8.6). So such a collection's memtable stays empty, and every record it replays is of another
//! collection.
//!
//! The mark needs no analysis: [`MarkOnlyDatabase::referenced_blobs`] lists the blob references of the stored
//! documents (segments, memtables whole) and of every WAL put from `wal_start` on. The handle offers the mark, the committed seq, and the
//! writer proof with which the blob pack opens read-write for the sweep (ADR-0053), and nothing that writes a
//! document, commits, merges, compacts or changes a schema.
//!
//! ```compile_fail
//! use persia_engine::db::{DatabaseOptions, MarkOnlyDatabase};
//!
//! fn commit(db: &mut MarkOnlyDatabase) {
//!     db.commit(); // no such method: it only marks
//! }
//! ```

use std::collections::HashSet;
use std::fs::File;
use std::path::Path;

use persia_format::BlobId;
use persia_storage::container::{Container, ContainerFile, Mode, WriterProof};

use super::read_only::SkippedAnalysis;
use super::{Database, DatabaseError, DatabaseOptions};

/// A database opened only for the blob GC's mark (module docs): the writer's open, with stand-ins for the analyzers
/// its registry lacks, and no flush of what they indexed.
#[derive(Debug)]
pub struct MarkOnlyDatabase<F = File> {
    db: Database<F>,
}

impl MarkOnlyDatabase<File> {
    /// Opens the database in the container file at `path` read-write, for GC's mark (module docs).
    ///
    /// ```
    /// use persia_engine::db::{Database, DatabaseOptions, MarkOnlyDatabase};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// drop(Database::create(&path, DatabaseOptions::default())?);
    /// let db = MarkOnlyDatabase::open(&path, DatabaseOptions::default())?;
    /// assert!(db.skipped_analysis().is_empty());
    /// assert!(db.referenced_blobs()?.is_empty());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Container::open`] ([`persia_storage::container::Error::Locked`] while another writer holds the lock) and
    /// [`MarkOnlyDatabase::open_container`].
    pub fn open(path: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self, DatabaseError> {
        Self::open_container(Container::open(path, Mode::ReadWrite)?, options)
    }
}

impl<F: ContainerFile + Sync> MarkOnlyDatabase<F> {
    /// [`MarkOnlyDatabase::open`] on a container the caller opened read-write (on a simulated file system in crash
    /// tests). `options.registry` names the analyzers it has; a `text` field whose analyzer it lacks is skipped
    /// (module docs).
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use persia_engine::db::{Database, DatabaseOptions, MarkOnlyDatabase};
    /// use persia_storage::container::{Container, Mode};
    /// use persia_storage::vfs::sim::SimFs;
    ///
    /// let fs = Arc::new(SimFs::new());
    /// let created = Container::create_in(Arc::clone(&fs) as _, "/app.persia")?;
    /// let seq = Database::create_container(created, DatabaseOptions::default())?.seq();
    /// let container = Container::open_in(fs, "/app.persia", Mode::ReadWrite)?;
    /// let db = MarkOnlyDatabase::open_container(container, DatabaseOptions::default())?;
    /// assert_eq!(db.seq(), seq);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`CompactOnlyDatabase::open_container`](super::CompactOnlyDatabase::open_container), whose open this is:
    /// [`DatabaseError::AnalysisSkipped`] when the WAL holds a record the replay would apply to a collection with a
    /// skipped field, before the replay and anything but the tail repair is written; a container opened read-only.
    pub fn open_container(
        container: Container<F>,
        options: DatabaseOptions,
    ) -> Result<Self, DatabaseError> {
        Database::open_skipping_analysis(container, options).map(|db| Self { db })
    }

    /// The `text` fields whose analysis is skipped, in collection then field order: those whose analyzer the
    /// registry lacks. Empty when it has every one: the handle is then the writer's open.
    ///
    /// ```
    /// use persia_engine::analysis::{Analyzer, AnalyzerRegistry, WhitespaceTokenizer};
    /// use persia_engine::db::{Database, DatabaseOptions, MarkOnlyDatabase};
    /// use persia_engine::schema::CollectionSchema;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let mut registry = AnalyzerRegistry::builtin();
    /// registry.register("tags", 1, Analyzer::builder(WhitespaceTokenizer).build())?;
    /// let schema = CollectionSchema::from_json_in(
    ///     r#"{"name": "notes", "fields": {"tags": {"type": "text", "analyzer": "tags"}}}"#,
    ///     &registry,
    /// )?;
    /// let mut app = Database::create(&path, DatabaseOptions { registry, ..DatabaseOptions::default() })?;
    /// app.create_collection(schema)?;
    /// drop(app);
    /// let db = MarkOnlyDatabase::open(&path, DatabaseOptions::default())?;
    /// assert_eq!(db.skipped_analysis()[0].analyzer, "tags@1");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn skipped_analysis(&self) -> &[SkippedAnalysis] {
        self.db.skipped_analysis()
    }

    /// The `seq` of the last commit: the open's. What GC judges reader leases against (SPEC §9.5).
    pub fn seq(&self) -> u64 {
        self.db.seq()
    }

    /// GC's mark of the engine's view: [`Database::referenced_blobs`]. It reads the stored documents and the WAL's
    /// puts, never a term.
    ///
    /// ```
    /// use persia_engine::analysis::{Analyzer, AnalyzerRegistry, WhitespaceTokenizer};
    /// use persia_engine::db::{Database, DatabaseError, DatabaseOptions, MarkOnlyDatabase};
    /// use persia_engine::document::{Document, Value};
    /// use persia_engine::schema::CollectionSchema;
    /// use persia_engine::wal::Durability;
    /// use persia_format::{BlobId, BlobRef};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let mut registry = AnalyzerRegistry::builtin();
    /// registry.register("tags", 1, Analyzer::builder(WhitespaceTokenizer).build())?;
    /// let schema = CollectionSchema::from_json_in(
    ///     r#"{"name": "notes", "fields": {"tags": {"type": "text", "analyzer": "tags"}}}"#,
    ///     &registry,
    /// )?;
    /// let tags = schema.field("tags").ok_or("no tags")?.id();
    /// let app_options = || DatabaseOptions { registry: registry.clone(), ..DatabaseOptions::default() };
    /// let mut app = Database::create(&path, app_options())?;
    /// app.create_collection(schema)?;
    /// let blob = BlobRef::new(BlobId::from_bytes([7; 32]), 3, "text/plain", 1 << 20, Default::default())?;
    /// let mut doc = Document::new("n").with(tags, Value::String("a b".into()));
    /// doc.set_blob_ref(Some(blob.clone()));
    /// app.put("notes", &doc, Durability::Fsync)?; // in the WAL only
    /// drop(app);
    /// // A record of `notes` in the WAL needs `tags@1` to be replayed: the open is refused, writing nothing.
    /// let refused = MarkOnlyDatabase::open(&path, DatabaseOptions::default());
    /// assert!(matches!(refused, Err(DatabaseError::AnalysisSkipped { .. })));
    /// // The application opens and commits; then the mark-only open marks the document's blob.
    /// Database::open(&path, app_options())?.commit()?;
    /// let db = MarkOnlyDatabase::open(&path, DatabaseOptions::default())?;
    /// assert!(db.referenced_blobs()?.contains(&blob.blob_id()));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Database::referenced_blobs`].
    pub fn referenced_blobs(&self) -> Result<HashSet<BlobId>, DatabaseError> {
        self.db.referenced_blobs()
    }

    /// The proof that this handle holds its container's writer lock, with which the blob pack opens read-write for
    /// the sweep and the pack rewrite (SPEC §9.3, ADR-0053): [`Database::writer_proof`].
    pub fn writer_proof(&self) -> WriterProof<'_> {
        self.db.writer_proof()
    }
}

impl<F: ContainerFile + Sync + 'static> MarkOnlyDatabase<F> {
    /// The container's path, when it was opened by one: where the blob pack is, beside it (SPEC §9.3).
    pub fn container_path(&self) -> Option<&Path> {
        self.db.container().path()
    }
}
