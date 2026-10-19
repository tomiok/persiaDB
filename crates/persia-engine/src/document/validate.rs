//! The value rules of SPEC §6.4.1.

use super::{
    Document, DocumentError, FieldSet, MAX_DOCUMENT_FIELDS, MAX_DOCUMENT_LEN, MAX_ID_LEN,
    MAX_KEYWORD_LEN, MAX_STORED_DEPTH, Value, codec, datetime,
};
use crate::schema::{CollectionSchema, Field, FieldType, field_name_problem};

pub(super) fn validate(doc: &Document, schema: &CollectionSchema) -> Result<(), DocumentError> {
    validate_and_encode(doc, schema).map(drop)
}

/// Validates `doc` and returns its encoding with every field, which the size limit needs anyway.
pub(super) fn validate_and_encode(
    doc: &Document,
    schema: &CollectionSchema,
) -> Result<Vec<u8>, DocumentError> {
    if !(1..=MAX_ID_LEN).contains(&doc.id.len()) {
        return Err(DocumentError::IdLen { len: doc.id.len() });
    }
    let count = doc.fields.len().saturating_add(doc.extra.len());
    if count > MAX_DOCUMENT_FIELDS {
        return Err(DocumentError::TooManyFields { count });
    }
    for (id, value) in &doc.fields {
        let field = schema
            .field_by_id(*id)
            .ok_or(DocumentError::UnknownFieldId { id: *id })?;
        check_value(field, value)?;
    }
    for (name, value) in &doc.extra {
        if let Some(problem) = field_name_problem(name) {
            return Err(DocumentError::FieldName {
                name: name.clone(),
                problem,
            });
        }
        if schema.field(name).is_some() {
            return Err(DocumentError::ExtraIsSchemaField { name: name.clone() });
        }
        if schema.strict() {
            return Err(DocumentError::UnknownField { name: name.clone() });
        }
        check_stored(name, value)?;
    }
    let bytes = codec::encode(doc, schema, FieldSet::All);
    if bytes.len() > MAX_DOCUMENT_LEN {
        return Err(DocumentError::TooLarge { len: bytes.len() });
    }
    Ok(bytes)
}

/// What a field of type `t` takes, for error messages.
pub(super) const fn expected(t: FieldType) -> &'static str {
    match t {
        FieldType::Text | FieldType::Keyword => "a string or a list of strings",
        FieldType::I64 => "an i64",
        FieldType::F64 => "an f64",
        FieldType::Bool => "a bool",
        FieldType::Datetime => "a datetime",
        FieldType::Stored => "a JSON value",
    }
}

fn check_value(field: &Field, value: &Value) -> Result<(), DocumentError> {
    let name = field.name();
    let field_type = field.field_type();
    match (field_type, value) {
        (FieldType::Text, Value::String(_) | Value::Strings(_))
        | (FieldType::I64, Value::I64(_))
        | (FieldType::Bool, Value::Bool(_)) => Ok(()),
        (FieldType::Keyword, Value::String(s)) => check_keyword(name, s),
        (FieldType::Keyword, Value::Strings(list)) => list
            .iter()
            .enumerate()
            .try_for_each(|(i, s)| check_keyword(&format!("{name}[{i}]"), s)),
        (FieldType::F64, Value::F64(v)) => {
            if v.is_finite() {
                Ok(())
            } else {
                Err(out_of_range(name, v, "finite numbers"))
            }
        }
        (FieldType::Datetime, Value::Datetime(v)) => {
            if datetime::in_range(*v) {
                Ok(())
            } else {
                Err(out_of_range(name, v, datetime::RANGE))
            }
        }
        (FieldType::Stored, Value::Json(v)) => check_stored(name, v),
        _ => Err(DocumentError::WrongType {
            field: name.to_owned(),
            expected: expected(field_type),
            found: value.kind(),
        }),
    }
}

pub(super) fn out_of_range(
    field: &str,
    value: &impl ToString,
    allowed: &'static str,
) -> DocumentError {
    DocumentError::OutOfRange {
        field: field.to_owned(),
        value: value.to_string(),
        allowed,
    }
}

fn check_keyword(field: &str, s: &str) -> Result<(), DocumentError> {
    if s.len() > MAX_KEYWORD_LEN {
        return Err(DocumentError::KeywordTooLong {
            field: field.to_owned(),
            len: s.len(),
        });
    }
    Ok(())
}

fn check_stored(field: &str, value: &serde_json::Value) -> Result<(), DocumentError> {
    if value.is_null() {
        return Err(DocumentError::NullStored {
            field: field.to_owned(),
        });
    }
    if !within_depth(value, 0) {
        return Err(DocumentError::TooDeep {
            field: field.to_owned(),
        });
    }
    Ok(())
}

/// Whether `value`, inside `depth` arrays and objects, nests at most [`MAX_STORED_DEPTH`] levels in total. The
/// recursion stops at the limit, so a hostile value cannot exhaust the stack.
fn within_depth(value: &serde_json::Value, depth: usize) -> bool {
    use serde_json::Value as J;
    let children: Box<dyn Iterator<Item = &serde_json::Value>> = match value {
        J::Array(items) => Box::new(items.iter()),
        J::Object(map) => Box::new(map.values()),
        _ => return true,
    };
    depth < MAX_STORED_DEPTH && { children }.all(|v| within_depth(v, depth + 1))
}
