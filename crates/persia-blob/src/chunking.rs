//! Chunking parameters: fixed-size chunks (SPEC §9.2, ADR-0034).
//!
//! Chunk `i` of a blob of `size` bytes holds bytes `[i·chunk_size, min((i+1)·chunk_size, size))`: every chunk but
//! the last is full and an empty blob has no chunks. The boundaries depend only on `size` and the chunk size.

use core::ops::Range;

use persia_format::BlobRef;
use persia_format::blob::BlobRefDefect;

use crate::error::Result;

/// The length of every chunk of a blob but the last: `1..=`[`ChunkSize::MAX`] bytes (SPEC §9.1).
///
/// ```
/// use persia_blob::ChunkSize;
///
/// let cs = ChunkSize::new(4)?;
/// assert_eq!(cs.chunk_count(10)?, 3);
/// assert_eq!(cs.chunk_range(10, 2), Some(8..10));
/// assert_eq!(cs.chunk_range(10, 3), None);
/// assert_eq!(ChunkSize::default().get(), 4 << 20);
/// assert!(ChunkSize::new(0).is_err());
/// # Ok::<(), persia_blob::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkSize(u32);

impl ChunkSize {
    /// Largest chunk size: 64 MiB ([`BlobRef::MAX_CHUNK_SIZE`]).
    pub const MAX: u32 = BlobRef::MAX_CHUNK_SIZE;
    /// The default chunk size: 4 MiB (SPEC §9.2).
    pub const DEFAULT: Self = Self(4 << 20);

    /// A chunk size of `bytes`.
    ///
    /// # Errors
    /// [`Error::InvalidChunking`](crate::Error::InvalidChunking) with [`BlobRefDefect::ChunkSize`] when `bytes` is
    /// outside `1..=`[`ChunkSize::MAX`].
    pub fn new(bytes: u32) -> Result<Self> {
        if bytes == 0 || bytes > Self::MAX {
            return Err(crate::Error::InvalidChunking(BlobRefDefect::ChunkSize {
                chunk_size: bytes,
            }));
        }
        Ok(Self(bytes))
    }

    /// The chunk size in bytes.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The largest blob this chunk size can store: `u32::MAX` full chunks.
    pub fn max_blob_size(self) -> u64 {
        // At most 2^32 · 2^26 = 2^58: no overflow.
        u64::from(u32::MAX).saturating_mul(u64::from(self.0))
    }

    /// `ceil(size / chunk_size)`: the number of chunks of a blob of `size` bytes, 0 for an empty one.
    ///
    /// # Errors
    /// [`Error::InvalidChunking`](crate::Error::InvalidChunking) with [`BlobRefDefect::TooManyChunks`] when the
    /// count does not fit in a `u32` (`size` above [`ChunkSize::max_blob_size`]).
    pub fn chunk_count(self, size: u64) -> Result<u32> {
        u32::try_from(size.div_ceil(u64::from(self.0))).map_err(|_| {
            crate::Error::InvalidChunking(BlobRefDefect::TooManyChunks {
                size,
                chunk_size: self.0,
            })
        })
    }

    /// The byte range of chunk `index` of a blob of `size` bytes; `None` when the blob has no such chunk.
    pub fn chunk_range(self, size: u64, index: u32) -> Option<Range<u64>> {
        let start = u64::from(index).checked_mul(u64::from(self.0))?;
        if start >= size {
            return None;
        }
        let end = start.saturating_add(u64::from(self.0)).min(size);
        Some(start..end)
    }
}

impl Default for ChunkSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u32> for ChunkSize {
    type Error = crate::Error;

    fn try_from(bytes: u32) -> Result<Self> {
        Self::new(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use persia_format::BlobId;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    #[test]
    fn bounds() {
        assert!(matches!(
            ChunkSize::new(0),
            Err(Error::InvalidChunking(BlobRefDefect::ChunkSize {
                chunk_size: 0
            }))
        ));
        assert!(matches!(
            ChunkSize::new(ChunkSize::MAX + 1),
            Err(Error::InvalidChunking(BlobRefDefect::ChunkSize { .. }))
        ));
        assert_eq!(ChunkSize::new(1).unwrap().get(), 1);
        assert_eq!(ChunkSize::new(ChunkSize::MAX).unwrap().get(), 64 << 20);
        assert_eq!(ChunkSize::try_from(7).unwrap().get(), 7);
        assert_eq!(ChunkSize::default(), ChunkSize::DEFAULT);
        assert_eq!(ChunkSize::DEFAULT.get(), 4 * 1024 * 1024);
        assert!(ChunkSize::new(ChunkSize::DEFAULT.get()).is_ok());
    }

    #[test]
    fn counts() {
        let cs = ChunkSize::new(4).unwrap();
        assert_eq!(cs.chunk_count(0).unwrap(), 0);
        assert_eq!(cs.chunk_count(1).unwrap(), 1);
        assert_eq!(cs.chunk_count(4).unwrap(), 1);
        assert_eq!(cs.chunk_count(5).unwrap(), 2);
        assert_eq!(cs.chunk_range(0, 0), None);
        assert_eq!(cs.chunk_range(4, 0), Some(0..4));
        assert_eq!(cs.chunk_range(4, 1), None);
        assert_eq!(cs.chunk_range(5, 1), Some(4..5));
    }

    #[test]
    fn largest_blob() {
        let one = ChunkSize::new(1).unwrap();
        assert_eq!(one.max_blob_size(), u64::from(u32::MAX));
        assert_eq!(one.chunk_count(u64::from(u32::MAX)).unwrap(), u32::MAX);
        assert!(matches!(
            one.chunk_count(u64::from(u32::MAX) + 1),
            Err(Error::InvalidChunking(BlobRefDefect::TooManyChunks {
                size: 4_294_967_296,
                chunk_size: 1
            }))
        ));
        let max = ChunkSize::new(ChunkSize::MAX).unwrap();
        assert_eq!(max.max_blob_size(), u64::from(u32::MAX) * (64 << 20));
        assert_eq!(max.chunk_count(max.max_blob_size()).unwrap(), u32::MAX);
        assert!(max.chunk_count(max.max_blob_size() + 1).is_err());
        assert!(max.chunk_count(u64::MAX).is_err());
        // The last chunk of the largest blob, and no overflow past it.
        let last = max.chunk_range(max.max_blob_size(), u32::MAX - 1).unwrap();
        assert_eq!(last.end, max.max_blob_size());
        assert_eq!(last.end - last.start, 64 << 20);
        assert_eq!(
            max.chunk_range(u64::MAX, u32::MAX).map(|r| r.end - r.start),
            Some(64 << 20)
        );
    }

    proptest! {
        /// The chunks tile `0..size` in order, all full but the last, and their number is `chunk_count`.
        #[test]
        fn ranges_tile_the_blob(chunk in 1_u32..=64, size in 0_u64..2048) {
            let cs = ChunkSize::new(chunk).unwrap();
            let count = cs.chunk_count(size).unwrap();
            let mut next = 0;
            for i in 0..count {
                let r = cs.chunk_range(size, i).unwrap();
                prop_assert_eq!(r.start, next);
                prop_assert!(r.end > r.start);
                if i + 1 < count {
                    prop_assert_eq!(r.end - r.start, u64::from(chunk));
                }
                next = r.end;
            }
            prop_assert_eq!(next, size);
            prop_assert_eq!(cs.chunk_range(size, count), None);
        }

        /// The chunking rules agree with the ones `BlobRef` enforces (SPEC §9.1).
        #[test]
        fn agrees_with_blob_ref(
            chunk in prop_oneof![0..=ChunkSize::MAX + 1, any::<u32>()],
            size in prop_oneof![any::<u64>(), 0_u64..1 << 40, Just(u64::from(u32::MAX) << 26)],
        ) {
            let ours = ChunkSize::new(chunk).and_then(|cs| cs.chunk_count(size));
            let theirs = BlobRef::new(BlobId::from_bytes([0; 32]), size, "a/b", chunk, BTreeMap::new());
            prop_assert_eq!(ours.is_ok(), theirs.is_ok());
            if let (Ok(count), Ok(blob)) = (ours, theirs) {
                prop_assert_eq!(count, blob.chunk_count());
            }
        }
    }
}
