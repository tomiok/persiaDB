//! The programmatic query AST (SPEC §7.1) and its validation against a schema and [`QueryLimits`].

use crate::schema::{CollectionSchema, Field, FieldId, FieldKind, FieldType};
use crate::segment::MAX_TERM_LEN;

use super::QueryError;
use super::filter::Filter;
use super::limits::{QueryLimits, Usage};

/// A query: what documents match and how they score (SPEC §7.1). Terms are given as analyzed (the form the index
/// holds them in): [`super::QueryCompiler`] analyzes user text; a hand-built query names terms as they are.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Query {
    /// One term of one `text` or `keyword` field.
    Term(TermQuery),
    /// One query word over several `text` fields, scored as a single BM25F term (SPEC §7.2).
    Combined(CombinedQuery),
    /// Terms at given relative positions of one `text` field that indexes positions.
    Phrase(PhraseQuery),
    /// The terms of one field that start with a prefix.
    Prefix(PrefixQuery),
    /// The terms of one field within an edit distance of a term.
    Fuzzy(FuzzyQuery),
    /// A boolean combination.
    Bool(BoolQuery),
    /// A query whose scores are multiplied by a factor.
    Boost(BoostQuery),
    /// A query whose matches all score the same.
    ConstantScore(ConstantScoreQuery),
    /// Every document, each scoring 0.
    MatchAll,
}

/// [`Query::Term`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermQuery {
    /// The field: `text` or `keyword`.
    pub field: FieldId,
    /// The term, as the index holds it.
    pub term: String,
}

/// [`Query::Combined`]: a document matches when it holds any of the terms in its field; BM25F sums the weighted
/// frequencies of all of them (SPEC §7.2).
#[derive(Debug, Clone, PartialEq)]
pub struct CombinedQuery {
    /// One term per field: at least one, each field once.
    pub fields: Vec<WeightedTerm>,
}

/// One field of a [`CombinedQuery`].
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedTerm {
    /// The field: `text`.
    pub field: FieldId,
    /// The term, as that field's analyzer produces it.
    pub term: String,
    /// The field weight `w_f` of SPEC §7.2: `0 < weight ≤ 1000`; the schema's `boost` unless `fields` overrides it.
    pub weight: f64,
}

/// [`Query::Phrase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhraseTerm {
    /// The term, as the index holds it.
    pub term: String,
    /// Its position relative to the phrase's first term: gaps left by removed tokens (stopwords) are kept.
    pub position: u32,
}

/// [`Query::Phrase`]: documents holding every term at its relative position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhraseQuery {
    /// The field: `text` with `positions: true`.
    pub field: FieldId,
    /// At least one term, in non-decreasing positions.
    pub terms: Vec<PhraseTerm>,
}

/// [`Query::Prefix`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixQuery {
    /// The field: `text` or `keyword`.
    pub field: FieldId,
    /// The prefix, not empty.
    pub prefix: String,
    /// Most terms it expands to (at least 1); the rest are left out (SPEC §7.6, L3).
    pub max_expansions: u32,
}

/// [`Query::Fuzzy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyQuery {
    /// The field: `text` or `keyword`.
    pub field: FieldId,
    /// The term.
    pub term: String,
    /// The Levenshtein distance: 1 or 2 (SPEC §7.6, L4).
    pub distance: u8,
    /// Most terms it expands to (at least 1).
    pub max_expansions: u32,
}

/// [`Query::Bool`]. A document matches when it matches every `must` and `filter` clause and no `must_not` clause,
/// and at least one `should` clause when there is no `must` or `filter` clause and some `should`. So a query of
/// `must_not` clauses alone matches every other document, and an empty one matches every document. `must` and
/// `should` clauses score; `filter` and `must_not` clauses do not.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BoolQuery {
    /// Clauses every match satisfies; their scores add up.
    pub must: Vec<Query>,
    /// Optional clauses whose scores add up (see the type's docs for when one is required).
    pub should: Vec<Query>,
    /// Clauses no match satisfies.
    pub must_not: Vec<Query>,
    /// Filters every match satisfies, without scoring (SPEC §7.1.2).
    pub filter: Vec<Filter>,
}

/// [`Query::Boost`].
#[derive(Debug, Clone, PartialEq)]
pub struct BoostQuery {
    /// The query.
    pub query: Box<Query>,
    /// The factor: `0 < boost ≤ 1000`.
    pub boost: f64,
}

/// [`Query::ConstantScore`].
#[derive(Debug, Clone, PartialEq)]
pub struct ConstantScoreQuery {
    /// The query that selects the documents.
    pub query: Box<Query>,
    /// The score of each: finite, at least 0.
    pub score: f64,
}

impl Query {
    /// Checks the query against `schema` and `limits`: every field exists and has a type the query applies to,
    /// terms fit the index, numbers are in range, and the query is within the limits (SPEC §7.1.3).
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
        match self {
            Self::Term(q) => {
                usage.add(limits, depth, 1, 0)?;
                let field = field_of(schema, q.field, "a term query", searchable)?;
                term_fits(field, &q.term)
            }
            Self::Combined(q) => {
                usage.add(limits, depth, q.fields.len(), 0)?;
                if q.fields.is_empty() {
                    return Err(QueryError::EmptyCombined);
                }
                for (i, clause) in q.fields.iter().enumerate() {
                    let field = field_of(schema, clause.field, "a combined query", is_text)?;
                    if q.fields.iter().take(i).any(|c| c.field == clause.field) {
                        return Err(QueryError::DuplicateField {
                            field: field.name().to_owned(),
                        });
                    }
                    term_fits(field, &clause.term)?;
                    boost_in_range(clause.weight)?;
                }
                Ok(())
            }
            Self::Phrase(q) => {
                usage.add(limits, depth, q.terms.len(), 0)?;
                let field = field_of(schema, q.field, "a phrase", is_text)?;
                if !matches!(field.kind(), FieldKind::Text(o) if o.positions) {
                    return Err(QueryError::NoPositions {
                        field: field.name().to_owned(),
                    });
                }
                let ordered = q.terms.windows(2).all(|w| match w {
                    [a, b] => a.position <= b.position,
                    _ => true,
                });
                if q.terms.is_empty() || !ordered {
                    return Err(QueryError::InvalidPhrase {
                        field: field.name().to_owned(),
                    });
                }
                q.terms.iter().try_for_each(|t| term_fits(field, &t.term))
            }
            Self::Prefix(q) => {
                usage.add(limits, depth, 1, expansions(q.max_expansions)?)?;
                let field = field_of(schema, q.field, "a prefix query", searchable)?;
                if q.prefix.is_empty() {
                    return Err(QueryError::EmptyPrefix {
                        field: field.name().to_owned(),
                    });
                }
                term_fits(field, &q.prefix)
            }
            Self::Fuzzy(q) => {
                usage.add(limits, depth, 1, expansions(q.max_expansions)?)?;
                let field = field_of(schema, q.field, "a fuzzy query", searchable)?;
                if !matches!(q.distance, 1 | 2) {
                    return Err(QueryError::InvalidDistance {
                        distance: q.distance,
                    });
                }
                term_fits(field, &q.term)
            }
            Self::Bool(q) => {
                usage.add(limits, depth, 0, 0)?;
                let child = depth.saturating_add(1);
                for clause in q.must.iter().chain(&q.should).chain(&q.must_not) {
                    clause.check(schema, limits, usage, child)?;
                }
                q.filter
                    .iter()
                    .try_for_each(|f| f.check(schema, limits, usage, child))
            }
            Self::Boost(q) => {
                usage.add(limits, depth, 0, 0)?;
                boost_in_range(q.boost)?;
                q.query
                    .check(schema, limits, usage, depth.saturating_add(1))
            }
            Self::ConstantScore(q) => {
                usage.add(limits, depth, 0, 0)?;
                if !q.score.is_finite() || q.score < 0.0 {
                    return Err(QueryError::InvalidScore { value: q.score });
                }
                q.query
                    .check(schema, limits, usage, depth.saturating_add(1))
            }
            Self::MatchAll => usage.add(limits, depth, 0, 0),
        }
    }
}

/// The field `id` of `schema` if `applies` to its type; `what` names the query for the error.
pub(super) fn field_of<'a>(
    schema: &'a CollectionSchema,
    id: FieldId,
    what: &'static str,
    applies: fn(FieldType) -> bool,
) -> Result<&'a Field, QueryError> {
    let field = schema
        .field_by_id(id)
        .ok_or(QueryError::UnknownFieldId { field: id })?;
    if applies(field.field_type()) {
        Ok(field)
    } else {
        Err(QueryError::WrongFieldType {
            field: field.name().to_owned(),
            field_type: field.field_type(),
            what,
        })
    }
}

/// Whether a field's terms are in a term dictionary (SPEC §5.3.1).
pub(super) fn searchable(t: FieldType) -> bool {
    matches!(t, FieldType::Text | FieldType::Keyword)
}

fn is_text(t: FieldType) -> bool {
    t == FieldType::Text
}

fn term_fits(field: &Field, term: &str) -> Result<(), QueryError> {
    if term.len() > MAX_TERM_LEN {
        return Err(QueryError::TermTooLong {
            field: field.name().to_owned(),
            len: term.len(),
        });
    }
    Ok(())
}

/// The range of a schema `boost` (SPEC §6.2.2), which query boosts and field weights share.
pub(super) fn boost_in_range(value: f64) -> Result<(), QueryError> {
    if value.is_finite() && value > 0.0 && value <= 1000.0 {
        Ok(())
    } else {
        Err(QueryError::InvalidBoost { value })
    }
}

fn expansions(max_expansions: u32) -> Result<usize, QueryError> {
    match max_expansions {
        0 => Err(QueryError::NoExpansions),
        n => Ok(usize::try_from(n).unwrap_or(usize::MAX)),
    }
}
