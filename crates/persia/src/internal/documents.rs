//! Documents that reference blobs: `put(doc, blob)` (SPEC §9.5, §9.6; ROADMAP 9.4.1, ADR-0051).
//!
//! [`BlobDatabase::put`] stores the blob first and logs the document second: the blob is durable when
//! [`Blobs::put_blob`] returns (the store's `bind` answered), so no WAL byte ever names a blob that is not, whatever
//! the document's [`Durability`]. A [`Blob::Stored`] reference is pinned, then checked against the store, before the
//! document is logged; one the store lacks is an error and logs nothing. The blob's pins are kept until the WAL
//! record that references it is durable ([`DocumentLog::durable_mark`] reaches its [`DocumentLog::wal_mark`]): at
//! once for `Fsync`, at the next commit or `Fsync` operation otherwise (SPEC §9.5).

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use persia_blob::{BlobId, BlobRef, BlobStore};
use persia_engine::db::{Database, DatabaseError};
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

impl DocumentLog for Database {
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
    /// Replacing the log through it (e.g. with the database reopened) restarts its marks at 0: the pins still held
    /// wait for the new log's `durable_mark` to reach marks of the old one, so they are held longer, never released
    /// early (safe; the reopened database's WAL records are on disk and GC's mark finds them). Its `put` takes a
    /// document with any blob reference: M10 must not expose it (SPEC §9.6).
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
// blob/wiring: `BlobDatabase` opened over a `Database` and its pack (`<name>.persia.blobs`, the container's
// `db_uuid`, the writer proof) goes here, and only here.
//
// -- end wiring --
