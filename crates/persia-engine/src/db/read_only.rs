//! A read-only open (SPEC §4.5.1 "Read-only open", §8.4; ROADMAP 6.6.2): a reader of a database another process
//! may be writing.
//!
//! It runs the open sequence of a writer ([`super::recovery`]) on a container opened read-only, which takes no lock
//! and never writes, so it opens next to a live writer (and a second writer fails fast on the writer lock, SPEC
//! §4.5.1). It holds the [`Snapshot`] of what it found: the committed segments, and memtables holding the WAL
//! records from `wal_start` on, those written after the last commit included (written, not necessarily synced: an
//! `Async` write a power loss may still drop), up to the first Manifest frame after `log_end` that a fresh
//! superblock read names as committed (a writer committed while the tail was scanned, SPEC §8.6). A Manifest the
//! slot still leaves after `log_end` is a cut commit's: the WAL after it belongs to this snapshot and is replayed
//! (ROADMAP 6.6.9). The open does not follow a writer's later commits; open it again for those.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use persia_analysis::{Analyzer, AnalyzerRegistry, TokenStream, Tokenizer};
use persia_storage::container::{Container, ContainerFile, Mode};

use super::recovery::{for_each_record, load, wal_chunks};
use super::{ContainerObjects, DatabaseError, Snapshot, check_analyzers, snapshot_of};
use crate::manifest::Manifest;
use crate::memtable::Memtable;
use crate::schema::{CollectionSchema, FieldKind};
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
    /// The positions of the WAL chunks the open replayed, in order: what `verify` reads the WAL's blob references
    /// from (SPEC §9.5, ROADMAP 9.5.3).
    wal_chunks: Vec<u64>,
    /// The end of the last of those chunks, the committed `wal_start` if none ([`ReadOnlyDatabase::wal_end`]).
    wal_end: u64,
    /// The `text` fields whose analysis the open skipped ([`ReadOnlyDatabase::open_container_skipping_analysis`]).
    skipped: Vec<SkippedAnalysis>,
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

    /// [`ReadOnlyDatabase::open`] that skips the analysis of `text` fields whose analyzer `registry` lacks
    /// ([`ReadOnlyDatabase::open_container_skipping_analysis`]).
    ///
    /// # Errors
    /// As [`Container::open`] and [`ReadOnlyDatabase::open_container_skipping_analysis`].
    pub fn open_skipping_analysis(
        path: impl AsRef<Path>,
        registry: &AnalyzerRegistry,
    ) -> Result<Self, DatabaseError> {
        Self::open_container_skipping_analysis(Container::open(path, Mode::ReadOnly)?, registry)
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

    /// Runs the open sequence on `container` (SPEC §8.6) without writing to it, whatever its mode.
    ///
    /// Replay cannot flush here. It does not stop at the writer's size threshold, so memory grows with the
    /// unflushed backlog of each collection until the memtable's docid limit ([`ReplayError::Memtable`] with
    /// [`crate::segment::SegmentError::Full`], about 2^31 operations) fails the open. A read-write open flushes
    /// at the threshold and so bounds that memory (ROADMAP 6.6.5, ADR-0033).
    ///
    /// # Errors
    /// As [`super::Database::open_on`].
    pub fn open_container(
        container: Container<F>,
        registry: &AnalyzerRegistry,
    ) -> Result<Self, DatabaseError> {
        Self::open_with(container, registry, Analysis::Required)
    }

    /// [`ReadOnlyDatabase::open_container`] for a check of the database, such as `persia verify` (ROADMAP 6.6.8): a
    /// `text` field whose analyzer `registry` lacks (a registered analyzer, SPEC §6.2.2) does not fail the open.
    /// Replay then validates and applies every record, but indexes no terms for that field, so its memtable cannot
    /// answer a query on the field; [`ReadOnlyDatabase::skipped_analysis`] lists such fields. Everything else is
    /// checked as by `open_container`.
    ///
    /// # Errors
    /// As [`ReadOnlyDatabase::open_container`], but never [`DatabaseError::UnregisteredAnalyzer`].
    pub fn open_container_skipping_analysis(
        container: Container<F>,
        registry: &AnalyzerRegistry,
    ) -> Result<Self, DatabaseError> {
        Self::open_with(container, registry, Analysis::SkipUnregistered)
    }

    fn open_with(
        container: Container<F>,
        registry: &AnalyzerRegistry,
        analysis: Analysis,
    ) -> Result<Self, DatabaseError> {
        let mut registry = registry.clone().with_builtins();
        let mut skipped = Vec::new();
        let (manifest, mut collections) = load(&container, |name, schema| {
            if analysis == Analysis::SkipUnregistered {
                skip_unregistered(name, schema, &mut registry, &mut skipped)?;
            }
            check_analyzers(name, schema, &registry)?;
            Ok(Memtable::new(schema, &registry)?)
        })?;
        let mut replayed = 0u64;
        let mut wal_end = container.superblock().wal_start;
        // Stop at a Manifest a fresh superblock read shows committed during this scan (SPEC §8.6).
        let chunks = wal_chunks(&container, manifest.seq(), false)?;
        for &chunk in &chunks {
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
            // Taken from the frame just read: no later lookup that could miss.
            wal_end = frame.frame.end();
        }
        let snapshot = Arc::new(snapshot_of(&Arc::new(manifest), &collections));
        Ok(Self {
            container,
            snapshot,
            replayed,
            wal_chunks: chunks,
            wal_end,
            skipped,
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

    /// The offset just past the last WAL chunk the open replayed, or the committed `wal_start` when it replayed none.
    ///
    /// Within one manifest `seq` the WAL only grows, so this and [`ReadOnlyDatabase::replayed`] grow together: what
    /// a refresh compares two opens of the same `seq` by. [`Container::end`] does not: it also counts frames that are
    /// not WAL, such as the segment frames a commit writes before its Manifest (SPEC §8.6).
    pub fn wal_end(&self) -> u64 {
        self.wal_end
    }

    /// The positions of the WAL chunks the open replayed, in order.
    pub(super) fn wal_chunks(&self) -> &[u64] {
        &self.wal_chunks
    }

    /// The `text` fields whose analysis the open skipped, in collection then field id order: none unless it was
    /// [`ReadOnlyDatabase::open_container_skipping_analysis`] with a registry lacking their analyzers.
    pub fn skipped_analysis(&self) -> &[SkippedAnalysis] {
        &self.skipped
    }

    /// The committed objects of the container, as [`persia_storage::Storage`] (ROADMAP 6.3.6).
    pub fn objects(&self) -> ContainerObjects<'_, F> {
        ContainerObjects::new(&self.container, self.snapshot.manifest())
    }
}

/// Whether a read-only open needs every `text` field's analyzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Analysis {
    /// A missing one fails the open ([`DatabaseError::UnregisteredAnalyzer`]).
    Required,
    /// A missing one is replaced by one that emits no token, and the field is listed as skipped.
    SkipUnregistered,
}

/// A `text` field whose analysis a read-only open skipped, because its registry lacks the field's analyzer
/// ([`ReadOnlyDatabase::open_container_skipping_analysis`], ROADMAP 6.6.8).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SkippedAnalysis {
    /// The collection.
    pub collection: String,
    /// The field.
    pub field: String,
    /// Its analyzer, `name@version`.
    pub analyzer: String,
}

/// A tokenizer that emits nothing: what a field whose analysis is skipped is indexed with.
struct NoTokens;

impl Tokenizer for NoTokens {
    fn tokenize(&self, _text: &str, _out: &mut TokenStream) {}
}

/// Registers, for every `text` field of `schema` whose analyzer `registry` lacks, an analyzer that emits no token
/// under its name and version, and lists the field in `skipped`. A built-in name cannot be registered: its versions
/// this build lacks were refused when the manifest was decoded (SPEC §6.2.2), so that fails the open as unregistered.
fn skip_unregistered(
    collection: &str,
    schema: &CollectionSchema,
    registry: &mut AnalyzerRegistry,
    skipped: &mut Vec<SkippedAnalysis>,
) -> Result<(), DatabaseError> {
    for field in schema.fields() {
        let FieldKind::Text(options) = field.kind() else {
            continue;
        };
        let analyzer = &options.analyzer;
        // An analyzer an earlier field lacked is in `registry` now, as the no-token one: this field is skipped too.
        let replaced = skipped.iter().any(|s| s.analyzer == analyzer.to_string());
        if !replaced {
            if registry.get(analyzer.name(), analyzer.version()).is_some() {
                continue;
            }
            let unregistered = || DatabaseError::UnregisteredAnalyzer {
                collection: collection.to_owned(),
                field: field.name().to_owned(),
                analyzer: analyzer.to_string(),
            };
            registry
                .register(
                    analyzer.name(),
                    analyzer.version(),
                    Analyzer::builder(NoTokens).build(),
                )
                .map_err(|_| unregistered())?;
        }
        skipped.push(SkippedAnalysis {
            collection: collection.to_owned(),
            field: field.name().to_owned(),
            analyzer: analyzer.to_string(),
        });
    }
    Ok(())
}
