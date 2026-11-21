//! Resumable uploads (ROADMAP 9.3.2; SPEC §9.4, ADR-0034 item 6, ADR-0050 item 4).
//!
//! The resume token is the upload id and its chunk size. [`RemoteBlobStore::resume`] finds the run of full chunks
//! staged from chunk 0, re-reads and re-hashes them; [`ResumedUpload::into_chunker`] continues the cut after them. A
//! chunk write that finds other bytes ([`Error::StagedChunkConflict`]) restarts under a new upload id:
//! [`RemoteBlobStore::restage`] copies what was staged there, so the client never rewinds.

use std::io::Read;

use persia_format::checksum::crc32c;
use persia_format::widen::{u64_from_usize, usize_from_u32};
use persia_storage::Storage;

use super::{RemoteBlobStore, RemoteKey, StagedChunk, parallel};
use crate::chunker::Chunker;
use crate::chunking::ChunkSize;
use crate::error::{Error, Result};
use crate::hash::ContentHasher;
use crate::manifest::UploadId;
use crate::store::BlobStore;

/// What a client keeps to resume an upload (SPEC §9.4): chunk objects do not record the chunk size.
///
/// A token is not checked against the store: it holds only the upload id and the chunk size, so a token of an unknown
/// upload, or of another prefix or bucket, looks like an upload whose chunks are gone. The listing is then empty and
/// the resume stages the whole blob under the token's upload id (an empty blob stages nothing, so `put_blob`'s
/// ROADMAP 9.3.6 restage binds it under a new upload id). Chunks already under that id are kept only when they
/// are full chunks of the token's size from chunk 0, and then must hold the reader's bytes (else `InvalidResume`, and
/// nothing is staged). Any other chunk there with other bytes at an index the resume writes conflicts, and the upload
/// restarts under a new upload id ([`RemoteBlobStore::restage`]), leaving those chunks to GC. A token whose chunk size
/// is not the one its upload was staged with is such a case: its chunk 0 is not of the token's length (unless the
/// blob is that one short chunk, which is then kept), so the run is empty and the first staged index the resume
/// writes conflicts (chunk 0 itself may have been swept). The blob id is computed from the bytes bound either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResumeToken {
    /// The upload the chunks were staged under.
    pub upload_id: UploadId,
    /// The chunk size the upload started with.
    pub chunk_size: ChunkSize,
}

/// An upload's stored prefix: the run of full chunks from chunk 0, hashed (see the module docs).
#[derive(Debug, Clone)]
pub struct ResumedUpload {
    token: ResumeToken,
    hasher: ContentHasher,
    crcs: Vec<u32>,
    /// Every chunk index the listing showed, kept run or not: sorted, without duplicates.
    listed: Vec<u32>,
}

impl ResumedUpload {
    /// The upload and chunk size the run is staged under.
    pub const fn token(&self) -> ResumeToken {
        self.token
    }

    /// The first chunk to stage: the number of chunks kept.
    pub fn next_index(&self) -> u32 {
        // `resume` keeps at most `u32::MAX` chunks.
        u32::try_from(self.crcs.len()).unwrap_or(u32::MAX)
    }

    /// The blob offset the caller's reader must continue from: `next_index · chunk_size`.
    pub const fn offset(&self) -> u64 {
        self.hasher.len()
    }

    /// The CRC32Cs of the kept chunks, in order: the start of the blob manifest's list.
    pub fn chunk_crcs(&self) -> &[u32] {
        &self.crcs
    }

    /// The chunk indices the listing [`RemoteBlobStore::resume`] read showed, in the kept run or not: sorted
    /// ascending and without duplicates, whatever order the backend listed them in (so a caller may binary-search
    /// it). A chunk listed here that the resume stages again with the same bytes is not rewritten (SPEC §9.4:
    /// `put_if_absent`), so it does not make the upload younger; a chunk not listed here is a new write
    /// (ROADMAP 9.3.6). Without `strong_list` a listing may miss a chunk written just before it, which is young
    /// anyway.
    pub fn listed(&self) -> &[u32] {
        &self.listed
    }

    /// Reads the kept run's bytes from `reader`, a reader of the whole blob from its start, and checks each chunk of
    /// them against the CRC32C of the chunk staged there (SPEC §9.4: the resuming client passes the whole blob, and
    /// its kept bytes are not uploaded again). The reader is left at [`ResumedUpload::offset`], where
    /// [`ResumedUpload::into_chunker`] continues. One chunk of the token's size is buffered at a time.
    ///
    /// # Errors
    /// [`Error::Read`] when the reader fails; [`Error::InvalidResume`] when it ends inside the kept run or yields a
    /// chunk whose CRC32C is not the staged one's: it is not the blob the upload was staging (a caller's error).
    pub fn skip_kept<R: Read>(&self, reader: &mut R) -> Result<()> {
        let full = u64::from(self.token.chunk_size.get());
        let mut buf = Vec::new();
        for &crc in &self.crcs {
            buf.clear();
            reader
                .by_ref()
                .take(full)
                .read_to_end(&mut buf)
                .map_err(Error::Read)?;
            if u64_from_usize(buf.len()) != full {
                return Err(Error::InvalidResume {
                    what: "the reader ends inside the chunks the upload kept",
                });
            }
            if crc32c(&buf) != crc {
                return Err(Error::InvalidResume {
                    what: "the reader's bytes are not the ones the upload staged",
                });
            }
        }
        Ok(())
    }

    /// A chunker that continues the blob after the kept chunks, over `reader` (which yields the blob from
    /// [`ResumedUpload::offset`] on), and the kept chunks' CRCs, which the caller extends with the new chunks'.
    ///
    /// # Errors
    /// None in practice: the hasher has seen exactly the kept chunks ([`Chunker::resume`]).
    pub fn into_chunker<R: Read>(self, reader: R) -> Result<(Chunker<R>, Vec<u32>)> {
        let next = self.next_index();
        let chunker = Chunker::resume(reader, self.token.chunk_size, self.hasher, next)?;
        Ok((chunker, self.crcs))
    }
}

impl<S: Storage> RemoteBlobStore<S> {
    /// Finds what `token`'s upload has stored. The listing is sorted by index and duplicate keys are collapsed first
    /// (a backend's order is its own, and a key may be listed twice; SPEC §9.4, §10.1): repeats of one index with the
    /// same length collapse to one, and repeats whose lengths differ end the run before that index. Then the run of
    /// chunks `0, 1, …` whose listed length is the chunk size is re-read (up to `parallelism` at a time), checked to
    /// be full again, hashed in order and its CRC32C kept. A chunk missing, of another length, or whose read reports
    /// corruption ends the run; it and later ones are written again (same bytes succeed, other bytes conflict, and a
    /// corrupt object that is still there conflicts, so the upload restarts under a new id). Without `strong_list` a
    /// just-written chunk may be missed, which only shortens the run.
    ///
    /// The caller pins `token.upload_id` before calling (SPEC §9.5).
    ///
    /// # Errors
    /// The backend's errors.
    pub fn resume(&self, token: ResumeToken) -> Result<ResumedUpload> {
        let full = u64::from(token.chunk_size.get());
        let listed = self.staged_chunks(token.upload_id)?;
        let indices = sorted_indices(&listed);
        let ordered = chunks_for_run(listed);
        let run = ordered
            .iter()
            .zip(0..u32::MAX)
            .take_while(|(chunk, index)| chunk.index == *index && chunk.size == full)
            .count();
        let mut resumed = ResumedUpload {
            token,
            hasher: ContentHasher::new(),
            crcs: Vec::new(),
            listed: indices,
        };
        let window = self.parallelism.get();
        let mut start = 0_usize;
        while start < run {
            let len = window.min(run.saturating_sub(start));
            let chunks = parallel::run(len, self.parallelism, |i| {
                let index = chunk_index(start, i)?;
                match self.get(
                    &RemoteKey::Chunk {
                        upload_id: token.upload_id,
                        index,
                    }
                    .key()?,
                ) {
                    Ok(bytes) => Ok(bytes),
                    // A corrupt object is not part of the run (SPEC §9.4): staging the index later conflicts
                    // if the object is still there, and the upload restarts under a new id.
                    Err(e) if e.is_corruption() => Ok(None),
                    Err(e) => Err(e),
                }
            })?;
            for bytes in chunks {
                match bytes {
                    Some(bytes) if u64_from_usize(bytes.len()) == full => {
                        resumed.hasher.update(&bytes);
                        resumed.crcs.push(crc32c(&bytes));
                    }
                    // Gone, not full, or corrupt since the listing: the run ends here.
                    _ => return Ok(resumed),
                }
            }
            start = start.saturating_add(len);
        }
        Ok(resumed)
    }

    /// The restart after a [`Error::StagedChunkConflict`] (SPEC §9.4): copies chunks `0..chunk_crcs.len()` of the
    /// upload `from` (full chunks of `from.chunk_size`, the run [`RemoteBlobStore::resume`] kept plus those staged
    /// since) under `to`, a new upload the caller pinned first (SPEC §9.5). Each chunk is re-read and checked against
    /// its CRC32C in `chunk_crcs` before it is staged again, so the client never rewinds: it stages the conflicting
    /// chunk and the rest under `to`.
    ///
    /// # Errors
    /// [`Error::InvalidResume`] when a chunk is not of the token's full chunk size (e.g. a short last chunk);
    /// [`Error::Corrupt`] when a chunk vanished or its CRC32C changed; [`Error::StagedChunkConflict`] when `to` already
    /// holds other bytes; the backend's errors.
    pub fn restage(&self, from: ResumeToken, chunk_crcs: &[u32], to: UploadId) -> Result<()> {
        let full = u64::from(from.chunk_size.get());
        let window = self.parallelism.get();
        let mut start = 0_usize;
        while start < chunk_crcs.len() {
            let len = window.min(chunk_crcs.len().saturating_sub(start));
            parallel::run(len, self.parallelism, |i| {
                let index = chunk_index(start, i)?;
                let key = RemoteKey::Chunk {
                    upload_id: from.upload_id,
                    index,
                }
                .key()?;
                let expected = chunk_crcs.get(usize_from_u32(index)).copied();
                match self.get(&key)? {
                    // Only full chunks are restaged: the caller passed the CRCs of more than were staged full.
                    Some(bytes) if u64_from_usize(bytes.len()) != full => {
                        Err(Error::InvalidResume {
                            what: "a chunk to restage is not a full chunk of the token's chunk size",
                        })
                    }
                    Some(bytes) if Some(crc32c(&bytes)) == expected => {
                        self.stage_chunk(to, index, &bytes)
                    }
                    _ => Err(Error::Corrupt {
                        encoding: "blob chunk",
                        what: "a staged chunk vanished or changed before it was restaged",
                    }),
                }
            })?;
            start = start.saturating_add(len);
        }
        Ok(())
    }
}

/// The listing sorted by index, with duplicate keys collapsed, up to the first index whose repeats disagree on
/// length: that index is not part of the run (neither length can be trusted), and neither is anything after it,
/// because the run is a prefix from chunk 0 (SPEC §9.4). Repeats of the same length collapse to one.
fn chunks_for_run(mut listed: Vec<StagedChunk>) -> Vec<StagedChunk> {
    listed.sort_by_key(|chunk| chunk.index);
    let mut out: Vec<StagedChunk> = Vec::with_capacity(listed.len());
    for chunk in listed {
        // `copied`: the match does not borrow `out`, so the disagreeing index can be popped.
        match out.last().copied() {
            Some(prev) if prev.index == chunk.index && prev.size != chunk.size => {
                out.pop();
                break;
            }
            Some(prev) if prev.index == chunk.index => {}
            _ => out.push(chunk),
        }
    }
    out
}

/// The listed chunks' indices, sorted and without duplicates: a backend may list them in another order, or twice.
fn sorted_indices(listed: &[StagedChunk]) -> Vec<u32> {
    let mut indices: Vec<u32> = listed.iter().map(|chunk| chunk.index).collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

fn chunk_index(start: usize, i: usize) -> Result<u32> {
    start
        .checked_add(i)
        .and_then(|index| u32::try_from(index).ok())
        .ok_or(Error::InvalidResume {
            what: "more than u32::MAX chunks",
        })
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;
    use std::io;
    use std::sync::Arc;
    use std::time::UNIX_EPOCH;

    use persia_storage::retry::ManualClock;
    use persia_storage::{MemoryStorage, Storage};

    use super::RemoteKey;
    use super::*;
    use crate::store::MemoryBlobStore;

    /// A remote store on a manual clock (no system time, so Miri runs it), one request at a time.
    fn store() -> RemoteBlobStore<MemoryStorage> {
        let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
        RemoteBlobStore::new(MemoryStorage::with_clock(clock)).with_parallelism(NonZeroUsize::MIN)
    }

    fn token(upload: u128) -> ResumeToken {
        ResumeToken {
            upload_id: UploadId::new(upload),
            chunk_size: ChunkSize::from_encoding(4).unwrap(),
        }
    }

    /// `abcd efgh ijkl mnop q`, with chunks 0, 1, 3 and 4 staged (2 is missing): the kept run is 0 and 1.
    fn staged(store: &RemoteBlobStore<MemoryStorage>, t: ResumeToken) {
        for (index, bytes) in [
            (0, b"abcd".as_slice()),
            (1, b"efgh"),
            (3, b"mnop"),
            (4, b"q"),
        ] {
            store.stage_chunk(t.upload_id, index, bytes).unwrap();
        }
    }

    struct Failing;

    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("the reader failed"))
        }
    }

    #[test]
    fn skip_kept_reads_the_kept_run_checked_and_leaves_the_reader_after_it() {
        let store = store();
        let t = token(1);
        staged(&store, t);
        // Through the trait, which forwards to the inherent method (ADR-0053).
        let resumed = BlobStore::resume(&store, t).unwrap();
        assert_eq!(resumed.next_index(), 2);
        assert_eq!(resumed.listed(), [0, 1, 3, 4]);
        let mut reader = &b"abcdefghijklmnopq"[..];
        resumed.skip_kept(&mut reader).unwrap();
        assert_eq!(reader, b"ijklmnopq");
        // The kept run continues where the reader was left: the blob binds whole.
        let (mut chunker, crcs) = resumed.clone().into_chunker(reader).unwrap();
        assert_eq!(crcs.len(), 2);
        while chunker.next_chunk().unwrap().is_some() {}
        assert_eq!(
            chunker.summary().unwrap().blob_id,
            crate::blob_id_of(b"abcdefghijklmnopq")
        );

        for other in [&b"abcdefgX"[..], b"Xbcdefgh"] {
            assert!(
                matches!(
                    resumed.skip_kept(&mut &other[..]),
                    Err(Error::InvalidResume { .. })
                ),
                "{other:?}"
            );
        }
        for short in [&b""[..], b"abcd", b"abcdefg"] {
            assert!(matches!(
                resumed.skip_kept(&mut &short[..]),
                Err(Error::InvalidResume { .. })
            ));
        }
        assert!(matches!(
            resumed.skip_kept(&mut Failing),
            Err(Error::Read(_))
        ));
    }

    #[test]
    fn listed_is_sorted_and_without_duplicates_however_the_backend_lists() {
        use crate::testing::{ListOrder, MisorderedList};
        for order in [ListOrder::Reversed, ListOrder::Duplicated] {
            let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
            let inner = MemoryStorage::with_clock(clock);
            let store = RemoteBlobStore::new(MisorderedList { inner, order })
                .with_parallelism(NonZeroUsize::MIN);
            let t = token(1);
            for (index, bytes) in [
                (0, b"abcd".as_slice()),
                (1, b"efgh"),
                (3, b"mnop"),
                (4, b"q"),
            ] {
                store.stage_chunk(t.upload_id, index, bytes).unwrap();
            }
            let resumed = BlobStore::resume(&store, t).unwrap();
            assert_eq!(resumed.listed(), [0, 1, 3, 4], "{order:?}");
            // The kept run is the full chunks from 0, not whatever the listing returned first.
            assert_eq!(resumed.next_index(), 2, "{order:?}");
        }
    }

    #[test]
    fn a_misordered_listing_of_a_fully_staged_upload_keeps_the_whole_run() {
        use crate::testing::{ListOrder, MisorderedList};
        for order in [ListOrder::Reversed, ListOrder::Duplicated] {
            let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
            let inner = MemoryStorage::with_clock(clock);
            let store = RemoteBlobStore::new(MisorderedList { inner, order })
                .with_parallelism(NonZeroUsize::MIN);
            let t = token(1);
            for (index, bytes) in [(0, b"abcd".as_slice()), (1, b"efgh"), (2, b"ijkl")] {
                store.stage_chunk(t.upload_id, index, bytes).unwrap();
            }
            let resumed = BlobStore::resume(&store, t).unwrap();
            assert_eq!(resumed.next_index(), 3, "{order:?}: the full run");
            assert_eq!(resumed.listed(), [0, 1, 2], "{order:?}");
        }
    }

    /// Lists `inner`, then repeats the chunk at `index` with a different length.
    struct DifferingSize<S> {
        inner: S,
        upload: UploadId,
        index: u32,
    }

    impl<S: Storage> Storage for DifferingSize<S> {
        fn get_range(
            &self,
            key: &persia_storage::Key,
            range: std::ops::Range<u64>,
        ) -> persia_storage::Result<persia_format::ByteSource> {
            self.inner.get_range(key, range)
        }

        fn get(&self, key: &persia_storage::Key) -> persia_storage::Result<bytes::Bytes> {
            self.inner.get(key)
        }

        fn put(&self, key: &persia_storage::Key, data: bytes::Bytes) -> persia_storage::Result<()> {
            self.inner.put(key, data)
        }

        fn put_if_absent(
            &self,
            key: &persia_storage::Key,
            data: bytes::Bytes,
        ) -> persia_storage::Result<persia_storage::PutOutcome> {
            self.inner.put_if_absent(key, data)
        }

        fn list(
            &self,
            prefix: &persia_storage::KeyPrefix,
        ) -> persia_storage::Result<Vec<persia_storage::ObjectMeta>> {
            let mut listed = self.inner.list(prefix)?;
            if let Some(meta) = listed.iter().find(|meta| {
                matches!(
                    RemoteKey::parse(&meta.key),
                    Some(RemoteKey::Chunk {
                        upload_id,
                        index,
                    }) if upload_id == self.upload && index == self.index
                )
            }) {
                let mut again = meta.clone();
                again.size = again.size.saturating_add(1);
                listed.push(again);
            }
            Ok(listed)
        }

        fn delete(&self, key: &persia_storage::Key) -> persia_storage::Result<()> {
            self.inner.delete(key)
        }

        fn capabilities(&self) -> persia_storage::Capabilities {
            self.inner.capabilities()
        }
    }

    #[test]
    fn a_duplicate_index_with_two_lengths_ends_the_run_there() {
        let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
        let inner = MemoryStorage::with_clock(clock);
        let t = token(1);
        let store = RemoteBlobStore::new(DifferingSize {
            inner,
            upload: t.upload_id,
            index: 1,
        })
        .with_parallelism(NonZeroUsize::MIN);
        for (index, bytes) in [(0, b"abcd".as_slice()), (1, b"efgh"), (2, b"ijkl")] {
            store.stage_chunk(t.upload_id, index, bytes).unwrap();
        }
        let resumed = BlobStore::resume(&store, t).unwrap();
        assert_eq!(resumed.next_index(), 1, "chunk 1's two lengths end the run");
        assert_eq!(resumed.listed(), [0, 1, 2]);
    }

    /// `get` of one chunk reports corruption; everything else is `inner`.
    struct CorruptGet<S> {
        inner: S,
        upload: UploadId,
        index: u32,
    }

    impl<S: Storage> Storage for CorruptGet<S> {
        fn get_range(
            &self,
            key: &persia_storage::Key,
            range: std::ops::Range<u64>,
        ) -> persia_storage::Result<persia_format::ByteSource> {
            self.inner.get_range(key, range)
        }

        fn get(&self, key: &persia_storage::Key) -> persia_storage::Result<bytes::Bytes> {
            if matches!(
                RemoteKey::parse(key),
                Some(RemoteKey::Chunk {
                    upload_id,
                    index,
                }) if upload_id == self.upload && index == self.index
            ) {
                return Err(persia_storage::Error::ShortRead {
                    key: key.clone(),
                    requested: 0..1,
                    returned: 0,
                });
            }
            self.inner.get(key)
        }

        fn put(&self, key: &persia_storage::Key, data: bytes::Bytes) -> persia_storage::Result<()> {
            self.inner.put(key, data)
        }

        fn put_if_absent(
            &self,
            key: &persia_storage::Key,
            data: bytes::Bytes,
        ) -> persia_storage::Result<persia_storage::PutOutcome> {
            self.inner.put_if_absent(key, data)
        }

        fn list(
            &self,
            prefix: &persia_storage::KeyPrefix,
        ) -> persia_storage::Result<Vec<persia_storage::ObjectMeta>> {
            self.inner.list(prefix)
        }

        fn delete(&self, key: &persia_storage::Key) -> persia_storage::Result<()> {
            self.inner.delete(key)
        }

        fn capabilities(&self) -> persia_storage::Capabilities {
            self.inner.capabilities()
        }
    }

    #[test]
    fn a_corrupt_chunk_ends_the_run_instead_of_failing_the_resume() {
        let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
        let inner = MemoryStorage::with_clock(clock);
        let t = token(1);
        let store = RemoteBlobStore::new(CorruptGet {
            inner,
            upload: t.upload_id,
            index: 1,
        })
        .with_parallelism(NonZeroUsize::MIN);
        for (index, bytes) in [(0, b"abcd".as_slice()), (1, b"efgh"), (2, b"ijkl")] {
            store.stage_chunk(t.upload_id, index, bytes).unwrap();
        }
        let resumed = BlobStore::resume(&store, t).unwrap();
        assert_eq!(resumed.next_index(), 1);
        assert_eq!(resumed.chunk_crcs().len(), 1);
    }

    #[test]
    fn an_empty_kept_run_skips_nothing() {
        let store = store();
        let resumed = BlobStore::resume(&store, token(2)).unwrap();
        assert_eq!(resumed.next_index(), 0);
        assert_eq!(resumed.listed(), [0_u32; 0]);
        resumed.skip_kept(&mut Failing).unwrap();
    }

    #[test]
    fn only_the_remote_layout_resumes() {
        let memory = MemoryBlobStore::new();
        assert!(!memory.resumable());
        assert!(matches!(
            memory.resume(token(1)),
            Err(Error::ResumeUnsupported)
        ));
        assert!(matches!(
            memory.restage(token(1), &[], UploadId::new(2)),
            Err(Error::ResumeUnsupported)
        ));

        let remote = store();
        assert!(remote.resumable());
        let t = token(1);
        staged(&remote, t);
        let resumed = BlobStore::resume(&remote, t).unwrap();
        BlobStore::restage(&remote, t, resumed.chunk_crcs(), UploadId::new(2)).unwrap();
        let copied: Vec<u32> = remote
            .staged_chunks(UploadId::new(2))
            .unwrap()
            .iter()
            .map(|c| c.index)
            .collect();
        assert_eq!(copied, [0, 1]);
    }
}

// Edges of `skip_kept` (SPEC §9.4: the resuming client passes the whole blob, and its kept bytes are checked against
// the staged chunks' CRC32Cs, not uploaded again). Small chunks over a manual clock: Miri runs them.
#[cfg(test)]
mod skip_kept_edges {
    use core::num::NonZeroUsize;
    use std::io;
    use std::sync::Arc;
    use std::time::UNIX_EPOCH;

    use persia_storage::MemoryStorage;
    use persia_storage::retry::ManualClock;

    use super::*;

    const BLOB: &[u8] = b"abcdefghijkl";

    /// `abcd efgh ijkl` fully staged at chunk size 4, plus `kept` chunks of it resumed: a run of `kept` chunks.
    fn kept_run(kept: usize) -> ResumedUpload {
        let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
        let store = RemoteBlobStore::new(MemoryStorage::with_clock(clock))
            .with_parallelism(NonZeroUsize::MIN);
        let token = ResumeToken {
            upload_id: UploadId::new(9),
            chunk_size: ChunkSize::from_encoding(4).unwrap(),
        };
        for (index, chunk) in (0..).zip(BLOB.chunks(4).take(kept)) {
            store.stage_chunk(token.upload_id, index, chunk).unwrap();
        }
        let resumed = store.resume(token).unwrap();
        assert_eq!(resumed.next_index() as usize, kept);
        resumed
    }

    /// Yields its bytes one per `read` call, and `Interrupted` before each (which `read_to_end` retries).
    struct Trickle<'a> {
        data: &'a [u8],
        interrupt: bool,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let Some((&first, rest)) = self.data.split_first() else {
                return Ok(0);
            };
            let Some(slot) = out.first_mut() else {
                return Ok(0);
            };
            *slot = first;
            self.data = rest;
            Ok(1)
        }
    }

    fn ends_inside(r: &Result<()>) -> bool {
        matches!(r, Err(Error::InvalidResume { what }) if what.contains("ends inside"))
    }

    fn not_staged(r: &Result<()>) -> bool {
        matches!(r, Err(Error::InvalidResume { what }) if what.contains("not the ones"))
    }

    #[test]
    fn a_reader_that_ends_exactly_at_the_run_end_is_accepted() {
        let resumed = kept_run(2);
        let mut reader = &BLOB[..8];
        resumed.skip_kept(&mut reader).unwrap();
        assert_eq!(reader, b"");
        // Then the chunker sees the end of the blob: an 8-byte blob of the two kept chunks.
        let (mut chunker, crcs) = resumed.into_chunker(reader).unwrap();
        assert!(chunker.next_chunk().unwrap().is_none());
        assert_eq!(crcs.len(), 2);
        assert_eq!(
            chunker.summary().unwrap().blob_id,
            crate::blob_id_of(&BLOB[..8])
        );
    }

    #[test]
    fn a_reader_one_byte_short_of_the_run_end_ends_inside_it() {
        let resumed = kept_run(2);
        assert!(ends_inside(&resumed.skip_kept(&mut &BLOB[..7])));
        // Short inside the first chunk too, and inside a run of the whole blob.
        assert!(ends_inside(&resumed.skip_kept(&mut &BLOB[..3])));
        assert!(ends_inside(&kept_run(3).skip_kept(&mut &BLOB[..11])));
    }

    #[test]
    fn a_reader_that_differs_in_the_last_or_first_byte_of_the_run_is_not_the_staged_blob() {
        let resumed = kept_run(2);
        for at in [0, 3, 4, 7] {
            let mut other = BLOB.to_vec();
            other[at] ^= 0x01;
            assert!(not_staged(&resumed.skip_kept(&mut &other[..])), "at {at}");
        }
        // A byte past the run is not the run's: the resume checks only what it kept.
        let mut after = BLOB.to_vec();
        after[8] ^= 0x01;
        resumed.skip_kept(&mut &after[..]).unwrap();
    }

    #[test]
    fn an_empty_kept_run_reads_nothing_from_the_reader() {
        let resumed = kept_run(0);
        assert_eq!(resumed.offset(), 0);
        let mut reader = BLOB;
        resumed.skip_kept(&mut reader).unwrap();
        assert_eq!(reader, BLOB);
        resumed.skip_kept(&mut &b""[..]).unwrap();
    }

    #[test]
    fn a_reader_of_one_byte_per_read_is_left_exactly_at_the_offset() {
        for kept in 0..=3 {
            let resumed = kept_run(kept);
            let mut reader = Trickle {
                data: BLOB,
                interrupt: false,
            };
            resumed.skip_kept(&mut reader).unwrap();
            assert_eq!(reader.data, &BLOB[kept * 4..], "kept {kept}");
            assert_eq!(resumed.offset(), (kept * 4) as u64);
        }
        // One byte short, a byte at a time, still ends inside the run.
        let mut reader = Trickle {
            data: &BLOB[..7],
            interrupt: false,
        };
        assert!(ends_inside(&kept_run(2).skip_kept(&mut reader)));
    }

    #[test]
    fn a_reader_that_fails_inside_the_run_is_a_read_error() {
        // Fails after the first chunk, then would yield the rest: the error is reported, not the short read.
        struct FailOnce<'a> {
            data: &'a [u8],
            failed: bool,
        }
        impl Read for FailOnce<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.data.len() == 8 && !self.failed {
                    self.failed = true;
                    return Err(io::Error::other("the reader failed"));
                }
                self.data.read(out)
            }
        }
        let mut reader = FailOnce {
            data: BLOB,
            failed: false,
        };
        // The first read takes 4 bytes (the `take` limit), leaving 8: the second fails.
        assert!(matches!(
            kept_run(2).skip_kept(&mut reader),
            Err(Error::Read(e)) if e.to_string() == "the reader failed"
        ));
    }
}
