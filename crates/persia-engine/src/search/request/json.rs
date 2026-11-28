//! The request JSON (SPEC §7.1, ADR-0062): one object, read as strictly as a collection definition (SPEC §6.2.1) and
//! a filter (SPEC §7.1.2), into a [`QueryRequest`].
//!
//! Requests are written by programs, so nothing is guessed: an unknown key, a repeated key at any depth, `null` for a
//! key and a value of another shape than the key's are errors that name the key. Only the text of `q` never fails
//! (SPEC §7.1.1). Each key's value is read by the parser of its own section: `filter` and `soft_filter` by the filter
//! DSL, `relax` as a query's `relax` (SPEC §7.6), `fields` as the default fields, `search_after` as a cursor token,
//! `projection` as the fields a hit returns (SPEC §7.5.1).

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Map, Value as J};

use super::query::{QueryRequest, QueryRequestError};
use crate::db::Cursor;
use crate::document::json::{Strict, json_kind};
use crate::query::{DefaultFields, Filter, MAX_SHOWN, QueryError, QueryLimits, shown_name};
use crate::relax::RelaxOptions;
use crate::schema::{CollectionSchema, Field};
use crate::search::{
    DEFAULT_FACET_SIZE, FacetRequest, MAX_FACETS, Missing, Order, Projection, RequestError, Sort,
    SortKey,
};

/// The longest request JSON [`QueryRequest::from_json`] reads, in bytes (SPEC §7.1): the bound of a filter's JSON
/// (SPEC §7.1.2), which a request holds.
pub const MAX_REQUEST_JSON_LEN: usize = crate::query::MAX_FILTER_JSON_LEN;

impl QueryRequest {
    /// Parses the request JSON (SPEC §7.1) for a collection of `schema`, holding its filters to `limits` (SPEC
    /// §7.1.3). The keys are `q`, `fields`, `filter`, `soft_filter`, `sort`, `limit`, `offset`, `search_after`,
    /// `facets`, `relax`, `explain`, `proximity`, `projection` and `timeout_ms`, each optional: a key left out has
    /// [`QueryRequest::default`]'s value.
    ///
    /// ```
    /// use persia_engine::query::QueryLimits;
    /// use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
    /// use persia_engine::search::request::{QueryRequest, QueryRequestError};
    /// use persia_engine::search::Projection;
    ///
    /// let schema = SchemaBuilder::new("shoes")
    ///     .field("title", FieldKind::Text(TextOptions::default()))
    ///     .field("brand", FieldKind::Keyword(KeywordOptions::default().facet(true)))
    ///     .field("price", FieldKind::F64 { stored: true })
    ///     .build()?;
    /// let limits = QueryLimits::default();
    /// let request = QueryRequest::from_json(
    ///     r#"{
    ///         "q": "zapatillas rojas",
    ///         "fields": ["title^3"],
    ///         "filter": {"term": ["brand", "nike"]},
    ///         "soft_filter": {"range": ["price", {"lte": 100}]},
    ///         "sort": ["score", {"field": "price", "order": "asc"}],
    ///         "limit": 10,
    ///         "facets": ["brand", {"field": "brand", "size": 3}],
    ///         "relax": {"min_results": 5},
    ///         "explain": true,
    ///         "projection": ["price"]
    ///     }"#,
    ///     &schema,
    ///     &limits,
    /// )?;
    /// assert_eq!((request.q(), request.page().limit, request.relax().min_results), ("zapatillas rojas", 10, Some(5)));
    /// // Each hit returns its `price` alone (and its id and blob reference).
    /// assert_eq!(request.page().projection, Projection::fields(["price"], &schema)?);
    /// // Written by programs, read strictly: an unknown key is an error, not ignored.
    /// let unknown = QueryRequest::from_json(r#"{"query": "shoes"}"#, &schema, &limits);
    /// assert!(matches!(unknown, Err(QueryRequestError::Json(_))));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`QueryRequestError::Json`] for JSON longer than [`MAX_REQUEST_JSON_LEN`], malformed, not an object, with an
    /// unknown or repeated key, `null` for a key, or a value of another shape than its key's;
    /// [`QueryRequestError::Query`] for `fields`, a filter or a sort or facet field that does not fit the schema or
    /// the limits; [`QueryRequestError::Relax`] for a `relax` that is not its form or breaks a rule;
    /// [`QueryRequestError::Cursor`] for a `search_after` that is not a cursor token;
    /// [`QueryRequestError::Request`] for a sort or a facet the schema does not allow, or too many of them, and for a
    /// `projection` name that is not a stored field (or, in a `strict` collection, not a field at all).
    pub fn from_json(
        json: &str,
        schema: &CollectionSchema,
        limits: &QueryLimits,
    ) -> Result<Self, QueryRequestError> {
        if json.len() > MAX_REQUEST_JSON_LEN {
            return Err(shape(format!(
                "{} bytes, at most {MAX_REQUEST_JSON_LEN}",
                json.len()
            )));
        }
        let Strict(value) = serde_json::from_str(json).map_err(|e| shape(e.to_string()))?;
        let J::Object(object) = value else {
            return Err(shape(format!(
                "expected an object, found {}",
                json_kind(&value)
            )));
        };
        let mut request = Self::default();
        for (key, value) in object {
            if value.is_null() {
                return Err(shape(match key.len() {
                    0..=MAX_SHOWN => format!("`{key}` is null"),
                    len => format!("a key of {len} bytes is null"),
                }));
            }
            match key.as_str() {
                "q" => request.q = string(&key, value)?,
                "fields" => {
                    let specs = strings(&key, &value)?;
                    request.fields = Some(DefaultFields::parse(schema, &specs)?);
                }
                "filter" => request.filter = Some(Filter::from_value(&value, schema, limits)?),
                "soft_filter" => {
                    request.soft_filter = Some(Filter::from_value(&value, schema, limits)?);
                }
                "sort" => request.page.sort = sort(&value, schema)?,
                "limit" => request.page.limit = count(&key, &value)?,
                "offset" => request.page.offset = count(&key, &value)?,
                "search_after" => {
                    request.page.search_after = Some(Cursor::from_token(&string(&key, value)?)?);
                }
                "facets" => request.page.facets = facets(&value, schema)?,
                "relax" => request.relax = RelaxOptions::from_value(value)?,
                "explain" => request.page.explain = boolean(&key, &value)?,
                "proximity" => request.proximity = boolean(&key, &value)?,
                "projection" => request.page.projection = projection(&value, schema)?,
                "timeout_ms" => {
                    request.timeout = Some(Duration::from_millis(millis(&key, &value)?));
                }
                _ => return Err(shape(format!("unknown key {}", shown_key(&key)))),
            }
        }
        Ok(request)
    }
}

fn shape(message: impl Into<String>) -> QueryRequestError {
    QueryRequestError::Json(message.into())
}

/// `key` takes `expected`, and `value` is something else.
fn wrong(key: &str, expected: &str, value: &J) -> QueryRequestError {
    shape(format!(
        "`{key}` takes {expected}, found {}",
        json_kind(value)
    ))
}

fn string(key: &str, value: J) -> Result<String, QueryRequestError> {
    match value {
        J::String(s) => Ok(s),
        other => Err(wrong(key, "a string", &other)),
    }
}

fn boolean(key: &str, value: &J) -> Result<bool, QueryRequestError> {
    value
        .as_bool()
        .ok_or_else(|| wrong(key, "true or false", value))
}

/// A count of hits: an integer of at least 0, written without a fraction or an exponent. `-0`, which JSON reads as a
/// number of its own (a negative zero), is refused with its own message (SPEC §7.1.4).
fn count(key: &str, value: &J) -> Result<usize, QueryRequestError> {
    if value
        .as_f64()
        .is_some_and(|n| n == 0.0 && n.is_sign_negative())
    {
        return Err(shape(format!(
            "`{key}` takes an integer of at least 0: -0 is not a valid count, write 0"
        )));
    }
    value
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| wrong(key, "an integer of at least 0", value))
}

/// A number of milliseconds (`timeout_ms`, SPEC §7.1.4): an integer of at least 0, written without a fraction or an
/// exponent, up to `u64::MAX` (an end past what the clock holds is no end, [`QueryRequest::with_timeout`]). `-0` is
/// refused with its own message, as a count's.
fn millis(key: &str, value: &J) -> Result<u64, QueryRequestError> {
    if value
        .as_f64()
        .is_some_and(|n| n == 0.0 && n.is_sign_negative())
    {
        return Err(shape(format!(
            "`{key}` takes an integer of at least 0: -0 is not a number of milliseconds, write 0"
        )));
    }
    value
        .as_u64()
        .ok_or_else(|| wrong(key, "an integer of milliseconds of at least 0", value))
}

fn strings(key: &str, value: &J) -> Result<Vec<String>, QueryRequestError> {
    let expected = "an array of strings";
    let J::Array(items) = value else {
        return Err(wrong(key, expected, value));
    };
    items
        .iter()
        .map(|item| match item {
            J::String(s) => Ok(s.clone()),
            other => Err(wrong(key, expected, other)),
        })
        .collect()
}

/// The field of `schema` named `name`.
fn field<'s>(schema: &'s CollectionSchema, name: &str) -> Result<&'s Field, QueryRequestError> {
    Ok(schema.field(name).ok_or_else(|| QueryError::UnknownField {
        field: name.to_owned(),
    })?)
}

/// `sort` (SPEC §7.4): `"score"`, or a list of keys, each `"score"`, `{"score": {"order"}}` or
/// `{"field", "order", "missing"}`.
fn sort(value: &J, schema: &CollectionSchema) -> Result<Sort, QueryRequestError> {
    match value {
        J::String(s) if s == "score" => Ok(Sort::score()),
        // Nothing is guessed (SPEC §7.1.4): an empty list names no order, and `"score"` is how to ask for the default.
        J::Array(keys) if keys.is_empty() => Err(shape(
            "`sort` takes \"score\" or a non-empty array of sort keys: [] names no order",
        )),
        J::Array(keys) => {
            let keys = keys
                .iter()
                .map(|key| sort_key(key, schema))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Sort::new(keys, schema)?)
        }
        other => Err(wrong("sort", "\"score\" or an array of sort keys", other)),
    }
}

fn sort_key(value: &J, schema: &CollectionSchema) -> Result<SortKey, QueryRequestError> {
    let expected = "\"score\", {\"score\": {\"order\"}} or {\"field\", \"order\", \"missing\"}";
    match value {
        J::String(s) if s == "score" => Ok(SortKey::score()),
        J::Object(key) => {
            if let Some(options) = key.get("score") {
                return score_key(key, options);
            }
            only(key, "a `sort` field key", &["field", "order", "missing"])?;
            let name = name_of(key, "a `sort` key")?;
            Ok(SortKey::Field {
                field: field(schema, name)?.id(),
                order: order(key)?.unwrap_or(Order::Asc),
                missing: missing(key)?,
            })
        }
        other => Err(wrong("sort", expected, other)),
    }
}

/// `{"score": {"order"}}`, whose `score` is `options`: the score, descending unless it says otherwise.
fn score_key(key: &Map<String, J>, options: &J) -> Result<SortKey, QueryRequestError> {
    only(key, "a `sort` score key", &["score"])?;
    let J::Object(options) = options else {
        return Err(wrong("sort", "{\"score\": {\"order\"}}", options));
    };
    only(options, "a `sort` score key", &["order"])?;
    Ok(SortKey::Score {
        order: order(options)?.unwrap_or(Order::Desc),
    })
}

/// An error unless every key of `object` is one of `allowed`.
fn only(object: &Map<String, J>, what: &str, allowed: &[&str]) -> Result<(), QueryRequestError> {
    match object.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(shape(format!("unknown key {} in {what}", shown_key(key)))),
        None => Ok(()),
    }
}

/// The `"field"` of `object`: a field name.
fn name_of<'j>(object: &'j Map<String, J>, what: &str) -> Result<&'j str, QueryRequestError> {
    match object.get("field") {
        Some(J::String(name)) => Ok(name),
        Some(other) => Err(shape(format!(
            "`field` of {what} takes a field name, found {}",
            json_kind(other)
        ))),
        None => Err(shape(format!("{what} needs a `field`"))),
    }
}

fn order(key: &Map<String, J>) -> Result<Option<Order>, QueryRequestError> {
    match key.get("order") {
        None => Ok(None),
        Some(J::String(s)) if s == "asc" => Ok(Some(Order::Asc)),
        Some(J::String(s)) if s == "desc" => Ok(Some(Order::Desc)),
        Some(other) => Err(shape(format!(
            "`order` of a `sort` key is \"asc\" or \"desc\", found {}",
            shown(other)
        ))),
    }
}

fn missing(key: &Map<String, J>) -> Result<Missing, QueryRequestError> {
    match key.get("missing") {
        None => Ok(Missing::Last),
        Some(J::String(s)) if s == "last" => Ok(Missing::Last),
        Some(J::String(s)) if s == "first" => Ok(Missing::First),
        Some(other) => Err(shape(format!(
            "`missing` of a `sort` key is \"last\" or \"first\", found {}",
            shown(other)
        ))),
    }
}

/// `facets` (SPEC §7.5): a list of at most [`MAX_FACETS`], each a field name (its size [`DEFAULT_FACET_SIZE`]) or
/// `{"field", "size"}`.
fn facets(value: &J, schema: &CollectionSchema) -> Result<Vec<FacetRequest>, QueryRequestError> {
    let expected = "an array of field names or {\"field\", \"size\"}";
    let J::Array(items) = value else {
        return Err(wrong("facets", expected, value));
    };
    if items.len() > MAX_FACETS {
        return Err(RequestError::TooManyFacets {
            count: items.len(),
            max: MAX_FACETS,
        }
        .into());
    }
    items
        .iter()
        .map(|item| {
            let (name, size) = match item {
                J::String(name) => (name.as_str(), DEFAULT_FACET_SIZE),
                J::Object(facet) => {
                    only(facet, "a facet", &["field", "size"])?;
                    let size = match facet.get("size") {
                        None => DEFAULT_FACET_SIZE,
                        Some(size) => count("size", size)?,
                    };
                    (name_of(facet, "a facet")?, size)
                }
                other => return Err(wrong("facets", expected, other)),
            };
            Ok(FacetRequest::new(field(schema, name)?.id(), size, schema)?)
        })
        .collect()
}

/// `value` as an error names it: a short string as written, anything else by its kind, so a message stays short
/// whatever the request holds.
fn shown(value: &J) -> String {
    match value {
        J::String(s) if s.len() <= MAX_SHOWN => format!("{s:?}"),
        other => json_kind(other).to_owned(),
    }
}

/// An object's key as an error names it: as [`shown`] gives a short one, a longer one by its length, so a message
/// stays short whatever the request holds (a key may be up to the request's 1 MiB). Field names, `fields` entries and
/// filter keys are named the same way ([`crate::query::shown_name`]).
fn shown_key(key: &str) -> String {
    shown_name(key)
}

/// `projection` (SPEC §7.1.4, §7.5.1, ADR-0065): `"all"` (every stored and extra field), `"none"` (the id and the blob
/// reference only), or a non-empty array of distinct names, each a stored field of the schema or, in a collection that
/// is not `strict`, an extra field ([`Projection::fields`]).
fn projection(value: &J, schema: &CollectionSchema) -> Result<Projection, QueryRequestError> {
    let expected = "\"all\", \"none\" or a non-empty array of field names";
    match value {
        J::String(s) if s == "all" => Ok(Projection::All),
        J::String(s) if s == "none" => Ok(Projection::None),
        // Nothing is guessed (SPEC §7.1.4): `[]` would read as "none" to some and as "all" to others.
        J::Array(names) if names.is_empty() => Err(shape(format!(
            "`projection` takes {expected}: [] names no field, write \"none\""
        ))),
        J::Array(_) => {
            let names = strings("projection", value)?;
            let mut seen = BTreeSet::new();
            if let Some(name) = names.iter().find(|n| !seen.insert(n.as_str())) {
                return Err(shape(format!(
                    "`projection` names {} twice",
                    shown_key(name)
                )));
            }
            Ok(Projection::fields(&names, schema)?)
        }
        other => Err(shape(format!(
            "`projection` takes {expected}, found {}",
            shown(other)
        ))),
    }
}
