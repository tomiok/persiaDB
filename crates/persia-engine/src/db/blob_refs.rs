//! The blob references GC marks (SPEC §9.5, ROADMAP 9.5.1): every `BlobRef` the writer's view can still show a
//! reader or a replay. That is the documents live in the current state (its segments as their tombstones leave them,
//! and its memtables whole), in every older collection snapshot a reader in this process still holds (a [`Snapshot`],
//! or a [`CollectionSnapshot`](super::CollectionSnapshot) cloned out of one), every document of a memtable a reader
//! still holds a view of, of a live segment as an older sidecar a reader still holds leaves it, and of the obsolete
//! segments still held (SPEC §8.5: readers and merges in flight), and every put of the WAL from the committed
//! `wal_start` on, the records after `log_end` included (what a reopen replays, and what `verify` checks).
//!
//! A caller that holds a bare [`Arc<Segment>`](crate::segment::Segment) without the snapshot or the committed
//! tombstones it was read with is not tracked: it reads every document as live, deleted ones included.
//!
//! The engine never looks a reference up (it does not depend on `persia-blob`): this only lists ids, and the blob
//! store's GC sweeps what no id names.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, RwLock, Weak};

use persia_format::BlobId;
use persia_format::frame::FrameKind;
use persia_storage::container::{Container, ContainerFile};

use super::{Database, DatabaseError, Snapshot};
use crate::document::Document;
use crate::manifest::SegmentId;
use crate::memtable::{self, Inner};
use crate::segment::{Segment, Tombstones};
use crate::stats::SegmentStatistics;
use crate::wal::{MAX_CHUNK_LEN, ReplayError, WalOp, decode_chunk};

/// What a collection snapshot a commit replaced still shows, while a reader holds it.
#[derive(Debug)]
pub(super) struct RetiredView {
    /// Alive while any clone of the collection snapshot is: each one shares its statistics, which no other value
    /// holds.
    lease: Weak<OnceLock<SegmentStatistics>>,
    collection: String,
    /// Its segments, each with the deletions it showed (`None`: none).
    segments: Vec<(Weak<Segment>, Option<Weak<Tombstones>>)>,
    /// Its memtable, which a [`MemtableView`](crate::memtable::MemtableView) may hold longer than the snapshot.
    memtable: Weak<RwLock<Inner>>,
}

impl RetiredView {
    /// Nothing still shows this view. A dead lease with a memtable the new snapshot already reads is dead too: that
    /// memtable's strong count stays above 0 for as long as it is the live one, so counting it would keep one entry
    /// per publish on an idle collection. An older memtable a [`MemtableView`](crate::memtable::MemtableView) still
    /// holds, or a snapshot whose lease is alive, stays.
    fn is_dead(&self, live_memtables: &[Arc<RwLock<Inner>>]) -> bool {
        if self.lease.strong_count() != 0 {
            return false;
        }
        // A count of 0 is final: nothing can upgrade it again.
        match self.memtable.upgrade() {
            None => true,
            Some(memtable) => live_memtables.iter().any(|m| Arc::ptr_eq(m, &memtable)),
        }
    }
}

impl<F: ContainerFile + Sync> Database<F> {
    /// The ids of every blob the writer's view references (see the module docs): what GC's mark starts from (SPEC
    /// §9.5). A replaced or deleted document still counts while a memtable holds it, a segment's deleted document
    /// counts while a reader holds a snapshot or sidecar from before its deletion, or an obsolete segment, and the
    /// WAL's puts count whatever later records did to them.
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
        let mut memtables: Vec<Arc<RwLock<Inner>>> = Vec::new();
        for (name, c) in &self.collections {
            for live in &c.segments {
                let deleted = live.deleted.as_ref().map(|(_, t)| &**t);
                segment_refs(name, &live.segment, deleted, &mut ids)?;
            }
            memtables.push(Arc::clone(c.memtable.reader().shared()));
        }
        for view in &self.retired {
            if let Some(_held) = view.lease.upgrade() {
                for (segment, deleted) in &view.segments {
                    let Some(segment) = segment.upgrade() else {
                        continue;
                    };
                    // While the snapshot is held so are its deletions; were they gone, every document would count.
                    let deleted = deleted.as_ref().and_then(Weak::upgrade);
                    segment_refs(&view.collection, &segment, deleted.as_deref(), &mut ids)?;
                }
            }
            if let Some(memtable) = view.memtable.upgrade()
                && !memtables.iter().any(|m| Arc::ptr_eq(m, &memtable))
            {
                memtables.push(memtable);
            }
        }
        for memtable in &memtables {
            memtable_refs(memtable, &mut ids)?;
        }
        for (name, segment, deleted) in self.merges.obsolete.held_sidecars() {
            let live = self
                .collections
                .get(name)
                .into_iter()
                .flat_map(|c| &c.segments)
                .find(|s| SegmentId::new(s.segment.segment_id()) == segment);
            if let Some(live) = live {
                segment_refs(name, &live.segment, Some(&deleted), &mut ids)?;
            }
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
        let chunks = container
            .frames()
            .iter()
            .filter(|frame| frame.kind == FrameKind::WalChunk)
            .map(|frame| frame.offset);
        for_each_wal_put(container, chunks, |put| {
            let blob = Document::encoded_blob_ref(put.payload).map_err(|error| {
                DatabaseError::from(ReplayError::Document {
                    chunk: put.chunk,
                    offset: put.offset,
                    error,
                })
            })?;
            ids.extend(blob.map(|b| b.blob_id()));
            Ok(())
        })
    }

    /// Publishes `snapshot` as the latest (SPEC §8.4) and keeps weak references to what the one it replaces shows,
    /// which readers may still hold, for [`Database::referenced_blobs`].
    pub(super) fn publish(&mut self, snapshot: Snapshot) {
        let new = Arc::new(snapshot);
        // The memtables the snapshot just published reads. An entry whose lease is dead and whose memtable is one of
        // these is redundant: the live state already reads that memtable whole.
        let live_memtables: Vec<Arc<RwLock<Inner>>> = new
            .collections()
            .map(|(_, c)| Arc::clone(c.memtable.shared()))
            .collect();
        let old = self.published.swap(new);
        for (name, c) in old.collections() {
            self.retired.push(RetiredView {
                lease: Arc::downgrade(&c.statistics),
                collection: name.to_owned(),
                segments: c
                    .segments
                    .iter()
                    .zip(&c.tombstones)
                    .map(|(s, t)| (Arc::downgrade(s), t.as_ref().map(Arc::downgrade)))
                    .collect(),
                memtable: Arc::downgrade(c.memtable.shared()),
            });
        }
        // Drop the previous snapshot before the prune, so a lease nobody else holds counts as dead.
        drop(old);
        self.retired.retain(|v| !v.is_dead(&live_memtables));
    }
}

/// A `Put` record of the WAL, as [`for_each_wal_put`] finds it.
pub(super) struct WalPut<'a> {
    /// The offset of its WAL chunk frame.
    pub chunk: u64,
    /// Its offset in the chunk's payload.
    pub offset: u64,
    /// Its collection.
    pub collection: &'a str,
    /// The encoded document.
    pub payload: &'a [u8],
}

/// Calls `f` with every `Put` record of the WAL chunk frames at `chunks`, in log order (SPEC §8.2, §8.6). The one walk
/// of the WAL's puts that [`Database::referenced_blobs`] (GC's mark) and `ReadOnlyDatabase::blob_references`
/// (`persia verify`) share, so what they find cannot drift apart (SPEC §9.5).
///
/// # Errors
/// [`DatabaseError::Replay`] for a chunk that cannot be read or decoded; `f`'s.
pub(super) fn for_each_wal_put<F: ContainerFile, E: From<DatabaseError>>(
    container: &Container<F>,
    chunks: impl IntoIterator<Item = u64>,
    mut f: impl FnMut(WalPut<'_>) -> Result<(), E>,
) -> Result<(), E> {
    let replay = |e: ReplayError| E::from(DatabaseError::from(e));
    for chunk in chunks {
        let source = container
            .read_frame_source(chunk, MAX_CHUNK_LEN)
            .map_err(|e| replay(ReplayError::Container(e)))?;
        let at_chunk = |error| replay(ReplayError::Chunk { chunk, error });
        let mut records = decode_chunk(&source.payload).map_err(at_chunk)?;
        loop {
            let offset = records.offset();
            let Some(record) = records.next() else {
                break;
            };
            let record = record.map_err(at_chunk)?;
            if record.op() == WalOp::Put {
                f(WalPut {
                    chunk,
                    offset,
                    collection: record.collection(),
                    payload: record.payload(),
                })?;
            }
        }
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
fn memtable_refs(memtable: &RwLock<Inner>, ids: &mut HashSet<BlobId>) -> Result<(), DatabaseError> {
    memtable::read(memtable).for_each_encoded(|bytes| {
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

    /// Compaction and merge republish the snapshot without replacing the live memtable when nothing was written.
    /// With no reader holding the previous snapshot, `retired` must not grow by one entry per publish.
    #[test]
    fn publishes_of_the_same_memtable_keep_retired_bounded() {
        let mut db =
            Database::create_on(SimFile::new(), [1; 16], DatabaseOptions::default()).unwrap();
        db.create_collection(SchemaBuilder::new("c").build().unwrap())
            .unwrap();
        put(&mut db, "a", Some(1));
        for _ in 0..8 {
            let snapshot = super::super::snapshot_of(&db.manifest, &db.collections);
            db.publish(snapshot);
        }
        assert!(
            db.retired.is_empty(),
            "retired grew to {} with no held snapshot",
            db.retired.len()
        );

        // A snapshot still held, even one that reads the live memtable, stays until it is dropped: its segments
        // may be the ones a merge has since replaced.
        let held = db.snapshot();
        db.publish(super::super::snapshot_of(&db.manifest, &db.collections));
        assert_eq!(db.retired.len(), 1, "a held snapshot stays tracked");
        assert!(db.referenced_blobs().unwrap().contains(&blob(1).blob_id()));
        drop(held);
        db.publish(super::super::snapshot_of(&db.manifest, &db.collections));
        assert!(
            db.retired.is_empty(),
            "dropping the snapshot lets the next publish prune it"
        );
    }
}
