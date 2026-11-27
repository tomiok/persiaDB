//! Typed documents through `serde` (SPEC §6.4.1; ROADMAP 10.1.4).
//!
//! A value is exactly its JSON (owner decision f, wave 7): `put_struct(&value)` is `put_json` of
//! `serde_json::to_string(&value)`, and a document becomes a value through its JSON output (SPEC §6.4.1, "Output").
//! SPEC §6.4.1 is the only rule set: no second mapping between Rust types and field values.

use persia_engine::document::Document;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::collection::Collection;
use crate::{Error, ErrorCode, Result};

impl Collection {
    /// [`put_json`](Self::put_json) of `value` as `serde_json` writes it: its JSON form must be a document of the
    /// collection's schema (SPEC §6.4.1), with an `"id"`.
    ///
    /// ```
    /// use serde::{Deserialize, Serialize};
    ///
    /// #[derive(Serialize, Deserialize, PartialEq, Debug)]
    /// struct Product {
    ///     id: String,
    ///     title: String,
    ///     price: f64,
    /// }
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let products = db.create_collection_json(
    ///     r#"{"name": "products", "fields": {"title": {"type": "text"}, "price": {"type": "f64", "stored": true}}}"#,
    /// )?;
    /// let lamp = Product { id: "p1".into(), title: "Lamp".into(), price: 19.5 };
    /// products.put_struct(&lamp)?;
    /// assert_eq!(products.get_struct::<Product>("p1")?, Some(lamp));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`] if `value` cannot be written as JSON (a map with keys that are not strings, a
    /// `Serialize` impl that fails), or its JSON is not a valid document of the schema; then as
    /// [`put`](Self::put).
    pub fn put_struct<T: Serialize + ?Sized>(&self, value: &T) -> Result<()> {
        self.put_json(&to_json(value)?)
    }

    /// The document with id `id` as a `T`, read from its JSON output (SPEC §6.4.1). `None` if the collection has
    /// no such document.
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {}, "strict": false}"#)?;
    /// notes.put_json(r#"{"id": "n1", "mood": "fine"}"#)?;
    /// let read: BTreeMap<String, String> = notes.get_struct("n1")?.ok_or("missing")?;
    /// assert_eq!(read["mood"], "fine");
    /// assert!(notes.get_struct::<BTreeMap<String, String>>("n2")?.is_none());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`] if the document's JSON is not a `T`; then as [`get`](Self::get).
    pub fn get_struct<T: DeserializeOwned>(&self, id: &str) -> Result<Option<T>> {
        self.get_json(id)?.map(|json| from_json(&json)).transpose()
    }

    /// `doc`, a document of this collection, as a `T`, read from its JSON output under the collection's current
    /// schema (SPEC §6.4.1): the reverse of [`put_struct`](Self::put_struct) for a document already at hand (from
    /// [`get`](Self::get), or a search hit). A [`Document`] names its fields by id, so it needs the schema to have
    /// names.
    ///
    /// ```
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize)]
    /// struct Note {
    ///     id: String,
    ///     body: String,
    /// }
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// notes.put_json(r#"{"id": "n1", "body": "hello"}"#)?;
    /// let doc = notes.get("n1")?.ok_or("missing")?;
    /// let note: Note = notes.to_struct(&doc)?;
    /// assert_eq!((note.id.as_str(), note.body.as_str()), ("n1", "hello"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`] if `doc` does not fit the schema (a field id it lacks, a value of another
    /// type), or its JSON is not a `T`; [`ErrorCode::NotFound`] if the collection was dropped;
    /// [`ErrorCode::Closed`].
    pub fn to_struct<T: DeserializeOwned>(&self, doc: &Document) -> Result<T> {
        let json = self.read(|c| doc.to_json(c.schema()).map_err(Error::wrap))?;
        from_json(&json)
    }
}

/// `value` as `serde_json` writes it: what `put_struct` puts (also the async one, ROADMAP 10.1.26).
pub(crate) fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|source| {
        Error::with_source(
            ErrorCode::InvalidArgument,
            "the value cannot be written as JSON",
            source,
        )
    })
}

fn from_json<T: DeserializeOwned>(json: &str) -> Result<T> {
    serde_json::from_str(json).map_err(|source| {
        Error::with_source(
            ErrorCode::InvalidArgument,
            "the document does not read as the requested type",
            source,
        )
    })
}
