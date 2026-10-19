//! The value rules of SPEC §6.4.1.

use super::{
    Document, DocumentError, MAX_DOCUMENT_FIELDS, MAX_DOCUMENT_LEN, MAX_DOCUMENT_VALUES,
    MAX_ID_LEN, MAX_KEYWORD_LEN, MAX_STORED_DEPTH, Value, codec, datetime,
};
use crate::schema::{CollectionSchema, Field, FieldType, field_name_problem};

pub(super) fn validate(doc: &Document, schema: &CollectionSchema) -> Result<(), DocumentError> {
    if !(1..=MAX_ID_LEN).contains(&doc.id.len()) {
        return Err(DocumentError::IdLen { len: doc.id.len() });
    }
    let count = doc.fields.len().saturating_add(doc.extra.len());
    if count > MAX_DOCUMENT_FIELDS {
        return Err(DocumentError::TooManyFields { count });
    }
    let mut values = 0usize;
    for (id, value) in &doc.fields {
        let field = schema
            .field_by_id(*id)
            .ok_or(DocumentError::UnknownFieldId { id: *id })?;
        values = values.saturating_add(check_value(field, value)?);
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
        values = values.saturating_add(check_stored(name, value)?);
    }
    if values > MAX_DOCUMENT_VALUES {
        return Err(DocumentError::TooManyValues { count: values });
    }
    let len = codec::encoded_len(doc, schema);
    if len > MAX_DOCUMENT_LEN {
        return Err(DocumentError::TooLarge { len });
    }
    Ok(())
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

/// Checks `value` for `field`; returns how many values it counts as (SPEC §6.4.1).
fn check_value(field: &Field, value: &Value) -> Result<usize, DocumentError> {
    let name = field.name();
    let field_type = field.field_type();
    match (field_type, value) {
        (FieldType::Text, Value::String(_))
        | (FieldType::I64, Value::I64(_))
        | (FieldType::Bool, Value::Bool(_)) => Ok(1),
        (FieldType::Text, Value::Strings(list)) => Ok(list.len().saturating_add(1)),
        (FieldType::Keyword, Value::String(s)) => check_keyword(name, s).map(|()| 1),
        (FieldType::Keyword, Value::Strings(list)) => {
            for (i, s) in list.iter().enumerate() {
                check_keyword(&format!("{name}[{i}]"), s)?;
            }
            Ok(list.len().saturating_add(1))
        }
        (FieldType::F64, Value::F64(v)) => {
            if v.is_finite() {
                Ok(1)
            } else {
                Err(out_of_range(name, v, "finite numbers"))
            }
        }
        (FieldType::Datetime, Value::Datetime(v)) => {
            if datetime::in_range(*v) {
                Ok(1)
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

/// Checks a stored or extra value; returns how many values it holds.
fn check_stored(field: &str, value: &serde_json::Value) -> Result<usize, DocumentError> {
    if value.is_null() {
        return Err(DocumentError::NullStored {
            field: field.to_owned(),
        });
    }
    json_values(value, 0).ok_or_else(|| DocumentError::TooDeep {
        field: field.to_owned(),
    })
}

/// How many values `value` holds, itself included, or `None` if, inside `depth` arrays and objects, it nests more
/// than [`MAX_STORED_DEPTH`] levels in total. The recursion stops at the limit, so a hostile value cannot exhaust the
/// stack.
pub(super) fn json_values(value: &serde_json::Value, depth: usize) -> Option<usize> {
    use serde_json::Value as J;
    let mut children: Box<dyn Iterator<Item = &serde_json::Value>> = match value {
        J::Array(items) => Box::new(items.iter()),
        J::Object(map) => Box::new(map.values()),
        _ => return Some(1),
    };
    if depth >= MAX_STORED_DEPTH {
        return None;
    }
    children.try_fold(1usize, |n, v| {
        Some(n.saturating_add(json_values(v, depth + 1)?))
    })
}
