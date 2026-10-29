//! A read-only open (SPEC §4.5.1 "Read-only open", §8.4; ROADMAP 6.6.2): a reader of a database another process
//! may be writing.
//!
//! It runs the open sequence of a writer ([`super::recovery`]) on a container opened read-only, which takes no lock
//! and never writes, so it opens next to a live writer (and a second writer fails fast on the writer lock, SPEC
//! §4.5.1). It holds the [`Snapshot`] of what it found: the committed segments, and memtables holding the WAL
//! records from `wal_start` on, the synced ones after the last commit included. It does not follow the writer's
//! later commits; open it again for those.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use persia_analysis::AnalyzerRegistry;
use persia_storage::container::{Container, ContainerFile, Mode};

use super::recovery::{for_each_record, load, wal_chunks};
use super::{ContainerObjects, DatabaseError, Snapshot, snapshot_of};
use crate::manifest::Manifest;
use crate::memtable::Memtable;
use crate::wal::{MAX_CHUNK_LEN, ReplayError, apply};

/// A database opened read-only (see the module docs).
///
/// ```
/// use persia_analysis::AnalyzerRegistry;
/// use persia_engine::db::{Database, DatabaseOptions, ReadOnlyDatabase};
/// use persia_engine::document::{Document, Value};
/// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
/// use persia_engine::wal::Durability;
///
/// # let dir = tempfile::tempdir()?;
/// let path = dir.path().join("app.persia");
/// let schema = SchemaBuilder::new("notes")
///     .field("body", FieldKind::Text(TextOptions::default()))
///     .build()?;
/// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
/// let mut db = Database::create(&path, DatabaseOptions::default())?;
/// db.create_collection(schema)?;
/// db.put("notes", &Document::new("n1").with(body, Value::String("hello".into())), Durability::Fsync)?;
///
/// // Next to the live writer: no lock, no writes. The synced put is replayed into the reader's memtable.
/// let reader = ReadOnlyDatabase::open(&path, &AnalyzerRegistry::builtin())?;
/// assert_eq!(reader.seq(), db.seq());
/// assert!(reader.snapshot().collection("notes").ok_or("no collection")?.get("n1")?.is_some());
/// // A second writer fails fast.
/// assert!(Database::open(&path, DatabaseOptions::default()).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct ReadOnlyDatabase<F = File> {
    container: Container<F>,
    snapshot: Arc<Snapshot>,
    replayed: u64,
}

impl ReadOnlyDatabase<File> {
    /// Opens the database in the container file at `path` read-only, analyzing replayed `text` fields with
    /// `registry`'s analyzers.
    ///
    /// # Errors
    /// As [`Container::open`] and [`ReadOnlyDatabase::open_container`].
    pub fn open(
        path: impl AsRef<Path>,
        registry: &AnalyzerRegistry,
    ) -> Result<Self, DatabaseError> {
        Self::open_container(Container::open(path, Mode::ReadOnly)?, registry)
    }
}

impl<F: ContainerFile + Sync> ReadOnlyDatabase<F> {
    /// [`ReadOnlyDatabase::open`] for a container in `file`.
    ///
    /// # Errors
    /// As [`Container::open_on`] and [`ReadOnlyDatabase::open_container`].
    pub fn open_on(file: F, registry: &AnalyzerRegistry) -> Result<Self, DatabaseError> {
        Self::open_container(Container::open_on(file, Mode::ReadOnly)?, registry)
    }

    /// Runs the open sequence on `container` (SPEC §8.6) without writing to it, whatever its mode. Replay cannot
    /// flush here, so a backlog that does not fit a memtable ([`ReplayError::Memtable`] with
    /// [`crate::segment::SegmentError::Full`], about 2^31 operations) fails the open; a read-write open flushes it
    /// (ROADMAP 6.6.5).
    ///
    /// # Errors
    /// As [`super::Database::open_on`].
    pub fn open_container(
        container: Container<F>,
        registry: &AnalyzerRegistry,
    ) -> Result<Self, DatabaseError> {
        let (manifest, mut collections) = load(&container, |s| Memtable::new(s, registry))?;
        let mut replayed = 0u64;
        for chunk in wal_chunks(&container) {
            let frame = container
                .read_frame_source(chunk, MAX_CHUNK_LEN)
                .map_err(ReplayError::from)?;
            let applied = for_each_record(chunk, &frame.payload, |offset, record| {
                let name = record.collection();
                let c =
                    collections
                        .get_mut(name)
                        .ok_or_else(|| ReplayError::UnknownCollection {
                            chunk,
                            offset,
                            collection: name.to_owned(),
                        })?;
                Ok(apply(&mut c.memtable, chunk, offset, record)?)
            })?;
            replayed = replayed.saturating_add(applied);
        }
        let snapshot = Arc::new(snapshot_of(&Arc::new(manifest), &collections));
        Ok(Self {
            container,
            snapshot,
            replayed,
        })
    }

    /// The committed manifest.
    pub fn manifest(&self) -> &Manifest {
        self.snapshot.manifest()
    }

    /// The `seq` of the last commit.
    pub fn seq(&self) -> u64 {
        self.snapshot.seq()
    }

    /// The container.
    pub fn container(&self) -> &Container<F> {
        &self.container
    }

    /// What the open found: the committed segments and the replayed memtables.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.snapshot)
    }

    /// The WAL records the open replayed.
    pub fn replayed(&self) -> u64 {
        self.replayed
    }

    /// The committed objects of the container, as [`persia_storage::Storage`] (ROADMAP 6.3.6).
    pub fn objects(&self) -> ContainerObjects<'_, F> {
        ContainerObjects::new(&self.container, self.snapshot.manifest())
    }
}
