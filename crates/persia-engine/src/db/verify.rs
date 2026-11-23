//! Full verification of a database (ROADMAP 6.6.3; `persia verify`).
//!
//! A read-only open already checks everything an open needs (SPEC §8.6, ROADMAP 6.3.7): the superblock and the
//! frames of `wal_start..log_end`, the manifest against its slot, each segment's footer and each sidecar against
//! its manifest entry, and every WAL record from `wal_start` on. [`ReadOnlyDatabase::verify`] adds what no open or
//! lookup checks: every segment's whole bytes ([`Segment::verify`](crate::segment::Segment::verify), SPEC §5.5),
//! that no document id is live in two segments of a collection (SPEC §8.3), and that nothing after `log_end` is
//! corrupt. [`ReadOnlyDatabase::blob_references`] lists the documents' blob references for the blob store's checks
//! (SPEC §9.5, ROADMAP 9.5.3).

use std::collections::HashSet;

use persia_format::BlobRef;
use persia_format::frame::ScanEnd;
use persia_storage::container::ContainerFile;

use super::{DatabaseError, ReadOnlyDatabase};
use crate::document::Document;
use crate::manifest::SegmentId;
use crate::view::SearchView;
use crate::wal::ReplayError;

/// How the container's bytes after its committed `log_end` ended (SPEC §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Tail {
    /// At a frame boundary.
    Clean,
    /// At a torn frame from `offset` on: a crash during an append, which a read-write open truncates.
    Torn {
        /// Where the torn frame starts.
        offset: u64,
    },
    /// At an invalid frame from `offset` on, followed only by zeros and stale frames (a lower `frame_seq`, ADR-0013):
    /// never acknowledged data, which a read-write open truncates like a torn tail.
    Stale {
        /// Where the invalid frame starts.
        offset: u64,
    },
}

/// What [`ReadOnlyDatabase::verify`] found, for a database that passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifyReport {
    /// The `seq` of the last commit.
    pub seq: u64,
    /// Collections in the manifest.
    pub collections: usize,
    /// Live segments, over every collection.
    pub segments: usize,
    /// Documents in those segments, deleted ones included.
    pub documents: u64,
    /// Of those, the ones their tombstone sidecars delete.
    pub deleted: u64,
    /// WAL records replayed into memtables: written since the last flush.
    pub replayed: u64,
    /// Valid frames after `log_end`: WAL chunks synced since the last commit, or the frames of a commit that did not
    /// complete (SPEC §4.5.1).
    pub uncommitted_frames: usize,
    /// How the bytes after them ended.
    pub tail: Tail,
}

impl<F: ContainerFile + Sync> ReadOnlyDatabase<F> {
    /// Checks the whole database (see the module docs). Reads every segment once and keeps every live id of the
    /// collection being checked in memory.
    ///
    /// # Errors
    /// A corruption ([`DatabaseError::is_corruption`]): [`DatabaseError::OpenSegment`] for a segment that fails
    /// [`Segment::verify`](crate::segment::Segment::verify) or an id read, [`DatabaseError::LiveTwice`],
    /// [`DatabaseError::CorruptTail`] for bytes after `log_end` that are still in the file and are not a torn tail
    /// or only zeros and stale frames (a read-write open refuses or quarantines them). [`DatabaseError::OpenSegment`]
    /// also reports an I/O error while a segment is read; that one is not a corruption.
    ///
    /// [`ReadOnlyDatabase::open_container`] accepts a container already opened read-write. If that open quarantined
    /// a garbage tail and truncated it, the bytes are gone and this reports [`Tail::Clean`] (`scan_end` still names
    /// the stop, and [`persia_storage::container::Recovery::truncated_from`] says the file was cut there).
    pub fn verify(&self) -> Result<VerifyReport, DatabaseError> {
        // One state throughout: a refresh meanwhile swaps in another container and snapshot (ROADMAP 6.6.6).
        let state = self.state();
        let recovery = state.container.recovery();
        let tail = match recovery.scan_end {
            ScanEnd::Clean { .. } => Tail::Clean,
            ScanEnd::TornTail { offset, .. } => Tail::Torn { offset },
            ScanEnd::Corrupt { offset, .. } if recovery.stale_tail => Tail::Stale { offset },
            // The open already copied this tail aside and truncated it (ADR-0014). A later open scans a clean end.
            ScanEnd::Corrupt { .. } if recovery.truncated_from.is_some() => Tail::Clean,
            // An unsupported frame fails the open itself; reported as corrupt should that ever change.
            ScanEnd::Corrupt { offset, .. } | ScanEnd::Unsupported { offset, .. } => {
                return Err(DatabaseError::CorruptTail { offset });
            }
        };
        let snapshot = &state.snapshot;
        let mut report = VerifyReport {
            seq: snapshot.seq(),
            collections: 0,
            segments: 0,
            documents: 0,
            deleted: 0,
            replayed: state.replayed,
            uncommitted_frames: state.container.uncommitted_frames().len(),
            tail,
        };
        for (name, collection) in snapshot.collections() {
            report.collections = report.collections.saturating_add(1);
            let mut live = HashSet::new();
            for view in collection.segment_views() {
                let segment = view.segment();
                let failed = |source| DatabaseError::OpenSegment {
                    collection: name.to_owned(),
                    id: SegmentId::new(segment.segment_id()),
                    source,
                };
                segment.verify().map_err(failed)?;
                for docid in 0..segment.doc_count() {
                    if !view.is_live(docid) {
                        report.deleted = report.deleted.saturating_add(1);
                        continue;
                    }
                    let id = segment.id_of(docid).map_err(failed)?;
                    if let Some(id) = live.replace(id) {
                        return Err(DatabaseError::LiveTwice {
                            collection: name.to_owned(),
                            id,
                        });
                    }
                }
                report.segments = report.segments.saturating_add(1);
                report.documents = report
                    .documents
                    .saturating_add(u64::from(segment.doc_count()));
            }
        }
        Ok(report)
    }
}

/// Where [`ReadOnlyDatabase::blob_references`] found a document's blob reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlobRefSite {
    /// A document of a committed segment that the segment's committed tombstones leave live.
    Segment {
        /// The segment.
        segment: SegmentId,
    },
    /// A put of the WAL from `wal_start` on, as the open replayed it.
    Wal {
        /// The position of its chunk's frame in the container.
        chunk: u64,
        /// The record's offset in the chunk.
        offset: u64,
    },
}

/// A document's blob reference, as [`ReadOnlyDatabase::blob_references`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlobReference {
    /// The document's collection.
    pub collection: String,
    /// The document's id.
    pub id: String,
    /// The reference it holds.
    pub blob_ref: BlobRef,
    /// Where it was found.
    pub site: BlobRefSite,
}

impl<F: ContainerFile + Sync> ReadOnlyDatabase<F> {
    /// Calls `f` with every blob reference a document of this database holds where `verify` checks them (SPEC §9.5,
    /// ROADMAP 9.5.3): each document of a committed segment that its committed tombstones leave live, then every put
    /// of the WAL chunks the current state replayed (from `wal_start` on, those after `log_end` included), whatever
    /// later records did to it, as GC's mark counts them. Documents without a reference are skipped. One segment's
    /// document, or one chunk, at a time; stops at `f`'s first error. It reads one state throughout, whatever a
    /// [`refresh`](ReadOnlyDatabase::refresh) does meanwhile.
    ///
    /// The engine never looks a reference up (it does not depend on `persia-blob`): the blob store's `verify` does.
    ///
    /// ```
    /// use persia_analysis::AnalyzerRegistry;
    /// use persia_engine::db::{BlobRefSite, Database, DatabaseError, DatabaseOptions, ReadOnlyDatabase};
    /// use persia_engine::document::Document;
    /// use persia_engine::schema::SchemaBuilder;
    /// use persia_engine::wal::Durability;
    /// use persia_format::{BlobId, BlobRef};
    ///
    /// # if cfg!(miri) { return Ok(()); } // a real container file: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let mut db = Database::create(&path, DatabaseOptions::default())?;
    /// db.create_collection(SchemaBuilder::new("files").build()?)?;
    /// let mut doc = Document::new("a");
    /// doc.set_blob_ref(Some(BlobRef::new(BlobId::from_bytes([7; 32]), 3, "text/plain", 1 << 16, Default::default())?));
    /// db.put("files", &doc, Durability::Fsync)?;
    ///
    /// let reader = ReadOnlyDatabase::open(&path, &AnalyzerRegistry::builtin())?;
    /// let mut found = Vec::new();
    /// reader.blob_references(|r| Ok::<_, DatabaseError>(found.push((r.id, r.site))))?;
    /// assert!(matches!(found.as_slice(), [(id, BlobRefSite::Wal { .. })] if id == "a"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`DatabaseError::ReadSegment`] for a segment whose ids or `BlobRefs` cannot be read; [`DatabaseError::Replay`]
    /// for a WAL chunk that cannot be read or decoded; `f`'s.
    pub fn blob_references<E: From<DatabaseError>>(
        &self,
        mut f: impl FnMut(BlobReference) -> Result<(), E>,
    ) -> Result<(), E> {
        // One state throughout, as `verify`.
        let state = self.state();
        let snapshot = &state.snapshot;
        for (name, collection) in snapshot.collections() {
            for view in collection.segment_views() {
                let segment = view.segment();
                let id = SegmentId::new(segment.segment_id());
                let failed = |source| DatabaseError::ReadSegment {
                    collection: name.to_owned(),
                    id,
                    source,
                };
                for docid in 0..segment.doc_count() {
                    if !view.is_live(docid) {
                        continue;
                    }
                    let Some(blob_ref) = segment.blob_ref(docid).map_err(failed)? else {
                        continue;
                    };
                    f(BlobReference {
                        collection: name.to_owned(),
                        id: segment.id_of(docid).map_err(failed)?,
                        blob_ref,
                        site: BlobRefSite::Segment { segment: id },
                    })?;
                }
            }
        }
        let chunks = state.chunks.iter().map(|c| c.offset);
        super::blob_refs::for_each_wal_put(&state.container, chunks, |put| {
            let undecodable = |error| {
                DatabaseError::from(ReplayError::Document {
                    chunk: put.chunk,
                    offset: put.offset,
                    error,
                })
            };
            let Some(blob_ref) = Document::encoded_blob_ref(put.payload).map_err(undecodable)?
            else {
                return Ok(());
            };
            f(BlobReference {
                collection: put.collection.to_owned(),
                id: Document::encoded_id(put.payload).map_err(undecodable)?,
                blob_ref,
                site: BlobRefSite::Wal {
                    chunk: put.chunk,
                    offset: put.offset,
                },
            })
        })
    }
}
