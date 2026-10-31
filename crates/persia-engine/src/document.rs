//! Documents: an id, typed field values and an optional blob reference; validation against a schema, the
//! deterministic CBOR encoding `DocStore` persists, and the JSON form (SPEC §6.4, ADR-0023).
//!
//! ```
//! use persia_engine::document::{Document, FieldSet, Value};
//! use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
//!
//! let schema = SchemaBuilder::new("products")
//!     .field("title", FieldKind::Text(TextOptions::default()))
//!     .field("price", FieldKind::F64 { stored: true })
//!     .build()?;
//! let doc = Document::from_json(r#"{"id": "p1", "title": "Lamp", "price": 19.5}"#, &schema)?;
//! let price = schema.field("price").map(|f| f.id());
//! assert_eq!(price.and_then(|id| doc.get(id)), Some(&Value::F64(19.5)));
//!
//! let bytes = doc.encode(&schema, FieldSet::All)?;
//! assert_eq!(Document::decode(&bytes, &schema)?, doc);
//! assert_eq!(doc.to_json(&schema)?, r#"{"id":"p1","price":19.5,"title":"Lamp"}"#);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod cbor;
mod codec;
pub(crate) mod datetime;
pub(crate) mod json;
mod validate;

use std::collections::BTreeMap;

use persia_format::BlobRef;

use crate::schema::{CollectionSchema, FieldId, NameProblem};

pub use codec::{DecodeError, DocumentCorruption};
pub use datetime::{MAX_DATETIME, MIN_DATETIME};

/// Longest document id, in bytes (SPEC §6.1).
pub const MAX_ID_LEN: usize = 512;
/// Longest `keyword` value, in bytes (SPEC §6.4.1).
pub const MAX_KEYWORD_LEN: usize = 32 << 10;
/// Most fields a document has, schema and extra fields together (SPEC §6.4.1).
pub const MAX_DOCUMENT_FIELDS: usize = 1024;
/// Deepest nesting of arrays and objects in a `stored` value (SPEC §6.4.1).
pub const MAX_STORED_DEPTH: usize = 64;
/// Most values a document holds: field values, extra field values, list elements and the values inside stored and
/// extra values (SPEC §6.4.1).
pub const MAX_DOCUMENT_VALUES: usize = 1 << 20;
/// Longest JSON text [`Document::from_json`] reads, in bytes (SPEC §6.4.1).
pub const MAX_DOCUMENT_JSON_LEN: usize = 16 << 20;
/// Longest binary encoding of a whole document, in bytes (SPEC §6.4.1).
pub const MAX_DOCUMENT_LEN: usize = 16 << 20;

/// A field's value (SPEC §6.4.1).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// One string: `text` or `keyword`.
    String(String),
    /// A list of strings: multi-valued `text` or `keyword`.
    Strings(Vec<String>),
    /// An `i64` value.
    I64(i64),
    /// An `f64` value; finite, and never `-0.0` inside a [`Document`].
    F64(f64),
    /// A `bool` value.
    Bool(bool),
    /// A `datetime`: microseconds since the Unix epoch, UTC, in [`MIN_DATETIME`]`..=`[`MAX_DATETIME`].
    Datetime(i64),
    /// A `stored` value: any JSON value but a top-level `null`.
    Json(serde_json::Value),
}

impl Value {
    /// What kind of value this is, for error messages.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::String(_) => "a string",
            Self::Strings(_) => "a list of strings",
            Self::I64(_) => "an i64",
            Self::F64(_) => "an f64",
            Self::Bool(_) => "a bool",
            Self::Datetime(_) => "a datetime",
            Self::Json(_) => "a JSON value",
        }
    }

    /// `-0.0` reads as `0.0` (SPEC §6.4.1).
    fn canonical(self) -> Self {
        match self {
            // A float pattern compares with `==`, so it matches `-0.0` too.
            Self::F64(0.0) => Self::F64(0.0),
            other => other,
        }
    }
}

/// Which fields [`Document::encode`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSet {
    /// Every field.
    All,
    /// Only fields with `stored: true` (SPEC §6.2.2), plus the id, blob reference and extra fields, which are
    /// always written: a `DocStore` row.
    Stored,
}

/// A document (SPEC §6.4): an id, field values by [`FieldId`], extra fields of non-strict collections by name,
/// and an optional blob reference.
///
/// A `Document` holds values of any shape; [`Document::validate`] checks them against a schema, and
/// [`Document::encode`] validates before encoding.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    id: String,
    fields: BTreeMap<FieldId, Value>,
    extra: BTreeMap<String, serde_json::Value>,
    blob_ref: Option<BlobRef>,
}

impl Document {
    /// A document with id `id` and no fields.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            fields: BTreeMap::new(),
            extra: BTreeMap::new(),
            blob_ref: None,
        }
    }

    /// The document's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The blob the document references (SPEC §9.1).
    pub fn blob_ref(&self) -> Option<&BlobRef> {
        self.blob_ref.as_ref()
    }

    /// Sets or clears the blob reference.
    pub fn set_blob_ref(&mut self, blob_ref: Option<BlobRef>) {
        self.blob_ref = blob_ref;
    }

    /// Sets field `field` to `value` (`-0.0` becomes `0.0`) and returns the previous value.
    pub fn insert(&mut self, field: FieldId, value: Value) -> Option<Value> {
        self.fields.insert(field, value.canonical())
    }

    /// [`Self::insert`] for building a document in one expression.
    #[must_use]
    pub fn with(mut self, field: FieldId, value: Value) -> Self {
        self.insert(field, value);
        self
    }

    /// Removes field `field` and returns its value.
    pub fn remove(&mut self, field: FieldId) -> Option<Value> {
        self.fields.remove(&field)
    }

    /// The value of field `field`.
    pub fn get(&self, field: FieldId) -> Option<&Value> {
        self.fields.get(&field)
    }

    /// Field values in ascending field id.
    pub fn fields(&self) -> impl Iterator<Item = (FieldId, &Value)> {
        self.fields.iter().map(|(id, v)| (*id, v))
    }

    /// Sets the extra field `name` (a field outside the schema, kept as `stored` by a non-strict collection) and
    /// returns the previous value.
    pub fn insert_extra(
        &mut self,
        name: impl Into<String>,
        value: serde_json::Value,
    ) -> Option<serde_json::Value> {
        self.extra.insert(name.into(), value)
    }

    /// Extra fields in ascending byte order of their names.
    pub fn extra(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.extra
    }

    /// Keeps the fields `field` accepts and the extra fields `extra` accepts, by name; drops the others.
    pub fn retain(
        &mut self,
        mut field: impl FnMut(FieldId) -> bool,
        mut extra: impl FnMut(&str) -> bool,
    ) {
        self.fields.retain(|id, _| field(*id));
        self.extra.retain(|name, _| extra(name));
    }

    /// Checks the document against `schema` (SPEC §6.4.1).
    ///
    /// # Errors
    /// The first rule the document breaks.
    pub fn validate(&self, schema: &CollectionSchema) -> Result<(), DocumentError> {
        validate::validate(self, schema)
    }

    /// Validates the document, then encodes `fields` of it (SPEC §6.4.2).
    ///
    /// # Errors
    /// As [`Self::validate`].
    pub fn encode(
        &self,
        schema: &CollectionSchema,
        fields: FieldSet,
    ) -> Result<Vec<u8>, DocumentError> {
        self.validate(schema)?;
        Ok(codec::encode(self, schema, fields))
    }

    /// Decodes a document encoded by [`Self::encode`] for `schema` or an earlier version of it (SPEC §6.4.2).
    /// Error offsets are relative to the start of `bytes`.
    ///
    /// # Errors
    /// [`DecodeError::Corrupt`] if `bytes` is not exactly one valid encoding, [`DecodeError::UnsupportedVersion`]
    /// for an encoding version this build does not read.
    pub fn decode(bytes: &[u8], schema: &CollectionSchema) -> Result<Self, DecodeError> {
        codec::decode(bytes, schema)
    }

    /// Parses a JSON document for `schema` and validates it (SPEC §6.4.1).
    ///
    /// # Errors
    /// [`DocumentError::Json`] for malformed JSON, duplicate keys or a missing id, another variant for a value
    /// rule.
    pub fn from_json(json: &str, schema: &CollectionSchema) -> Result<Self, DocumentError> {
        json::from_json(json, schema)
    }

    /// The document as JSON (SPEC §6.4.1, "Output").
    ///
    /// # Errors
    /// [`DocumentError::UnknownFieldId`] if a field is not in `schema`, [`DocumentError::WrongType`] if a value
    /// does not have its field's type, [`DocumentError::OutOfRange`] for a non-finite float or a datetime out of range,
    /// [`DocumentError::TooDeep`] for a stored value nested too deep, [`DocumentError::NullStored`] for a `null` stored
    /// or extra value, [`DocumentError::FieldName`] for an extra field whose name breaks the field name rule,
    /// [`DocumentError::ExtraIsSchemaField`] for an extra field next to a value of the schema field of that name.
    pub fn to_json(&self, schema: &CollectionSchema) -> Result<String, DocumentError> {
        json::to_json(self, schema)
    }
}

/// A document that breaks a rule of SPEC §6.4.1. `field` names the field, with the index of a list element
/// (`tags[2]`) where one is at fault.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DocumentError {
    /// Malformed JSON, a duplicate key, a top level that is not an object, a missing or non-string `id`, or a
    /// JSON text over [`MAX_DOCUMENT_JSON_LEN`].
    #[error("invalid document JSON: {0}")]
    Json(String),
    /// The id is empty or longer than [`MAX_ID_LEN`] bytes.
    #[error("document id of {len} bytes, must be 1..={MAX_ID_LEN}")]
    IdLen {
        /// Its length in bytes.
        len: usize,
    },
    /// A key the schema does not have, in a strict collection.
    #[error("unknown field {name:?} (the collection is strict)")]
    UnknownField {
        /// The key.
        name: String,
    },
    /// An extra field whose name breaks the field name rule, or a reserved key used as a field.
    #[error("invalid field name {name:?}: {problem}")]
    FieldName {
        /// The name.
        name: String,
        /// What is wrong with it.
        problem: NameProblem,
    },
    /// An extra field named like a schema field.
    #[error("extra field {name:?} is a schema field")]
    ExtraIsSchemaField {
        /// The name.
        name: String,
    },
    /// A field id the schema does not have.
    #[error("field id {id} is not in the schema")]
    UnknownFieldId {
        /// The id.
        id: FieldId,
    },
    /// A value that does not have the field's type, a list for a single-valued field, or a non-string element.
    #[error("field {field}: expected {expected}, found {found}")]
    WrongType {
        /// The field.
        field: String,
        /// What the field takes.
        expected: &'static str,
        /// What was given.
        found: &'static str,
    },
    /// A value outside its type's range: an integer beyond `i64`, a non-finite float, a datetime outside
    /// [`MIN_DATETIME`]`..=`[`MAX_DATETIME`].
    #[error("field {field}: {value} is outside {allowed}")]
    OutOfRange {
        /// The field.
        field: String,
        /// The value given.
        value: String,
        /// The valid range.
        allowed: &'static str,
    },
    /// A datetime string that is not RFC 3339 as SPEC §6.4.1 accepts it.
    #[error(
        "field {field}: {value:?} is not an RFC 3339 datetime with an offset and at most 6 fractional digits"
    )]
    InvalidDatetime {
        /// The field.
        field: String,
        /// The string given.
        value: String,
    },
    /// A `keyword` value longer than [`MAX_KEYWORD_LEN`] bytes.
    #[error("field {field}: keyword of {len} bytes, at most {MAX_KEYWORD_LEN}")]
    KeywordTooLong {
        /// The field.
        field: String,
        /// Its length in bytes.
        len: usize,
    },
    /// A `stored` value that is `null` itself (absent fields are left out instead).
    #[error("field {field}: a stored value cannot be null; leave the field out")]
    NullStored {
        /// The field.
        field: String,
    },
    /// A `stored` value nested deeper than [`MAX_STORED_DEPTH`].
    #[error("field {field}: nested more than {MAX_STORED_DEPTH} levels deep")]
    TooDeep {
        /// The field.
        field: String,
    },
    /// More than [`MAX_DOCUMENT_VALUES`] values.
    #[error("{count} values, at most {MAX_DOCUMENT_VALUES}")]
    TooManyValues {
        /// How many the document has, or a lower bound of it.
        count: usize,
    },
    /// More than [`MAX_DOCUMENT_FIELDS`] fields.
    #[error("{count} fields, at most {MAX_DOCUMENT_FIELDS}")]
    TooManyFields {
        /// How many the document has.
        count: usize,
    },
    /// An encoding longer than [`MAX_DOCUMENT_LEN`] bytes.
    #[error("document encodes to {len} bytes, at most {MAX_DOCUMENT_LEN}")]
    TooLarge {
        /// The encoded length.
        len: usize,
    },
    /// An invalid `blob` reference in JSON.
    #[error("invalid blob reference: {0}")]
    Blob(String),
}
