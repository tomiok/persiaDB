//! The JSON form of a document (SPEC §6.4.1).

use std::collections::BTreeMap;
use std::fmt;

use persia_format::{BlobId, BlobRef};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value as J};

use super::validate::{expected, json_values, out_of_range};
use super::{Document, DocumentError, MAX_DOCUMENT_JSON_LEN, Value, datetime};
use crate::schema::{CollectionSchema, Field, FieldType, field_name_problem};

pub(super) fn from_json(json: &str, schema: &CollectionSchema) -> Result<Document, DocumentError> {
    if json.len() > MAX_DOCUMENT_JSON_LEN {
        return Err(DocumentError::Json(format!(
            "{} bytes, at most {MAX_DOCUMENT_JSON_LEN}",
            json.len()
        )));
    }
    let Strict(value) =
        serde_json::from_str(json).map_err(|e| DocumentError::Json(e.to_string()))?;
    let J::Object(mut object) = value else {
        return Err(DocumentError::Json(format!(
            "expected an object, found {}",
            json_kind(&value)
        )));
    };
    let id = match object.remove("id") {
        Some(J::String(id)) => id,
        Some(other) => {
            return Err(DocumentError::Json(format!(
                "`id` must be a string, found {}",
                json_kind(&other)
            )));
        }
        None => return Err(DocumentError::Json("missing `id`".to_owned())),
    };
    let mut doc = Document::new(id);
    match object.remove("blob") {
        None | Some(J::Null) => {}
        Some(blob) => doc.blob_ref = Some(blob_from_json(blob)?),
    }
    for (name, value) in object {
        if let Some(field) = schema.field(&name) {
            if let Some(value) = field_value(field, value)? {
                doc.insert(field.id(), value);
            }
            continue;
        }
        if let Some(problem) = field_name_problem(&name) {
            return Err(DocumentError::FieldName { name, problem });
        }
        if schema.strict() {
            return Err(DocumentError::UnknownField { name });
        }
        if !value.is_null() {
            doc.extra.insert(name, value);
        }
    }
    doc.validate(schema)?;
    Ok(doc)
}

/// `field`'s value from JSON, `None` for `null`.
fn field_value(field: &Field, value: J) -> Result<Option<Value>, DocumentError> {
    let name = field.name();
    let field_type = field.field_type();
    let wrong = |value: &J| DocumentError::WrongType {
        field: name.to_owned(),
        expected: expected(field_type),
        found: json_kind(value),
    };
    let value = match (field_type, value) {
        (_, J::Null) => return Ok(None),
        (FieldType::Stored, value) => Value::Json(value),
        (FieldType::Text | FieldType::Keyword, J::String(s)) => Value::String(s),
        (FieldType::Text | FieldType::Keyword, J::Array(items)) => {
            let list = items.into_iter().enumerate().map(|(i, item)| match item {
                J::String(s) => Ok(s),
                other => Err(DocumentError::WrongType {
                    field: format!("{name}[{i}]"),
                    expected: "a string",
                    found: json_kind(&other),
                }),
            });
            Value::Strings(list.collect::<Result<_, _>>()?)
        }
        (FieldType::I64, J::Number(n)) => Value::I64(integer(name, &n)?),
        (FieldType::F64, J::Number(n)) => {
            Value::F64(n.as_f64().ok_or_else(|| wrong(&J::Number(n)))?)
        }
        (FieldType::Bool, J::Bool(b)) => Value::Bool(b),
        (FieldType::Datetime, J::Number(n)) => Value::Datetime(integer(name, &n)?),
        (FieldType::Datetime, J::String(s)) => match datetime::parse(&s) {
            Some(micros) => Value::Datetime(micros),
            None => {
                return Err(DocumentError::InvalidDatetime {
                    field: name.to_owned(),
                    value: s,
                });
            }
        },
        (_, other) => return Err(wrong(&other)),
    };
    Ok(Some(value))
}

/// An integer JSON number as an `i64`. `serde_json` reads `-0` and integers beyond `u64` or below `i64::MIN` as
/// floats, so those are told apart from numbers written with a fraction or exponent by their value.
pub(crate) fn integer(field: &str, n: &Number) -> Result<i64, DocumentError> {
    if let Some(v) = n.as_i64() {
        return Ok(v);
    }
    let x = n.as_f64().unwrap_or(f64::NAN);
    if x == 0.0 && x.is_sign_negative() {
        return Ok(0);
    }
    // 2^63: an integral float this large is beyond i64 whichever way it was written.
    if n.is_u64() || (x.fract() == 0.0 && x.abs() >= 9_223_372_036_854_775_808.0) {
        return Err(out_of_range(field, n, "-2^63..2^63"));
    }
    Err(DocumentError::WrongType {
        field: field.to_owned(),
        expected: "an integer",
        found: "a number with a fraction or exponent",
    })
}

pub(crate) fn json_kind(value: &J) -> &'static str {
    match value {
        J::Null => "null",
        J::Bool(_) => "a bool",
        J::Number(n) if n.is_f64() => "a number with a fraction or exponent",
        J::Number(_) => "an integer",
        J::String(_) => "a string",
        J::Array(_) => "an array",
        J::Object(_) => "an object",
    }
}

/// `{ "blob_id", "size", "mime", "chunk_size", "user_meta"? }` (SPEC §9.1).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBlob {
    blob_id: String,
    size: u64,
    mime: String,
    chunk_size: u32,
    #[serde(default)]
    user_meta: Option<BTreeMap<String, String>>,
}

fn blob_from_json(value: J) -> Result<BlobRef, DocumentError> {
    if !value.is_object() {
        // serde would also take a struct from an array, by position.
        return Err(DocumentError::Blob(format!(
            "expected an object, found {}",
            json_kind(&value)
        )));
    }
    let raw: RawBlob =
        serde_json::from_value(value).map_err(|e| DocumentError::Blob(e.to_string()))?;
    let id: BlobId = raw
        .blob_id
        .parse()
        .map_err(|e| DocumentError::Blob(format!("blob_id: {e}")))?;
    BlobRef::new(
        id,
        raw.size,
        raw.mime,
        raw.chunk_size,
        raw.user_meta.unwrap_or_default(),
    )
    .map_err(|e| DocumentError::Blob(e.to_string()))
}

/// A document's JSON form (SPEC §6.4.1, "Output") as a [`serde::Serialize`] value: an object with `id`, then `blob`
/// when the document has a blob reference, then its fields and extra fields in ascending byte order of their names,
/// each value in its field type's JSON form. Made by [`DocumentJson::new`], which checks the document against its
/// schema, so serializing it cannot fail on the document's account. [`Document::to_json`] is
/// `serde_json::to_string` of it, and a writer that embeds a document in a larger value (the response JSON, SPEC
/// §7.5.2) serializes it in place, without writing the text and reading it back.
///
/// ```
/// use persia_engine::document::{Document, DocumentJson, Value};
/// use persia_engine::schema::{FieldKind, SchemaBuilder};
///
/// let schema = SchemaBuilder::new("c").field("n", FieldKind::I64 { stored: true }).build()?;
/// let n = schema.field("n").ok_or("no field")?.id();
/// let doc = Document::new("a").with(n, Value::I64(7));
/// let view = DocumentJson::new(&doc, &schema)?;
/// // Serialized in place, inside another value (a `serde_json::Value` would sort its keys: `id` first is kept here).
/// let embedded = serde_json::to_string(&[&view])?;
/// assert_eq!(embedded, r#"[{"id":"a","n":7}]"#);
/// assert_eq!(serde_json::to_string(&view)?, doc.to_json(&schema)?);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct DocumentJson<'a> {
    id: &'a str,
    blob: Option<&'a BlobRef>,
    /// The fields and extra fields by name, in ascending byte order.
    entries: Vec<(&'a str, Entry<'a>)>,
}

/// One value of a [`DocumentJson`], checked against its field.
#[derive(Debug, Clone)]
enum Entry<'a> {
    String(&'a str),
    Strings(&'a [String]),
    I64(i64),
    /// Finite.
    F64(f64),
    Bool(bool),
    /// Already in the output form: formatting checked the range.
    Datetime(String),
    /// A `stored` or extra value: not `null`, nested at most [`super::MAX_STORED_DEPTH`] levels.
    Json(&'a J),
}

impl<'a> DocumentJson<'a> {
    /// The JSON form of `document`, a document of the collection of `schema`.
    ///
    /// # Errors
    /// As [`Document::to_json`]: [`DocumentError::UnknownFieldId`] if a field is not in `schema`,
    /// [`DocumentError::WrongType`] if a value does not have its field's type, [`DocumentError::OutOfRange`] for a
    /// non-finite float or a datetime out of range, [`DocumentError::TooDeep`] for a stored value nested too deep,
    /// [`DocumentError::NullStored`] for a `null` stored or extra value, [`DocumentError::FieldName`] for an extra
    /// field whose name breaks the field name rule, [`DocumentError::ExtraIsSchemaField`] for an extra field next to a
    /// value of the schema field of that name.
    pub fn new(
        document: &'a Document,
        schema: &'a CollectionSchema,
    ) -> Result<Self, DocumentError> {
        let mut entries = Vec::with_capacity(document.fields.len() + document.extra.len());
        for (id, value) in &document.fields {
            let field = schema
                .field_by_id(*id)
                .ok_or(DocumentError::UnknownFieldId { id: *id })?;
            entries.push((field.name(), entry(field, value)?));
        }
        for (name, value) in &document.extra {
            if schema
                .field(name)
                .and_then(|f| document.get(f.id()))
                .is_some()
            {
                return Err(DocumentError::ExtraIsSchemaField { name: name.clone() });
            }
            if let Some(problem) = field_name_problem(name) {
                return Err(DocumentError::FieldName {
                    name: name.clone(),
                    problem,
                });
            }
            entries.push((name.as_str(), Entry::Json(stored_json(name, value)?)));
        }
        entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        Ok(Self {
            id: &document.id,
            blob: document.blob_ref.as_ref(),
            entries,
        })
    }
}

/// The keys come in the order of SPEC §6.4.1 whatever the serializer's map type.
impl Serialize for DocumentJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let len = 1 + usize::from(self.blob.is_some()) + self.entries.len();
        let mut out = s.serialize_map(Some(len))?;
        out.serialize_entry("id", self.id)?;
        if let Some(blob) = self.blob {
            out.serialize_entry("blob", &BlobJson(blob))?;
        }
        for (name, value) in &self.entries {
            out.serialize_entry(name, value)?;
        }
        out.end()
    }
}

impl Serialize for Entry<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::String(text) => s.serialize_str(text),
            Self::Strings(list) => list.serialize(s),
            Self::I64(v) => s.serialize_i64(*v),
            Self::F64(v) => s.serialize_f64(*v),
            Self::Bool(v) => s.serialize_bool(*v),
            Self::Datetime(text) => s.serialize_str(text),
            Self::Json(value) => value.serialize(s),
        }
    }
}

/// `{ "blob_id", "chunk_size", "mime", "size", "user_meta"? }` (SPEC §9.1): its keys in byte order, as a JSON object
/// with sorted keys has always written them here.
struct BlobJson<'a>(&'a BlobRef);

impl Serialize for BlobJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let blob = self.0;
        let meta = !blob.user_meta().is_empty();
        let mut out = s.serialize_map(Some(4 + usize::from(meta)))?;
        out.serialize_entry("blob_id", &BlobIdText(blob.blob_id()))?;
        out.serialize_entry("chunk_size", &blob.chunk_size())?;
        out.serialize_entry("mime", blob.mime())?;
        out.serialize_entry("size", &blob.size())?;
        if meta {
            out.serialize_entry("user_meta", blob.user_meta())?;
        }
        out.end()
    }
}

/// A blob id as its text form (64 lowercase hex characters).
struct BlobIdText(BlobId);

impl Serialize for BlobIdText {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

pub(super) fn to_json(doc: &Document, schema: &CollectionSchema) -> Result<String, DocumentError> {
    let view = DocumentJson::new(doc, schema)?;
    // The view was checked: only a broken serializer could fail here, and that is reported, not hidden.
    serde_json::to_string(&view)
        .map_err(|e| DocumentError::Json(format!("writing the document: {e}")))
}

/// `value`, a value of `field`, as a JSON form [`DocumentJson`] writes.
fn entry<'a>(field: &Field, value: &'a Value) -> Result<Entry<'a>, DocumentError> {
    let name = field.name();
    let field_type = field.field_type();
    Ok(match (field_type, value) {
        (FieldType::Text | FieldType::Keyword, Value::String(s)) => Entry::String(s),
        (FieldType::Text | FieldType::Keyword, Value::Strings(list)) => Entry::Strings(list),
        (FieldType::I64, Value::I64(v)) => Entry::I64(*v),
        (FieldType::F64, Value::F64(v)) if v.is_finite() => Entry::F64(*v),
        (FieldType::F64, Value::F64(v)) => return Err(out_of_range(name, v, "finite numbers")),
        (FieldType::Bool, Value::Bool(v)) => Entry::Bool(*v),
        (FieldType::Datetime, Value::Datetime(v)) => Entry::Datetime(
            datetime::format(*v).ok_or_else(|| out_of_range(name, v, datetime::RANGE))?,
        ),
        (FieldType::Stored, Value::Json(v)) => Entry::Json(stored_json(name, v)?),
        _ => {
            return Err(DocumentError::WrongType {
                field: name.to_owned(),
                expected: expected(field_type),
                found: value.kind(),
            });
        }
    })
}

/// A JSON value that rejects duplicate object keys at every level (`serde_json` keeps the last one).
pub(crate) struct Strict(pub(crate) J);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = J;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<J, E> {
        Ok(J::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<J, E> {
        Ok(J::from(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<J, E> {
        Ok(J::from(v))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<J, E> {
        Number::from_f64(v)
            .map(J::Number)
            .ok_or_else(|| E::custom("number out of range"))
    }

    fn visit_str<E>(self, v: &str) -> Result<J, E> {
        Ok(J::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<J, E> {
        Ok(J::String(v))
    }

    fn visit_unit<E>(self) -> Result<J, E> {
        Ok(J::Null)
    }

    fn visit_none<E>(self) -> Result<J, E> {
        Ok(J::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<J, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(J::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<J, A::Error> {
        let mut map = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if map.contains_key(&key) {
                return Err(de::Error::custom(format_args!("duplicate key {key:?}")));
            }
            let Strict(value) = access.next_value()?;
            map.insert(key, value);
        }
        Ok(J::Object(map))
    }
}

/// `value` if it is a valid stored value: not `null` (that would read back as absent), and nested at most
/// [`super::MAX_STORED_DEPTH`] levels (writing deeper JSON would recurse without bound).
fn stored_json<'a>(field: &str, value: &'a J) -> Result<&'a J, DocumentError> {
    if value.is_null() {
        return Err(DocumentError::NullStored {
            field: field.to_owned(),
        });
    }
    json_values(value, 0)
        .map(|_| value)
        .ok_or_else(|| DocumentError::TooDeep {
            field: field.to_owned(),
        })
}
