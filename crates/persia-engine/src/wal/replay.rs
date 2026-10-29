//! WAL replay (SPEC §8.2.1, §8.6, ADR-0028): applies the chunks from `wal_start` to the collections' memtables.

use std::collections::BTreeMap;

use persia_format::frame::FrameKind;
use persia_storage::container::{self, Container, ContainerFile};

use super::chunk::{MAX_CHUNK_LEN, WalCorruption, WalError, WalOp, WalRecord, decode_chunk};
use crate::document::{DecodeError, Document};
use crate::memtable::{Memtable, MemtableError};

/// Why replay stopped. Every variant fails the open: the WAL from `wal_start` on is made of whole frames that
/// passed their CRC, so nothing here is a torn write (SPEC §8.2.1).
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The chunk at `chunk` is invalid or from a newer encoding ([`WalError::is_corruption`] tells which); its
    /// offsets are from the start of the chunk.
    #[error("WAL chunk at {chunk}: {error}")]
    Chunk {
        /// Position of the chunk.
        chunk: u64,
        /// What was wrong.
        error: WalError,
    },
    /// A record names a collection the manifest lacks (SPEC §8.2.1): corruption.
    #[error("WAL chunk at {chunk}, record at {offset}: unknown collection {collection:?}")]
    UnknownCollection {
        /// Position of the chunk.
        chunk: u64,
        /// Offset of the record in the chunk.
        offset: u64,
        /// The name.
        collection: String,
    },
    /// A put's document does not decode with its collection's schema ([`DecodeError::is_corruption`] tells whether
    /// it is corrupt or from a newer document encoding).
    #[error("WAL chunk at {chunk}, record at {offset}: {error}")]
    Document {
        /// Position of the chunk.
        chunk: u64,
        /// Offset of the record in the chunk.
        offset: u64,
        /// What was wrong.
        error: DecodeError,
    },
    /// The memtable rejected a record (a document it cannot index, or no docid left).
    #[error("WAL chunk at {chunk}, record at {offset}: {error}")]
    Memtable {
        /// Position of the chunk.
        chunk: u64,
        /// Offset of the record in the chunk.
        offset: u64,
        /// What was wrong.
        error: MemtableError,
    },
    /// Reading a chunk from the container failed.
    #[error("reading the WAL: {0}")]
    Container(#[from] container::Error),
}

impl ReplayError {
    /// Whether the WAL is corrupt, as opposed to written by a newer build or unreadable.
    pub fn is_corruption(&self) -> bool {
        match self {
            Self::Chunk { error, .. } => error.is_corruption(),
            Self::Document { error, .. } => error.is_corruption(),
            Self::UnknownCollection { .. } => true,
            Self::Memtable { .. } => false,
            Self::Container(e) => e.is_corruption(),
        }
    }
}

/// What a replay applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaySummary {
    /// Records applied.
    pub records: u64,
    /// Per collection with replayed records, the position of the oldest chunk holding one: what
    /// [`super::WalWriter::resume`] needs to keep `wal_start` there until the collection is flushed.
    pub pending: BTreeMap<String, u64>,
}

/// Applies chunks, in log order, to the memtables of the collections of the manifest replay starts from
/// (SPEC §8.6).
///
/// Replaying the same chunks again, into fresh memtables, gives the same state; replaying chunks whose records are
/// already in segments gives the same documents (SPEC §8.2.1).
#[derive(Debug)]
pub struct Replay<'m> {
    memtables: &'m mut BTreeMap<String, Memtable>,
    summary: ReplaySummary,
}

impl<'m> Replay<'m> {
    /// A replay into `memtables`, one per collection of the manifest, by name.
    pub fn new(memtables: &'m mut BTreeMap<String, Memtable>) -> Self {
        Self {
            memtables,
            summary: ReplaySummary::default(),
        }
    }

    /// Applies the records of the chunk at position `chunk` (locally, its frame's offset) in order. On an error the
    /// records before it stay applied, and replay must stop: the open fails.
    ///
    /// # Errors
    /// See [`ReplayError`].
    pub fn chunk(&mut self, chunk: u64, bytes: &[u8]) -> Result<(), ReplayError> {
        let at_chunk = |error| ReplayError::Chunk { chunk, error };
        let mut records = decode_chunk(bytes).map_err(at_chunk)?;
        loop {
            let offset = records.offset();
            let Some(record) = records.next() else {
                return Ok(());
            };
            let record = record.map_err(at_chunk)?;
            let collection = record.collection();
            let memtable = self.memtables.get_mut(collection).ok_or_else(|| {
                ReplayError::UnknownCollection {
                    chunk,
                    offset,
                    collection: collection.to_owned(),
                }
            })?;
            apply(memtable, chunk, offset, &record)?;
            self.summary.records = self.summary.records.saturating_add(1);
            self.summary
                .pending
                .entry(collection.to_owned())
                .or_insert(chunk);
        }
    }

    /// Ends the replay.
    pub fn finish(self) -> ReplaySummary {
        self.summary
    }
}

/// Replays every `WalChunk` frame of `container` from the committed `wal_start` on, those after `log_end` included
/// (SPEC §8.6), into `memtables`.
///
/// # Errors
/// See [`ReplayError`].
pub fn replay_container<F: ContainerFile>(
    container: &Container<F>,
    memtables: &mut BTreeMap<String, Memtable>,
) -> Result<ReplaySummary, ReplayError> {
    let mut replay = Replay::new(memtables);
    for frame in container.frames() {
        if frame.kind != FrameKind::WalChunk {
            continue;
        }
        let chunk = container.read_frame_source(frame.offset, MAX_CHUNK_LEN)?;
        replay.chunk(frame.offset, &chunk.payload)?;
    }
    Ok(replay.finish())
}

/// Applies one record, at `offset` in the chunk at position `chunk`, to its collection's `memtable`: a put as an
/// upsert, a delete as a delete (SPEC §8.2.1). On an error the memtable is as it was.
pub(crate) fn apply(
    memtable: &mut Memtable,
    chunk: u64,
    offset: u64,
    record: &WalRecord<'_>,
) -> Result<(), ReplayError> {
    let at_record = |error| ReplayError::Memtable {
        chunk,
        offset,
        error,
    };
    match record.op() {
        WalOp::Put => {
            let doc = Document::decode(record.payload(), memtable.schema()).map_err(|error| {
                ReplayError::Document {
                    chunk,
                    offset,
                    error,
                }
            })?;
            memtable.put(&doc).map_err(at_record)?;
        }
        WalOp::Delete => {
            // Checked by the decoder; should that check ever regress, the record is corrupt, not a delete of the
            // empty id.
            let id = record.deleted_id().ok_or(ReplayError::Chunk {
                chunk,
                error: WalError::Corrupt {
                    offset,
                    reason: WalCorruption::Id,
                },
            })?;
            memtable.delete(id).map_err(at_record)?;
        }
    }
    Ok(())
}
