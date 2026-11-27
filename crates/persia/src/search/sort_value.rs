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
            // `#[non_exhaustive]`: a kind of sort value the engine returned and this build does not know, for a key
            // of a schema this build read. A mismatch of the engine and the facade, not the caller's: the broken
            // invariant of ADR-0063 item 8.
            _ => {
                return Err(Error::new(
                    ErrorCode::Io,
                    "the engine returned a sort value of a kind this build of persia does not read",
                ));
            }
        })
    }

    /// `hit`'s sort values, one per key of the sort that ranked it, typed by `schema` (the collection's that answered).
    /// The engine's values are taken out of `hit` (its `sort` is left empty): a keyword's bytes move, not copied, so
    /// take the hit's cursor first.
    ///
    /// # Errors
    /// [`ErrorCode::Io`], the code of a broken invariant (ADR-0063 item 8), for values that do not fit their keys: a
    /// count other than the keys', a value of another kind than its key's field, a key on a field `schema` does not
    /// have, a `datetime` key out of range. None comes from stored data: the engine reads a column key that no
    /// document can hold as the corruption of its segment ([`ErrorCode::Corrupt`] from the search, before this
    /// runs), so a hit a search of the collection returned always fits.
    pub(super) fn take_from(hit: &mut SearchHit, schema: &CollectionSchema) -> Result<Vec<Self>> {
        let values = std::mem::take(&mut hit.sort);
        let keys = hit.sort_keys();
        let id = hit.document.id();
        if keys.len() != values.len() {
            return Err(Error::new(
                ErrorCode::Io,
                format!(
                    "hit {id:?} has {} sort values for {} sort keys",
                    values.len(),
                    keys.len()
                ),
            ));
        }
        keys.iter()
            .zip(values)
            .map(|(key, value)| {
                let typed = TypedSortValue::of_owned(*key, value, schema).map_err(|e| {
                    Error::with_source(ErrorCode::Io, format_args!("hit {id:?}"), e)
                })?;
                Self::of(*key, typed)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use persia_engine::db::SearchHit;
    use persia_engine::query::QueryLimits;
    use persia_engine::search::SortValue as EngineSortValue;
    use persia_engine::search::request::{QueryRequest, QueryResults, RunOptions};

    use super::*;
    use crate::{Collection, Db};

    /// A collection of two documents with a `datetime` and a `keyword`, and the engine's hits of `request` on it.
    fn hits(request: &str) -> (tempfile::TempDir, Db, Collection, Vec<SearchHit>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("app.persia")).unwrap();
        let c = db
            .create_collection_json(
                r#"{"name": "c", "fields": {"t": {"type": "text"}, "at": {"type": "datetime"},
                    "brand": {"type": "keyword"}}}"#,
            )
            .unwrap();
        c.put_json(r#"{"id": "a", "t": "red", "at": "2023-11-14T22:13:20Z", "brand": "acme"}"#)
            .unwrap();
        c.put_json(r#"{"id": "b", "t": "red", "at": 0, "brand": "zeta"}"#)
            .unwrap();
        let found = c
            .read(|s| {
                let parsed = QueryRequest::from_json(request, s.schema(), &QueryLimits::default())
                    .map_err(Error::wrap)?;
                let ran = parsed.run(s, &RunOptions::new()).map_err(Error::wrap)?;
                match ran.results {
                    QueryResults::Plain(results) => Ok(results.hits),
                    _ => panic!("a plain search"),
                }
            })
            .unwrap();

        (dir, db, c, found)
    }

    /// ROADMAP 10.1.30 review: real hits of the engine, tampered with, are refused with `Io` (a broken invariant),
    /// never typed: a value too few, a `datetime` key out of range, a value of another kind than its key; untouched,
    /// they are typed, and their keyword bytes are taken out of the hit.
    #[test]
    fn a_hit_whose_values_do_not_fit_its_keys_is_a_broken_invariant() {
        let (_dir, _db, c, found) = hits(
            r#"{"q": "red", "relax": false, "sort": [{"field": "at"}, {"field": "brand"}, "score"]}"#,
        );
        let schema = c.schema().unwrap();
        assert_eq!(found.len(), 2);
        let tamper = |f: fn(&mut Vec<EngineSortValue>)| {
            let mut hit = found[0].clone();
            f(&mut hit.sort);
            SortValue::take_from(&mut hit, &schema)
        };
        for (what, result) in [
            ("a value too few", tamper(|s| drop(s.pop()))),
            (
                "a value too many",
                tamper(|s| s.push(EngineSortValue::Missing)),
            ),
            (
                "a datetime out of range",
                tamper(|s| s[0] = EngineSortValue::Key(u64::MAX)),
            ),
            (
                "a keyword for a datetime",
                tamper(|s| s[0] = EngineSortValue::Keyword(b"x".to_vec())),
            ),
            (
                "a key for a keyword",
                tamper(|s| s[1] = EngineSortValue::Key(1)),
            ),
            (
                "a key for the score",
                tamper(|s| s[2] = EngineSortValue::Key(1)),
            ),
        ] {
            let err = result.unwrap_err();
            assert_eq!(err.code(), ErrorCode::Io, "{what}: {err}");
        }
        // `b`'s `at` is 0, so it ranks first ascending.
        let mut hit = found[0].clone();
        let typed = SortValue::take_from(&mut hit, &schema).unwrap();
        assert_eq!(
            typed,
            [
                SortValue::Datetime(0),
                SortValue::Keyword(b"zeta".to_vec()),
                SortValue::Score(found[0].score),
            ]
        );
        assert!(hit.sort.is_empty(), "the values were taken out of the hit");
        // The facade's hit of it: typed the same, its cursor taken before the values.
        let facade = super::super::plain_hit(found[0].clone(), &schema).unwrap();
        assert_eq!(facade.sort_values(), typed);
        assert_eq!(facade.cursor, found[0].cursor().to_token());
        let mut broken = found[1].clone();
        broken.sort[0] = EngineSortValue::Key(u64::MAX);
        let err = super::super::plain_hit(broken, &schema).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Io, "{err}");
    }
}
