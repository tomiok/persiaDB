//! The filter DSL (SPEC §7.1.2): structured predicates over `keyword`, numeric, `bool` and `datetime` fields that
//! select documents without scoring.

use std::ops::Bound;

use serde_json::Value as J;

use crate::document::MAX_KEYWORD_LEN;
use crate::document::datetime;
use crate::document::json::{Strict, integer, json_kind};
use crate::schema::{CollectionSchema, Field, FieldId, FieldType};

use super::QueryError;
use super::ast::field_of;
use super::limits::{QueryLimits, Usage};

/// Longest filter JSON [`Filter::from_json`] reads, in bytes (SPEC §7.1.2).
pub const MAX_FILTER_JSON_LEN: usize = 1 << 20;

/// A filter (SPEC §7.1.2). Built by [`Filter::from_json`], which validates it, or by hand and checked with
/// [`Filter::validate`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Filter {
    /// Documents whose field holds the value. Fields: `keyword`, `i64`, `f64`, `bool`, `datetime`.
    Term {
        /// The field.
        field: FieldId,
        /// The value, of the field's type.
        value: FilterValue,
    },
    /// Documents whose field holds any of the values (none: no document). Fields as [`Filter::Term`].
    Terms {
        /// The field.
        field: FieldId,
        /// The values, of the field's type.
        values: Vec<FilterValue>,
    },
    /// Documents whose field's value lies within the bounds. Fields: `i64`, `f64`, `datetime`.
    Range {
        /// The field.
        field: FieldId,
        /// The lower bound.
        lower: Bound<FilterValue>,
        /// The upper bound; at least one of the two is bounded.
        upper: Bound<FilterValue>,
    },
    /// Documents that have a value in the field (a `text` field: at least one token). Any field but `stored`.
    Exists {
        /// The field.
        field: FieldId,
    },
    /// Documents every filter selects (none: every document).
    And(Vec<Filter>),
    /// Documents any filter selects (none: no document).
    Or(Vec<Filter>),
    /// Documents the filter does not select.
    Not(Box<Filter>),
}

/// A value of a [`Filter`], typed like the field it applies to (SPEC §6.4.1).
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    /// A `keyword` value, at most [`MAX_KEYWORD_LEN`] bytes.
    Keyword(String),
    /// An `i64` value.
    I64(i64),
    /// An `f64` value, not NaN; `-0.0` equals `0.0` (matching normalizes it: a hand-built filter may hold it).
    F64(f64),
    /// A `bool` value.
    Bool(bool),
    /// A `datetime` value: microseconds in [`crate::document::MIN_DATETIME`]`..=`[`crate::document::MAX_DATETIME`].
    Datetime(i64),
}

impl Filter {
    /// Parses the JSON form of a filter (SPEC §7.1.2) and validates it against `schema` and `limits`.
    ///
    /// ```
    /// use persia_engine::query::{Filter, FilterValue, QueryLimits};
    /// use persia_engine::schema::{FieldKind, SchemaBuilder};
    ///
    /// let schema = SchemaBuilder::new("events").field("at", FieldKind::Datetime { stored: true }).build()?;
    /// let filter = Filter::from_json(r#"{"range": ["at", {"gte": "2026-01-01T00:00:00Z"}]}"#, &schema, &QueryLimits::default())?;
    /// let Filter::Range { lower, .. } = filter else { unreachable!() };
    /// assert_eq!(lower, std::ops::Bound::Included(FilterValue::Datetime(1_767_225_600_000_000)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`QueryError::FilterSyntax`] for JSON that is malformed, has duplicate keys or is not of the DSL's shape;
    /// otherwise the errors of [`Filter::validate`] and of values that do not fit their field.
    pub fn from_json(
        json: &str,
        schema: &CollectionSchema,
        limits: &QueryLimits,
    ) -> Result<Self, QueryError> {
        if json.len() > MAX_FILTER_JSON_LEN {
            return Err(syntax(format!(
                "{} bytes, at most {MAX_FILTER_JSON_LEN}",
                json.len()
            )));
        }
        let Strict(value) = serde_json::from_str(json).map_err(|e| syntax(e.to_string()))?;
        let filter = parse(&value, schema)?;
        filter.validate(schema, limits)?;
        Ok(filter)
    }

    /// Checks the filter against `schema` and `limits`: fields exist and have a type the filter applies to, values
    /// have the field's type and range, every range has a bound, and the filter is within the limits.
    ///
    /// # Errors
    /// The first problem found, depth first.
    pub fn validate(
        &self,
        schema: &CollectionSchema,
        limits: &QueryLimits,
    ) -> Result<(), QueryError> {
        self.check(schema, limits, &mut Usage::default(), 1)
    }

    pub(super) fn check(
        &self,
        schema: &CollectionSchema,
        limits: &QueryLimits,
        usage: &mut Usage,
        depth: usize,
    ) -> Result<(), QueryError> {
        let child = depth.saturating_add(1);
        match self {
            Self::Term { field, value } => {
                usage.add(limits, depth, 1, 0)?;
                let field = field_of(schema, *field, "a term filter", has_values)?;
                value_fits(field, value)
            }
            Self::Terms { field, values } => {
                usage.add(limits, depth, values.len(), 0)?;
                let field = field_of(schema, *field, "a terms filter", has_values)?;
                values.iter().try_for_each(|v| value_fits(field, v))
            }
            Self::Range {
                field,
                lower,
                upper,
            } => {
                let mut bounds = [lower, upper].into_iter().filter_map(bound_value);
                usage.add(limits, depth, bounds.clone().count(), 0)?;
                let field = field_of(schema, *field, "a range filter", is_ordered)?;
                if bounds.clone().next().is_none() {
                    return Err(range_problem(field, "needs a bound"));
                }
                bounds.try_for_each(|v| value_fits(field, v))
            }
            Self::Exists { field } => {
                usage.add(limits, depth, 0, 0)?;
                field_of(schema, *field, "an exists filter", |t| {
                    t != FieldType::Stored
                })
                .map(|_| ())
            }
            Self::And(all) | Self::Or(all) => {
                usage.add(limits, depth, 0, 0)?;
                all.iter()
                    .try_for_each(|f| f.check(schema, limits, usage, child))
            }
            Self::Not(inner) => {
                usage.add(limits, depth, 0, 0)?;
                inner.check(schema, limits, usage, child)
            }
        }
    }
}

fn has_values(t: FieldType) -> bool {
    matches!(
        t,
        FieldType::Keyword
            | FieldType::I64
            | FieldType::F64
            | FieldType::Bool
            | FieldType::Datetime
    )
}

fn is_ordered(t: FieldType) -> bool {
    matches!(t, FieldType::I64 | FieldType::F64 | FieldType::Datetime)
}

fn bound_value(bound: &Bound<FilterValue>) -> Option<&FilterValue> {
    match bound {
        Bound::Included(v) | Bound::Excluded(v) => Some(v),
        Bound::Unbounded => None,
    }
}

/// Whether `value` has `field`'s type and is in its range.
fn value_fits(field: &Field, value: &FilterValue) -> Result<(), QueryError> {
    let ok = match (field.field_type(), value) {
        (FieldType::Keyword, FilterValue::Keyword(s)) => s.len() <= MAX_KEYWORD_LEN,
        (FieldType::I64, FilterValue::I64(_)) | (FieldType::Bool, FilterValue::Bool(_)) => true,
        (FieldType::F64, FilterValue::F64(v)) => !v.is_nan(),
        (FieldType::Datetime, FilterValue::Datetime(v)) => datetime::in_range(*v),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(QueryError::FilterValue {
            field: field.name().to_owned(),
            expected: expected(field.field_type()),
            found: format!("{value:?}"),
        })
    }
}

fn expected(t: FieldType) -> &'static str {
    match t {
        FieldType::Keyword => "a string of at most 32768 bytes",
        FieldType::I64 => "an integer in the i64 range",
        FieldType::F64 => "a number",
        FieldType::Bool => "a bool",
        FieldType::Datetime => {
            "microseconds or an RFC 3339 string in 0000-01-01T00:00:00Z..=9999-12-31T23:59:59.999999Z"
        }
        FieldType::Text | FieldType::Stored => "no value",
    }
}

fn syntax(message: impl Into<String>) -> QueryError {
    QueryError::FilterSyntax(message.into())
}

fn range_problem(field: &Field, problem: &'static str) -> QueryError {
    QueryError::InvalidRange {
        field: field.name().to_owned(),
        problem,
    }
}

/// One filter object: exactly one key, the operator.
fn parse(value: &J, schema: &CollectionSchema) -> Result<Filter, QueryError> {
    let J::Object(object) = value else {
        return Err(syntax(format!(
            "expected a filter object, found {}",
            json_kind(value)
        )));
    };
    let mut entries = object.iter();
    let (Some((op, arg)), None) = (entries.next(), entries.next()) else {
        return Err(syntax(format!(
            "a filter object has exactly one key, found {}",
            object.len()
        )));
    };
    match op.as_str() {
        "term" => {
            let (field, value) = field_and(arg, op, schema)?;
            field_of(schema, field.id(), "a term filter", has_values)?;
            Ok(Filter::Term {
                field: field.id(),
                value: to_value(field, value)?,
            })
        }
        "terms" => {
            let (field, values) = field_and(arg, op, schema)?;
            field_of(schema, field.id(), "a terms filter", has_values)?;
            let J::Array(values) = values else {
                return Err(syntax(format!(
                    "`terms` takes an array of values, found {}",
                    json_kind(values)
                )));
            };
            Ok(Filter::Terms {
                field: field.id(),
                values: values
                    .iter()
                    .map(|v| to_value(field, v))
                    .collect::<Result<_, _>>()?,
            })
        }
        "range" => {
            let (field, bounds) = field_and(arg, op, schema)?;
            field_of(schema, field.id(), "a range filter", is_ordered)?;
            let (lower, upper) = parse_bounds(field, bounds)?;
            Ok(Filter::Range {
                field: field.id(),
                lower,
                upper,
            })
        }
        "exists" => Ok(Filter::Exists {
            field: named_field(arg, op, schema)?.id(),
        }),
        "and" | "or" => {
            let J::Array(items) = arg else {
                return Err(syntax(format!(
                    "`{op}` takes an array of filters, found {}",
                    json_kind(arg)
                )));
            };
            let all = items
                .iter()
                .map(|f| parse(f, schema))
                .collect::<Result<_, _>>()?;
            Ok(if op == "and" {
                Filter::And(all)
            } else {
                Filter::Or(all)
            })
        }
        "not" => Ok(Filter::Not(Box::new(parse(arg, schema)?))),
        other => Err(syntax(format!("unknown filter {other:?}"))),
    }
}

/// `[field, value]`.
fn field_and<'a, 'j>(
    arg: &'j J,
    op: &str,
    schema: &'a CollectionSchema,
) -> Result<(&'a Field, &'j J), QueryError> {
    match arg {
        J::Array(pair) => match pair.as_slice() {
            [name, value] => Ok((named_field(name, op, schema)?, value)),
            _ => Err(syntax(format!(
                "`{op}` takes [field, value], found {} elements",
                pair.len()
            ))),
        },
        _ => Err(syntax(format!(
            "`{op}` takes [field, value], found {}",
            json_kind(arg)
        ))),
    }
}

fn named_field<'a>(
    name: &J,
    op: &str,
    schema: &'a CollectionSchema,
) -> Result<&'a Field, QueryError> {
    let J::String(name) = name else {
        return Err(syntax(format!(
            "`{op}` takes a field name, found {}",
            json_kind(name)
        )));
    };
    schema.field(name).ok_or_else(|| QueryError::UnknownField {
        field: name.clone(),
    })
}

/// `{"gt"|"gte": v, "lt"|"lte": v}`: at least one bound, at most one per side.
fn parse_bounds(
    field: &Field,
    bounds: &J,
) -> Result<(Bound<FilterValue>, Bound<FilterValue>), QueryError> {
    let J::Object(bounds) = bounds else {
        return Err(syntax(format!(
            "`range` takes [field, {{bounds}}], found {}",
            json_kind(bounds)
        )));
    };
    let (mut lower, mut upper) = (Bound::Unbounded, Bound::Unbounded);
    for (key, value) in bounds {
        let (side, bound) = match key.as_str() {
            "gt" => (&mut lower, Bound::Excluded(to_value(field, value)?)),
            "gte" => (&mut lower, Bound::Included(to_value(field, value)?)),
            "lt" => (&mut upper, Bound::Excluded(to_value(field, value)?)),
            "lte" => (&mut upper, Bound::Included(to_value(field, value)?)),
            other => return Err(syntax(format!("unknown range bound {other:?}"))),
        };
        if !matches!(side, Bound::Unbounded) {
            return Err(range_problem(field, "two bounds on one side"));
        }
        *side = bound;
    }
    Ok((lower, upper))
}

/// A JSON value as a value of `field` (SPEC §6.4.1 forms).
fn to_value(field: &Field, value: &J) -> Result<FilterValue, QueryError> {
    let converted = match (field.field_type(), value) {
        (FieldType::Keyword, J::String(s)) => Some(FilterValue::Keyword(s.clone())),
        // Integers as in documents: a negative zero written with a fraction or exponent is 0 (SPEC §6.4.1).
        (FieldType::I64, J::Number(n)) => integer(field.name(), n).ok().map(FilterValue::I64),
        (FieldType::F64, J::Number(n)) => n.as_f64().map(|v| FilterValue::F64(v + 0.0)),
        (FieldType::Bool, J::Bool(b)) => Some(FilterValue::Bool(*b)),
        (FieldType::Datetime, J::Number(n)) => {
            integer(field.name(), n).ok().map(FilterValue::Datetime)
        }
        (FieldType::Datetime, J::String(s)) => datetime::parse(s).map(FilterValue::Datetime),
        // Callers check the field's type first; `text` and `stored` take no value.
        _ => None,
    };
    converted.ok_or_else(|| QueryError::FilterValue {
        field: field.name().to_owned(),
        expected: expected(field.field_type()),
        found: json_kind(value).to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{MAX_DATETIME, MIN_DATETIME};
    use crate::schema::{FieldKind, KeywordOptions, SchemaBuilder};

    #[test]
    fn datetime_bounds_match_the_document_range() {
        // `expected` spells the range out; keep it in step with the document limits.
        assert!(expected(FieldType::Datetime).ends_with(datetime::RANGE));
        assert!(datetime::in_range(MIN_DATETIME) && datetime::in_range(MAX_DATETIME));
        assert_eq!(
            expected(FieldType::Keyword),
            format!("a string of at most {MAX_KEYWORD_LEN} bytes")
        );
    }

    #[test]
    fn negative_zero_is_zero() {
        let schema = SchemaBuilder::new("c")
            .field("f", FieldKind::F64 { stored: true })
            .field("i", FieldKind::I64 { stored: true })
            .field("k", FieldKind::Keyword(KeywordOptions::default()))
            .build()
            .unwrap();
        let limits = QueryLimits::default();
        let f = Filter::from_json(r#"{"term": ["f", -0.0]}"#, &schema, &limits).unwrap();
        let Filter::Term {
            value: FilterValue::F64(v),
            ..
        } = f
        else {
            panic!("{f:?}")
        };
        assert!(v == 0.0 && v.is_sign_positive());
        let i = Filter::from_json(r#"{"term": ["i", -0.0]}"#, &schema, &limits).unwrap();
        assert!(matches!(
            i,
            Filter::Term {
                value: FilterValue::I64(0),
                ..
            }
        ));
    }
}
