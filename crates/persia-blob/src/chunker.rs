//! Streaming chunker: a blob's bytes from a [`Read`] → fixed-size chunks with their CRC32C, and the blob's id
//! (SPEC §9.2, ADR-0034).
//!
//! The chunks and the id depend only on the bytes and the chunk size, never on how the reader delivers them: a
//! chunk is cut only when it is full or the reader has ended. The SHA-256 runs over the bytes in order as each
//! chunk is cut, so the id is known when the last chunk is (ADR-0002).

use std::io::Read;

use persia_format::BlobId;
use persia_format::blob::BlobRefDefect;
use persia_format::checksum::crc32c;
use persia_format::widen::{u64_from_usize, usize_from_u32};

use crate::chunking::ChunkSize;
use crate::error::{Error, Result};
use crate::hash::ContentHasher;

/// The first chunk's buffer starts this small, so a small blob does not reserve a whole chunk; once a chunk is full,
/// the next one reserves the full chunk size.
const FIRST_CAPACITY: u32 = 64 << 10;

/// One chunk of a blob: its index, where it starts, its bytes and their CRC32C (SPEC §9.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    index: u32,
    offset: u64,
    crc: u32,
    bytes: Vec<u8>,
}

impl Chunk {
    /// The chunk's index in the blob, from 0.
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// The blob offset of the chunk's first byte: `index · chunk_size`.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// The CRC32C of the chunk's bytes.
    pub const fn crc32c(&self) -> u32 {
        self.crc
    }

    /// The chunk's bytes: `1..=chunk_size` of them, `chunk_size` unless it is the last chunk.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The chunk's bytes, owned.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// What a [`Chunker`] produced once its reader ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkedBlob {
    /// The SHA-256 of every byte read (ADR-0002).
    pub blob_id: BlobId,
    /// Bytes read.
    pub size: u64,
    /// The chunk size the blob was cut with.
    pub chunk_size: ChunkSize,
    /// Chunks produced: `ceil(size / chunk_size)`.
    pub chunk_count: u32,
}

#[derive(Debug)]
enum State {
    Reading,
    Ended(ChunkedBlob),
    Failed,
}

/// Cuts a blob read from `R` into chunks of a [`ChunkSize`], hashing its bytes as it goes.
///
/// Call [`Chunker::next_chunk`] until it returns `None`; [`Chunker::summary`] then holds the blob's id and size.
/// After an error the chunker is spent: later calls return [`Error::ChunkerFailed`].
///
/// ```
/// use persia_blob::{ChunkSize, Chunker, blob_id_of};
///
/// let data = b"hello, chunks";
/// let mut chunker = Chunker::new(&data[..], ChunkSize::new(5)?);
/// let mut lens = Vec::new();
/// while let Some(chunk) = chunker.next_chunk()? {
///     lens.push(chunk.bytes().len());
/// }
/// assert_eq!(lens, [5, 5, 3]);
/// let blob = chunker.summary().unwrap();
/// assert_eq!((blob.size, blob.chunk_count), (13, 3));
/// assert_eq!(blob.blob_id, blob_id_of(data));
/// # Ok::<(), persia_blob::Error>(())
/// ```
#[derive(Debug)]
pub struct Chunker<R> {
    reader: R,
    chunk_size: ChunkSize,
    hasher: ContentHasher,
    next_index: u32,
    offset: u64,
    /// Chunks this chunker may produce; `u32::MAX` but in tests (the largest count SPEC §9.1 allows).
    max_chunks: u32,
    capacity: u32,
    /// The reader returned end of input while filling the last chunk, which was therefore short: the blob has
    /// ended, even if the reader would return more bytes now.
    reader_ended: bool,
    state: State,
}

impl<R: Read> Chunker<R> {
    /// A chunker over the blob `reader` yields, cut into chunks of `chunk_size`.
    pub fn new(reader: R, chunk_size: ChunkSize) -> Self {
        Self::with_max_chunks(reader, chunk_size, u32::MAX)
    }

    fn with_max_chunks(reader: R, chunk_size: ChunkSize, max_chunks: u32) -> Self {
        Self {
            reader,
            chunk_size,
            hasher: ContentHasher::new(),
            next_index: 0,
            offset: 0,
            max_chunks,
            capacity: chunk_size.get().min(FIRST_CAPACITY),
            reader_ended: false,
            state: State::Reading,
        }
    }

    /// The next chunk, or `None` once the reader has ended (an empty blob has no chunks).
    ///
    /// Reads until the chunk is full or the reader ends; [`io::ErrorKind::Interrupted`] is retried.
    ///
    /// # Errors
    /// - [`Error::Read`] when the reader fails;
    /// - [`Error::InvalidChunking`] with [`BlobRefDefect::TooManyChunks`] when the blob needs more than `u32::MAX`
    ///   chunks;
    /// - [`Error::ChunkerFailed`] after either of those.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        match self.state {
            State::Reading => {}
            State::Ended(_) => return Ok(None),
            State::Failed => return Err(Error::ChunkerFailed),
        }
        let result = self.read_chunk();
        match &result {
            Err(_) => self.state = State::Failed,
            Ok(None) => {
                self.state = State::Ended(ChunkedBlob {
                    blob_id: self.hasher.clone().finish(),
                    size: self.offset,
                    chunk_size: self.chunk_size,
                    chunk_count: self.next_index,
                });
            }
            Ok(Some(_)) => {}
        }
        result
    }

    fn read_chunk(&mut self) -> Result<Option<Chunk>> {
        if self.reader_ended {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(usize_from_u32(self.capacity));
        (&mut self.reader)
            .take(u64::from(self.chunk_size.get()))
            .read_to_end(&mut bytes)
            .map_err(Error::Read)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let len = u64_from_usize(bytes.len());
        if self.next_index >= self.max_chunks {
            return Err(Error::InvalidChunking(BlobRefDefect::TooManyChunks {
                size: self.offset.saturating_add(len),
                chunk_size: self.chunk_size.get(),
            }));
        }
        if len == u64::from(self.chunk_size.get()) {
            self.capacity = self.chunk_size.get();
        } else {
            self.reader_ended = true;
        }
        self.hasher.update(&bytes);
        let chunk = Chunk {
            index: self.next_index,
            offset: self.offset,
            crc: crc32c(&bytes),
            bytes,
        };
        // `next_index < max_chunks ≤ u32::MAX`, and `offset + len ≤ u32::MAX · 64 MiB < 2^64`.
        self.next_index = self.next_index.saturating_add(1);
        self.offset = self.offset.saturating_add(len);
        Ok(Some(chunk))
    }

    /// The blob's id, size and chunk count once [`Chunker::next_chunk`] has returned `None`; `None` before, or after
    /// an error.
    pub fn summary(&self) -> Option<ChunkedBlob> {
        match self.state {
            State::Ended(blob) => Some(blob),
            State::Reading | State::Failed => None,
        }
    }

    /// Bytes read so far.
    pub const fn bytes_read(&self) -> u64 {
        self.offset
    }

    /// The reader, e.g. to return it to a pool once the blob is chunked.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

/// Reads all of `reader` into chunks of `chunk_size`, calling `sink` with each in order.
///
/// # Errors
/// As [`Chunker::next_chunk`], or `sink`'s error.
pub fn chunk_all<R: Read>(
    reader: R,
    chunk_size: ChunkSize,
    mut sink: impl FnMut(Chunk) -> Result<()>,
) -> Result<ChunkedBlob> {
    let mut chunker = Chunker::new(reader, chunk_size);
    while let Some(chunk) = chunker.next_chunk()? {
        sink(chunk)?;
    }
    chunker.summary().ok_or(Error::ChunkerFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::blob_id_of;
    use proptest::prelude::*;
    use std::io;

    fn io_error(kind: io::ErrorKind) -> io::Error {
        io::Error::new(kind, "test reader")
    }

    /// Delivers its data in the given piece lengths (cycled; 0 means an `Interrupted` error), like a socket.
    struct Pieces<'a> {
        data: &'a [u8],
        lens: Vec<usize>,
        next: usize,
    }

    impl Read for Pieces<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.data.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            let piece = self
                .lens
                .get(self.next % self.lens.len().max(1))
                .copied()
                .unwrap_or(usize::MAX);
            self.next += 1;
            if piece == 0 {
                return Err(io_error(io::ErrorKind::Interrupted));
            }
            let n = piece.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    /// Fails after `ok` bytes.
    struct Failing {
        ok: usize,
    }

    impl Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.ok == 0 {
                return Err(io_error(io::ErrorKind::ConnectionReset));
            }
            let n = self.ok.min(buf.len());
            buf[..n].fill(7);
            self.ok -= n;
            Ok(n)
        }
    }

    fn collect<R: Read>(mut chunker: Chunker<R>) -> (Vec<Chunk>, ChunkedBlob) {
        let mut chunks = Vec::new();
        while let Some(c) = chunker.next_chunk().unwrap() {
            chunks.push(c);
        }
        let blob = chunker.summary().unwrap();
        // Ended stays ended.
        assert_eq!(chunker.next_chunk().unwrap(), None);
        assert_eq!(chunker.summary(), Some(blob));
        (chunks, blob)
    }

    /// The chunks a slice gives, checked against SPEC §9.2 directly.
    fn check(data: &[u8], cs: ChunkSize, chunks: &[Chunk], blob: ChunkedBlob) {
        assert_eq!(blob.size, data.len() as u64);
        assert_eq!(blob.chunk_size, cs);
        assert_eq!(blob.chunk_count, cs.chunk_count(blob.size).unwrap());
        assert_eq!(blob.blob_id, blob_id_of(data));
        assert_eq!(chunks.len(), blob.chunk_count as usize);
        for (i, c) in chunks.iter().enumerate() {
            let r = cs
                .chunk_range(blob.size, u32::try_from(i).unwrap())
                .unwrap();
            assert_eq!(c.index() as usize, i);
            assert_eq!(c.offset(), r.start);
            assert_eq!(
                c.bytes(),
                &data[usize::try_from(r.start).unwrap()..usize::try_from(r.end).unwrap()]
            );
            assert_eq!(c.crc32c(), crc32c(c.bytes()));
        }
    }

    #[test]
    fn empty_blob_has_no_chunks() {
        let (chunks, blob) = collect(Chunker::new(&b""[..], ChunkSize::DEFAULT));
        assert_eq!(chunks, []);
        assert_eq!(blob.size, 0);
        assert_eq!(blob.chunk_count, 0);
        assert_eq!(blob.blob_id, blob_id_of(b""));
    }

    #[test]
    fn exact_multiple_has_no_empty_last_chunk() {
        let data = [1_u8; 12];
        let cs = ChunkSize::new(4).unwrap();
        let (chunks, blob) = collect(Chunker::new(&data[..], cs));
        assert_eq!(chunks.len(), 3);
        check(&data, cs, &chunks, blob);
    }

    #[test]
    fn summary_is_none_until_the_end() {
        let data = [9_u8; 10];
        let mut chunker = Chunker::new(&data[..], ChunkSize::new(4).unwrap());
        assert_eq!(chunker.summary(), None);
        chunker.next_chunk().unwrap().unwrap();
        assert_eq!(chunker.bytes_read(), 4);
        assert_eq!(chunker.summary(), None);
        chunker.next_chunk().unwrap().unwrap();
        chunker.next_chunk().unwrap().unwrap();
        assert_eq!(chunker.summary(), None);
        assert_eq!(chunker.next_chunk().unwrap(), None);
        assert_eq!(chunker.summary().unwrap().size, 10);
        assert_eq!(chunker.into_inner(), b"");
    }

    #[test]
    fn large_chunks_span_many_reads() {
        // Larger than the first buffer, so the capacity grows and then resets to the full chunk size.
        let data: Vec<u8> = (0..300_000_u32).map(|i| (i % 251) as u8).collect();
        let cs = ChunkSize::new(100_000).unwrap();
        let reader = Pieces {
            data: &data,
            lens: vec![8191, 0, 1, 65536],
            next: 0,
        };
        let (chunks, blob) = collect(Chunker::new(reader, cs));
        check(&data, cs, &chunks, blob);
        assert_eq!(chunks.len(), 3);
    }

    /// Returns `first`, then end of input once, then `again` (a file appended to after it was read to the end).
    struct Resumes {
        first: Vec<u8>,
        again: Vec<u8>,
        ended: bool,
    }

    impl Read for Resumes {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let src = if self.first.is_empty() {
                if !self.ended {
                    self.ended = true;
                    return Ok(0);
                }
                &mut self.again
            } else {
                &mut self.first
            };
            let n = src.len().min(buf.len());
            buf[..n].copy_from_slice(&src[..n]);
            src.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn ended_chunker_reads_no_more() {
        // A short last chunk, and a blob that ends exactly at a chunk boundary.
        for first in [&b"abc"[..], b"abcdefgh"] {
            let reader = Resumes {
                first: first.to_vec(),
                again: b"late".to_vec(),
                ended: false,
            };
            let mut chunker = Chunker::new(reader, ChunkSize::new(8).unwrap());
            assert_eq!(chunker.next_chunk().unwrap().unwrap().bytes(), first);
            assert_eq!(chunker.next_chunk().unwrap(), None);
            let blob = chunker.summary().unwrap();
            assert_eq!(chunker.next_chunk().unwrap(), None);
            assert_eq!(chunker.summary(), Some(blob));
            assert_eq!(blob.blob_id, blob_id_of(first));
            assert_eq!(blob.chunk_count, 1);
        }
    }

    #[test]
    fn reader_error_fails_the_chunker() {
        let mut chunker = Chunker::new(Failing { ok: 6 }, ChunkSize::new(4).unwrap());
        assert_eq!(chunker.next_chunk().unwrap().unwrap().bytes(), &[7; 4]);
        match chunker.next_chunk() {
            Err(Error::Read(e)) => assert_eq!(e.kind(), io::ErrorKind::ConnectionReset),
            other => panic!("expected a read error, got {other:?}"),
        }
        assert!(matches!(chunker.next_chunk(), Err(Error::ChunkerFailed)));
        assert_eq!(chunker.summary(), None);
    }

    #[test]
    fn too_many_chunks() {
        let data = [0_u8; 7];
        let cs = ChunkSize::new(2).unwrap();
        let mut chunker = Chunker::with_max_chunks(&data[..], cs, 3);
        for _ in 0..3 {
            chunker.next_chunk().unwrap().unwrap();
        }
        assert!(matches!(
            chunker.next_chunk(),
            Err(Error::InvalidChunking(BlobRefDefect::TooManyChunks {
                size: 7,
                chunk_size: 2
            }))
        ));
        assert!(matches!(chunker.next_chunk(), Err(Error::ChunkerFailed)));
        // Exactly the limit is fine.
        let (chunks, blob) = collect(Chunker::with_max_chunks(&data[..6], cs, 3));
        check(&data[..6], cs, &chunks, blob);
    }

    #[test]
    fn chunk_all_feeds_the_sink_in_order() {
        let data = b"abcdefghij";
        let cs = ChunkSize::new(3).unwrap();
        let mut seen = Vec::new();
        let blob = chunk_all(&data[..], cs, |c| {
            seen.push((c.index(), c.into_bytes()));
            Ok(())
        })
        .unwrap();
        assert_eq!(blob.blob_id, blob_id_of(data));
        assert_eq!(
            seen,
            [
                (0, b"abc".to_vec()),
                (1, b"def".to_vec()),
                (2, b"ghi".to_vec()),
                (3, b"j".to_vec())
            ]
        );
        let err = chunk_all(&data[..], cs, |c| {
            if c.index() == 1 {
                Err(Error::ChunkerFailed)
            } else {
                Ok(())
            }
        });
        assert!(matches!(err, Err(Error::ChunkerFailed)));
    }

    proptest! {
        /// Chunks, CRCs and the id match SPEC §9.2 and a one-shot hash, whatever piece sizes (and `Interrupted`
        /// errors) the reader delivers them in.
        #[test]
        fn independent_of_how_bytes_arrive(
            data in proptest::collection::vec(any::<u8>(), 0..2048),
            chunk in 1_u32..=300,
            pieces in proptest::collection::vec(0_usize..700, 1..6),
        ) {
            prop_assume!(pieces.iter().any(|&p| p > 0));
            let cs = ChunkSize::new(chunk).unwrap();
            let (whole, whole_blob) = collect(Chunker::new(&data[..], cs));
            check(&data, cs, &whole, whole_blob);
            let reader = Pieces { data: &data, lens: pieces, next: 0 };
            let (pieced, pieced_blob) = collect(Chunker::new(reader, cs));
            prop_assert_eq!(pieced, whole);
            prop_assert_eq!(pieced_blob, whole_blob);
        }
    }
}
