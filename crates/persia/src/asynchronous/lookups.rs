//! The calls of [`Db`](crate::Db) and [`Collection`](crate::Collection) that do no I/O of their own, as
//! synchronous methods of the async handles (ROADMAP 10.1.26; ADR-0064, amended 2026-11-27): a job would only add a
//! thread hop, as for [`AsyncDb::collection`]. They take no writer mutex, but on a read-only database a lookup in the
//! published state can wait for a running [`AsyncDb::refresh`] to swap its state in (ADR-0064 amendment, item 14).

use std::path::Path;

use serde::de::DeserializeOwned;

use super::{AsyncCollection, AsyncDb};
use crate::{BlockCacheStats, CollectionSchema, Document, Durability, Result};

impl AsyncDb {
    /// The path the database was opened at ([`Db::path`](crate::Db::path)).
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
    /// let path = dir.path().join("app.persia");
    /// let db = block_on(persia::AsyncDb::open(&path))?;
    /// assert_eq!(db.path(), path);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn path(&self) -> &Path {
        self.shared.path()
    }

    /// Whether the database was opened read-only ([`Db::is_read_only`](crate::Db::is_read_only)).
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
    /// assert!(!db.is_read_only());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.shared.is_read_only()
    }

    /// How the database's block cache is used ([`Db::block_cache_stats`](crate::Db::block_cache_stats)): counters read
    /// on the caller's thread.
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
    /// let options = persia::OpenOptions::new().block_cache_bytes(1 << 20);
    /// let db = block_on(persia::AsyncDb::open_with(options, dir.path().join("app.persia")))?;
    /// assert_eq!(db.block_cache_stats()?.capacity, 1 << 20);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::block_cache_stats`](crate::Db::block_cache_stats).
    pub fn block_cache_stats(&self) -> Result<BlockCacheStats> {
        self.shared.block_cache_stats()
    }
}

impl AsyncCollection {
    /// The collection's name ([`Collection::name`](crate::Collection::name)).
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
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {}}"#))?;
    /// assert_eq!(notes.name(), "notes");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        self.collection.name()
    }

    /// The collection's current schema ([`Collection::schema`](crate::Collection::schema)), from the latest published
    /// state.
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
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#))?;
    /// assert!(notes.schema()?.field("body").is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::schema`](crate::Collection::schema).
    pub fn schema(&self) -> Result<CollectionSchema> {
        self.collection.schema()
    }

    /// This handle, its writes at `durability` ([`Collection::with_durability`](crate::Collection::with_durability)):
    /// the other handles on the collection keep theirs.
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
    /// use persia::{Document, Durability};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(persia::AsyncDb::open(dir.path().join("app.persia")))?;
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {}}"#))?;
    /// let fast = notes.clone().with_durability(Durability::Buffered);
    /// assert_eq!((fast.durability(), notes.durability()), (Durability::Buffered, Durability::Fsync));
    /// block_on(fast.put(Document::new("n1")))?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn with_durability(self, durability: Durability) -> Self {
        Self {
            collection: self.collection.with_durability(durability),
            pool: self.pool,
        }
    }

    /// The durability this handle's writes reach before their futures resolve
    /// ([`Collection::durability`](crate::Collection::durability)).
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
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {}}"#))?;
    /// assert_eq!(notes.durability(), persia::Durability::Fsync);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn durability(&self) -> Durability {
        self.collection.durability()
    }

    /// `doc`, a document of this collection at hand, as a `T`
    /// ([`Collection::to_struct`](crate::Collection::to_struct)): its JSON under the current schema, built on the
    /// caller's thread (no read of the database).
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
    /// use std::collections::BTreeMap;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(persia::AsyncDb::open(dir.path().join("app.persia")))?;
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#))?;
    /// block_on(notes.put_json(r#"{"id": "n1", "body": "hello"}"#))?;
    /// let doc = block_on(notes.get("n1"))?.ok_or("missing")?;
    /// let read: BTreeMap<String, String> = notes.to_struct(&doc)?;
    /// assert_eq!(read["body"], "hello");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::to_struct`](crate::Collection::to_struct).
    pub fn to_struct<T: DeserializeOwned>(&self, doc: &Document) -> Result<T> {
        self.collection.to_struct(doc)
    }
}
