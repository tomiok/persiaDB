//! The WAL writer (SPEC §8.2.1, ADR-0028): group commit, durability levels, chunk rotation and `wal_start`.

use std::collections::BTreeMap;

use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::{self, Container, ContainerFile};

use super::chunk::{ChunkBuilder, MAX_CHUNK_LEN, WalRecord};

/// Default size at which the writer closes its in-memory chunk and writes it out (SPEC §8.2.1).
pub const DEFAULT_CHUNK_TARGET: usize = 1 << 20;

/// How durable a write is when the call returns (SPEC §8.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// In the writer's memory until the next write-out; lost on a crash.
    Buffered,
    /// Written to the file, not synced: survives the process dying, not the machine failing.
    Async,
    /// Written and synced, with everything written before it (group commit).
    #[default]
    Fsync,
}

/// Where the WAL's chunks go: locally the container, one `WalChunk` frame per chunk.
pub trait WalLog {
    /// The error of a failed write or sync.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Appends one chunk and returns its position (locally, the frame's offset). Not durable until
    /// [`WalLog::sync`].
    ///
    /// # Errors
    /// Whatever the log reports.
    fn append_chunk(&mut self, chunk: &[u8]) -> Result<u64, Self::Error>;

    /// Makes every chunk appended so far durable.
    ///
    /// # Errors
    /// Whatever the log reports.
    fn sync(&mut self) -> Result<(), Self::Error>;

    /// The position the next chunk would take: the end of the log.
    fn end(&self) -> u64;
}

impl<F: ContainerFile> WalLog for Container<F> {
    type Error = container::Error;

    fn append_chunk(&mut self, chunk: &[u8]) -> Result<u64, Self::Error> {
        // Uncompressed: chunks are written at every sync point, where latency counts more than bytes.
        self.append(FrameKind::WalChunk, chunk, Compression::None)
            .map(|frame| frame.offset)
    }

    fn sync(&mut self) -> Result<(), Self::Error> {
        Container::sync(self)
    }

    fn end(&self) -> u64 {
        Container::end(self)
    }
}

/// Why a write to the WAL failed.
#[derive(Debug, thiserror::Error)]
pub enum WalWriteError<E: std::error::Error + 'static> {
    /// The log failed to write or sync. The writer is poisoned: whether the records it held reached the disk is
    /// known only after a reopen (SPEC §4.5).
    #[error("WAL write failed: {0}")]
    Log(#[source] E),
    /// An earlier write failed; the writer takes no more.
    #[error("the WAL writer is poisoned by an earlier failed write")]
    Poisoned,
}

/// The oldest chunk holding an unflushed record of a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// A chunk in the log, at this position.
    Written(u64),
    /// The in-memory chunk.
    Buffered,
}

/// Writes records to a [`WalLog`], grouping them into chunks and syncs (SPEC §8.2.1).
///
/// ```
/// use persia_engine::wal::{Durability, WalRecord, WalWriter};
/// use persia_storage::container::Container;
/// use persia_storage::container::sim::SimFile;
///
/// let container = Container::create_on(SimFile::new(), [7; 16])?;
/// let mut wal = WalWriter::new(container);
/// wal.append(&WalRecord::delete("notes", "n1")?, Durability::Buffered)?;
/// assert_eq!(wal.wal_start(), wal.log().end()); // nothing written yet
/// wal.append(&WalRecord::delete("notes", "n2")?, Durability::Fsync)?;
/// // Both records went out as one chunk with one sync; it is needed until "notes" is flushed.
/// assert_eq!(wal.log().frames().len(), 1);
/// assert_eq!(wal.wal_start(), wal.log().frames()[0].offset);
/// wal.flushed("notes");
/// assert_eq!(wal.wal_start(), wal.log().end());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct WalWriter<L> {
    log: L,
    chunk: ChunkBuilder,
    target: usize,
    /// Chunks written since the last sync.
    unsynced: bool,
    pending: BTreeMap<String, Pending>,
    poisoned: bool,
}

impl<L: WalLog> WalWriter<L> {
    /// A writer appending to `log`, with no records waiting for a flush.
    pub fn new(log: L) -> Self {
        Self::resume(log, BTreeMap::new())
    }

    /// A writer appending to `log` after replay: `pending` gives, per collection, the position of the oldest chunk
    /// holding one of its records ([`super::ReplaySummary::pending`]), which stay needed until it is flushed.
    pub fn resume(log: L, pending: BTreeMap<String, u64>) -> Self {
        Self {
            log,
            chunk: ChunkBuilder::new(),
            target: DEFAULT_CHUNK_TARGET,
            unsynced: false,
            pending: pending
                .into_iter()
                .map(|(c, at)| (c, Pending::Written(at)))
                .collect(),
            poisoned: false,
        }
    }

    /// Sets the size at which the in-memory chunk is written out (rotation), at most [`MAX_CHUNK_LEN`].
    #[must_use]
    pub fn with_chunk_target(mut self, bytes: usize) -> Self {
        self.target = bytes.min(MAX_CHUNK_LEN);
        self
    }

    /// The log.
    pub fn log(&self) -> &L {
        &self.log
    }

    /// The log, to append other frames (a commit's Segment and Manifest frames). [`WalWriter::sync`] first, so the
    /// records buffered so far precede them (SPEC §4.5 step 1).
    pub fn log_mut(&mut self) -> &mut L {
        &mut self.log
    }

    /// The log; records still buffered are dropped.
    pub fn into_log(self) -> L {
        self.log
    }

    /// Bytes of records in the in-memory chunk.
    pub fn buffered_len(&self) -> usize {
        if self.chunk.is_empty() {
            0
        } else {
            self.chunk.len()
        }
    }

    /// Appends one record with durability `durability`.
    ///
    /// # Errors
    /// [`WalWriteError::Log`] (which poisons the writer), [`WalWriteError::Poisoned`].
    pub fn append(
        &mut self,
        record: &WalRecord<'_>,
        durability: Durability,
    ) -> Result<(), WalWriteError<L::Error>> {
        self.append_batch(std::slice::from_ref(record), durability)
    }

    /// Appends `records` in order, then reaches `durability` once for all of them: an `Fsync` batch takes one sync
    /// (group commit). Chunks may close in between (rotation), so a crash may keep a prefix of the batch.
    ///
    /// # Errors
    /// As [`WalWriter::append`].
    pub fn append_batch(
        &mut self,
        records: &[WalRecord<'_>],
        durability: Durability,
    ) -> Result<(), WalWriteError<L::Error>> {
        self.usable()?;
        for record in records {
            if !self.chunk.fits(record) {
                self.write_out()?;
            }
            // A record always fits an empty chunk: the constructors bound it far below the limit.
            if self.chunk.push(record).is_err() {
                debug_assert!(false, "a record does not fit an empty chunk");
            }
            self.pending
                .entry(record.collection().to_owned())
                .or_insert(Pending::Buffered);
            if self.chunk.len() >= self.target {
                self.write_out()?;
            }
        }
        match durability {
            Durability::Buffered => Ok(()),
            Durability::Async => self.write_out(),
            Durability::Fsync => self.sync(),
        }
    }

    /// Writes the in-memory chunk out and syncs every chunk written since the last sync.
    ///
    /// # Errors
    /// As [`WalWriter::append`].
    pub fn sync(&mut self) -> Result<(), WalWriteError<L::Error>> {
        self.write_out()?;
        if self.unsynced {
            self.log.sync().map_err(|e| self.poison(e))?;
            self.unsynced = false;
        }
        Ok(())
    }

    /// Records that `collection`'s memtable was flushed: its records written so far are in a segment and no
    /// longer hold `wal_start` back (SPEC §8.2.1).
    pub fn flushed(&mut self, collection: &str) {
        self.pending.remove(collection);
    }

    /// The first WAL position still needed: the oldest chunk holding a record of a collection not flushed since,
    /// or the end of the log (SPEC §8.2.1). A buffered chunk goes at the end or later.
    pub fn wal_start(&self) -> u64 {
        self.pending
            .values()
            .filter_map(|p| match p {
                Pending::Written(at) => Some(*at),
                Pending::Buffered => None,
            })
            .min()
            .unwrap_or_else(|| self.log.end())
    }

    fn usable(&self) -> Result<(), WalWriteError<L::Error>> {
        if self.poisoned {
            Err(WalWriteError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn poison(&mut self, e: L::Error) -> WalWriteError<L::Error> {
        self.poisoned = true;
        WalWriteError::Log(e)
    }

    /// Appends the in-memory chunk to the log, if it has records.
    fn write_out(&mut self) -> Result<(), WalWriteError<L::Error>> {
        self.usable()?;
        let Some(bytes) = self.chunk.finish() else {
            return Ok(());
        };
        let at = match self.log.append_chunk(bytes) {
            Ok(at) => at,
            Err(e) => return Err(self.poison(e)),
        };
        for p in self.pending.values_mut() {
            if *p == Pending::Buffered {
                *p = Pending::Written(at);
            }
        }
        self.chunk.clear();
        self.unsynced = true;
        Ok(())
    }
}
