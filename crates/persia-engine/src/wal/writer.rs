//! The WAL writer (SPEC §8.2.1, ADR-0028): group commit, durability levels, chunk rotation and `wal_start`.

use std::collections::{BTreeMap, VecDeque};

use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::{self, Container, ContainerFile};

use super::chunk::{ChunkBuilder, ChunkFull, HEADER_LEN, MAX_CHUNK_LEN, WalRecord};

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

    /// Whether the log holds chunks a replay would read (locally, a `WalChunk` frame from the committed `wal_start`
    /// on).
    fn holds_chunks(&self) -> bool;
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

    fn holds_chunks(&self) -> bool {
        self.frames().iter().any(|f| f.kind == FrameKind::WalChunk)
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
    /// A record does not fit even an empty chunk. Unreachable through [`WalRecord`]'s constructors, which bound a
    /// record far below [`MAX_CHUNK_LEN`]; nothing of the batch is buffered.
    #[error(transparent)]
    ChunkFull(#[from] ChunkFull),
}

/// [`WalWriter::new`] was given a log that holds chunks; replay them and use [`WalWriter::resume`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the log holds WAL chunks: replay them and resume the writer with what replay found")]
pub struct LogHoldsChunks;

/// Where a chunk holding records of a collection is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// In the log, at this position.
    Written(u64),
    /// The in-memory chunk.
    Buffered,
}

/// A chunk holding records of one collection.
#[derive(Debug, Clone, Copy)]
struct Held {
    /// The number of the collection's last record in the chunk; 0 for chunks found by replay.
    last: u64,
    at: At,
}

/// A point in the sequence of records a [`WalWriter`] appended, from [`WalWriter::mark`]: a flush that started
/// there covers the records appended before it (SPEC §8.2.1). Meaningful only for the writer that issued it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WalMark(u64);

/// Writes records to a [`WalLog`], grouping them into chunks and syncs (SPEC §8.2.1).
///
/// ```
/// use persia_engine::wal::{Durability, WalRecord, WalWriter};
/// use persia_storage::container::Container;
/// use persia_storage::container::sim::SimFile;
///
/// let container = Container::create_on(SimFile::new(), [7; 16])?;
/// let mut wal = WalWriter::new(container)?;
/// wal.append(&WalRecord::delete("notes", "n1")?, Durability::Buffered)?;
/// assert_eq!(wal.wal_start(), wal.log().end()); // nothing written yet
/// let flush = wal.mark(); // the memtable of "notes" is frozen here
/// wal.append(&WalRecord::delete("notes", "n2")?, Durability::Fsync)?;
/// // Both records went out as one chunk with one sync; it is needed until "notes" is flushed past "n2".
/// let chunk = wal.log().frames()[0].offset;
/// assert_eq!(wal.wal_start(), chunk);
/// wal.flushed("notes", flush);
/// assert_eq!(wal.wal_start(), chunk); // the flush covers "n1" only
/// wal.flushed("notes", wal.mark());
/// assert_eq!(wal.commit_point()?, wal.log().end());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct WalWriter<L> {
    log: L,
    chunk: ChunkBuilder,
    target: usize,
    /// Chunks written since the last sync.
    unsynced: bool,
    /// Records appended so far; records are numbered from 1.
    appended: u64,
    /// Per collection with unflushed records, the chunks holding them, oldest first. Only the last can be buffered.
    held: BTreeMap<String, VecDeque<Held>>,
    poisoned: bool,
}

impl<L: WalLog> WalWriter<L> {
    /// A writer appending to `log`, which holds no chunks yet (a new container, or one whose `wal_start` is the end
    /// of its log). A log with chunks goes through replay and [`WalWriter::resume`], which keeps them needed.
    ///
    /// # Errors
    /// [`LogHoldsChunks`] if `log` holds chunks: a writer that forgot them could move `wal_start` past them.
    pub fn new(log: L) -> Result<Self, LogHoldsChunks> {
        if log.holds_chunks() {
            return Err(LogHoldsChunks);
        }
        Ok(Self::resume(log, BTreeMap::new()))
    }

    /// A writer appending to `log` after replay: `pending` gives, per collection, the position of the oldest chunk
    /// holding one of its records ([`super::ReplaySummary::pending`]), which stay needed until a flush covers them
    /// (any [`WalWriter::mark`] does).
    pub fn resume(log: L, pending: BTreeMap<String, u64>) -> Self {
        Self {
            log,
            chunk: ChunkBuilder::new(),
            target: DEFAULT_CHUNK_TARGET,
            unsynced: false,
            appended: 0,
            held: pending
                .into_iter()
                .map(|(c, at)| {
                    let held = Held {
                        last: 0,
                        at: At::Written(at),
                    };
                    (c, VecDeque::from([held]))
                })
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

    /// The log, to append other frames (a commit's Segment and Manifest frames). Take the commit's `wal_start` from
    /// [`WalWriter::commit_point`] first, so the records buffered so far precede them (SPEC §4.5 step 1).
    pub fn log_mut(&mut self) -> &mut L {
        &mut self.log
    }

    /// The log; records still buffered are dropped.
    pub fn into_log(self) -> L {
        self.log
    }

    /// Bytes of the records in the in-memory chunk, without its header.
    pub fn buffered_len(&self) -> usize {
        self.chunk.len().saturating_sub(HEADER_LEN)
    }

    /// Appends one record with durability `durability`. The caller logs only an operation replay can apply: its
    /// collection is in the committed manifest and its memtable accepts it (SPEC §8.2.1).
    ///
    /// # Errors
    /// [`WalWriteError::Log`] (which poisons the writer), [`WalWriteError::Poisoned`], [`WalWriteError::ChunkFull`].
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
        // Unreachable through the constructors, but checked before anything is buffered, so a failed batch logs
        // nothing.
        if !records.iter().all(ChunkBuilder::fits_empty) {
            return Err(ChunkFull.into());
        }
        for record in records {
            if !self.chunk.fits(record) {
                self.write_out()?;
            }
            self.chunk.push(record)?;
            self.appended = self.appended.saturating_add(1);
            let collection = record.collection();
            if !self.held.contains_key(collection) {
                self.held.insert(collection.to_owned(), VecDeque::new());
            }
            let Some(held) = self.held.get_mut(collection) else {
                continue;
            };
            match held.back_mut() {
                Some(back) if back.at == At::Buffered => back.last = self.appended,
                _ => held.push_back(Held {
                    last: self.appended,
                    at: At::Buffered,
                }),
            }
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

    /// Writes the in-memory chunk out and syncs every chunk written since the last sync. Frames appended through
    /// [`WalWriter::log_mut`] are the caller's to sync (a container commit syncs them).
    ///
    /// # Errors
    /// [`WalWriteError::Log`] (which poisons the writer), [`WalWriteError::Poisoned`].
    pub fn sync(&mut self) -> Result<(), WalWriteError<L::Error>> {
        self.write_out()?;
        if self.unsynced {
            self.log.sync().map_err(|e| self.poison(e))?;
            self.unsynced = false;
        }
        Ok(())
    }

    /// The point a flush starts from: take it when the memtable to flush stops taking writes, and pass it to
    /// [`WalWriter::flushed`] once the segment holds that memtable.
    pub fn mark(&self) -> WalMark {
        WalMark(self.appended)
    }

    /// Records that a flush of `collection` from `mark` is in a segment (SPEC §8.2.1): its records appended before
    /// `mark` no longer hold `wal_start` back; those appended since still do. Call it once the segment frame is
    /// written and before the [`WalWriter::commit_point`] of the commit that publishes it, so that commit advances
    /// `wal_start`. If the commit then fails, the container is poisoned and no later commit uses this state; called
    /// after the commit instead, `wal_start` lags one commit.
    pub fn flushed(&mut self, collection: &str, mark: WalMark) {
        let Some(held) = self.held.get_mut(collection) else {
            return;
        };
        while held.front().is_some_and(|h| h.last <= mark.0) {
            held.pop_front();
        }
        if held.is_empty() {
            self.held.remove(collection);
        }
    }

    /// The first WAL position still needed: the oldest chunk holding a record of a collection not flushed past it,
    /// or the end of the log (SPEC §8.2.1). The buffered chunk goes at the end or later, but only if it is written
    /// before any other frame: a commit takes its `wal_start` from [`WalWriter::commit_point`] instead.
    pub fn wal_start(&self) -> u64 {
        self.held
            .values()
            .filter_map(|held| match held.front()?.at {
                At::Written(at) => Some(at),
                At::Buffered => None,
            })
            .min()
            .unwrap_or_else(|| self.log.end())
    }

    /// Writes and syncs the buffered records, then returns [`WalWriter::wal_start`]: the `wal_start` of a commit whose
    /// Segment and Manifest frames are appended next (SPEC §4.5 step 1). Without the sync, a buffered record of a
    /// collection the commit drops could land after it and fail every later replay.
    ///
    /// # Errors
    /// As [`WalWriter::sync`].
    pub fn commit_point(&mut self) -> Result<u64, WalWriteError<L::Error>> {
        self.sync()?;
        Ok(self.wal_start())
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
        for back in self.held.values_mut().filter_map(VecDeque::back_mut) {
            if back.at == At::Buffered {
                back.at = At::Written(at);
            }
        }
        self.chunk.clear();
        self.unsynced = true;
        Ok(())
    }
}
