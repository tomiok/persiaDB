//! The synonym scan of a compilation (SPEC §6.3.2, §7.6 L1): which runs of the query's required words are a term of
//! the collection's synonym groups, and what each expands to. L0 does not search them; the relaxation cascade's L1
//! `synonyms` level ORs each run with its alternatives (ROADMAP 8.2.1, ADR-0039).
//!
//! Each default field scans its own analysis of the query, as SPEC §6.3.2 analyzes the groups per field: the tokens of
//! the required plain words, their positions continuing from one clause to the next as one analysis of the words
//! would number them (a removed stopword keeps its position). An excluded clause, a phrase and a `field:` clause end the run: a match never spans one.
//! A match starts and ends only where a clause does, so it replaces clauses, not part of a word. Fields that match
//! the same clauses share their alternatives; a match overlapping another field's earlier one is dropped.

use std::collections::BTreeSet;
use std::ops::Range;

use persia_analysis::{DEFAULT_SYNONYM_EXPANSIONS, Token, TokenStream};

use super::{BoolQuery, Clause, CompiledClause, DefaultFields, Query, QueryCompiler, QueryToken};
use crate::query::fit::share;
use crate::query::limits::QueryLimits;

/// A run of required words of the user text that is a term of the collection's synonym groups, and what the
/// relaxation cascade's L1 `synonyms` ORs it with (SPEC §6.3.2, §7.6 L1).
///
/// ```
/// use persia_analysis::AnalyzerRegistry;
/// use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
/// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
///
/// let schema = SchemaBuilder::new("c")
///     .field("title", FieldKind::Text(TextOptions { positions: true, ..TextOptions::default() }))
///     .synonyms(vec![vec!["TV".into(), "television set".into()]])
///     .build()?;
/// let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())?;
/// let compiled = compiler.compile("cheap tv", &DefaultFields::all(&schema), &QueryLimits::default());
/// // `tv` is the second clause; it expands to the phrase "television set".
/// assert_eq!(compiled.synonyms.len(), 1);
/// assert_eq!(compiled.synonyms[0].clauses, 1..2);
/// assert_eq!(compiled.synonyms[0].alternatives.len(), 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CompiledSynonym {
    /// The clauses the run covers: consecutive indexes into [`super::Compiled::clauses`], each a required word.
    pub clauses: Range<usize>,
    /// The other terms of every group holding the run's term, each compiled as the quoted phrase `"<term>"` over the
    /// default fields would be (a phrase in a field with positions, every term required in one without, one word
    /// over the fields when it is one token). In group order, then term order, each once, without one that compiles
    /// to the run's own query, and at most [`DEFAULT_SYNONYM_EXPANSIONS`] of them. Never empty.
    pub alternatives: Vec<Query>,
}

/// A clause's tokens in each default field, in the order of [`DefaultFields::fields`]: each token's term and position
/// within the clause, and how many positions the clause took in the field ([`persia_analysis::QueryAnalysis`]).
#[derive(Debug, Default)]
pub(super) struct Captured {
    fields: Vec<(Vec<(String, u32)>, u32)>,
}

impl Captured {
    /// The next default field's tokens of the clause, and the positions it took.
    pub(super) fn push(&mut self, tokens: &[QueryToken], positions: u32) {
        self.fields.push((
            tokens
                .iter()
                .map(|t| (t.term.clone(), t.position))
                .collect(),
            positions,
        ));
    }
}

/// One default field's run of required words: their tokens, positions continuing across clauses, and each token's
/// clause.
#[derive(Debug, Default)]
struct Run {
    tokens: Vec<Token>,
    clause_of: Vec<usize>,
    /// The position the next clause starts at.
    next: u32,
}

/// A field's match of whole clauses, and where its alternatives were given in the collection's groups.
#[derive(Debug)]
struct Candidate {
    field: usize,
    clauses: Range<usize>,
    sources: Vec<(usize, usize)>,
}

/// A run of clauses some field matched, and where the alternatives of every field that matched it were given:
/// (group, index in it), whose order is the alternatives' (group, then term).
type Accepted = (Range<usize>, BTreeSet<(usize, usize)>);

/// The synonym scan of one compilation.
#[derive(Debug)]
pub(super) struct Scan<'c> {
    compiler: &'c QueryCompiler,
    fields: &'c DefaultFields,
    /// Whether the collection has synonyms: otherwise nothing is captured or scanned.
    on: bool,
    runs: Vec<Run>,
    candidates: Vec<Candidate>,
}

impl<'c> Scan<'c> {
    pub(super) fn new(compiler: &'c QueryCompiler, fields: &'c DefaultFields) -> Self {
        let on = !compiler.schema.synonyms().is_empty();
        Self {
            compiler,
            fields,
            on,
            runs: if on {
                fields.fields().iter().map(|_| Run::default()).collect()
            } else {
                Vec::new()
            },
            candidates: Vec::new(),
        }
    }

    /// Whether clauses are to be captured.
    pub(super) fn scans(&self) -> bool {
        self.on
    }

    /// A clause that compiled to nothing: a required word every field analyzed to no token keeps the positions it
    /// took in each field (a stopword's, SPEC §6.3.2: a stopword gap must line up); any other clause ends the run.
    pub(super) fn dropped(&mut self, eligible: bool, captured: &Captured) {
        if !self.on {
            return;
        }
        if !eligible {
            self.end_run();
            return;
        }
        for (run, &(_, positions)) in self.runs.iter_mut().zip(&captured.fields) {
            run.next = run.next.saturating_add(positions);
        }
    }

    /// Clause `index` of the compilation: a required word joins the run, its positions after the clause before it, as
    /// one analysis of the run's words would number them; any other clause ends the run.
    pub(super) fn clause(&mut self, eligible: bool, index: usize, captured: Captured) {
        if !self.on {
            return;
        }
        if !eligible {
            self.end_run();
            return;
        }
        for (run, (tokens, positions)) in self.runs.iter_mut().zip(captured.fields) {
            let base = run.next;
            for (text, position) in tokens {
                run.tokens.push(Token {
                    text,
                    start: 0,
                    end: 0,
                    position: base.saturating_add(position),
                });
                run.clause_of.push(index);
            }
            run.next = base.saturating_add(positions);
        }
    }

    /// Finds each field's matches of whole clauses in the run, and starts a new run. Matches start and end only where a
    /// clause does, so a longer term ending inside a word does not hide a shorter one ending with a word.
    fn end_run(&mut self) {
        for (field, run) in self.runs.iter_mut().enumerate() {
            let run = std::mem::take(run);
            let Some(map) = self
                .fields
                .fields()
                .get(field)
                .and_then(|&(id, _)| self.compiler.synonym_map(id))
            else {
                continue;
            };
            if run.tokens.is_empty() {
                continue;
            }
            let boundary = |i: usize| {
                i == 0
                    || i >= run.clause_of.len()
                    || run.clause_of.get(i - 1) != run.clause_of.get(i)
            };
            for matched in map.find_between(&run.tokens, boundary) {
                let Some(clauses) = whole_clauses(&run.clause_of, matched.tokens()) else {
                    continue;
                };
                self.candidates.push(Candidate {
                    field,
                    clauses,
                    sources: map.alternative_sources(&matched, DEFAULT_SYNONYM_EXPANSIONS),
                });
            }
        }
    }

    /// The runs that match a synonym term, each with its alternatives, in text order. `root` and `clauses` are the
    /// compilation's root `bool` (before a single part collapses it) and clauses, and `terms` the terms of its query.
    ///
    /// L1's step holds L0's terms twice (its own clauses, and L0's query, which it excludes) and at least a term per
    /// alternative (SPEC §7.1.3), so at most `limits.max_terms − 2 · terms` alternatives are compiled in all, each run's
    /// share as [`share`] cuts (the longest lists first, at least one each), and none when that leaves less than one
    /// per run: such a step never fits. A hostile text therefore costs the compilation no more than the limits allow
    /// the step. A cascade held to larger limits than the compilation's still sees only these.
    pub(super) fn finish(
        mut self,
        root: &BoolQuery,
        clauses: &[CompiledClause],
        limits: &QueryLimits,
        terms: usize,
    ) -> Vec<CompiledSynonym> {
        if !self.on {
            return Vec::new();
        }
        self.end_run();
        let mut candidates = std::mem::take(&mut self.candidates);
        // Fields in default-field order, then text order: an earlier field's match wins an overlap.
        candidates.sort_by_key(|c| c.field);
        let mut accepted: Vec<Accepted> = Vec::new();
        for candidate in candidates {
            if let Some((_, sources)) = accepted.iter_mut().find(|(r, _)| *r == candidate.clauses) {
                sources.extend(candidate.sources);
            } else if !accepted
                .iter()
                .any(|(r, _)| r.start < candidate.clauses.end && candidate.clauses.start < r.end)
            {
                accepted.push((candidate.clauses, candidate.sources.into_iter().collect()));
            }
        }
        accepted.sort_by_key(|(r, _)| r.start);
        // Each run's share, by its sources: duplicates compile away, so a run may end with fewer.
        let mut counts: Vec<u32> = accepted
            .iter()
            .map(|(_, s)| {
                u32::try_from(s.len().min(DEFAULT_SYNONYM_EXPANSIONS)).unwrap_or(u32::MAX)
            })
            .collect();
        let mut caps: Vec<&mut u32> = counts.iter_mut().collect();
        let spare = limits.max_terms.saturating_sub(terms.saturating_mul(2));
        if !share(&mut caps, spare) {
            return Vec::new();
        }
        accepted
            .into_iter()
            .zip(counts)
            .filter_map(|((range, sources), count)| {
                let original = original(root, clauses, &range)?;
                let limit = usize::try_from(count).unwrap_or(usize::MAX);
                let alternatives = self.alternatives(&original, &sources, limit);
                (!alternatives.is_empty()).then_some(CompiledSynonym {
                    clauses: range,
                    alternatives,
                })
            })
            .collect()
    }

    /// The raw terms at `sources` (group, index) compiled as quoted phrases over the default fields, in source order,
    /// each once and none equal to `original`, at most `limit`.
    fn alternatives(
        &self,
        original: &Query,
        sources: &BTreeSet<(usize, usize)>,
        limit: usize,
    ) -> Vec<Query> {
        let groups = self.compiler.schema.synonyms();
        let mut tokens = TokenStream::new();
        let mut alternatives: Vec<Query> = Vec::new();
        for &(group, index) in sources {
            if alternatives.len() >= limit {
                break;
            }
            let Some(text) = groups.get(group).and_then(|g| g.get(index)) else {
                continue;
            };
            let phrase = Clause {
                negated: false,
                field: None,
                phrase: true,
                text,
            };
            let (mut parts, _, _) = self
                .compiler
                .clause(phrase, self.fields, &mut tokens, false);
            let query = match parts.len() {
                0 => continue,
                1 => parts.pop().unwrap_or(Query::MatchAll),
                _ => Query::Bool(BoolQuery {
                    must: parts,
                    ..BoolQuery::default()
                }),
            };
            if query != *original && !alternatives.contains(&query) {
                alternatives.push(query);
            }
        }
        alternatives
    }
}

/// The clauses `tokens` (indexes into a run) cover, when they cover them whole: the first token is its clause's first in
/// the run and the last its clause's last.
fn whole_clauses(clause_of: &[usize], tokens: Range<usize>) -> Option<Range<usize>> {
    let first = *clause_of.get(tokens.start)?;
    let last = *clause_of.get(tokens.end.checked_sub(1)?)?;
    let starts = tokens.start == 0 || clause_of.get(tokens.start - 1) != Some(&first);
    let ends = clause_of.get(tokens.end) != Some(&last);
    (starts && ends).then(|| first..last.saturating_add(1))
}

/// The query of required clauses `range` in `root`: their parts, one alone or all required. `None` when a clause is
/// excluded or its parts are not in `root` (never, for a compilation's own clauses).
fn original(root: &BoolQuery, clauses: &[CompiledClause], range: &Range<usize>) -> Option<Query> {
    let covered = clauses.get(range.clone())?;
    if covered.iter().any(|c| c.negated) {
        return None;
    }
    let start = covered.first()?.parts.start;
    let end = covered.last()?.parts.end;
    let mut parts = root.must.get(start..end)?.to_vec();
    Some(match parts.len() {
        1 => parts.pop()?,
        _ => Query::Bool(BoolQuery {
            must: parts,
            ..BoolQuery::default()
        }),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests: inputs are fixed.

    use persia_analysis::AnalyzerRegistry;

    use super::*;
    use crate::schema::{FieldKind, SchemaBuilder, TextOptions};

    #[test]
    fn an_alternative_that_compiles_to_the_run_itself_is_left_out() {
        // Unreachable through `compile` (terms equal in every field are one term), so checked on its own.
        let schema = SchemaBuilder::new("c")
            .field("title", FieldKind::Text(TextOptions::default()))
            .synonyms(vec![vec!["tv".into(), "telly".into(), "Telly".into()]])
            .build()
            .unwrap();
        let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        let fields = DefaultFields::all(&schema);
        let scan = Scan::new(&compiler, &fields);
        let sources = BTreeSet::from([(0, 1), (0, 2)]);
        let telly = scan.alternatives(&Query::MatchAll, &sources, 50);
        // `Telly` compiles as `telly` does: once.
        assert_eq!(telly.len(), 1);
        assert_eq!(scan.alternatives(&telly[0], &sources, 50), []);
    }

    #[test]
    fn a_match_counts_only_over_whole_clauses() {
        // Clause 0 is one token, clause 1 two (`e-mail`), clause 2 one.
        let clause_of = [0, 1, 1, 2];
        assert_eq!(whole_clauses(&clause_of, 0..1), Some(0..1));
        assert_eq!(whole_clauses(&clause_of, 1..3), Some(1..2));
        assert_eq!(whole_clauses(&clause_of, 0..4), Some(0..3));
        assert_eq!(whole_clauses(&clause_of, 3..4), Some(2..3));
        // Part of `e-mail`, at either end.
        assert_eq!(whole_clauses(&clause_of, 1..2), None);
        assert_eq!(whole_clauses(&clause_of, 2..4), None);
        assert_eq!(whole_clauses(&clause_of, 0..2), None);
        // Out of range or empty.
        assert_eq!(whole_clauses(&clause_of, 4..5), None);
        assert_eq!(whole_clauses(&clause_of, 2..2), None);
    }
}
