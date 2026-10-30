//! A query planned against the collection statistics (SPEC §7.3): its scorers built once, then a matcher tree per
//! view.

use crate::query::{BoolQuery, PhraseQuery, Query, QueryError};
use crate::schema::{CollectionSchema, Field, FieldId, FieldKind};
use crate::score::TermScorer;
use crate::segment::ReadError;
use crate::stats::StatisticsProvider;
use crate::view::SearchView;

use super::boolean::{
    AllDocs, Conjunction, Disjunction, Empty, Exclusion, Optional, Scale, Scaled,
};
use super::phrase::{PhraseMatcher, PhrasePostings};
use super::word::{WordField, WordMatcher};
use super::{Hit, Matcher, SearchError, collect};

/// A query ready to run over any view of the collection whose statistics planned it: a segment, the memtable
/// (SPEC §7.3).
#[derive(Debug, Clone)]
pub struct SearchPlan {
    root: Node,
}

#[derive(Debug, Clone)]
enum Node {
    /// A `term` or `combined`: per field of the scorer, its term and whether it is a `text` field (has norms).
    Word {
        terms: Vec<(FieldId, Vec<u8>, bool)>,
        scorer: TermScorer,
    },
    Phrase {
        field: FieldId,
        terms: Vec<(Vec<u8>, u32)>,
        scorer: TermScorer,
    },
    Bool {
        must: Vec<Node>,
        should: Vec<Node>,
        must_not: Vec<Node>,
    },
    Scaled(Box<Node>, Scale),
    All,
}

type BoxedMatcher<'a> = Box<dyn Matcher + 'a>;

impl SearchPlan {
    /// Plans `query` over a collection of `schema` whose statistics `stats` gives (SPEC §7.2). The query should
    /// have passed [`Query::validate`] (the limits are not checked again); what would plan into different results
    /// is rejected here too.
    ///
    /// # Errors
    /// [`SearchError::Score`] for a field a query does not apply to or a statistic that cannot be read;
    /// [`SearchError::Query`] for a phrase on a field without positions or with decreasing positions, and a boost
    /// or constant score out of range; [`SearchError::Unsupported`] for a `prefix`, a `fuzzy` or a `bool` filter.
    pub fn new(
        query: &Query,
        schema: &CollectionSchema,
        stats: &(impl StatisticsProvider + ?Sized),
    ) -> Result<Self, SearchError> {
        Ok(Self {
            root: plan(query, schema, stats)?,
        })
    }

    /// The matcher of the plan over `view`.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`] or [`ReadError::FieldType`] if `view` is not of the planned schema; a
    /// corruption of the view.
    pub fn matcher<'a, V: SearchView>(
        &'a self,
        view: &'a V,
    ) -> Result<BoxedMatcher<'a>, ReadError> {
        build(&self.root, view)
    }

    /// The best `k` hits of the plan in `view`: score descending, then docid ascending (ROADMAP 7.4.3).
    ///
    /// # Errors
    /// As [`Self::matcher`].
    pub fn top_k<V: SearchView>(&self, view: &V, k: usize) -> Result<Vec<Hit>, ReadError> {
        collect(&mut self.matcher(view)?, k)
    }
}

fn plan(
    query: &Query,
    schema: &CollectionSchema,
    stats: &(impl StatisticsProvider + ?Sized),
) -> Result<Node, SearchError> {
    let is_text = |field| {
        matches!(
            schema.field_by_id(field).map(Field::kind),
            Some(FieldKind::Text(_))
        )
    };
    Ok(match query {
        Query::Term(q) => Node::Word {
            scorer: TermScorer::for_term(q, schema, stats)?,
            terms: vec![(q.field, q.term.as_bytes().to_vec(), is_text(q.field))],
        },
        Query::Combined(q) => Node::Word {
            scorer: TermScorer::for_combined(q, schema, stats)?,
            terms: q
                .fields
                .iter()
                .map(|c| (c.field, c.term.as_bytes().to_vec(), true))
                .collect(),
        },
        Query::Phrase(q) => {
            phrase_runs(q, schema)?;
            Node::Phrase {
                scorer: TermScorer::for_phrase(q, schema, stats)?,
                field: q.field,
                // Positions are relative to the first term, which need not be at 0.
                terms: q
                    .terms
                    .iter()
                    .map(|t| {
                        let first = q.terms.first().map_or(0, |f| f.position);
                        (t.term.as_bytes().to_vec(), t.position.saturating_sub(first))
                    })
                    .collect(),
            }
        }
        Query::Prefix(_) => return Err(SearchError::Unsupported { what: "prefix" }),
        Query::Fuzzy(_) => return Err(SearchError::Unsupported { what: "fuzzy" }),
        Query::Bool(q) => plan_bool(q, schema, stats)?,
        Query::Boost(q) if !(q.boost.is_finite() && q.boost > 0.0 && q.boost <= 1000.0) => {
            return Err(QueryError::InvalidBoost { value: q.boost }.into());
        }
        Query::ConstantScore(q) if !(q.score.is_finite() && q.score >= 0.0) => {
            return Err(QueryError::InvalidScore { value: q.score }.into());
        }
        Query::Boost(q) => Node::Scaled(
            Box::new(plan(&q.query, schema, stats)?),
            Scale::Boost(q.boost),
        ),
        Query::ConstantScore(q) => Node::Scaled(
            Box::new(plan(&q.query, schema, stats)?),
            Scale::Constant(q.score),
        ),
        Query::MatchAll => Node::All,
    })
}

fn plan_bool(
    q: &BoolQuery,
    schema: &CollectionSchema,
    stats: &(impl StatisticsProvider + ?Sized),
) -> Result<Node, SearchError> {
    if !q.filter.is_empty() {
        return Err(SearchError::Unsupported {
            what: "bool filter",
        });
    }
    let all = |clauses: &[Query]| -> Result<Vec<Node>, SearchError> {
        clauses.iter().map(|c| plan(c, schema, stats)).collect()
    };
    Ok(Node::Bool {
        must: all(&q.must)?,
        should: all(&q.should)?,
        must_not: all(&q.must_not)?,
    })
}

fn build<'a, V: SearchView>(node: &'a Node, view: &'a V) -> Result<BoxedMatcher<'a>, ReadError> {
    Ok(match node {
        Node::Word { terms, scorer } => {
            let mut fields = Vec::with_capacity(terms.len());
            for (slot, (field, term, text)) in terms.iter().enumerate() {
                let Some(postings) = view.postings(*field, term)? else {
                    continue;
                };
                let norms = if *text {
                    Some(view.norms(*field)?)
                } else {
                    None
                };
                fields.push(WordField {
                    slot,
                    postings,
                    norms,
                });
            }
            if fields.is_empty() {
                Box::new(Empty)
            } else {
                Box::new(WordMatcher::<V>::new(fields, scorer))
            }
        }
        Node::Phrase {
            field,
            terms,
            scorer,
        } => {
            let mut postings = Vec::with_capacity(terms.len());
            for (term, position) in terms {
                let Some(p) = view.postings(*field, term)? else {
                    return Ok(Box::new(Empty));
                };
                postings.push(PhrasePostings::new(p, *position));
            }
            if postings.is_empty() {
                return Ok(Box::new(Empty));
            }
            Box::new(PhraseMatcher::<V>::new(
                postings,
                view.norms(*field)?,
                scorer,
            ))
        }
        Node::Bool {
            must,
            should,
            must_not,
        } => build_bool(must, should, must_not, view)?,
        Node::Scaled(inner, scale) => Box::new(Scaled::new(build(inner, view)?, *scale)),
        Node::All => Box::new(AllDocs::new(view)),
    })
}

/// A `bool` (SPEC §7.1): every `must`, at least one `should` when there is no `must` (`filter` is not planned
/// yet), no `must_not`; an empty one matches every document.
fn build_bool<'a, V: SearchView>(
    must: &'a [Node],
    should: &'a [Node],
    must_not: &'a [Node],
    view: &'a V,
) -> Result<BoxedMatcher<'a>, ReadError> {
    let all = |nodes: &'a [Node]| -> Result<Vec<BoxedMatcher<'a>>, ReadError> {
        nodes.iter().map(|n| build(n, view)).collect()
    };
    let optional = disjunction(all(should)?);
    let (required, optional): (BoxedMatcher<'a>, _) = if !must.is_empty() {
        (conjunction(all(must)?), optional)
    } else if should.is_empty() {
        (Box::new(AllDocs::new(view)), None)
    } else {
        // Some `should` is required: none that can match in this view means no document.
        (optional.unwrap_or_else(|| Box::new(Empty)), None)
    };
    let scored = match optional {
        Some(optional) => Box::new(Optional::new(required, optional)),
        None => required,
    };
    Ok(match disjunction(all(must_not)?) {
        Some(excluded) => Box::new(Exclusion::new(scored, excluded)),
        None => scored,
    })
}

/// AND of at least one clause; no document if a clause has none in the view.
fn conjunction(mut clauses: Vec<BoxedMatcher<'_>>) -> BoxedMatcher<'_> {
    if clauses.iter().any(|c| c.cost() == 0) {
        return Box::new(Empty);
    }
    if clauses.len() == 1
        && let Some(only) = clauses.pop()
    {
        return only;
    }
    Box::new(Conjunction::new(clauses))
}

/// OR of the clauses that can match; `None` when none can.
fn disjunction(mut clauses: Vec<BoxedMatcher<'_>>) -> Option<BoxedMatcher<'_>> {
    clauses.retain(|c| c.cost() > 0);
    match clauses.len() {
        0 => None,
        1 => clauses.pop(),
        _ => Some(Box::new(Disjunction::new(clauses))),
    }
}

/// Checks what would otherwise silently change what a phrase matches (SPEC §7.1): its field indexes positions,
/// and its terms are in non-decreasing positions.
fn phrase_runs(q: &PhraseQuery, schema: &CollectionSchema) -> Result<(), QueryError> {
    let Some(field) = schema.field_by_id(q.field) else {
        return Err(QueryError::UnknownFieldId { field: q.field });
    };
    if let FieldKind::Text(o) = field.kind()
        && !o.positions
    {
        return Err(QueryError::NoPositions {
            field: field.name().to_owned(),
        });
    }
    if q.terms.windows(2).any(|w| match w {
        [a, b] => a.position > b.position,
        _ => false,
    }) {
        return Err(QueryError::InvalidPhrase {
            field: field.name().to_owned(),
        });
    }
    Ok(())
}
