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

use persia_blob::pack::{ContainerFile, Vfs};
use persia_blob::{BlobId, BlobRef, BlobStore, PackStore, pack_path};
use persia_engine::db::{Database, DatabaseError, DatabaseOptions, ReadOnlyDatabase};
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
    /// [`ReadOnlyBlobDatabase::refresh`] was given an open older than the current one: nothing changed.
    #[error("the reopened database is at seq {reopened}, older than the current seq {current}")]
    RefreshedOlder {
        /// The current manifest `seq`.
        current: u64,
        /// The reopened one's.
        reopened: u64,
    },
}

/// The blob of a [`BlobDatabase::put`] (SPEC §9.6).
pub enum Blob<'r> {
    /// Bytes to store, from a reader.
    Reader(Box<dyn Read + 'r>, PutBlobOptions),
    /// Bytes to store, from a file.
    Path(PathBuf, PutBlobOptions),
    /// A blob this database's `put_blob` stored before.
    Stored(BlobRef),
}

impl<'r> Blob<'r> {
    /// The bytes `reader` yields, with the default [`PutBlobOptions`].
    pub fn from_reader(reader: impl Read + 'r) -> Self {
        Self::Reader(Box::new(reader), PutBlobOptions::default())
    }

    /// The bytes of the file at `path`, with the default [`PutBlobOptions`].
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into(), PutBlobOptions::default())
    }

    /// `options` instead of the defaults; no effect on [`Blob::Stored`].
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
    blobs: Blobs<S>,
    log: L,
}

impl<S: BlobStore + ?Sized, L: DocumentLog> BlobDatabase<S, L> {
    /// `log`'s documents with `blobs`' blobs.
    pub fn new(log: L, blobs: Blobs<S>) -> Self {
        Self {
            log,
            held: VecDeque::new(),
            blobs,
        }
    }

    /// The document log.
    pub fn log(&self) -> &L {
        &self.log
    }

    /// The document log, for the operations this layer does not wrap (commit, delete, search). Call
    /// [`BlobDatabase::release_durable`] after one that may make records durable, or let the next `put` do it.
    ///
    /// Not for replacing the log: the blob pack was opened with this log's writer proof. To reopen the database, drop
    /// this value and open a new one (the pack's writer lock refuses a second writer while this pack lives, however it
    /// is kept alive, ROADMAP 9.2.7). Its `put` takes a document with any blob reference: M10 must not expose it
    /// (SPEC §9.6).
    pub fn log_mut(&mut self) -> &mut L {
        &mut self.log
    }

    /// The blob store with its pin set.
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

impl From<DatabaseError> for Error {
    fn from(e: DatabaseError) -> Self {
        Self::Database(Box::new(e))
    }
}

// -- wiring (9.4.5) --
// `BlobDatabase` opened over a `Database` and its pack (`<name>.persia.blobs`, the container's `db_uuid`, the writer
// proof), and its read-only counterpart (SPEC §9.3; ROADMAP 9.2.7, 9.4.5; ADR-0053).

/// The pack beside `container`, the canonical path a container was opened at (so every process that opens the
/// database, by any name, finds the same pack), or beside `given` for a container without one. The pack path is fixed
/// at open: the handle keeps it, and does not follow a container renamed or moved later.
fn pack_beside(container: Option<&Path>, given: &Path) -> PathBuf {
    pack_path(container.unwrap_or(given))
}

impl BlobDatabase<PackStore> {
    /// Creates a database in a new container file at `path` ([`Database::create`]) with its blob pack at
    /// [`pack_path`] of it, `<name>.persia.blobs`, opened read-write with the database's writer proof, so the pack
    /// carries the container's `db_uuid` (SPEC §9.3). The pack file is not created here: the first `put_blob` creates
    /// it, and a database that never stores a blob has none.
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
    /// [`Error::Blob`] as [`PackStore::open_writer`].
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
        let pack = pack_beside(db.container().path(), path);
        Self::with_pack(db, pack)
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
        let proof = db.writer_proof();
        let store = PackStore::open_writer(pack, &proof)?;
        Ok(Self::new(db, Blobs::new(Arc::new(store))))
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
        let proof = db.writer_proof();
        let store = PackStore::open_writer_in(vfs, pack, &proof)?;
        Ok(Self::new(db, Blobs::new(Arc::new(store))))
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
}

impl ReadOnlyBlobDatabase<File> {
    /// Opens the database in the container file at `path` read-only ([`ReadOnlyDatabase::open`], analyzing replayed
    /// `text` fields with `options.registry`) and its pack read-only, beside the container's canonical path (as
    /// [`BlobDatabase::open`] finds it).
    ///
    /// # Errors
    /// [`Error::Database`] as [`ReadOnlyDatabase::open`]; [`Error::Blob`] as [`PackStore::open`] (a damaged pack, or
    /// one of another database).
    pub fn open(path: impl AsRef<Path>, options: &DatabaseOptions) -> Result<Self, Error> {
        let path = path.as_ref();
        Self::with_pack_beside(ReadOnlyDatabase::open(path, &options.registry)?, path)
    }

    /// `db`, opened read-only from the container at `path`, with its pack opened read-only beside the container's
    /// canonical path, as [`ReadOnlyBlobDatabase::open`] finds it: for a caller that opens the documents itself (the
    /// CLI's `verify` reports their errors first).
    ///
    /// # Errors
    /// As [`PackStore::open`].
    pub fn with_pack_beside(db: ReadOnlyDatabase, path: impl AsRef<Path>) -> Result<Self, Error> {
        let pack = pack_beside(db.container().path(), path.as_ref());
        let store = PackStore::open(pack, db.container().header().db_uuid)?;
        Ok(Self {
            blobs: Blobs::new(Arc::new(store)),
            db,
        })
    }
}

impl<F: ContainerFile + Sync + 'static> ReadOnlyBlobDatabase<F> {
    /// `db` with the pack at `pack` on `vfs`, opened read-only with the `db_uuid` of `db`'s container
    /// ([`PackStore::open_in`]).
    ///
    /// # Errors
    /// As [`PackStore::open_in`].
    pub fn with_pack_in(
        db: ReadOnlyDatabase<F>,
        vfs: Arc<dyn Vfs<File = F>>,
        pack: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        let store = PackStore::open_in(vfs, pack, db.container().header().db_uuid)?;
        Ok(Self {
            blobs: Blobs::new(Arc::new(store)),
            db,
        })
    }

    /// The read-only database.
    pub fn database(&self) -> &ReadOnlyDatabase<F> {
        &self.db
    }

    /// The read-only pack.
    pub fn blobs(&self) -> &Blobs<PackStore<F>> {
        &self.blobs
    }

    /// Catches up with the writer: `reopened`, a newer read-only open of the same database (the engine's read-only
    /// open does not follow later commits, SPEC §8.4), replaces the documents, then the pack is refreshed
    /// ([`PackStore::refresh`]), which opens a pack the writer has created since (SPEC §9.3). In that order: the writer
    /// binds a blob before it logs a document that references it (SPEC §9.5), so every document `reopened` shows was
    /// logged after its blob was bound, and the refresh that follows finds the blob.
    ///
    /// `reopened` must be this database, opened again: the same `db_uuid`, at the same canonical path (a copy
    /// elsewhere has the `db_uuid` but not this pack), and at a manifest `seq` no older than the current one (an older
    /// open would show fewer documents than the pack's blobs were bound for, and could pair with an older pack).
    ///
    /// Returns whether the pack found anything new: `true` when it opened a pack that did not exist before or read
    /// frames committed since the last open or refresh, `false` when the pack is as it was. The documents are replaced
    /// either way.
    ///
    /// # Errors
    /// Nothing changes on these: [`persia_blob::Error::PackOfAnotherDatabase`] if `reopened` is another database,
    /// [`Error::RefreshedElsewhere`] if it was opened at another path, [`Error::RefreshedOlder`] if its `seq` is
    /// older. Then as [`PackStore::refresh`] (the documents are replaced already;
    /// [`persia_blob::Error::PackReplaced`] asks for a new open).
    pub fn refresh(&mut self, reopened: ReadOnlyDatabase<F>) -> Result<bool, Error> {
        let found = reopened.container().header().db_uuid;
        if found != self.db.container().header().db_uuid {
            return Err(persia_blob::Error::PackOfAnotherDatabase { found }.into());
        }
        let (current, other) = (self.db.container().path(), reopened.container().path());
        if current != other {
            return Err(Error::RefreshedElsewhere {
                current: current.map(Path::to_path_buf),
                reopened: other.map(Path::to_path_buf),
            });
        }
        if reopened.seq() < self.db.seq() {
            return Err(Error::RefreshedOlder {
                current: self.db.seq(),
                reopened: reopened.seq(),
            });
        }
        self.db = reopened;
        Ok(self.blobs.store().refresh()?)
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
            | Self::RefreshedOlder { .. } => false,
        }
    }
}
// -- end wiring --
