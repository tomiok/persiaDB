//! Full verification of a database (ROADMAP 6.6.3; `persia verify`).
//!
//! A read-only open already checks everything an open needs (SPEC §8.6, ROADMAP 6.3.7): the superblock and the
//! frames of `wal_start..log_end`, the manifest against its slot, each segment's footer and each sidecar against
//! its manifest entry, and every WAL record from `wal_start` on. [`ReadOnlyDatabase::verify`] adds what no open or
//! lookup checks: every segment's whole bytes ([`Segment::verify`](crate::segment::Segment::verify), SPEC §5.5),
//! that no document id is live in two segments of a collection (SPEC §8.3), and that nothing after `log_end` is
//! corrupt.

use std::collections::HashSet;

use persia_format::frame::ScanEnd;
use persia_storage::container::ContainerFile;

use super::{DatabaseError, ReadOnlyDatabase};
use crate::manifest::SegmentId;
use crate::view::SearchView;

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
    /// [`DatabaseError::CorruptTail`] for bytes after `log_end` that may be acknowledged data or are not frames (a
    /// read-write open refuses or quarantines them). [`DatabaseError::OpenSegment`] also reports an I/O error while a
    /// segment is read; that one is not a corruption.
    pub fn verify(&self) -> Result<VerifyReport, DatabaseError> {
        let recovery = self.container().recovery();
        let tail = match recovery.scan_end {
            ScanEnd::Clean { .. } => Tail::Clean,
            ScanEnd::TornTail { offset, .. } => Tail::Torn { offset },
            ScanEnd::Corrupt { offset, .. } if recovery.stale_tail => Tail::Stale { offset },
            // An unsupported frame fails the open itself; reported as corrupt should that ever change.
            ScanEnd::Corrupt { offset, .. } | ScanEnd::Unsupported { offset, .. } => {
                return Err(DatabaseError::CorruptTail { offset });
            }
        };
        let snapshot = self.snapshot();
        let mut report = VerifyReport {
            seq: snapshot.seq(),
            collections: 0,
            segments: 0,
            documents: 0,
            deleted: 0,
            replayed: self.replayed(),
            uncommitted_frames: self.container().uncommitted_frames().len(),
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
