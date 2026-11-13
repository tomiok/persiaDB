//! Chunking parameters: fixed-size chunks (SPEC §9.2, ADR-0034).
//!
//! Chunk `i` of a blob of `size` bytes holds bytes `[i·chunk_size, min((i+1)·chunk_size, size))`: every chunk but
//! the last is full and an empty blob has no chunks. The boundaries depend only on `size` and the chunk size.

use core::ops::Range;

use persia_format::BlobRef;
use persia_format::blob::BlobRefDefect;

use crate::error::Result;

/// The length of every chunk of a blob but the last (SPEC §9.1, §9.2). A writer picks it in
/// [`ChunkSize::MIN`]`..=`[`ChunkSize::MAX`] ([`ChunkSize::new`]): the floor bounds the blob manifest at 4 bytes per
/// 64 KiB of blob (ROADMAP 9.4.4). The encoding allows `1..=`[`ChunkSize::MAX`], and a reader accepts what it allows
/// ([`ChunkSize::from_encoding`]).
///
/// ```
/// use persia_blob::ChunkSize;
///
/// let cs = ChunkSize::new(64 << 10)?;
/// assert_eq!(cs.chunk_count(200 << 10)?, 4);
/// assert_eq!(cs.chunk_range(200 << 10, 3), Some((192 << 10)..(200 << 10)));
/// assert_eq!(cs.chunk_range(200 << 10, 4), None);
/// assert_eq!(ChunkSize::default().get(), 4 << 20);
/// assert!(ChunkSize::new(0).is_err());
/// assert!(ChunkSize::new(4).is_err()); // below the floor
/// assert_eq!(ChunkSize::from_encoding(4)?.get(), 4); // as a stored manifest may hold it
/// # Ok::<(), persia_blob::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkSize(u32);

impl ChunkSize {
    /// Smallest chunk size a writer uses: 64 KiB (SPEC §9.2, ROADMAP 9.4.4). With it a blob manifest is at most
    /// `76 + 4·ceil(size / 64 KiB)` bytes: 256 KiB for a 4 GiB blob, where a chunk size of 1 would need 16 GiB.
    pub const MIN: u32 = 64 << 10;
    /// Largest chunk size: 64 MiB ([`BlobRef::MAX_CHUNK_SIZE`]).
    pub const MAX: u32 = BlobRef::MAX_CHUNK_SIZE;
    /// The default chunk size: 4 MiB (SPEC §9.2).
    pub const DEFAULT: Self = Self(4 << 20);

    /// A chunk size of `bytes` for a writer: [`ChunkSize::MIN`]`..=`[`ChunkSize::MAX`].
    ///
    /// # Errors
    /// [`Error::ChunkSizeBelowMin`](crate::Error::ChunkSizeBelowMin) below [`ChunkSize::MIN`];
    /// [`Error::InvalidChunking`](crate::Error::InvalidChunking) with [`BlobRefDefect::ChunkSize`] above
    /// [`ChunkSize::MAX`].
    pub fn new(bytes: u32) -> Result<Self> {
        if bytes < Self::MIN {
            return Err(crate::Error::ChunkSizeBelowMin {
                chunk_size: bytes,
                min: Self::MIN,
            });
        }
        Self::from_encoding(bytes)
    }

    /// A chunk size of `bytes` as the encodings allow it (`1..=`[`ChunkSize::MAX`], SPEC §9.1, §9.1.1): what a
    /// reader accepts from a stored blob manifest or `BlobRef`. A writer uses [`ChunkSize::new`]; the floor is a
    /// writer's rule, so it never makes stored bytes invalid.
    ///
    /// # Errors
    /// [`Error::InvalidChunking`](crate::Error::InvalidChunking) with [`BlobRefDefect::ChunkSize`] when `bytes` is
    /// outside `1..=`[`ChunkSize::MAX`].
    pub fn from_encoding(bytes: u32) -> Result<Self> {
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
        // The encoding's range: what a reader accepts.
        assert!(matches!(
            ChunkSize::from_encoding(0),
            Err(Error::InvalidChunking(BlobRefDefect::ChunkSize {
                chunk_size: 0
            }))
        ));
        assert!(matches!(
            ChunkSize::from_encoding(ChunkSize::MAX + 1),
            Err(Error::InvalidChunking(BlobRefDefect::ChunkSize { .. }))
        ));
        assert_eq!(ChunkSize::from_encoding(1).unwrap().get(), 1);
        assert_eq!(
            ChunkSize::from_encoding(ChunkSize::MAX).unwrap().get(),
            64 << 20
        );
        // A writer's: the floor too.
        assert_eq!(ChunkSize::new(ChunkSize::MAX).unwrap().get(), 64 << 20);
        assert!(matches!(
            ChunkSize::new(ChunkSize::MAX + 1),
            Err(Error::InvalidChunking(BlobRefDefect::ChunkSize { .. }))
        ));
        assert_eq!(ChunkSize::try_from(ChunkSize::MIN).unwrap().get(), 64 << 10);
        assert_eq!(ChunkSize::default(), ChunkSize::DEFAULT);
        assert_eq!(ChunkSize::DEFAULT.get(), 4 * 1024 * 1024);
        assert!(ChunkSize::new(ChunkSize::DEFAULT.get()).is_ok());
    }

    /// ROADMAP 9.4.4: a writer's chunk size below 64 KiB is rejected, so a blob manifest is at most 4 bytes per
    /// 64 KiB of blob (a chunk size of 1 would make a 4 GiB blob's manifest 16 GiB).
    #[test]
    fn a_writer_rejects_a_chunk_size_below_the_floor() {
        assert_eq!(ChunkSize::MIN, 64 * 1024);
        for bytes in [0, 1, 4, 4096, ChunkSize::MIN - 1] {
            assert!(
                matches!(
                    ChunkSize::new(bytes),
                    Err(Error::ChunkSizeBelowMin { chunk_size, min: ChunkSize::MIN }) if chunk_size == bytes
                ),
                "{bytes}"
            );
            assert!(ChunkSize::try_from(bytes).is_err(), "{bytes}");
        }
        assert!(!ChunkSize::new(1).unwrap_err().is_corruption());
        assert_eq!(
            ChunkSize::new(ChunkSize::MIN).unwrap().get(),
            ChunkSize::MIN
        );
        // The manifest of a 4 GiB blob at the floor: 2^16 chunks, 4 bytes each.
        let four_gib = 4u64 << 30;
        assert_eq!(
            ChunkSize::new(ChunkSize::MIN)
                .unwrap()
                .chunk_count(four_gib)
                .unwrap(),
            1 << 16
        );
        // A reader still accepts what the encoding allows (a stored manifest is never invalidated by the floor).
        assert_eq!(
            ChunkSize::from_encoding(1)
                .unwrap()
                .chunk_count(four_gib - 1)
                .unwrap(),
            u32::MAX
        );
    }

    #[test]
    fn counts() {
        let cs = ChunkSize::from_encoding(4).unwrap();
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
        let one = ChunkSize::from_encoding(1).unwrap();
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
            let cs = ChunkSize::from_encoding(chunk).unwrap();
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

        /// The encoding's chunking rules agree with the ones `BlobRef` enforces (SPEC §9.1).
        #[test]
        fn agrees_with_blob_ref(
            chunk in prop_oneof![0..=ChunkSize::MAX + 1, any::<u32>()],
            size in prop_oneof![any::<u64>(), 0_u64..1 << 40, Just(u64::from(u32::MAX) << 26)],
        ) {
            let ours = ChunkSize::from_encoding(chunk).and_then(|cs| cs.chunk_count(size));
            let theirs = BlobRef::new(BlobId::from_bytes([0; 32]), size, "a/b", chunk, BTreeMap::new());
            prop_assert_eq!(ours.is_ok(), theirs.is_ok());
            if let (Ok(count), Ok(blob)) = (ours, theirs) {
                prop_assert_eq!(count, blob.chunk_count());
            }
        }
    }
}
