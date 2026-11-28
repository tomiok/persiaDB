//! The JSON and typed calls of a collection on the database's pool (ROADMAP 10.1.26; ADR-0064, amended 2026-11-27).
//!
//! Text is taken owned and read on the pool's thread. A typed value is written as JSON on the caller's thread, when
//! `put_struct` is called, so it is borrowed only for the call (no `Send + 'static` bound on it); a value read back
//! is built on the pool's thread, so its type is `DeserializeOwned + Send + 'static`.

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{AsyncCollection, JobFuture, JobKind, check_cancelled};
use crate::SearchResponse;

impl AsyncCollection {
    /// [`Collection::put_json`](crate::Collection::put_json) on the pool: the JSON text is read against the
    /// collection's schema on the pool's thread. Once started it runs to its end, even if the future is dropped.
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
    /// block_on(notes.put_json(r#"{"id": "n1", "body": "hello"}"#))?;
    /// assert!(block_on(notes.get("n1"))?.is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::put_json`](crate::Collection::put_json); [`ErrorCode::Closed`](crate::ErrorCode::Closed) if
    /// the database closed before the job started.
    pub fn put_json(&self, json: impl Into<String>) -> JobFuture<()> {
        let collection = self.collection.clone();
        let json = json.into();
        self.pool
            .submit(JobKind::PutJson, move |_| collection.put_json(&json))
    }

    /// [`Collection::get_json`](crate::Collection::get_json) on the pool. Dropping the future of a get that has
    /// started sets its cancellation token, as for [`get`](Self::get).
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
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {}, "strict": false}"#))?;
    /// block_on(notes.put_json(r#"{"id": "a", "mood": "fine"}"#))?;
    /// assert_eq!(block_on(notes.get_json("a"))?.as_deref(), Some(r#"{"id":"a","mood":"fine"}"#));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::get_json`](crate::Collection::get_json); [`ErrorCode::Closed`](crate::ErrorCode::Closed) if
    /// the database closed before the job started.
    pub fn get_json(&self, id: impl Into<String>) -> JobFuture<Option<String>> {
        let collection = self.collection.clone();
        let id = id.into();
        self.pool.submit(JobKind::GetJson, move |token| {
            check_cancelled(token, "get")?;
            collection.get_json(&id)
        })
    }

    /// [`Collection::put_struct`](crate::Collection::put_struct) on the pool: `value` is written as JSON now, on the
    /// caller's thread (a value that cannot be is refused at once, and nothing is queued), and that JSON is put on the
    /// pool as [`put_json`](Self::put_json) puts it. Once started it runs to its end, even if the future is dropped.
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
    /// use serde::{Deserialize, Serialize};
    ///
    /// #[derive(Serialize, Deserialize, PartialEq, Debug)]
    /// struct Product {
    ///     id: String,
    ///     price: f64,
    /// }
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(persia::AsyncDb::open(dir.path().join("app.persia")))?;
    /// let products = block_on(db.create_collection_json(
    ///     r#"{"name": "products", "fields": {"price": {"type": "f64", "stored": true}}}"#,
    /// ))?;
    /// let lamp = Product { id: "p1".into(), price: 19.5 };
    /// block_on(products.put_struct(&lamp))?;
    /// assert_eq!(block_on(products.get_struct::<Product>("p1"))?, Some(lamp));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::put_struct`](crate::Collection::put_struct);
    /// [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the database closed before the job started.
    pub fn put_struct<T: Serialize + ?Sized>(&self, value: &T) -> JobFuture<()> {
        match crate::typed::to_json(value) {
            Ok(json) => self.put_json(json),
            Err(e) => JobFuture::ready(Err(e)),
        }
    }

    /// [`Collection::get_struct`](crate::Collection::get_struct) on the pool: the document is read and built as a `T`
    /// on the pool's thread. Dropping the future of a get that has started sets its cancellation token, as for
    /// [`get`](Self::get).
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
    /// let notes = block_on(db.create_collection_json(r#"{"name": "notes", "fields": {}, "strict": false}"#))?;
    /// block_on(notes.put_json(r#"{"id": "n1", "mood": "fine"}"#))?;
    /// let read: BTreeMap<String, String> = block_on(notes.get_struct("n1"))?.ok_or("missing")?;
    /// assert_eq!(read["mood"], "fine");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::get_struct`](crate::Collection::get_struct);
    /// [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the database closed before the job started.
    pub fn get_struct<T>(&self, id: impl Into<String>) -> JobFuture<Option<T>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let collection = self.collection.clone();
        let id = id.into();
        self.pool.submit(JobKind::GetJson, move |token| {
            check_cancelled(token, "get")?;
            collection.get_struct(&id)
        })
    }

    /// [`Collection::search_json`](crate::Collection::search_json) on the pool: the request JSON is read on the pool's
    /// thread. Dropping the future of a search that has started sets its cancellation token: the search stops soon
    /// after (SPEC §7.3) with [`ErrorCode::Cancelled`](crate::ErrorCode::Cancelled), which nobody receives. A
    /// `timeout_ms` runs from when the job starts on the pool, not while it waits in the queue.
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
    /// block_on(notes.put_json(r#"{"id": "n1", "body": "hello world"}"#))?;
    /// let response = block_on(notes.search_json(r#"{"q": "hello", "limit": 5}"#))?;
    /// assert_eq!(response.hits[0].document.id(), "n1");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::search_json`](crate::Collection::search_json);
    /// [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the database closed before the job started.
    pub fn search_json(&self, json: impl Into<String>) -> JobFuture<SearchResponse> {
        let collection = self.collection.clone();
        let json = json.into();
        self.pool.submit(JobKind::SearchJson, move |token| {
            collection.search_json_cancellable(&json, token.cloned())
        })
    }
}
