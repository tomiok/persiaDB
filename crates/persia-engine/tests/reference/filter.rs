//! Reference filter evaluation (SPEC §7.1.2) over a document's values as given: no postings, no column keys.

#![allow(dead_code)]

use std::cmp::Ordering;
use std::ops::Bound;

use persia_engine::document::{Document, Value};
use persia_engine::query::{Filter, FilterValue};
use persia_engine::schema::{CollectionSchema, FieldId, FieldType};

/// Whether `filter` selects `doc`, a document of `schema`.
pub fn selects(filter: &Filter, doc: &Document, schema: &CollectionSchema) -> bool {
    match filter {
        Filter::Term { field, value } => values(doc, *field).iter().any(|v| equal(v, value)),
        Filter::Terms { field, values: any } => values(doc, *field)
            .iter()
            .any(|v| any.iter().any(|w| equal(v, w))),
        Filter::Range {
            field,
            lower,
            upper,
        } => values(doc, *field)
            .iter()
            .any(|v| holds(v, lower, Ordering::Greater) && holds(v, upper, Ordering::Less)),
        Filter::Exists { field } => exists(doc, *field, schema),
        Filter::And(all) => all.iter().all(|f| selects(f, doc, schema)),
        Filter::Or(any) => any.iter().any(|f| selects(f, doc, schema)),
        Filter::Not(inner) => !selects(inner, doc, schema),
        other => panic!("no reference for {other:?}"),
    }
}

/// The values of `field` in `doc`, as filter values.
fn values(doc: &Document, field: FieldId) -> Vec<FilterValue> {
    match doc.get(field) {
        None => Vec::new(),
        Some(Value::String(s)) => vec![FilterValue::Keyword(s.clone())],
        Some(Value::Strings(list)) => list.iter().cloned().map(FilterValue::Keyword).collect(),
        Some(Value::I64(v)) => vec![FilterValue::I64(*v)],
        Some(Value::F64(v)) => vec![FilterValue::F64(*v)],
        Some(Value::Bool(v)) => vec![FilterValue::Bool(*v)],
        Some(Value::Datetime(v)) => vec![FilterValue::Datetime(*v)],
        Some(other) => panic!("no filter value for {other:?}"),
    }
}

/// `-0.0` equals `0.0`: `==` on floats.
fn equal(a: &FilterValue, b: &FilterValue) -> bool {
    a == b
}

fn compare(a: &FilterValue, b: &FilterValue) -> Option<Ordering> {
    match (a, b) {
        (FilterValue::I64(a), FilterValue::I64(b))
        | (FilterValue::Datetime(a), FilterValue::Datetime(b)) => Some(a.cmp(b)),
        (FilterValue::F64(a), FilterValue::F64(b)) => a.partial_cmp(b),
        _ => None,
    }
}

/// Whether `v` is on the inner side of `bound`: `side` is how `v` compares to a strict bound it passes.
fn holds(v: &FilterValue, bound: &Bound<FilterValue>, side: Ordering) -> bool {
    match bound {
        Bound::Unbounded => true,
        Bound::Included(b) => compare(v, b).is_some_and(|o| o == side || o == Ordering::Equal),
        Bound::Excluded(b) => compare(v, b) == Some(side),
    }
}

/// A value in the field; a `text` field needs a token, and the tests' analyzer splits on whitespace at least.
fn exists(doc: &Document, field: FieldId, schema: &CollectionSchema) -> bool {
    let text = schema
        .field_by_id(field)
        .is_some_and(|f| f.field_type() == FieldType::Text);
    match doc.get(field) {
        None => false,
        Some(Value::String(s)) if text => s.split_whitespace().next().is_some(),
        Some(Value::Strings(list)) if text => {
            list.iter().any(|s| s.split_whitespace().next().is_some())
        }
        Some(Value::Strings(list)) => !list.is_empty(),
        Some(_) => true,
    }
}
