//! A runtime-agnostic async API (ROADMAP 10.1.11; ADR-0064 items 10 to 15), behind the cargo feature `async`, which
//! adds no dependency.
//!
//! [`AsyncDb`] and [`AsyncCollection`] run the calls of [`Db`] and [`Collection`] on a pool of plain threads that
//! belongs to the database: started by its first job, [`OpenOptions::async_threads`] of them (`min(4, cores)` by
//! default), joined by [`AsyncDb::close`], and not shared with the maintenance thread. Each call returns a
//! [`JobFuture`], a std [`Future`](std::future::Future) that any executor polls: no runtime is named, none is needed.

// The rest of the embedded API on the same pool (ROADMAP 10.1.26; ADR-0064, amended 2026-11-27).
mod blobs;
mod collections;
mod job;
mod json;
mod pool;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_engine::db::CancelToken;

pub use job::JobFuture;
use job::Slot;
use pool::Pool;
pub(crate) use pool::PoolOptions;
#[cfg(not(feature = "test-hooks"))]
use pool::{JobEvent, JobKind};
// Test hooks: what the pool did with each job (ADR-0064 item 9).
#[cfg(feature = "test-hooks")]
pub use pool::{JobEvent, JobHook, JobKind};

use crate::db::Inner;
use crate::{Collection, Db, Document, Error, ErrorCode, OpenOptions, Result};
use crate::{SearchRequest, SearchResponse};

/// A database opened for async use: a [`Db`] whose calls run on its own pool of threads (ADR-0064 item 12).
///
/// The pool starts with the first job, [`OpenOptions::async_threads`] threads (`min(4, cores)` by default), and
/// [`AsyncDb::close`] joins it before it closes the database. Dropping an `AsyncDb` without closing it refuses later
/// jobs with [`ErrorCode::Closed`], completes the queued ones with it, lets the running ones end on their own (the
/// pool's threads are not joined), and drops the database, which waits a while for its last commit (as dropping a
/// [`Db`] does): prefer `close`.
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
/// use persia::{AsyncDb, Document, SearchRequest};
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("app.persia");
/// # let setup = persia::Db::open(&path)?;
/// # let created = setup.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
/// # let body = created.schema()?.field("body").map(|f| f.id()).ok_or("no body")?;
/// # setup.close()?;
/// // In an async fn this is `AsyncDb::open(&path).await?`; here a `block_on` drives each future.
/// let db = block_on(AsyncDb::open(&path))?;
/// let notes = db.collection("notes")?;
/// block_on(notes.put(Document::new("n1").with(body, persia::Value::String("hello async world".into()))))?;
/// let response = block_on(notes.search(SearchRequest::new("hello")))?;
/// assert_eq!(response.hits[0].document.id(), "n1");
/// block_on(db.close())?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct AsyncDb {
    /// `None` once [`AsyncDb::close`] took it.
    db: Option<Db>,
    pool: Pool,
}

impl fmt::Debug for AsyncDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncDb")
            .field("db", &self.db)
            .field("pool", &self.pool)
            .finish()
    }
}

impl AsyncDb {
    /// [`Db::open`] on a thread of its own (no pool exists before the database): opens the database at `location`,
    /// creating it if no file is there. Dropping the future before it resolves lets the open finish on that thread,
    /// then closes what it opened.
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
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(persia::AsyncDb::open(dir.path().join("app.persia")))?; // created: it did not exist
    /// block_on(db.close())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::open`]; [`ErrorCode::Io`] if the OS refuses the open's thread.
    pub fn open(location: impl AsRef<Path>) -> JobFuture<Self> {
        Self::open_with(OpenOptions::new(), location)
    }

    /// [`AsyncDb::open`] with `options` ([`OpenOptions::open`]).
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
    /// let missing = dir.path().join("missing.persia");
    /// let err = block_on(AsyncDb::open_with(OpenOptions::new().create_if_missing(false), &missing)).unwrap_err();
    /// assert_eq!(err.code(), ErrorCode::NotFound);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`OpenOptions::open`]; [`ErrorCode::Io`] if the OS refuses the open's thread.
    pub fn open_with(options: OpenOptions, location: impl AsRef<Path>) -> JobFuture<Self> {
        let location: PathBuf = location.as_ref().to_owned();
        let slot = Slot::new(None);
        let delivered = Arc::clone(&slot);
        let hook = options.pool.hook.clone();
        let thread = std::thread::Builder::new()
            .name("persia-async-open".to_owned())
            .spawn(move || {
                if delivered.start() {
                    let kind = JobKind::Open;
                    let started = JobEvent::Started { kind, token: None };
                    pool::emit(hook.as_ref(), &started);
                    let opened = options.open(&location).map(|db| {
                        let pool = Pool::new(db.inner.options.pool.clone());
                        Self { db: Some(db), pool }
                    });
                    let error = opened.as_ref().err().map(Error::code);
                    // A dropped future's open is closed here, before `Finished`: `finish` drops the `Db`.
                    let delivered = delivered.finish(opened);
                    let finished = JobEvent::Finished {
                        kind,
                        error,
                        delivered,
                    };
                    pool::emit(hook.as_ref(), &finished);
                }
            });
        match thread {
            Ok(thread) => JobFuture::on_thread(slot, thread),
            Err(source) => JobFuture::ready(Err(Error::with_source(
                ErrorCode::Io,
                "starting the thread of an async open",
                source,
            ))),
        }
    }

    /// Closes the database ([`Db::close`]) once its pool is done: later jobs are refused with
    /// [`ErrorCode::Closed`], the jobs still queued complete with it, the running ones end, the pool's threads are
    /// joined, and then the database makes its final commit and releases the file. The waiting runs on a thread of
    /// its own; dropping the future before it resolves lets the close finish there.
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
    /// use persia::{AsyncDb, Document, ErrorCode};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # let setup = persia::Db::open(&path)?;
    /// # setup.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// # setup.close()?;
    /// let db = block_on(AsyncDb::open(&path))?;
    /// let notes = db.collection("notes")?;
    /// block_on(db.close())?;
    /// let err = block_on(notes.put(Document::new("late"))).unwrap_err();
    /// assert_eq!(err.code(), ErrorCode::Closed);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::close`]; [`ErrorCode::Io`] if the OS refuses the close's thread (the database is then closed as a
    /// dropped [`Db`] is).
    pub fn close(mut self) -> JobFuture<()> {
        let threads = self.pool.close();
        let Some(db) = self.db.take() else {
            return JobFuture::ready(Ok(()));
        };
        let slot = Slot::new(None);
        let delivered = Arc::clone(&slot);
        let thread = std::thread::Builder::new()
            .name("persia-async-close".to_owned())
            .spawn(move || {
                // Started at once: a close is not skipped when its future is dropped.
                delivered.start();
                pool::join(threads);
                delivered.finish(db.close());
            });
        match thread {
            Ok(thread) => JobFuture::on_thread(slot, thread),
            Err(source) => JobFuture::ready(Err(Error::with_source(
                ErrorCode::Io,
                "starting the thread of an async close",
                source,
            ))),
        }
    }

    /// A handle on the collection named `name` ([`Db::collection`]): a lookup in the latest published state, which
    /// takes no lock and runs on the caller's thread.
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
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(persia::AsyncDb::open(dir.path().join("app.persia")))?;
    /// assert_eq!(db.collection("missing").unwrap_err().code(), persia::ErrorCode::NotFound);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::collection`].
    pub fn collection(&self, name: &str) -> Result<AsyncCollection> {
        let db = self.db.as_ref().ok_or_else(Error::closed)?;
        Ok(AsyncCollection {
            collection: db.collection(name)?,
            pool: self.pool.clone(),
        })
    }

    /// Test hook: the number of threads the database's async pool runs now: 0 before its first job and once closed.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn pool_threads(&self) -> usize {
        self.pool.running_threads()
    }

    /// [`Db::commit`] on the pool.
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
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(persia::AsyncDb::open(dir.path().join("app.persia")))?;
    /// block_on(db.commit())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::commit`].
    pub fn commit(&self) -> JobFuture<()> {
        self.submit(JobKind::Commit, |inner, _| inner.commit())
    }

    /// Queues `work` on the pool, with the state the database's handles share; a closed `AsyncDb` gives a future ready
    /// with [`ErrorCode::Closed`].
    fn submit<T, F>(&self, kind: JobKind, work: F) -> JobFuture<T>
    where
        T: Send + 'static,
        F: FnOnce(&Arc<Inner>, Option<&CancelToken>) -> Result<T> + Send + 'static,
    {
        let Some(db) = self.db.as_ref() else {
            return JobFuture::ready(Err(Error::closed()));
        };
        let inner = Arc::clone(&db.inner);
        self.pool.submit(kind, move |token| work(&inner, token))
    }
}

impl Drop for AsyncDb {
    fn drop(&mut self) {
        // The running jobs end on their own: their threads are not joined here (a destructor that could wait behind
        // a long write); `close` joins them.
        drop(self.pool.close());
    }
}

/// A handle on one collection of an [`AsyncDb`]: [`Collection`]'s calls on the database's pool.
///
/// Arguments are taken by value, since the job runs on another thread. Like a [`Collection`] it is cheap to clone
/// and can outlive its database: after [`AsyncDb::close`] (or a drop) its calls resolve to [`ErrorCode::Closed`].
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
/// use persia::Document;
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("app.persia");
/// # let setup = persia::Db::open(&path)?;
/// # setup.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
/// # setup.close()?;
/// let db = block_on(persia::AsyncDb::open(&path))?;
/// let notes = db.collection("notes")?;
/// let other = notes.clone(); // the same collection, the same pool
/// block_on(other.put(Document::new("a")))?;
/// assert!(block_on(notes.get("a"))?.is_some());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct AsyncCollection {
    collection: Collection,
    pool: Pool,
}

impl fmt::Debug for AsyncCollection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncCollection")
            .field("collection", &self.collection)
            .finish_non_exhaustive()
    }
}

/// [`ErrorCode::Cancelled`] if the future of the read `what` was dropped (its `token` set) before it read: a read
/// that is one lookup (a get, a blob range) has no point to stop at once it has started reading.
fn check_cancelled(token: Option<&CancelToken>, what: &str) -> Result<()> {
    if token.is_some_and(CancelToken::is_cancelled) {
        return Err(Error::new(
            ErrorCode::Cancelled,
            format!("the {what} was cancelled: its future was dropped"),
        ));
    }
    Ok(())
}

impl AsyncCollection {
    /// [`Collection::put`] on the pool. Once started it runs to its end, even if the future is dropped.
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
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # let setup = persia::Db::open(&path)?;
    /// # setup.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// # setup.close()?;
    /// # let db = block_on(persia::AsyncDb::open(&path))?;
    /// let notes = db.collection("notes")?;
    /// block_on(notes.put(persia::Document::new("a")))?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::put`]; [`ErrorCode::Closed`] if the database closed before the job started.
    pub fn put(&self, doc: Document) -> JobFuture<()> {
        let collection = self.collection.clone();
        self.pool
            .submit(JobKind::Put, move |_| collection.put(&doc))
    }

    /// [`Collection::put_batch`] on the pool: the batch is seen whole once it returns `Ok`. Once started it runs to
    /// its end, even if the future is dropped.
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
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # let setup = persia::Db::open(&path)?;
    /// # setup.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// # setup.close()?;
    /// # let db = block_on(persia::AsyncDb::open(&path))?;
    /// let notes = db.collection("notes")?;
    /// block_on(notes.put_batch(vec![persia::Document::new("a"), persia::Document::new("b")]))?;
    /// assert!(block_on(notes.get("b"))?.is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::put_batch`]; [`ErrorCode::Closed`] if the database closed before the job started.
    pub fn put_batch(&self, docs: Vec<Document>) -> JobFuture<()> {
        let collection = self.collection.clone();
        self.pool
            .submit(JobKind::PutBatch, move |_| collection.put_batch(&docs))
    }

    /// [`Collection::delete`] on the pool. Once started it runs to its end, even if the future is dropped.
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
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # let setup = persia::Db::open(&path)?;
    /// # setup.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// # setup.close()?;
    /// # let db = block_on(persia::AsyncDb::open(&path))?;
    /// let notes = db.collection("notes")?;
    /// block_on(notes.put(persia::Document::new("a")))?;
    /// block_on(notes.delete("a"))?;
    /// assert!(block_on(notes.get("a"))?.is_none());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::delete`]; [`ErrorCode::Closed`] if the database closed before the job started.
    pub fn delete(&self, id: impl Into<String>) -> JobFuture<()> {
        let collection = self.collection.clone();
        let id = id.into();
        self.pool
            .submit(JobKind::Delete, move |_| collection.delete(&id))
    }

    /// [`Collection::get`] on the pool. Dropping the future of a get that has started sets its cancellation token:
    /// a get that has not read yet stops with [`ErrorCode::Cancelled`] (which nobody receives).
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
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # let setup = persia::Db::open(&path)?;
    /// # setup.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// # setup.close()?;
    /// # let db = block_on(persia::AsyncDb::open(&path))?;
    /// let notes = db.collection("notes")?;
    /// assert!(block_on(notes.get("missing"))?.is_none());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::get`]; [`ErrorCode::Closed`] if the database closed before the job started.
    pub fn get(&self, id: impl Into<String>) -> JobFuture<Option<Document>> {
        let collection = self.collection.clone();
        let id = id.into();
        self.pool.submit(JobKind::Get, move |token| {
            check_cancelled(token, "get")?;
            collection.get(&id)
        })
    }

    /// [`Collection::commit`] on the pool: commits every collection of the database.
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
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # let setup = persia::Db::open(&path)?;
    /// # setup.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// # setup.close()?;
    /// # let db = block_on(persia::AsyncDb::open(&path))?;
    /// let notes = db.collection("notes")?;
    /// block_on(notes.put(persia::Document::new("a")))?;
    /// block_on(notes.commit())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::commit`]; [`ErrorCode::Closed`] if the database closed before the job started.
    pub fn commit(&self) -> JobFuture<()> {
        let collection = self.collection.clone();
        self.pool
            .submit(JobKind::Commit, move |_| collection.commit())
    }

    /// [`Collection::search`] on the pool. Dropping the future of a search that has started sets its cancellation
    /// token: the search stops soon after (SPEC §7.3) with [`ErrorCode::Cancelled`], which nobody receives.
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
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// # let setup = persia::Db::open(&path)?;
    /// # let created = setup.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// # let body = created.schema()?.field("body").map(|f| f.id()).ok_or("no body")?;
    /// # setup.close()?;
    /// # let db = block_on(persia::AsyncDb::open(&path))?;
    /// let notes = db.collection("notes")?;
    /// block_on(notes.put(persia::Document::new("n1").with(body, persia::Value::String("red shoes".into()))))?;
    /// let response = block_on(notes.search(persia::SearchRequest::new("shoes")))?;
    /// assert_eq!(response.hits.len(), 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::search`]; [`ErrorCode::Closed`] if the database closed before the job started.
    pub fn search(&self, request: SearchRequest) -> JobFuture<SearchResponse> {
        let collection = self.collection.clone();
        self.pool.submit(JobKind::Search, move |token| {
            collection.search_cancellable(&request, token.cloned())
        })
    }
}
