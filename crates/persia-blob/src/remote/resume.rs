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
    /// [`Error::Corrupt`] when a chunk vanished or changed; [`Error::StagedChunkConflict`] when `to` already holds
    /// other bytes; the backend's errors.
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
                    Some(bytes)
                        if Some(crc32c(&bytes)) == expected
                            && u64_from_usize(bytes.len()) == full =>
                    {
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
