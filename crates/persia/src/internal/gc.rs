//! Garbage collection of the blob store (SPEC §9.5; ROADMAP 9.5.1, 9.5.2, 9.5.8; ADR-0052): the mark over the
//! engine's references and the pin set, then the store's sweep.
//!
//! GC runs in the writer, the process that runs `put_blob` and holds the pin set (SPEC §9.5), only when the writer
//! calls [`BlobDatabase::gc`]: nothing triggers it in the background. [`BlobDatabase::mark`] takes the pinned ids,
//! then the blob ids the engine's view references ([`BlobReferences`], SPEC §9.5: live and held snapshots, obsolete
//! segments still held, the WAL from `wal_start` on), then the upload ids the marked manifests name. Both borrow the
//! database shared, so no document can be logged, deleted or committed while they run: nothing the engine does not
//! reference can become referenced before the sweep, except through a pin, which the sweep checks again under the
//! pin set's lock. `put_blob` and reads may run meanwhile, from other threads.
//!
//! The view is this process's: readers in other processes are not seen. [`persia_blob::PackStore::refresh`]
//! keeps the pack file it opened until it returns [`persia_blob::Error::PackReplaced`] and does not reopen it
//! (ROADMAP 9.2.13). [`ReadOnlyBlobDatabase::refresh`](super::documents::ReadOnlyBlobDatabase::refresh) does reopen
//! the pack read-only at that path, and restores the previous documents if the pack refresh fails any other way.
//! Remotely a reader that lags behind the writer may find the blob of a document the writer has deleted gone
//! (ADR-0052, ROADMAP 13.2.5).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

pub use persia_blob::gc::SweepReport;
use persia_blob::gc::{GcObject, Mark, PinCheck, PinGate, Sweep, SweepOptions};
pub use persia_blob::pack::lease::ReaderLeaseReport;
use persia_blob::{BlobId, BlobStore};
// What [`GcReport`] holds, for the CLI (`persia gc`, ROADMAP 10.4.5), which reaches `persia-blob` only through here.
pub use persia_blob::Error as BlobError;
pub use persia_blob::gc::Rewrite;
use persia_engine::db::{Database, DatabaseError};

use super::blob::MAX_BIND_BUDGET;
use super::documents::{BlobDatabase, DocumentLog};
use super::pins::{PinSet, PinView, Pinned};

/// How long an unmarked remote object is kept after it was written, by default (SPEC §9.5).
pub const DEFAULT_GRACE: Duration = Duration::from_hours(24);

/// The share of the pack's bytes a sweep must drop before it rewrites the pack, by default.
pub const DEFAULT_REWRITE_RATIO: f64 = 0.5;

/// How [`BlobDatabase::gc`] runs (SPEC §9.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GcOptions {
    /// How long an unmarked remote object is kept after it was written (staged chunks: after their upload's newest
    /// chunk). [`DEFAULT_GRACE`] by default, and at least twice [`MAX_BIND_BUDGET`] (2 h): an attempt started just
    /// before the deadline can land its `put_if_absent` after the budget ends (SPEC §9.4). The pack has none.
    pub grace: Duration,
    /// The pack is rewritten only when it would drop at least this share of its frames' bytes: 0 forces the rewrite,
    /// above 1 it never runs. [`DEFAULT_REWRITE_RATIO`] by default. The remote layout ignores it.
    pub rewrite_ratio: f64,
    /// The time ages are measured at, by the backend's clock (its objects' `last_modified`, SPEC §10.1); `None`
    /// reads the system clock. Tests pass their injected clock's time.
    pub now: Option<SystemTime>,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            grace: DEFAULT_GRACE,
            rewrite_ratio: DEFAULT_REWRITE_RATIO,
            now: None,
        }
    }
}

/// What [`BlobDatabase::gc`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcReport {
    /// Blob ids marked: referenced or pinned.
    pub marked_blobs: usize,
    /// Upload ids marked: pinned, or named by a marked manifest.
    pub marked_uploads: usize,
    /// Referenced blobs the store has no manifest for (dangling references, which `verify` reports).
    pub missing: Vec<BlobId>,
    /// What the sweep did: nothing (the default report) when the reader leases held it back.
    pub sweep: SweepReport,
    /// The reader leases of the pack (SPEC §9.3, §9.5; ROADMAP 9.2.16), judged against the committed seq; `None` for
    /// a store without them (the remote layout) or no pack yet. GC does not sweep while any reader lease is behind
    /// the committed seq (or opening, ahead, unreadable): [`ReaderLeaseReport::held_back`]. Every commit and every
    /// compaction puts every read-only handle of another process behind until it refreshes.
    pub readers: Option<ReaderLeaseReport>,
    /// A [`GcHold`] lived (a backup copying the pack, ROADMAP 10.1.8): nothing was marked or swept.
    pub held_by_copy: bool,
}

/// Errors of [`BlobDatabase::mark`] and [`BlobDatabase::gc`].
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    /// The blob store failed, or holds bytes it cannot read where GC needs them (a poisoned pack among them:
    /// [`persia_blob::Error::is_store_poisoned`]). Whatever the sweep deleted before was unmarked.
    #[error(transparent)]
    Blob(#[from] persia_blob::Error),
    /// The engine could not list its references (a poisoned database, an unreadable segment or WAL chunk): nothing
    /// was deleted.
    #[error(transparent)]
    Database(Box<DatabaseError>),
    /// [`GcOptions::grace`] is below twice [`MAX_BIND_BUDGET`] (2 h). An attempt started just before the deadline can
    /// land its `put_if_absent` after the budget ends, so a grace of only the budget could sweep those chunks while
    /// that write is still in flight (SPEC §9.4). Nothing was done.
    #[error("the grace period {grace:?} is below {floor:?}, twice the longest bind budget")]
    GraceBelowBindBudget {
        /// The grace period asked for.
        grace: Duration,
        /// `2 * `[`MAX_BIND_BUDGET`].
        floor: Duration,
    },
    /// [`GcOptions::rewrite_ratio`] is negative or not a number. Nothing was done.
    #[error("invalid rewrite ratio {0}")]
    InvalidRewriteRatio(f64),
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

    /// The committed seq ([`Database::seq`]): what GC judges reader leases against (SPEC §9.5, ROADMAP 9.2.16).
    fn committed_seq(&self) -> u64;
}

impl BlobReferences for Database {
    #[allow(clippy::result_large_err)] // As the trait.
    fn referenced_blobs(&self) -> Result<HashSet<BlobId>, DatabaseError> {
        Database::referenced_blobs(self)
    }

    fn committed_seq(&self) -> u64 {
        Database::seq(self)
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

impl<S: BlobStore + Sweep + ?Sized, L: DocumentLog + BlobReferences> BlobDatabase<S, L> {
    /// Collects the blob store's garbage (SPEC §9.5; ROADMAP 9.5.1, 9.5.2): [`BlobDatabase::mark`], then the store's
    /// [`Sweep`] with the pin set. Remotely, an unmarked blob manifest older than [`GcOptions::grace`] is deleted,
    /// then the staged chunks of every upload no kept manifest names, aged from its newest chunk and listed again under
    /// the pin set's lock before its chunks go (a change since the first listing keeps it this round, ROADMAP 9.5.10); an
    /// object of unknown age is kept and counted. Locally the pack is rewritten without what nothing keeps, when that is
    /// at least [`GcOptions::rewrite_ratio`] of it. Every pin check and delete is one step under the pin set's lock, so
    /// a `put_blob` or a read that starts meanwhile keeps what it pins.
    ///
    /// While a [`GcHold`] lives ([`BlobDatabase::hold_gc`], a backup copying the pack) it marks and sweeps nothing.
    ///
    /// Only the writer calls it, never in the background: it borrows the database shared, so it runs between writes,
    /// while `put_blob` and reads may go on in other threads (a `put_blob` into the pack waits for a rewrite). Two
    /// calls at once are safe: rewrites take the pack's writer mutex in turn, and remote deletes are idempotent.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use persia::internal::blob::{Blobs, PutBlobOptions};
    /// use persia::internal::documents::{Blob, BlobDatabase};
    /// use persia::internal::gc::GcOptions;
    /// use persia_blob::{BlobStore, PackStore};
    /// use persia_engine::db::{Database, DatabaseOptions};
    /// use persia_engine::document::Document;
    /// use persia_engine::schema::SchemaBuilder;
    /// use persia_engine::wal::Durability;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let mut log = Database::create(dir.path().join("app.persia"), DatabaseOptions::default())?;
    /// log.create_collection(SchemaBuilder::new("files").build()?)?;
    /// let proof = log.writer_proof(); // the pack opens read-write only with it
    /// let pack = PackStore::open_writer(dir.path().join("app.persia.blobs"), &proof)?;
    /// let mut db = BlobDatabase::new(log, Blobs::new(Arc::new(pack)));
    /// let kept = db.put("files", &Document::new("a"), Blob::from_reader(&b"kept"[..]), Durability::Fsync)?;
    /// let (orphan, pins) = db.put_blob(&[7; 1000][..], &PutBlobOptions::default())?;
    /// drop(pins); // nothing references it now
    ///
    /// let report = db.gc(&GcOptions::default())?;
    /// assert_eq!(report.sweep.blobs_deleted, 1);
    /// assert!(db.blobs().store().contains(&kept)?);
    /// assert!(!db.blobs().store().contains(&orphan)?);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`GcError::GraceBelowBindBudget`] and [`GcError::InvalidRewriteRatio`] before anything is read; otherwise as
    /// [`BlobDatabase::mark`] and the store's sweep ([`GcError::Blob`]): the sweep stops at the first error, and what
    /// it deleted before was unmarked.
    pub fn gc(&self, options: &GcOptions) -> Result<GcReport, GcError> {
        // Twice the cap, not the cap: a `put_if_absent` started inside the budget can land after it (SPEC §9.4).
        if options.grace < 2 * MAX_BIND_BUDGET {
            return Err(GcError::GraceBelowBindBudget {
                grace: options.grace,
                floor: 2 * MAX_BIND_BUDGET,
            });
        }
        if options.rewrite_ratio.is_nan() || options.rewrite_ratio < 0.0 {
            return Err(GcError::InvalidRewriteRatio(options.rewrite_ratio));
        }
        if self.gc_held() {
            return Ok(GcReport {
                held_by_copy: true,
                ..GcReport::default()
            });
        }
        let mark = self.mark()?;
        // With the log frozen (this borrow), the committed seq is the mark's (SPEC §9.5, ADR-0054 item 9).
        let readers = self
            .blobs()
            .store()
            .reader_leases()?
            .map(|scan| scan.judge(self.log().committed_seq()));
        if readers.as_ref().is_some_and(|r| r.held_back) {
            return Ok(GcReport {
                marked_blobs: mark.blobs.len(),
                marked_uploads: mark.uploads.len(),
                missing: mark.missing,
                sweep: SweepReport::default(),
                readers,
                held_by_copy: false,
            });
        }
        let sweep = SweepOptions {
            grace: options.grace,
            now: options.now.unwrap_or_else(SystemTime::now),
            rewrite_ratio: options.rewrite_ratio,
        };
        let swept = self
            .blobs()
            .store()
            .sweep(&mark, &**self.blobs().pin_set(), &sweep)?;
        Ok(GcReport {
            marked_blobs: mark.blobs.len(),
            marked_uploads: mark.uploads.len(),
            missing: mark.missing,
            sweep: swept,
            readers,
            held_by_copy: false,
        })
    }
}
/// Holds [`BlobDatabase::gc`] back while it lives ([`BlobDatabase::hold_gc`]): a backup copying the pack takes one, so
/// that no GC rewrites the pack (SPEC §9.3, §9.5) until the copy ends (SPEC §4.8, ROADMAP 10.1.8). Released when
/// dropped, from any thread.
#[derive(Debug)]
pub struct GcHold(Arc<AtomicUsize>);

impl Drop for GcHold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<S: ?Sized, L> BlobDatabase<S, L> {
    /// Holds GC back until the returned hold is dropped: a [`BlobDatabase::gc`] that starts while any hold lives marks
    /// and sweeps nothing and reports [`GcReport::held_by_copy`]. A GC already running when the hold is taken is not
    /// stopped: take it where no GC runs (`persia`'s `Db` takes it under its writer mutex, which GC also needs).
    ///
    /// ```
    /// use persia::internal::documents::BlobDatabase;
    /// use persia::internal::gc::GcOptions;
    /// use persia_engine::db::DatabaseOptions;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = BlobDatabase::create(dir.path().join("app.persia"), DatabaseOptions::default())?;
    /// let hold = db.hold_gc();
    /// assert!(db.gc(&GcOptions::default())?.held_by_copy);
    /// drop(hold);
    /// assert!(!db.gc(&GcOptions::default())?.held_by_copy);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn hold_gc(&self) -> GcHold {
        self.gc_holds.fetch_add(1, Ordering::SeqCst);
        GcHold(Arc::clone(&self.gc_holds))
    }

    /// Whether a [`GcHold`] lives.
    pub fn gc_held(&self) -> bool {
        self.gc_holds.load(Ordering::SeqCst) > 0
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
