//! A hit's sort values, typed by their keys (SPEC §7.4, §7.5.2; ROADMAP 10.1.30, ADR-0065 amended): what the response
//! JSON writes in a hit's `"sort"` array, as typed values.

use persia_engine::db::SearchHit;
use persia_engine::schema::CollectionSchema;
use persia_engine::search::SortKey;
use persia_engine::search::request::TypedSortValue;

use crate::{Error, ErrorCode, Result};

/// A hit's value for one key of the request's sort (SPEC §7.4), typed by the key: the score, or the value of the
/// key's field, as the sort compares it. [`Hit::sort_values`](crate::Hit::sort_values) holds one per key, and
/// [`SearchResponse::to_json`](crate::SearchResponse::to_json) writes each in the hit's `"sort"` array as its variant
/// says (SPEC §7.5.2).
///
/// ```
/// use persia::{SearchRequest, Sort, SortKey, SortValue, Order};
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// # let db = persia::Db::open(dir.path().join("app.persia"))?;
/// let shoes = db.create_collection_json(
///     r#"{"name": "shoes", "fields": {"title": {"type": "text"}, "price": {"type": "f64"}}}"#,
/// )?;
/// shoes.put_json(r#"{"id": "a", "title": "red shoes", "price": 80.5}"#)?;
/// shoes.put_json(r#"{"id": "b", "title": "red hat"}"#)?;
/// let schema = shoes.schema()?;
/// let price = schema.field("price").map(|f| f.id()).ok_or("no price")?;
/// let sort = Sort::new([SortKey::field(price, Order::Asc)], &schema)?;
/// let response = shoes.search(&SearchRequest::new("red").sort(sort))?;
/// assert_eq!(response.hits[0].sort_values(), [SortValue::F64(80.5)]);
/// assert_eq!(response.hits[1].sort_values(), [SortValue::Missing]); // `b` has no price: last ascending
/// let json: serde_json::Value = serde_json::from_str(&response.to_json()?)?;
/// assert_eq!(json["hits"][0]["sort"], serde_json::json!([80.5]));
/// assert_eq!(json["hits"][1]["sort"], serde_json::json!([null]));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SortValue {
    /// The hit's score, for the sort's score key (`SortKey::score()`, and a request without a sort): a JSON number.
    Score(f64),
    /// An `i64` field's value: a JSON integer.
    I64(i64),
    /// An `f64` field's value: a JSON number.
    F64(f64),
    /// A `datetime` field's value, microseconds since the Unix epoch (UTC), as [`Value::Datetime`](crate::Value)
    /// holds it: a JSON string in the output form of SPEC §6.4.1 (`"2023-11-14T22:13:20Z"`, with `.ffffff` when the
    /// microseconds are not zero).
    Datetime(i64),
    /// A `bool` field's value: `true` or `false`.
    Bool(bool),
    /// A `keyword` field's value, as the bytes the index sorts: a JSON string when they are UTF-8 (a document's
    /// keywords always are), else `{"base64url": "..."}` (SPEC §7.5.2).
    Keyword(Vec<u8>),
    /// The document has no value in the key's field: `null`, which keeps its place in the array. Where the hits
    /// without one sort is the key's `Missing` (SPEC §7.4).
    Missing,
}

impl SortValue {
    /// The facade's value of a sort value the engine typed by its key.
    fn of(key: SortKey, typed: TypedSortValue) -> Result<Self> {
        Ok(match typed {
            TypedSortValue::Missing => Self::Missing,
            TypedSortValue::Number(n) if matches!(key, SortKey::Score { .. }) => Self::Score(n),
            TypedSortValue::Number(n) => Self::F64(n),
            TypedSortValue::Integer(i) => Self::I64(i),
            TypedSortValue::Datetime(micros) => Self::Datetime(micros),
            TypedSortValue::Bool(b) => Self::Bool(b),
            TypedSortValue::Keyword(bytes) => Self::Keyword(bytes),
            // `#[non_exhaustive]`: a type of sort value this build does not know.
            _ => {
                return Err(Error::new(
                    ErrorCode::Unsupported,
                    "the engine returned a sort value of a type this build of persia does not read",
                ));
            }
        })
    }

    /// `hit`'s sort values, one per key of the sort that ranked it, typed by `schema` (the collection's that
    /// answered).
    ///
    /// # Errors
    /// [`ErrorCode::Io`], the code of a broken invariant (ADR-0063 item 8), for values that do not fit their keys: a
    /// count other than the keys', a value of another type than its key's field, a key on a field `schema` does not
    /// have. A hit a search of the collection returned always fits.
    pub(super) fn of_hit(hit: &SearchHit, schema: &CollectionSchema) -> Result<Vec<Self>> {
        let keys = hit.sort_keys();
        if keys.len() != hit.sort.len() {
            return Err(Error::new(
                ErrorCode::Io,
                format!(
                    "hit {:?} has {} sort values for {} sort keys",
                    hit.document.id(),
                    hit.sort.len(),
                    keys.len()
                ),
            ));
        }
        keys.iter()
            .zip(&hit.sort)
            .map(|(key, value)| {
                let typed = TypedSortValue::of(*key, value, schema).map_err(|e| {
                    Error::with_source(
                        ErrorCode::Io,
                        format_args!("hit {:?}", hit.document.id()),
                        e,
                    )
                })?;
                Self::of(*key, typed)
            })
            .collect()
    }
}
