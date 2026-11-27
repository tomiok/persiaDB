//! Collection management, `refresh` and `backup` on the database's pool (ROADMAP 10.1.26; ADR-0064, amended
//! 2026-11-27). Each is a change of the database (or, for `refresh`, of what it shows) that has no point to stop at
//! once started: dropping the future of one that has started lets it run to its end, and its result is discarded.

use std::path::{Path, PathBuf};

use super::{AsyncCollection, AsyncDb, JobFuture, JobKind};
use crate::{CollectionSchema, Error, ErrorCode, Result};

impl AsyncDb {
    /// [`Db::create_collection`](crate::Db::create_collection) on the pool; resolves to a handle on the new collection.
    /// Once started it runs to its end (the collection is created and committed) even if the future is dropped.
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
    /// use persia::{AsyncDb, FieldKind, SchemaBuilder, TextOptions};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(AsyncDb::open(dir.path().join("app.persia")))?;
    /// let schema = SchemaBuilder::new("notes").field("body", FieldKind::Text(TextOptions::default())).build()?;
    /// let notes = block_on(db.create_collection(schema))?;
    /// assert!(block_on(notes.get("n1"))?.is_none());
    /// block_on(db.close())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::create_collection`](crate::Db::create_collection); [`ErrorCode::Closed`](crate::ErrorCode::Closed) if
    /// the database closed before the job started.
    pub fn create_collection(&self, schema: CollectionSchema) -> JobFuture<AsyncCollection> {
        let pool = self.pool.clone();
        self.submit(JobKind::CreateCollection, move |inner, _| {
            let collection = inner.create_collection(schema)?;
            Ok(AsyncCollection { collection, pool })
        })
    }

    /// [`Db::create_collection_json`](crate::Db::create_collection_json) on the pool: the JSON is read on the pool's
    /// thread. Once started it runs to its end even if the future is dropped.
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
    /// use persia::{AsyncDb, ErrorCode};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(AsyncDb::open(dir.path().join("app.persia")))?;
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {}}"#))?;
    /// block_on(notes.put(persia::Document::new("n1")))?;
    /// let err = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)).unwrap_err();
    /// assert_eq!(err.code(), ErrorCode::AlreadyExists);
    /// block_on(db.close())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::create_collection_json`](crate::Db::create_collection_json);
    /// [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the database closed before the job started.
    pub fn create_collection_json(&self, json: impl Into<String>) -> JobFuture<AsyncCollection> {
        let json = json.into();
        let pool = self.pool.clone();
        self.submit(JobKind::CreateCollection, move |inner, _| {
            let collection = inner.create_collection_json(&json)?;
            Ok(AsyncCollection { collection, pool })
        })
    }

    /// The names of the database's collections, in ascending byte order
    /// ([`Db::collection_names`](crate::Db::collection_names)): a read of the latest published state, which takes no
    /// lock and runs on the caller's thread, as [`AsyncDb::collection`] does.
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
    /// block_on(db.create_collection_json(r#"{"name": "b", "fields": {}}"#))?;
    /// block_on(db.create_collection_json(r#"{"name": "a", "fields": {}}"#))?;
    /// assert_eq!(db.collection_names()?, ["a", "b"]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::collection_names`](crate::Db::collection_names).
    pub fn collection_names(&self) -> Result<Vec<String>> {
        self.db
            .as_ref()
            .ok_or_else(Error::closed)?
            .collection_names()
    }

    /// [`Db::drop_collection`](crate::Db::drop_collection) on the pool. Once started it runs to its end (the collection
    /// is dropped and the drop committed) even if the future is dropped.
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
    /// use persia::ErrorCode;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(persia::AsyncDb::open(dir.path().join("app.persia")))?;
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {}}"#))?;
    /// block_on(db.drop_collection("notes"))?;
    /// assert_eq!(block_on(notes.get("n1")).unwrap_err().code(), ErrorCode::NotFound);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::drop_collection`](crate::Db::drop_collection); [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the
    /// database closed before the job started.
    pub fn drop_collection(&self, name: impl Into<String>) -> JobFuture<()> {
        let name = name.into();
        self.submit(JobKind::DropCollection, move |inner, _| {
            inner.drop_collection(&name)
        })
    }

    /// [`Db::refresh`](crate::Db::refresh) on the pool: a read-only database shows what the writer has made durable
    /// since; a read-write one has nothing to do. Once started it runs to its end even if the future is dropped: the
    /// database shows the refreshed state, or keeps the one it had on an error.
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
    /// use persia::{AsyncDb, Db, OpenOptions};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("app.persia");
    /// let writer = Db::open(&path)?;
    /// let reader = block_on(AsyncDb::open_with(OpenOptions::new().read_only(true), &path))?;
    /// writer.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// assert!(reader.collection("notes").is_err()); // the state the reader opened
    /// block_on(reader.refresh())?;
    /// assert!(reader.collection("notes").is_ok());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::refresh`](crate::Db::refresh); [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the database closed
    /// before the job started.
    pub fn refresh(&self) -> JobFuture<()> {
        self.submit(JobKind::Refresh, |inner, _| inner.refresh())
    }

    /// [`Db::backup`](crate::Db::backup) to `dest` on the pool: a copy of the database at one commit, never replacing a
    /// file (SPEC §4.8). Once started it runs to its end even if the future is dropped: the backup is then complete at
    /// `dest` (or, on an error, cleaned up as [`Db::backup`](crate::Db::backup) says), never cut off halfway. A dropped
    /// future does not stop a long copy: to have no backup, drop the future before its job starts, or remove `dest`
    /// after.
    ///
    /// `dest` is read at the call, as the synchronous backup reads it: an empty or malformed one, or a URL of another
    /// scheme, is refused at once and nothing is queued, and a relative one names a place under the working directory
    /// at the call, not when the job runs.
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
    /// use persia::{AsyncDb, Db, Document};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(AsyncDb::open(dir.path().join("app.persia")))?;
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {}}"#))?;
    /// block_on(notes.put(Document::new("n1")))?;
    /// let dest = dir.path().join("backup.persia");
    /// block_on(db.backup(&dest))?;
    /// let copy = Db::open(&dest)?;
    /// assert!(copy.collection("notes")?.get("n1")?.is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::backup`](crate::Db::backup); [`ErrorCode::Io`](crate::ErrorCode::Io) if the working directory cannot be
    /// read to resolve a relative `dest`; [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the database closed before
    /// the job started.
    pub fn backup(&self, dest: impl AsRef<Path>) -> JobFuture<()> {
        let dest = match resolved(dest.as_ref()) {
            Ok(dest) => dest,
            Err(e) => return JobFuture::ready(Err(e)),
        };
        self.submit(JobKind::Backup, move |inner, _| inner.backup(&dest))
    }
}

/// `dest` as a path (a `file://` URL read, another scheme refused), made absolute against the working directory now.
fn resolved(dest: &Path) -> Result<PathBuf> {
    let path = crate::location::path_of(dest)?;
    std::path::absolute(&path).map_err(|source| {
        Error::with_source(
            ErrorCode::Io,
            format_args!("resolving the backup destination {}", path.display()),
            source,
        )
    })
}
