//! Opening a database (SPEC §4.5.1 "Recovery", §8.6; ROADMAP 6.6.1, 6.6.5).
//!
//! The open sequence, the same for the writer ([`Database`]) and a read-only open ([`super::ReadOnlyDatabase`]):
//! 1. The container's recovery (SPEC §4.5.1): header, active superblock slot, the frames of `wal_start..log_end`,
//!    the scan from `log_end`. [`Container`] does it before anything here runs.
//! 2. The committed manifest: its frame, decoded, checked against the slot and the local layout (ROADMAP 6.3.7).
//! 3. Each collection's segments and tombstone sidecars, opened through the container's objects and checked against
//!    their manifest entries, and an empty memtable per collection.
//! 4. The WAL chunks from `wal_start` on, replayed in log order into the memtables (SPEC §8.2.1), stopping at the
//!    first Manifest frame after `log_end` whose `seq` is newer than the loaded manifest (SPEC §8.6).
//! 5. Ready: the first snapshot shows the segments and the replayed memtables.
//!
//! Nothing before step 4 writes but the container's tail repair, and replay writes only to memtables, except that
//! the writer flushes a replayed memtable that reaches its size threshold or runs out of docids, as a write would (ROADMAP 6.6.5): one
//! collection's unflushed record holds `wal_start` back for every collection (SPEC §8.2.1), so a backlog can exceed
//! a memtable. Such a commit keeps `wal_start` at the chunk being replayed, since its later records are not in a
//! memtable yet; a crash after it replays that chunk again, which is harmless (replay is idempotent, SPEC §8.2.1).
//! So an open that is interrupted and repeated reaches the same documents.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_format::frame::FrameKind;
use persia_format::superblock::Superblock;
use persia_storage::Storage;
use persia_storage::container::{Container, ContainerFile};

use super::{
    Collection, ContainerObjects, Database, DatabaseError, DatabaseOptions, LiveSegment, objects,
    open_source,
};
use crate::manifest::{Layout, MAX_MANIFEST_LEN, Manifest, SegmentEntry};
use crate::memtable::{Memtable, MemtableError};
use crate::schema::CollectionSchema;
use crate::segment::SegmentError;
use crate::tombstone::TombstoneSidecar;
use crate::wal::{MAX_CHUNK_LEN, ReplayError, WalRecord, WalWriter, apply, decode_chunk};

impl<F: ContainerFile + Sync> Database<F> {
    /// Steps 2 to 5 of the open sequence (module docs) on a container opened read-write.
    pub(super) fn recover(
        container: Container<F>,
        options: DatabaseOptions,
    ) -> Result<Self, DatabaseError> {
        let (manifest, collections) =
            load(&container, |schema| super::new_memtable(schema, &options))?;
        let wal = WalWriter::resume(container, BTreeMap::new())
            .with_chunk_target(options.wal_chunk_target);
        let mut db = Self::assemble(wal, manifest, collections, options);
        db.replay()?;
        // Replayed operations count as written at the open: their age is not logged (SPEC §8.2).
        let now = db.options.clock.now();
        for c in db.collections.values_mut() {
            if !c.memtable.is_empty() {
                c.first_write = Some(now);
                c.last_write = Some(now);
            }
        }
        Ok(db)
    }

    /// Step 4: replays every chunk from `wal_start` that belongs to the loaded snapshot into the memtables,
    /// flushing as a write would.
    fn replay(&mut self) -> Result<(), DatabaseError> {
        let seq = self.manifest.seq();
        for chunk in wal_chunks(self.wal.log(), seq)? {
            let frame = self
                .wal
                .log()
                .read_frame_source(chunk, MAX_CHUNK_LEN)
                .map_err(ReplayError::from)?;
            self.replaying = Some(chunk);
            let replayed = for_each_record(chunk, &frame.payload, |offset, record| {
                self.replay_record(chunk, offset, record)
            });
            self.replaying = None;
            replayed?;
        }
        Ok(())
    }

    /// Applies one replayed record (ROADMAP 6.6.5): first commits if its memtable reached the size threshold, and
    /// commits and applies it again if the memtable has no room left for it (no docid, operation or id left).
    fn replay_record(
        &mut self,
        chunk: u64,
        offset: u64,
        record: &WalRecord<'_>,
    ) -> Result<(), DatabaseError> {
        let name = record.collection();
        let unknown = || ReplayError::UnknownCollection {
            chunk,
            offset,
            collection: name.to_owned(),
        };
        if !self.collections.contains_key(name) {
            return Err(unknown().into());
        }
        self.flush_if_full(name)?;
        let c = self.collections.get_mut(name).ok_or_else(unknown)?;
        match apply(&mut c.memtable, chunk, offset, record) {
            Err(ReplayError::Memtable { error, .. })
                if is_capacity(&error) && !c.memtable.is_empty() =>
            {
                self.commit()?;
                let c = self.collections.get_mut(name).ok_or_else(unknown)?;
                apply(&mut c.memtable, chunk, offset, record)?;
            }
            other => other?,
        }
        self.wal.replayed(name, chunk);
        Ok(())
    }
}

/// Whether a memtable rejected an operation only because it is full: an empty one would take it.
fn is_capacity(error: &MemtableError) -> bool {
    matches!(
        error,
        MemtableError::Segment(SegmentError::Full { .. } | SegmentError::TooLarge { .. })
    )
}

/// Steps 2 and 3 of the open sequence (module docs): the committed manifest, and each collection with its segments
/// open and an empty memtable from `memtable`.
pub(super) fn load<F: ContainerFile + Sync>(
    container: &Container<F>,
    mut memtable: impl FnMut(&CollectionSchema) -> Result<Memtable, MemtableError>,
) -> Result<(Manifest, BTreeMap<String, Collection>), DatabaseError> {
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
    {
        let objects = ContainerObjects::new(container, &manifest);
        for entry in manifest.collections() {
            let name = entry.name();
            let schema = Arc::new(entry.schema().clone());
            let mut segments = Vec::with_capacity(entry.segments().len());
            for segment in entry.segments() {
                segments.push(open_segment(&objects, name, segment, &schema)?);
            }
            let collection = Collection {
                memtable: memtable(&schema)?,
                schema,
                segments,
                first_write: None,
                last_write: None,
            };
            collections.insert(name.to_owned(), collection);
        }
    }
    Ok((manifest, collections))
}

/// The positions of the `WalChunk` frames of the snapshot loaded at `loaded_seq`, in log order (SPEC §8.6).
///
/// That is every `WalChunk` from the committed `wal_start` on, including those recovery found after `log_end`,
/// until the first Manifest frame after `log_end` whose `seq` is greater than `loaded_seq`. A read-only open
/// holds no lock, so a writer may commit while the tail is scanned: the new Manifest and the WAL logged after
/// it (a collection that manifest creates, then a put) are the next snapshot's. Replaying them onto the loaded
/// manifest fails with [`ReplayError::UnknownCollection`] or mixes two commits. Frames before that Manifest
/// were logged against the loaded snapshot and are replayed. A crash can leave the same Manifest after
/// `log_end` without a finished superblock (SPEC §4.5); a read-write open stops there too.
pub(super) fn wal_chunks<F: ContainerFile>(
    container: &Container<F>,
    loaded_seq: u64,
) -> Result<Vec<u64>, DatabaseError> {
    let log_end = container.superblock().log_end;
    let mut chunks = Vec::new();
    for frame in container.frames() {
        if frame.offset >= log_end && frame.kind == FrameKind::Manifest {
            let source = container.read_frame_source(frame.offset, MAX_MANIFEST_LEN)?;
            let seq = Manifest::decode(&source.payload)?.seq();
            if seq > loaded_seq {
                break;
            }
        }
        if frame.kind == FrameKind::WalChunk {
            chunks.push(frame.offset);
        }
    }
    Ok(chunks)
}

/// Calls `f` with each record of the chunk at position `chunk`, and the record's offset in it, in order; returns how
/// many it applied. Stops at the first invalid record (SPEC §8.2.1) or error of `f`.
pub(super) fn for_each_record(
    chunk: u64,
    bytes: &[u8],
    mut f: impl FnMut(u64, &WalRecord<'_>) -> Result<(), DatabaseError>,
) -> Result<u64, DatabaseError> {
    let at_chunk = |error| ReplayError::Chunk { chunk, error };
    let mut records = decode_chunk(bytes).map_err(at_chunk)?;
    let mut applied = 0u64;
    loop {
        let offset = records.offset();
        let Some(record) = records.next() else {
            return Ok(applied);
        };
        f(offset, &record.map_err(at_chunk)?)?;
        applied = applied.saturating_add(1);
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

#[cfg(test)]
mod tests;
