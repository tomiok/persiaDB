//! Documents that reference blobs: `put(doc, blob)` (SPEC §9.5, §9.6; ROADMAP 9.4.1, ADR-0051).
//!
//! [`BlobDatabase::put`] stores the blob first and logs the document second: the blob is durable when
//! [`Blobs::put_blob`] returns (the store's `bind` answered), so no WAL byte ever names a blob that is not, whatever
//! the document's [`Durability`]. A [`Blob::Stored`] reference is pinned, then checked against the store, before the
//! document is logged; one the store lacks is an error and logs nothing. The blob's pins are kept until the WAL
//! record that references it is durable ([`DocumentLog::durable_mark`] reaches its [`DocumentLog::wal_mark`]): at
//! once for `Fsync`, at the next commit or `Fsync` operation otherwise (SPEC §9.5).
//!
//! [`BlobDatabase::create`] and [`BlobDatabase::open`] open a database with its pack, `<name>.persia.blobs`, read-write
//! with the database's writer proof (SPEC §9.3; ROADMAP 9.2.7, 9.4.5); [`ReadOnlyBlobDatabase`] opens both read-only.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

/// A read-only handle's reader lease (SPEC §9.3, ROADMAP 9.2.16, ADR-0054): [`ReaderLease::take_beside`], then
/// [`ReadOnlyBlobDatabase::with_lease`].
pub use persia_blob::pack::lease::ReaderLease;
use persia_blob::pack::lease::leases_dir;
use persia_blob::pack::{ContainerFile, RealFs, Vfs};
use persia_blob::{BlobId, BlobRef, BlobStore, PackStore, pack_path};
use persia_engine::db::{
    Database, DatabaseError, DatabaseOptions, MappedReads, MarkOnlyDatabase, ReadOnlyDatabase,
    ReadOnlyOptions,
};
use persia_engine::document::Document;
use persia_engine::wal::{Durability, WalMark};

use super::blob::{BlobReader, Blobs, PutBlobOptions};
use super::pins::Pins;

/// Errors of [`BlobDatabase`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Storing or reading the blob failed (nothing was logged when `put` returns it).
    #[error(transparent)]
    Blob(#[from] persia_blob::Error),
    /// Logging the document failed, as [`Database::put`] reports it (boxed: it is large).
    #[error(transparent)]
    Database(Box<DatabaseError>),
    /// The file of a [`Blob::Path`] could not be opened.
    #[error("cannot open the blob file {path}")]
    Open {
        /// The path.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// A [`Blob::Stored`] reference names a blob this database's store has not bound (SPEC §9.6: no dangling
    /// references). Nothing was logged.
    #[error("blob {blob_id} is not in this database's blob store")]
    BlobNotStored {
        /// The id the reference names.
        blob_id: BlobId,
    },
    /// [`BlobDatabase::create`] found a file at the pack's path already (a leftover of another database, or of
    /// this path's earlier one): nothing was created, since the database would open with a pack it cannot use.
    #[error("a blob pack already exists at {path}: move it away before creating a database there")]
    PackExists {
        /// The pack's path.
        path: PathBuf,
    },
    /// [`ReadOnlyBlobDatabase::refresh`] was given the database opened at another canonical path (a copy): nothing
    /// changed.
    #[error("the reopened database is at {reopened:?}, not {current:?}")]
    RefreshedElsewhere {
        /// Where the current one was opened.
        current: Option<PathBuf>,
        /// Where the reopened one was.
        reopened: Option<PathBuf>,
    },
    /// [`ReadOnlyBlobDatabase::refresh`] was given an open older than the current one: nothing changed. Older is a
    /// lower manifest `seq`, or the same `seq` with both fewer replayed WAL records and an earlier WAL end
    /// ([`ReadOnlyDatabase::wal_end`]). After a crash that lost `Async` data the reader stayed ahead, so open the
    /// reader again.
    #[error(
        "the reopened database is older than the current one (seq {reopened}, {reopened_replayed} replayed WAL records ending at {reopened_end}; current seq {current}, {current_replayed} replayed ending at {current_end})"
    )]
    RefreshedOlder {
        /// The current manifest `seq`.
        current: u64,
        /// The reopened one's.
        reopened: u64,
        /// WAL records the current open replayed.
        current_replayed: u64,
        /// WAL records the reopened one replayed.
        reopened_replayed: u64,
        /// End of the WAL the current open replayed ([`ReadOnlyDatabase::wal_end`]).
        current_end: u64,
        /// End of the WAL the reopened one replayed.
        reopened_end: u64,
    },
    /// [`ReadOnlyBlobDatabase::refresh`] was given an open of the same manifest `seq` whose replayed WAL records and
    /// WAL end ([`ReadOnlyDatabase::wal_end`]) disagree with the current one's: the same count at another end, or
    /// more records at an end not past the current one, or fewer at an end not before it. Within one `seq` the WAL
    /// only grows, every record past the end of the last, so the file at the path now holds another history (a copy
    /// or a restore was put there): neither open is older. Nothing changed; open the reader again. Detection is
    /// best-effort: another history with a higher `seq`, or with the same `seq`, records and WAL end but other
    /// records, is taken. Do not rely on this error to catch a copy or a restore.
    #[error(
        "the reopened database diverged from the current one at seq {current} ({reopened_replayed} replayed WAL records ending at {reopened_end}; current {current_replayed} ending at {current_end})"
    )]
    RefreshDiverged {
        /// The current manifest `seq`.
        current: u64,
        /// The reopened one's (the same).
        reopened: u64,
        /// WAL records the current open replayed.
        current_replayed: u64,
        /// WAL records the reopened one replayed.
        reopened_replayed: u64,
        /// End of the WAL the current open replayed ([`ReadOnlyDatabase::wal_end`]).
        current_end: u64,
        /// End of the WAL the reopened one replayed.
        reopened_end: u64,
    },
    // -- reader leases (9.2.16) --
    /// The documents given to [`ReadOnlyBlobDatabase::with_lease`] are older than the container's committed seq
    /// when the reader lease was taken ([`ReaderLease::taken_seq`]): a GC may have swept a blob they show before the
    /// lease existed (SPEC §9.3, ADR-0054 item 4). Nothing was opened; open the documents again.
    #[error(
        "the documents (seq {opened}) were opened before the reader lease (taken at seq {taken}): open them again"
    )]
    OpenedBeforeLease {
        /// The documents' seq.
        opened: u64,
        /// The container's committed seq once the reader lease was held.
        taken: u64,
    },
    /// The reader lease is in another leases directory than the one beside the pack these documents use (another
    /// database, or a symbolic link retargeted between the lease and the documents' open). The paths are compared
    /// exactly, both derived from canonical container paths (or the pack path given to `with_lease_in`). Nothing was
    /// opened.
    #[error("the reader lease is in {lease}, not {expected}")]
    LeaseElsewhere {
        /// The lease's directory.
        lease: PathBuf,
        /// The leases directory of the documents' pack.
        expected: PathBuf,
    },
    /// [`ReadOnlyBlobDatabase::with_pack_in`] was given documents of a container opened without a path
    /// ([`ReadOnlyDatabase::open_on`]): a reader lease must reopen the container at its path to read its committed seq,
    /// or [`Error::OpenedBeforeLease`] could not be checked. Nothing was opened.
    #[error("the documents' container has no path, so no reader lease can be taken for them")]
    NoContainerPath,
}

/// The blob of a document (SPEC §9.5, §9.6) as [`BlobDatabase::put`] takes it: bytes to store with it, or a blob
/// stored before. The facade's [`crate::Blob`] converts to it (ROADMAP 10.1.15). Bytes are stored, and durable, before
/// the document is logged; a stored blob must be in this database's store.
///
/// ```
/// use persia::internal::blob::PutBlobOptions;
/// use persia::internal::documents::{Blob, BlobDatabase};
/// use persia_engine::db::DatabaseOptions;
/// use persia_engine::document::Document;
/// use persia_engine::schema::SchemaBuilder;
/// use persia_engine::wal::Durability;
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// let mut db = BlobDatabase::create(dir.path().join("app.persia"), DatabaseOptions::default())?;
/// db.log_mut().create_collection(SchemaBuilder::new("files").build()?)?;
/// let text = PutBlobOptions { mime: "text/plain".into(), ..PutBlobOptions::default() };
/// let blob = Blob::from_reader(&b"hi"[..]).with_options(text);
/// let stored = db.put("files", &Document::new("a"), blob, Durability::Fsync)?;
/// assert_eq!((stored.mime(), db.get_blob_range(&stored, 0..2)?), ("text/plain", b"hi".to_vec()));
/// db.put("files", &Document::new("b"), Blob::Stored(stored), Durability::Fsync)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub enum Blob<'r> {
    /// Bytes to store, from a reader ([`Blob::from_reader`]).
    Reader(Box<dyn Read + 'r>, PutBlobOptions),
    /// Bytes to store, from the file at the path ([`Blob::from_path`]).
    Path(PathBuf, PutBlobOptions),
    /// A blob this database stored before, by its reference.
    Stored(BlobRef),
}

impl<'r> Blob<'r> {
    /// The bytes `reader` yields, with the default [`PutBlobOptions`].
    pub fn from_reader(reader: impl Read + 'r) -> Self {
        Self::Reader(Box::new(reader), PutBlobOptions::default())
    }

    /// The bytes of the file at `path`, with the default [`PutBlobOptions`]. The file is opened when the blob is
    /// stored; a missing one is an [`Error::Open`], and nothing is logged.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into(), PutBlobOptions::default())
    }

    /// `options` instead of the defaults; no effect on [`Blob::Stored`], whose blob keeps what it was stored with.
    #[must_use]
    pub fn with_options(self, options: PutBlobOptions) -> Self {
        match self {
            Self::Reader(reader, _) => Self::Reader(reader, options),
            Self::Path(path, _) => Self::Path(path, options),
            stored @ Self::Stored(_) => stored,
        }
    }
}

impl std::fmt::Debug for Blob<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reader(_, options) => f.debug_tuple("Reader").field(options).finish(),
            Self::Path(path, options) => f.debug_tuple("Path").field(path).field(options).finish(),
            Self::Stored(blob) => f.debug_tuple("Stored").field(blob).finish(),
        }
    }
}

/// Where `put` logs documents: the engine's [`Database`], or a wrapper around it (tests check the order of the
/// store's calls against the log's through one).
pub trait DocumentLog {
    /// [`Database::put`].
    ///
    /// # Errors
    /// As [`Database::put`].
    #[allow(clippy::result_large_err)] // The engine's own error, as `Database::put` returns it.
    fn put(
        &mut self,
        collection: &str,
        doc: &Document,
        durability: Durability,
    ) -> Result<(), DatabaseError>;

    /// [`Database::wal_mark`].
    fn wal_mark(&self) -> WalMark;

    /// [`Database::durable_mark`].
    fn durable_mark(&self) -> WalMark;

    /// The container `db_uuid`, when this log is a database a pack was opened with. [`BlobDatabase::log_mut`]
    /// checks it in debug builds so replacing the log cannot pair the pack with another database.
    fn container_db_uuid(&self) -> Option<[u8; 16]> {
        None
    }

    /// [`Database::commit`].
    ///
    /// # Errors
    /// As [`Database::commit`].
    #[allow(clippy::result_large_err)] // The engine's own error, as `Database::commit` returns it.
    fn commit(&mut self) -> Result<u64, DatabaseError>;
}

impl<F: ContainerFile + Sync> DocumentLog for Database<F> {
    #[allow(clippy::result_large_err)] // As the trait.
    fn put(
        &mut self,
        collection: &str,
        doc: &Document,
        durability: Durability,
    ) -> Result<(), DatabaseError> {
        Database::put(self, collection, doc, durability)
    }

    fn wal_mark(&self) -> WalMark {
        Database::wal_mark(self)
    }

    fn durable_mark(&self) -> WalMark {
        Database::durable_mark(self)
    }

    fn container_db_uuid(&self) -> Option<[u8; 16]> {
        Some(self.container().header().db_uuid)
    }

    #[allow(clippy::result_large_err)] // As the trait.
    fn commit(&mut self) -> Result<u64, DatabaseError> {
        Database::commit(self)
    }
}

/// A database and its blob store: documents with blobs (see the module docs). M10's `Db` wraps it.
#[derive(Debug)]
pub struct BlobDatabase<S: ?Sized, L = Database> {
    /// Pins of blobs referenced by logged documents, with the mark of each document's record, oldest first: released
    /// once the record is durable.
    held: VecDeque<(WalMark, Pins)>,
    /// Declared before the log, so it drops first: a pack opened read-write with the log's writer proof must not
    /// outlive the container whose lock the proof stood for (ROADMAP 9.2.7, ADR-0053).
    pub(super) blobs: Blobs<S>,
    log: L,
    /// The `db_uuid` the pack was opened with, when [`BlobDatabase::with_pack`] did. [`LogMut`]'s drop checks the
    /// log still names it (debug builds).
    pack_db_uuid: Option<[u8; 16]>,
    /// How many [`GcHold`](super::gc::GcHold)s live: while any does, [`BlobDatabase::gc`] sweeps nothing (a backup
    /// copies the pack, ROADMAP 10.1.8).
    pub(super) gc_holds: Arc<AtomicUsize>,
}

impl<S: ?Sized, L> BlobDatabase<S, L> {
    /// The document log.
    pub fn log(&self) -> &L {
        &self.log
    }
}

impl<S: BlobStore + ?Sized, L: DocumentLog> BlobDatabase<S, L> {
    /// `log`'s documents with `blobs`' blobs.
    pub fn new(log: L, blobs: Blobs<S>) -> Self {
        Self {
            log,
            held: VecDeque::new(),
            blobs,
            pack_db_uuid: None,
            gc_holds: Arc::default(),
        }
    }

    /// The document log, for the operations this layer does not wrap (create a collection, delete, finish merges,
    /// and the engine's own `put`). Call [`BlobDatabase::release_durable`] after one that may make records durable,
    /// or let the next `put` do it.
    ///
    /// The pack was opened with this log's writer proof. Replacing the log is checked in debug builds: when the
    /// handle drops, its `db_uuid` must still be the one the pack was opened with. To reopen the database, drop
    /// this value and open a new one (the pack's writer lock refuses a second writer while this pack lives, however
    /// it is kept alive, ROADMAP 9.2.7). The engine's `put` takes a document with any blob reference: M10 must not
    /// expose it (SPEC §9.6).
    pub fn log_mut(&mut self) -> LogMut<'_, L> {
        LogMut {
            log: &mut self.log,
            pack_db_uuid: self.pack_db_uuid,
        }
    }

    /// The blob store with its pin set. Only with a [`DocumentLog`]: through the store a blob can be stored, so a
    /// handle over a [`MarkOnlyDatabase`] (which collects only, ADR-0069 item 3) does not offer it.
    pub fn blobs(&self) -> &Blobs<S> {
        &self.blobs
    }

    /// Stores `blob` if it is bytes, else checks this store holds it, then logs `doc` of `collection` with its
    /// reference, at `durability` (SPEC §9.5: the blob durable, then the document). `doc`'s own blob reference is
    /// replaced. Returns the reference logged.
    ///
    /// # Errors
    /// - [`Error::Blob`] or [`Error::Open`] when the blob cannot be read or stored: nothing is logged;
    /// - [`Error::BlobNotStored`] for a [`Blob::Stored`] the store lacks: nothing is logged;
    /// - [`Error::Blob`] of [`persia_blob::Error::BlobRefMismatch`] for a [`Blob::Stored`] whose size or chunk size is
    ///   not the bound blob's (the caller's error, ROADMAP 10.1.33): nothing is logged;
    /// - [`Error::Database`] as [`Database::put`]; the blob is stored and, unless the record may have reached the
    ///   WAL, unpinned (an orphan GC reclaims).
    pub fn put(
        &mut self,
        collection: &str,
        doc: &Document,
        blob: Blob<'_>,
        durability: Durability,
    ) -> Result<BlobRef, Error> {
        self.release_durable();
        let (blob_ref, pins) = match blob {
            Blob::Reader(reader, options) => self.blobs.put_blob(reader, &options)?,
            Blob::Path(path, options) => match File::open(&path) {
                Ok(file) => self.blobs.put_blob(file, &options)?,
                Err(source) => return Err(Error::Open { path, source }),
            },
            Blob::Stored(blob_ref) => {
                let Some(pins) = self.blobs.pin_contained(&blob_ref)? else {
                    return Err(Error::BlobNotStored {
                        blob_id: blob_ref.blob_id(),
                    });
                };
                (blob_ref, pins)
            }
        };
        let mut doc = doc.clone();
        doc.set_blob_ref(Some(blob_ref.clone()));
        let before = self.log.wal_mark();
        let logged = self.log.put(collection, &doc, durability);
        let mark = self.log.wal_mark();
        // A record appended, even by a put that failed afterwards (a failed sync), may be on disk: its pins wait for a
        // durable mark that a poisoned database never reaches, so they are kept while this value lives.
        if mark > before {
            self.held.push_back((mark, pins));
        }
        // Released on either outcome: a put that failed may have made earlier records durable first (a flush commit
        // for a full memtable syncs the WAL, then the document may be rejected).
        self.release_durable();
        logged?;
        Ok(blob_ref)
    }

    /// Stores a blob without a document (SPEC §9.6 `put_blob`): as [`Blobs::put_blob`].
    ///
    /// # Errors
    /// As [`Blobs::put_blob`].
    pub fn put_blob(
        &self,
        reader: impl Read,
        options: &PutBlobOptions,
    ) -> Result<(BlobRef, Pins), Error> {
        Ok(self.blobs.put_blob(reader, options)?)
    }

    /// A reader of the blob `blob_ref` refers to: as [`Blobs::get_blob`].
    ///
    /// # Errors
    /// As [`Blobs::get_blob`].
    pub fn get_blob(&self, blob_ref: &BlobRef) -> Result<BlobReader<'_, S>, Error> {
        Ok(self.blobs.get_blob(blob_ref)?)
    }

    /// Bytes `range` of the blob `blob_ref` refers to: as [`Blobs::get_blob_range`].
    ///
    /// # Errors
    /// As [`Blobs::get_blob_range`].
    pub fn get_blob_range(
        &self,
        blob_ref: &BlobRef,
        range: core::ops::Range<u64>,
    ) -> Result<Vec<u8>, Error> {
        Ok(self.blobs.get_blob_range(blob_ref, range)?)
    }

    /// Releases the pins of every document whose record is durable now (SPEC §9.5).
    pub fn release_durable(&mut self) {
        let durable = self.log.durable_mark();
        while self.held.front().is_some_and(|(mark, _)| *mark <= durable) {
            self.held.pop_front();
        }
    }

    /// How many logged documents still hold their blob's pins (not durable yet).
    pub fn held(&self) -> usize {
        self.held.len()
    }
}

impl<S: BlobStore + ?Sized, L: DocumentLog> BlobDatabase<S, L> {
    /// [`Database::commit`], then the pins of the documents it made durable are released, whether it succeeded or
    /// not.
    ///
    /// # Errors
    /// As [`Database::commit`].
    pub fn commit(&mut self) -> Result<u64, Error> {
        let committed = self.log.commit();
        // Even a failed commit may have synced the WAL first (SPEC §4.5 step 1): what is durable is released.
        self.release_durable();
        Ok(committed?)
    }
}

/// A mutable borrow of a [`BlobDatabase`]'s document log (see [`BlobDatabase::log_mut`]).
///
/// It forwards the operations this layer does not wrap, and dereferences to the log. Replacing the log in a debug
/// build is checked against the `db_uuid` the pack was opened with.
#[derive(Debug)]
pub struct LogMut<'a, L: DocumentLog + ?Sized> {
    log: &'a mut L,
    pack_db_uuid: Option<[u8; 16]>,
}

impl<L: DocumentLog + ?Sized> std::ops::Deref for LogMut<'_, L> {
    type Target = L;

    fn deref(&self) -> &L {
        self.log
    }
}

impl<L: DocumentLog + ?Sized> std::ops::DerefMut for LogMut<'_, L> {
    fn deref_mut(&mut self) -> &mut L {
        self.log
    }
}

impl<L: DocumentLog + ?Sized> Drop for LogMut<'_, L> {
    fn drop(&mut self) {
        if let (Some(expected), Some(found)) = (self.pack_db_uuid, self.log.container_db_uuid()) {
            debug_assert_eq!(
                found, expected,
                "replacing the log breaks the pack opened with its writer proof"
            );
        }
    }
}

/// Removes the container `create` just made at `container` on `vfs`, after the pack failed to open, and syncs its
/// directory so the removal is durable: a retry, also after a crash, must not find it (SPEC §9.3).
///
/// The caller still holds the database, and with it the writer lock (ROADMAP 10.2.7), until it drops the database
/// after this returns. A racing open stops at that lock while the name exists (`Locked`: on Unix the lock is on the
/// container file, on Windows the `<name>.lock` file beside it), and at its path lookup once the name is removed
/// (`NotFound`; on Windows also `PermissionDenied`, while a file the creator still has open waits to be deleted). On
/// Unix an open that had the file before the removal finds it unlinked once it takes the lock, and opens the path
/// again (`Container::open`). A racing create takes the lock beside the path first (Windows) or links its new file to
/// the name (Unix) and gets `AlreadyExists` while the name, or that lock, is taken; so the file removed is the one
/// `create` made. A removal or sync that fails is reported with the pack error: a retry would not work.
fn remove_created_container<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    container: &Path,
    error: Error,
) -> Error {
    let removed = match vfs.remove_file(container) {
        // Gone already (removed by hand): its directory is synced all the same.
        Err(cleanup) if cleanup.kind() != io::ErrorKind::NotFound => Err((
            "remove the database created before its pack failed to open",
            cleanup,
        )),
        _ => Ok(()),
    };
    let dir = container
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let done = removed.and_then(|()| {
        vfs.sync_dir(dir).map_err(|cleanup| {
            (
                "sync the directory of the database removed after its pack failed to open",
                cleanup,
            )
        })
    });
    match done {
        Ok(()) => error,
        Err((op, cleanup)) => Error::Blob(persia_blob::Error::PackIo {
            op,
            source: io::Error::other(format!(
                "{error}; then for {}: {op}: {cleanup}",
                container.display()
            )),
        }),
    }
}

/// Where [`BlobDatabase::finish_create_observed`] calls its observer, the database still held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateStage {
    /// The pack failed to open; the container is still at its path.
    PackFailed,
    /// The container was removed.
    Removed,
}

impl From<DatabaseError> for Error {
    fn from(e: DatabaseError) -> Self {
        Self::Database(Box::new(e))
    }
}

// `BlobDatabase` opened over a `Database` and its pack (`<name>.persia.blobs`, the container's `db_uuid`, the writer
// proof), and its read-only counterpart (SPEC §9.3; ROADMAP 9.2.7, 9.4.5; ADR-0053).

/// The pack beside `container`, the canonical path a container was opened at (so every process that opens the
/// database, by any name, finds the same pack), or beside `given` for a container without one. The pack path is fixed
/// at open: the handle keeps it, and does not follow a container renamed or moved later.
fn pack_beside(container: Option<&Path>, given: &Path) -> PathBuf {
    pack_path(container.unwrap_or(given))
}

/// A reader lease's error as a read-only open reports it: the container's failure as [`Error::Database`], with its
/// classification ([`Error::is_corruption`]), as [`ReadOnlyDatabase::open`] would have reported it.
fn lease_error(error: persia_blob::Error) -> Error {
    match error {
        persia_blob::Error::Container(e) => Error::Database(Box::new(DatabaseError::from(e))),
        other => Error::Blob(other),
    }
}

impl BlobDatabase<PackStore> {
    /// Creates a database in a new container file at `path` ([`Database::create`]) with its blob pack at
    /// [`pack_path`] of it, `<name>.persia.blobs`, opened read-write with the database's writer proof, so the pack
    /// carries the container's `db_uuid` (SPEC §9.3). The pack file is not created here: the first `put_blob` creates
    /// it, and a database that never stores a blob has none. Its writer lock file, `<name>.persia.blobs.lock`, is created
    /// here and never removed (ROADMAP 9.2.15).
    ///
    /// ```
    /// use persia::internal::blob::PutBlobOptions;
    /// use persia::internal::documents::BlobDatabase;
    /// use persia_engine::db::DatabaseOptions;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let db = BlobDatabase::create(&path, DatabaseOptions::default())?;
    /// assert!(!dir.path().join("app.persia.blobs").exists()); // no blob yet, no pack
    /// let (blob, _pins) = db.put_blob(&b"hello"[..], &PutBlobOptions::default())?;
    /// assert!(dir.path().join("app.persia.blobs").exists());
    /// let db_uuid = db.log().container().header().db_uuid;
    /// assert_eq!(db.blobs().store().header().map(|h| h.db_uuid), Some(db_uuid));
    /// drop(db);
    ///
    /// let db = BlobDatabase::open(&path, DatabaseOptions::default())?;
    /// assert_eq!(db.get_blob_range(&blob, 0..5)?, b"hello");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::PackExists`] if a file is at the pack's path already, before anything is created (a leftover pack
    /// would leave a database that opens with no usable pack); [`Error::Database`] as [`Database::create`];
    /// [`Error::Blob`] as [`PackStore::open_writer`]. If the container was created and the pack then fails to open
    /// ([`persia_blob::Error::PackLocked`], an I/O error), the new container is removed so a retry can create it.
    pub fn create(path: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self, Error> {
        let path = path.as_ref();
        // `path` does not exist yet, so its canonical form is unknown: the pack beside it as given names the same file
        // the container's canonical path will (only the file name is appended, SPEC §9.3).
        let leftover = pack_path(path);
        match std::fs::symlink_metadata(&leftover) {
            Ok(_) => return Err(Error::PackExists { path: leftover }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(persia_blob::Error::PackIo { op: "stat", source }.into()),
        }
        let db = Database::create(path, options)?;
        let container = db
            .container()
            .path()
            .map_or_else(|| path.to_path_buf(), Path::to_path_buf);
        let pack = pack_beside(Some(container.as_path()), path);
        Self::finish_create(db, &pack, &container)
    }

    /// `db`, just created at `container`, with its pack. A pack error removes the container: it was created for this
    /// call and never returned, and a retry must not find it (SPEC §9.3). It is removed before `db` drops, while the
    /// writer lock is still held (ROADMAP 10.2.7).
    fn finish_create(db: Database, pack: &Path, container: &Path) -> Result<Self, Error> {
        let vfs: Arc<dyn Vfs<File = File>> = Arc::new(RealFs);
        Self::finish_create_observed(db, &vfs, pack, container, |_| {})
    }

    /// Opens the database in the container file at `path` read-write ([`Database::open`]) with its blob pack, as
    /// [`BlobDatabase::create`] does. A pack that does not exist yet is not created until the first `put_blob`.
    ///
    /// # Errors
    /// [`Error::Database`] as [`Database::open`] (another writer holds the lock: no second pack writer can exist,
    /// ROADMAP 9.2.7); [`Error::Blob`] as [`PackStore::open_writer`].
    pub fn open(path: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self, Error> {
        let path = path.as_ref();
        let db = Database::open(path, options)?;
        let pack = pack_beside(db.container().path(), path);
        Self::with_pack(db, pack)
    }

    /// `db` with the pack at `pack` (normally [`pack_path`] of its container's path), opened read-write with `db`'s
    /// writer proof ([`PackStore::open_writer`]).
    ///
    /// # Errors
    /// As [`PackStore::open_writer`]: [`persia_blob::Error::PackLocked`] while another handle writes the pack.
    pub fn with_pack(db: Database, pack: impl AsRef<Path>) -> Result<Self, Error> {
        let store = {
            let proof = db.writer_proof();
            PackStore::open_writer(pack, &proof)?
        };
        Ok(Self::with_writer_pack(db, store))
    }
}

impl BlobDatabase<PackStore, MarkOnlyDatabase> {
    /// Opens the database at `path` for the blob GC only (ROADMAP 10.4.7, ADR-0069): [`MarkOnlyDatabase::open`], the
    /// writer's open with a no-token stand-in for each `text` field's analyzer `options.registry` lacks, refused before
    /// its replay when the WAL holds a record of such a field's collection (ROADMAP 10.4.8), then its pack read-write
    /// with its writer proof, as [`BlobDatabase::open`] does. The handle marks and sweeps ([`BlobDatabase::gc`]) and
    /// does nothing else: a [`MarkOnlyDatabase`] is no [`DocumentLog`], so nothing can be put, stored or committed
    /// through it.
    ///
    /// ```
    /// use persia::internal::documents::{Blob, BlobDatabase};
    /// use persia::internal::gc::GcOptions;
    /// use persia_engine::analysis::{Analyzer, AnalyzerRegistry, WhitespaceTokenizer};
    /// use persia_engine::db::DatabaseOptions;
    /// use persia_engine::document::Document;
    /// use persia_engine::schema::CollectionSchema;
    /// use persia_engine::wal::Durability;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// // The application registers `tags@1` and names it in a schema.
    /// let mut registry = AnalyzerRegistry::builtin();
    /// registry.register("tags", 1, Analyzer::builder(WhitespaceTokenizer).build())?;
    /// let schema = CollectionSchema::from_json_in(
    ///     r#"{"name": "notes", "fields": {"tags": {"type": "text", "analyzer": "tags"}}}"#,
    ///     &registry,
    /// )?;
    /// let app_options = || DatabaseOptions { registry: registry.clone(), ..DatabaseOptions::default() };
    /// let mut app = BlobDatabase::create(&path, app_options())?;
    /// app.log_mut().create_collection(schema)?;
    /// let kept = app.put("notes", &Document::new("n"), Blob::from_reader(&b"kept"[..]), Durability::Fsync)?;
    /// app.commit()?; // closed cleanly: no record of `notes` is left in the WAL
    /// drop(app);
    /// // A tool with the built-in analyzers only collects it.
    /// let db = BlobDatabase::open_mark_only(&path, DatabaseOptions::default())?;
    /// assert_eq!(db.log().skipped_analysis().len(), 1);
    /// let report = db.gc(&GcOptions::default())?;
    /// assert_eq!(report.marked_blobs, 1);
    /// drop(db); // it offers no blob store: nothing can be stored through it
    /// let app = BlobDatabase::open(&path, app_options())?;
    /// assert_eq!(app.get_blob_range(&kept, 0..4)?, b"kept");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// The handle offers no blob store, which needs a [`DocumentLog`] (through it a blob could be stored):
    ///
    /// ```compile_fail
    /// use persia::internal::documents::BlobDatabase;
    /// use persia_engine::db::DatabaseOptions;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let db = BlobDatabase::open_mark_only(dir.path().join("app.persia"), DatabaseOptions::default())?;
    /// let _store = db.blobs(); // no such method on a mark-only handle
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// and nothing can be put through it:
    ///
    /// ```compile_fail
    /// use persia::internal::documents::{Blob, BlobDatabase};
    /// use persia_engine::db::DatabaseOptions;
    /// use persia_engine::document::Document;
    /// use persia_engine::wal::Durability;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let mut db = BlobDatabase::open_mark_only(dir.path().join("app.persia"), DatabaseOptions::default())?;
    /// db.put("notes", &Document::new("n"), Blob::from_reader(&b"x"[..]), Durability::Fsync)?; // needs a `DocumentLog`
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::Database`] as [`MarkOnlyDatabase::open`] (another writer holds the lock; `AnalysisSkipped` when the
    /// WAL holds a record of a skipped field's collection, refused before the replay, ROADMAP 10.4.8); [`Error::Blob`]
    /// as [`PackStore::open_writer`].
    pub fn open_mark_only(path: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self, Error> {
        let path = path.as_ref();
        let db = MarkOnlyDatabase::open(path, options)?;
        let pack = pack_beside(db.container_path(), path);
        let (store, db_uuid) = {
            let proof = db.writer_proof();
            (PackStore::open_writer(pack, &proof)?, proof.db_uuid())
        };
        Ok(Self {
            held: VecDeque::new(),
            blobs: Blobs::new(Arc::new(store)),
            log: db,
            pack_db_uuid: Some(db_uuid),
            gc_holds: Arc::default(),
        })
    }
}

impl<F: ContainerFile + Sync + 'static> BlobDatabase<PackStore<F>, Database<F>> {
    /// [`BlobDatabase::with_pack`] with the pack on `vfs` (a simulated file system in crash tests).
    ///
    /// # Errors
    /// As [`PackStore::open_writer_in`]: [`persia_blob::Error::PackLocked`] while another handle writes the pack.
    pub fn with_pack_in(
        db: Database<F>,
        vfs: Arc<dyn Vfs<File = F>>,
        pack: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        let store = {
            let proof = db.writer_proof();
            PackStore::open_writer_in(vfs, pack, &proof)?
        };
        Ok(Self::with_writer_pack(db, store))
    }

    /// `db`, just created at `container` on `vfs`, with its pack at `pack` ([`Self::finish_create`]), calling `observe`
    /// while `db` still holds its lock: once the pack failed to open, and once the container is removed. Tests race
    /// other opens there, and crash a simulated file system after it.
    fn finish_create_observed(
        db: Database<F>,
        vfs: &Arc<dyn Vfs<File = F>>,
        pack: &Path,
        container: &Path,
        mut observe: impl FnMut(CreateStage),
    ) -> Result<Self, Error> {
        // `db` is borrowed by the proof only: a failure leaves it here, its lock still held.
        let opened = {
            let proof = db.writer_proof();
            PackStore::open_writer_in(Arc::clone(vfs), pack, &proof)
        };
        match opened {
            Ok(store) => Ok(Self::with_writer_pack(db, store)),
            Err(error) => {
                observe(CreateStage::PackFailed);
                let error = remove_created_container(&**vfs, container, error.into());
                observe(CreateStage::Removed);
                drop(db);
                Err(error)
            }
        }
    }

    /// `db` with `store`, a pack opened with `db`'s writer proof.
    fn with_writer_pack(db: Database<F>, store: PackStore<F>) -> Self {
        let db_uuid = db.writer_proof().db_uuid();
        let mut opened = Self::new(db, Blobs::new(Arc::new(store)));
        opened.pack_db_uuid = Some(db_uuid);
        opened
    }
}

/// A database opened read-only ([`ReadOnlyDatabase`], SPEC §8.4) with its blob pack opened read-only: it never
/// writes, and never creates the pack (SPEC §9.3, ROADMAP 9.4.5). With no pack file, a `get_blob` fails as not bound
/// ([`persia_blob::Error::BlobNotFound`]); [`ReadOnlyBlobDatabase::refresh`] finds a pack the writer has created since.
///
/// ```
/// use persia::internal::blob::PutBlobOptions;
/// use persia::internal::documents::{Blob, BlobDatabase, ReadOnlyBlobDatabase};
/// use persia_engine::db::{DatabaseOptions, ReadOnlyDatabase};
/// use persia_engine::document::Document;
/// use persia_engine::schema::SchemaBuilder;
/// use persia_engine::wal::Durability;
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// let path = dir.path().join("app.persia");
/// let options = DatabaseOptions::default();
/// let mut writer = BlobDatabase::create(&path, DatabaseOptions::default())?;
/// writer.log_mut().create_collection(SchemaBuilder::new("files").build()?)?;
/// let mut reader = ReadOnlyBlobDatabase::open(&path, &options)?; // no pack yet: none is created
///
/// let blob = writer.put("files", &Document::new("a"), Blob::from_reader(&b"hi"[..]), Durability::Fsync)?;
/// assert!(reader.get_blob_range(&blob, 0..2).is_err()); // the pack this reader opened had none
/// // A newer read-only open of the documents, then the pack refreshed: every document it shows finds its blob.
/// reader.refresh(ReadOnlyDatabase::open(&path, &options.registry)?)?;
/// assert_eq!(reader.get_blob_range(&blob, 0..2)?, b"hi");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct ReadOnlyBlobDatabase<F: ContainerFile = File> {
    /// Declared first, so the pack drops before the container (as in [`BlobDatabase`]).
    blobs: Blobs<PackStore<F>>,
    db: ReadOnlyDatabase<F>,
    /// Declared last, so it drops after the documents and the pack (ADR-0054 item 4).
    lease: ReaderLease<F>,
}

impl ReadOnlyBlobDatabase<File> {
    /// Opens the database in the container file at `path` read-only ([`ReadOnlyDatabase::open`], analyzing replayed
    /// `text` fields with `options.registry`) and its pack read-only, beside the container's canonical path (as
    /// [`BlobDatabase::open`] finds it). A reader lease is taken first, beside the pack ([`ReaderLease::take_beside`],
    /// SPEC §9.3), so a GC never sweeps a blob these documents show while the handle lives; where the directory
    /// refuses writes the handle opens without one ([`ReadOnlyBlobDatabase::lease`] is `None`, a warning is logged).
    ///
    /// # Errors
    /// [`Error::Database`] when the container cannot be opened (a file that is not one, a damaged one: corruption, as
    /// [`ReadOnlyDatabase::open`] reports it), also while the lease is taken; [`Error::Blob`] when the reader lease
    /// cannot be taken ([`persia_blob::Error::LeaseContended`], an I/O error); then as [`ReadOnlyDatabase::open`] and
    /// [`ReadOnlyBlobDatabase::with_lease`].
    pub fn open(path: impl AsRef<Path>, options: &DatabaseOptions) -> Result<Self, Error> {
        let path = path.as_ref();
        let lease = ReaderLease::take_beside(path).map_err(lease_error)?;
        Self::with_lease(
            lease,
            ReadOnlyDatabase::open(path, &options.registry)?,
            path,
        )
    }

    /// [`ReadOnlyBlobDatabase::open`] with the documents opened by [`ReadOnlyDatabase::open_with_options`]: `options`
    /// choose the registry, the block cache and whether the container is mapped (SPEC §4.7, §10.6). The embedded API's
    /// read-only `Db` opens with this. The pack follows the container's choice ([`ReadOnlyOptions::mapped_reads_choice`],
    /// ROADMAP 10.1.23): with [`MappedReads::IfLocal`] it is opened with [`PackStore::open_mapped_if_local`], and it
    /// keeps that mapping across the reopen of a refresh that finds it rewritten; otherwise it reads with `pread`.
    ///
    /// # Errors
    /// As [`ReadOnlyBlobDatabase::open`].
    pub fn open_with_options(
        path: impl AsRef<Path>,
        options: &ReadOnlyOptions,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        let lease = ReaderLease::take_beside(path).map_err(lease_error)?;
        let db = ReadOnlyDatabase::open_with_options(path, options)?;
        let pack = pack_beside(db.container().path(), path);
        let mapped = matches!(options.mapped_reads_choice(), MappedReads::IfLocal);
        Self::leased(lease, db, &pack, |pack, db_uuid| {
            if mapped {
                PackStore::open_mapped_if_local(pack, db_uuid)
            } else {
                PackStore::open(pack, db_uuid)
            }
        })
    }

    /// `db`, opened read-only from the container at `path`, with a reader lease taken now and its pack opened
    /// read-only beside the container's canonical path: [`ReaderLease::take_beside`], then
    /// [`ReadOnlyBlobDatabase::with_lease`]. The lease comes after the documents, so this fails with
    /// [`Error::OpenedBeforeLease`] when a writer committed in between; take the lease first and use `with_lease`
    /// to leave no window.
    ///
    /// # Errors
    /// As [`ReaderLease::take_beside`] and [`ReadOnlyBlobDatabase::with_lease`].
    pub fn with_pack_beside(db: ReadOnlyDatabase, path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let lease =
            ReaderLease::take_beside(db.container().path().unwrap_or(path)).map_err(lease_error)?;
        Self::with_lease(lease, db, path)
    }

    /// `db`, opened read-only from the container at `path` after `lease` was taken ([`ReaderLease::take_beside`]),
    /// with its pack opened read-only beside the container's canonical path. The CLI's `verify` takes the lease, opens
    /// and verifies the documents ([`ReadOnlyDatabase::verify`]), then calls this, so a document error is reported
    /// before the pack is opened. The pack open bounds each frame's length by its kind before reading it, as the scan
    /// does (ROADMAP 9.2.14).
    ///
    /// # Errors
    /// [`Error::OpenedBeforeLease`] when `db` is older than the lease's taken seq; [`Error::LeaseElsewhere`] when the
    /// lease is not beside this pack; then as [`PackStore::open`].
    pub fn with_lease(
        lease: ReaderLease,
        db: ReadOnlyDatabase,
        path: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        let pack = pack_beside(db.container().path(), path.as_ref());
        Self::leased(lease, db, &pack, |pack, db_uuid| {
            PackStore::open(pack, db_uuid)
        })
    }
}

impl<F: ContainerFile + Sync + 'static> ReadOnlyBlobDatabase<F> {
    /// `db` with the pack at `pack` on `vfs`, opened read-only with the `db_uuid` of `db`'s container
    /// ([`PackStore::open_in`]), after a reader lease taken now in `<pack>.leases` ([`ReaderLease::take_in`], which
    /// reads the committed seq of `db`'s container again at its path, through `vfs`: the documents and the pack must
    /// be on that one file system).
    ///
    /// # Errors
    /// [`Error::NoContainerPath`] for documents of a container opened without a path; then as [`ReaderLease::take_in`]
    /// (a container error as [`Error::Database`]) and [`ReadOnlyBlobDatabase::with_lease_in`].
    pub fn with_pack_in(
        db: ReadOnlyDatabase<F>,
        vfs: Arc<dyn Vfs<File = F>>,
        pack: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        let container = db.container();
        let path = container.path().ok_or(Error::NoContainerPath)?;
        let lease = ReaderLease::take_in(&vfs, pack.as_ref(), path).map_err(lease_error)?;
        Self::with_lease_in(lease, db, vfs, pack)
    }

    /// [`ReadOnlyBlobDatabase::with_lease`] with the pack at `pack` on `vfs`.
    ///
    /// # Errors
    /// [`Error::OpenedBeforeLease`], [`Error::LeaseElsewhere`] (the lease is not in `<pack>.leases`), then as
    /// [`PackStore::open_in`].
    pub fn with_lease_in(
        lease: ReaderLease<F>,
        db: ReadOnlyDatabase<F>,
        vfs: Arc<dyn Vfs<File = F>>,
        pack: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        Self::leased(lease, db, pack.as_ref(), |pack, db_uuid| {
            PackStore::open_in(vfs, pack, db_uuid)
        })
    }

    /// The checks of [`ReadOnlyBlobDatabase::with_lease`] (ADR-0054 item 4), then the pack opened by `open`, then
    /// the record says which documents the handle holds.
    fn leased(
        mut lease: ReaderLease<F>,
        db: ReadOnlyDatabase<F>,
        pack: &Path,
        open: impl FnOnce(&Path, [u8; 16]) -> persia_blob::Result<PackStore<F>>,
    ) -> Result<Self, Error> {
        // Equal is safe: every GC before the lease had a mark seq at most the taken seq, and one whose mark is not
        // newer than a reader's documents sweeps no blob they show.
        if db.seq() < lease.taken_seq() {
            return Err(Error::OpenedBeforeLease {
                opened: db.seq(),
                taken: lease.taken_seq(),
            });
        }
        let expected = leases_dir(pack);
        if lease.dir() != expected {
            return Err(Error::LeaseElsewhere {
                lease: lease.dir().to_path_buf(),
                expected,
            });
        }
        let db_uuid = db.container().header().db_uuid;
        let store = open(pack, db_uuid)?;
        lease.set_open(db_uuid, db.seq());
        Ok(Self {
            blobs: Blobs::new(Arc::new(store)),
            db,
            lease,
        })
    }

    /// The reader lease this handle holds (SPEC §9.3); `None` when the directory refused writes and the handle opened
    /// without one: then GC in another process may sweep a blob a document it shows references, which reads as
    /// [`persia_blob::Error::BlobNotFound`], never as other bytes.
    pub fn lease(&self) -> Option<&ReaderLease<F>> {
        self.lease.is_held().then_some(&self.lease)
    }

    /// The read-only database. Refresh through [`ReadOnlyBlobDatabase::refresh`], not its
    /// [`ReadOnlyDatabase::refresh`]: that one advances the documents without the pack and the reader lease, so a blob
    /// bound since reads as not found (ADR-0060).
    pub fn database(&self) -> &ReadOnlyDatabase<F> {
        &self.db
    }

    /// The read-only pack.
    pub fn blobs(&self) -> &Blobs<PackStore<F>> {
        &self.blobs
    }

    /// Catches up with the writer: `reopened`, a newer read-only open of the same database (taken here instead of
    /// [`ReadOnlyDatabase::refresh`], which would advance the documents before this pairing, ADR-0060), then the
    /// pack is refreshed ([`PackStore::refresh`]), which opens a pack the writer has created since (SPEC §9.3).
    /// The documents change only together with that refresh: the writer binds a blob before it logs a document that
    /// references it (SPEC §9.5), so every document
    /// `reopened` shows was logged after its blob was bound, and the refresh finds the blob.
    ///
    /// `reopened` must be this database, opened again: the same `db_uuid`, at the same canonical path (a copy
    /// elsewhere has the `db_uuid` but not this pack), and not older than the current open. Its manifest `seq` is
    /// compared first; within one `seq` the WAL only grows, so the replayed WAL records
    /// ([`ReadOnlyDatabase::replayed`]) and the WAL end ([`ReadOnlyDatabase::wal_end`]) must move the same way:
    ///
    /// | seq | replayed records, WAL end | the reopened open is |
    /// |---|---|---|
    /// | higher | any | newer: taken |
    /// | the same | both more, or both equal | newer, or the same state: taken |
    /// | the same | both fewer | older: [`Error::RefreshedOlder`] |
    /// | the same | they disagree | another history: [`Error::RefreshDiverged`] |
    /// | lower | any | older: [`Error::RefreshedOlder`] |
    ///
    /// The WAL end, not the container's end, because a commit in progress appends segment frames that are not WAL.
    /// The check is best-effort: another history with a higher `seq`, or with the same `seq`, records and WAL end but
    /// other records, is taken (SPEC §9.3). Do not rely on [`Error::RefreshDiverged`] to catch a copy or a restore.
    ///
    /// Returns whether the pack found anything new: `true` when it opened a pack that did not exist before, read
    /// frames committed since the last open or refresh, or reopened a pack a rewrite replaced; `false` when the pack
    /// is as it was. The return value says nothing of the documents: an open in the same state as the current one is
    /// taken (the documents are replaced by the same state) and returns `false` when the pack is unchanged too. The
    /// documents are replaced only when the pack refresh succeeds, or when a replaced pack is reopened.
    ///
    /// The handle's reader lease (SPEC §9.3, ROADMAP 9.2.16) keeps naming the documents it held until this succeeds,
    /// then names `reopened`'s: no GC sweeps a blob the documents of either show meanwhile. A handle without a
    /// lease ([`ReadOnlyBlobDatabase::lease`] is `None`), and a snapshot taken from [`ReadOnlyBlobDatabase::database`]
    /// and kept across this refresh, are not protected: a blob GC swept after a delete reads as
    /// [`persia_blob::Error::BlobNotFound`], never as other bytes.
    ///
    /// # Errors
    /// Nothing changes on these: [`persia_blob::Error::PackOfAnotherDatabase`] if `reopened` is another database,
    /// [`Error::RefreshedElsewhere`] if it was opened at another path, [`Error::RefreshedOlder`] if it is older,
    /// [`Error::RefreshDiverged`] if it holds another history.
    /// Then as [`PackStore::refresh`]. [`persia_blob::Error::PackReplaced`] (a GC rewrite, SPEC §18.10) reopens the
    /// pack read-only at the same path with the same `db_uuid` ([`PackStore::reopen`]): this method holds `&mut self`,
    /// so no stream still borrows the old pack. Any other pack error, and a reopen that itself fails (a damaged
    /// replacement, or the pack of another database: [`persia_blob::Error::PackOfAnotherDatabase`]), restores the
    /// previous documents and keeps the pack handle this reader had, so the documents and the pack stay paired.
    /// [`PackStore::refresh`] itself does not reopen (ROADMAP 9.2.13).
    pub fn refresh(&mut self, reopened: ReadOnlyDatabase<F>) -> Result<bool, Error> {
        let found = reopened.container().header().db_uuid;
        if found != self.db.container().header().db_uuid {
            return Err(persia_blob::Error::PackOfAnotherDatabase { found }.into());
        }
        let (current, other) = (self.db.container(), reopened.container());
        let (current, other) = (current.path(), other.path());
        if current != other {
            return Err(Error::RefreshedElsewhere {
                current: current.map(Path::to_path_buf),
                reopened: other.map(Path::to_path_buf),
            });
        }
        self.check_not_older(&reopened)?;
        let previous = std::mem::replace(&mut self.db, reopened);
        let found = match self.blobs.store().refresh() {
            Ok(found) => found,
            Err(persia_blob::Error::PackReplaced { .. }) => match self.blobs.store().reopen() {
                Ok(store) => {
                    // `&mut self`: no blob stream still borrows the old pack. A fresh handle drops that file.
                    self.blobs = Blobs::new(Arc::new(store));
                    true
                }
                Err(error) => {
                    self.db = previous;
                    return Err(error.into());
                }
            },
            Err(error) => {
                self.db = previous;
                return Err(error.into());
            }
        };
        // Only now: the previous documents are dropped, so the older seq no longer protects anything (ADR-0054 item 4).
        drop(previous);
        let db_uuid = self.db.container().header().db_uuid;
        self.lease.set_open(db_uuid, self.db.seq());
        Ok(found)
    }

    /// [`ReadOnlyBlobDatabase::refresh`]'s order check (the table there).
    fn check_not_older(&self, reopened: &ReadOnlyDatabase<F>) -> Result<(), Error> {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let current = &self.db;
        let (current_seq, reopened_seq) = (current.seq(), reopened.seq());
        let (current_replayed, reopened_replayed) = (current.replayed(), reopened.replayed());
        let (current_end, reopened_end) = (current.wal_end(), reopened.wal_end());
        let older = match reopened_seq.cmp(&current_seq) {
            Greater => return Ok(()),
            Less => true,
            Equal => match (
                reopened_replayed.cmp(&current_replayed),
                reopened_end.cmp(&current_end),
            ) {
                (Greater, Greater) | (Equal, Equal) => return Ok(()),
                (Less, Less) => true,
                _ => false,
            },
        };
        Err(if older {
            Error::RefreshedOlder {
                current: current_seq,
                reopened: reopened_seq,
                current_replayed,
                reopened_replayed,
                current_end,
                reopened_end,
            }
        } else {
            Error::RefreshDiverged {
                current: current_seq,
                reopened: reopened_seq,
                current_replayed,
                reopened_replayed,
                current_end,
                reopened_end,
            }
        })
    }

    /// A reader of the blob `blob_ref` refers to: as [`Blobs::get_blob`].
    ///
    /// # Errors
    /// As [`Blobs::get_blob`]: [`persia_blob::Error::BlobNotFound`] when the pack has not bound it (or there is no
    /// pack).
    pub fn get_blob(&self, blob_ref: &BlobRef) -> Result<BlobReader<'_, PackStore<F>>, Error> {
        Ok(self.blobs.get_blob(blob_ref)?)
    }

    /// Bytes `range` of the blob `blob_ref` refers to: as [`Blobs::get_blob_range`].
    ///
    /// # Errors
    /// As [`Blobs::get_blob_range`].
    pub fn get_blob_range(
        &self,
        blob_ref: &BlobRef,
        range: core::ops::Range<u64>,
    ) -> Result<Vec<u8>, Error> {
        Ok(self.blobs.get_blob_range(blob_ref, range)?)
    }
}

impl Error {
    /// Whether this reports corrupt stored bytes: a damaged pack or database ([`persia_blob::Error::is_corruption`],
    /// [`DatabaseError::is_corruption`]). What `persia verify` exits with its corruption status for.
    pub fn is_corruption(&self) -> bool {
        match self {
            Self::Blob(e) => e.is_corruption(),
            Self::Database(e) => e.is_corruption(),
            Self::Open { .. }
            | Self::BlobNotStored { .. }
            | Self::PackExists { .. }
            | Self::RefreshedElsewhere { .. }
            | Self::RefreshedOlder { .. }
            | Self::RefreshDiverged { .. }
            | Self::OpenedBeforeLease { .. }
            | Self::LeaseElsewhere { .. }
            | Self::NoContainerPath => false,
        }
    }
}

impl<S: BlobStore + ?Sized, L: DocumentLog> BlobDatabase<S, L> {
    /// Logs `doc` of `collection` at `durability`: with [`BlobDatabase::put`] of [`Blob::Stored`] when it carries a
    /// blob reference (pinned and checked against the store first, SPEC §9.6), else as it is. What the embedded API's
    /// `put` does: a reference set by hand is held to the same rule as one `put_blob` returned (ADR-0063 item 5).
    ///
    /// # Errors
    /// [`Error::BlobNotStored`] for a reference the store lacks (nothing is logged); then as [`BlobDatabase::put`].
    pub fn put_document(
        &mut self,
        collection: &str,
        doc: &Document,
        durability: Durability,
    ) -> Result<(), Error> {
        if let Some(blob_ref) = doc.blob_ref() {
            let stored = Blob::Stored(blob_ref.clone());
            return self.put(collection, doc, stored, durability).map(drop);
        }
        self.release_durable();
        let logged = self.log.put(collection, doc, durability);
        self.release_durable();
        Ok(logged?)
    }
}

impl<S: BlobStore + ?Sized, F: ContainerFile + Sync> BlobDatabase<S, Database<F>> {
    /// [`Database::put_batch`] of `docs`, after pinning every blob reference they carry and checking the store holds
    /// it (SPEC §9.6): a reference the store lacks, or one it holds with another size or chunk size, rejects the whole
    /// batch before anything is logged. The pins are held until the batch is durable, as [`BlobDatabase::put`] holds
    /// its document's.
    ///
    /// # Errors
    /// [`Error::BlobNotStored`], or [`Error::Blob`] of [`persia_blob::Error::BlobRefMismatch`] (ROADMAP 10.1.33): nothing
    /// is logged; then as [`Database::put_batch`].
    pub fn put_batch(
        &mut self,
        collection: &str,
        docs: &[Document],
        durability: Durability,
    ) -> Result<(), Error> {
        self.release_durable();
        let mut pins = Vec::new();
        for blob_ref in docs.iter().filter_map(Document::blob_ref) {
            let Some(pinned) = self.blobs.pin_contained(blob_ref)? else {
                return Err(Error::BlobNotStored {
                    blob_id: blob_ref.blob_id(),
                });
            };
            pins.push(pinned);
        }
        let before = self.log.wal_mark();
        let logged = self.log.put_batch(collection, docs, durability);
        let mark = self.log.wal_mark();
        // As in `put`: a record appended may be on disk, so its blob stays pinned until the mark is durable.
        if mark > before {
            self.held.extend(pins.into_iter().map(|p| (mark, p)));
        }
        self.release_durable();
        Ok(logged?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use persia_engine::db::DatabaseOptions;
    use persia_storage::container;

    use super::*;

    /// A pack that fails to open after `Database::create` is not [`Error::PackExists`]: the container just created
    /// is removed, and a retry creates the database.
    #[test]
    fn a_pack_open_failure_after_create_removes_the_container() {
        if cfg!(miri) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let db = Database::create(&path, DatabaseOptions::default()).unwrap();
        let pack = pack_path(&path);
        // A directory is not a pack. `open` fails with an I/O error, not [`Error::PackExists`] (that check runs
        // before anything is created, and only when the path already exists).
        std::fs::create_dir(&pack).unwrap();
        let err = BlobDatabase::finish_create(db, &pack, &path).unwrap_err();
        assert!(
            matches!(err, Error::Blob(persia_blob::Error::PackIo { .. })),
            "{err:?}"
        );
        assert!(
            !path.exists(),
            "the container just created is removed so a retry can create it"
        );
        std::fs::remove_dir(&pack).unwrap();
        BlobDatabase::create(&path, DatabaseOptions::default()).unwrap();
    }

    /// The operating system's file system, as `finish_create_observed` takes it.
    fn real() -> Arc<dyn Vfs<File = File>> {
        Arc::new(RealFs)
    }

    /// A database just created at `path`, with a directory where its pack goes, so the pack open of `finish_create`
    /// fails with an I/O error.
    fn created_with_a_failing_pack(path: &Path) -> (Database, PathBuf) {
        let db = Database::create(path, DatabaseOptions::default()).unwrap();
        let pack = pack_path(path);
        std::fs::create_dir(&pack).unwrap();
        (db, pack)
    }

    /// The file the writer lock of the container at `path` is taken on: the container itself (Unix), or the lock
    /// file beside it (Windows, SPEC §4.5.1).
    fn writer_lock_file(path: &Path) -> File {
        let lock = if cfg!(windows) {
            let mut name = path.file_name().unwrap().to_owned();
            name.push(".lock");
            path.with_file_name(name)
        } else {
            path.to_path_buf()
        };
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock)
            .unwrap()
    }

    /// ROADMAP 10.2.7: the container is removed while the creating handle still holds the writer lock. An open that
    /// races in once the pack failed finds the lock taken (no handle on a file about to be removed), and once the
    /// file is removed it finds nothing to open: never the removed file, never a database without its pack.
    #[test]
    fn the_failed_create_removes_its_container_while_it_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let (db, pack) = created_with_a_failing_pack(&path);
        let mut stages = Vec::new();
        // A handle on the file the lock is on, taken while the container is still there: its lock being taken
        // tells that the creator still holds it, also once the container's name is gone.
        let mut lock_probe = None;
        let err = BlobDatabase::finish_create_observed(db, &real(), &pack, &path, |stage| {
            stages.push(stage);
            let probe = lock_probe.get_or_insert_with(|| writer_lock_file(&path));
            assert!(
                matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
                "the creator holds the writer lock at {stage:?}"
            );
            let raced = BlobDatabase::open(&path, DatabaseOptions::default());
            match stage {
                CreateStage::PackFailed => {
                    assert!(path.exists(), "removed only after the pack failed");
                    let Err(Error::Database(e)) = raced else {
                        panic!("an open while the creator holds the lock must fail: {raced:?}");
                    };
                    assert!(
                        matches!(*e, DatabaseError::Container(container::Error::Locked)),
                        "{e:?}"
                    );
                }
                CreateStage::Removed => {
                    assert!(!path.exists(), "removed before the lock is released");
                    let Err(Error::Database(e)) = raced else {
                        panic!("nothing is left to open: {raced:?}");
                    };
                    // The open stops at its path lookup (`canonicalize`), before any lock: the name is gone (`NotFound`),
                    // or on Windows the file waits to be deleted while the creator still has it open, and the lookup is
                    // refused (`PermissionDenied`).
                    let refused = matches!(
                        &*e,
                        DatabaseError::Container(container::Error::Io { source, .. })
                            if matches!(
                                source.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                            )
                    );
                    assert!(refused, "{e:?}");
                    // Unix: the name is free while the lock is still held (it is on the removed file), so a create
                    // makes a new database there, which the failed create must leave alone.
                    #[cfg(unix)]
                    drop(Database::create(&path, DatabaseOptions::default()).unwrap());
                }
            }
        })
        .unwrap_err();
        assert!(
            matches!(err, Error::Blob(persia_blob::Error::PackIo { .. })),
            "{err:?}"
        );
        assert_eq!(stages, [CreateStage::PackFailed, CreateStage::Removed]);
        #[cfg(unix)]
        drop(Database::open(&path, DatabaseOptions::default()).unwrap());
        #[cfg(not(unix))]
        assert!(!path.exists());
    }

    /// Sets its flag when dropped, also while a panic unwinds.
    struct StopOnDrop<'a>(&'a AtomicBool);

    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// ROADMAP 10.2.7, SPEC §9.3: the removal of a failed create is durable. A crash right after it (every unsynced
    /// change lost) leaves no container for a retry to find, and the retry creates the database.
    #[test]
    fn a_failed_creates_removal_survives_a_crash() {
        use persia_storage::container::Container;
        use persia_storage::container::sim::CrashLoss;
        use persia_storage::vfs::sim::{SimFs, SimFsFile};

        let fs = SimFs::new();
        fs.create_dir(Path::new("/db")).unwrap();
        fs.sync_dir(Path::new("/")).unwrap();
        let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
        let path = Path::new("/db/app.persia");
        let db = Database::create_container(
            Container::create_in(Arc::clone(&vfs), path).unwrap(),
            DatabaseOptions::default(),
        )
        .unwrap();
        // The container is durable now (create syncs its directory): only the removal's own sync can make it go.
        assert!(fs.crash(CrashLoss::All).read(path).is_some());
        let pack = pack_path(path);
        fs.create_dir(&pack).unwrap(); // a directory is not a pack: the open fails
        let err = BlobDatabase::finish_create_observed(db, &vfs, &pack, path, |_| {}).unwrap_err();
        assert!(
            matches!(err, Error::Blob(persia_blob::Error::PackIo { .. })),
            "{err:?}"
        );
        let after = fs.crash(CrashLoss::All);
        assert!(after.read(path).is_none(), "the removal was not synced");
        let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(after);
        Container::create_in(vfs, path).unwrap();
    }

    /// A sync of the directory that fails after the removal is reported with the pack error, never swallowed.
    #[test]
    fn a_removal_whose_directory_sync_fails_is_reported() {
        use persia_storage::vfs::sim::SimFs;

        let fs = SimFs::new();
        fs.create_dir(Path::new("/db")).unwrap();
        let pack_error = || Error::PackExists {
            path: PathBuf::from("/db/pack"),
        };
        // The directory of `/gone/app.persia` does not exist: the removal finds nothing, and the sync fails.
        let failed = remove_created_container(&fs, Path::new("/gone/app.persia"), pack_error());
        let Error::Blob(persia_blob::Error::PackIo { op, .. }) = &failed else {
            panic!("{failed:?}");
        };
        assert_eq!(
            *op,
            "sync the directory of the database removed after its pack failed to open"
        );
        let gone = remove_created_container(&fs, Path::new("/db/app.persia"), pack_error());
        assert!(matches!(gone, Error::PackExists { .. }), "{gone:?}");
    }

    /// A removal that finds the container gone already reports the pack error alone; one that fails otherwise
    /// reports both, as an I/O error of the pack (a retry would not work).
    #[test]
    fn a_removal_that_fails_is_reported_with_the_pack_error() {
        let dir = tempfile::tempdir().unwrap();
        let pack_error = || Error::PackExists {
            path: dir.path().join("pack"),
        };
        let gone = remove_created_container(&RealFs, &dir.path().join("gone.persia"), pack_error());
        assert!(matches!(gone, Error::PackExists { .. }), "{gone:?}");
        // A directory cannot be removed as a file.
        let stuck = dir.path().join("stuck.persia");
        std::fs::create_dir(&stuck).unwrap();
        let failed = remove_created_container(&RealFs, &stuck, pack_error());
        let Error::Blob(persia_blob::Error::PackIo { op, source }) = &failed else {
            panic!("{failed:?}");
        };
        assert_eq!(
            *op,
            "remove the database created before its pack failed to open"
        );
        assert_ne!(source.kind(), io::ErrorKind::NotFound);
        assert!(stuck.exists());
    }

    /// ROADMAP 10.2.7, raced on real threads: an open of the path in a loop, while creates whose pack fails come and
    /// go, never opens one of their containers (each is removed before its lock is released).
    #[test]
    fn an_open_racing_failed_creates_never_gets_their_container() {
        const CREATES: usize = 16;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let racer = scope.spawn(|| {
                let mut attempts = 0_u64;
                while !done.load(Ordering::Acquire) {
                    attempts += 1;
                    if BlobDatabase::open(&path, DatabaseOptions::default()).is_ok() {
                        return Err(attempts);
                    }
                }
                Ok(attempts)
            });
            // Stops the racer however this thread leaves the loop: a failed assertion fails the test, never hangs it.
            let stop = StopOnDrop(&done);
            for _ in 0..CREATES {
                let (db, pack) = created_with_a_failing_pack(&path);
                let err = BlobDatabase::finish_create(db, &pack, &path).unwrap_err();
                assert!(
                    matches!(err, Error::Blob(persia_blob::Error::PackIo { .. })),
                    "{err:?}"
                );
                std::fs::remove_dir(&pack).unwrap();
            }
            drop(stop);
            let outcome = racer.join().unwrap();
            assert!(
                outcome.is_ok(),
                "an open got a container being removed, at attempt {outcome:?}"
            );
        });
        assert!(!path.exists());
    }
}
