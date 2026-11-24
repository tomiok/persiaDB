//! The database handle (SPEC §8.1, §8.4; ROADMAP 10.1.1).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use persia_blob::PackStore;
use persia_engine::db::DatabaseReader;
use persia_engine::wal::Durability;

use crate::internal::blob::Blobs;
use crate::internal::documents::{BlobDatabase, ReadOnlyBlobDatabase};
use crate::options::OpenOptions;
use crate::{Error, ErrorCode, Result};

/// The writer: the engine's database with its blob pack.
type Writer = BlobDatabase<PackStore>;

/// What reads of a read-write database use, so that they never take the writer's mutex.
#[allow(dead_code)] // Read by the collection handles (ROADMAP 10.1.2).
struct ReadSide {
    /// Loads the latest published snapshot without a lock (SPEC §8.4).
    reader: DatabaseReader,
    /// The blob store and the writer's pin set, shared with the writer.
    blobs: Blobs<PackStore>,
}

/// How the database is open.
enum Mode {
    /// The single writer of the file (SPEC §8.4).
    ReadWrite {
        /// The writer mutex: one write at a time. `None` once closed.
        writer: Mutex<Option<Box<Writer>>>,
        /// `None` once closed.
        reads: RwLock<Option<ReadSide>>,
    },
    /// A reader beside whoever writes the file. `None` once closed; a refresh takes the write lock.
    ReadOnly(RwLock<Option<Box<ReadOnlyBlobDatabase>>>),
}

/// The state every handle of one open database shares.
pub(crate) struct Inner {
    path: PathBuf,
    /// The durability of writes through handles that did not choose their own.
    #[allow(dead_code)] // Read by the collection handles (ROADMAP 10.1.2).
    durability: Durability,
    /// Set first by a close or a drop: later calls on any handle fail with [`ErrorCode::Closed`].
    closed: AtomicBool,
    mode: Mode,
}

/// An open database: one `.persia` file (and its blob pack beside it), opened in this process.
///
/// `Db::open(path)` just works: it creates the file when it is missing (SPEC §8.1) and needs no configuration
/// and no server. A `Db` is `Send + Sync`: share it by reference or in an `Arc`. It is not `Clone`, because
/// closing it is one event: [`Db::close`] consumes it.
///
/// A database has one writer at a time, across processes (SPEC §8.4): a second read-write open fails at once.
/// Other processes, and other handles in this one, open it read-only ([`OpenOptions::read_only`]).
///
/// ```
/// use persia::Db;
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// let path = dir.path().join("app.persia");
/// let db = Db::open(&path)?; // created: it did not exist
/// assert_eq!(db.path(), path);
/// db.close()?;
/// let db = Db::open(&path)?; // opened: it is never replaced
/// # drop(db);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Db {
    inner: Arc<Inner>,
}

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.inner.path)
            .field("read_only", &self.is_read_only())
            .field("closed", &self.inner.closed.load(Ordering::SeqCst))
            .finish()
    }
}

impl Db {
    /// Opens the database at `location`, creating it if no file is there: [`OpenOptions::new`]`().open(location)`.
    ///
    /// `location` is a filesystem path, or a URL. It is a URL only when it starts with `scheme://`, so
    /// `C:\data\app.persia` is a path. The only scheme this build opens is `file://`, with an empty host or
    /// `localhost` and a percent-encoded absolute path: `file:///var/lib/app.persia`.
    ///
    /// ```
    /// use persia::{Db, ErrorCode};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = Db::open(dir.path().join("app.persia"))?;
    /// db.close()?;
    ///
    /// // Cloud schemes are named by SPEC §10.2 and have no backend in this build.
    /// assert_eq!(Db::open("s3://bucket/app").unwrap_err().code(), ErrorCode::Unsupported);
    /// assert_eq!(Db::open("ftp://host/app").unwrap_err().code(), ErrorCode::InvalidArgument);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// - [`ErrorCode::InvalidArgument`]: an empty location, a malformed `file://` URL, an unknown scheme;
    /// - [`ErrorCode::Unsupported`]: `s3://`, `gs://`, `az://` or `memory://` (no backend in this build), or a
    ///   file written by a newer format;
    /// - [`ErrorCode::NotFound`]: no file at the path, and the open does not create one;
    /// - [`ErrorCode::Conflict`]: another read-write handle has the database open, in this process or another;
    /// - [`ErrorCode::Corrupt`]: the file is not a database, or is damaged;
    /// - [`ErrorCode::Io`]: the file system failed.
    pub fn open(location: impl AsRef<Path>) -> Result<Self> {
        OpenOptions::new().open(location)
    }

    /// Opens the database at the filesystem path `path` as `options` say.
    pub(crate) fn open_at(path: PathBuf, options: &OpenOptions) -> Result<Self> {
        let mode = if options.read_only {
            open_read_only(&path, options)?
        } else {
            open_read_write(&path, options)?
        };
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                durability: options.durability,
                closed: AtomicBool::new(false),
                mode,
            }),
        })
    }

    /// The path the database was opened at: the one given, or the one a `file://` URL named.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// let db = persia::Db::open(&path)?;
    /// assert_eq!(db.path(), path);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Whether the database was opened read-only ([`OpenOptions::read_only`]): its writes fail with
    /// [`ErrorCode::ReadOnly`].
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// assert!(!db.is_read_only());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        matches!(self.inner.mode, Mode::ReadOnly(_))
    }

    /// Closes the database: commits what is not committed yet, then releases the writer lock, so the next open
    /// succeeds at once. Returns the first error of that; the database is closed either way.
    ///
    /// Dropping a `Db` closes it too, but cannot report an error: call `close` to learn whether the last commit
    /// reached the disk.
    ///
    /// ```
    /// use persia::Db;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// let db = Db::open(&path)?;
    /// db.close()?;
    /// let again = Db::open(&path)?; // the writer lock is free
    /// # drop(again);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::Poisoned`] if an earlier write had failed (nothing more was committed); otherwise as the final
    /// commit fails: [`ErrorCode::Io`], [`ErrorCode::Corrupt`].
    pub fn close(self) -> Result<()> {
        self.inner.shut_down()
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Err(error) = self.inner.shut_down() {
            tracing::error!(path = %self.inner.path.display(), %error, "closing a dropped database failed");
        }
    }
}

impl Inner {
    /// Closes the database once; later calls do nothing.
    fn shut_down(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        match &self.mode {
            Mode::ReadOnly(db) => {
                let closed = db.write().unwrap_or_else(PoisonError::into_inner).take();
                drop(closed);
                Ok(())
            }
            Mode::ReadWrite { writer, reads } => {
                let (mut writer, panicked) = match writer.lock() {
                    Ok(guard) => (guard, false),
                    Err(poisoned) => (poisoned.into_inner(), true),
                };
                let result = match (writer.as_mut(), panicked) {
                    (None, _) => Ok(()),
                    // A write panicked half-way: what the memtables hold is unknown, so nothing more is committed.
                    (Some(_), true) => Err(Error::writer_panicked()),
                    (Some(db), false) => {
                        let committed = db.commit().map(drop).map_err(Error::wrap);
                        // Nothing a merge in flight did is committed after this (SPEC §8.5).
                        db.log_mut().cancel_merges();
                        committed
                    }
                };
                // The reads' share of the pack first, then the writer, whose pack drops before its container
                // (ADR-0053 item 2): the pack never outlives the container whose lock its proof stood for.
                let closed = reads.write().unwrap_or_else(PoisonError::into_inner).take();
                drop(closed);
                drop(writer.take());
                result
            }
        }
    }
}

/// Whether a file (or anything else) is at `path`.
fn exists(path: &Path) -> Result<bool> {
    path.try_exists().map_err(|source| {
        Error::with_source(
            ErrorCode::Io,
            format_args!("looking for {}", path.display()),
            source,
        )
    })
}

fn not_found(path: &Path) -> Error {
    Error::new(
        ErrorCode::NotFound,
        format!(
            "no database at {}, and this open does not create one",
            path.display()
        ),
    )
}

fn open_read_only(path: &Path, options: &OpenOptions) -> Result<Mode> {
    if !exists(path)? {
        return Err(not_found(path));
    }
    let db = ReadOnlyBlobDatabase::open(path, &options.database_options())
        .map_err(|e| Error::wrap(e).context(format_args!("opening {}", path.display())))?;
    Ok(Mode::ReadOnly(RwLock::new(Some(Box::new(db)))))
}

fn open_read_write(path: &Path, options: &OpenOptions) -> Result<Mode> {
    let opened = if exists(path)? {
        BlobDatabase::open(path, options.database_options())
    } else if options.create_if_missing {
        BlobDatabase::create(path, options.database_options())
    } else {
        return Err(not_found(path));
    };
    let db =
        opened.map_err(|e| Error::wrap(e).context(format_args!("opening {}", path.display())))?;
    let reads = ReadSide {
        reader: db.log().reader(),
        blobs: db.blobs().clone(),
    };
    Ok(Mode::ReadWrite {
        writer: Mutex::new(Some(Box::new(db))),
        reads: RwLock::new(Some(reads)),
    })
}
