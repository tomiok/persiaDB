//! Record arrays (SPEC §5.3.6): `DocStore` rows, `IdList` ids and `BlobRefs`, read through a small cache of
//! decoded blocks shared by all readers of the segment (ROADMAP 5.3.6).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use super::super::SectionKind;
use super::error::{ReadError, SegmentCorruption};
use super::stream::Stream;

/// Decoded blocks the cache keeps per segment.
const CACHED_BLOCKS: usize = 8;

/// A decoded block: its section kind, index and bytes.
type CachedBlock = (SectionKind, u32, Arc<Vec<u8>>);

/// Recently decoded blocks of the record-array sections, most recent last.
#[derive(Debug, Default)]
pub(super) struct BlockCache {
    blocks: Mutex<VecDeque<CachedBlock>>,
}

/// The cached block `(kind, index)`, moved to the most recent end.
fn lookup(
    blocks: &mut VecDeque<CachedBlock>,
    kind: SectionKind,
    index: u32,
) -> Option<Arc<Vec<u8>>> {
    let i = blocks
        .iter()
        .position(|&(k, b, _)| k == kind && b == index)?;
    let entry = blocks.remove(i)?;
    let block = Arc::clone(&entry.2);
    blocks.push_back(entry);
    Some(block)
}

impl BlockCache {
    fn get(&self, stream: &Stream<'_>, index: u32) -> Result<Arc<Vec<u8>>, ReadError> {
        let kind = stream.kind;
        {
            let mut blocks = self.blocks.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(block) = lookup(&mut blocks, kind, index) {
                return Ok(block);
            }
        }
        // Decoded outside the lock, so readers of other blocks do not wait on it.
        let mut buf = Vec::new();
        stream.read_block(index, &mut buf)?;
        let block = Arc::new(buf);
        let mut blocks = self.blocks.lock().unwrap_or_else(PoisonError::into_inner);
        // Another reader may have decoded it meanwhile: keep one copy.
        if let Some(cached) = lookup(&mut blocks, kind, index) {
            return Ok(cached);
        }
        if blocks.len() >= CACHED_BLOCKS {
            blocks.pop_front();
        }
        blocks.push_back((kind, index, Arc::clone(&block)));
        Ok(block)
    }

    /// Fills `out` with the stream bytes from `start`, block by block through the cache.
    fn read(&self, stream: &Stream<'_>, start: u64, out: &mut [u8]) -> Result<(), ReadError> {
        let end = start
            .checked_add(u64::try_from(out.len()).unwrap_or(u64::MAX))
            .filter(|&e| e <= stream.len())
            .ok_or_else(|| {
                stream.corrupt(
                    start,
                    SegmentCorruption::Invalid {
                        what: "a record past the end of its stream",
                    },
                )
            })?;
        let block_size = stream.block_size();
        let mut pos = start;
        let mut written = 0_usize;
        while pos < end {
            let index = pos.checked_div(block_size).unwrap_or(0);
            let block_start = index.saturating_mul(block_size);
            let block = self.get(stream, u32::try_from(index).unwrap_or(u32::MAX))?;
            let from = usize::try_from(pos.saturating_sub(block_start)).unwrap_or(usize::MAX);
            let to_pos = end.min(block_start.saturating_add(block_size));
            let to = usize::try_from(to_pos.saturating_sub(block_start)).unwrap_or(usize::MAX);
            let short = || {
                stream.corrupt(
                    pos,
                    SegmentCorruption::Invalid {
                        what: "a block shorter than its table says",
                    },
                )
            };
            let bytes = block.get(from..to).ok_or_else(short)?;
            let next = written.saturating_add(bytes.len());
            out.get_mut(written..next)
                .ok_or_else(short)?
                .copy_from_slice(bytes);
            written = next;
            pos = to_pos;
        }
        Ok(())
    }

    fn u64_at(&self, stream: &Stream<'_>, at: u64) -> Result<u64, ReadError> {
        let mut bytes = [0_u8; 8];
        self.read(stream, at, &mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }
}

/// Checks the shape of a record array of `count` records (SPEC §5.3.6): the stream holds the ends and the last end
/// is the stream's end. The ends in between are checked as records are read.
pub(super) fn check_records(
    stream: &Stream<'_>,
    count: u32,
    cache: &BlockCache,
) -> Result<(), ReadError> {
    let header = u64::from(count).saturating_mul(8);
    let data_len = stream.len().checked_sub(header).ok_or_else(|| {
        stream.corrupt(
            0,
            SegmentCorruption::Invalid {
                what: "a record array shorter than its ends",
            },
        )
    })?;
    let last_at = header.saturating_sub(8);
    if cache.u64_at(stream, last_at)? != data_len {
        return Err(stream.corrupt(
            last_at,
            SegmentCorruption::Invalid {
                what: "a last record end other than the data length",
            },
        ));
    }
    Ok(())
}

/// Record `docid` of a record array of `count` records. One longer than `max` bytes (the longest valid record of the
/// section) is corruption, reported before anything is allocated.
pub(super) fn record(
    stream: &Stream<'_>,
    count: u32,
    docid: u32,
    max: u64,
    cache: &BlockCache,
) -> Result<Vec<u8>, ReadError> {
    let header = u64::from(count).saturating_mul(8);
    let end_at = u64::from(docid).saturating_mul(8);
    let end = cache.u64_at(stream, end_at)?;
    let start = match end_at.checked_sub(8) {
        Some(at) => cache.u64_at(stream, at)?,
        None => 0,
    };
    let data_len = stream.len().saturating_sub(header);
    if start > end || end > data_len {
        return Err(stream.corrupt(
            end_at,
            SegmentCorruption::Invalid {
                what: "record ends that decrease or pass the data",
            },
        ));
    }
    let len = end.saturating_sub(start);
    if len > max {
        return Err(stream.corrupt(
            end_at,
            SegmentCorruption::Invalid {
                what: "a record longer than any valid one",
            },
        ));
    }
    let len_usize = usize::try_from(len).unwrap_or(usize::MAX);
    let mut out = Vec::new();
    out.try_reserve_exact(len_usize)
        .map_err(|_| stream.error(persia_format::Error::OutOfMemory { bytes: Some(len) }))?;
    out.resize(len_usize, 0);
    cache.read(stream, header.saturating_add(start), &mut out)?;
    Ok(out)
}
