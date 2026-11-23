//! A read-only open (SPEC §4.5.1 "Read-only open", §8.4, §8.6; ROADMAP 6.6.2, 6.6.6): a reader of a database another
//! process may be writing.
//!
//! It runs the open sequence of a writer ([`super::recovery`]) on a container opened read-only, which takes no lock
//! and never writes, so it opens next to a live writer (and a second writer fails fast on the writer lock, SPEC
//! §4.5.1). It holds the [`Snapshot`] of what it found: the committed segments, and memtables holding the WAL
//! records from `wal_start` on, those written after the last commit included (written, not necessarily synced: an
//! `Async` write a power loss may still drop), up to the first Manifest frame after `log_end` that a fresh
//! superblock read names as committed (a writer committed while the tail was scanned, SPEC §8.6). A Manifest the
//! slot still leaves after `log_end` is a cut commit's: the WAL after it belongs to this snapshot and is replayed
//! (ROADMAP 6.6.9).
//!
//! [`ReadOnlyDatabase::refresh`] follows the writer's later commits and WAL (ROADMAP 6.6.6, ADR-0060). It reopens the
//! container, then either replays only the WAL chunks written since into the same memtables, opening only the
//! segments whose manifest entry changed, or, where that is not sound, runs the open sequence again. Either way the
//! next state is swapped in at once ([`ArcSwap`]); a snapshot taken before keeps what it holds.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use persia_analysis::{Analyzer, AnalyzerRegistry, TokenStream, Tokenizer};
use persia_format::hash::xxh3_128;
use persia_storage::container::{Container, ContainerFile, FrameRef, Mode};

use super::recovery::{for_each_record, load, open_segment, read_manifest, wal_chunks};
use super::{Collection, ContainerObjects, DatabaseError, Snapshot, check_analyzers, snapshot_of};
use crate::manifest::{CollectionEntry, Manifest};
use crate::memtable::Memtable;
use crate::schema::{CollectionSchema, FieldKind};
use crate::wal::{MAX_CHUNK_LEN, ReplayError, apply};

/// A database opened read-only (see the module docs).
///
/// ```
/// use persia_analysis::AnalyzerRegistry;
/// use persia_engine::db::{Database, DatabaseOptions, ReadOnlyDatabase, Refreshed};
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
/// let before = reader.snapshot();
/// assert!(before.collection("notes").ok_or("no collection")?.get("n1")?.is_some());
/// // A second writer fails fast.
/// assert!(Database::open(&path, DatabaseOptions::default()).is_err());
///
/// // The writer logs another put: a refresh replays only it.
/// db.put("notes", &Document::new("n2").with(body, Value::String("again".into())), Durability::Fsync)?;
/// assert!(matches!(reader.refresh()?, Refreshed::Incremental { records: 1, .. }));
/// assert!(reader.snapshot().collection("notes").ok_or("no collection")?.get("n2")?.is_some());
/// // A flush commit moves `wal_start` past the records the memtables hold: the next state is opened in full.
/// db.commit()?;
/// assert!(matches!(reader.refresh()?, Refreshed::Reloaded { .. }));
/// assert_eq!(reader.seq(), db.seq());
/// assert_eq!(reader.refresh()?, Refreshed::Unchanged);
/// // The snapshot taken first still shows the commit it was taken at.
/// assert_eq!(before.seq() + 1, reader.seq());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct ReadOnlyDatabase<F = File> {
    /// What readers see; a refresh swaps it.
    state: ArcSwap<State<F>>,
    /// The open's registry with the built-ins added, which every later state's memtables analyze with.
    registry: AnalyzerRegistry,
    /// The open's treatment of a `text` field whose analyzer `registry` lacks.
    analysis: Analysis,
    /// What replay writes into. A refresh holds it throughout, so refreshes run one at a time; readers never take it.
    live: Mutex<Live>,
}

/// What readers see: what the open found, or the last refresh.
#[derive(Debug)]
pub(super) struct State<F> {
    /// The container the state was read from.
    pub(super) container: Arc<Container<F>>,
    pub(super) snapshot: Arc<Snapshot>,
    /// The WAL records replayed into the snapshot's memtables.
    pub(super) replayed: u64,
    /// The WAL chunk frames replayed into them, in order: what `verify` reads the WAL's blob references from (SPEC
    /// §9.5, ROADMAP 9.5.3), and what a refresh finds again at the start of its scan to replay only the frames after.
    pub(super) chunks: Vec<FrameRef>,
    /// A content hash of each of those chunks a power loss may still replace, those past the `log_end` of
    /// `container`'s superblock, in log order ([`unsynced`]). A refresh reads them again and compares, so a history
    /// in which a power loss dropped them and the writer logged others at the same places reloads.
    unsynced: Vec<Fingerprint>,
    /// The end of the last of those chunks, the committed `wal_start` if none ([`ReadOnlyDatabase::wal_end`]).
    pub(super) wal_end: u64,
    /// The `text` fields whose analysis is skipped ([`ReadOnlyDatabase::open_container_skipping_analysis`]).
    skipped: Vec<SkippedAnalysis>,
}

/// The writer's side of the current state: the memtables replay applies records to, which the snapshot reads.
#[derive(Debug)]
struct Live {
    /// The manifest the collections' segments are of.
    manifest: Arc<Manifest>,
    collections: BTreeMap<String, Collection>,
    /// A refresh failed or panicked after it changed `collections`: the memtables may hold part of a chunk, and
    /// stay held, so the published state is unchanged. The next refresh reloads in full.
    broken: bool,
}

/// What [`ReadOnlyDatabase::refresh`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refreshed {
    /// The same commit, and no WAL chunk since: the state stays as it was.
    Unchanged,
    /// The WAL records logged since were applied to the memtables, and the commits since, which flushed none of the
    /// records they hold and dropped or changed no collection (a merge, a retiring commit, a created collection), were
    /// taken by opening only the segments whose manifest entry changed (ADR-0060 item 3).
    Incremental {
        /// The committed `seq` the snapshot now shows.
        seq: u64,
        /// The WAL records applied.
        records: u64,
    },
    /// The next state was opened in full, as [`ReadOnlyDatabase::open_container`] would open it (ADR-0060 item 4).
    Reloaded {
        /// The committed `seq` the snapshot now shows.
        seq: u64,
    },
}

impl<F> fmt::Debug for ReadOnlyDatabase<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.load();
        f.debug_struct("ReadOnlyDatabase")
            .field("seq", &state.snapshot.seq())
            .field("replayed", &state.replayed)
            .field("wal_end", &state.wal_end)
            .field("analysis", &self.analysis)
            .finish_non_exhaustive()
    }
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
    /// checked as by `open_container`. A [`refresh`](Self::refresh) keeps skipping them, and skips those of a
    /// collection created since.
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
        let registry = registry.clone().with_builtins();
        let (state, live) = opened(container, &registry, analysis)?;
        Ok(Self {
            state: ArcSwap::from_pointee(state),
            registry,
            analysis,
            live: Mutex::new(live),
        })
    }

    /// The current state, as one value: what [`Self::verify`] and [`Self::blob_references`] read throughout, so a
    /// refresh meanwhile cannot pair one state's snapshot with another's container.
    pub(super) fn state(&self) -> Arc<State<F>> {
        self.state.load_full()
    }

    /// The committed manifest of the current state.
    pub fn manifest(&self) -> Arc<Manifest> {
        Arc::clone(&self.state.load().snapshot.manifest)
    }

    /// The `seq` of the last commit the current state shows.
    pub fn seq(&self) -> u64 {
        self.state.load().snapshot.seq()
    }

    /// The container the current state was read from: the open's, or the last refresh's.
    pub fn container(&self) -> Arc<Container<F>> {
        Arc::clone(&self.state.load().container)
    }

    /// What the open, or the last refresh, found: the committed segments and the replayed memtables.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.state.load().snapshot)
    }

    /// The WAL records replayed into the current snapshot's memtables, by the open and the refreshes since its last
    /// full reload.
    pub fn replayed(&self) -> u64 {
        self.state.load().replayed
    }

    /// The offset just past the last WAL chunk replayed into the current snapshot, or the committed `wal_start` when
    /// none was.
    ///
    /// Within one manifest `seq` the WAL only grows, so this and [`ReadOnlyDatabase::replayed`] grow together: what
    /// a refresh compares two opens of the same `seq` by. [`Container::end`] does not: it also counts frames that are
    /// not WAL, such as the segment frames a commit writes before its Manifest (SPEC §8.6).
    pub fn wal_end(&self) -> u64 {
        self.state.load().wal_end
    }

    /// The `text` fields whose analysis the current state skipped, in collection then field id order: none unless it
    /// was opened with [`ReadOnlyDatabase::open_container_skipping_analysis`] and a registry lacking their analyzers.
    pub fn skipped_analysis(&self) -> Vec<SkippedAnalysis> {
        self.state.load().skipped.clone()
    }
}

impl<F: ContainerFile + Sync + 'static> ReadOnlyDatabase<F> {
    /// The committed objects of the current state's container, as [`persia_storage::Storage`] (ROADMAP 6.3.6). The
    /// view holds that container, so a refresh does not change it.
    pub fn objects(&self) -> ContainerObjects<'static, F> {
        let state = self.state.load();
        ContainerObjects::shared(Arc::clone(&state.container), state.snapshot.manifest())
    }

    /// Follows the writer (ROADMAP 6.6.6, ADR-0060): reopens the container read-only
    /// ([`Container::reopen_read_only`]: at its path when it was opened by one, so it follows a compaction that
    /// renamed a new file over it, SPEC §4.6) and swaps in the state found there. A search, or anything else holding
    /// a [`Snapshot`], keeps the one it took.
    ///
    /// - [`Refreshed::Unchanged`]: the same commit and no WAL chunk since; nothing is swapped.
    /// - [`Refreshed::Incremental`]: the WAL chunks already replayed are the first ones the new scan finds (the same
    ///   frames, and those a power loss may have replaced since, the ones past the `log_end` the current state read,
    ///   with the same payload bytes), and the commits since, if any, moved `wal_start` past no chunk
    ///   the memtables replayed and kept every collection and its schema (a merge, a commit that retires a cut one, a
    ///   created collection, a flush of records this reader never replayed). The chunks after them
    ///   are replayed, with the read-only stop of SPEC §8.6, into the same memtables, and only the segments and
    ///   sidecars whose manifest entry changed are opened. The memtables are held while records apply (as a batch
    ///   holds them, SPEC §8.2.1), so no view sees part of a refresh. A snapshot taken before shares them, so a view
    ///   taken from it after the refresh sees the new records too, as a writer's snapshot sees its later writes
    ///   (SPEC §8.4).
    /// - [`Refreshed::Reloaded`]: anything else (a flush commit moved `wal_start` past the records the memtables
    ///   hold, a collection was dropped or its schema changed, the chunks replayed are not the first ones found, or an
    ///   earlier refresh failed after it changed the memtables) runs the open sequence on the reopened container,
    ///   with the open's registry and analysis.
    ///
    /// Refreshes run one at a time; readers never wait for one.
    ///
    /// # Errors
    /// Nothing readers see changes on an error. [`DatabaseError::RefreshRefused`] if the container holds another
    /// database (its `db_uuid`) or an older commit of this one; otherwise as [`Container::reopen_read_only`] and
    /// [`ReadOnlyDatabase::open_container`]. A replay that fails part-way leaves its memtables held, so views keep
    /// showing the published state, and the next refresh reloads in full.
    pub fn refresh(&self) -> Result<Refreshed, DatabaseError> {
        let mut live = self.live.lock().unwrap_or_else(|poisoned| {
            // A refresh panicked: what it applied is unknown, so the next state is opened in full.
            let mut live = poisoned.into_inner();
            live.broken = true;
            self.live.clear_poison();
            live
        });
        let current = self.state.load_full();
        let container = current.container.reopen_read_only()?;
        let current_seq = current.snapshot.seq();
        let found = container.superblock().seq;
        let other_database = container.header().db_uuid != current.container.header().db_uuid;
        if other_database || found < current_seq {
            return Err(DatabaseError::RefreshRefused {
                other_database,
                current: current_seq,
                found,
            });
        }
        let step = if live.broken {
            Step::Reload
        } else {
            plan(&current, &live, &container)?
        };
        match step {
            Step::Unchanged => Ok(Refreshed::Unchanged),
            Step::Reload => {
                let (state, next) = opened(container, &self.registry, self.analysis)?;
                self.state.store(Arc::new(state));
                *live = next;
                Ok(Refreshed::Reloaded { seq: found })
            }
            Step::Incremental { manifest, chunks } => {
                self.incremental(&mut live, &current, container, manifest, chunks)
            }
        }
    }

    /// [`Step::Incremental`]: takes `manifest` (a later commit [`plan`] found compatible) if any, replays `fresh`
    /// into the held memtables, swaps the state, releases them.
    fn incremental(
        &self,
        live: &mut Live,
        current: &State<F>,
        container: Container<F>,
        manifest: Option<Manifest>,
        fresh: Vec<FrameRef>,
    ) -> Result<Refreshed, DatabaseError> {
        let seq = container.superblock().seq;
        let skipped = match manifest {
            // Changes `live` only once every segment opened.
            Some(next) => take_manifest(live, &container, next, &self.registry, self.analysis)?,
            None => current.skipped.clone(),
        };
        // From here until the swap, a failure leaves the memtables held and the next refresh reloads.
        live.broken = true;
        for c in live.collections.values() {
            c.memtable.hold();
        }
        // With nothing replayed yet, the WAL ends where the commit found now puts `wal_start`, as a fresh open's.
        let end = if current.chunks.is_empty() {
            container.superblock().wal_start
        } else {
            current.wal_end
        };
        let applied = replay(&container, &fresh, &mut live.collections, end)?;
        let snapshot = Arc::new(snapshot_of(&live.manifest, &live.collections));
        let mut chunks = current.chunks.clone();
        chunks.extend(fresh);
        // `plan` checked the old fingerprints against this container, so they hold for it: those it has synced since
        // are dropped.
        let mut fingerprints = current.unsynced.clone();
        fingerprints.extend(applied.fingerprints);
        self.state.store(Arc::new(State {
            unsynced: unsynced(&container, fingerprints),
            container: Arc::new(container),
            snapshot,
            replayed: current.replayed.saturating_add(applied.records),
            chunks,
            wal_end: applied.wal_end,
            skipped,
        }));
        live.broken = false;
        for c in live.collections.values() {
            c.memtable.release();
        }
        Ok(Refreshed::Incremental {
            seq,
            records: applied.records,
        })
    }
}

/// What a refresh does with the container it reopened.
#[derive(Debug)]
enum Step {
    /// Nothing new.
    Unchanged,
    /// Take `manifest` (the next commit) if any, then replay `chunks` (the WAL chunk frames after those replayed).
    Incremental {
        manifest: Option<Manifest>,
        chunks: Vec<FrameRef>,
    },
    /// Run the open sequence on it.
    Reload,
}

/// What `container` lets a refresh of `current` (whose memtables are `live`'s, not broken) do (ADR-0060 items 3
/// to 5).
fn plan<F: ContainerFile + Sync>(
    current: &State<F>,
    live: &Live,
    container: &Container<F>,
) -> Result<Step, DatabaseError> {
    let (old, new) = (current.container.superblock(), container.superblock());
    let manifest = if new.seq == old.seq {
        // The same commit, unless another history was put at the path (a copy, a restore).
        if (new.manifest_off, new.wal_start) != (old.manifest_off, old.wal_start) {
            return Ok(Step::Reload);
        }
        None
    } else {
        let next = read_manifest(container)?;
        // A dropped collection, or a changed schema, needs memtables of its own (a schema change flushes first,
        // ADR-0029 item 5); a created one has no record before its commit.
        let kept = live.manifest.collections().all(|old| {
            next.collections()
                .find(|c| c.name() == old.name())
                .is_some_and(|c| c.schema() == old.schema())
        });
        if !kept {
            return Ok(Step::Reload);
        }
        Some(next)
    };
    // The new scan starts at the new `wal_start`. A commit that moved it past a chunk the memtables replayed flushed
    // records they hold, and a memtable can drop no prefix: that chunk is not found again. One that passed only
    // chunks they never replayed (they replayed none) leaves them as a fresh open of it builds them.
    let found = wal_chunks(container, new.seq, false)?;
    let Some(fresh) = found.strip_prefix(current.chunks.as_slice()) else {
        return Ok(Step::Reload);
    };
    // The same frames may still hold other records: a power loss on the writer's host (this reader may be on another
    // one, sharing the disk) drops the chunks written since the last sync, and the restarted writer logs others at
    // the same offsets, with the same lengths and `frame_seq`s. It may drop several, so the new history can first
    // differ at any of them, and the last one can be byte-identical. Every chunk past the `log_end` of the slot
    // the current state read is compared by content, the frame's CRC aside: each record ends with the CRC-32C of
    // its own bytes (SPEC §8.2.1), so a CRC over the payload depends only on the record lengths. The chunks before
    // that `log_end` cannot change: the commit wrote the slot only after it synced every frame before its
    // `log_end` (SPEC §4.5 steps 1 to 3), so they survive a power loss even if the slot itself does not.
    for fingerprint in &current.unsynced {
        let frame = container
            .read_frame_source(fingerprint.offset, MAX_CHUNK_LEN)
            .map_err(ReplayError::from)?;
        if xxh3_128(&frame.payload) != fingerprint.hash {
            return Ok(Step::Reload);
        }
    }
    if manifest.is_none() && fresh.is_empty() {
        return Ok(Step::Unchanged);
    }
    Ok(Step::Incremental {
        manifest,
        chunks: fresh.to_vec(),
    })
}

/// Takes `next`, a later commit [`plan`] found compatible, into `live`: each collection's segments as `next` lists
/// them, sharing those whose entry is unchanged and opening the others, and an empty memtable for each new collection,
/// analyzed with `registry` as an open would. Returns the fields whose analysis is skipped. `live` changes only once
/// every segment opened.
fn take_manifest<F: ContainerFile + Sync>(
    live: &mut Live,
    container: &Container<F>,
    next: Manifest,
    registry: &AnalyzerRegistry,
    analysis: Analysis,
) -> Result<Vec<SkippedAnalysis>, DatabaseError> {
    let objects = ContainerObjects::new(container, &next);
    let mut registry = registry.clone();
    let mut skipped = Vec::new();
    let mut segments = BTreeMap::new();
    let mut created = Vec::new();
    for entry in next.collections() {
        let name = entry.name();
        // As the open's `load`: the same registry and skipped fields as a fresh open of `next`.
        if analysis == Analysis::SkipUnregistered {
            skip_unregistered(name, entry.schema(), &mut registry, &mut skipped)?;
        }
        check_analyzers(name, entry.schema(), &registry)?;
        let old = live.collections.get(name);
        let old_entries = live
            .manifest
            .collections()
            .find(|c| c.name() == name)
            .map(CollectionEntry::segments);
        let schema = old.map_or_else(
            || Arc::new(entry.schema().clone()),
            |c| Arc::clone(&c.schema),
        );
        let mut opened = Vec::with_capacity(entry.segments().len());
        for segment in entry.segments() {
            // The collection's segments are in its manifest entry's order.
            let reused = old_entries
                .and_then(|entries| entries.iter().position(|e| e == segment))
                .and_then(|i| old?.segments.get(i).cloned());
            opened.push(match reused {
                Some(s) => s,
                None => open_segment(&objects, name, segment, &schema)?,
            });
        }
        if old.is_none() {
            let collection = Collection {
                memtable: Memtable::new(&schema, &registry)?,
                schema,
                segments: Vec::new(),
                first_write: None,
                last_write: None,
            };
            created.push((name.to_owned(), collection));
        }
        segments.insert(name.to_owned(), opened);
    }
    live.collections.extend(created);
    for (name, c) in &mut live.collections {
        if let Some(s) = segments.remove(name) {
            c.segments = s;
        }
    }
    live.manifest = Arc::new(next);
    Ok(skipped)
}

/// The open sequence (SPEC §8.6) on `container`, with `registry` and `analysis`: the state readers see, and the
/// memtables behind it.
fn opened<F: ContainerFile + Sync>(
    container: Container<F>,
    registry: &AnalyzerRegistry,
    analysis: Analysis,
) -> Result<(State<F>, Live), DatabaseError> {
    let mut registry = registry.clone();
    let mut skipped = Vec::new();
    let (manifest, mut collections) = load(&container, |name, schema| {
        if analysis == Analysis::SkipUnregistered {
            skip_unregistered(name, schema, &mut registry, &mut skipped)?;
        }
        check_analyzers(name, schema, &registry)?;
        Ok(Memtable::new(schema, &registry)?)
    })?;
    // Stop at a Manifest a fresh superblock read shows committed during this scan (SPEC §8.6).
    let chunks = wal_chunks(&container, manifest.seq(), false)?;
    let wal_start = container.superblock().wal_start;
    let applied = replay(&container, &chunks, &mut collections, wal_start)?;
    let manifest = Arc::new(manifest);
    let state = State {
        snapshot: Arc::new(snapshot_of(&manifest, &collections)),
        unsynced: unsynced(&container, applied.fingerprints),
        container: Arc::new(container),
        replayed: applied.records,
        chunks,
        wal_end: applied.wal_end,
        skipped,
    };
    let live = Live {
        manifest,
        collections,
        broken: false,
    };
    Ok((state, live))
}

/// What [`replay`] applied.
struct Applied {
    records: u64,
    /// The end of the last chunk replayed, or the `end` it was given when there was none.
    wal_end: u64,
    /// One per chunk replayed past the container's `log_end`, in order.
    fingerprints: Vec<Fingerprint>,
}

/// A replayed WAL chunk and a hash of the payload it was replayed from.
#[derive(Debug, Clone, Copy)]
struct Fingerprint {
    offset: u64,
    end: u64,
    /// XXH3-128, not a CRC (see [`plan`]).
    hash: u128,
}

/// The `fingerprints` of chunks a power loss may still replace in `container`: those past its superblock's `log_end`,
/// which a commit syncs before it writes the slot (SPEC §4.5). The others are never read again.
fn unsynced<F: ContainerFile>(
    container: &Container<F>,
    mut fingerprints: Vec<Fingerprint>,
) -> Vec<Fingerprint> {
    let log_end = container.superblock().log_end;
    fingerprints.retain(|f| f.end > log_end);
    fingerprints
}

/// Applies every record of `chunks` to its collection's memtable, in order (SPEC §8.2.1).
fn replay<F: ContainerFile + Sync>(
    container: &Container<F>,
    chunks: &[FrameRef],
    collections: &mut BTreeMap<String, Collection>,
    end: u64,
) -> Result<Applied, DatabaseError> {
    let mut applied = Applied {
        records: 0,
        wal_end: end,
        fingerprints: Vec::new(),
    };
    let log_end = container.superblock().log_end;
    for chunk in chunks.iter().map(|c| c.offset) {
        let frame = container
            .read_frame_source(chunk, MAX_CHUNK_LEN)
            .map_err(ReplayError::from)?;
        let records = for_each_record(chunk, &frame.payload, |offset, record| {
            let name = record.collection();
            let c = collections
                .get_mut(name)
                .ok_or_else(|| ReplayError::UnknownCollection {
                    chunk,
                    offset,
                    collection: name.to_owned(),
                })?;
            Ok(apply(&mut c.memtable, chunk, offset, record)?)
        })?;
        applied.records = applied.records.saturating_add(records);
        // Taken from the frame just read: no later lookup that could miss.
        applied.wal_end = frame.frame.end();
        // Only those a power loss may replace ([`unsynced`]) are hashed.
        if applied.wal_end > log_end {
            applied.fingerprints.push(Fingerprint {
                offset: chunk,
                end: applied.wal_end,
                hash: xxh3_128(&frame.payload),
            });
        }
    }
    Ok(applied)
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
