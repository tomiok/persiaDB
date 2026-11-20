//! The resumed upload of [`Blobs::put_blob`] (SPEC §9.4, §9.5; ROADMAP 9.3.4, 9.3.6; ADR-0053).
//!
//! A caller resumes with the [`ResumeToken`] of an [`Error::UploadInterrupted`] and a reader of the whole blob. The
//! token's upload is pinned, then listed and its run of full chunks from chunk 0 re-read and hashed
//! ([`BlobStore::resume`]); the reader's bytes of that run are read and checked against the staged chunks' CRC32Cs, not
//! uploaded; the rest is cut by the resumed chunker and staged under the same upload, a window at a time.
//!
//! The upload moves to a new upload id, pinned before its chunks are copied there ([`BlobStore::restage`]), in two
//! cases:
//! - a chunk of the token's upload holds other bytes ([`Error::StagedChunkConflict`], SPEC §9.4): the chunks before the
//!   window that conflicted are copied, and the window is staged under the new id;
//! - the resume would write no chunk the listing did not show (ROADMAP 9.3.6): every chunk it stages is there
//!   already, so the upload stays as old as its newest chunk, maybe older than GC's grace period, and a sweep that
//!   listed the manifests before this bind would not see the binding and could delete the chunks once its pins drop.
//!   The chunks before the last window are copied before the last window is staged, so the bound upload is young.

use std::io::Read;

use persia_blob::{BlobRef, BlobStore, Chunk, Error, Result, ResumeToken, UploadId};

use super::{Blobs, PutBlobOptions, bind_interrupted, interrupted, window_chunks};
use crate::internal::pins::{Pinned, Pins};

/// Where a resumed upload stands.
#[derive(Debug, Clone, Copy)]
struct Upload {
    /// The token the caller passed.
    from: ResumeToken,
    /// The upload being staged and bound: `from`'s until a restage, the new one after.
    current: ResumeToken,
    /// Whether this call wrote a chunk of `current` the resume's listing did not show (a restage's chunks are all
    /// such): the upload's newest chunk is then younger than this call (SPEC §9.5: an upload is aged from it).
    fresh: bool,
}

impl<S: BlobStore + ?Sized> Blobs<S> {
    /// `put_blob` with `options.resume` (see the module docs); the options are checked already.
    pub(super) fn put_resumed(
        &self,
        mut reader: impl Read,
        token: ResumeToken,
        options: &PutBlobOptions,
    ) -> Result<(BlobRef, Pins)> {
        // SPEC §9.5: pinned before its chunks are listed, so no sweep takes them between the listing and the bind.
        let mut pins = self.pins.pin([Pinned::Upload(token.upload_id)]);
        // A store that cannot resume fails here, before any byte is read (`ResumeUnsupported`, never wrapped).
        let resumed = self
            .store
            .resume(token)
            .map_err(|e| interrupted(Some(token), e))?;
        resumed
            .skip_kept(&mut reader)
            .map_err(|e| interrupted(Some(token), e))?;
        let listed = resumed.listed().to_vec();
        let was_listed = |index: u32| listed.binary_search(&index).is_ok();
        let (mut chunker, mut crcs) = resumed.into_chunker(reader)?;
        let mut upload = Upload {
            from: token,
            current: token,
            fresh: false,
        };
        let window = window_chunks(token.chunk_size);
        let mut staged: Vec<Chunk> = Vec::new();
        loop {
            let next = chunker.next_chunk();
            let Some(chunk) = next.map_err(|e| interrupted(Some(upload.current), e))? else {
                break;
            };
            crcs.push(chunk.crc32c());
            staged.push(chunk);
            if staged.len() >= window {
                self.stage_resumed(&mut upload, &crcs, &mut staged, &was_listed, &mut pins)?;
            }
        }
        let blob = chunker.summary().ok_or(Error::ChunkerFailed)?;
        // ROADMAP 9.3.6: nothing this call stages would be new, so the chunks move to a new upload before the bind.
        if !upload.fresh && staged.iter().all(|chunk| was_listed(chunk.index())) {
            let before = staged
                .first()
                .map_or(Ok(crcs.len()), |c| index(c.index()))?;
            let copied = crcs.get(..before).ok_or(Error::InvalidResume {
                what: "a window starts past the blob's chunks",
            })?;
            self.restage_resumed(&mut upload, copied, false, &mut pins)?;
        }
        self.stage_resumed(&mut upload, &crcs, &mut staged, &was_listed, &mut pins)?;
        let manifest =
            self.checked_manifest(blob, upload.current.upload_id, crcs, options, true)?;
        pins.add(Pinned::Blob(manifest.blob_id()));
        let outcome = self.bind(upload.current.upload_id, options, || {
            self.store.bind(&manifest)
        });
        let outcome = bind_interrupted(Some(upload.current), outcome);
        let blob_ref = self.bound(&manifest, outcome, options, &mut pins)?;
        Ok((blob_ref, pins))
    }

    /// Stages the window `staged` (chunks after the kept run, `crcs` holding every chunk's CRC32C up to its end) under
    /// the upload being staged, then empties it. A conflict on the token's upload restages there first (SPEC §9.4).
    fn stage_resumed(
        &self,
        upload: &mut Upload,
        crcs: &[u32],
        staged: &mut Vec<Chunk>,
        was_listed: &dyn Fn(u32) -> bool,
        pins: &mut Pins,
    ) -> Result<()> {
        let Some(first) = staged.first().map(Chunk::index) else {
            return Ok(());
        };
        let unlisted = staged.iter().any(|chunk| !was_listed(chunk.index()));
        match self.stage(upload.current.upload_id, staged) {
            Ok(()) => {}
            // SPEC §9.4: the token's upload holds other bytes at an index of this window, so the client restarts
            // under a new upload id without rewinding: what was staged before the window is copied there.
            Err(Error::StagedChunkConflict { upload_id, .. })
                if upload_id == upload.from.upload_id && upload.current == upload.from =>
            {
                let copied = crcs.get(..index(first)?).ok_or(Error::InvalidResume {
                    what: "a window starts past the blob's chunks",
                })?;
                self.restage_resumed(upload, copied, true, pins)?;
                self.stage(upload.current.upload_id, staged)
                    .map_err(|e| interrupted(Some(upload.current), e))?;
            }
            Err(e) => return Err(interrupted(Some(upload.current), e)),
        }
        if unlisted {
            upload.fresh = true;
        }
        Ok(())
    }

    /// Moves the upload to a new upload id, pinned first (SPEC §9.5), copying chunks `0..copied.len()` there.
    ///
    /// A failed copy is resumable: after a `conflict` with the new upload (the token's holds other bytes), otherwise
    /// with the upload it copied from, which is whole.
    fn restage_resumed(
        &self,
        upload: &mut Upload,
        copied: &[u32],
        conflict: bool,
        pins: &mut Pins,
    ) -> Result<()> {
        let to = ResumeToken {
            upload_id: UploadId::new(self.next_upload_id()),
            chunk_size: upload.current.chunk_size,
        };
        pins.add(Pinned::Upload(to.upload_id));
        let resume_with = if conflict { to } else { upload.current };
        self.store
            .restage(upload.current, copied, to.upload_id)
            .map_err(|e| interrupted(Some(resume_with), e))?;
        upload.current = to;
        upload.fresh = true;
        Ok(())
    }
}

/// A chunk index as a position in the list of CRC32Cs.
fn index(index: u32) -> Result<usize> {
    usize::try_from(index).map_err(|_| Error::InvalidResume {
        what: "a chunk index does not fit in this platform's usize",
    })
}
