//! How a database is opened (SPEC §8.1, §14; ROADMAP 10.1.1).

use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use persia_engine::db::{Clock, CommitPolicy, DatabaseOptions, MappedReads, ReadOnlyOptions};
use persia_engine::memtable::FlushPolicy;
use persia_engine::merge::MergeOptions;
use persia_engine::segment::BlockCache;
use persia_engine::wal::Durability;

use crate::db::Db;
use crate::{Result, location};

/// How long dropping a `Db` waits for its maintenance thread (ADR-0064 item 6).
const DROP_WAIT: Duration = Duration::from_secs(5);

/// Test hook: a place for a detached maintenance thread (`OpenOptions::detached_into`, feature `test-hooks`).
#[doc(hidden)]
pub type DetachedSlot = Arc<Mutex<Option<JoinHandle<()>>>>;

/// Options for opening a database: a builder whose defaults are the ones of SPEC §14, with no file to read them
/// from. [`Db::open`] is `OpenOptions::new().open(..)`.
///
/// | Option | Default | SPEC §14 knob |
/// |---|---|---|
/// | [`create_if_missing`](Self::create_if_missing) | `true` | |
/// | [`read_only`](Self::read_only) | `false` | |
/// | [`memtable_bytes`](Self::memtable_bytes) | 64 MiB | `memtable_bytes` |
/// | [`idle_flush`](Self::idle_flush) | 10 s | (with `memtable_bytes`) |
/// | [`commit_interval`](Self::commit_interval) | 60 s | `commit_interval` |
/// | [`merge_threads`](Self::merge_threads) | 1 | `merge_threads` |
/// | [`durability`](Self::durability) | [`Durability::Fsync`] | `fsync_policy` |
/// | [`block_cache_bytes`](Self::block_cache_bytes) | 8 MiB | (SPEC §10.6) |
/// | [`mapped_reads`](Self::mapped_reads) | on a local disk, not on Windows | (SPEC §4.7) |
/// | `async_threads` (feature `async`) | `min(4, cores)` | (ADR-0064) |
///
/// No option is stored in the database: each open chooses its own.
///
/// ```
/// use std::time::Duration;
/// use persia::{Durability, OpenOptions};
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// let path = dir.path().join("app.persia");
/// let db = OpenOptions::new()
///     .memtable_bytes(16 << 20)
///     .commit_interval(Duration::from_secs(30))
///     .durability(Durability::Async)
///     .open(&path)?;
/// db.close()?;
///
/// // The same file, read-only: nothing is created, nothing is written.
/// let reader = OpenOptions::new().read_only(true).open(&path)?;
/// assert!(reader.is_read_only());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct OpenOptions {
    pub(crate) create_if_missing: bool,
    pub(crate) read_only: bool,
    memtable_bytes: usize,
    idle_flush: Duration,
    commit_interval: Duration,
    merge_threads: usize,
    pub(crate) durability: Durability,
    /// The size of the `Db`'s block cache, in decoded bytes.
    block_cache_bytes: u64,
    /// Whether a read-only open maps its container; the engine's default when `None`.
    mapped_reads: Option<bool>,
    /// Where the database reads the time; the system's clocks when `None`.
    clock: Option<Arc<dyn Clock>>,
    /// Test hook: the maintenance thread runs only the rounds `Db::maintain_now` asks for.
    pub(crate) hold_maintenance: bool,
    /// How long a `Drop` waits for the maintenance thread before it detaches it (ADR-0064 item 6).
    pub(crate) drop_wait: Duration,
    /// Test hook: where a `Drop` puts the thread it detached.
    pub(crate) detached: Option<DetachedSlot>,
    /// Test hook: called by `Db::backup` at each of its steps.
    #[cfg(feature = "test-hooks")]
    pub(crate) backup_hook: Option<crate::BackupHook>,
    /// How the async pool is set up (ADR-0064 item 12).
    #[cfg(feature = "async")]
    pub(crate) pool: crate::asynchronous::PoolOptions,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenOptions {
    /// The defaults (the table above).
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = persia::OpenOptions::new().open(dir.path().join("app.persia"))?;
    /// assert!(!db.is_read_only());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            create_if_missing: true,
            read_only: false,
            memtable_bytes: FlushPolicy::DEFAULT_MEMTABLE_BYTES,
            idle_flush: FlushPolicy::DEFAULT_IDLE,
            commit_interval: CommitPolicy::DEFAULT_INTERVAL,
            merge_threads: MergeOptions::DEFAULT_THREADS,
            durability: Durability::Fsync,
            block_cache_bytes: BlockCache::DEFAULT_CAPACITY,
            mapped_reads: None,
            clock: None,
            hold_maintenance: false,
            drop_wait: DROP_WAIT,
            detached: None,
            #[cfg(feature = "test-hooks")]
            backup_hook: None,
            #[cfg(feature = "async")]
            pool: crate::asynchronous::PoolOptions::default(),
        }
    }

    /// Whether the open creates the database when no file is at the path (SPEC §8.1). On by default; `false`
    /// requires the file to exist and creates nothing.
    ///
    /// ```
    /// use persia::{ErrorCode, OpenOptions};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let err = OpenOptions::new().create_if_missing(false).open(&path).unwrap_err();
    /// assert_eq!(err.code(), ErrorCode::NotFound);
    /// assert!(!path.exists());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn create_if_missing(mut self, create: bool) -> Self {
        self.create_if_missing = create;
        self
    }

    /// Whether the database is opened read-only (SPEC §8.1, §8.4): no writer lock, no write, beside a writer in
    /// this process or another. It shows what the writer had made durable when the open ran, and what each
    /// [`Db::refresh`] finds later. A read-only open never creates the database, whatever [`create_if_missing`](Self::create_if_missing) says.
    ///
    /// ```
    /// use persia::{Db, ErrorCode, OpenOptions};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let writer = Db::open(&path)?;
    /// let reader = OpenOptions::new().read_only(true).open(&path)?; // beside the writer
    /// assert!(reader.is_read_only() && !writer.is_read_only());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// The size at which a collection's unflushed writes are flushed into a segment and committed (SPEC §8.2
    /// step 3, §14 `memtable_bytes`). 64 MiB by default. 0 is refused at open.
    ///
    /// ```
    /// let options = persia::OpenOptions::new().memtable_bytes(8 << 20);
    /// # let _ = options;
    /// ```
    #[must_use]
    pub fn memtable_bytes(mut self, bytes: usize) -> Self {
        self.memtable_bytes = bytes;
        self
    }

    /// How long a collection's writes must pause before they are flushed and committed in the background
    /// (SPEC §8.2 step 3). 10 s by default. Zero commits at every maintenance round (about once a
    /// second) that finds unflushed writes.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// let options = persia::OpenOptions::new().idle_flush(Duration::from_secs(2));
    /// # let _ = options;
    /// ```
    #[must_use]
    pub fn idle_flush(mut self, idle: Duration) -> Self {
        self.idle_flush = idle;
        self
    }

    /// The longest a write stays unflushed while writes never pause: a background commit flushes it once this
    /// long has passed since the first unflushed write (SPEC §14 `commit_interval`), which bounds what an open
    /// after a crash replays. 60 s by default. Zero commits at every maintenance round that finds
    /// unflushed writes.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// let options = persia::OpenOptions::new().commit_interval(Duration::from_secs(15));
    /// # let _ = options;
    /// ```
    #[must_use]
    pub fn commit_interval(mut self, interval: Duration) -> Self {
        self.commit_interval = interval;
        self
    }

    /// How many threads merge segments in the background (SPEC §8.5, §14 `merge_threads`). 1 by default. With 0, no
    /// merge thread is started: each merge runs to its end on the database's maintenance thread, one at a time, and
    /// holds the writer while it runs (writes wait for it).
    ///
    /// ```
    /// let options = persia::OpenOptions::new().merge_threads(2);
    /// # let _ = options;
    /// ```
    #[must_use]
    pub fn merge_threads(mut self, threads: usize) -> Self {
        self.merge_threads = threads;
        self
    }

    /// The durability a write reaches before its call returns (SPEC §8.2.1, §14 `fsync_policy`), for every
    /// collection handle of this database that does not choose its own. [`Durability::Fsync`] by default: a write
    /// that returned survives a power loss.
    ///
    /// ```
    /// use persia::{Durability, OpenOptions};
    ///
    /// // Writes return once they are in the file, not synced: they survive the process, not the machine.
    /// let options = OpenOptions::new().durability(Durability::Async);
    /// # let _ = options;
    /// ```
    #[must_use]
    pub fn durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    /// The size of the database's block cache, in decoded bytes (SPEC §10.6, ROADMAP 10.2.5): every segment the
    /// `Db` opens reads its blocks through this one cache, its own, not one shared with other databases.
    /// 8 MiB by default (SPEC §14, until ROADMAP 15.2.4 measures a better one); 0 caches nothing.
    /// [`Db::block_cache_stats`] reads how it is used.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = persia::OpenOptions::new().block_cache_bytes(64 << 20).open(dir.path().join("app.persia"))?;
    /// assert_eq!(db.block_cache_stats()?.capacity, 64 << 20);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn block_cache_bytes(mut self, bytes: u64) -> Self {
        self.block_cache_bytes = bytes;
        self
    }

    /// Whether a read-only open reads its container and its blob pack through memory mappings (SPEC §4.7, ROADMAP
    /// 10.2.4, 10.1.23): `true` maps each file that is on a local disk, `false` always reads both with ordinary reads
    /// (`pread`). By default it maps on a local disk, except on Windows, where it does not (ADR-0017, amended). A
    /// read-write `Db` maps neither, whatever this says. A mapped reader needs that no other program writes or
    /// truncates either file while it is open. On Windows, `true` also maps the blob pack, and a mapped pack may make
    /// the writer's blob GC fail to rename its rewritten pack over it (on FAT32 or exFAT) or keep the writer from
    /// truncating a torn pack tail, as for the container (ADR-0017 amended; ROADMAP 10.1.22, 10.1.32).
    ///
    /// ```
    /// use persia::OpenOptions;
    ///
    /// // A file another program may rewrite under the reader: read it with ordinary reads.
    /// let options = OpenOptions::new().read_only(true).mapped_reads(false);
    /// # let _ = options;
    /// ```
    #[must_use]
    pub fn mapped_reads(mut self, mapped: bool) -> Self {
        self.mapped_reads = Some(mapped);
        self
    }

    /// How many threads the database's async pool runs (ADR-0064 item 12, feature `async`): the threads every call of
    /// an [`AsyncDb`](crate::AsyncDb) and its [`AsyncCollection`](crate::AsyncCollection)s runs on. `min(4, cores)` by
    /// default. The pool starts with the first job, not at the open, and [`AsyncDb::close`](crate::AsyncDb::close)
    /// joins it; a `Db` opened without `AsyncDb` never starts it. 0 is refused at open. There is no upper bound: a
    /// count the OS cannot start fails the first job with [`ErrorCode::Io`](crate::ErrorCode::Io) (the threads it had
    /// started are joined, and the next job tries again).
    ///
    /// ```
    /// # use std::future::Future;
    /// # fn block_on<F: Future>(future: F) -> F::Output {
    /// #     struct Unpark(std::thread::Thread);
    /// #     impl std::task::Wake for Unpark {
    /// #         fn wake(self: std::sync::Arc<Self>) { self.0.unpark(); }
    /// #     }
    /// #     let waker = std::sync::Arc::new(Unpark(std::thread::current())).into();
    /// #     let mut cx = std::task::Context::from_waker(&waker);
    /// #     let mut future = std::pin::pin!(future);
    /// #     loop {
    /// #         match future.as_mut().poll(&mut cx) {
    /// #             std::task::Poll::Ready(output) => return output,
    /// #             std::task::Poll::Pending => std::thread::park(),
    /// #         }
    /// #     }
    /// # }
    /// use persia::{AsyncDb, ErrorCode, OpenOptions};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(AsyncDb::open_with(OpenOptions::new().async_threads(2), dir.path().join("app.persia")))?;
    /// block_on(db.commit())?; // the first job starts the pool's two threads
    /// block_on(db.close())?;
    ///
    /// let none = OpenOptions::new().async_threads(0);
    /// let err = block_on(AsyncDb::open_with(none, dir.path().join("other.persia"))).unwrap_err();
    /// assert_eq!(err.code(), ErrorCode::InvalidArgument);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[cfg(feature = "async")]
    #[must_use]
    pub fn async_threads(mut self, threads: usize) -> Self {
        self.pool.threads = Some(threads);
        self
    }

    /// Test hook: the maintenance thread runs only the rounds [`Db::maintain_now`] asks for, so a test decides when
    /// background commits and merges happen.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    #[must_use]
    pub fn hold_maintenance(mut self, hold: bool) -> Self {
        self.hold_maintenance = hold;
        self
    }

    /// Test hook: how long dropping a `Db` waits for its maintenance thread (5 s by default).
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    #[must_use]
    pub fn drop_wait(mut self, wait: Duration) -> Self {
        self.drop_wait = wait;
        self
    }

    /// Test hook: where dropping a `Db` puts the maintenance thread it gave up waiting for, so a test can join it.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    #[must_use]
    pub fn detached_into(mut self, slot: DetachedSlot) -> Self {
        self.detached = Some(slot);
        self
    }

    /// Test hook: where the database reads the time, so tests decide when a background commit is due.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Test hook: called by `Db::backup` at each of its steps, on the caller's thread; a hook that blocks holds the
    /// backup there (with the writer mutex released).
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    #[must_use]
    pub fn backup_hook(mut self, hook: crate::BackupHook) -> Self {
        self.backup_hook = Some(hook);
        self
    }

    /// Test hook: the async pool takes the OS to refuse its thread of index `n` (0-based) at its first start, as
    /// `std::thread::Builder::spawn` does when the OS is out of threads; the next start is not refused.
    #[cfg(all(feature = "async", feature = "test-hooks"))]
    #[doc(hidden)]
    #[must_use]
    pub fn async_refuse_thread(mut self, n: usize) -> Self {
        self.pool.refuse_thread = Some(n);
        self
    }

    /// Test hook: called by the async pool's threads around each job; a hook that blocks holds the job there.
    #[cfg(all(feature = "async", feature = "test-hooks"))]
    #[doc(hidden)]
    #[must_use]
    pub fn async_hook(mut self, hook: crate::JobHook) -> Self {
        self.pool.hook = Some(hook);
        self
    }

    /// Opens the database at `location` with these options: a filesystem path, or a `file://` URL with an empty
    /// host or `localhost` (see [`Db::open`] for what is a URL and for the rules of the open, SPEC §8.1).
    ///
    /// ```
    /// use persia::OpenOptions;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let options = OpenOptions::new().merge_threads(0);
    /// let first = options.open(dir.path().join("a.persia"))?; // the options can open several databases
    /// let second = options.open(dir.path().join("b.persia"))?;
    /// # drop((first, second));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`](crate::ErrorCode::InvalidArgument) for a `memtable_bytes` of 0, which would
    /// flush a segment before every write, and for an `async_threads` of 0 (feature `async`); then as
    /// [`Db::open`].
    pub fn open(&self, location: impl AsRef<Path>) -> Result<Db> {
        self.validate()?;
        let path = location::path_of(location.as_ref())?;
        Db::open_at(path, self)
    }

    /// Refuses the settings that have no useful meaning (see [`OpenOptions::open`]). `merge_threads(0)` is not one
    /// (each merge runs on the maintenance thread), nor a zero `idle_flush` or `commit_interval` (every maintenance
    /// round that finds unflushed writes commits them).
    fn validate(&self) -> Result<()> {
        let refused = if self.memtable_bytes == 0 {
            Some("memtable_bytes must be at least 1: 0 would flush a segment at every write")
        } else {
            self.refused_pool()
        };
        refused.map_or(Ok(()), |message| {
            Err(crate::Error::new(
                crate::ErrorCode::InvalidArgument,
                message,
            ))
        })
    }

    /// Refuses an async pool of no thread, whose jobs would wait for ever.
    #[cfg(feature = "async")]
    fn refused_pool(&self) -> Option<&'static str> {
        (self.pool.threads == Some(0)).then_some(
            "async_threads must be at least 1: a pool of no thread would never run a job",
        )
    }

    #[cfg(not(feature = "async"))]
    #[allow(clippy::unused_self)] // The `async` form reads the pool's options.
    fn refused_pool(&self) -> Option<&'static str> {
        None
    }

    /// The clock a search reads its timeout and its relaxation budget on (SPEC §7.1.4, §7.6, ROADMAP 10.1.44): the
    /// database's, the one its commit policy reads (`OpenOptions::clock`, a test hook), else the system's
    /// monotonic clock, one shared by every search rather than allocated for each.
    pub(crate) fn search_clock(&self) -> Arc<dyn Clock> {
        static SYSTEM: LazyLock<Arc<dyn Clock>> =
            LazyLock::new(|| Arc::new(persia_engine::db::SystemClock));
        self.clock.as_ref().unwrap_or(&SYSTEM).clone()
    }

    /// The engine's options for one open (they are not `Clone`: each open draws its own segment ids).
    pub(crate) fn database_options(&self, cache: &Arc<BlockCache>) -> DatabaseOptions {
        let defaults = DatabaseOptions::default();
        DatabaseOptions {
            // Given explicitly, not the per-open default: the `Db` reads its stats ([`Db::block_cache_stats`]).
            block_cache: Some(Arc::clone(cache)),
            policy: CommitPolicy {
                flush: FlushPolicy {
                    memtable_bytes: self.memtable_bytes,
                    idle: self.idle_flush,
                },
                interval: self.commit_interval,
            },
            merge: MergeOptions {
                threads: self.merge_threads,
                ..defaults.merge
            },
            clock: self.clock.clone().unwrap_or(defaults.clock),
            ..defaults
        }
    }

    /// The engine's options for one read-only open or refresh: the registry of the read-write ones, `cache`, and the
    /// mapping [`OpenOptions::mapped_reads`] chose (the engine's default unless set).
    pub(crate) fn read_only_options(&self, cache: &Arc<BlockCache>) -> ReadOnlyOptions {
        let options = ReadOnlyOptions::new().block_cache(Arc::clone(cache));
        match self.mapped_reads {
            None => options,
            Some(true) => options.mapped_reads(MappedReads::IfLocal),
            Some(false) => options.mapped_reads(MappedReads::Never),
        }
    }

    /// A new block cache of the size these options say, for one `Db` (SPEC §10.6).
    pub(crate) fn new_block_cache(&self) -> Arc<BlockCache> {
        Arc::new(BlockCache::new(self.block_cache_bytes))
    }
}

#[cfg(test)]
mod tests {
    use persia_engine::db::ManualClock;

    use super::*;

    #[test]
    fn the_defaults_are_the_specs() {
        let options = OpenOptions::new();
        assert!(options.create_if_missing && !options.read_only);
        assert_eq!(options.durability, Durability::Fsync);
        let engine = options.database_options(&Arc::new(BlockCache::default()));
        assert_eq!(engine.policy, CommitPolicy::default());
        assert_eq!(engine.policy.flush.memtable_bytes, 64 << 20);
        assert_eq!(engine.policy.flush.idle, Duration::from_secs(10));
        assert_eq!(engine.policy.interval, Duration::from_secs(60));
        assert_eq!(engine.merge.threads, 1);
        let by_default = OpenOptions::default().database_options(&Arc::new(BlockCache::default()));
        assert_eq!(by_default.policy, engine.policy);
    }

    #[test]
    fn every_knob_reaches_the_engine() {
        let clock = Arc::new(ManualClock::new(1_000));
        let options = OpenOptions::new()
            .create_if_missing(false)
            .read_only(true)
            .memtable_bytes(1 << 20)
            .idle_flush(Duration::from_secs(2))
            .commit_interval(Duration::from_secs(7))
            .merge_threads(3)
            .durability(Durability::Buffered)
            .clock(clock)
            .hold_maintenance(true)
            .drop_wait(Duration::from_millis(7))
            .detached_into(Arc::default());
        assert!(!options.create_if_missing && options.read_only);
        assert!(options.hold_maintenance && options.detached.is_some());
        assert_eq!(options.drop_wait, Duration::from_millis(7));
        let defaults = OpenOptions::new();
        assert!(!defaults.hold_maintenance && defaults.detached.is_none());
        assert_eq!(defaults.drop_wait, Duration::from_secs(5));
        assert_eq!(options.durability, Durability::Buffered);
        let engine = options.database_options(&Arc::new(BlockCache::default()));
        assert_eq!(engine.policy.flush.memtable_bytes, 1 << 20);
        assert_eq!(engine.policy.flush.idle, Duration::from_secs(2));
        assert_eq!(engine.policy.interval, Duration::from_secs(7));
        assert_eq!(engine.merge.threads, 3);
        assert_eq!(engine.clock.unix_micros(), 1_000, "the injected clock");
        // Without one, the system's: not 1970.
        let system = OpenOptions::new().database_options(&Arc::new(BlockCache::default()));
        assert!(system.clock.unix_micros() > 1_000_000_000_000_000);
    }
}
