//! Collection handles (SPEC §6.2, §8.1; ROADMAP 10.1.2).

use std::fmt;
use std::sync::Arc;

use persia_engine::db::CollectionSnapshot;
use persia_engine::schema::CollectionSchema;
use persia_engine::wal::Durability;

use crate::db::{Db, Inner};
use crate::{Error, ErrorCode, Result};

/// A handle on one collection of a database: what documents are put to, read from and searched in.
///
/// A `Collection` names its collection: it owns a share of the database (no lifetime, no lock), so it can move to
/// another thread and outlive the [`Db`] it came from. Every call reads the collection as the database has it at
/// that moment: after [`Db::drop_collection`] its calls fail with [`ErrorCode::NotFound`], and a collection created
/// again under the same name is the one they reach. After the database is closed (or dropped) they fail with
/// [`ErrorCode::Closed`]. Cloning a handle is cheap.
///
/// ```
/// use persia::{CollectionSchema, Db, ErrorCode};
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// let db = Db::open(dir.path().join("app.persia"))?;
/// let products = db.create_collection_json(r#"{"name": "products", "fields": {"title": {"type": "text"}}}"#)?;
/// assert_eq!(products.name(), "products");
/// assert!(products.schema()?.field("title").is_some());
///
/// let same = db.collection("products")?; // another handle on the same collection
/// db.close()?;
/// assert_eq!(same.schema().unwrap_err().code(), ErrorCode::Closed);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct Collection {
    pub(crate) db: Arc<Inner>,
    pub(crate) name: String,
    /// The durability of this handle's writes; the database's when `None`.
    pub(crate) durability: Option<Durability>,
}

impl fmt::Debug for Collection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Collection")
            .field("name", &self.name)
            .field("db", &self.db.path())
            .finish_non_exhaustive()
    }
}

impl Collection {
    /// The collection's name.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection(persia::SchemaBuilder::new("notes").build()?)?;
    /// assert_eq!(notes.name(), "notes");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The collection's current schema (SPEC §6.2): its fields with their ids, ranking and relaxation settings.
    ///
    /// ```
    /// use persia::{FieldKind, SchemaBuilder, TextOptions};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let schema = SchemaBuilder::new("notes").field("body", FieldKind::Text(TextOptions::default())).build()?;
    /// let notes = db.create_collection(schema.clone())?;
    /// assert_eq!(notes.schema()?, schema);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::NotFound`] if the collection was dropped; [`ErrorCode::Closed`] once the database is closed.
    pub fn schema(&self) -> Result<CollectionSchema> {
        self.read(|c| Ok(c.schema().clone()))
    }

    /// Test hook: runs `f` while holding the database's writer mutex, as a long write would (ADR-0065 item 9); a
    /// panic in `f` poisons the writer as a write that panicked does.
    ///
    /// # Errors
    /// As a write: [`ErrorCode::ReadOnly`], [`ErrorCode::Closed`], [`ErrorCode::Poisoned`].
    #[doc(hidden)]
    pub fn hold_writer_while<T>(&self, f: impl FnOnce() -> T) -> Result<T> {
        self.db.write("hold the writer", |_| Ok(f()))
    }

    /// `f` on the collection in the latest published snapshot (SPEC §8.4): no lock held across it.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&CollectionSnapshot) -> Result<T>) -> Result<T> {
        let snapshot = self.db.snapshot()?;
        let collection = snapshot
            .collection(&self.name)
            .ok_or_else(|| no_such_collection(&self.name))?;
        f(collection)
    }
}

/// The error of a call on a collection the database does not have.
pub(crate) fn no_such_collection(name: &str) -> Error {
    Error::new(ErrorCode::NotFound, format!("no collection named {name:?}"))
}

impl Db {
    /// Creates the collection `schema` describes (SPEC §6.2) and returns a handle on it. The creation is committed
    /// before this returns.
    ///
    /// ```
    /// use persia::{Db, ErrorCode, FieldKind, KeywordOptions, SchemaBuilder};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = Db::open(dir.path().join("app.persia"))?;
    /// let schema = SchemaBuilder::new("users").field("email", FieldKind::Keyword(KeywordOptions::default())).build()?;
    /// let users = db.create_collection(schema.clone())?;
    /// assert_eq!(users.name(), "users");
    /// assert_eq!(db.create_collection(schema).unwrap_err().code(), ErrorCode::AlreadyExists);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::AlreadyExists`] if the database has a collection of that name; [`ErrorCode::Unsupported`] if the
    /// schema names an analyzer this open lacks; [`ErrorCode::ReadOnly`] on a read-only database;
    /// a commit that fails: [`ErrorCode::Io`], and [`ErrorCode::Poisoned`] for every later write.
    pub fn create_collection(&self, schema: CollectionSchema) -> Result<Collection> {
        let name = schema.name().to_owned();
        self.inner.write("create a collection", |db| {
            db.log_mut()
                .create_collection(schema)
                .map(drop)
                .map_err(Error::wrap)
        })?;
        Ok(self.handle(name))
    }

    /// [`Db::create_collection`] of the schema the JSON text `json` defines (SPEC §6.2).
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let err = db.create_collection_json(r#"{"name": "bad name!", "fields": {}}"#).unwrap_err();
    /// assert_eq!(err.code(), persia::ErrorCode::InvalidArgument);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::InvalidArgument`] for JSON that is not a valid schema ([`ErrorCode::Limit`] past a limit of SPEC
    /// §17); then as [`Db::create_collection`].
    pub fn create_collection_json(&self, json: &str) -> Result<Collection> {
        self.inner.check_writable("create a collection")?;
        let schema = CollectionSchema::from_json(json).map_err(Error::wrap)?;
        self.create_collection(schema)
    }

    /// A handle on the collection named `name`.
    ///
    /// ```
    /// use persia::ErrorCode;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// assert_eq!(db.collection("notes")?.name(), "notes");
    /// assert_eq!(db.collection("other").unwrap_err().code(), ErrorCode::NotFound);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::NotFound`] if the database has no such collection; [`ErrorCode::Closed`] once it is closed.
    pub fn collection(&self, name: &str) -> Result<Collection> {
        let snapshot = self.inner.snapshot()?;
        if snapshot.collection(name).is_none() {
            return Err(no_such_collection(name));
        }
        Ok(self.handle(name.to_owned()))
    }

    /// The names of the database's collections, in ascending byte order.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// db.create_collection_json(r#"{"name": "b", "fields": {}}"#)?;
    /// db.create_collection_json(r#"{"name": "a", "fields": {}}"#)?;
    /// assert_eq!(db.collection_names()?, ["a", "b"]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::Closed`] once the database is closed.
    pub fn collection_names(&self) -> Result<Vec<String>> {
        let snapshot = self.inner.snapshot()?;
        Ok(snapshot
            .collections()
            .map(|(name, _)| name.to_owned())
            .collect())
    }

    /// Drops the collection named `name` and every document in it (SPEC §8.1), committing it before this returns.
    /// Handles on it fail with [`ErrorCode::NotFound`] from then on, until a collection of that name is created again.
    ///
    /// ```
    /// use persia::ErrorCode;
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// db.drop_collection("notes")?;
    /// assert_eq!(notes.schema().unwrap_err().code(), ErrorCode::NotFound);
    /// assert_eq!(db.drop_collection("notes").unwrap_err().code(), ErrorCode::NotFound);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::NotFound`] if the database has no such collection; [`ErrorCode::ReadOnly`] on a read-only
    /// database; a commit that fails: [`ErrorCode::Io`], and [`ErrorCode::Poisoned`] for every later write.
    pub fn drop_collection(&self, name: &str) -> Result<()> {
        self.inner.write("drop a collection", |db| {
            db.log_mut()
                .drop_collection(name)
                .map(drop)
                .map_err(Error::wrap)
        })
    }

    fn handle(&self, name: String) -> Collection {
        Collection {
            db: Arc::clone(&self.inner),
            name,
            durability: None,
        }
    }
}
