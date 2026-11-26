//! Blobs of the embedded API (SPEC §9.5, §9.6; ROADMAP 10.1.3).

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::ops::Range;
use std::sync::Arc;

use persia_blob::BlobRef;

use crate::db::{Db, Inner};
use crate::internal::documents::{Blob as StoreBlob, Error as DocumentsError};
use crate::internal::pins::Pins;
use crate::{Error, ErrorCode, Result};

// The facade's blob options and blob of a document (ROADMAP 10.1.15).
mod put;

pub use put::{Blob, PutBlobOptions, ResumeToken};

/// How many bytes a [`BlobReader`] reads at a time.
const WINDOW: u64 = 4 << 20;

impl Inner {
    /// Stores the bytes of `blob` (not [`Blob::Stored`]) in the store reads use, without the writer mutex, and pins
    /// it until the returned pins drop.
    pub(crate) fn store_blob(&self, blob: Blob<'_>) -> Result<(BlobRef, Pins)> {
        self.check_writable("store a blob")?;
        let blob = blob.into_store()?;
        self.with_blobs(|blobs| {
            let stored = match blob {
                StoreBlob::Reader(reader, options) => blobs.put_blob(reader, &options),
                StoreBlob::Path(path, options) => match File::open(&path) {
                    Ok(file) => blobs.put_blob(file, &options),
                    Err(source) => return Err(Error::wrap(DocumentsError::Open { path, source })),
                },
                StoreBlob::Stored(blob_ref) => {
                    return match blobs.pin_contained(&blob_ref).map_err(Error::wrap)? {
                        Some(pins) => Ok((blob_ref, pins)),
                        None => Err(not_stored(&blob_ref)),
                    };
                }
            };
            stored.map_err(Error::wrap)
        })
    }
}

fn not_stored(blob_ref: &BlobRef) -> Error {
    Error::wrap(DocumentsError::BlobNotStored {
        blob_id: blob_ref.blob_id(),
    })
}

impl Db {
    /// Stores the bytes `reader` yields as a blob (SPEC §9.6) and returns its reference, once the blob is durable.
    /// A blob no document references is collected by a later garbage collection: put the reference in a document
    /// with [`Collection::put_with_blob`](crate::Collection::put_with_blob) and [`Blob::Stored`], or store the bytes
    /// and the document in one call with [`Blob::from_reader`].
    ///
    /// `reader` runs while the database is held open for the upload: it must not call back into this `Db` (to close it,
    /// or to store another blob), or that call may wait for the upload, and the upload for it.
    ///
    /// ```
    /// use persia::PutBlobOptions;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let options = PutBlobOptions::new().mime("text/plain");
    /// let blob = db.put_blob(&b"hello, blobs"[..], &options)?;
    /// assert_eq!((blob.size(), blob.mime()), (12, "text/plain"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`] for options SPEC §9.1 refuses, or bytes that do not match `expected_id` or
    /// `expected_size`; [`ErrorCode::Io`] when the reader or the pack fails; [`ErrorCode::Transient`] when the outcome
    /// is unknown (storing again is safe: blobs are content-addressed); [`ErrorCode::ReadOnly`],
    /// [`ErrorCode::Closed`].
    pub fn put_blob(&self, reader: impl Read, options: &PutBlobOptions) -> Result<BlobRef> {
        let (blob_ref, _pins) = self
            .inner
            .store_blob(Blob::Reader(Box::new(reader), options.clone()))?;
        Ok(blob_ref)
    }

    /// A reader of the whole blob `blob_ref` refers to (SPEC §9.6). It reads a window at a time, each window checked;
    /// the blob stays pinned (no garbage collection takes it) while the reader lives.
    ///
    /// ```
    /// use std::io::Read;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let blob = db.put_blob(&b"streamed"[..], &persia::PutBlobOptions::default())?;
    /// let mut text = String::new();
    /// db.get_blob(&blob)?.read_to_string(&mut text)?;
    /// assert_eq!(text, "streamed");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::NotFound`] if this database's store does not hold the blob; [`ErrorCode::InvalidArgument`] if it
    /// holds the blob with another size or chunk size than `blob_ref` gives (the reference is the caller's input: the
    /// same `blob_id` may have another chunk size in another database, SPEC §9.6); [`ErrorCode::Corrupt`] if what it
    /// holds under the id is damaged or names another blob; [`ErrorCode::Closed`]. Errors while reading come from
    /// [`Read::read`] as [`io::Error`]s whose inner error is an [`Error`].
    pub fn get_blob(&self, blob_ref: &BlobRef) -> Result<BlobReader> {
        let pins = self.inner.with_blobs(|blobs| {
            blobs
                .pin_contained(blob_ref)
                .map_err(Error::wrap)?
                .ok_or_else(|| not_stored(blob_ref))
        })?;
        Ok(BlobReader {
            db: Arc::clone(&self.inner),
            blob_ref: blob_ref.clone(),
            _pins: pins,
            offset: 0,
            buf: Vec::new(),
            pos: 0,
        })
    }

    /// Bytes `range` of the blob `blob_ref` refers to (SPEC §9.6).
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let blob = db.put_blob(&b"0123456789"[..], &persia::PutBlobOptions::default())?;
    /// assert_eq!(db.get_blob_range(&blob, 3..6)?, b"345");
    /// assert_eq!(db.get_blob_range(&blob, 3..11).unwrap_err().code(), persia::ErrorCode::InvalidArgument);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`] for a range outside the blob; then as [`Db::get_blob`].
    pub fn get_blob_range(&self, blob_ref: &BlobRef, range: Range<u64>) -> Result<Vec<u8>> {
        self.inner
            .with_blobs(|blobs| blobs.get_blob_range(blob_ref, range).map_err(Error::wrap))
    }
}

/// A blob being read ([`Db::get_blob`]): a [`Read`] that owns a share of the database (no lifetime), so it can
/// move to another thread. Once the database is closed its reads fail with [`ErrorCode::Closed`].
pub struct BlobReader {
    db: Arc<Inner>,
    blob_ref: BlobRef,
    /// The blob and its upload, pinned while the reader lives (SPEC §9.5).
    _pins: Pins,
    /// Where the next window starts.
    offset: u64,
    buf: Vec<u8>,
    pos: usize,
}

impl fmt::Debug for BlobReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobReader")
            .field("blob_ref", &self.blob_ref)
            .field("offset", &self.offset)
            .finish_non_exhaustive()
    }
}

impl BlobReader {
    /// The reference of the blob being read.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let blob = db.put_blob(&b"x"[..], &persia::PutBlobOptions::default())?;
    /// assert_eq!(db.get_blob(&blob)?.blob_ref(), &blob);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn blob_ref(&self) -> &BlobRef {
        &self.blob_ref
    }
}

impl Read for BlobReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // Closed first, also with bytes in the window: a reader serves nothing once its database is closed.
        self.db.check_open().map_err(io::Error::other)?;
        if self.pos >= self.buf.len() {
            let size = self.blob_ref.size();
            if self.offset >= size || out.is_empty() {
                return Ok(0);
            }
            let end = self.offset.saturating_add(WINDOW).min(size);
            let (blob_ref, range) = (&self.blob_ref, self.offset..end);
            self.buf = self
                .db
                .with_blobs(|blobs| blobs.get_blob_range(blob_ref, range).map_err(Error::wrap))
                .map_err(io::Error::other)?;
            self.offset = end;
            self.pos = 0;
        }
        let rest = self.buf.get(self.pos..).unwrap_or_default();
        let n = rest.len().min(out.len());
        if let (Some(to), Some(from)) = (out.get_mut(..n), rest.get(..n)) {
            to.copy_from_slice(from);
        }
        self.pos = self.pos.saturating_add(n);
        Ok(n)
    }
}

/// Only for the `ErrorCode` links of this module's docs.
const _: ErrorCode = ErrorCode::NotFound;
