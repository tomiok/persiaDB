//! What a read-write open does with the bytes after a scan that ends in corruption (SPEC §4.4, §4.5.1; ADR-0014;
//! ROADMAP 2.4.10).
//!
//! The bytes after the stop are searched for valid frames. One whose `frame_seq` is at least the one expected at
//! the stop may be an acknowledged write (a synced WAL frame behind a damaged one): it is never discarded unless the
//! caller asks for [`Repair::Quarantine`]. One with a lower `frame_seq` can never be acknowledged data (ADR-0013): a
//! leftover, e.g. a stale block exposed by a crash during a file extension. Zeros and such stale frames are
//! truncated like a torn tail. Anything else (the remains of an append whose pages reached the disk out of order) is
//! first copied to a quarantine file next to the container, then truncated.

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
    /// A valid frame at this offset whose `frame_seq` is at least the expected one.
    MayBeAcknowledged(u64),
}

/// Classifies `from..len` of `file`, where `from` is 8-aligned and a scan expected `expected` as the next
/// `frame_seq`. Frames start 8-aligned, so only aligned words are tried as frame starts.
pub(super) fn classify<F: ContainerFile>(
    file: &F,
    from: u64,
    len: u64,
    expected: u64,
) -> Result<TailContents> {
    let mut read = |offset, buf: &mut [u8]| file.read_exact_at(buf, offset);
    let mut garbage = false;
    let mut buf = Vec::new();
    let mut at = from;
    'outer: while at < len {
        let n = usize::try_from(CHUNK.min(len.saturating_sub(at))).unwrap_or(0);
        buf.resize(n, 0);
        read(at, &mut buf).map_err(Error::io("read"))?;
        for (i, word) in buf.chunks(8).enumerate() {
            let word_at = at.saturating_add((i as u64).saturating_mul(8));
            if word.starts_with(&FRAME_MAGIC) {
                let mut one = StreamScanner::new(word_at, len);
                if let Some(frame) = one.next_frame(&mut read).map_err(Error::io("read"))? {
                    if frame.frame_seq() >= expected {
                        return Ok(TailContents::MayBeAcknowledged(word_at));
                    }
                    // A stale frame: skip it whole, so bytes inside it are never taken for frames.
                    at = word_at.saturating_add(frame.total_len());
                    continue 'outer;
                }
            }
            garbage |= word.iter().any(|&b| b != 0);
        }
        at = at.saturating_add(n as u64);
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
    vfs.sync_dir(parent_or_cwd(&target))
        .map_err(Error::io("sync directory"))?;
    Ok(target)
}
