//! What a blob is stored with, and the blob of a document, as the facade has them (SPEC §9.1, §9.4, §9.6; ROADMAP
//! 10.1.15, ADR-0063 amended): the facade's own types, converted to the blob store's here, so no `persia-blob` type is
//! part of the public API.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use persia_blob::{BlobId, BlobRef, ChunkSize, UploadId};

use crate::internal::blob::PutBlobOptions as StoreOptions;
use crate::internal::documents::Blob as StoreBlob;
use crate::{Error, Result};

/// What a blob is stored with beside its bytes (SPEC §9.1, §9.6): for [`Db::put_blob`](crate::Db::put_blob) and
/// [`Blob::with_options`]. Start from [`PutBlobOptions::new`] (the defaults) and set what you need; the values are
/// checked when the blob is stored.
///
/// ```
/// use persia::PutBlobOptions;
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// # let db = persia::Db::open(dir.path().join("app.persia"))?;
/// let options = PutBlobOptions::new().mime("image/png").expected_size(4);
/// let blob = db.put_blob(&b"\x89PNG"[..], &options)?;
/// assert_eq!((blob.mime(), blob.size()), ("image/png", 4));
/// // Five bytes where four were promised: refused, nothing stored.
/// let err = db.put_blob(&b"\x89PNG!"[..], &options).unwrap_err();
/// assert_eq!(err.code(), persia::ErrorCode::InvalidArgument);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PutBlobOptions {
    mime: Option<String>,
    user_meta: BTreeMap<String, String>,
    expected_id: Option<BlobId>,
    expected_size: Option<u64>,
    chunk_size: Option<u32>,
    bind_budget: Option<Duration>,
    resume: Option<ResumeToken>,
}

impl Default for PutBlobOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl PutBlobOptions {
    /// The defaults: MIME type `application/octet-stream`, no user metadata, no expected id or size, 4 MiB chunks, a
    /// bind budget of 60 s, a new upload.
    ///
    /// ```
    /// assert_eq!(persia::PutBlobOptions::new(), persia::PutBlobOptions::default());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            mime: None,
            user_meta: BTreeMap::new(),
            expected_id: None,
            expected_size: None,
            chunk_size: None,
            bind_budget: None,
            resume: None,
        }
    }

    /// The blob's MIME type: 1..=255 bytes of printable ASCII (SPEC §9.1; another is `InvalidArgument` when the blob
    /// is stored).
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let blob = db.put_blob(&b"hi"[..], &persia::PutBlobOptions::new().mime("text/plain"))?;
    /// assert_eq!(blob.mime(), "text/plain");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn mime(mut self, mime: impl Into<String>) -> Self {
        self.mime = Some(mime.into());
        self
    }

    /// Adds `key` = `value` to the blob's user metadata, replacing a value `key` had (SPEC §9.1 limits). Searchable
    /// metadata belongs in document fields.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let options = persia::PutBlobOptions::new().user_meta("camera", "x100").user_meta("iso", "200");
    /// db.put_blob(&b"raw"[..], &options)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn user_meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.user_meta.insert(key.into(), value.into());
        self
    }

    /// The id the caller expects the bytes to hash to (SHA-256, ADR-0002); a mismatch is `InvalidArgument` and
    /// stores nothing.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let first = db.put_blob(&b"same"[..], &persia::PutBlobOptions::new())?;
    /// let again = persia::PutBlobOptions::new().expected_id(first.blob_id());
    /// assert_eq!(db.put_blob(&b"same"[..], &again)?, first);
    /// assert!(db.put_blob(&b"other"[..], &again).is_err());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn expected_id(mut self, id: BlobId) -> Self {
        self.expected_id = Some(id);
        self
    }

    /// The length in bytes the caller expects the whole blob to have; a mismatch is `InvalidArgument` and stores
    /// nothing. The reader is read at most one byte past it. A resume needs this or [`expected_id`](Self::expected_id).
    /// See [`PutBlobOptions`] for an example.
    #[must_use]
    pub fn expected_size(mut self, size: u64) -> Self {
        self.expected_size = Some(size);
        self
    }

    /// The chunk size in bytes to store a new blob with (SPEC §9.1, §9.2): 64 KiB to 64 MiB, 4 MiB by default; another is
    /// `InvalidArgument` when the blob is stored. A blob already stored keeps its own, and a resume uses its token's.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let bytes = vec![7_u8; 200 << 10];
    /// let blob = db.put_blob(&bytes[..], &persia::PutBlobOptions::new().chunk_size(64 << 10))?;
    /// assert_eq!(blob.size(), 200 << 10);
    /// let tiny = persia::PutBlobOptions::new().chunk_size(1024);
    /// assert_eq!(db.put_blob(&b"x"[..], &tiny).unwrap_err().code(), persia::ErrorCode::InvalidArgument);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn chunk_size(mut self, bytes: u32) -> Self {
        self.chunk_size = Some(bytes);
        self
    }

    /// How long a store whose outcome is unknown is retried, with backoff, before the call fails with `Transient`
    /// (storing again is safe: blobs are content-addressed); at least one attempt is made. 60 s by default; more
    /// than an hour is `InvalidArgument` when the blob is stored.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// let options = persia::PutBlobOptions::new().bind_budget(Duration::from_secs(5));
    /// # let _ = options;
    /// ```
    #[must_use]
    pub fn bind_budget(mut self, budget: Duration) -> Self {
        self.bind_budget = Some(budget);
        self
    }

    /// The upload to resume (SPEC §9.4): the token of an interrupted upload ([`Error::resume_token`]), with a reader
    /// that yields the blob from its start and [`expected_id`](Self::expected_id) or
    /// [`expected_size`](Self::expected_size) set. Without it (the default) a new upload starts. The local blob pack
    /// cannot resume: there, a token is `Unsupported` before any byte is read.
    ///
    /// ```
    /// use persia::{ErrorCode, PutBlobOptions, ResumeToken};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let token = ResumeToken::new(7, 64 << 10);
    /// let options = PutBlobOptions::new().resume(token).expected_size(2);
    /// assert_eq!(db.put_blob(&b"hi"[..], &options).unwrap_err().code(), ErrorCode::Unsupported);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn resume(mut self, token: ResumeToken) -> Self {
        self.resume = Some(token);
        self
    }

    /// The blob store's options for these.
    ///
    /// # Errors
    /// `InvalidArgument` for a chunk size, or a token's chunk size, that the encodings cannot hold (SPEC §9.1); the
    /// store checks the rest.
    pub(crate) fn to_store(&self) -> Result<StoreOptions> {
        let defaults = StoreOptions::default();
        let chunk_size = match self.chunk_size {
            Some(bytes) => ChunkSize::new(bytes).map_err(Error::wrap)?,
            None => defaults.chunk_size,
        };
        let resume = match self.resume {
            Some(token) => Some(token.to_store()?),
            None => None,
        };
        Ok(StoreOptions {
            mime: self.mime.clone().unwrap_or(defaults.mime),
            user_meta: self.user_meta.clone(),
            expected_id: self.expected_id,
            expected_size: self.expected_size,
            chunk_size,
            bind_budget: self.bind_budget.unwrap_or(defaults.bind_budget),
            resume,
        })
    }
}

/// The token of an upload that can be resumed (SPEC §9.4): the upload's id and the chunk size it started with. A
/// failed [`Db::put_blob`](crate::Db::put_blob) gives one ([`Error::resume_token`]) when its upload attempted a chunk
/// write; pass it back with [`PutBlobOptions::resume`]. Its two parts let a caller keep it across a restart.
///
/// ```
/// let token = persia::ResumeToken::new(42, 4 << 20);
/// assert_eq!((token.upload_id(), token.chunk_size()), (42, 4 << 20));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResumeToken {
    upload_id: u128,
    chunk_size: u32,
}

impl ResumeToken {
    /// The token of the upload `upload_id`, staged with chunks of `chunk_size` bytes. It is not checked here: a
    /// chunk size the encodings cannot hold is `InvalidArgument` when it is used, and one below the writer's floor
    /// (64 KiB) is refused by the store (SPEC §9.2).
    #[must_use]
    pub const fn new(upload_id: u128, chunk_size: u32) -> Self {
        Self {
            upload_id,
            chunk_size,
        }
    }

    /// The upload's id.
    #[must_use]
    pub const fn upload_id(&self) -> u128 {
        self.upload_id
    }

    /// The chunk size, in bytes, the upload started with: a resume stores the blob with it.
    #[must_use]
    pub const fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// The token as the blob store has it.
    fn to_store(self) -> Result<persia_blob::ResumeToken> {
        Ok(persia_blob::ResumeToken {
            upload_id: UploadId::new(self.upload_id),
            chunk_size: ChunkSize::from_encoding(self.chunk_size).map_err(Error::wrap)?,
        })
    }

    /// The facade's token for `token`.
    pub(crate) fn from_store(token: persia_blob::ResumeToken) -> Self {
        Self::new(token.upload_id.get(), token.chunk_size.get())
    }
}

/// The blob of a document (SPEC §9.5, §9.6), for [`Collection::put_with_blob`](crate::Collection::put_with_blob):
/// bytes to store with it, or a blob [`Db::put_blob`](crate::Db::put_blob) stored before. Bytes are stored, and
/// durable, before the document is logged; a stored blob must be in this database's store.
pub enum Blob<'r> {
    /// Bytes to store, from a reader ([`Blob::from_reader`]).
    Reader(Box<dyn Read + 'r>, PutBlobOptions),
    /// Bytes to store, from the file at the path ([`Blob::from_path`]).
    Path(PathBuf, PutBlobOptions),
    /// A blob this database stored before, by its reference.
    Stored(BlobRef),
}

impl<'r> Blob<'r> {
    /// The bytes `reader` yields, with the default [`PutBlobOptions`].
    ///
    /// ```
    /// use persia::{Blob, Document};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let files = db.create_collection_json(r#"{"name": "files", "fields": {}}"#)?;
    /// let blob = files.put_with_blob(&Document::new("hello"), Blob::from_reader(&b"hello"[..]))?;
    /// assert_eq!((blob.size(), blob.mime()), (5, "application/octet-stream"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn from_reader(reader: impl Read + 'r) -> Self {
        Self::Reader(Box::new(reader), PutBlobOptions::default())
    }

    /// The bytes of the file at `path`, with the default [`PutBlobOptions`]. The file is opened when the blob is
    /// stored; a missing one is `NotFound`, and nothing is logged.
    ///
    /// ```
    /// use persia::{Blob, Document};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let path = dir.path().join("report.txt");
    /// std::fs::write(&path, "quarterly numbers")?;
    /// let files = db.create_collection_json(r#"{"name": "files", "fields": {}}"#)?;
    /// let blob = files.put_with_blob(&Document::new("report"), Blob::from_path(&path))?;
    /// assert_eq!(db.get_blob_range(&blob, 0..9)?, b"quarterly");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into(), PutBlobOptions::default())
    }

    /// `options` instead of the defaults; no effect on [`Blob::Stored`], whose blob keeps what it was stored with.
    ///
    /// ```
    /// use persia::{Blob, Document, PutBlobOptions};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let files = db.create_collection_json(r#"{"name": "files", "fields": {}}"#)?;
    /// let text = PutBlobOptions::new().mime("text/plain");
    /// let blob = files.put_with_blob(&Document::new("a"), Blob::from_reader(&b"hi"[..]).with_options(text))?;
    /// assert_eq!(blob.mime(), "text/plain");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn with_options(self, options: PutBlobOptions) -> Self {
        match self {
            Self::Reader(reader, _) => Self::Reader(reader, options),
            Self::Path(path, _) => Self::Path(path, options),
            stored @ Self::Stored(_) => stored,
        }
    }

    /// The blob as the blob store's layer has it.
    ///
    /// # Errors
    /// As [`PutBlobOptions::to_store`].
    pub(crate) fn into_store(self) -> Result<StoreBlob<'r>> {
        Ok(match self {
            Self::Reader(reader, options) => StoreBlob::Reader(reader, options.to_store()?),
            Self::Path(path, options) => StoreBlob::Path(path, options.to_store()?),
            Self::Stored(blob_ref) => StoreBlob::Stored(blob_ref),
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    #[test]
    fn the_defaults_are_the_stores() {
        assert_eq!(
            PutBlobOptions::new().to_store().unwrap(),
            StoreOptions::default()
        );
    }

    #[test]
    fn every_option_reaches_the_store() {
        let id = persia_blob::blob_id_of(b"x");
        let options = PutBlobOptions::new()
            .mime("text/plain")
            .user_meta("a", "1")
            .user_meta("b", "2")
            .user_meta("a", "3")
            .expected_id(id)
            .expected_size(9)
            .chunk_size(ChunkSize::MIN)
            .bind_budget(Duration::from_secs(5))
            .resume(ResumeToken::new(u128::MAX, ChunkSize::MIN * 2));
        let store = options.to_store().unwrap();
        assert_eq!(store.mime, "text/plain");
        assert_eq!(
            store.user_meta,
            BTreeMap::from([("a".into(), "3".into()), ("b".into(), "2".into())])
        );
        assert_eq!(store.expected_id, Some(id));
        assert_eq!(store.expected_size, Some(9));
        assert_eq!(store.chunk_size.get(), ChunkSize::MIN);
        assert_eq!(store.bind_budget, Duration::from_secs(5));
        let token = store.resume.unwrap();
        assert_eq!(token.upload_id.get(), u128::MAX);
        assert_eq!(token.chunk_size.get(), ChunkSize::MIN * 2);
        assert_eq!(
            ResumeToken::from_store(token),
            ResumeToken::new(u128::MAX, ChunkSize::MIN * 2)
        );
    }

    /// The writer's floor applies to a new blob's chunk size; a token's chunk size only has to be encodable here (the
    /// store refuses one below the floor, SPEC §9.4).
    #[test]
    fn chunk_sizes_are_checked_on_conversion() {
        for bytes in [0, ChunkSize::MIN - 1, ChunkSize::MAX + 1] {
            let err = PutBlobOptions::new()
                .chunk_size(bytes)
                .to_store()
                .unwrap_err();
            assert_eq!(err.code(), ErrorCode::InvalidArgument, "{bytes}");
        }
        assert!(
            PutBlobOptions::new()
                .chunk_size(ChunkSize::MAX)
                .to_store()
                .is_ok()
        );
        for bytes in [0, ChunkSize::MAX + 1] {
            let options = PutBlobOptions::new().resume(ResumeToken::new(1, bytes));
            assert_eq!(
                options.to_store().unwrap_err().code(),
                ErrorCode::InvalidArgument,
                "{bytes}"
            );
        }
        let below_floor = PutBlobOptions::new().resume(ResumeToken::new(1, 1));
        assert_eq!(
            below_floor
                .to_store()
                .unwrap()
                .resume
                .unwrap()
                .chunk_size
                .get(),
            1
        );
    }

    #[test]
    fn a_blob_converts_with_its_options() {
        let text = PutBlobOptions::new().mime("text/plain");
        let reader = Blob::from_reader(&b"hi"[..]).with_options(text.clone());
        let StoreBlob::Reader(_, options) = reader.into_store().unwrap() else {
            panic!("a reader stays a reader");
        };
        assert_eq!(options.mime, "text/plain");
        let path = Blob::from_path("a.txt").with_options(text);
        assert!(format!("{path:?}").starts_with("Path(\"a.txt\", PutBlobOptions"));
        let StoreBlob::Path(path, options) = path.into_store().unwrap() else {
            panic!("a path stays a path");
        };
        assert_eq!(
            (path, options.mime.as_str()),
            (PathBuf::from("a.txt"), "text/plain")
        );
        let blob_ref = BlobRef::new(
            persia_blob::blob_id_of(b"x"),
            1,
            "text/plain",
            ChunkSize::MIN,
            BTreeMap::new(),
        )
        .unwrap();
        let stored = Blob::Stored(blob_ref.clone()).with_options(PutBlobOptions::new().mime("a/b"));
        let StoreBlob::Stored(kept) = stored.into_store().unwrap() else {
            panic!("a stored blob stays stored");
        };
        assert_eq!(kept, blob_ref);
        let bad = Blob::from_reader(&b""[..]).with_options(PutBlobOptions::new().chunk_size(1));
        assert_eq!(
            bad.into_store().unwrap_err().code(),
            ErrorCode::InvalidArgument
        );
    }
}
