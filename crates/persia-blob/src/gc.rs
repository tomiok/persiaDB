//! Garbage collection of the blob store (SPEC §9.5; ROADMAP 9.5.1, 9.5.2, 9.5.8, ADR-0052): the mark, and the sweep
//! of each store.
//!
//! GC runs only in the writer, which also runs `put_blob` and keeps the **pin set** (SPEC §9.5, ADR-0051). This crate
//! does not know the engine nor the pin set (both live above it, in `persia`), so the writer hands it:
//! - a [`Mark`]: the blob ids its documents reference (the engine's view, SPEC §9.5) and every id pinned when the
//!   mark was taken; [`Mark::add_manifest_uploads`] then adds the upload ids the marked blobs' manifests name;
//! - a [`PinGate`] over its pin set: every pin check and delete of the sweep is one step under the set's lock, so a
//!   pin taken after the mark still protects what it names.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use persia_format::BlobId;

use crate::error::{Error, Result};
use crate::manifest::UploadId;
use crate::store::BlobStore;

mod pack;
mod remote;

/// How a sweep runs (SPEC §9.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweepOptions {
    /// How long an unmarked remote object is kept after it was written (SPEC §9.5: 24 h by default, at least twice the
    /// longest bind budget; staged chunks counted from their upload's newest chunk). A conditional write started
    /// inside the budget can land after it (SPEC §9.4). The pack has none.
    pub grace: Duration,
    /// The time ages are measured at, by the backend's clock: objects' `last_modified` come from it (SPEC §10.1).
    pub now: SystemTime,
    /// The pack is rewritten only when the bytes it would drop reach this share of its frames' bytes (0 forces the
    /// rewrite; above 1 it never runs). The remote layout deletes objects one by one and ignores it.
    pub rewrite_ratio: f64,
}

/// What a sweep did (SPEC §9.5). Counts are of objects: remotely, blob manifests and staged chunk objects; in the
/// pack, blobs and chunk frames the rewrite dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Unmarked blobs deleted: their manifests (remote) or their index and chunk frames (pack).
    pub blobs_deleted: u64,
    /// Uploads whose staged chunks were deleted: unbound uploads, and (remote) those of blobs deleted by this or an
    /// earlier sweep.
    pub uploads_deleted: u64,
    /// Staged chunk objects (remote) or chunk frames (pack) deleted.
    pub chunks_deleted: u64,
    /// Unmarked remote objects kept because a pin named them at their check (or, for chunks, a pinned blob's
    /// manifest names their upload, ADR-0051 item 5).
    pub pinned: u64,
    /// Unmarked remote objects kept because they are younger than the grace period (an upload counts once).
    pub young: u64,
    /// Unmarked remote objects kept because the backend cannot tell their age (`last_modified` is `None`, SPEC
    /// §10.1): never taken for expired (an upload counts once).
    pub unknown_age: u64,
    /// Expired remote uploads kept because their listing under the pin set's lock differs from the sweep's listing
    /// of `blobs/uploads/`: a chunk added, gone, or of another size or age (ROADMAP 9.5.10). The next sweep decides
    /// again.
    pub changed: u64,
    /// Remote manifests that could not be read, the mark's and those a kept manifest's read failed on (never both for
    /// the same blob: the listing skips blobs the mark already resolved): the upload each names is unknown.
    pub unreadable: u64,
    /// Whether the staged chunks were left alone because a marked or kept manifest could not be read (the upload it
    /// names is unknown): GC deletes less, never more.
    pub uploads_skipped: bool,
    /// What became of the pack rewrite; `None` for the remote layout.
    pub rewrite: Option<Rewrite>,
}

/// What became of a pack rewrite (SPEC §9.3, §18.10; ADR-0052).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rewrite {
    /// The database has no pack: nothing to collect.
    NoPack,
    /// The bytes to drop are below [`SweepOptions::rewrite_ratio`] of the frames' bytes: nothing changed.
    BelowRatio {
        /// Bytes of frames nothing keeps.
        dead_bytes: u64,
        /// Bytes of all frames (`12288..` the end of the file).
        frame_bytes: u64,
    },
    /// A pin taken after the plan names something the rewrite drops: the new file was discarded and the pack is
    /// unchanged. The next sweep tries again.
    Deferred,
    /// The pack was replaced by a new file.
    Done {
        /// Its generation (the old one plus one, SPEC §18.10).
        generation: u64,
        /// Bytes of frames before.
        frame_bytes_before: u64,
        /// Bytes of frames after.
        frame_bytes_after: u64,
    },
}

/// The sweep of a blob store (SPEC §9.5): deletes what a [`Mark`] does not keep, each pin check and delete one step
/// under the pin set's lock.
pub trait Sweep {
    /// Deletes every unmarked blob manifest, then the staged chunks of every upload no kept manifest names (SPEC
    /// §9.5), skipping whatever a pin names at its check. Remotely, an object is deleted only once older than
    /// [`SweepOptions::grace`]; an object of unknown age never is. The pack drops them at once, by rewriting itself
    /// when enough is dropped ([`SweepOptions::rewrite_ratio`]).
    ///
    /// # Errors
    /// The store's errors: the sweep stops at the first, and what it deleted before was unmarked.
    fn sweep<P: PinGate>(
        &self,
        mark: &Mark,
        pins: &P,
        options: &SweepOptions,
    ) -> Result<SweepReport>;
}

/// What a pin protects from GC (SPEC §9.5): the writer's `Pinned`, as this crate names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GcObject {
    /// A bound blob's manifest (and, through it, the chunks of the upload it names, ADR-0051 item 5).
    Blob(BlobId),
    /// The chunks staged under an upload.
    Upload(UploadId),
}

/// The pins as a [`PinGate`] holds them locked: what a sweep's delete reads instead of the gate, which it cannot
/// lock again.
pub trait PinCheck {
    /// Whether `object` is pinned.
    fn is_pinned(&self, object: GcObject) -> bool;

    /// Every blob id pinned.
    fn pinned_blobs(&self) -> Vec<BlobId>;
}

/// The writer's pin set, as GC uses it (SPEC §9.5, ADR-0051 item 5). `persia` implements it for its `PinSet`.
pub trait PinGate {
    /// Every object pinned now: what the mark adds to the referenced ids. A pin taken after this returns is still
    /// honored by [`PinGate::unless_pinned`].
    fn pinned(&self) -> Vec<GcObject>;

    /// Runs `delete` unless `object` is pinned, holding the pin set's lock throughout, and returns its result; `None`
    /// when `object` is pinned and nothing ran. No pin can be taken or released while `delete` runs; it reads the pins
    /// through the [`PinCheck`] it is given (it must not lock the set again).
    fn unless_pinned<T>(
        &self,
        object: GcObject,
        delete: impl FnOnce(&dyn PinCheck) -> T,
    ) -> Option<T>;
}

/// What GC keeps (SPEC §9.5): the blob ids the writer's documents reference or it pinned, and the upload ids pinned
/// or named by a marked blob's manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mark {
    /// Referenced or pinned blob ids: their manifests are kept.
    pub blobs: HashSet<BlobId>,
    /// Pinned upload ids, and those the marked blobs' manifests name: their staged chunks are kept.
    pub uploads: HashSet<UploadId>,
    /// Referenced blobs the store has no manifest for: dangling references, which `verify` reports (ROADMAP 9.5.3). GC
    /// keeps nothing for them. A blob only pinned is not listed: a `put_blob` pins its id before it binds it.
    pub missing: Vec<BlobId>,
    /// Marked blobs whose manifest could not be read (corrupt, or from a newer build): the upload they name is
    /// unknown, so a sweep deletes no staged chunk while any is listed (it deletes less, never more).
    pub unreadable: Vec<BlobId>,
    /// Marked blobs whose manifest was read: an upload was added, or the manifest is inline and names none. A marked
    /// blob that was missing or unreadable is not here, so a pin can still make the sweep read it.
    resolved: HashSet<BlobId>,
    /// Pinned blob ids no document references: not dangling when unbound.
    pinned_only: HashSet<BlobId>,
}

impl Mark {
    /// The mark of `referenced` blob ids (the engine's view, SPEC §9.5) and the objects `pinned` (taken before or
    /// after them: the log cannot change while GC runs, and later pins are checked again by the sweep).
    pub fn new(
        referenced: impl IntoIterator<Item = BlobId>,
        pinned: impl IntoIterator<Item = GcObject>,
    ) -> Self {
        let mut mark = Self {
            blobs: referenced.into_iter().collect(),
            ..Self::default()
        };
        for object in pinned {
            match object {
                GcObject::Blob(id) if mark.blobs.insert(id) => mark.pinned_only.insert(id),
                GcObject::Blob(_) => false,
                GcObject::Upload(id) => mark.uploads.insert(id),
            };
        }
        mark
    }

    /// Whether [`Mark::add_manifest_uploads`] read `id`'s manifest. A marked blob it did not resolve (missing, or
    /// unreadable) stays out, so the sweep reads it again when a pin holds it.
    pub(crate) fn manifest_resolved(&self, id: BlobId) -> bool {
        self.resolved.contains(&id)
    }

    /// Adds the upload id each marked blob's manifest in `store` names (SPEC §9.5: the mark's last step), so a sweep
    /// keeps the chunks behind every kept manifest. An inline manifest (SPEC §9.1.1) names none: its upload id is 0
    /// and nothing is staged for it. A referenced blob with no manifest is listed in [`Mark::missing`], a marked one whose
    /// manifest cannot be read in [`Mark::unreadable`].
    ///
    /// ```
    /// use persia_blob::gc::Mark;
    /// use persia_blob::remote::RemoteBlobStore;
    /// use persia_blob::{BlobManifest, BlobStore, ChunkSize, UploadId, blob_id_of};
    /// use persia_format::checksum::crc32c;
    /// use persia_storage::MemoryStorage;
    /// use persia_storage::retry::ManualClock;
    ///
    /// let store = RemoteBlobStore::new(MemoryStorage::with_clock(std::sync::Arc::new(ManualClock::new(
    ///     std::time::UNIX_EPOCH,
    /// ))));
    /// let data = vec![7; 300];
    /// let upload = UploadId::new(5);
    /// store.stage_chunk(upload, 0, &data)?;
    /// let m = BlobManifest::new(blob_id_of(&data), 300, ChunkSize::DEFAULT, upload, vec![crc32c(&data)])?;
    /// store.bind(&m)?;
    /// let gone = blob_id_of(b"never stored");
    /// let mut mark = Mark::new([m.blob_id(), gone], []);
    /// mark.add_manifest_uploads(&store)?;
    /// assert!(mark.uploads.contains(&upload));
    /// assert_eq!(mark.missing, [gone]);
    /// # Ok::<(), persia_blob::Error>(())
    /// ```
    ///
    /// # Errors
    /// The store's errors other than unreadable bytes (a storage failure): the sweep must not run on a partial mark.
    pub fn add_manifest_uploads<S: BlobStore + ?Sized>(&mut self, store: &S) -> Result<()> {
        let mut blobs: Vec<BlobId> = self.blobs.iter().copied().collect();
        // A deterministic order: the same calls for the same mark, so tests and logs repeat.
        blobs.sort_unstable();
        for id in blobs {
            match store.manifest(id) {
                Ok(Some(m)) if m.is_inline() => {
                    self.resolved.insert(id);
                }
                Ok(Some(m)) => {
                    self.uploads.insert(m.upload_id());
                    self.resolved.insert(id);
                }
                Ok(None) if self.pinned_only.contains(&id) => {}
                Ok(None) => self.missing.push(id),
                Err(e) if is_unreadable(&e) => self.unreadable.push(id),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Whether `e` says stored bytes cannot be read by this build (corrupt, or a newer encoding), rather than that the
/// store failed.
pub(crate) const fn is_unreadable(e: &Error) -> bool {
    matches!(
        e,
        Error::Corrupt { .. } | Error::UnsupportedVersion { .. } | Error::UnsupportedFlags { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::UNIX_EPOCH;

    use bytes::Bytes;
    use persia_format::checksum::crc32c;
    use persia_storage::MemoryStorage;
    use persia_storage::faulty::{Fault, FaultyStorage, Trigger};
    use persia_storage::retry::ManualClock;

    use super::*;
    use crate::manifest::BlobManifest;
    use crate::remote::{RemoteBlobStore, RemoteKey};
    use crate::{BindOutcome, ChunkSize, blob_id_of};

    fn remote() -> RemoteBlobStore<MemoryStorage> {
        RemoteBlobStore::new(MemoryStorage::with_clock(Arc::new(ManualClock::new(
            UNIX_EPOCH,
        ))))
    }

    /// Stages `data` as one chunk under `upload` and binds its plain manifest.
    fn staged(store: &RemoteBlobStore<MemoryStorage>, data: &[u8], upload: u128) -> BlobManifest {
        let upload = UploadId::new(upload);
        store.stage_chunk(upload, 0, data).unwrap();
        let m = BlobManifest::new(
            blob_id_of(data),
            data.len() as u64,
            ChunkSize::DEFAULT,
            upload,
            vec![crc32c(data)],
        )
        .unwrap();
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        m
    }

    #[test]
    fn pins_split_into_blobs_and_uploads() {
        let (a, b) = (blob_id_of(b"a"), blob_id_of(b"b"));
        let mark = Mark::new([a], [GcObject::Blob(b), GcObject::Upload(UploadId::new(3))]);
        assert_eq!(mark.blobs, [a, b].into());
        assert_eq!(mark.uploads, [UploadId::new(3)].into());
    }

    #[test]
    fn marked_manifests_add_their_uploads_except_inline_ones() {
        let store = remote();
        let big = staged(&store, &vec![1; 300], 11);
        let unmarked = staged(&store, &vec![2; 300], 12);
        let small = BlobManifest::new(
            blob_id_of(b"tiny"),
            4,
            ChunkSize::DEFAULT,
            UploadId::new(13),
            vec![crc32c(b"tiny")],
        )
        .unwrap();
        assert_eq!(
            store.bind_small(&small, b"tiny").unwrap(),
            BindOutcome::Bound
        );
        let mut mark = Mark::new([big.blob_id(), small.blob_id()], []);
        mark.add_manifest_uploads(&store).unwrap();
        // The inline manifest names upload 0 and stages nothing; the unmarked blob's upload is not kept.
        assert_eq!(mark.uploads, [UploadId::new(11)].into());
        assert!(!mark.uploads.contains(&unmarked.upload_id()));
        assert!(mark.missing.is_empty() && mark.unreadable.is_empty());
    }

    #[test]
    fn a_missing_or_unreadable_manifest_is_listed_and_a_failing_store_fails_the_mark() {
        let store = remote();
        let damaged = staged(&store, &vec![3; 300], 21);
        let key = RemoteKey::Manifest(damaged.blob_id()).key().unwrap();
        persia_storage::Storage::put(store.storage(), &key, Bytes::from_static(b"not a manifest"))
            .unwrap();
        let gone = blob_id_of(b"gone");
        // A blob only pinned (a `put_blob` before its bind) is not dangling.
        let in_flight = blob_id_of(b"pinned, not bound yet");
        let mut mark = Mark::new([damaged.blob_id(), gone], [GcObject::Blob(in_flight)]);
        mark.add_manifest_uploads(&store).unwrap();
        assert_eq!(mark.missing, [gone]);
        assert_eq!(mark.unreadable, [damaged.blob_id()]);
        assert!(mark.uploads.is_empty());

        // A store that cannot answer stops the mark: a sweep on a partial one could delete a live upload.
        let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
        let failing = RemoteBlobStore::new(FaultyStorage::new(MemoryStorage::with_clock(clock), 1));
        failing
            .storage()
            .inject(Trigger::Nth(persia_storage::Op::Get, 1), Fault::Io);
        let mut mark = Mark::new([gone], []);
        assert!(matches!(
            mark.add_manifest_uploads(&failing),
            Err(Error::Storage(_))
        ));
    }
}
