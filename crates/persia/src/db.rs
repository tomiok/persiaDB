//! The database handle (SPEC §8.1, §8.4; ROADMAP 10.1.1, 10.1.9, 10.2.3; ADR-0064).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use persia_blob::PackStore;
use persia_engine::db::{DatabaseReader, ReadOnlyDatabase, Snapshot, create_parent_dirs};
use persia_engine::wal::Durability;

use crate::error::Classified as _;
use crate::internal::blob::Blobs;
use crate::internal::documents::{BlobDatabase, Error as DocumentsError, ReadOnlyBlobDatabase};
use crate::maintenance::{Signals, Thread};
use crate::options::OpenOptions;
use crate::{Error, ErrorCode, Result};

/// The writer: the engine's database with its blob pack.
pub(crate) type Writer = BlobDatabase<PackStore>;

/// What reads of a read-write database use, so that they never take the writer's mutex.
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
        /// How writes and the close reach the maintenance thread (ADR-0064).
        signals: Signals,
        /// The maintenance thread; `None` before it starts and once the close took it.
        thread: Mutex<Option<Thread>>,
    },
    /// A reader beside whoever writes the file. `None` once closed; a refresh takes the write lock.
    ReadOnly(RwLock<Option<Box<ReadOnlyBlobDatabase>>>),
}

/// The state every handle of one open database shares.
pub(crate) struct Inner {
    path: PathBuf,
    /// The durability of writes through handles that did not choose their own.
    durability: Durability,
    /// The options of the open, for what a refresh opens again.
    options: OpenOptions,
    /// Set first by a close or a drop: later calls on any handle fail with [`ErrorCode::Closed`].
    closed: AtomicBool,
    /// One refresh at a time, from its reopen to its swap: two racing ones would otherwise swap in their reopens out of
    /// order, and the later swap would refuse the older one as `RefreshedOlder`. Reads never take it.
    refreshing: Mutex<()>,
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
/// Reads take no lock and never wait for a write: each one loads the latest published state of the database (SPEC
/// §8.4), so it sees every write that returned before it started. A read-only `Db` shows one state, the one its
/// open found, until [`Db::refresh`].
///
/// **Read-your-writes** (SPEC §8.4): on a read-write `Db`, a read that starts after a write returned sees it, from
/// any handle and any thread, at every [`Durability`], committed or not. A [`Collection::put_batch`](crate::Collection::put_batch)
/// that returns `Ok` is seen whole from then on. Durability decides what survives a crash, not what reads see.
///
/// **Background work** (ADR-0064): a read-write `Db` commits and merges on its own maintenance thread, as the open's
/// [`OpenOptions`] say (`idle_flush`, `commit_interval`, `memtable_bytes`, `merge_threads`). A program only opens,
/// writes and closes; [`Db::close`] joins that thread.
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
    pub(crate) inner: Arc<Inner>,
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
    /// The open is SQLite-like (SPEC §8.1): when no file is at the path it creates the missing parent directories,
    /// then the database; an existing database is opened, never replaced. Opens that race on a missing path agree:
    /// one creates the database, and each other one opens it or fails as a second writer.
    ///
    /// ```
    /// use persia::{Db, ErrorCode};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("data/2026/app.persia");
    /// let db = Db::open(&path)?; // creates data/ and data/2026/, then the database
    /// db.close()?;
    /// assert!(path.is_file());
    ///
    /// // Cloud schemes are named by SPEC §10.2 and have no backend in this build; nor does any other scheme.
    /// assert_eq!(Db::open("s3://bucket/app").unwrap_err().code(), ErrorCode::Unsupported);
    /// assert_eq!(Db::open("ftp://host/app").unwrap_err().code(), ErrorCode::Unsupported);
    /// // A malformed file:// URL is the caller's mistake.
    /// assert_eq!(Db::open("file://host/app").unwrap_err().code(), ErrorCode::InvalidArgument);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// - [`ErrorCode::InvalidArgument`]: an empty location, a malformed `file://` URL, a path under a file that is not
    ///   a directory;
    /// - [`ErrorCode::Unsupported`]: any scheme but `file://` (`s3://`, `gs://`, `az://` and `memory://` have no
    ///   backend in this build yet), or a file written by a newer format;
    /// - [`ErrorCode::NotFound`]: no file at the path, and the open does not create one;
    /// - [`ErrorCode::Conflict`]: another read-write handle has the database open, in this process or another;
    ///   or a blob pack (`<name>.persia.blobs`) is at the path of a database to create;
    /// - [`ErrorCode::Corrupt`]: the file is not a database, or is damaged;
    /// - [`ErrorCode::Io`]: the file system failed;
    /// - [`ErrorCode::Transient`]: the file kept appearing and vanishing while it was opened.
    pub fn open(location: impl AsRef<Path>) -> Result<Self> {
        OpenOptions::new().open(location)
    }

    /// Opens the database at the filesystem path `path` as `options` say, and starts its maintenance thread when it
    /// is read-write.
    pub(crate) fn open_at(path: PathBuf, options: &OpenOptions) -> Result<Self> {
        let mode = if options.read_only {
            open_read_only(&path, options)?
        } else {
            open_read_write(&path, options)?
        };
        let db = Self {
            inner: Arc::new(Inner {
                path,
                durability: options.durability,
                options: options.clone(),
                closed: AtomicBool::new(false),
                refreshing: Mutex::new(()),
                mode,
            }),
        };
        if let Mode::ReadWrite { thread, .. } = &db.inner.mode {
            // On an error `db` drops here: with no thread to stop, the drop closes the database itself.
            let started = Thread::start(&db.inner)?;
            *lock(thread) = Some(started);
        }
        Ok(db)
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

    /// Brings a read-only database up to what its writer has made durable since the open or the last refresh: its
    /// commits and its logged writes (SPEC §8.4, §8.6). A read-only `Db` shows one fixed state until then: calls made
    /// meanwhile, from any thread, read the same state, and a call that runs during the refresh reads the state before
    /// it or after it, never a mix. A read-write `Db` always reads its latest writes: there, `refresh` does nothing.
    ///
    /// ```
    /// use persia::OpenOptions;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// let writer = persia::Db::open(&path)?;
    /// let reader = OpenOptions::new().read_only(true).open(&path)?;
    /// writer.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// assert!(reader.collection("notes").is_err()); // the state the reader opened
    /// reader.refresh()?;
    /// assert!(reader.collection("notes").is_ok());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::Conflict`] if the file at the path is not this database any more (replaced, or an older copy
    /// restored); [`ErrorCode::Corrupt`] if it is damaged; [`ErrorCode::Io`]; [`ErrorCode::Closed`]. The database
    /// keeps the state it had on any error.
    pub fn refresh(&self) -> Result<()> {
        self.inner.refresh()
    }

    /// Closes the database: stops its maintenance thread, which makes the final commit, cancels the merges in
    /// flight and releases the writer lock, so the next open succeeds at once (ADR-0064). It waits for that without
    /// limit, behind a write another thread is making and behind a blob upload in flight (whose bytes are stored
    /// before its document takes the writer). Returns the first error: the first failure of a background
    /// commit or merge since the open, else the final commit's. The database is closed either way, and every
    /// handle on it fails with [`ErrorCode::Closed`] from then on.
    ///
    /// Dropping a `Db` makes the same request but waits at most 5 seconds and cannot report an error (it is
    /// logged). Past that bound the thread is left to finish on its own, and **it keeps the writer lock until it
    /// ends**, so an open right after such a drop can fail with [`ErrorCode::Conflict`]: prefer `close`.
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
    /// [`ErrorCode::Poisoned`] if a write had failed and stopped the writer, or a thread panicked while it wrote
    /// (nothing more was committed); otherwise as a background commit or the final commit failed:
    /// [`ErrorCode::Io`], [`ErrorCode::Corrupt`].
    pub fn close(self) -> Result<()> {
        self.inner.shut_down(None)
    }

    /// Test hook: runs one maintenance round now (a `tick` and a `merge_step`, ADR-0064 item 9) and returns its
    /// outcome; nothing on a read-only database.
    #[doc(hidden)]
    pub fn maintain_now(&self) -> Result<()> {
        self.inner.check_open()?;
        match self.inner.signals() {
            Some(signals) => signals.round_now(),
            None => Ok(()),
        }
    }

    /// Test hook: the `seq` of the latest commit the database shows.
    #[doc(hidden)]
    pub fn committed_seq(&self) -> Result<u64> {
        Ok(self.inner.snapshot()?.seq())
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let bound = self.inner.options.drop_wait;
        if let Err(error) = self.inner.shut_down(Some(bound)) {
            tracing::error!(path = %self.inner.path.display(), %error, "closing a dropped database failed");
        }
    }
}

/// The lock of a mutex whose value stays whole if a holder panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    /// Closes the database once; later calls do nothing. `bound` is how long to wait for the maintenance thread:
    /// without limit for `close`, a while for `Drop`, past which the thread is detached.
    fn shut_down(&self, bound: Option<Duration>) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let Mode::ReadWrite {
            signals, thread, ..
        } = &self.mode
        else {
            return self.finish();
        };
        signals.stop();
        let Some(running) = lock(thread).take() else {
            // It never started (a failed open): nobody else drops the database.
            return self.finish();
        };
        let joined = match bound {
            None => running.join(),
            Some(bound) => match running.join_within(bound) {
                Ok(joined) => joined,
                Err(running) => {
                    let handle = running.detach();
                    tracing::error!(
                        path = %self.path.display(),
                        waited = ?bound,
                        "a dropped database's maintenance thread is still running: it makes the final commit and \
                         releases the writer lock when the call holding the writer returns; prefer close()"
                    );
                    if let Some(slot) = &self.options.detached {
                        *lock(slot) = Some(handle);
                    }
                    return Ok(());
                }
            },
        };
        if let Some(result) = joined {
            return result;
        }
        // The thread panicked before its last step: drop the database here, with no final commit.
        if let Err(error) = self.finish() {
            tracing::error!(path = %self.path.display(), %error, "closing after the maintenance thread panicked");
        }
        Err(Error::writer_panicked())
    }

    /// The last step of a close: the final commit (none if a thread panicked holding the writer), the merges in flight
    /// cancelled, then the database dropped, which releases the writer lock. Idempotent.
    pub(crate) fn finish(&self) -> Result<()> {
        match &self.mode {
            Mode::ReadOnly(db) => {
                let closed = db.write().unwrap_or_else(PoisonError::into_inner).take();
                drop(closed);
                Ok(())
            }
            Mode::ReadWrite { writer, reads, .. } => {
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

    /// The maintenance thread's signals; `None` on a read-only database.
    pub(crate) fn signals(&self) -> Option<&Signals> {
        match &self.mode {
            Mode::ReadWrite { signals, .. } => Some(signals),
            Mode::ReadOnly(_) => None,
        }
    }

    /// `f` on the writer under the writer mutex, for the maintenance thread: it runs after the close too (its last
    /// round), so the closed flag is not checked.
    pub(crate) fn with_writer<T>(&self, f: impl FnOnce(&mut Writer) -> Result<T>) -> Result<T> {
        let Mode::ReadWrite { writer, .. } = &self.mode else {
            return Err(Error::read_only("maintain"));
        };
        let mut guard = writer.lock().map_err(|_| Error::writer_panicked())?;
        let db = guard.as_mut().ok_or_else(Error::closed)?;
        f(db)
    }
}

impl Inner {
    /// The path the database was opened at.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// [`ErrorCode::Closed`] once the database is closed or dropped.
    pub(crate) fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            Err(Error::closed())
        } else {
            Ok(())
        }
    }

    /// The durability of writes through handles that did not choose their own.
    pub(crate) fn durability(&self) -> Durability {
        self.durability
    }

    /// `f` on the blob store reads use: the writer's pack (shared, without the writer mutex) or the read-only pack.
    pub(crate) fn with_blobs<T>(
        &self,
        f: impl FnOnce(&Blobs<PackStore>) -> Result<T>,
    ) -> Result<T> {
        self.check_open()?;
        match &self.mode {
            Mode::ReadWrite { reads, .. } => match read_lock(reads).as_ref() {
                Some(r) => f(&r.blobs),
                None => Err(Error::closed()),
            },
            Mode::ReadOnly(db) => match read_lock(db).as_ref() {
                Some(db) => f(db.blobs()),
                None => Err(Error::closed()),
            },
        }
    }

    /// [`Db::refresh`]: a new read-only open of the documents, taken without the state's lock so reads go on meanwhile
    /// (only another refresh waits), then paired with the pack and swapped in under the write lock ([`ReadOnlyBlobDatabase::refresh`], ADR-0060).
    pub(crate) fn refresh(&self) -> Result<()> {
        self.check_open()?;
        let Mode::ReadOnly(state) = &self.mode else {
            return Ok(());
        };
        let _one_at_a_time = lock(&self.refreshing);
        let registry = self.options.database_options().registry;
        let reopened = ReadOnlyDatabase::open(&self.path, &registry).map_err(|e| {
            Error::wrap(e).context(format_args!("refreshing {}", self.path.display()))
        })?;
        let mut guard = state.write().unwrap_or_else(PoisonError::into_inner);
        let db = guard.as_mut().ok_or_else(Error::closed)?;
        db.refresh(reopened)
            .map(drop)
            .map_err(|e| Error::wrap(e).context(format_args!("refreshing {}", self.path.display())))
    }

    /// [`Db::commit`]: every collection's unflushed writes flushed and committed, under the writer mutex.
    pub(crate) fn commit(&self) -> Result<()> {
        self.write("commit", |db| db.commit().map(drop).map_err(Error::wrap))
    }

    /// [`ErrorCode::Closed`] once closed, [`ErrorCode::ReadOnly`] (naming `what`) on a read-only database.
    pub(crate) fn check_writable(&self, what: &str) -> Result<()> {
        self.check_open()?;
        match self.mode {
            Mode::ReadWrite { .. } => Ok(()),
            Mode::ReadOnly(_) => Err(Error::read_only(what)),
        }
    }

    /// The latest published snapshot (SPEC §8.4): a read-write database's latest commit plus its memtables, a
    /// read-only one's state as of its open or last refresh. No writer lock is taken.
    pub(crate) fn snapshot(&self) -> Result<Arc<Snapshot>> {
        self.check_open()?;
        match &self.mode {
            Mode::ReadWrite { reads, .. } => read_lock(reads)
                .as_ref()
                .map(|r| r.reader.snapshot())
                .ok_or_else(Error::closed),
            Mode::ReadOnly(db) => read_lock(db)
                .as_ref()
                .map(|db| db.database().snapshot())
                .ok_or_else(Error::closed),
        }
    }

    /// `f` on the writer, under the writer mutex: one write at a time (ADR-0064 item 1). `what` names the write for
    /// the error of a read-only database.
    pub(crate) fn write<T>(
        &self,
        what: &str,
        f: impl FnOnce(&mut Writer) -> Result<T>,
    ) -> Result<T> {
        self.check_open()?;
        let Mode::ReadWrite {
            writer, signals, ..
        } = &self.mode
        else {
            return Err(Error::read_only(what));
        };
        let mut guard = writer.lock().map_err(|_| Error::writer_panicked())?;
        let db = guard.as_mut().ok_or_else(Error::closed)?;
        let result = f(db);
        drop(guard);
        // Even a write that failed may have logged something: the next rounds look.
        signals.wrote();
        result
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    // A panic while it was held for writing (a close, a refresh) left its value whole: `Option::take` or a swap.
    lock.read().unwrap_or_else(PoisonError::into_inner)
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

/// The engine's or the pack's failure to open the database at `path`, with the path in front.
fn opening(path: &Path, error: DocumentsError) -> Error {
    Error::wrap(error).context(format_args!("opening {}", path.display()))
}

fn open_read_only(path: &Path, options: &OpenOptions) -> Result<Mode> {
    check_parents(path)?;
    if !exists(path)? {
        return Err(not_found(path));
    }
    let db = ReadOnlyBlobDatabase::open(path, &options.database_options())
        .map_err(|e| opening(path, e))?;
    Ok(Mode::ReadOnly(RwLock::new(Some(Box::new(db)))))
}

/// How many times an open starts over when the file appears or vanishes under it (SPEC §8.1, "Racing opens").
const OPEN_ATTEMPTS: usize = 8;

/// The pause before the first new start of an open; it doubles before each next one, up to [`MAX_OPEN_BACKOFF`], so the
/// starts of racing opens spread out instead of meeting again (at most about a quarter of a second in all).
const FIRST_OPEN_BACKOFF: Duration = Duration::from_millis(1);
const MAX_OPEN_BACKOFF: Duration = Duration::from_millis(128);

/// Fails with [`ErrorCode::InvalidArgument`] when the nearest ancestor of `path` that exists is not a directory: the
/// path cannot name a file (SPEC §8.1), whatever the mode. Nothing is created.
fn check_parents(path: &Path) -> Result<()> {
    for ancestor in path.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            return Ok(()); // a relative path: the current directory
        }
        match std::fs::metadata(ancestor) {
            Ok(meta) if meta.is_dir() => return Ok(()),
            Ok(_) => {
                return Err(Error::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "{} cannot name a database: {} is not a directory",
                        path.display(),
                        ancestor.display()
                    ),
                ));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(source) => {
                return Err(Error::with_source(
                    ErrorCode::Io,
                    format_args!("looking for {}", ancestor.display()),
                    source,
                ));
            }
        }
    }
    Ok(())
}

/// Creates the missing parent directories of `path` durably (SPEC §8.1: the facade's step, not the container's;
/// §4.5.2: each new directory's parent is synced, so a power loss after the first commit cannot lose the file's
/// directory entry), with the engine's [`create_parent_dirs`].
fn create_parents(path: &Path) -> Result<()> {
    create_parents_with(path, |p: &Path| create_parent_dirs(p))
}

/// [`create_parents`] with `create` as the engine's walk: [`create_parent_dirs`] on the operating system's file
/// system, or its `create_parent_dirs_in` on a simulated one in the crash tests below.
fn create_parents_with(path: &Path, create: impl FnOnce(&Path) -> io::Result<()>) -> Result<()> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    create(path).map_err(|source| {
        let code = if source.kind() == io::ErrorKind::InvalidInput {
            // An ancestor that is not a directory, created in between by someone else: `check_parents` refused
            // the ones it saw.
            ErrorCode::InvalidArgument
        } else {
            ErrorCode::Io
        };
        Error::with_source(
            code,
            format_args!("creating the directory {}", parent.display()),
            source,
        )
    })
}

fn open_read_write(path: &Path, options: &OpenOptions) -> Result<Mode> {
    check_parents(path)?;
    let mut backoff = FIRST_OPEN_BACKOFF;
    for attempt in 0..OPEN_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(backoff);
            backoff = backoff.saturating_mul(2).min(MAX_OPEN_BACKOFF);
        }
        let db = if exists(path)? {
            match BlobDatabase::open(path, options.database_options()) {
                Ok(db) => db,
                // The file vanished between the look and the open: decide again.
                Err(_) if !exists(path)? => continue,
                Err(e) => return Err(opening(path, e)),
            }
        } else if options.create_if_missing {
            create_parents(path)?;
            match BlobDatabase::create(path, options.database_options()) {
                Ok(db) => db,
                Err(DocumentsError::PackExists { path: pack }) => {
                    return Err(Error::new(
                        ErrorCode::Conflict,
                        format!(
                            "cannot create {}: a blob pack is at {} with no database beside it, and a new \
                             database could not use it",
                            path.display(),
                            pack.display()
                        ),
                    ));
                }
                // A link to a file that does not exist: `exists` follows it, the create refuses the taken name.
                Err(e) if e.code() == ErrorCode::AlreadyExists && is_dangling_link(path)? => {
                    return Err(dangling(path));
                }
                // Another open created the file first: open it.
                Err(e) if e.code() == ErrorCode::AlreadyExists => continue,
                Err(e) => return Err(opening(path, e)),
            }
        } else {
            return Err(not_found(path));
        };
        return Ok(read_write(db, options));
    }
    Err(Error::new(
        ErrorCode::Transient,
        format!(
            "opening {}: the file appeared or vanished {OPEN_ATTEMPTS} times while it was opened",
            path.display()
        ),
    ))
}

fn read_write(db: Writer, options: &OpenOptions) -> Mode {
    let reads = ReadSide {
        reader: db.log().reader(),
        blobs: db.blobs().clone(),
    };
    Mode::ReadWrite {
        writer: Mutex::new(Some(Box::new(db))),
        reads: RwLock::new(Some(reads)),
        signals: Signals::new(options.hold_maintenance),
        thread: Mutex::new(None),
    }
}

/// Whether `path` is a symbolic link whose target does not exist.
fn is_dangling_link(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Ok(!exists(path)?),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::with_source(
            ErrorCode::Io,
            format_args!("looking at {}", path.display()),
            source,
        )),
    }
}

/// The error of an open at a link to nothing: the database is not there, and the link is not replaced by one.
fn dangling(path: &Path) -> Error {
    let target = std::fs::read_link(path).map_or_else(
        |_| "a missing file".to_owned(),
        |target| target.display().to_string(),
    );
    Error::new(
        ErrorCode::NotFound,
        format!(
            "{} is a link to {target}, which does not exist: no database is created through it",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use persia_engine::db::create_parent_dirs_in;
    use persia_storage::container::sim::CrashLoss;
    use persia_storage::vfs::Vfs as _;
    use persia_storage::vfs::sim::SimFs;

    use super::*;

    /// The facade's parent step, on a simulated file system: every directory it creates survives a crash that drops
    /// whatever was not synced (SPEC §4.5.2).
    #[test]
    fn the_parents_an_open_creates_survive_a_crash() {
        let fs = SimFs::new();
        fs.create_dir(Path::new("/data")).unwrap();
        fs.sync_dir(Path::new("/")).unwrap();
        let path = Path::new("/data/2026/11/app.persia");
        create_parents_with(path, |p| create_parent_dirs_in(&fs, p)).unwrap();
        let crashed = fs.crash(CrashLoss::All);
        for dir in ["/data", "/data/2026", "/data/2026/11"] {
            assert!(crashed.is_dir(Path::new(dir)).unwrap(), "{dir}");
        }
        // A bare file name has no parent to create: the walk is not even called.
        create_parents_with(Path::new("app.persia"), |_| Err(io::Error::other("called"))).unwrap();
    }

    #[test]
    fn a_failed_parent_step_names_the_directory_and_its_cause() {
        let fs = SimFs::new();
        fs.create_dir(Path::new("/data")).unwrap();
        fs.sync_dir(Path::new("/")).unwrap();
        // An ancestor that became a file after `check_parents` looked: the engine's walk refuses it.
        let file = fs.create_new(Path::new("/data/notes")).unwrap();
        drop(file);
        let err = create_parents_with(Path::new("/data/notes/app.persia"), |p| {
            create_parent_dirs_in(&fs, p)
        })
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
        assert!(err.message().contains("/data/notes"), "{err}");
        let io = create_parents_with(Path::new("/x/app.persia"), |_| {
            Err(io::Error::other("disk on fire"))
        })
        .unwrap_err();
        assert_eq!(io.code(), ErrorCode::Io);
        assert!(std::error::Error::source(&io).is_some());
    }
}
