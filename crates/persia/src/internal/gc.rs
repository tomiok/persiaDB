//! Garbage collection of the blob store (SPEC §9.5; ROADMAP 9.5.1, 9.5.2, 9.5.8; ADR-0052): the mark over the
//! engine's references and the pin set, then the store's sweep.
//!
//! GC runs in the writer, the process that runs `put_blob` and holds the pin set (SPEC §9.5). [`BlobDatabase::mark`]
//! takes the pinned ids, then the blob ids the engine's view references ([`BlobReferences`], SPEC §9.5: live and held
//! snapshots, obsolete segments still held, the WAL from `wal_start` on), then the upload ids the marked manifests
//! name. It borrows the database shared, so no document can be logged, deleted or committed while it runs: nothing
//! the engine does not reference can become referenced before the sweep, except through a pin, which the sweep
//! checks again under the pin set's lock.

use std::collections::HashSet;

use persia_blob::gc::{GcObject, Mark, PinCheck, PinGate};
use persia_blob::{BlobId, BlobStore};
use persia_engine::db::{Database, DatabaseError};

use super::documents::{BlobDatabase, DocumentLog};
use super::pins::{PinSet, PinView, Pinned};

/// Errors of [`BlobDatabase::mark`] and GC.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    /// The blob store failed, or holds bytes it cannot read where GC needs them. Whatever the sweep deleted before
    /// is unreferenced; nothing else was.
    #[error(transparent)]
    Blob(#[from] persia_blob::Error),
    /// The engine could not list its references (a poisoned database, an unreadable segment or WAL chunk): nothing
    /// was deleted.
    #[error(transparent)]
    Database(Box<DatabaseError>),
}

impl From<DatabaseError> for GcError {
    fn from(e: DatabaseError) -> Self {
        Self::Database(Box::new(e))
    }
}

/// The document log's side of GC's mark: every blob id its documents may still name (SPEC §9.5). The engine's
/// [`Database`] in production ([`Database::referenced_blobs`]), a wrapper in tests.
pub trait BlobReferences {
    /// [`Database::referenced_blobs`].
    ///
    /// # Errors
    /// As [`Database::referenced_blobs`].
    #[allow(clippy::result_large_err)] // The engine's own error, as `Database::referenced_blobs` returns it.
    fn referenced_blobs(&self) -> Result<HashSet<BlobId>, DatabaseError>;
}

impl BlobReferences for Database {
    #[allow(clippy::result_large_err)] // As the trait.
    fn referenced_blobs(&self) -> Result<HashSet<BlobId>, DatabaseError> {
        Database::referenced_blobs(self)
    }
}

impl<S: BlobStore + ?Sized, L: DocumentLog + BlobReferences> BlobDatabase<S, L> {
    /// GC's mark (SPEC §9.5, ROADMAP 9.5.1): the pinned ids, the blob ids the log references, then the upload ids the
    /// marked blobs' manifests name ([`Mark::add_manifest_uploads`]).
    ///
    /// The pins are read first: a document's pins are released only once its record is durable, which takes a
    /// `&mut` call this borrow excludes, so every document is seen through its pins, its record, or both.
    ///
    /// # Errors
    /// [`GcError::Database`] when the log cannot list its references; [`GcError::Blob`] when the store cannot be
    /// read (an unreadable manifest is listed in [`Mark::unreadable`] instead).
    pub fn mark(&self) -> Result<Mark, GcError> {
        let pinned = PinGate::pinned(&**self.blobs().pin_set());
        let referenced = self.log().referenced_blobs()?;
        let mut mark = Mark::new(referenced, pinned);
        mark.add_manifest_uploads(&**self.blobs().store())?;
        Ok(mark)
    }
}

const fn object(pinned: Pinned) -> GcObject {
    match pinned {
        Pinned::Upload(id) => GcObject::Upload(id),
        Pinned::Blob(id) => GcObject::Blob(id),
    }
}

const fn pinned(object: GcObject) -> Pinned {
    match object {
        GcObject::Upload(id) => Pinned::Upload(id),
        GcObject::Blob(id) => Pinned::Blob(id),
    }
}

impl PinGate for PinSet {
    fn pinned(&self) -> Vec<GcObject> {
        PinSet::pinned(self).into_iter().map(object).collect()
    }

    fn unless_pinned<T>(
        &self,
        object: GcObject,
        delete: impl FnOnce(&dyn PinCheck) -> T,
    ) -> Option<T> {
        PinSet::unless_pinned(self, pinned(object), |view| delete(view))
    }
}

impl PinCheck for PinView<'_> {
    fn is_pinned(&self, object: GcObject) -> bool {
        PinView::is_pinned(*self, pinned(object))
    }

    fn pinned_blobs(&self) -> Vec<BlobId> {
        PinView::pinned(*self)
            .into_iter()
            .filter_map(|p| match p {
                Pinned::Blob(id) => Some(id),
                Pinned::Upload(_) => None,
            })
            .collect()
    }
}
