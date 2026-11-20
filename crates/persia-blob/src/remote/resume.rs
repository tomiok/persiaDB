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

use super::{RemoteBlobStore, RemoteKey, parallel};
use crate::chunker::Chunker;
use crate::chunking::ChunkSize;
use crate::error::{Error, Result};
use crate::hash::ContentHasher;
use crate::manifest::UploadId;
use crate::store::BlobStore;

/// What a client keeps to resume an upload (SPEC §9.4): chunk objects do not record the chunk size.
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
    /// Every chunk index the listing showed, kept run or not, in order.
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

    /// Every chunk index the listing [`RemoteBlobStore::resume`] read showed, in the kept run or not, in order.
    /// Staging such a chunk again with the same bytes writes nothing (SPEC §9.4: `put_if_absent`), so it leaves the
    /// upload as old as it was; one the listing did not show is written then (ROADMAP 9.3.6). Without `strong_list`
    /// a listing may miss a chunk written just before it, which is young anyway.
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
    /// Finds what `token`'s upload has stored: the run of chunks `0, 1, …` whose listed length is the chunk size,
    /// each re-read (up to `parallelism` at a time), checked to be full again, hashed in order and its CRC32C kept. A
    /// chunk missing or of another length ends the run; it and later ones are written again (same bytes succeed,
    /// other bytes conflict). Without `strong_list` a just-written chunk may be missed, which only shortens the run.
    ///
    /// The caller pins `token.upload_id` before calling (SPEC §9.5).
    ///
    /// # Errors
    /// The backend's errors.
    pub fn resume(&self, token: ResumeToken) -> Result<ResumedUpload> {
        let full = u64::from(token.chunk_size.get());
        let listed = self.staged_chunks(token.upload_id)?;
        let run = listed
            .iter()
            .zip(0..u32::MAX)
            .take_while(|(chunk, index)| chunk.index == *index && chunk.size == full)
            .count();
        let mut resumed = ResumedUpload {
            token,
            hasher: ContentHasher::new(),
            crcs: Vec::new(),
            listed: listed.iter().map(|chunk| chunk.index).collect(),
        };
        let window = self.parallelism.get();
        let mut start = 0_usize;
        while start < run {
            let len = window.min(run.saturating_sub(start));
            let chunks = parallel::run(len, self.parallelism, |i| {
                let index = chunk_index(start, i)?;
                self.get(
                    &RemoteKey::Chunk {
                        upload_id: token.upload_id,
                        index,
                    }
                    .key()?,
                )
            })?;
            for bytes in chunks {
                match bytes {
                    Some(bytes) if u64_from_usize(bytes.len()) == full => {
                        resumed.hasher.update(&bytes);
                        resumed.crcs.push(crc32c(&bytes));
                    }
                    // Gone or not full since the listing: the run ends here.
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

    use persia_storage::MemoryStorage;
    use persia_storage::retry::ManualClock;

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
