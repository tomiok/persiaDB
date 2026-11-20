//! The sweep of the remote layout (SPEC §9.4, §9.5; ROADMAP 9.5.2, ADR-0052).
//!
//! First the blob manifests: `blobs/<xx>/` is listed for each of the 256 prefixes, and an unmarked manifest older
//! than the grace period is deleted unless pinned at its check. A manifest kept for any reason is read, so the
//! upload it names keeps its chunks. Then the staged chunks: `blobs/uploads/` is listed once, chunks are grouped by
//! upload and aged from the upload's newest chunk, and an upload no kept manifest names, old enough, has its chunks
//! deleted unless it is pinned at its check. Manifests go first, so a crash mid-sweep leaves chunks without a
//! manifest, never a manifest without chunks. An upload's chunks are deleted in one step under the lock, not one
//! chunk per step: a pin taken midway (a blob bound late naming the upload, ROADMAP 9.5.8) must find the upload whole,
//! at the cost of holding the lock for one request per chunk. Every key is parsed (`RemoteKey::parse`); one that
//! does not parse was not written by Persia and is ignored, never deleted.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::SystemTime;

use persia_format::BlobId;
use persia_storage::{Key, Storage};

use super::{GcObject, Mark, PinCheck, PinGate, Sweep, SweepOptions, SweepReport, is_unreadable};
use crate::error::{Error, Result};
use crate::manifest::UploadId;
use crate::remote::RemoteBlobStore;
use crate::remote::keys::{self, RemoteKey};
use crate::store::BlobStore;

/// How old an object is, against the grace period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Age {
    /// The backend cannot tell: never taken for expired (SPEC §10.1).
    Unknown,
    /// Younger than the grace period, or written after `now` (another clock).
    Young,
    /// Older than the grace period.
    Expired,
}

fn age(written: Option<SystemTime>, options: &SweepOptions) -> Age {
    match written.map(|t| options.now.duration_since(t)) {
        None => Age::Unknown,
        Some(Ok(age)) if age > options.grace => Age::Expired,
        Some(_) => Age::Young,
    }
}

/// One upload's staged chunks, as listed.
#[derive(Debug, Default)]
struct Upload {
    /// The newest chunk's time, of those whose time is known.
    newest: Option<SystemTime>,
    /// Whether a chunk's time is unknown: then so is the upload's age.
    unknown: bool,
    indices: Vec<u32>,
}

impl<S: Storage> Sweep for RemoteBlobStore<S> {
    fn sweep<P: PinGate>(
        &self,
        mark: &Mark,
        pins: &P,
        options: &SweepOptions,
    ) -> Result<SweepReport> {
        let mut report = SweepReport::default();
        let mut keep = mark.uploads.clone();
        // The upload each kept, unmarked manifest names (`None`: inline), for the check of pinned blobs below.
        let mut named: HashMap<BlobId, Option<UploadId>> = HashMap::new();
        // The listing skips marked blobs, so a manifest the mark already failed to read is not counted again below.
        report.unreadable = u64::try_from(mark.unreadable.len()).unwrap_or(u64::MAX);
        let mut unreadable = report.unreadable > 0;
        for first in 0..=u8::MAX {
            let listed = self
                .storage()
                .list(&keys::manifest_prefix(first)?)
                .map_err(storage)?;
            for meta in listed {
                let Some(RemoteKey::Manifest(id)) = RemoteKey::parse(&meta.key) else {
                    continue;
                };
                // Its upload is in the mark already (`Mark::add_manifest_uploads`).
                if mark.blobs.contains(&id) {
                    continue;
                }
                if self.sweep_manifest(
                    id,
                    &meta.key,
                    meta.last_modified,
                    pins,
                    options,
                    &mut report,
                )? {
                    continue;
                }
                // Kept: the chunks of the upload it names stay too.
                match self.manifest(id) {
                    Ok(Some(m)) => {
                        let upload = (!m.is_inline()).then(|| m.upload_id());
                        keep.extend(upload);
                        named.insert(id, upload);
                    }
                    Ok(None) => {}
                    Err(e) if is_unreadable(&e) => {
                        report.unreadable = report.unreadable.saturating_add(1);
                        unreadable = true;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        if unreadable {
            report.uploads_skipped = true;
            return Ok(report);
        }
        for (upload_id, upload) in self.unkept_uploads(&keep)? {
            let newest = if upload.unknown { None } else { upload.newest };
            match age(newest, options) {
                Age::Unknown => report.unknown_age = report.unknown_age.saturating_add(1),
                Age::Young => report.young = report.young.saturating_add(1),
                Age::Expired => {
                    let swept = pins.unless_pinned(GcObject::Upload(upload_id), |view| {
                        // ADR-0051 item 5: a blob is pinned before the manifest naming this upload is read, and the
                        // upload right after, so between the two only the blob's pin protects these chunks.
                        if self.named_by_pinned_blob(upload_id, view, mark, &mut named)? {
                            return Ok(None);
                        }
                        self.delete_chunks(upload_id, &upload.indices).map(Some)
                    });
                    match swept.transpose()?.flatten() {
                        None => report.pinned = report.pinned.saturating_add(1),
                        Some(deleted) => {
                            report.chunks_deleted = report.chunks_deleted.saturating_add(deleted);
                            report.uploads_deleted = report.uploads_deleted.saturating_add(1);
                        }
                    }
                }
            }
        }
        Ok(report)
    }
}

impl<S: Storage> RemoteBlobStore<S> {
    /// Whether a blob pinned now has a manifest that names `upload_id` (SPEC §9.5, ADR-0051 item 5), read through the
    /// locked pins `view`. A marked blob whose manifest the mark resolved is skipped: its upload is kept already. One
    /// the mark did not resolve (the manifest was missing then) is read now, as is any other pinned blob the listing
    /// did not name (`named`). A manifest that cannot be read counts as naming the upload: GC deletes less, never more.
    fn named_by_pinned_blob(
        &self,
        upload_id: UploadId,
        view: &dyn PinCheck,
        mark: &Mark,
        named: &mut HashMap<BlobId, Option<UploadId>>,
    ) -> Result<bool> {
        for blob in view.pinned_blobs() {
            if mark.manifest_resolved(blob) {
                continue;
            }
            let upload = if let Some(upload) = named.get(&blob) {
                *upload
            } else {
                let upload = match self.manifest(blob) {
                    Ok(Some(m)) => (!m.is_inline()).then(|| m.upload_id()),
                    Ok(None) => None,
                    Err(e) if is_unreadable(&e) => Some(upload_id),
                    Err(e) => return Err(e),
                };
                named.insert(blob, upload);
                upload
            };
            if upload == Some(upload_id) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Deletes the unmarked manifest `id` at `key` if it is old enough and unpinned at its check; returns whether it
    /// did.
    fn sweep_manifest<P: PinGate>(
        &self,
        id: BlobId,
        key: &Key,
        written: Option<SystemTime>,
        pins: &P,
        options: &SweepOptions,
        report: &mut SweepReport,
    ) -> Result<bool> {
        match age(written, options) {
            Age::Unknown => report.unknown_age = report.unknown_age.saturating_add(1),
            Age::Young => report.young = report.young.saturating_add(1),
            Age::Expired => {
                match pins.unless_pinned(GcObject::Blob(id), |_| self.storage().delete(key)) {
                    None => report.pinned = report.pinned.saturating_add(1),
                    Some(deleted) => {
                        deleted.map_err(storage)?;
                        report.blobs_deleted = report.blobs_deleted.saturating_add(1);
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// The staged chunks of every upload not in `keep`, grouped by upload, from one listing of `blobs/uploads/`.
    fn unkept_uploads(&self, keep: &HashSet<UploadId>) -> Result<BTreeMap<UploadId, Upload>> {
        let listed = self
            .storage()
            .list(&keys::uploads_prefix()?)
            .map_err(storage)?;
        let mut uploads: BTreeMap<UploadId, Upload> = BTreeMap::new();
        for meta in listed {
            let Some(RemoteKey::Chunk { upload_id, index }) = RemoteKey::parse(&meta.key) else {
                continue;
            };
            if keep.contains(&upload_id) {
                continue;
            }
            let upload = uploads.entry(upload_id).or_default();
            upload.indices.push(index);
            match meta.last_modified {
                // SPEC §9.5: an upload is aged from its newest chunk, so an idle upload stays resumable that long.
                Some(t) => upload.newest = upload.newest.max(Some(t)),
                None => upload.unknown = true,
            }
        }
        Ok(uploads)
    }

    /// Deletes chunks `indices` of `upload_id`; returns how many. Stops at the first error.
    fn delete_chunks(&self, upload_id: UploadId, indices: &[u32]) -> Result<u64> {
        let mut deleted = 0_u64;
        for &index in indices {
            let key = RemoteKey::Chunk { upload_id, index }.key()?;
            self.storage().delete(&key).map_err(storage)?;
            deleted = deleted.saturating_add(1);
        }
        Ok(deleted)
    }
}

/// A backend error as this crate reports it: corruption the backend detected stays corruption (as the remote store
/// maps it, ADR-0050 item 2).
fn storage(e: persia_storage::Error) -> Error {
    if e.is_corruption() {
        Error::Corrupt {
            encoding: "blob storage object",
            what: "the storage backend reported a corrupt object",
        }
    } else {
        Error::Storage(e)
    }
}
