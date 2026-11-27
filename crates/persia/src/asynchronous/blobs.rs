//! Blobs on the database's pool (ROADMAP 10.1.26; ADR-0064, amended 2026-11-27).
//!
//! An upload reads a caller's reader on a pool thread, so the reader is owned (`Read + Send + 'static`): a job on
//! another thread cannot borrow one. An upload has no point to stop at that would leave nothing behind (its staged
//! chunks would wait for a resume or a GC), so dropping the future of one that has started lets it run to its end:
//! the blob is stored whole, and a blob no document references is collected by a later GC. There is no async
//! [`Db::get_blob`](crate::Db::get_blob): its [`BlobReader`](crate::BlobReader) reads on the caller's thread, so
//! an async program reads a blob by ranges with [`AsyncDb::get_blob_range`].

use std::io::Read;
use std::ops::Range;

use super::{AsyncCollection, AsyncDb, JobFuture, JobKind, check_cancelled};
use crate::{Blob, BlobRef, Document, PutBlobOptions};

impl AsyncDb {
    /// [`Db::put_blob`](crate::Db::put_blob) on the pool: stores the bytes `reader` yields as a blob and resolves to
    /// its reference once the blob is durable. The pool thread reads `reader`, which must not call back into this
    /// database (or that call may wait for the upload, and the upload for it). Once started it runs to its end even if
    /// the future is dropped: the blob is stored, and a later garbage collection takes it if no document references
    /// it. Bytes in a file: pass the opened [`File`](std::fs::File).
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
    /// use persia::{AsyncDb, PutBlobOptions};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(AsyncDb::open(dir.path().join("app.persia")))?;
    /// let options = PutBlobOptions::new().mime("text/plain");
    /// let blob = block_on(db.put_blob(std::io::Cursor::new(b"hello, blobs".to_vec()), options))?;
    /// assert_eq!((blob.size(), blob.mime()), (12, "text/plain"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::put_blob`](crate::Db::put_blob); [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the database
    /// closed before the job started.
    pub fn put_blob<R>(&self, reader: R, options: PutBlobOptions) -> JobFuture<BlobRef>
    where
        R: Read + Send + 'static,
    {
        self.submit(JobKind::PutBlob, move |inner, _| {
            let (blob_ref, _pins) = inner.store_blob(Blob::Reader(Box::new(reader), options))?;
            Ok(blob_ref)
        })
    }

    /// [`Db::get_blob_range`](crate::Db::get_blob_range) on the pool: bytes `range` of the blob `blob_ref` refers to.
    /// Dropping the future of a read that has started sets its cancellation token: a read that has not started
    /// reading stops with [`ErrorCode::Cancelled`](crate::ErrorCode::Cancelled) (which nobody receives); one that
    /// has runs to its end and its bytes are discarded.
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
    /// let blob = block_on(db.put_blob(&b"0123456789"[..], persia::PutBlobOptions::new()))?;
    /// assert_eq!(block_on(db.get_blob_range(blob, 3..6))?, b"345");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::get_blob_range`](crate::Db::get_blob_range); [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the
    /// database closed before the job started.
    pub fn get_blob_range(&self, blob_ref: BlobRef, range: Range<u64>) -> JobFuture<Vec<u8>> {
        self.submit(JobKind::GetBlobRange, move |inner, token| {
            check_cancelled(token, "blob read")?;
            inner.get_blob_range(&blob_ref, range)
        })
    }
}

impl AsyncCollection {
    /// [`Collection::put_with_blob`](crate::Collection::put_with_blob) on the pool, with the bytes `reader` yields as
    /// the document's blob, stored with `options`: the blob is stored and durable before the document is logged, and
    /// the future resolves to the reference the document carries. The reader is owned and read on a pool thread, as
    /// for [`AsyncDb::put_blob`]. A blob already stored is not passed here: give the document its reference
    /// ([`Document::set_blob_ref`]) and [`put`](Self::put) it. Once started it runs to its end even if the future is
    /// dropped: the blob is stored and the document put.
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
    /// use persia::{Document, PutBlobOptions};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = block_on(persia::AsyncDb::open(dir.path().join("app.persia")))?;
    /// let files = block_on(db.create_collection_json(r#"{"name": "files", "fields": {}}"#))?;
    /// let blob = block_on(files.put_with_blob(Document::new("readme"), &b"# Hello"[..], PutBlobOptions::new()))?;
    /// assert_eq!(block_on(files.get("readme"))?.and_then(|d| d.blob_ref().cloned()), Some(blob.clone()));
    /// assert_eq!(block_on(db.get_blob_range(blob, 2..7))?, b"Hello");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Collection::put_with_blob`](crate::Collection::put_with_blob);
    /// [`ErrorCode::Closed`](crate::ErrorCode::Closed) if the database closed before the job started.
    pub fn put_with_blob<R>(
        &self,
        doc: Document,
        reader: R,
        options: PutBlobOptions,
    ) -> JobFuture<BlobRef>
    where
        R: Read + Send + 'static,
    {
        let collection = self.collection.clone();
        self.pool.submit(JobKind::PutWithBlob, move |_| {
            collection.put_with_blob(&doc, Blob::Reader(Box::new(reader), options))
        })
    }
}
