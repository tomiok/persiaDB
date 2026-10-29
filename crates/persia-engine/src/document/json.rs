//! The JSON form of a document (SPEC §6.4.1).

use std::collections::BTreeMap;
use std::fmt;

use persia_format::{BlobId, BlobRef};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
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

fn blob_to_json(blob: &BlobRef) -> J {
    let mut map = Map::new();
    map.insert("blob_id".to_owned(), J::String(blob.blob_id().to_string()));
    map.insert("size".to_owned(), J::from(blob.size()));
    map.insert("mime".to_owned(), J::String(blob.mime().to_owned()));
    map.insert("chunk_size".to_owned(), J::from(blob.chunk_size()));
    if !blob.user_meta().is_empty() {
        let meta = blob
            .user_meta()
            .iter()
            .map(|(k, v)| (k.clone(), J::String(v.clone())));
        map.insert("user_meta".to_owned(), J::Object(meta.collect()));
    }
    J::Object(map)
}

pub(super) fn to_json(doc: &Document, schema: &CollectionSchema) -> Result<String, DocumentError> {
    let mut entries: Vec<(&str, J)> = Vec::with_capacity(doc.fields.len() + doc.extra.len());
    for (id, value) in &doc.fields {
        let field = schema
            .field_by_id(*id)
            .ok_or(DocumentError::UnknownFieldId { id: *id })?;
        entries.push((field.name(), value_to_json(field, value)?));
    }
    for (name, value) in &doc.extra {
        if schema.field(name).and_then(|f| doc.get(f.id())).is_some() {
            return Err(DocumentError::ExtraIsSchemaField { name: name.clone() });
        }
        if let Some(problem) = field_name_problem(name) {
            return Err(DocumentError::FieldName {
                name: name.clone(),
                problem,
            });
        }
        entries.push((name, stored_json(name, value)?.clone()));
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    // Written by hand: the order (id, blob, then names) must not depend on serde_json's map type.
    let mut out = String::from("{\"id\":");
    out.push_str(&J::String(doc.id.clone()).to_string());
    if let Some(blob) = &doc.blob_ref {
        out.push_str(",\"blob\":");
        out.push_str(&blob_to_json(blob).to_string());
    }
    for (name, value) in entries {
        out.push(',');
        out.push_str(&J::String(name.to_owned()).to_string());
        out.push(':');
        out.push_str(&value.to_string());
    }
    out.push('}');
    Ok(out)
}

fn value_to_json(field: &Field, value: &Value) -> Result<J, DocumentError> {
    let name = field.name();
    let field_type = field.field_type();
    Ok(match (field_type, value) {
        (FieldType::Text | FieldType::Keyword, Value::String(s)) => J::String(s.clone()),
        (FieldType::Text | FieldType::Keyword, Value::Strings(list)) => {
            J::Array(list.iter().cloned().map(J::String).collect())
        }
        (FieldType::I64, Value::I64(v)) => J::from(*v),
        (FieldType::F64, Value::F64(v)) => Number::from_f64(*v)
            .map(J::Number)
            .ok_or_else(|| out_of_range(name, v, "finite numbers"))?,
        (FieldType::Bool, Value::Bool(v)) => J::Bool(*v),
        (FieldType::Datetime, Value::Datetime(v)) => {
            J::String(datetime::format(*v).ok_or_else(|| out_of_range(name, v, datetime::RANGE))?)
        }
        (FieldType::Stored, Value::Json(v)) => stored_json(name, v)?.clone(),
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
