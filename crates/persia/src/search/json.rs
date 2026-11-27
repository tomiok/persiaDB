//! The response JSON of the embedded API (SPEC §7.5.2; ROADMAP 10.1.25; ADR-0065, amended): [`SearchResponse`] is its
//! one source. Its `Serialize` impl writes the object the engine's `QueryResponse::to_json` writes, through `serde`
//! (no JSON is assembled from strings here), so the C ABI (ADR-0018) and the server (M11) can pass the facade's text
//! through. A value that is absent is left out, never `null`; the only `null` is a missing sort value. A hit's
//! document is the engine's `DocumentJson` (SPEC §6.4.1), serialized in place (ROADMAP 10.1.31).

// `Document` for the tests below, which name it through `super::*` as they did when this module rendered documents.
#[cfg(test)]
use persia_engine::document::Document;
use persia_engine::document::{DocumentJson, datetime_text};
use persia_engine::schema::{CollectionSchema, FieldId};
use persia_engine::search::request::base64url_text;
use persia_engine::search::{
    Explanation, FacetCounts, FieldExplanation, PairExplanation, PhraseExplanation,
    ProximityExplanation, RelaxedExplanation, WordExplanation,
};
use serde::ser::{Error as _, SerializeMap, SerializeSeq, SerializeStruct};
use serde::{Serialize, Serializer};

use super::{Hit, Relaxation, SearchResponse, SortValue};
use crate::{Error, Result};

impl SearchResponse {
    /// The response as the response JSON of SPEC §7.5.2, the object the engine writes for the same search: each hit's
    /// document (its JSON form, SPEC §6.4.1), score, typed sort values and cursor token, its explanation when the
    /// request asked, and for a relaxed search its level, step and rank score; the facets; `relaxation`; and
    /// `query_truncated`. The same text as `serde_json::to_string(&response)`: [`SearchResponse`] is `Serialize`.
    ///
    /// ```
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// # let db = persia::Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#)?;
    /// notes.put_json(r#"{"id": "n1", "body": "hello world"}"#)?;
    /// let response = notes.search_json(r#"{"q": "hello"}"#)?;
    /// let json: serde_json::Value = serde_json::from_str(&response.to_json()?)?;
    /// assert_eq!(json["hits"][0]["document"]["id"], "n1");
    /// assert_eq!(json["hits"][0]["level"], "exact");
    /// assert_eq!(json["relaxation"]["original"], "hello");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::Io`](crate::ErrorCode::Io), the code of a broken invariant (ADR-0063 item 8), when the response
    /// does not fit its collection's schema: a value of another type than its field, a number that is not finite. A
    /// response a search returned always fits.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| {
            Error::new(
                crate::ErrorCode::Io,
                format!("cannot write the response as JSON: {e}"),
            )
        })
    }
}

/// The response JSON of SPEC §7.5.2 ([`SearchResponse::to_json`]).
impl Serialize for SearchResponse {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let w = Writer {
            schema: &self.schema,
        };
        let mut out = s.serialize_struct("SearchResponse", 4)?;
        out.serialize_field("hits", &List(&self.hits, |hit| HitJson { w, hit }))?;
        out.serialize_field(
            "facets",
            &List(&self.facets, |facet| FacetJson { w, facet }),
        )?;
        out.serialize_field("relaxation", &RelaxationJson(&self.relaxation))?;
        out.serialize_field("query_truncated", &self.query_truncated)?;
        out.end()
    }
}

/// Names the fields of a response by its collection's schema.
#[derive(Clone, Copy)]
struct Writer<'a> {
    schema: &'a CollectionSchema,
}

impl<'a> Writer<'a> {
    fn name<E: serde::ser::Error>(self, field: FieldId) -> std::result::Result<&'a str, E> {
        self.schema
            .field_by_id(field)
            .map(persia_engine::schema::Field::name)
            .ok_or_else(|| {
                E::custom(format_args!(
                    "field id {field:?} is not a field of the collection"
                ))
            })
    }
}

/// A list whose items are serialized as `f` makes them.
struct List<'a, T, F>(&'a [T], F);

impl<'a, T, F, J> Serialize for List<'a, T, F>
where
    F: Fn(&'a T) -> J,
    J: Serialize,
{
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for item in self.0 {
            seq.serialize_element(&(self.1)(item))?;
        }
        seq.end()
    }
}

/// A finite `f64` (SPEC §7.2): JSON has no `Infinity` or `NaN`, and `serde_json` would write `null` for one.
struct Finite(f64, &'static str);

impl Serialize for Finite {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        if self.0.is_finite() {
            s.serialize_f64(self.0)
        } else {
            Err(S::Error::custom(format_args!(
                "{} is not a finite number",
                self.1
            )))
        }
    }
}

/// A keyword's bytes (a facet value, a sort value, an explained term): a string when they are UTF-8, else
/// `{"base64url": "..."}` (SPEC §7.5.2).
struct Keyword<'a>(&'a [u8]);

impl Serialize for Keyword<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        if let Ok(text) = std::str::from_utf8(self.0) {
            s.serialize_str(text)
        } else {
            let mut map = s.serialize_map(Some(1))?;
            map.serialize_entry("base64url", &base64url_text(self.0))?;
            map.end()
        }
    }
}

struct HitJson<'a> {
    w: Writer<'a>,
    hit: &'a Hit,
}

impl Serialize for HitJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let hit = self.hit;
        // The document's JSON form is the engine's one writer of it (SPEC §6.4.1; ROADMAP 10.1.31), serialized in
        // place: `id`, `blob`, then the fields by name.
        let document = DocumentJson::new(&hit.document, self.w.schema)
            .map_err(|e| S::Error::custom(format_args!("hit {:?}: {e}", hit.document.id())))?;
        let mut out = s.serialize_map(None)?;
        out.serialize_entry("document", &document)?;
        out.serialize_entry("score", &Finite(hit.score, "a score"))?;
        out.serialize_entry("sort", &List(hit.sort_values(), SortJson))?;
        out.serialize_entry("cursor", &hit.cursor)?;
        if let Some(explanation) = &hit.explanation {
            out.serialize_entry(
                "explanation",
                &ExplanationJson {
                    w: self.w,
                    e: explanation,
                },
            )?;
        }
        if let Some(relaxed) = hit.relaxed {
            out.serialize_entry("level", hit.level.name())?;
            out.serialize_entry("step", &relaxed.step)?;
            out.serialize_entry("rank_score", &Finite(relaxed.rank_score, "a rank score"))?;
        }
        out.end()
    }
}

/// A sort value as its key's field types it (SPEC §7.4, §7.5.2), as [`SortValue`] documents it.
struct SortJson<'a>(&'a SortValue);

impl Serialize for SortJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self.0 {
            SortValue::Missing => s.serialize_unit(),
            SortValue::Score(n) => Finite(*n, "a score").serialize(s),
            SortValue::F64(n) => Finite(*n, "a sort value").serialize(s),
            SortValue::I64(i) => s.serialize_i64(*i),
            SortValue::Datetime(micros) => {
                s.serialize_str(&datetime_text(*micros).ok_or_else(|| {
                    S::Error::custom(format_args!("datetime {micros} is out of range"))
                })?)
            }
            SortValue::Bool(b) => s.serialize_bool(*b),
            SortValue::Keyword(bytes) => Keyword(bytes).serialize(s),
        }
    }
}

struct FacetJson<'a> {
    w: Writer<'a>,
    facet: &'a FacetCounts,
}

impl Serialize for FacetJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut out = s.serialize_struct("Facet", 2)?;
        out.serialize_field("field", self.w.name::<S::Error>(self.facet.field)?)?;
        out.serialize_field("values", &List(&self.facet.values, facet_value))?;
        out.end()
    }
}

/// A facet value with its count, for [`List`].
fn facet_value(v: &persia_engine::search::FacetValue) -> FacetValueJson<'_> {
    FacetValueJson(&v.value, v.count)
}

/// A keyword, for [`List`].
#[allow(clippy::ptr_arg)] // `List` hands each element as it is: `&Vec<u8>`.
fn keyword_of(bytes: &Vec<u8>) -> Keyword<'_> {
    Keyword(bytes)
}

struct FacetValueJson<'a>(&'a [u8], u64);

impl Serialize for FacetValueJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut out = s.serialize_struct("FacetValue", 2)?;
        out.serialize_field("value", &Keyword(self.0))?;
        out.serialize_field("count", &self.1)?;
        out.end()
    }
}

/// `relaxation` (SPEC §7.6): `level`, `applied`, `original` and `effective` when the query has text, `corrected` (in
/// query order, a word repeated in the query named once), `truncated`, `fallback` when L5 found the hits, and
/// `filters_dropped`.
struct RelaxationJson<'a>(&'a Relaxation);

impl Serialize for RelaxationJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let r = self.0;
        let mut out = s.serialize_map(None)?;
        out.serialize_entry("level", r.level.name())?;
        out.serialize_entry("applied", &r.applied)?;
        if let Some(original) = &r.original {
            out.serialize_entry("original", original)?;
        }
        if let Some(effective) = &r.effective {
            out.serialize_entry("effective", effective)?;
        }
        out.serialize_entry("corrected", &Corrected(r))?;
        out.serialize_entry("truncated", &r.truncated)?;
        if let Some(fallback) = r.fallback {
            out.serialize_entry("fallback", fallback.name())?;
        }
        out.serialize_entry("filters_dropped", &r.filters_dropped)?;
        out.end()
    }
}

/// The words the fuzzy level replaced: one key per word, the first time the query has it.
struct Corrected<'a>(&'a Relaxation);

impl Serialize for Corrected<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut written: Vec<&str> = Vec::with_capacity(self.0.corrected.len());
        let mut out = s.serialize_map(None)?;
        for correction in &self.0.corrected {
            // A word the query repeats is corrected the same way each time, and a reader keeps one of two equal keys.
            if written.contains(&correction.original.as_str()) {
                continue;
            }
            out.serialize_entry(&correction.original, &correction.corrected)?;
            written.push(&correction.original);
        }
        out.end()
    }
}

/// An explanation node (SPEC §7.7): one object whose one key names the node's kind.
struct ExplanationJson<'a> {
    w: Writer<'a>,
    e: &'a Explanation,
}

impl<'a> ExplanationJson<'a> {
    fn of(&self, e: &'a Explanation) -> Self {
        Self { w: self.w, e }
    }
}

impl Serialize for ExplanationJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let w = self.w;
        let mut out = s.serialize_map(Some(1))?;
        match self.e {
            Explanation::Word(word) => out.serialize_entry("word", &WordJson { w, word })?,
            Explanation::Phrase(phrase) => {
                out.serialize_entry("phrase", &PhraseJson { w, phrase })?;
            }
            Explanation::Bool {
                score,
                must,
                proximity,
                should,
            } => out.serialize_entry(
                "bool",
                &BoolJson {
                    node: self,
                    score: *score,
                    must,
                    proximity: proximity.as_ref(),
                    should,
                },
            )?,
            Explanation::Boost {
                score,
                boost,
                query,
            } => out.serialize_entry(
                "boost",
                &Wrapped {
                    score: *score,
                    boost: Some(*boost),
                    query: self.of(query),
                },
            )?,
            Explanation::Constant { score, query } => out.serialize_entry(
                "constant_score",
                &Wrapped {
                    score: *score,
                    boost: None,
                    query: self.of(query),
                },
            )?,
            Explanation::All => out.serialize_entry("match_all", &ScoreOnly(0.0, None))?,
            Explanation::StaticRank { score, value } => {
                out.serialize_entry("static_rank", &ScoreOnly(*score, *value))?;
            }
            Explanation::Relaxed(relaxed) => {
                out.serialize_entry(
                    "relaxed",
                    &RelaxedJson {
                        node: self,
                        relaxed,
                    },
                )?;
            }
            // `#[non_exhaustive]`: a node this build does not write.
            _ => {
                return Err(S::Error::custom(
                    "an explanation node this build does not write",
                ));
            }
        }
        out.end()
    }
}

struct BoolJson<'n, 'a> {
    node: &'n ExplanationJson<'a>,
    score: f64,
    must: &'a [Explanation],
    proximity: Option<&'a ProximityExplanation>,
    should: &'a [Explanation],
}

impl Serialize for BoolJson<'_, '_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut out = s.serialize_map(None)?;
        out.serialize_entry("score", &Finite(self.score, "a score"))?;
        out.serialize_entry("must", &List(self.must, |e| self.node.of(e)))?;
        if let Some(proximity) = self.proximity {
            out.serialize_entry(
                "proximity",
                &ProximityJson {
                    w: self.node.w,
                    proximity,
                },
            )?;
        }
        out.serialize_entry("should", &List(self.should, |e| self.node.of(e)))?;
        out.end()
    }
}

/// A `boost` (with its factor) or a `constant_score` (without one) around its query.
struct Wrapped<'a> {
    score: f64,
    boost: Option<f64>,
    query: ExplanationJson<'a>,
}

impl Serialize for Wrapped<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut out = s.serialize_map(None)?;
        out.serialize_entry("score", &Finite(self.score, "a score"))?;
        if let Some(boost) = self.boost {
            out.serialize_entry("boost", &Finite(boost, "a boost"))?;
        }
        out.serialize_entry("query", &self.query)?;
        out.end()
    }
}

/// `match_all` (a score of 0) or `static_rank` (its score, and the value it read when there is one).
struct ScoreOnly(f64, Option<f64>);

impl Serialize for ScoreOnly {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut out = s.serialize_map(None)?;
        out.serialize_entry("score", &Finite(self.0, "a score"))?;
        if let Some(value) = self.1 {
            out.serialize_entry("value", &Finite(value, "a static rank value"))?;
        }
        out.end()
    }
}

struct WordJson<'a> {
    w: Writer<'a>,
    word: &'a WordExplanation,
}

impl Serialize for WordJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let word = self.word;
        let mut out = s.serialize_struct("Word", 5)?;
        out.serialize_field("score", &Finite(word.score, "a score"))?;
        out.serialize_field("idf", &Finite(word.idf, "an idf"))?;
        out.serialize_field("k1", &Finite(word.k1, "a k1"))?;
        out.serialize_field("tf", &Finite(word.tf, "a tf"))?;
        out.serialize_field(
            "fields",
            &List(&word.fields, |field| FieldJson { w: self.w, field }),
        )?;
        out.end()
    }
}

struct FieldJson<'a> {
    w: Writer<'a>,
    field: &'a FieldExplanation,
}

impl Serialize for FieldJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let f = self.field;
        let mut out = s.serialize_map(None)?;
        out.serialize_entry("field", self.w.name::<S::Error>(f.field)?)?;
        out.serialize_entry("term", &Keyword(&f.term))?;
        out.serialize_entry("tf", &f.tf)?;
        out.serialize_entry("norm", &f.norm)?;
        if let Some(length) = f.length {
            out.serialize_entry("length", &length)?;
        }
        out.serialize_entry("factor", &Finite(f.factor, "a factor"))?;
        out.serialize_entry("contribution", &Finite(f.contribution, "a contribution"))?;
        out.end()
    }
}

struct PhraseJson<'a> {
    w: Writer<'a>,
    phrase: &'a PhraseExplanation,
}

impl Serialize for PhraseJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let p = self.phrase;
        let mut out = s.serialize_struct("Phrase", 10)?;
        out.serialize_field("score", &Finite(p.score, "a score"))?;
        out.serialize_field("field", self.w.name::<S::Error>(p.field)?)?;
        out.serialize_field("terms", &List(&p.terms, keyword_of))?;
        out.serialize_field("idf", &Finite(p.idf, "an idf"))?;
        out.serialize_field("k1", &Finite(p.k1, "a k1"))?;
        out.serialize_field("freq", &p.freq)?;
        out.serialize_field("norm", &p.norm)?;
        out.serialize_field("length", &p.length)?;
        out.serialize_field("factor", &Finite(p.factor, "a factor"))?;
        out.serialize_field("tf", &Finite(p.tf, "a tf"))?;
        out.end()
    }
}

struct ProximityJson<'a> {
    w: Writer<'a>,
    proximity: &'a ProximityExplanation,
}

impl Serialize for ProximityJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut out = s.serialize_struct("Proximity", 2)?;
        out.serialize_field("score", &Finite(self.proximity.score, "a proximity bonus"))?;
        out.serialize_field(
            "pairs",
            &List(&self.proximity.pairs, |pair| PairJson { w: self.w, pair }),
        )?;
        out.end()
    }
}

struct PairJson<'a> {
    w: Writer<'a>,
    pair: &'a PairExplanation,
}

impl Serialize for PairJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let pair = self.pair;
        let mut out = s.serialize_map(None)?;
        out.serialize_entry("first", &pair.first)?;
        out.serialize_entry("weight", &Finite(pair.weight, "a pair weight"))?;
        if let Some(field) = pair.field {
            out.serialize_entry("field", self.w.name::<S::Error>(field)?)?;
        }
        if let Some(distance) = pair.distance {
            out.serialize_entry("distance", &distance)?;
        }
        out.serialize_entry("closeness", &Finite(pair.closeness, "a closeness"))?;
        out.serialize_entry("score", &Finite(pair.score, "a score"))?;
        out.end()
    }
}

struct RelaxedJson<'n, 'a> {
    node: &'n ExplanationJson<'a>,
    relaxed: &'a RelaxedExplanation,
}

impl Serialize for RelaxedJson<'_, '_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let r = self.relaxed;
        let mut out = s.serialize_map(None)?;
        out.serialize_entry("level", r.level.name())?;
        out.serialize_entry("step", &r.step)?;
        if let Some(applied) = &r.applied {
            out.serialize_entry("applied", applied)?;
        }
        out.serialize_entry("score", &Finite(r.explanation.score(), "a score"))?;
        out.serialize_entry("explanation", &self.node.of(&r.explanation))?;
        out.end()
    }
}

#[cfg(test)]
mod tests {
    use persia_engine::query::QueryLimits;
    use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
    use persia_engine::search::request::{QueryRequest, RunOptions};
    use proptest::prelude::*;
    use proptest::test_runner::{RngSeed, TestRunner};
    use serde_json::{Value as J, json};

    use super::*;
    use crate::{Blob, Collection, Db};

    /// `(id, title, brand, price, stock, active, at)`; `c` gets a blob, `d` has no price and no stock.
    const DOCS: [&str; 6] = [
        r#"{"id": "a", "title": "red running shoes", "brand": "nike", "price": 80.5, "stock": 3, "active": true, "at": "2023-11-14T22:13:20Z", "notes": {"k": [1, 2]}}"#,
        r#"{"id": "b", "title": "red trail shoes", "brand": "acme", "price": 120.0, "stock": -2, "active": false, "at": "2020-09-13T12:26:40Z"}"#,
        r#"{"id": "c", "title": "blue running shoes", "brand": "nike", "price": 60.0, "stock": 0, "active": true, "at": "2022-04-15T03:46:40Z"}"#,
        r#"{"id": "d", "title": "red hat", "brand": "acme", "active": true, "at": "1970-01-01T00:00:00Z"}"#,
        r#"{"id": "e", "title": "green boots", "brand": "zapatería", "price": 150.25, "stock": 7, "active": false, "at": "1969-12-31T00:00:00Z"}"#,
        r#"{"id": "f", "title": "red sandals", "brand": "nike", "price": 40.0, "stock": 1, "active": true, "at": "2023-11-14T22:13:20.000001Z"}"#,
    ];

    /// A database whose collection `shoes` has a field of every type, three documents in a segment and three in the
    /// memtable, one with a blob.
    fn shoes() -> (tempfile::TempDir, Db, Collection) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("app.persia")).unwrap();
        let schema = SchemaBuilder::new("shoes")
            .field(
                "title",
                FieldKind::Text(TextOptions::default().positions(true)),
            )
            .field(
                "brand",
                FieldKind::Keyword(KeywordOptions::default().facet(true)),
            )
            .field("price", FieldKind::F64 { stored: true })
            .field("stock", FieldKind::I64 { stored: true })
            .field("active", FieldKind::Bool { stored: true })
            .field("at", FieldKind::Datetime { stored: true })
            .field("notes", FieldKind::Stored)
            .build()
            .unwrap();
        let shoes = db.create_collection(schema).unwrap();
        for (i, doc) in DOCS.iter().enumerate() {
            if i == 2 {
                let schema = shoes.schema().unwrap();
                let doc = Document::from_json(doc, &schema).unwrap();
                shoes
                    .put_with_blob(&doc, Blob::from_reader(&b"\x89PNG"[..]))
                    .unwrap();
                shoes.commit().unwrap();
            } else {
                shoes.put_json(doc).unwrap();
            }
        }
        (dir, db, shoes)
    }

    /// The facade's JSON of `request` and the engine's (`QueryResponse::to_json`) for the same request on the same
    /// snapshot, as text.
    fn both(shoes: &Collection, request: &str) -> (String, String) {
        let facade = shoes.search_json(request).unwrap().to_json().unwrap();
        let engine = shoes
            .read(|c| {
                let parsed = QueryRequest::from_json(request, c.schema(), &QueryLimits::default())
                    .map_err(Error::wrap)?;
                let ran = parsed.run(c, &RunOptions::new()).map_err(Error::wrap)?;
                Ok(ran.to_json(c.schema()).unwrap())
            })
            .unwrap();
        (facade, engine)
    }

    /// ROADMAP 10.1.25: the facade writes the engine's response JSON, compared as parsed JSON values (the engine's
    /// golden text is the reference) and, where the two serializers agree on the order of every object's keys, as
    /// text.
    fn assert_same(shoes: &Collection, request: &str) -> J {
        let (facade, engine) = both(shoes, request);
        let (f, e): (J, J) = (
            serde_json::from_str(&facade).unwrap(),
            serde_json::from_str(&engine).unwrap(),
        );
        assert_eq!(f, e, "request {request}\nfacade {facade}\nengine {engine}");
        assert_eq!(facade, engine, "the same text for {request}");
        f
    }

    #[test]
    fn the_facade_writes_the_engines_response_json() {
        let (_dir, _db, shoes) = shoes();
        // Plain: every sort value type, a missing one first, facets, explanations, a projection.
        let plain = assert_same(
            &shoes,
            r#"{"q": "red shoes", "relax": false, "explain": true, "facets": ["brand"],
                "sort": ["score", {"field": "price", "order": "desc", "missing": "first"}, {"field": "stock"},
                         {"field": "active"}, {"field": "at", "order": "desc"}, {"field": "brand"}],
                "projection": ["price", "notes"]}"#,
        );
        assert!(plain["hits"][0].get("level").is_none(), "{plain}");
        assert_eq!(plain["relaxation"]["effective"], "red shoes");
        // Relaxed: L4 corrections, facets with a size, the `relaxed` explanation node.
        let fuzzy = assert_same(
            &shoes,
            r#"{"q": "red snadals", "relax": {"levels": ["fuzzy"]}, "facets": [{"field": "brand", "size": 2}],
                "explain": true}"#,
        );
        assert_eq!(
            fuzzy["relaxation"]["corrected"],
            json!({"snadals": "sandals"})
        );
        assert_eq!(fuzzy["hits"][0]["level"], "fuzzy");
        // L5: the fallback order, static rank nodes, a blob reference, no stored fields.
        let fallback = assert_same(
            &shoes,
            r#"{"q": "purple", "limit": 6, "explain": true, "projection": "none"}"#,
        );
        assert_eq!(fallback["relaxation"]["fallback"], "recency");
        // A phrase, the proximity bonus and its pairs.
        let phrase = assert_same(
            &shoes,
            r#"{"q": "\"running shoes\" red", "relax": false, "proximity": true, "explain": true}"#,
        );
        assert!(
            phrase["hits"].as_array().is_some_and(|h| !h.is_empty()),
            "{phrase}"
        );
        // The defaults, an empty page, and every term dropped.
        assert_same(&shoes, r#"{"q": "red"}"#);
        assert_same(&shoes, r#"{"q": "red", "relax": false, "limit": 0}"#);
        // `d` has no price: a `null` sort value, last ascending.
        let missing = assert_same(
            &shoes,
            r#"{"q": "red", "relax": false, "sort": [{"field": "price"}]}"#,
        );
        assert!(
            missing["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["sort"] == json!([null])),
            "{missing}"
        );
        assert_same(
            &shoes,
            r#"{"q": "shoes", "relax": false, "offset": 2, "limit": 1}"#,
        );
        assert_same(&shoes, r#"{"q": "", "relax": false}"#);
        assert_same(
            &shoes,
            r#"{"q": "red shoes running", "relax": {"levels": ["drop_terms"]}}"#,
        );
    }

    /// The parts no request of the table reaches: a keyword that is not UTF-8 (base64url), and a number that is not
    /// finite (an error, never `null`).
    #[test]
    fn a_keyword_that_is_not_utf8_is_base64url_and_a_non_finite_number_is_an_error() {
        assert_eq!(
            serde_json::to_value(Keyword(b"nike")).unwrap(),
            json!("nike")
        );
        assert_eq!(
            serde_json::to_value(Keyword(&[0xff, 0xfe])).unwrap(),
            json!({"base64url": "__4"})
        );
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = serde_json::to_string(&Finite(bad, "a score")).unwrap_err();
            assert!(err.to_string().contains("a score"), "{err}");
        }
        assert_eq!(serde_json::to_string(&Finite(-0.5, "x")).unwrap(), "-0.5");
        let (_dir, _db, shoes) = shoes();
        let mut response = shoes.search_json(r#"{"q": "red"}"#).unwrap();
        response.hits[0].score = f64::NAN;
        assert_eq!(response.to_json().unwrap_err().code(), crate::ErrorCode::Io);
    }

    /// The nodes no request of the table reaches (`constant_score` comes from a programmatic query), written as the
    /// engine writes them (SPEC §7.7).
    #[test]
    fn explanation_nodes_no_request_reaches_are_written_as_the_engine_writes_them() {
        let schema = SchemaBuilder::new("c").build().unwrap();
        let w = Writer { schema: &schema };
        let constant = Explanation::Constant {
            score: 1.5,
            query: Box::new(Explanation::All),
        };
        assert_eq!(
            serde_json::to_value(ExplanationJson { w, e: &constant }).unwrap(),
            json!({"constant_score": {"score": 1.5, "query": {"match_all": {"score": 0.0}}}})
        );
    }

    /// `Relaxation`'s getters state what the JSON states (ROADMAP 10.1.25 review): for a collection that lets the last
    /// resort drop filters, an L5 search and one whose filters were dropped.
    #[test]
    fn the_relaxation_getters_match_the_json() {
        use persia_engine::schema::{FilterRelaxation, RelaxationConfig};

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("app.persia")).unwrap();
        let schema = SchemaBuilder::new("shoes")
            .field("title", FieldKind::Text(TextOptions::default()))
            .field("brand", FieldKind::Keyword(KeywordOptions::default()))
            .relaxation(RelaxationConfig::default().filters(FilterRelaxation::LastResort))
            .build()
            .unwrap();
        let shoes = db.create_collection(schema).unwrap();
        shoes
            .put_json(r#"{"id": "a", "title": "red sandals", "brand": "nike"}"#)
            .unwrap();
        let mut seen = (false, false);
        for request in [
            r#"{"q": "purple"}"#,
            r#"{"q": "red", "soft_filter": {"term": ["brand", "zeta"]}}"#,
            r#"{"q": "red snadals", "relax": {"levels": ["fuzzy"]}}"#,
        ] {
            let response = shoes.search_json(request).unwrap();
            let json: J = serde_json::from_str(&response.to_json().unwrap()).unwrap();
            let r = &response.relaxation;
            let corrected: serde_json::Map<String, J> = r
                .corrected()
                .iter()
                .map(|c| (c.original.clone(), J::String(c.corrected.clone())))
                .collect();
            assert_eq!(
                json["relaxation"]["corrected"],
                J::Object(corrected),
                "{request}"
            );
            assert_eq!(
                json["relaxation"].get("fallback").and_then(J::as_str),
                r.fallback().map(persia_engine::relax::FallbackOrder::name),
                "{request}"
            );
            assert_eq!(
                json["relaxation"]["filters_dropped"],
                J::Bool(r.filters_dropped()),
                "{request}"
            );
            let debug = format!("{response:?}");
            assert!(
                debug.contains("relaxation") && !debug.contains("schema"),
                "{debug}"
            );
            seen.0 |= r.fallback().is_some();
            seen.1 |= r.filters_dropped();
        }
        assert_eq!(
            seen,
            (true, true),
            "an L5 search and one whose filters were dropped"
        );
    }

    /// Requests of the grammar's keys that change what a response holds, as the engine's test draws them.
    fn any_request() -> impl Strategy<Value = J> {
        let words = prop_oneof![
            Just("red"),
            Just("shoes"),
            Just("snadals"),
            Just("runing"),
            Just("purple"),
            Just("\"running shoes\""),
        ];
        let sort = prop_oneof![
            Just(json!(null)),
            Just(json!([{"field": "price", "order": "desc"}])),
            Just(json!([{"field": "brand"}, "score"])),
            Just(json!([{"field": "at", "missing": "first"}, {"field": "active"}])),
        ];
        let relax = prop_oneof![
            Just(json!(null)),
            Just(json!(false)),
            Just(json!({"levels": ["fuzzy"]})),
            Just(json!({"levels": ["prefix", "fuzzy"], "fill": true})),
        ];
        let projection = prop_oneof![
            Just(json!(null)),
            Just(json!("none")),
            Just(json!(["brand"])),
        ];
        (
            prop::collection::vec(words, 0..3),
            sort,
            relax,
            any::<bool>(),
            any::<bool>(),
            projection,
            0_usize..4,
        )
            .prop_map(|(words, sort, relax, facets, explain, projection, limit)| {
                let mut request = json!({"q": words.join(" "), "limit": limit, "explain": explain});
                let object = request.as_object_mut().unwrap();
                for (key, value) in [("sort", sort), ("relax", relax), ("projection", projection)] {
                    if !value.is_null() {
                        object.insert(key.into(), value);
                    }
                }
                if facets {
                    object.insert("facets".into(), json!(["brand"]));
                }
                request
            })
    }

    /// The facade's JSON is the engine's for any request of [`any_request`] (seeded; a failure prints its case).
    #[test]
    fn any_request_gets_the_engines_json() {
        let (_dir, _db, shoes) = shoes();
        let config = ProptestConfig {
            cases: 64,
            failure_persistence: None,
            rng_seed: RngSeed::Fixed(0x1012_5000_0001),
            ..ProptestConfig::default()
        };
        TestRunner::new(config)
            .run(&any_request(), |request| {
                let text = request.to_string();
                let (facade, engine) = both(&shoes, &text);
                prop_assert_eq!(&facade, &engine, "request {}", text);
                Ok(())
            })
            .unwrap();
    }
}
