//! Why a document scores what it does (SPEC §7.7, ROADMAP 7.7.1): the plan's tree evaluated on one document, each
//! word with its BM25F parts per field (SPEC §7.2).
//!
//! An explanation reads the same postings and norms a matcher does and sums in the same order, so its score is the
//! hit's, bit for bit. The tree is built once per view, its leaves exhaustive matchers, and the documents asked are
//! walked in docid order, each `bool` selecting its filters once (ROADMAP 7.7.2): the hits of a page in a view cost
//! about one unpruned search of it.

use roaring::RoaringBitmap;

use crate::query::Filter;
use crate::schema::FieldId;
use crate::score::TermScorer;
use crate::segment::{ReadError, norms};
use crate::view::SearchView;

use super::boolean::Scale;
use super::filter::select;
use super::phrase::PhraseMatcher;
use super::plan::{Node, phrase_matcher, word_matcher};
use super::word::WordMatcher;
use super::{Matcher, Pruning, sum_in_order};

/// How a document's score adds up (SPEC §7.7): a tree like the query's, with only the clauses that match it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Explanation {
    /// A `term`, a `combined`, or a term a `prefix` or `fuzzy` kept.
    Word(WordExplanation),
    /// A `phrase`.
    Phrase(PhraseExplanation),
    /// A `bool`: the sum of its `must` clauses, then of its matching `should` clauses, each in query order. Filters
    /// and `must_not` clauses score nothing and are left out.
    Bool {
        /// The score.
        score: f64,
        /// Every `must` clause, in query order.
        must: Vec<Explanation>,
        /// The `should` clauses the document matches, in query order.
        should: Vec<Explanation>,
    },
    /// A `boost` (or a `fuzzy` term's edit-distance weight, SPEC §7.1): its query's score times `boost`.
    Boost {
        /// The score.
        score: f64,
        /// The factor.
        boost: f64,
        /// The query boosted.
        query: Box<Explanation>,
    },
    /// A `constant_score`: `score` whatever its query scores.
    Constant {
        /// The score.
        score: f64,
        /// The query, which only has to match.
        query: Box<Explanation>,
    },
    /// `match_all`, or a `bool` with neither `must` nor `should`: 0.
    All,
}

impl Explanation {
    /// The score: the hit's, bit for bit.
    pub fn score(&self) -> f64 {
        match self {
            Self::Word(w) => w.score,
            Self::Phrase(p) => p.score,
            Self::Bool { score, .. } | Self::Boost { score, .. } | Self::Constant { score, .. } => {
                *score
            }
            Self::All => 0.0,
        }
    }
}

/// One query word scored as a BM25F term (SPEC §7.2): `score = idf · tf~ · (k1 + 1) / (k1 + tf~)`, with `tf~` the
/// sum of its fields' contributions.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct WordExplanation {
    /// The score.
    pub score: f64,
    /// `idf(t)`, from the collection `df` (for an expansion, the largest kept term's, SPEC §7.1).
    pub idf: f64,
    /// The `k1` the word saturates with.
    pub k1: f64,
    /// `tf~(t, d)`: the fields' contributions summed in query order.
    pub tf: f64,
    /// The fields that hold the word in the document, in query order.
    pub fields: Vec<FieldExplanation>,
}

/// One field's part of a word's `tf~` (SPEC §7.2): `contribution = tf · factor`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FieldExplanation {
    /// The field.
    pub field: FieldId,
    /// The field's analyzed term.
    pub term: Vec<u8>,
    /// Its frequency in the document's field.
    pub tf: u32,
    /// The document's norm byte in the field (SPEC §5.3.4); 0 for a `keyword` field.
    pub norm: u8,
    /// The length the norm byte stands for; `None` for a `keyword` field, which has no norms.
    pub length: Option<u32>,
    /// `w_f / (1 − b_f + b_f · len_f(d)/avglen_f)`.
    pub factor: f64,
    /// `tf · factor`.
    pub contribution: f64,
}

/// A phrase scored as one term over its field (SPEC §7.2), from its phrase frequency.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PhraseExplanation {
    /// The score.
    pub score: f64,
    /// The field.
    pub field: FieldId,
    /// Its terms, in query order.
    pub terms: Vec<Vec<u8>>,
    /// The sum of its terms' `idf`s.
    pub idf: f64,
    /// The field's `k1`.
    pub k1: f64,
    /// How many distinct starts hold the phrase.
    pub freq: u32,
    /// The document's norm byte in the field.
    pub norm: u8,
    /// The length the norm byte stands for.
    pub length: u32,
    /// `w_f / (1 − b_f + b_f · len_f(d)/avglen_f)`.
    pub factor: f64,
    /// `tf~ = freq · factor`.
    pub tf: f64,
}

/// The explanations of `docids` in `view` under `node`, in the order given; `None` for a document `node` does not
/// match. The tree is built once and walked in docid order (ROADMAP 7.7.2).
pub(super) fn explain_all<V: SearchView>(
    node: &Node,
    view: &V,
    docids: &[u32],
) -> Result<Vec<Option<Explanation>>, ReadError> {
    let mut order: Vec<usize> = (0..docids.len()).collect();
    order.sort_by_key(|&i| docids.get(i).copied());
    let mut tree = Explainer::new(node, view)?;
    let mut out = vec![None; docids.len()];
    for i in order {
        if let (Some(&docid), Some(slot)) = (docids.get(i), out.get_mut(i)) {
            *slot = tree.explain(view, docid)?;
        }
    }
    Ok(out)
}

/// A plan's tree over one view, for documents asked in ascending docid order: each leaf's matcher only advances,
/// and a `bool` selects its filters once, the first time it needs them.
enum Explainer<'a, V: SearchView> {
    Word {
        terms: &'a [(FieldId, Vec<u8>, bool)],
        scorer: &'a TermScorer,
        matcher: Option<WordMatcher<'a, V>>,
    },
    Phrase {
        field: FieldId,
        terms: &'a [(Vec<u8>, u32)],
        scorer: &'a TermScorer,
        matcher: Option<PhraseMatcher<'a, V>>,
    },
    Bool {
        must: Vec<Explainer<'a, V>>,
        should: Vec<Explainer<'a, V>>,
        must_not: Vec<Explainer<'a, V>>,
        filter: &'a [Filter],
        selected: Option<RoaringBitmap>,
    },
    Scaled(Box<Explainer<'a, V>>, Scale),
    All,
    Nothing,
}

impl<'a, V: SearchView> Explainer<'a, V> {
    fn new(node: &'a Node, view: &'a V) -> Result<Self, ReadError> {
        let all = |nodes: &'a [Node]| -> Result<Vec<Self>, ReadError> {
            nodes.iter().map(|n| Self::new(n, view)).collect()
        };
        Ok(match node {
            Node::Word { terms, scorer } => Self::Word {
                terms,
                scorer,
                matcher: word_matcher(terms, scorer, view, Pruning::Exhaustive)?,
            },
            Node::Phrase {
                field,
                terms,
                scorer,
            } => Self::Phrase {
                field: *field,
                terms,
                scorer,
                matcher: phrase_matcher(*field, terms, scorer, view)?,
            },
            Node::Bool {
                must,
                should,
                must_not,
                filter,
            } => Self::Bool {
                must: all(must)?,
                should: all(should)?,
                must_not: all(must_not)?,
                filter,
                selected: None,
            },
            Node::Scaled(inner, scale) => Self::Scaled(Box::new(Self::new(inner, view)?), *scale),
            Node::All => Self::All,
            Node::Nothing => Self::Nothing,
        })
    }

    /// The explanation of `docid`, at or after every document asked before; `None` when the tree does not match it.
    fn explain(&mut self, view: &V, docid: u32) -> Result<Option<Explanation>, ReadError> {
        Ok(match self {
            Self::Word {
                terms,
                scorer,
                matcher,
            } => match matcher {
                Some(m) => word(terms, scorer, m, docid)?,
                None => None,
            },
            Self::Phrase {
                field,
                terms,
                scorer,
                matcher,
            } => match matcher {
                Some(m) => phrase(*field, terms, scorer, m, docid)?,
                None => None,
            },
            Self::Bool {
                must,
                should,
                must_not,
                filter,
                selected,
            } => boolean(must, should, must_not, filter, selected, view, docid)?,
            Self::Scaled(inner, scale) => {
                let Some(query) = inner.explain(view, docid)? else {
                    return Ok(None);
                };
                Some(match *scale {
                    Scale::Boost(boost) => Explanation::Boost {
                        score: query.score() * boost,
                        boost,
                        query: Box::new(query),
                    },
                    Scale::Constant(score) => Explanation::Constant {
                        score,
                        query: Box::new(query),
                    },
                })
            }
            Self::All => view.is_live(docid).then_some(Explanation::All),
            Self::Nothing => None,
        })
    }
}

/// Whether `m` matches `docid`: moved onto it, or left after it (no match) for the next, larger docid.
fn reaches(m: &mut impl Matcher, docid: u32) -> Result<bool, ReadError> {
    Ok(m.advance(docid)? == Some(docid))
}

fn word<V: SearchView>(
    terms: &[(FieldId, Vec<u8>, bool)],
    scorer: &TermScorer,
    m: &mut WordMatcher<'_, V>,
    docid: u32,
) -> Result<Option<Explanation>, ReadError> {
    if !reaches(m, docid)? {
        return Ok(None);
    }
    let hits = m.field_hits()?;
    let mut fields = Vec::new();
    for (slot, ((field, term, text), hit)) in terms.iter().zip(hits).enumerate() {
        let Some(hit) = hit.filter(|h| h.tf > 0) else {
            continue;
        };
        let factor = field_factor(scorer, slot, hit.norm);
        fields.push(FieldExplanation {
            field: *field,
            term: term.clone(),
            tf: hit.tf,
            norm: hit.norm,
            length: text.then(|| norms::decode(hit.norm)),
            factor,
            contribution: f64::from(hit.tf) * factor,
        });
    }
    Ok(Some(Explanation::Word(WordExplanation {
        score: scorer.score(hits),
        idf: scorer.idf(),
        k1: scorer.k1(),
        tf: scorer.tf(hits),
        fields,
    })))
}

fn phrase<V: SearchView>(
    field: FieldId,
    terms: &[(Vec<u8>, u32)],
    scorer: &TermScorer,
    m: &mut PhraseMatcher<'_, V>,
    docid: u32,
) -> Result<Option<Explanation>, ReadError> {
    if !reaches(m, docid)? {
        return Ok(None);
    }
    let Some(hit) = m.field_hit()? else {
        return Ok(None);
    };
    let factor = field_factor(scorer, 0, hit.norm);
    Ok(Some(Explanation::Phrase(PhraseExplanation {
        score: scorer.score(&[Some(hit)]),
        field,
        terms: terms.iter().map(|(t, _)| t.clone()).collect(),
        idf: scorer.idf(),
        k1: scorer.k1(),
        freq: hit.tf,
        norm: hit.norm,
        length: norms::decode(hit.norm),
        factor,
        tf: scorer.tf(&[Some(hit)]),
    })))
}

fn boolean<V: SearchView>(
    must: &mut [Explainer<'_, V>],
    should: &mut [Explainer<'_, V>],
    must_not: &mut [Explainer<'_, V>],
    filter: &[Filter],
    selected: &mut Option<RoaringBitmap>,
    view: &V,
    docid: u32,
) -> Result<Option<Explanation>, ReadError> {
    for clause in must_not.iter_mut() {
        if clause.explain(view, docid)?.is_some() {
            return Ok(None);
        }
    }
    let mut musts = Vec::with_capacity(must.len());
    for clause in must.iter_mut() {
        let Some(e) = clause.explain(view, docid)? else {
            return Ok(None);
        };
        musts.push(e);
    }
    if !filter.is_empty() {
        let docs = match selected {
            Some(docs) => docs,
            None => selected.insert(select(filter, view)?),
        };
        if !docs.contains(docid) {
            return Ok(None);
        }
    }
    let mut shoulds = Vec::new();
    for clause in should.iter_mut() {
        if let Some(e) = clause.explain(view, docid)? {
            shoulds.push(e);
        }
    }
    // Some `should` is required when there is neither `must` nor `filter`.
    if must.is_empty() && filter.is_empty() && !should.is_empty() && shoulds.is_empty() {
        return Ok(None);
    }
    if musts.is_empty() && shoulds.is_empty() {
        // Every live document of the view, or those the filters select (already live).
        let live = !filter.is_empty() || view.is_live(docid);
        return Ok(live.then_some(Explanation::All));
    }
    // As the matchers add: the conjunction's sum, then the optional disjunction's when it matches.
    let required = sum_in_order(musts.iter().map(Explanation::score));
    let optional = sum_in_order(shoulds.iter().map(Explanation::score));
    let score = match (musts.is_empty(), shoulds.is_empty()) {
        (false, false) => required + optional,
        (false, true) => required,
        (true, _) => optional,
    };
    Ok(Some(Explanation::Bool {
        score,
        must: musts,
        should: shoulds,
    }))
}

/// The factor of `slot` for `norm`. A word's terms and its scorer's fields are planned together, so every slot is
/// one of the scorer's; were it not, the scorer would not have scored that field either (0).
fn field_factor(scorer: &TermScorer, slot: usize, norm: u8) -> f64 {
    scorer.factor(slot, norm).unwrap_or(0.0)
}
