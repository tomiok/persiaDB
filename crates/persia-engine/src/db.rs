//! A database on a local container (SPEC §4, §8): the write path from `put` to a published commit.
//!
//! [`Database`] is the single writer (SPEC §8.4). An operation is checked by its collection's memtable, logged to
//! the WAL, and then made visible (SPEC §8.2.1 "What may be logged", ROADMAP 6.3.9). A commit flushes memtables into
//! `Segment` frames, appends the Manifest frame that lists them and writes the superblock slot (SPEC §4.5,
//! ROADMAP 6.3.1, 6.3.2), then publishes a new [`Snapshot`] to readers (ROADMAP 6.3.3). Commits happen on
//! [`Database::commit`], or when the [`CommitPolicy`] says so ([`Database::tick`], ROADMAP 6.3.4).
//!
//! Opening a database recovers the container, checks the committed manifest against the superblock (ROADMAP
//! 6.3.7), opens the segments through [`ContainerObjects`] (ROADMAP 6.3.6) and replays the WAL into fresh
//! memtables (SPEC §8.6).
//!
//! ```
//! use persia_engine::db::{Database, DatabaseOptions};
//! use persia_engine::document::{Document, Value};
//! use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
//! use persia_engine::wal::Durability;
//! use persia_storage::container::sim::{CrashLoss, SimFile};
//!
//! let schema = SchemaBuilder::new("notes")
//!     .field("body", FieldKind::Text(TextOptions::default()))
//!     .build()?;
//! let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
//! let file = SimFile::new();
//! let mut db = Database::create_on(file.clone(), [7; 16], DatabaseOptions::default())?;
//! db.create_collection(schema)?;
//! db.put("notes", &Document::new("n1").with(body, Value::String("hello".into())), Durability::Fsync)?;
//! let reader = db.reader();
//! assert_eq!(reader.snapshot().collection("notes").map(|c| c.memtable().live_count()), Some(1));
//!
//! assert_eq!(db.commit()?, 3); // create, collection, flush
//! let notes = reader.snapshot();
//! let notes = notes.collection("notes").ok_or("no collection")?;
//! assert_eq!((notes.segments().len(), notes.memtable().live_count()), (1, 0));
//! drop(db);
//!
//! let db = Database::open_on(file.crash(CrashLoss::All), DatabaseOptions::default())?;
//! assert_eq!(db.snapshot().collection("notes").map(|c| c.segments()[0].doc_count()), Some(1));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod clock;
mod flush;
mod merging;
mod objects;
mod obsolete;
mod policy;
mod snapshot;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use persia_analysis::AnalyzerRegistry;
use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_format::source::ByteSource;
use persia_format::superblock::Superblock;
use persia_storage::Storage;
use persia_storage::container::{self, Commit, Container, ContainerFile, Mode};
use roaring::RoaringBitmap;

pub use clock::{Clock, ManualClock, SegmentIds, SystemClock};
pub use merging::MergeOutcome;
pub use objects::ContainerObjects;
use obsolete::Pin;
pub use obsolete::{ObsoleteKind, ObsoleteObject};
pub use policy::{CommitPolicy, CommitReason, MemtableState};
pub use snapshot::{CollectionSnapshot, DatabaseReader, Located, Snapshot};

use crate::document::Document;
use crate::manifest::{
    ApplyError, CollectionChange, CollectionDiff, Layout, LayoutError, Location, MAX_MANIFEST_LEN,
    Manifest, ManifestDiff, ManifestEncodeError, ManifestError, SegmentEntry, SegmentId,
};
use crate::memtable::{Logged, Memtable, MemtableError};
use crate::merge::MergeOptions;
use crate::schema::CollectionSchema;
use crate::segment::{ReadError, Segment, SegmentError, SegmentMeta, Tombstones, WriteOptions};
use crate::tombstone::{SidecarError, TombstoneSidecar};
use crate::view::{SearchView, SegmentView};
use crate::wal::{
    DEFAULT_CHUNK_TARGET, Durability, RecordError, ReplayError, WalMark, WalRecord, WalWriteError,
    WalWriter, replay_container,
};

/// Why a database operation failed.
#[derive(Debug, thiserror::Error)]
pub enum DatabaseError {
    /// The container failed; after a failed write or sync the database is poisoned.
    #[error("container: {0}")]
    Container(#[from] container::Error),
    /// Reading a committed object failed ([`ContainerObjects`]).
    #[error(transparent)]
    Storage(#[from] persia_storage::Error),
    /// The committed manifest does not decode.
    #[error("manifest: {0}")]
    Manifest(#[from] ManifestError),
    /// The next manifest does not encode (it would exceed its size limit).
    #[error(transparent)]
    ManifestEncode(#[from] ManifestEncodeError),
    /// A commit does not apply to the current manifest: a bug, since the database plans every commit itself.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// The committed manifest holds an object location (SPEC §8.7.1).
    #[error(transparent)]
    Layout(#[from] LayoutError),
    /// The committed manifest disagrees with the superblock slot that commits it (SPEC §8.7, ROADMAP 6.3.7).
    #[error("the manifest's {field} is {manifest}, the superblock's {superblock}")]
    SlotMismatch {
        /// `seq`, `fence_token` or `wal_start`.
        field: &'static str,
        /// The manifest's value.
        manifest: u64,
        /// The slot's value.
        superblock: u64,
    },
    /// A committed segment does not open, or is not the segment its manifest entry describes.
    #[error("collection {collection:?}, segment {id}: {source}")]
    OpenSegment {
        /// The collection.
        collection: String,
        /// The segment.
        id: SegmentId,
        /// What was wrong.
        source: ReadError,
    },
    /// A committed segment's footer disagrees with its manifest entry (corruption).
    #[error(
        "collection {collection:?}, segment {id}: the footer's {field} disagrees with the manifest"
    )]
    SegmentMismatch {
        /// The collection.
        collection: String,
        /// The segment.
        id: SegmentId,
        /// `segment_id` or `doc_count`.
        field: &'static str,
    },
    /// Reading a committed segment failed while a commit was planned: nothing was written.
    #[error("collection {collection:?}, segment {id}: {source}")]
    ReadSegment {
        /// The collection.
        collection: String,
        /// The segment.
        id: SegmentId,
        /// What was wrong.
        source: ReadError,
    },
    /// A committed tombstone sidecar does not decode, or a planned one breaks a rule of SPEC §8.3 (a bug: nothing
    /// was written then).
    #[error("collection {collection:?}, segment {id}: {source}")]
    Sidecar {
        /// The collection.
        collection: String,
        /// The segment.
        id: SegmentId,
        /// What was wrong.
        source: SidecarError,
    },
    /// A committed tombstone sidecar disagrees with its manifest entry (corruption).
    #[error(
        "collection {collection:?}, segment {id}: the tombstone sidecar's {field} disagrees with the manifest"
    )]
    SidecarMismatch {
        /// The collection.
        collection: String,
        /// The segment.
        id: SegmentId,
        /// `segment_id`, `version`, `doc_count` or `deleted`.
        field: &'static str,
    },
    /// Building a segment failed: nothing was written.
    #[error(transparent)]
    Segment(#[from] SegmentError),
    /// The memtable rejected the operation; it was not logged.
    #[error(transparent)]
    Memtable(#[from] MemtableError),
    /// Document `index` of a batch was rejected; the documents before it are applied and logged with the batch's
    /// durability, the rest are not.
    #[error("document {index} of the batch: {source}")]
    BatchRejected {
        /// Its position in the batch.
        index: usize,
        /// Why.
        source: MemtableError,
    },
    /// The operation cannot be a WAL record; it was not logged.
    #[error(transparent)]
    Record(#[from] RecordError),
    /// Writing the WAL failed; the database is poisoned.
    #[error(transparent)]
    Wal(#[from] WalWriteError<container::Error>),
    /// Replaying the WAL failed: the open fails.
    #[error(transparent)]
    Replay(#[from] ReplayError),
    /// No committed collection has the name.
    #[error("no collection {name:?}")]
    NoSuchCollection {
        /// The name.
        name: String,
    },
    /// A collection of that name exists.
    #[error("collection {name:?} exists")]
    CollectionExists {
        /// The name.
        name: String,
    },
    /// An invariant of the database did not hold: a bug, reported rather than acted on. Found while a commit is
    /// planned, nothing was written; found later, the database is poisoned.
    #[error("internal error: {what}")]
    Internal {
        /// What did not hold.
        what: &'static str,
    },
    /// A merge worker thread could not be started; nothing was merged.
    #[error("starting a merge thread: {0}")]
    MergeThread(#[source] std::io::Error),
    /// An earlier write, sync or commit failed, so what reached the disk is unknown: reopen the database.
    #[error("an earlier write failed; reopen the database to recover")]
    Poisoned,
}

impl DatabaseError {
    /// Whether stored bytes are corrupt.
    pub fn is_corruption(&self) -> bool {
        match self {
            Self::Container(e) => e.is_corruption(),
            Self::Storage(e) => e.is_corruption(),
            Self::Manifest(e) => e.is_corruption(),
            Self::OpenSegment { source, .. } | Self::ReadSegment { source, .. } => {
                source.is_corruption()
            }
            Self::Sidecar { source, .. } => source.is_corruption(),
            Self::Replay(e) => e.is_corruption(),
            Self::Layout(_)
            | Self::SlotMismatch { .. }
            | Self::SegmentMismatch { .. }
            | Self::SidecarMismatch { .. } => true,
            _ => false,
        }
    }
}

/// How a [`Database`] is opened. No option is stored in the container.
#[derive(Debug)]
pub struct DatabaseOptions {
    /// When the database commits on its own.
    pub policy: CommitPolicy,
    /// The analyzers of `text` fields; the built-in ones by default.
    pub registry: AnalyzerRegistry,
    /// Where time is read.
    pub clock: Arc<dyn Clock>,
    /// Where new segment ids come from.
    pub segment_ids: SegmentIds,
    /// How segments are encoded.
    pub write_options: WriteOptions,
    /// The size at which the WAL writer closes a chunk (SPEC §8.2.1).
    pub wal_chunk_target: usize,
    /// How segments are merged in the background (SPEC §8.5).
    pub merge: MergeOptions,
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        Self {
            policy: CommitPolicy::default(),
            registry: AnalyzerRegistry::builtin(),
            clock: Arc::new(SystemClock),
            segment_ids: SegmentIds::random(),
            write_options: WriteOptions::default(),
            wal_chunk_target: DEFAULT_CHUNK_TARGET,
            merge: MergeOptions::default(),
        }
    }
}

/// A committed segment and its current deletions (SPEC §8.3).
#[derive(Debug, Clone)]
struct LiveSegment {
    segment: Arc<Segment>,
    /// Its current tombstone sidecar's version and deleted docids; `None` while it has none.
    deleted: Option<(u64, Arc<Tombstones>)>,
}

impl LiveSegment {
    /// The segment without its deleted documents.
    fn view(&self) -> SegmentView<'_> {
        SegmentView::new(&self.segment, self.deleted.as_ref().map(|(_, t)| &**t))
    }
}

/// The writer's state of one committed collection.
#[derive(Debug)]
struct Collection {
    schema: Arc<CollectionSchema>,
    segments: Vec<LiveSegment>,
    memtable: Memtable,
    /// When the memtable's oldest unflushed operation was applied; `None` while it is empty.
    first_write: Option<Instant>,
    /// When the memtable was last written; `None` while it is empty.
    last_write: Option<Instant>,
}

/// A memtable flush planned before anything is written.
#[derive(Debug)]
struct Planned {
    collection: String,
    /// Where the memtable stopped taking writes.
    mark: WalMark,
    /// Its segment, unless it has no live document.
    segment: Option<(SegmentId, flush::Flushed)>,
    /// The next tombstone sidecar of each committed segment it deletes from (SPEC §8.3).
    sidecars: Vec<PlannedSidecar>,
    /// The empty memtable that replaces it once the commit is durable.
    memtable: Memtable,
}

/// The next tombstone sidecar of a committed segment, encoded before anything is written.
#[derive(Debug)]
struct PlannedSidecar {
    /// The segment's index in its collection's segments.
    index: usize,
    sidecar: TombstoneSidecar,
    bytes: Vec<u8>,
}

/// The single writer of a database on a local container (see the module docs).
#[derive(Debug)]
pub struct Database<F = File> {
    wal: WalWriter<Container<F>>,
    manifest: Arc<Manifest>,
    collections: BTreeMap<String, Collection>,
    published: Arc<ArcSwap<Snapshot>>,
    options: DatabaseOptions,
    /// Merges in flight, segments they failed on, and objects merges and commits made obsolete.
    merges: merging::Merges,
    poisoned: bool,
}

impl Database<File> {
    /// Creates a database in a new container file at `path` (SPEC §4.5.1), which must not exist.
    ///
    /// # Errors
    /// As [`Container::create`] and [`Database::create_on`].
    pub fn create(path: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self, DatabaseError> {
        Self::created(Container::create(path)?, options)
    }

    /// Opens the database in the container file at `path`, read-write.
    ///
    /// # Errors
    /// As [`Container::open`] and [`Database::open_on`].
    pub fn open(path: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self, DatabaseError> {
        Self::recover(Container::open(path, Mode::ReadWrite)?, options)
    }
}

impl<F: ContainerFile + Sync> Database<F> {
    /// Creates a database in `file`, which must be empty, with id `db_uuid`, and commits its first, empty manifest
    /// (`seq` 1).
    ///
    /// # Errors
    /// [`DatabaseError::Container`] if the container cannot be created, or the first commit fails.
    pub fn create_on(
        file: F,
        db_uuid: [u8; 16],
        options: DatabaseOptions,
    ) -> Result<Self, DatabaseError> {
        Self::created(Container::create_on(file, db_uuid)?, options)
    }

    /// Opens the database in `file`, read-write (SPEC §8.6): recovers the container, reads the committed manifest
    /// and checks it against the superblock (ROADMAP 6.3.7), opens every segment, and replays the WAL.
    ///
    /// # Errors
    /// A corruption ([`DatabaseError::is_corruption`]) for a manifest, segment or WAL record that breaks the
    /// format, or disagrees with the superblock; [`DatabaseError::Container`] for an unusable container;
    /// [`DatabaseError::Replay`] for a record replay cannot apply.
    pub fn open_on(file: F, options: DatabaseOptions) -> Result<Self, DatabaseError> {
        Self::recover(Container::open_on(file, Mode::ReadWrite)?, options)
    }

    /// [`Database::open_on`] for a container the caller opened read-write and recovered, e.g. with
    /// [`Repair::Quarantine`](persia_storage::container::Repair::Quarantine) for a corrupt tail (ADR-0014), or with
    /// mapped reads enabled (SPEC §4.7).
    ///
    /// # Errors
    /// [`DatabaseError::Container`] with [`container::Error::ReadOnly`] for a container opened read-only; otherwise
    /// as [`Database::open_on`].
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
        Self::recover(container, options)
    }

    fn created(container: Container<F>, options: DatabaseOptions) -> Result<Self, DatabaseError> {
        let wal = WalWriter::resume(container, BTreeMap::new())
            .with_chunk_target(options.wal_chunk_target);
        let mut db = Self::assemble(wal, Manifest::empty(), BTreeMap::new(), options);
        db.commit_guarded(Vec::new(), ManifestDiff::default(), None)?;
        Ok(db)
    }

    fn recover(container: Container<F>, options: DatabaseOptions) -> Result<Self, DatabaseError> {
        let sb = *container.superblock();
        let manifest = if sb.seq == 0 {
            // Created, but the first commit did not complete.
            Manifest::empty()
        } else {
            let frame = container.read_frame_source(sb.manifest_off, MAX_MANIFEST_LEN)?;
            let manifest = Manifest::decode(&frame.payload)?;
            check_slot(&manifest, &sb)?;
            manifest
        };
        manifest.check_layout(Layout::Container)?;
        let mut collections = BTreeMap::new();
        let mut memtables = BTreeMap::new();
        {
            let objects = ContainerObjects::new(&container, &manifest);
            for entry in manifest.collections() {
                let name = entry.name();
                let schema = Arc::new(entry.schema().clone());
                let mut segments = Vec::with_capacity(entry.segments().len());
                for segment in entry.segments() {
                    segments.push(open_segment(&objects, name, segment, &schema)?);
                }
                memtables.insert(name.to_owned(), Memtable::new(&schema, &options.registry)?);
                collections.insert(name.to_owned(), (schema, segments));
            }
        }
        let summary = replay_container(&container, &mut memtables)?;
        let now = options.clock.now();
        let mut states = BTreeMap::new();
        for (name, (schema, segments)) in collections {
            let memtable = match memtables.remove(&name) {
                Some(m) => m,
                None => Memtable::new(&schema, &options.registry)?,
            };
            let last_write = (!memtable.is_empty()).then_some(now);
            let state = Collection {
                schema,
                segments,
                memtable,
                first_write: last_write,
                last_write,
            };
            states.insert(name, state);
        }
        let wal = WalWriter::resume(container, summary.pending)
            .with_chunk_target(options.wal_chunk_target);
        Ok(Self::assemble(wal, manifest, states, options))
    }

    fn assemble(
        wal: WalWriter<Container<F>>,
        manifest: Manifest,
        collections: BTreeMap<String, Collection>,
        options: DatabaseOptions,
    ) -> Self {
        let manifest = Arc::new(manifest);
        let snapshot = snapshot_of(&manifest, &collections);
        Self {
            wal,
            manifest,
            collections,
            published: Arc::new(ArcSwap::from_pointee(snapshot)),
            merges: merging::Merges::new(options.merge.threads),
            options,
            poisoned: false,
        }
    }

    /// The committed manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The `seq` of the last commit.
    pub fn seq(&self) -> u64 {
        self.manifest.seq()
    }

    /// The container (read-only).
    pub fn container(&self) -> &Container<F> {
        self.wal.log()
    }

    /// The latest published snapshot.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.published.load_full()
    }

    /// A handle that loads the latest published snapshot from any thread.
    pub fn reader(&self) -> DatabaseReader {
        DatabaseReader {
            current: Arc::clone(&self.published),
        }
    }

    /// The committed objects of the container, as [`Storage`] (ROADMAP 6.3.6).
    pub fn objects(&self) -> ContainerObjects<'_, F> {
        ContainerObjects::new(self.wal.log(), &self.manifest)
    }

    fn usable(&self) -> Result<(), DatabaseError> {
        if self.poisoned {
            Err(DatabaseError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Creates the collection `schema` describes, committing it at once: a record of a collection is logged only
    /// once its create is committed (SPEC §8.2.1). Returns the commit's `seq`.
    ///
    /// # Errors
    /// [`DatabaseError::CollectionExists`]; [`DatabaseError::Memtable`] if the registry lacks one of its analyzers;
    /// a failed commit (which poisons the database).
    pub fn create_collection(&mut self, schema: CollectionSchema) -> Result<u64, DatabaseError> {
        self.usable()?;
        let name = schema.name().to_owned();
        if self.collections.contains_key(&name) {
            return Err(DatabaseError::CollectionExists { name });
        }
        let memtable = Memtable::new(&schema, &self.options.registry)?;
        let mut diff = ManifestDiff::default();
        let create = CollectionDiff::Create {
            schema: schema.clone(),
            segments: Vec::new(),
        };
        diff.collections.insert(name.clone(), create);
        let collection = Collection {
            schema: Arc::new(schema),
            segments: Vec::new(),
            memtable,
            first_write: None,
            last_write: None,
        };
        self.commit_guarded(Vec::new(), diff, Some((name, collection)))
    }

    /// Adds or replaces the document `doc` of `collection` (SPEC §8.1), with `durability` (SPEC §8.2.1). A memtable
    /// at its size threshold is flushed and committed first.
    ///
    /// The new version hides the earlier ones at once; the next commit tombstones the committed one (SPEC §8.3).
    ///
    /// # Errors
    /// [`DatabaseError::NoSuchCollection`], [`DatabaseError::Memtable`] or [`DatabaseError::Record`] for an operation
    /// that is rejected (and not logged), [`DatabaseError::Wal`] (which poisons the database), or a failed commit.
    pub fn put(
        &mut self,
        collection: &str,
        doc: &Document,
        durability: Durability,
    ) -> Result<(), DatabaseError> {
        self.put_batch(collection, std::slice::from_ref(doc), durability)
            .map_err(|e| match e {
                DatabaseError::BatchRejected { source, .. } => DatabaseError::Memtable(source),
                other => other,
            })
    }

    /// [`Database::put`] of every document of `docs` in order, reaching `durability` once for all of them (one
    /// sync for `Fsync`, SPEC §8.2.1). The last document is logged with `durability` and becomes visible once it is
    /// durable, like a single `put`; the ones before it are visible once buffered, so a reader may see them before
    /// the batch is durable (a failed sync then poisons the database, and a reopen decides what survived). The size
    /// threshold is checked before each document, so a batch may commit part of itself.
    ///
    /// # Errors
    /// As [`Database::put`]; a rejected document is [`DatabaseError::BatchRejected`], after the documents before it
    /// are made durable.
    pub fn put_batch(
        &mut self,
        collection: &str,
        docs: &[Document],
        durability: Durability,
    ) -> Result<(), DatabaseError> {
        self.usable()?;
        if !self.collections.contains_key(collection) {
            return Err(no_such(collection));
        }
        let last = docs.len().saturating_sub(1);
        for (index, doc) in docs.iter().enumerate() {
            let level = if index == last {
                durability
            } else {
                Durability::Buffered
            };
            self.flush_if_full(collection)?;
            let Self {
                wal, collections, ..
            } = &mut *self;
            let state = collections
                .get_mut(collection)
                .ok_or_else(|| no_such(collection))?;
            let logged = state.memtable.put_logged(doc, |encoded| {
                let record = WalRecord::put(collection, encoded)?;
                wal.append(&record, level).map_err(DatabaseError::from)
            });
            match self.written(collection, logged.map(drop)) {
                Ok(()) => {}
                Err(DatabaseError::Memtable(source)) => {
                    self.reach(durability)?;
                    return Err(DatabaseError::BatchRejected { index, source });
                }
                // The documents before it are durable at `durability`, unless the WAL failed (then nothing is).
                Err(e) if !self.poisoned => {
                    self.reach(durability)?;
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Deletes the document with id `id` from `collection` (SPEC §8.1), with `durability`. The delete hides every
    /// version of the id at once; the next commit tombstones the committed ones (SPEC §8.3). Deleting an id the
    /// collection does not have is not an error.
    ///
    /// # Errors
    /// As [`Database::put`].
    pub fn delete(
        &mut self,
        collection: &str,
        id: &str,
        durability: Durability,
    ) -> Result<(), DatabaseError> {
        self.usable()?;
        self.flush_if_full(collection)?;
        let Self {
            wal, collections, ..
        } = &mut *self;
        let state = collections
            .get_mut(collection)
            .ok_or_else(|| no_such(collection))?;
        let logged = state.memtable.delete_logged(id, || {
            let record = WalRecord::delete(collection, id)?;
            wal.append(&record, durability).map_err(DatabaseError::from)
        });
        self.written(collection, logged.map(drop))
    }

    /// Records the outcome of a logged operation on `collection`: its write times when it applied, poisoning when the
    /// WAL failed. A rejection becomes [`DatabaseError::Memtable`].
    fn written(
        &mut self,
        collection: &str,
        logged: Result<(), Logged<DatabaseError>>,
    ) -> Result<(), DatabaseError> {
        match logged {
            Ok(()) => {
                let now = self.options.clock.now();
                if let Some(c) = self.collections.get_mut(collection) {
                    c.first_write.get_or_insert(now);
                    c.last_write = Some(now);
                }
                Ok(())
            }
            Err(Logged::Rejected(e)) => Err(e.into()),
            Err(Logged::Log(e)) => {
                if matches!(e, DatabaseError::Wal(_)) {
                    self.poisoned = true;
                }
                Err(e)
            }
        }
    }

    /// Makes the records logged so far durable at `durability`.
    fn reach(&mut self, durability: Durability) -> Result<(), DatabaseError> {
        // An empty batch reaches the level for what is buffered.
        let result = self.wal.append_batch(&[], durability);
        if result.is_err() {
            self.poisoned = true;
        }
        Ok(result?)
    }

    /// Commits if `collection`'s memtable reached the size threshold (SPEC §8.2 step 3).
    fn flush_if_full(&mut self, collection: &str) -> Result<(), DatabaseError> {
        let full = self.collections.get(collection).is_some_and(|c| {
            c.memtable.size_in_bytes() >= self.options.policy.flush.memtable_bytes
        });
        if full {
            self.commit()?;
        }
        Ok(())
    }

    /// Flushes every memtable that holds operations and commits (SPEC §4.5); returns the `seq` of the committed
    /// state, unchanged if there was nothing to flush.
    ///
    /// Every flush commit flushes every collection with operations, whatever triggered it (ADR-0029): `wal_start`
    /// then reaches the end of the log, so no record already in a segment is replayed into a memtable and flushed a
    /// second time after a crash.
    ///
    /// # Errors
    /// Found while the commit is planned, before anything is written (the database is unchanged and usable):
    /// [`DatabaseError::Segment`] if a segment cannot be built, [`DatabaseError::ReadSegment`] if a committed one
    /// cannot be read to find the documents to tombstone (a corrupt `IdMap`: every commit touching that id fails
    /// until the segment is repaired), [`DatabaseError::Sidecar`] if a sidecar cannot follow the current one. Any
    /// other error poisons the database: the commit may or may not have happened, and a reopen tells.
    pub fn commit(&mut self) -> Result<u64, DatabaseError> {
        self.usable()?;
        // Build every segment and sidecar before writing anything, so a failure here leaves the database as it was.
        let mut planned = Vec::new();
        for (name, c) in &self.collections {
            if c.memtable.is_empty() {
                continue;
            }
            let merges = &self.merges;
            let taken = |id| {
                merges.reserves(id)
                    || planned
                        .iter()
                        .any(|p: &Planned| p.segment.as_ref().is_some_and(|(s, _)| *s == id))
            };
            let id = new_segment_id(&mut self.options.segment_ids, &self.manifest, taken);
            let meta = SegmentMeta {
                segment_id: id.get(),
                created_at: self.options.clock.unix_micros(),
                merge_generation: 0,
            };
            let registry = &self.options.registry;
            let segment = flush::segment(&c.memtable, registry, meta, self.options.write_options)?;
            planned.push(Planned {
                collection: name.clone(),
                mark: self.wal.mark(),
                segment: segment.map(|s| (id, s)),
                sidecars: plan_sidecars(name, c)?,
                memtable: Memtable::new(&c.schema, registry)?,
            });
        }
        if planned.is_empty() {
            return Ok(self.manifest.seq());
        }
        self.commit_guarded(planned, ManifestDiff::default(), None)
    }

    /// Commits if the [`CommitPolicy`] finds any memtable due at the clock's `now` (ROADMAP 6.3.4), flushing every
    /// memtable with operations ([`Database::commit`]); returns the new `seq` if it committed. Call it periodically:
    /// the engine runs no thread of its own.
    ///
    /// # Errors
    /// As [`Database::commit`].
    pub fn tick(&mut self) -> Result<Option<u64>, DatabaseError> {
        self.usable()?;
        let now = self.options.clock.now();
        let memtables = self.collections.values().map(|c| MemtableState {
            size_in_bytes: c.memtable.size_in_bytes(),
            first_write: c.first_write,
            last_write: c.last_write,
        });
        if self
            .options
            .policy
            .due(memtables, now)
            .iter()
            .all(Option::is_none)
        {
            return Ok(None);
        }
        self.commit().map(Some)
    }
    /// [`Database::commit_planned`]; any failure poisons the database, since the container may hold part of it.
    fn commit_guarded(
        &mut self,
        planned: Vec<Planned>,
        diff: ManifestDiff,
        created: Option<(String, Collection)>,
    ) -> Result<u64, DatabaseError> {
        let result = self.commit_planned(planned, diff, created);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    /// The commit protocol (SPEC §4.5): the segment and sidecar frames and the WAL records still buffered, synced
    /// (step 1); the
    /// Manifest frame and the superblock slot ([`Container::commit`], steps 2 and 3); then the new state is
    /// published (step 4).
    fn commit_planned(
        &mut self,
        planned: Vec<Planned>,
        mut diff: ManifestDiff,
        created: Option<(String, Collection)>,
    ) -> Result<u64, DatabaseError> {
        for p in &planned {
            if let Some((id, flushed)) = &p.segment {
                let container = self.wal.log_mut();
                let frame =
                    container.append(FrameKind::Segment, &flushed.bytes, Compression::None)?;
                let entry = SegmentEntry {
                    id: *id,
                    doc_count: flushed.doc_count,
                    size: u64::try_from(flushed.bytes.len()).unwrap_or(u64::MAX),
                    location: Location::Frame {
                        offset: frame.offset,
                        len: frame.len,
                    },
                    tombstones: None,
                };
                change_of(&mut diff, &p.collection)?.added.push(entry);
            }
            // Before `commit_point`, so this commit advances `wal_start` past the flushed records (SPEC §8.2.1).
            self.wal.flushed(&p.collection, p.mark);
        }
        // The sidecars that delete the older versions, after the new segments (SPEC §8.3).
        self.append_sidecars(&planned, &mut diff)?;
        let next = self.commit_diff(diff)?;

        // Durable: step 4. The new segments are opened from the bytes just written: an open checks their frames.
        let seq = next.seq();
        for p in planned {
            let Some(c) = self.collections.get_mut(&p.collection) else {
                continue;
            };
            for PlannedSidecar { index, sidecar, .. } in p.sidecars {
                let live = c.segments.get_mut(index).ok_or(DatabaseError::Internal {
                    what: "a tombstoned segment is not open",
                })?;
                let version = sidecar.version();
                let segment = sidecar.segment_id();
                // `TombstoneSidecar::new` checked every docid against the segment's count.
                let tombstones = live
                    .segment
                    .tombstones(sidecar.into_deleted())
                    .map_err(|_| DatabaseError::Internal {
                        what: "a planned sidecar does not fit its segment",
                    })?;
                let superseded = live.deleted.replace((version, Arc::new(tombstones)));
                if let Some((_, old)) = superseded {
                    // Older snapshots may still read it (SPEC §8.3).
                    let entry = self
                        .manifest
                        .collection(&p.collection)
                        .and_then(|e| e.segment(segment));
                    if let Some(t) = entry.and_then(|e| e.tombstones) {
                        let kind = ObsoleteKind::Sidecar {
                            segment,
                            version: t.version,
                        };
                        let pin = Pin::sidecar(&old);
                        self.merges
                            .obsolete
                            .retire(&p.collection, kind, t.location, seq, pin);
                    }
                }
            }
            if let Some((id, flushed)) = p.segment {
                let entry = next
                    .collection(&p.collection)
                    .and_then(|e| e.segment(id))
                    .ok_or(DatabaseError::Internal {
                        what: "a committed segment is not in its manifest",
                    })?;
                let source = ByteSource::new(flushed.bytes.into());
                let segment = open_source(source, &p.collection, entry, &c.schema)?;
                c.segments.push(LiveSegment {
                    segment,
                    deleted: None,
                });
            }
            c.memtable = p.memtable;
            c.first_write = None;
            c.last_write = None;
        }
        if let Some((name, collection)) = created {
            self.collections.insert(name, collection);
        }
        self.manifest = Arc::new(next);
        self.published
            .store(Arc::new(snapshot_of(&self.manifest, &self.collections)));
        Ok(self.manifest.seq())
    }

    /// Steps 1 (the WAL's part) to 3 of the commit protocol (SPEC §4.5) for `diff`, whose frames are appended: writes
    /// and syncs the WAL records still buffered, takes `wal_start`, syncs, appends the Manifest frame and writes the
    /// superblock slot. Returns the committed manifest; the caller publishes it.
    fn commit_diff(&mut self, mut diff: ManifestDiff) -> Result<Manifest, DatabaseError> {
        let wal_start = self.wal.commit_point()?.max(self.manifest.wal_start());
        self.wal.log_mut().sync()?;
        diff.wal_start = Some(wal_start);
        let next = self.manifest.apply(&diff)?;
        let encoded = next.encode()?;
        let container = self.wal.log_mut();
        let frame = container.append(FrameKind::Manifest, &encoded, Compression::None)?;
        container.commit(Commit {
            manifest: frame,
            wal_start,
            fence_token: next.fence_token(),
        })?;
        Ok(next)
    }

    /// Appends the tombstone sidecar frames of `planned` and adds each as an updated entry to `diff` (SPEC §8.3).
    fn append_sidecars(
        &mut self,
        planned: &[Planned],
        diff: &mut ManifestDiff,
    ) -> Result<(), DatabaseError> {
        for p in planned {
            for PlannedSidecar { sidecar, bytes, .. } in &p.sidecars {
                let container = self.wal.log_mut();
                let frame =
                    container.append(FrameKind::TombstoneSidecar, bytes, Compression::None)?;
                let mut entry = *self
                    .manifest
                    .collection(&p.collection)
                    .and_then(|e| e.segment(sidecar.segment_id()))
                    .ok_or(DatabaseError::Internal {
                        what: "a tombstoned segment is not in the manifest",
                    })?;
                entry.tombstones = Some(crate::manifest::Tombstones {
                    version: sidecar.version(),
                    deleted: sidecar.deleted_count(),
                    location: Location::Frame {
                        offset: frame.offset,
                        len: frame.len,
                    },
                });
                change_of(diff, &p.collection)?.updated.push(entry);
            }
        }
        Ok(())
    }
}

/// The change of collection `name` in `diff`, added if it has none. A flush never changes a collection the same commit
/// creates or drops: a segment or sidecar recorded elsewhere would be written but not referenced, its deletions
/// visible until a reopen.
fn change_of<'d>(
    diff: &'d mut ManifestDiff,
    name: &str,
) -> Result<&'d mut CollectionChange, DatabaseError> {
    let entry = diff
        .collections
        .entry(name.to_owned())
        .or_insert_with(|| CollectionDiff::Change(CollectionChange::default()));
    match entry {
        CollectionDiff::Change(change) => Ok(change),
        _ => Err(DatabaseError::Internal {
            what: "a flush of a collection the commit creates or drops",
        }),
    }
}

fn no_such(name: &str) -> DatabaseError {
    DatabaseError::NoSuchCollection {
        name: name.to_owned(),
    }
}

/// The next tombstone sidecar of each committed segment of `c` that holds a live version of an id `c`'s memtable
/// has an operation for, by the segment's index (SPEC §8.3, ROADMAP 6.4.3), each checked to keep every deletion of
/// the current one. Every segment is searched, not only the newest holding the id, so copies left live by earlier
/// builds are deleted too (ADR-0030).
fn plan_sidecars(name: &str, c: &Collection) -> Result<Vec<PlannedSidecar>, DatabaseError> {
    let mut added: BTreeMap<usize, RoaringBitmap> = BTreeMap::new();
    if c.segments.is_empty() {
        return Ok(Vec::new());
    }
    c.memtable.for_each_id(|id| {
        let id = std::str::from_utf8(id).map_err(|_| DatabaseError::Internal {
            what: "a memtable id is not UTF-8",
        })?;
        for (index, live) in c.segments.iter().enumerate() {
            let found = live
                .view()
                .docid_of(id)
                .map_err(|source| DatabaseError::ReadSegment {
                    collection: name.to_owned(),
                    id: SegmentId::new(live.segment.segment_id()),
                    source,
                })?;
            if let Some(docid) = found {
                added.entry(index).or_default().insert(docid);
            }
        }
        Ok::<(), DatabaseError>(())
    })?;
    let none = RoaringBitmap::new();
    let mut sidecars = Vec::with_capacity(added.len());
    for (index, new) in added {
        let live = c.segments.get(index).ok_or(DatabaseError::Internal {
            what: "a tombstoned segment is not open",
        })?;
        let id = SegmentId::new(live.segment.segment_id());
        let invalid = |source| DatabaseError::Sidecar {
            collection: name.to_owned(),
            id,
            source,
        };
        let (version, current) = live
            .deleted
            .as_ref()
            .map_or((0, &none), |(v, t)| (*v, t.docids()));
        let next = version
            .checked_add(1)
            .ok_or(invalid(SidecarError::Invalid {
                what: "no version after u64::MAX",
            }))?;
        let doc_count = live.segment.doc_count();
        let sidecar =
            TombstoneSidecar::new(id, next, doc_count, current | &new).map_err(invalid)?;
        sidecar.check_follows(version, current).map_err(invalid)?;
        let bytes = sidecar.encode().map_err(invalid)?;
        sidecars.push(PlannedSidecar {
            index,
            sidecar,
            bytes,
        });
    }
    Ok(sidecars)
}

/// A random segment id that no segment of `manifest` has, nor any that `taken` reports: planned in the same
/// commit, or the output of a merge in flight (SPEC §8.7.1).
fn new_segment_id(
    ids: &mut SegmentIds,
    manifest: &Manifest,
    taken: impl Fn(SegmentId) -> bool,
) -> SegmentId {
    loop {
        let id = SegmentId::new(ids.next_id());
        let in_manifest = manifest.collections().any(|c| c.segment(id).is_some());
        if !in_manifest && !taken(id) {
            return id;
        }
    }
}

/// The manifest's `seq`, `fence_token` and `wal_start` equal those of the slot that commits it (SPEC §8.7,
/// ROADMAP 6.3.7).
fn check_slot(manifest: &Manifest, sb: &Superblock) -> Result<(), DatabaseError> {
    let fields = [
        ("seq", manifest.seq(), sb.seq),
        ("fence_token", manifest.fence_token(), sb.fence_token),
        ("wal_start", manifest.wal_start(), sb.wal_start),
    ];
    for (field, manifest, superblock) in fields {
        if manifest != superblock {
            return Err(DatabaseError::SlotMismatch {
                field,
                manifest,
                superblock,
            });
        }
    }
    Ok(())
}

/// Opens a committed segment and its tombstone sidecar through the container's objects, which check their frames
/// (ROADMAP 6.3.6, 6.3.7), and checks both against the manifest entry.
fn open_segment<F: ContainerFile + Sync>(
    objects: &ContainerObjects<'_, F>,
    collection: &str,
    entry: &SegmentEntry,
    schema: &CollectionSchema,
) -> Result<LiveSegment, DatabaseError> {
    let key = persia_storage::Key::new(objects::segment_key(collection, entry))
        .map_err(persia_storage::Error::from)?;
    let source = objects.get_range(&key, 0..entry.size)?;
    let segment = open_source(source, collection, entry, schema)?;
    let deleted = match entry.tombstones {
        None => None,
        Some(t) => {
            let key = objects::sidecar_key(collection, entry.id, t.version);
            let key = persia_storage::Key::new(key).map_err(persia_storage::Error::from)?;
            let sidecar = read_sidecar(&objects.get(&key)?, collection, entry)?;
            let tombstones = segment
                .tombstones(sidecar.into_deleted())
                .map_err(|source| DatabaseError::OpenSegment {
                    collection: collection.to_owned(),
                    id: entry.id,
                    source,
                })?;
            Some((t.version, Arc::new(tombstones)))
        }
    };
    Ok(LiveSegment { segment, deleted })
}

/// Decodes the tombstone sidecar of the segment `entry` describes and checks it against the entry (SPEC §8.3).
fn read_sidecar(
    bytes: &[u8],
    collection: &str,
    entry: &SegmentEntry,
) -> Result<TombstoneSidecar, DatabaseError> {
    let sidecar = TombstoneSidecar::decode(bytes).map_err(|source| DatabaseError::Sidecar {
        collection: collection.to_owned(),
        id: entry.id,
        source,
    })?;
    let mismatch = |field| DatabaseError::SidecarMismatch {
        collection: collection.to_owned(),
        id: entry.id,
        field,
    };
    let Some(t) = entry.tombstones else {
        return Err(DatabaseError::Internal {
            what: "a sidecar read for a segment without one",
        });
    };
    if sidecar.segment_id() != entry.id {
        return Err(mismatch("segment_id"));
    }
    if sidecar.version() != t.version {
        return Err(mismatch("version"));
    }
    if sidecar.doc_count() != entry.doc_count {
        return Err(mismatch("doc_count"));
    }
    if sidecar.deleted_count() != t.deleted {
        return Err(mismatch("deleted"));
    }
    Ok(sidecar)
}

/// Opens the bytes of the segment `entry` describes and checks its footer against the entry.
fn open_source(
    source: ByteSource,
    collection: &str,
    entry: &SegmentEntry,
    schema: &CollectionSchema,
) -> Result<Arc<Segment>, DatabaseError> {
    let segment = Segment::open(source, schema).map_err(|source| DatabaseError::OpenSegment {
        collection: collection.to_owned(),
        id: entry.id,
        source,
    })?;
    let mismatch = |field| DatabaseError::SegmentMismatch {
        collection: collection.to_owned(),
        id: entry.id,
        field,
    };
    if segment.segment_id() != entry.id.get() {
        return Err(mismatch("segment_id"));
    }
    if segment.doc_count() != entry.doc_count {
        return Err(mismatch("doc_count"));
    }
    Ok(Arc::new(segment))
}

fn snapshot_of(manifest: &Arc<Manifest>, collections: &BTreeMap<String, Collection>) -> Snapshot {
    Snapshot {
        manifest: Arc::clone(manifest),
        collections: collections
            .iter()
            .map(|(name, c)| {
                let snapshot = CollectionSnapshot {
                    schema: Arc::clone(&c.schema),
                    segments: c.segments.iter().map(|s| Arc::clone(&s.segment)).collect(),
                    tombstones: c
                        .segments
                        .iter()
                        .map(|s| s.deleted.as_ref().map(|(_, t)| Arc::clone(t)))
                        .collect(),
                    memtable: c.memtable.reader(),
                };
                (name.clone(), snapshot)
            })
            .collect(),
    }
}
