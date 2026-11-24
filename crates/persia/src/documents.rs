//! Writing and reading documents through a [`Collection`] (SPEC §6.4, §8.1, §8.2, §9.6; ROADMAP 10.1.3).

use persia_blob::BlobRef;
use persia_engine::document::Document;
use persia_engine::wal::Durability;

use crate::collection::Collection;
use crate::db::Db;
use crate::internal::documents::Blob;
use crate::{Error, Result};

impl Collection {
    /// This handle, writing at `durability` (SPEC §8.2.1) instead of the database's
    /// ([`OpenOptions::durability`](crate::OpenOptions::durability)).
    ///
    /// ```
    /// use persia::Durability;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let logs = db.create_collection_json(r#"{"name": "logs", "fields": {}, "strict": false}"#)?.with_durability(Durability::Async);
    /// assert_eq!(logs.durability(), Durability::Async);
    /// logs.put_json(r#"{"id": "1", "line": "started"}"#)?; // in the file when this returns, not synced
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = Some(durability);
        self
    }

    /// The durability this handle's writes reach before they return.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// assert_eq!(notes.durability(), persia::Durability::Fsync); // the database's default
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn durability(&self) -> Durability {
        self.durability.unwrap_or_else(|| self.db.durability())
    }

    /// Adds `doc`, or replaces the document with its id (SPEC §8.1, §8.3), at this handle's
    /// [`durability`](Self::durability). Every read of the collection sees it once this returns
    /// (read-your-writes, SPEC §8.4); it is committed in the background or by [`Db::commit`].
    ///
    /// A document that carries a blob reference ([`Document::set_blob_ref`]) is put as
    /// [`put_with_blob`](Self::put_with_blob) with [`Blob::Stored`]: the blob must be in this database's store.
    ///
    /// ```
    /// use persia::{Document, FieldKind, SchemaBuilder, TextOptions, Value};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let schema = SchemaBuilder::new("notes").field("body", FieldKind::Text(TextOptions::default())).build()?;
    /// let body = schema.field("body").map(|f| f.id()).ok_or("no body")?;
    /// let notes = db.create_collection(schema)?;
    /// notes.put(&Document::new("n1").with(body, Value::String("hello".into())))?;
    /// let read = notes.get("n1")?.ok_or("missing")?;
    /// assert_eq!(read.get(body), Some(&Value::String("hello".into())));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`](crate::ErrorCode::InvalidArgument) for a document that breaks a rule of SPEC
    /// §6.4.1 ([`ErrorCode::Limit`](crate::ErrorCode::Limit) past a limit); [`ErrorCode::NotFound`](crate::ErrorCode::NotFound)
    /// if the collection was dropped, or for a blob reference this database's store does not hold;
    /// [`ErrorCode::ReadOnly`](crate::ErrorCode::ReadOnly), [`ErrorCode::Closed`](crate::ErrorCode::Closed);
    /// [`ErrorCode::Io`](crate::ErrorCode::Io) if the log cannot be written, after which every write fails with
    /// [`ErrorCode::Poisoned`](crate::ErrorCode::Poisoned).
    pub fn put(&self, doc: &Document) -> Result<()> {
        let durability = self.durability();
        self.db.write("put a document", |db| {
            db.put_document(&self.name, doc, durability)
                .map_err(Error::wrap)
        })
    }

    /// [`put`](Self::put) of every document of `docs`, in order, reaching the durability once for all of them (one
    /// sync for [`Durability::Fsync`], SPEC §8.2.1). A reader sees none of the batch until all of it is durable.
    ///
    /// ```
    /// use persia::Document;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// notes.put_batch(&[Document::new("a"), Document::new("b")])?;
    /// assert!(notes.get("a")?.is_some() && notes.get("b")?.is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`put`](Self::put). A rejected document ends the batch: the documents before it are put, it and the ones
    /// after are not. A blob reference the store does not hold rejects the whole batch before anything is logged.
    pub fn put_batch(&self, docs: &[Document]) -> Result<()> {
        let durability = self.durability();
        self.db.write("put documents", |db| {
            db.put_batch(&self.name, docs, durability)
                .map_err(Error::wrap)
        })
    }

    /// [`put`](Self::put) of the document the JSON text `json` holds (SPEC §6.4.1), read with the collection's
    /// schema.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let products = db.create_collection_json(
    ///     r#"{"name": "products", "fields": {"title": {"type": "text"}, "price": {"type": "f64", "stored": true}}}"#,
    /// )?;
    /// products.put_json(r#"{"id": "p1", "title": "Lamp", "price": 19.5}"#)?;
    /// assert_eq!(products.get_json("p1")?.as_deref(), Some(r#"{"id":"p1","price":19.5,"title":"Lamp"}"#));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`](crate::ErrorCode::InvalidArgument) for text that is not a valid document of
    /// the schema; then as [`put`](Self::put).
    pub fn put_json(&self, json: &str) -> Result<()> {
        let schema = self.schema()?;
        let doc = Document::from_json(json, &schema).map_err(Error::wrap)?;
        self.put(&doc)
    }

    /// [`put`](Self::put) of `doc` with `blob` as its blob (SPEC §9.5, §9.6): bytes to store
    /// ([`Blob::from_reader`], [`Blob::from_path`]), which are stored and durable before the document is logged, or a
    /// [`Blob::Stored`] reference [`Db::put_blob`] returned, which must be in this database's store. Returns the
    /// reference the document carries. The bytes are read before the writer is taken, so a large blob does not hold
    /// up other writes.
    ///
    /// ```
    /// use persia::{Blob, Document};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let files = db.create_collection_json(r#"{"name": "files", "fields": {}}"#)?;
    /// let blob = files.put_with_blob(&Document::new("readme"), Blob::from_reader(&b"# Hello"[..]))?;
    /// assert_eq!(files.get("readme")?.and_then(|d| d.blob_ref().cloned()), Some(blob.clone()));
    /// assert_eq!(db.get_blob_range(&blob, 2..7)?, b"Hello");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::put_blob`] for bytes (nothing is logged); [`ErrorCode::NotFound`](crate::ErrorCode::NotFound) for a
    /// reference the store does not hold (nothing is logged); then as [`put`](Self::put).
    pub fn put_with_blob(&self, doc: &Document, blob: Blob<'_>) -> Result<BlobRef> {
        let durability = self.durability();
        self.db.check_writable("put a document")?;
        // Stored first, outside the writer mutex (ADR-0065 item 1); its pins hold it until the document is logged.
        let (blob_ref, _pins) = match blob {
            Blob::Stored(blob_ref) => (blob_ref, None),
            bytes => {
                let (blob_ref, pins) = self.db.store_blob(bytes)?;
                (blob_ref, Some(pins))
            }
        };
        self.db.write("put a document", |db| {
            db.put(&self.name, doc, Blob::Stored(blob_ref.clone()), durability)
                .map_err(Error::wrap)
        })?;
        Ok(blob_ref)
    }

    /// Deletes the document with id `id` (SPEC §8.1, §8.3), at this handle's durability. Deleting an id the
    /// collection does not have is not an error.
    ///
    /// ```
    /// use persia::Document;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// notes.put(&Document::new("a"))?;
    /// notes.delete("a")?;
    /// assert!(notes.get("a")?.is_none());
    /// notes.delete("never-there")?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`put`](Self::put).
    pub fn delete(&self, id: &str) -> Result<()> {
        let durability = self.durability();
        self.db.write("delete a document", |db| {
            db.log_mut()
                .delete(&self.name, id, durability)
                .map_err(Error::wrap)
        })
    }

    /// The document with id `id`, as the latest published state of the database shows it: every write that
    /// returned, committed or not (SPEC §8.4). `None` if the collection has no such document.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// assert!(notes.get("missing")?.is_none());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::NotFound`](crate::ErrorCode::NotFound) if the collection was dropped;
    /// [`ErrorCode::Corrupt`](crate::ErrorCode::Corrupt) if the segment holding it is damaged;
    /// [`ErrorCode::Closed`](crate::ErrorCode::Closed).
    pub fn get(&self, id: &str) -> Result<Option<Document>> {
        self.read(|c| c.get(id).map_err(Error::wrap))
    }

    /// [`get`](Self::get), as JSON text (SPEC §6.4.1, "Output").
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {}, "strict": false}"#)?;
    /// notes.put_json(r#"{"id": "a", "mood": "fine"}"#)?;
    /// assert_eq!(notes.get_json("a")?.as_deref(), Some(r#"{"id":"a","mood":"fine"}"#));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`get`](Self::get).
    pub fn get_json(&self, id: &str) -> Result<Option<String>> {
        self.read(|c| match c.get(id).map_err(Error::wrap)? {
            None => Ok(None),
            Some(doc) => doc.to_json(c.schema()).map(Some).map_err(Error::wrap),
        })
    }

    /// [`Db::commit`]: commits every collection of the database, not only this one.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// notes.put(&persia::Document::new("a"))?;
    /// notes.commit()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// As [`Db::commit`].
    pub fn commit(&self) -> Result<()> {
        self.db.commit()
    }
}

impl Db {
    /// Flushes every collection's unflushed writes into segments and commits them (SPEC §4.5, §8.2): an open after
    /// a crash then finds them without replaying the log. Writes are durable at their own durability whether or not
    /// they are committed; the database also commits in the background ([`OpenOptions`](crate::OpenOptions)'s
    /// `idle_flush`, `commit_interval`), and at [`Db::close`].
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?.put(&persia::Document::new("a"))?;
    /// db.commit()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::ReadOnly`](crate::ErrorCode::ReadOnly), [`ErrorCode::Closed`](crate::ErrorCode::Closed);
    /// [`ErrorCode::Io`](crate::ErrorCode::Io) or [`ErrorCode::Corrupt`](crate::ErrorCode::Corrupt) if the commit
    /// fails, after which every write may fail with [`ErrorCode::Poisoned`](crate::ErrorCode::Poisoned).
    pub fn commit(&self) -> Result<()> {
        self.inner.commit()
    }
}
