//! The response JSON (SPEC §7.5.2, ADR-0065): a [`QueryResponse`] as one JSON object, written by the engine beside the
//! request JSON it answers, so the embedded API, its bindings and the server write the same text.
//!
//! The object is written by hand, so its keys come in the order SPEC §7.5.2 lists them whatever `serde_json`'s map
//! type: a golden response pins the text. A value that is absent is left out, never `null`; the only `null` is a
//! missing sort value, which keeps its place among the hit's sort values.

use persia_format::for_codec::{key_to_f64, key_to_i64};
use serde_json::{Number, Value as J};

use super::base64url;
use super::query::{QueryResponse, QueryResults};
use crate::db::{Relaxation, RelaxedHit, SearchHit};
use crate::document::{DocumentError, MAX_DATETIME, MIN_DATETIME, datetime_text};
use crate::schema::{CollectionSchema, FieldId, FieldType};
use crate::search::{
    Explanation, FacetCounts, FieldExplanation, PhraseExplanation, ProximityExplanation,
    RelaxedExplanation, SortKey, SortValue, WordExplanation,
};

/// Why a response cannot be written as JSON. A response the engine returned for a collection of `schema` always can: each
/// is a broken invariant (a value of another type than its field, a number that is not finite), reported rather
/// than written as JSON that a reader would take for a value.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ResponseJsonError {
    /// A hit's document does not have its JSON form (SPEC §6.4.1).
    #[error("hit {id:?}: {source}")]
    Document {
        /// The document's id.
        id: String,
        /// Why.
        source: DocumentError,
    },
    /// A field id the schema does not have: the response is of another schema.
    #[error("field id {0:?} is not a field of the collection")]
    UnknownField(FieldId),
    /// A number that is not finite (SPEC §7.2: a score is a finite number), which JSON cannot hold.
    #[error("{0} is not a finite number")]
    NotFinite(&'static str),
    /// A sort value of another type than its key's field, or a datetime out of range.
    #[error("a sort value that is not of its key's type")]
    SortValue,
}

impl QueryResponse {
    /// The response as JSON (SPEC §7.5.2, ROADMAP 10.1.13, ADR-0065), for the collection of `schema` that answered it:
    ///
    /// ```jsonc
    /// {"hits": [{"document": {"id": "a", "title": "red shoes"}, "score": 1.25, "sort": [1.25],
    ///            "cursor": "AgAD...", "level": "exact", "step": 0, "rank_score": 1.25}],
    ///  "facets": [{"field": "brand", "values": [{"value": "nike", "count": 1}]}],
    ///  "relaxation": {"level": "exact", "applied": [], "original": "red shoes", "effective": "red shoes",
    ///                 "corrected": {}, "truncated": false, "filters_dropped": false},
    ///  "query_truncated": false}
    /// ```
    ///
    /// ```
    /// use persia_engine::db::{Database, DatabaseOptions};
    /// use persia_engine::document::{Document, Value};
    /// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    /// use persia_engine::search::request::{QueryRequest, RunOptions};
    /// use persia_engine::wal::Durability;
    /// use persia_storage::container::sim::SimFile;
    ///
    /// let schema = SchemaBuilder::new("c").field("title", FieldKind::Text(TextOptions::default())).build()?;
    /// let title = schema.field("title").ok_or("no field")?.id();
    /// let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
    /// db.create_collection(schema)?;
    /// db.put("c", &Document::new("a").with(title, Value::String("red shoes".into())), Durability::Fsync)?;
    /// let snapshot = db.snapshot();
    /// let c = snapshot.collection("c").ok_or("no collection")?;
    ///
    /// let response = QueryRequest::new("red").run(c, &RunOptions::default())?;
    /// let json: serde_json::Value = serde_json::from_str(&response.to_json(c.schema())?)?;
    /// assert_eq!(json["hits"][0]["document"]["id"], "a");
    /// assert_eq!(json["relaxation"]["level"], "exact");
    /// // The hit's cursor token pages the request's next page (`search_after`).
    /// assert!(json["hits"][0]["cursor"].is_string());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ResponseJsonError`] when the response is not one this collection's schema can describe: a field id it does
    /// not have, a value of another type than its field, or a number that is not finite.
    pub fn to_json(&self, schema: &CollectionSchema) -> Result<String, ResponseJsonError> {
        let w = Writer { schema };
        let mut out = String::from("{\"hits\":[");
        let (facets, relaxation) = match &self.results {
            QueryResults::Relaxed(results) => {
                for (i, hit) in results.hits.iter().enumerate() {
                    comma(&mut out, i);
                    w.relaxed_hit(&mut out, hit)?;
                }
                (&results.facets, &results.relaxation)
            }
            QueryResults::Plain(results) => {
                for (i, hit) in results.hits.iter().enumerate() {
                    comma(&mut out, i);
                    w.hit(&mut out, hit)?;
                    out.push('}');
                }
                (&results.facets, self.relaxation())
            }
        };
        out.push_str("],\"facets\":[");
        for (i, facet) in facets.iter().enumerate() {
            comma(&mut out, i);
            w.facet(&mut out, facet)?;
        }
        out.push_str("],\"relaxation\":");
        relaxation_json(&mut out, relaxation);
        out.push_str(",\"query_truncated\":");
        out.push_str(bool_text(self.query_truncated));
        out.push('}');
        Ok(out)
    }
}

/// A sort value as the response JSON types it (SPEC §7.4, §7.5.2): by its key's field, the score and an `f64` as a
/// number, an `i64` as an integer, a `datetime` (its microseconds) in the document's output form, a `bool`, and a
/// keyword's bytes. What [`QueryResponse::to_json`] writes, and what another writer of the response JSON (the embedded
/// API's, ROADMAP 10.1.25) types a hit's sort values with: the embedded API's typed sort values (ROADMAP 10.1.30) are
/// made from it.
///
/// ```
/// use persia_engine::schema::SchemaBuilder;
/// use persia_engine::search::request::TypedSortValue;
/// use persia_engine::search::{Order, SortKey, SortValue};
///
/// let schema = SchemaBuilder::new("c").build()?;
/// let score = SortKey::Score { order: Order::Desc };
/// assert_eq!(TypedSortValue::of(score, &SortValue::Score(1.5), &schema)?, TypedSortValue::Number(1.5));
/// assert_eq!(TypedSortValue::of(score, &SortValue::Missing, &schema)?, TypedSortValue::Missing);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TypedSortValue {
    /// The document has no value in the field (`null`).
    Missing,
    /// The score, or an `f64` field's value.
    Number(f64),
    /// An `i64` field's value.
    Integer(i64),
    /// A `datetime` field's value: microseconds in [`MIN_DATETIME`]`..=`[`MAX_DATETIME`], written in the document's
    /// output form (SPEC §6.4.1, [`datetime_text`]).
    Datetime(i64),
    /// A `bool` field's value.
    Bool(bool),
    /// A `keyword` field's bytes: a string when they are UTF-8, else `{"base64url": ...}` ([`base64url_text`]).
    Keyword(Vec<u8>),
}

impl TypedSortValue {
    /// `value`, a hit's value for the sort key `key`, typed by `key`'s field in `schema`. As [`Self::of_owned`], for a
    /// value the caller keeps (a keyword's bytes are copied).
    ///
    /// # Errors
    /// As [`Self::of_owned`].
    pub fn of(
        key: SortKey,
        value: &SortValue,
        schema: &CollectionSchema,
    ) -> Result<Self, ResponseJsonError> {
        Self::of_owned(key, value.clone(), schema)
    }

    /// `value`, a hit's value for the sort key `key`, typed by `key`'s field in `schema`, taking it: a keyword's
    /// bytes move into [`Self::Keyword`] without a copy (the embedded API types each hit's values this way).
    ///
    /// ```
    /// use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder};
    /// use persia_engine::search::request::TypedSortValue;
    /// use persia_engine::search::{Order, SortKey, SortValue};
    ///
    /// let schema = SchemaBuilder::new("c").field("brand", FieldKind::Keyword(KeywordOptions::default())).build()?;
    /// let brand = SortKey::field(schema.field("brand").ok_or("no field")?.id(), Order::Asc);
    /// let typed = TypedSortValue::of_owned(brand, SortValue::Keyword(b"acme".to_vec()), &schema)?;
    /// assert_eq!(typed, TypedSortValue::Keyword(b"acme".to_vec()));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ResponseJsonError::UnknownField`] for a key on a field `schema` does not have;
    /// [`ResponseJsonError::SortValue`] for a value of another type than its key's field, or a datetime out of range.
    pub fn of_owned(
        key: SortKey,
        value: SortValue,
        schema: &CollectionSchema,
    ) -> Result<Self, ResponseJsonError> {
        let field_type = match key {
            SortKey::Score { .. } => None,
            SortKey::Field { field, .. } => Some(
                schema
                    .field_by_id(field)
                    .ok_or(ResponseJsonError::UnknownField(field))?
                    .field_type(),
            ),
        };
        Ok(match (field_type, value) {
            (_, SortValue::Missing) => Self::Missing,
            (None, SortValue::Score(score)) => Self::Number(score),
            (Some(FieldType::I64), SortValue::Key(key)) => Self::Integer(key_to_i64(key)),
            (Some(FieldType::F64), SortValue::Key(key)) => Self::Number(key_to_f64(key)),
            (Some(FieldType::Datetime), SortValue::Key(key)) => {
                let micros = key_to_i64(key);
                if !(MIN_DATETIME..=MAX_DATETIME).contains(&micros) {
                    return Err(ResponseJsonError::SortValue);
                }
                Self::Datetime(micros)
            }
            (Some(FieldType::Bool), SortValue::Key(0)) => Self::Bool(false),
            (Some(FieldType::Bool), SortValue::Key(1)) => Self::Bool(true),
            (Some(FieldType::Keyword), SortValue::Keyword(bytes)) => Self::Keyword(bytes),
            _ => return Err(ResponseJsonError::SortValue),
        })
    }
}

/// The base64url text, without padding, that the response JSON writes a keyword's bytes as when they are not UTF-8
/// (`{"base64url": "..."}`, SPEC §7.5.2): the cursor token's alphabet.
///
/// ```
/// assert_eq!(persia_engine::search::request::base64url_text(&[0xff, 0xfe]), "__4");
/// ```
pub fn base64url_text(bytes: &[u8]) -> String {
    base64url::encode(bytes)
}
/// Writes the parts of a response that name fields, by `schema`.
struct Writer<'a> {
    schema: &'a CollectionSchema,
}

impl Writer<'_> {
    fn name(&self, field: FieldId) -> Result<&str, ResponseJsonError> {
        self.schema
            .field_by_id(field)
            .map(crate::schema::Field::name)
            .ok_or(ResponseJsonError::UnknownField(field))
    }

    /// A hit's object without its closing brace: a relaxed hit adds its keys after these.
    fn hit(&self, out: &mut String, hit: &SearchHit) -> Result<(), ResponseJsonError> {
        let document =
            hit.document
                .to_json(self.schema)
                .map_err(|source| ResponseJsonError::Document {
                    id: hit.document.id().to_owned(),
                    source,
                })?;
        out.push_str("{\"document\":");
        out.push_str(&document);
        out.push_str(",\"score\":");
        number(out, hit.score, "a score")?;
        out.push_str(",\"sort\":[");
        let keys = hit.sort_keys();
        if keys.len() != hit.sort.len() {
            return Err(ResponseJsonError::SortValue);
        }
        for (i, (key, value)) in keys.iter().zip(&hit.sort).enumerate() {
            comma(out, i);
            self.sort_value(out, *key, value)?;
        }
        out.push_str("],\"cursor\":");
        string(out, &hit.cursor().to_token());
        if let Some(explanation) = &hit.explanation {
            out.push_str(",\"explanation\":");
            self.explanation(out, explanation)?;
        }
        Ok(())
    }

    fn relaxed_hit(&self, out: &mut String, hit: &RelaxedHit) -> Result<(), ResponseJsonError> {
        self.hit(out, &hit.hit)?;
        out.push_str(",\"level\":");
        string(out, hit.level.name());
        out.push_str(",\"step\":");
        out.push_str(&hit.step.to_string());
        out.push_str(",\"rank_score\":");
        number(out, hit.rank_score, "a rank score")?;
        out.push('}');
        Ok(())
    }

    /// A sort value as its key's field types it ([`TypedSortValue::of`]).
    fn sort_value(
        &self,
        out: &mut String,
        key: SortKey,
        value: &SortValue,
    ) -> Result<(), ResponseJsonError> {
        match TypedSortValue::of(key, value, self.schema)? {
            TypedSortValue::Missing => out.push_str("null"),
            TypedSortValue::Number(n) => {
                let what = if matches!(key, SortKey::Score { .. }) {
                    "a score"
                } else {
                    "a sort value"
                };
                number(out, n, what)?;
            }
            TypedSortValue::Integer(i) => out.push_str(&i.to_string()),
            TypedSortValue::Datetime(micros) => {
                string(
                    out,
                    &datetime_text(micros).ok_or(ResponseJsonError::SortValue)?,
                );
            }
            TypedSortValue::Bool(b) => out.push_str(bool_text(b)),
            TypedSortValue::Keyword(bytes) => keyword(out, &bytes),
        }
        Ok(())
    }

    fn facet(&self, out: &mut String, facet: &FacetCounts) -> Result<(), ResponseJsonError> {
        out.push_str("{\"field\":");
        string(out, self.name(facet.field)?);
        out.push_str(",\"values\":[");
        for (i, value) in facet.values.iter().enumerate() {
            comma(out, i);
            out.push_str("{\"value\":");
            keyword(out, &value.value);
            out.push_str(",\"count\":");
            out.push_str(&value.count.to_string());
            out.push('}');
        }
        out.push_str("]}");
        Ok(())
    }

    /// An explanation node (SPEC §7.7): one object whose one key names the node's kind.
    fn explanation(&self, out: &mut String, e: &Explanation) -> Result<(), ResponseJsonError> {
        match e {
            Explanation::Word(word) => self.word(out, word)?,
            Explanation::Phrase(phrase) => self.phrase(out, phrase)?,
            Explanation::Bool {
                score,
                must,
                proximity,
                should,
            } => {
                out.push_str("{\"bool\":{\"score\":");
                number(out, *score, "a score")?;
                out.push_str(",\"must\":");
                self.explanations(out, must)?;
                if let Some(proximity) = proximity {
                    out.push_str(",\"proximity\":");
                    self.proximity(out, proximity)?;
                }
                out.push_str(",\"should\":");
                self.explanations(out, should)?;
                out.push_str("}}");
            }
            Explanation::Boost {
                score,
                boost,
                query,
            } => {
                out.push_str("{\"boost\":{\"score\":");
                number(out, *score, "a score")?;
                out.push_str(",\"boost\":");
                number(out, *boost, "a boost")?;
                out.push_str(",\"query\":");
                self.explanation(out, query)?;
                out.push_str("}}");
            }
            Explanation::Constant { score, query } => {
                out.push_str("{\"constant_score\":{\"score\":");
                number(out, *score, "a score")?;
                out.push_str(",\"query\":");
                self.explanation(out, query)?;
                out.push_str("}}");
            }
            Explanation::All => {
                out.push_str("{\"match_all\":{\"score\":");
                number(out, 0.0, "a score")?;
                out.push_str("}}");
            }
            Explanation::StaticRank { score, value } => {
                out.push_str("{\"static_rank\":{\"score\":");
                number(out, *score, "a score")?;
                if let Some(value) = value {
                    out.push_str(",\"value\":");
                    number(out, *value, "a static rank value")?;
                }
                out.push_str("}}");
            }
            Explanation::Relaxed(relaxed) => self.relaxed(out, relaxed)?,
        }
        Ok(())
    }

    fn explanations(
        &self,
        out: &mut String,
        list: &[Explanation],
    ) -> Result<(), ResponseJsonError> {
        out.push('[');
        for (i, e) in list.iter().enumerate() {
            comma(out, i);
            self.explanation(out, e)?;
        }
        out.push(']');
        Ok(())
    }

    fn word(&self, out: &mut String, word: &WordExplanation) -> Result<(), ResponseJsonError> {
        out.push_str("{\"word\":{\"score\":");
        number(out, word.score, "a score")?;
        out.push_str(",\"idf\":");
        number(out, word.idf, "an idf")?;
        out.push_str(",\"k1\":");
        number(out, word.k1, "a k1")?;
        out.push_str(",\"tf\":");
        number(out, word.tf, "a tf")?;
        out.push_str(",\"fields\":[");
        for (i, field) in word.fields.iter().enumerate() {
            comma(out, i);
            self.field(out, field)?;
        }
        out.push_str("]}}");
        Ok(())
    }

    fn field(&self, out: &mut String, f: &FieldExplanation) -> Result<(), ResponseJsonError> {
        out.push_str("{\"field\":");
        string(out, self.name(f.field)?);
        out.push_str(",\"term\":");
        keyword(out, &f.term);
        out.push_str(",\"tf\":");
        out.push_str(&f.tf.to_string());
        out.push_str(",\"norm\":");
        out.push_str(&f.norm.to_string());
        if let Some(length) = f.length {
            out.push_str(",\"length\":");
            out.push_str(&length.to_string());
        }
        out.push_str(",\"factor\":");
        number(out, f.factor, "a factor")?;
        out.push_str(",\"contribution\":");
        number(out, f.contribution, "a contribution")?;
        out.push('}');
        Ok(())
    }

    fn phrase(&self, out: &mut String, p: &PhraseExplanation) -> Result<(), ResponseJsonError> {
        out.push_str("{\"phrase\":{\"score\":");
        number(out, p.score, "a score")?;
        out.push_str(",\"field\":");
        string(out, self.name(p.field)?);
        out.push_str(",\"terms\":[");
        for (i, term) in p.terms.iter().enumerate() {
            comma(out, i);
            keyword(out, term);
        }
        out.push_str("],\"idf\":");
        number(out, p.idf, "an idf")?;
        out.push_str(",\"k1\":");
        number(out, p.k1, "a k1")?;
        out.push_str(",\"freq\":");
        out.push_str(&p.freq.to_string());
        out.push_str(",\"norm\":");
        out.push_str(&p.norm.to_string());
        out.push_str(",\"length\":");
        out.push_str(&p.length.to_string());
        out.push_str(",\"factor\":");
        number(out, p.factor, "a factor")?;
        out.push_str(",\"tf\":");
        number(out, p.tf, "a tf")?;
        out.push_str("}}");
        Ok(())
    }

    fn proximity(
        &self,
        out: &mut String,
        p: &ProximityExplanation,
    ) -> Result<(), ResponseJsonError> {
        out.push_str("{\"score\":");
        number(out, p.score, "a proximity bonus")?;
        out.push_str(",\"pairs\":[");
        for (i, pair) in p.pairs.iter().enumerate() {
            comma(out, i);
            out.push_str("{\"first\":");
            out.push_str(&pair.first.to_string());
            out.push_str(",\"weight\":");
            number(out, pair.weight, "a pair weight")?;
            if let Some(field) = pair.field {
                out.push_str(",\"field\":");
                string(out, self.name(field)?);
            }
            if let Some(distance) = pair.distance {
                out.push_str(",\"distance\":");
                out.push_str(&distance.to_string());
            }
            out.push_str(",\"closeness\":");
            number(out, pair.closeness, "a closeness")?;
            out.push_str(",\"score\":");
            number(out, pair.score, "a score")?;
            out.push('}');
        }
        out.push_str("]}");
        Ok(())
    }

    fn relaxed(&self, out: &mut String, r: &RelaxedExplanation) -> Result<(), ResponseJsonError> {
        out.push_str("{\"relaxed\":{\"level\":");
        string(out, r.level.name());
        out.push_str(",\"step\":");
        out.push_str(&r.step.to_string());
        if let Some(applied) = &r.applied {
            out.push_str(",\"applied\":");
            string(out, applied);
        }
        out.push_str(",\"score\":");
        number(out, r.explanation.score(), "a score")?;
        out.push_str(",\"explanation\":");
        self.explanation(out, &r.explanation)?;
        out.push_str("}}");
        Ok(())
    }
}

/// `relaxation` (SPEC §7.6): `level`, `applied`, `original` and `effective` when the query has text, `corrected` (in
/// query order, a word repeated in the query named once), `truncated`, `fallback` when L5 found the hits, and
/// `filters_dropped`.
fn relaxation_json(out: &mut String, r: &Relaxation) {
    out.push_str("{\"level\":");
    string(out, r.level.name());
    out.push_str(",\"applied\":[");
    for (i, applied) in r.applied.iter().enumerate() {
        comma(out, i);
        string(out, applied);
    }
    out.push(']');
    if let Some(original) = &r.original {
        out.push_str(",\"original\":");
        string(out, original);
    }
    if let Some(effective) = &r.effective {
        out.push_str(",\"effective\":");
        string(out, effective);
    }
    out.push_str(",\"corrected\":{");
    let mut written: Vec<&str> = Vec::with_capacity(r.corrected.len());
    for correction in &r.corrected {
        // One key per word: a word the query repeats is corrected the same way each time, and a reader would keep
        // only one of two equal keys anyway.
        if written.contains(&correction.original.as_str()) {
            continue;
        }
        comma(out, written.len());
        string(out, &correction.original);
        out.push(':');
        string(out, &correction.corrected);
        written.push(&correction.original);
    }
    out.push_str("},\"truncated\":");
    out.push_str(bool_text(r.truncated));
    if let Some(fallback) = r.fallback {
        out.push_str(",\"fallback\":");
        string(out, fallback.name());
    }
    out.push_str(",\"filters_dropped\":");
    out.push_str(bool_text(r.filters_dropped));
    out.push('}');
}

/// A keyword's bytes (a facet value, a sort value, an explained term): a JSON string when they are UTF-8, else
/// `{"base64url": "..."}`, their base64url text without padding (SPEC §7.5.2, the cursor token's alphabet).
fn keyword(out: &mut String, bytes: &[u8]) {
    if let Ok(text) = std::str::from_utf8(bytes) {
        string(out, text);
    } else {
        out.push_str("{\"base64url\":\"");
        out.push_str(&base64url::encode(bytes));
        out.push_str("\"}");
    }
}

/// `text` as a JSON string, escaped as `serde_json` escapes it.
fn string(out: &mut String, text: &str) {
    out.push_str(&J::String(text.to_owned()).to_string());
}

/// A finite `f64` as a JSON number (shortest round-trip form). JSON has no `Infinity` or `NaN` (SPEC §7.2).
fn number(out: &mut String, n: f64, what: &'static str) -> Result<(), ResponseJsonError> {
    let n = Number::from_f64(n).ok_or(ResponseJsonError::NotFinite(what))?;
    out.push_str(&n.to_string());
    Ok(())
}

fn bool_text(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

/// The comma before the `i`-th item of a list.
fn comma(out: &mut String, i: usize) {
    if i > 0 {
        out.push(',');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Correction;
    use crate::relax::{FallbackOrder, RelaxLevel};
    use crate::schema::{FieldKind, KeywordOptions, SchemaBuilder};
    use crate::search::Order;

    fn written(f: impl FnOnce(&mut String)) -> String {
        let mut out = String::new();
        f(&mut out);
        out
    }

    #[test]
    fn a_keyword_is_a_string_when_utf8_and_base64url_otherwise() {
        assert_eq!(written(|o| keyword(o, b"nike")), r#""nike""#);
        assert_eq!(written(|o| keyword(o, "ñ\"\\".as_bytes())), r#""ñ\"\\""#);
        assert_eq!(written(|o| keyword(o, b"")), r#""""#);
        // Not UTF-8: the bytes as base64url without padding.
        assert_eq!(
            written(|o| keyword(o, b"nike\xff")),
            r#"{"base64url":"bmlrZf8"}"#
        );
        assert_eq!(written(|o| keyword(o, b"\xc3")), r#"{"base64url":"ww"}"#);
    }

    #[test]
    fn a_number_is_finite_or_an_error() {
        assert_eq!(written(|o| number(o, 1.5, "x").unwrap()), "1.5");
        assert_eq!(written(|o| number(o, 0.0, "x").unwrap()), "0.0");
        assert_eq!(written(|o| number(o, -0.0, "x").unwrap()), "-0.0");
        assert_eq!(written(|o| number(o, 1e300, "x").unwrap()), "1e+300");
        for n in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let mut out = String::new();
            assert_eq!(
                number(&mut out, n, "a score"),
                Err(ResponseJsonError::NotFinite("a score"))
            );
        }
    }

    fn schema() -> CollectionSchema {
        let facet = KeywordOptions {
            facet: true,
            ..KeywordOptions::default()
        };
        SchemaBuilder::new("c")
            .field("brand", FieldKind::Keyword(facet))
            .field("n", FieldKind::I64 { stored: true })
            .field("on", FieldKind::Bool { stored: true })
            .field("at", FieldKind::Datetime { stored: true })
            .field("x", FieldKind::F64 { stored: true })
            .build()
            .unwrap()
    }

    #[test]
    fn a_sort_value_of_another_type_than_its_key_is_an_error() {
        let schema = schema();
        let w = Writer { schema: &schema };
        let id = |name: &str| schema.field(name).unwrap().id();
        let key = |name: &str| SortKey::field(id(name), Order::Asc);
        let value = |k: SortKey, v: &SortValue| {
            let mut out = String::new();
            w.sort_value(&mut out, k, v).map(|()| out)
        };
        let ok = |k: SortKey, v: SortValue, text: &str| assert_eq!(value(k, &v).unwrap(), text);
        ok(
            key("n"),
            SortValue::Key(persia_format::for_codec::i64_to_key(-5)),
            "-5",
        );
        ok(
            key("x"),
            SortValue::Key(persia_format::for_codec::f64_to_key(2.5)),
            "2.5",
        );
        ok(key("on"), SortValue::Key(0), "false");
        ok(key("on"), SortValue::Key(1), "true");
        ok(
            key("at"),
            SortValue::Key(persia_format::for_codec::i64_to_key(0)),
            r#""1970-01-01T00:00:00Z""#,
        );
        ok(
            key("brand"),
            SortValue::Keyword(b"\xff".to_vec()),
            r#"{"base64url":"_w"}"#,
        );
        ok(key("brand"), SortValue::Missing, "null");
        ok(SortKey::score(), SortValue::Score(1.25), "1.25");
        ok(SortKey::score(), SortValue::Missing, "null");
        for (k, v) in [
            (key("on"), SortValue::Key(2)),
            (key("brand"), SortValue::Key(1)),
            (key("n"), SortValue::Keyword(b"1".to_vec())),
            (key("n"), SortValue::Score(1.0)),
            (SortKey::score(), SortValue::Key(1)),
            (SortKey::score(), SortValue::Keyword(Vec::new())),
            // A datetime past the range has no text form.
            (
                key("at"),
                SortValue::Key(persia_format::for_codec::i64_to_key(i64::MAX)),
            ),
        ] {
            assert_eq!(value(k, &v), Err(ResponseJsonError::SortValue), "{v:?}");
        }
        // A non-finite score or float column value, and a field the schema does not have.
        assert_eq!(
            value(SortKey::score(), &SortValue::Score(f64::NAN)),
            Err(ResponseJsonError::NotFinite("a score"))
        );
        assert_eq!(
            value(
                key("x"),
                &SortValue::Key(persia_format::for_codec::f64_to_key(f64::INFINITY))
            ),
            Err(ResponseJsonError::NotFinite("a sort value"))
        );
        let other = FieldId::new(99);
        assert_eq!(
            value(SortKey::field(other, Order::Asc), &SortValue::Missing),
            Err(ResponseJsonError::UnknownField(other))
        );
    }

    #[test]
    fn relaxation_states_each_field_and_a_repeated_correction_once() {
        let correction = |original: &str, corrected: &str| Correction {
            original: original.into(),
            corrected: corrected.into(),
        };
        let mut r = Relaxation::of(
            (RelaxLevel::Fallback, true),
            vec!["fuzzy".into(), "fallback".into()],
            None,
            (None, Vec::new()),
            true,
        );
        r.fallback = Some(FallbackOrder::StaticRank);
        assert_eq!(
            written(|o| relaxation_json(o, &r)),
            r#"{"level":"fallback","applied":["fuzzy","fallback"],"corrected":{},"truncated":true,"fallback":"static_rank","filters_dropped":false}"#
        );
        r.original = Some("rojs zapatilas rojs".into());
        r.effective = Some("rojas zapatillas rojas".into());
        r.corrected = vec![
            correction("rojs", "rojas"),
            correction("zapatilas", "zapatillas"),
            correction("rojs", "rojas"),
        ];
        r.fallback = None;
        r.filters_dropped = true;
        r.truncated = false;
        assert_eq!(
            written(|o| relaxation_json(o, &r)),
            r#"{"level":"fallback","applied":["fuzzy","fallback"],"original":"rojs zapatilas rojs","effective":"rojas zapatillas rojas","corrected":{"rojs":"rojas","zapatilas":"zapatillas"},"truncated":false,"filters_dropped":true}"#
        );
    }

    #[test]
    fn a_facet_of_a_field_the_schema_lacks_is_an_error() {
        let schema = schema();
        let w = Writer { schema: &schema };
        let brand = schema.field("brand").unwrap().id();
        let facet = |field| FacetCounts {
            field,
            values: vec![
                crate::search::FacetValue {
                    value: b"a".to_vec(),
                    count: 2,
                },
                crate::search::FacetValue {
                    value: b"\x80".to_vec(),
                    count: 1,
                },
            ],
        };
        let mut out = String::new();
        w.facet(&mut out, &facet(brand)).unwrap();
        assert_eq!(
            out,
            r#"{"field":"brand","values":[{"value":"a","count":2},{"value":{"base64url":"gA"},"count":1}]}"#
        );
        let other = FieldId::new(42);
        assert_eq!(
            w.facet(&mut String::new(), &facet(other)),
            Err(ResponseJsonError::UnknownField(other))
        );
    }
}

#[cfg(test)]
mod explain_tests {
    use super::*;
    use crate::relax::RelaxLevel;
    use crate::schema::{FieldKind, SchemaBuilder, TextOptions};
    use crate::search::PairExplanation;

    /// The nodes the golden responses do not reach: a phrase, a constant score, `match_all`, a proximity bonus (one pair
    /// with a field, one without), a keyword field's word (no length), and a relaxed node at L0 (no `applied`).
    #[test]
    #[allow(clippy::too_many_lines)] // One tree of every node, written out.
    fn every_node_has_its_form() {
        let schema = SchemaBuilder::new("c")
            .field("body", FieldKind::Text(TextOptions::default()))
            .build()
            .unwrap();
        let body = schema.field("body").unwrap().id();
        let w = Writer { schema: &schema };
        let word = Explanation::Word(WordExplanation {
            score: 1.0,
            idf: 2.0,
            k1: 1.2,
            tf: 0.5,
            fields: vec![FieldExplanation {
                field: body,
                term: b"\xffa".to_vec(),
                tf: 1,
                norm: 0,
                length: None,
                factor: 0.5,
                contribution: 0.5,
            }],
        });
        let phrase = Explanation::Phrase(PhraseExplanation {
            score: 3.0,
            field: body,
            terms: vec![b"red".to_vec(), b"hat".to_vec()],
            idf: 4.0,
            k1: 1.2,
            freq: 2,
            norm: 7,
            length: 9,
            factor: 0.75,
            tf: 1.5,
        });
        let pairs = vec![
            PairExplanation {
                first: 0,
                weight: 0.5,
                field: Some(body),
                distance: Some(2),
                closeness: 0.25,
                score: 0.125,
            },
            PairExplanation {
                first: 1,
                weight: 0.5,
                field: None,
                distance: None,
                closeness: 0.0,
                score: 0.0,
            },
        ];
        let tree = Explanation::Relaxed(RelaxedExplanation {
            level: RelaxLevel::Exact,
            step: 0,
            applied: None,
            explanation: Box::new(Explanation::Bool {
                score: 4.125,
                must: vec![word, phrase],
                proximity: Some(ProximityExplanation {
                    score: 0.125,
                    pairs,
                }),
                should: vec![
                    Explanation::Constant {
                        score: 0.0,
                        query: Box::new(Explanation::All),
                    },
                    Explanation::StaticRank {
                        score: 0.5,
                        value: Some(10.0),
                    },
                ],
            }),
        });
        let mut out = String::new();
        w.explanation(&mut out, &tree).unwrap();
        assert_eq!(
            out,
            concat!(
                r#"{"relaxed":{"level":"exact","step":0,"score":4.125,"explanation":{"bool":{"score":4.125,"must":["#,
                r#"{"word":{"score":1.0,"idf":2.0,"k1":1.2,"tf":0.5,"fields":[{"field":"body","term":{"base64url":"_2E"},"#,
                r#""tf":1,"norm":0,"factor":0.5,"contribution":0.5}]}},"#,
                r#"{"phrase":{"score":3.0,"field":"body","terms":["red","hat"],"idf":4.0,"k1":1.2,"freq":2,"norm":7,"#,
                r#""length":9,"factor":0.75,"tf":1.5}}],"#,
                r#""proximity":{"score":0.125,"pairs":[{"first":0,"weight":0.5,"field":"body","distance":2,"#,
                r#""closeness":0.25,"score":0.125},{"first":1,"weight":0.5,"closeness":0.0,"score":0.0}]},"#,
                r#""should":[{"constant_score":{"score":0.0,"query":{"match_all":{"score":0.0}}}},"#,
                r#"{"static_rank":{"score":0.5,"value":10.0}}]}}}}"#,
            )
        );
        let _: J = serde_json::from_str(&out).unwrap();
        // A node naming a field the schema does not have, or holding a number that is not finite, is an error.
        let other = FieldId::new(9);
        let unknown = Explanation::Phrase(PhraseExplanation {
            score: 1.0,
            field: other,
            terms: Vec::new(),
            idf: 1.0,
            k1: 1.0,
            freq: 1,
            norm: 1,
            length: 1,
            factor: 1.0,
            tf: 1.0,
        });
        assert_eq!(
            w.explanation(&mut String::new(), &unknown),
            Err(ResponseJsonError::UnknownField(other))
        );
        let infinite = Explanation::Boost {
            score: f64::INFINITY,
            boost: 1.0,
            query: Box::new(Explanation::All),
        };
        assert_eq!(
            w.explanation(&mut String::new(), &infinite),
            Err(ResponseJsonError::NotFinite("a score"))
        );
    }
}
