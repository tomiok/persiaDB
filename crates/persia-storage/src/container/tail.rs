//! What a read-write open does with the bytes after a scan that ends in corruption (SPEC §4.4, §4.5.1; ADR-0014;
//! ROADMAP 2.4.10).
//!
//! The bytes after the stop are searched for valid frames. One whose `frame_seq` is at least the one expected at
//! the stop may be an acknowledged write (a synced WAL frame behind a damaged one): it is never discarded unless the
//! caller asks for [`Repair::Quarantine`]. One with a lower `frame_seq` can never be acknowledged data (ADR-0013): a
//! leftover, e.g. a stale block exposed by a crash during a file extension. Zeros and such stale frames are
//! truncated like a torn tail. Anything else (the remains of an append whose pages reached the disk out of order) is
//! first copied to a quarantine file next to the container, then truncated.

use std::io;
use std::path::{Path, PathBuf};

use persia_format::frame::{FRAME_MAGIC, StreamScanner};

use super::path::temp_path;
use super::{ContainerFile, Error, Result};
use crate::local::fs::parent_or_cwd;
use crate::vfs::Vfs;

/// How much a corruption search or quarantine copy reads at once.
const CHUNK: u64 = 64 * 1024;

/// What a read-write open may do with the bytes after a scan that ends in corruption (SPEC §4.5.1, ADR-0014).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repair {
    /// Truncate zeros and stale frames; quarantine other bytes first; refuse ([`Error::CorruptTail`]) if a frame
    /// there may hold an acknowledged write.
    #[default]
    Safe,
    /// Like [`Repair::Safe`], but frames that may hold acknowledged writes are quarantined and truncated too, so
    /// the container opens with what precedes the corruption. Their bytes stay in the quarantine file.
    Quarantine,
}

/// What follows the stop of a corrupt scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TailContents {
    /// Only zeros and frames whose `frame_seq` is below the expected one.
    Discardable,
    /// Other non-zero bytes, but no valid frame that could continue the run.
    Garbage,
    /// A valid frame at this offset whose `frame_seq` is at least the expected one, or whose flags this build does
    /// not know (a newer writer's frame, which nothing may truncate, SPEC §4.4).
    MayBeAcknowledged(u64),
    /// The search gave up: candidate frames claimed so many bytes that checking them would cost more than
    /// [`READ_BUDGET_FACTOR`] times the tail. Nothing is known, so nothing may be discarded.
    Undecided,
}

/// A search may read at most this many times the tail's length (plus one chunk): every 8-aligned word holding the
/// frame magic is checked as a frame, and a hostile tail could otherwise make each check read to the end.
const READ_BUDGET_FACTOR: u64 = 4;

/// Classifies `from..len` of `file`, where `from` is 8-aligned and a scan expected `expected` as the next
/// `frame_seq`. Frames start 8-aligned, so only aligned words are tried as frame starts. Reads are bounded by
/// [`READ_BUDGET_FACTOR`].
pub(super) fn classify<F: ContainerFile>(
    file: &F,
    from: u64,
    len: u64,
    expected: u64,
) -> Result<TailContents> {
    let tail = len.saturating_sub(from);
    let mut budget = tail
        .saturating_mul(READ_BUDGET_FACTOR)
        .saturating_add(CHUNK);
    let mut read = |offset, buf: &mut [u8]| -> core::result::Result<(), Stop> {
        budget = budget
            .checked_sub(buf.len() as u64)
            .ok_or(Stop::OverBudget)?;
        file.read_exact_at(buf, offset).map_err(Stop::Io)
    };
    match search(&mut read, from, len, expected) {
        Ok(contents) => Ok(contents),
        Err(Stop::OverBudget) => Ok(TailContents::Undecided),
        Err(Stop::Io(err)) => Err(Error::io("read")(err)),
    }
}

/// Why a search stopped early.
enum Stop {
    Io(io::Error),
    OverBudget,
}

fn search(
    read: &mut impl FnMut(u64, &mut [u8]) -> core::result::Result<(), Stop>,
    from: u64,
    len: u64,
    expected: u64,
) -> core::result::Result<TailContents, Stop> {
    let mut garbage = false;
    // A window of the file at `buf_at`, refilled only when `at` leaves it: skipping a stale frame inside the window
    // reads nothing again, so a tail of many small frames costs one pass plus each frame's own check.
    let mut buf = Vec::new();
    let mut buf_at = from;
    let mut at = from;
    while at < len {
        let window_end = buf_at.saturating_add(buf.len() as u64);
        if at < buf_at || at >= window_end {
            let n = usize::try_from(CHUNK.min(len.saturating_sub(at))).unwrap_or(0);
            buf.resize(n, 0);
            read(at, &mut buf)?;
            buf_at = at;
        }
        let i = usize::try_from(at.saturating_sub(buf_at)).unwrap_or(usize::MAX);
        let word = buf
            .get(i..buf.len().min(i.saturating_add(8)))
            .unwrap_or_default();
        if word.starts_with(&FRAME_MAGIC) {
            match StreamScanner::new(at, len).check_frame(&mut *read)? {
                Ok(frame) if frame.frame_seq() < expected => {
                    // A stale frame: skip it whole, so bytes inside it are never taken for frames.
                    at = at.saturating_add(frame.total_len());
                    continue;
                }
                Ok(_) | Err(persia_format::Error::Unsupported { .. }) => {
                    return Ok(TailContents::MayBeAcknowledged(at));
                }
                Err(_) => {}
            }
        }
        garbage |= word.iter().any(|&b| b != 0);
        at = at.saturating_add(8);
    }
    Ok(if garbage {
        TailContents::Garbage
    } else {
        TailContents::Discardable
    })
}

/// Copies `from..len` of `file` to a new file `<name>.quarantine.<from>.<nonce>.<n>` next to `path`, durably
/// (file and directory synced), and returns its path. The container is not touched.
pub(super) fn quarantine<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    path: &Path,
    file: &F,
    from: u64,
    len: u64,
) -> Result<PathBuf> {
    let target = temp_path(path, &format!(".quarantine.{from:016x}"))?;
    let copy = vfs
        .create_new(&target)
        .map_err(Error::io("create quarantine file"))?;
    copy_tail(vfs, &copy, &target, file, from, len).map_err(|err| {
        // A partial copy is useless; remove it, so failed attempts (e.g. a full disk) do not pile up.
        drop(copy);
        match vfs.remove_file(&target) {
            Ok(()) => err,
            Err(cleanup) => Error::Io {
                op: "quarantine",
                source: io::Error::other(format!(
                    "{err}; removing {} also failed: {cleanup}",
                    target.display()
                )),
            },
        }
    })?;
    Ok(target)
}

/// Copies `from..len` of `file` into `copy` at `target`, durably.
fn copy_tail<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    copy: &F,
    target: &Path,
    file: &F,
    from: u64,
    len: u64,
) -> Result<()> {
    let mut buf = Vec::new();
    let mut at = from;
    while at < len {
        let n = usize::try_from(CHUNK.min(len.saturating_sub(at))).unwrap_or(0);
        buf.resize(n, 0);
        file.read_exact_at(&mut buf, at)
            .map_err(Error::io("read"))?;
        copy.write_all_at(&buf, at.saturating_sub(from))
            .map_err(Error::io("write quarantine file"))?;
        at = at.saturating_add(n as u64);
    }
    copy.sync_data()
        .map_err(Error::io("sync quarantine file"))?;
    vfs.sync_dir(parent_or_cwd(target))
        .map_err(Error::io("sync directory"))
}
