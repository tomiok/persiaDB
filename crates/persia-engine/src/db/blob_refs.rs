//! The blob references GC marks (SPEC §9.5, ROADMAP 9.5.1): every `BlobRef` the writer's view can still show a
//! reader or a replay. That is the documents live in the current state (its segments as their tombstones leave them,
//! and its memtables whole) and in every older snapshot a reader still holds, every document of the obsolete segments
//! still held (SPEC §8.5: older snapshots and merges in flight), and every put of the WAL from the committed
//! `wal_start` on, the records after `log_end` included (what a reopen replays, and what `verify` checks).
//!
//! The engine never looks a reference up (it does not depend on `persia-blob`): this only lists ids, and the blob
//! store's GC sweeps what no id names.

use std::collections::HashSet;
use std::sync::{Arc, Weak};

use persia_format::BlobId;
use persia_format::frame::FrameKind;
use persia_storage::container::ContainerFile;

use super::{Database, DatabaseError, Snapshot};
use crate::document::Document;
use crate::manifest::SegmentId;
use crate::memtable::MemtableReader;
use crate::segment::{Segment, Tombstones};
use crate::wal::{MAX_CHUNK_LEN, ReplayError, WalOp, decode_chunk};

impl<F: ContainerFile + Sync> Database<F> {
    /// The ids of every blob the writer's view references (see the module docs): what GC's mark starts from (SPEC
    /// §9.5). A replaced or deleted document still counts while a memtable holds it, a segment's deleted document
    /// counts in the snapshots taken before its deletion and in obsolete segments still held, and the WAL's puts count
    /// whatever later records did to them.
    ///
    /// Reads the `BlobRefs` of every segment it visits (one record per document) and every WAL chunk from
    /// `wal_start` on; memory is one id per referenced blob. The writer's state cannot change while it runs (`&self`),
    /// and readers in this process only drop what they hold, so nothing it found unreferenced can become referenced
    /// before the next write.
    ///
    /// ```
    /// use persia_engine::db::{Database, DatabaseOptions};
    /// use persia_engine::document::Document;
    /// use persia_engine::schema::SchemaBuilder;
    /// use persia_engine::wal::Durability;
    /// use persia_format::{BlobId, BlobRef};
    ///
    /// # if cfg!(miri) { return Ok(()); } // a real container file: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let mut db = Database::create(dir.path().join("app.persia"), DatabaseOptions::default())?;
    /// db.create_collection(SchemaBuilder::new("files").build()?)?;
    /// let blob = BlobRef::new(BlobId::from_bytes([7; 32]), 3, "text/plain", 1 << 16, Default::default())?;
    /// let mut doc = Document::new("a");
    /// doc.set_blob_ref(Some(blob.clone()));
    /// db.put("files", &doc, Durability::Fsync)?;
    /// assert!(db.referenced_blobs()?.contains(&blob.blob_id()));
    /// db.commit()?;
    /// assert!(db.referenced_blobs()?.contains(&blob.blob_id()));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`DatabaseError::Poisoned`] after a failed write (what reached the disk is unknown until a reopen, so no sweep
    /// may trust this view); [`DatabaseError::ReadSegment`] for a segment whose `BlobRefs` cannot be read;
    /// [`DatabaseError::Replay`] for a WAL chunk that cannot be read or decoded.
    pub fn referenced_blobs(&self) -> Result<HashSet<BlobId>, DatabaseError> {
        if self.poisoned {
            return Err(DatabaseError::Poisoned);
        }
        let mut ids = HashSet::new();
        for (name, c) in &self.collections {
            for live in &c.segments {
                let deleted = live.deleted.as_ref().map(|(_, t)| &**t);
                segment_refs(name, &live.segment, deleted, &mut ids)?;
            }
            memtable_refs(&c.memtable.reader(), &mut ids)?;
        }
        for snapshot in self.retired.iter().filter_map(Weak::upgrade) {
            snapshot_refs(&snapshot, &mut ids)?;
        }
        for (name, segment) in self.merges.obsolete.held_segments() {
            segment_refs(name, &segment, None, &mut ids)?;
        }
        self.wal_refs(&mut ids)?;
        Ok(ids)
    }

    /// The blob ids of the WAL's puts from the committed `wal_start` on, those after `log_end` included: the records
    /// a reopen would replay (SPEC §8.6). Records still buffered in memory are in the memtables.
    fn wal_refs(&self, ids: &mut HashSet<BlobId>) -> Result<(), DatabaseError> {
        let container = self.wal.log();
        for frame in container.frames() {
            if frame.kind != FrameKind::WalChunk {
                continue;
            }
            let chunk = frame.offset;
            let source = container
                .read_frame_source(chunk, MAX_CHUNK_LEN)
                .map_err(ReplayError::Container)?;
            let at_chunk = |error| ReplayError::Chunk { chunk, error };
            let mut records = decode_chunk(&source.payload).map_err(at_chunk)?;
            loop {
                let offset = records.offset();
                let Some(record) = records.next() else {
                    break;
                };
                let record = record.map_err(at_chunk)?;
                if record.op() != WalOp::Put {
                    continue;
                }
                let blob = Document::encoded_blob_ref(record.payload()).map_err(|error| {
                    ReplayError::Document {
                        chunk,
                        offset,
                        error,
                    }
                })?;
                ids.extend(blob.map(|b| b.blob_id()));
            }
        }
        Ok(())
    }

    /// Publishes `snapshot` as the latest (SPEC §8.4) and keeps a weak reference to the one it replaces, which readers
    /// may still hold, for [`Database::referenced_blobs`].
    pub(super) fn publish(&mut self, snapshot: Snapshot) {
        let old = self.published.swap(Arc::new(snapshot));
        // A count of 0 is final (nothing can upgrade it again), so this only drops snapshots no reader holds.
        self.retired.retain(|w| w.strong_count() > 0);
        self.retired.push(Arc::downgrade(&old));
    }
}

/// The ids of an older snapshot's documents: its segments as its tombstones leave them, and its memtables whole.
fn snapshot_refs(snapshot: &Snapshot, ids: &mut HashSet<BlobId>) -> Result<(), DatabaseError> {
    for (name, c) in snapshot.collections() {
        for (segment, deleted) in c.segments.iter().zip(&c.tombstones) {
            segment_refs(name, segment, deleted.as_deref(), ids)?;
        }
        memtable_refs(&c.memtable, ids)?;
    }
    Ok(())
}

/// The ids of `segment`'s documents that `deleted` does not delete, from its `BlobRefs` (SPEC §9.1).
fn segment_refs(
    collection: &str,
    segment: &Segment,
    deleted: Option<&Tombstones>,
    ids: &mut HashSet<BlobId>,
) -> Result<(), DatabaseError> {
    for docid in 0..segment.doc_count() {
        if deleted.is_some_and(|t| t.contains(docid)) {
            continue;
        }
        let blob = segment
            .blob_ref(docid)
            .map_err(|source| DatabaseError::ReadSegment {
                collection: collection.to_owned(),
                id: SegmentId::new(segment.segment_id()),
                source,
            })?;
        ids.extend(blob.map(|b| b.blob_id()));
    }
    Ok(())
}

/// The ids of every document a memtable holds, replaced and deleted ones included.
fn memtable_refs(
    memtable: &MemtableReader,
    ids: &mut HashSet<BlobId>,
) -> Result<(), DatabaseError> {
    memtable.for_each_encoded(|bytes| {
        // The memtable encoded these bytes itself: a failure is a bug, not corruption.
        let blob = Document::encoded_blob_ref(bytes).map_err(|_| DatabaseError::Internal {
            what: "a memtable document whose encoding does not decode",
        })?;
        ids.extend(blob.map(|b| b.blob_id()));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use persia_format::BlobRef;
    use persia_storage::container::sim::{CrashLoss, SimFile};

    use super::*;
    use crate::db::DatabaseOptions;
    use crate::schema::SchemaBuilder;
    use crate::wal::Durability;

    fn blob(seed: u8) -> BlobRef {
        BlobRef::new(
            BlobId::from_bytes([seed; 32]),
            1,
            "a/b",
            64 << 10,
            BTreeMap::new(),
        )
        .unwrap()
    }

    fn put(db: &mut Database<SimFile>, id: &str, seed: Option<u8>) {
        let mut doc = Document::new(id);
        doc.set_blob_ref(seed.map(blob));
        db.put("c", &doc, Durability::Fsync).unwrap();
    }

    fn wal_refs(db: &Database<SimFile>) -> HashSet<BlobId> {
        let mut ids = HashSet::new();
        db.wal_refs(&mut ids).unwrap();
        ids
    }

    /// The WAL's puts from `wal_start` on count, written or replayed, whatever later records did; a commit moves
    /// `wal_start` past them (SPEC §8.2.1). The memtables hold the same documents, so only a direct look tells the
    /// WAL is read at all.
    #[test]
    fn the_wals_puts_from_wal_start_on_are_read() {
        let file = SimFile::new();
        let mut db =
            Database::create_on(file.clone(), [1; 16], DatabaseOptions::default()).unwrap();
        db.create_collection(SchemaBuilder::new("c").build().unwrap())
            .unwrap();
        put(&mut db, "a", Some(1));
        put(&mut db, "b", None);
        put(&mut db, "a", Some(2));
        db.delete("c", "a", Durability::Fsync).unwrap();
        let both: HashSet<_> = [blob(1).blob_id(), blob(2).blob_id()].into();
        assert_eq!(wal_refs(&db), both);
        drop(db);
        let mut db =
            Database::open_on(file.crash(CrashLoss::Nothing), DatabaseOptions::default()).unwrap();
        assert_eq!(wal_refs(&db), both, "after a replay");
        db.commit().unwrap();
        assert!(wal_refs(&db).is_empty());
        assert!(db.referenced_blobs().unwrap().is_empty());

        // A buffered put has no WAL frame yet: only the memtable names its blob.
        let mut doc = Document::new("buffered");
        doc.set_blob_ref(Some(blob(3)));
        db.put("c", &doc, Durability::Buffered).unwrap();
        assert!(wal_refs(&db).is_empty());
        assert_eq!(db.referenced_blobs().unwrap(), [blob(3).blob_id()].into());
    }
}
