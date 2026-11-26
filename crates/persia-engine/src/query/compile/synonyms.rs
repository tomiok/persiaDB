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
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

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
///     .field("title", FieldKind::Text(TextOptions::default().positions(true)))
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
    /// Whether the compilation's bound on what L1's step could hold (SPEC §6.3.2) left out some of them: the cascade
    /// then lists the step as cut (`synonyms:cut`, §7.1.3), whether or not it cuts more.
    pub cut: bool,
}

/// How many sets of default fields keep their compiled alternatives ([`AlternativeCache`]): a request that names its own
/// `fields` (SPEC §7.1) compiles over another set, and most requests use a few.
const CACHED_FIELD_SETS: usize = 4;

/// One synonym term's cached alternative: `None` within when it compiles to nothing. Boxed, so a slot not yet filled
/// costs a pointer and its `Once`, not a whole [`Query`] (ADR-0058, Memory).
type Slot = OnceLock<Option<Box<Query>>>;

/// The collection's synonym terms compiled as alternatives (the quoted phrase `"<term>"`, [`CompiledSynonym::alternatives`])
/// over one set of default fields, each the first time a compilation needs it (ROADMAP 8.1.16, ADR-0058). An
/// alternative depends only on its term, the fields and their analyzers, not on the query or the limits, so a run
/// that matches again reuses what an earlier compilation analyzed.
///
/// A group's slots are allocated the first time one of its terms is asked for: a new set costs one empty cell per
/// group, not one slot per term (SPEC §6.2.1 allows 10 000 groups of 64 terms), so a client that names new `fields`
/// with every request makes each compilation allocate what its own runs use.
#[derive(Debug)]
struct Alternatives {
    fields: DefaultFields,
    /// By group: its terms' slots, in term order, once a term of the group was asked for.
    groups: Box<[OnceLock<Box<[Slot]>>]>,
}

impl Alternatives {
    /// An empty set over `fields` for `groups` groups.
    fn new(groups: usize, fields: DefaultFields) -> Self {
        Self {
            fields,
            groups: (0..groups).map(|_| OnceLock::new()).collect(),
        }
    }

    /// The alternative of term `index` of group `group` of `groups` (the compiler's schema's, the same at every call),
    /// compiled by `compile` the first time it is asked for. `None` when the term compiles to nothing, or there is no
    /// such term.
    fn get(
        &self,
        groups: &[Vec<String>],
        group: usize,
        index: usize,
        compile: impl FnOnce() -> Option<Query>,
    ) -> Option<&Query> {
        let len = groups.get(group)?.len();
        if index >= len {
            return None;
        }
        let slots = self
            .groups
            .get(group)?
            .get_or_init(|| (0..len).map(|_| OnceLock::new()).collect());
        slots
            .get(index)?
            .get_or_init(|| compile().map(Box::new))
            .as_deref()
    }

    /// How many groups have their slots allocated, and how many slots they hold.
    #[cfg(test)]
    fn allocated(&self) -> (usize, usize) {
        let filled = self.groups.iter().filter_map(OnceLock::get);
        filled.fold((0, 0), |(groups, slots), s| (groups + 1, slots + s.len()))
    }
}

/// A compiler's [`Alternatives`] for the last [`CACHED_FIELD_SETS`] sets of default fields it compiled runs over, the
/// least recently used one dropped first. A set dropped while a compilation still holds it lives until that ends.
#[derive(Debug, Default)]
pub(super) struct AlternativeCache {
    /// Least recently used first.
    sets: Mutex<Vec<Arc<Alternatives>>>,
}

impl AlternativeCache {
    /// The alternatives over `fields` of the terms of `groups` (the compiler's schema's, the same at every call).
    fn of(&self, groups: &[Vec<String>], fields: &DefaultFields) -> Arc<Alternatives> {
        // A compilation that panicked holding the lock left the list whole: each step below keeps it consistent.
        let lock = || self.sets.lock().unwrap_or_else(PoisonError::into_inner);
        let used = |sets: &mut Vec<Arc<Alternatives>>| {
            let at = sets.iter().position(|s| s.fields == *fields)?;
            let set = sets.remove(at);
            sets.push(Arc::clone(&set));
            Some(set)
        };
        if let Some(set) = used(&mut lock()) {
            return set;
        }
        // A new set is built unlocked, so a miss does not hold up the compilations that hit.
        let new = Arc::new(Alternatives::new(groups.len(), fields.clone()));
        let mut sets = lock();
        // Another compilation may have added the same set meanwhile: keep one, so both fill it.
        if let Some(set) = used(&mut sets) {
            return set;
        }
        if sets.len() >= CACHED_FIELD_SETS {
            sets.remove(0);
        }
        sets.push(Arc::clone(&new));
        new
    }
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
    /// share as [`share`] cuts (the longest lists first, at least one each), and one each when that leaves less than
    /// one per run: such a step never fits, and the cascade lists it skipped. A run that keeps none is dropped; its
    /// share goes to no other run. A hostile text therefore costs the compilation no more than the limits allow
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
            // The step cannot hold one alternative each: keep one, so the cascade lists it skipped (§7.1.3).
            counts.fill(1);
        }
        accepted
            .into_iter()
            .zip(counts)
            .filter_map(|((range, sources), count)| {
                let original = original(root, clauses, &range)?;
                let limit = usize::try_from(count).unwrap_or(usize::MAX);
                let (alternatives, cut) = self.alternatives(&original, &sources, limit);
                (!alternatives.is_empty()).then_some(CompiledSynonym {
                    clauses: range,
                    alternatives,
                    cut,
                })
            })
            .collect()
    }

    /// The raw terms at `sources` (group, index) compiled as quoted phrases over the default fields, in source order,
    /// each once and none equal to `original`, at most `limit`; and whether `limit` left one out that the
    /// 50-cap would have kept.
    fn alternatives(
        &self,
        original: &Query,
        sources: &BTreeSet<(usize, usize)>,
        limit: usize,
    ) -> (Vec<Query>, bool) {
        let groups = self.compiler.schema.synonyms();
        let cached = self.compiler.alternatives.of(groups, self.fields);
        let mut tokens = TokenStream::new();
        let mut alternatives: Vec<Query> = Vec::new();
        // Below the cap, one more source is compiled to tell a cut from duplicates that compile away.
        let probe = limit < DEFAULT_SYNONYM_EXPANSIONS;
        for &(group, index) in sources {
            let full = alternatives.len() >= limit;
            if full && !probe {
                break;
            }
            let Some(text) = groups.get(group).and_then(|g| g.get(index)) else {
                continue;
            };
            let Some(query) = cached.get(groups, group, index, || self.compile(text, &mut tokens))
            else {
                continue;
            };
            if query != original && !alternatives.contains(query) {
                if full {
                    return (alternatives, true);
                }
                alternatives.push(query.clone());
            }
        }
        (alternatives, false)
    }

    /// `text` compiled as the quoted phrase `"<text>"` over the default fields: `None` when it analyzes to nothing.
    fn compile(&self, text: &str, tokens: &mut TokenStream) -> Option<Query> {
        let phrase = Clause {
            negated: false,
            field: None,
            phrase: true,
            text,
        };
        let (mut parts, _, _) = self.compiler.clause(phrase, self.fields, tokens, false);
        match parts.len() {
            0 => None,
            1 => parts.pop(),
            _ => Some(Query::Bool(BoolQuery {
                must: parts,
                ..BoolQuery::default()
            })),
        }
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
        // Unreachable through `compile`, for any schema and analyzer (ROADMAP 8.1.26), so checked on its own: (a) a
        // run's own query is built of `combined` leaves (`word()`); (b) an alternative of several tokens in some field
        // compiles there through `text_query`, to a `term`, a `phrase` or a `bool` of required `term`s, never to a
        // `combined`; (c) one of at most one token per field compiles as a word, equal to the run's query only when
        // its token is the run's in every field, and then each field's `SynonymMap` reads it as the run's own term
        // (the same term id), so `alternative_sources` never lists it.
        let schema = SchemaBuilder::new("c")
            .field("title", FieldKind::Text(TextOptions::default()))
            .synonyms(vec![vec!["tv".into(), "telly".into(), "Telly".into()]])
            .build()
            .unwrap();
        let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        let fields = DefaultFields::all(&schema);
        let scan = Scan::new(&compiler, &fields);
        let sources = BTreeSet::from([(0, 1), (0, 2)]);
        let (telly, cut) = scan.alternatives(&Query::MatchAll, &sources, 50);
        // `Telly` compiles as `telly` does: once.
        assert_eq!((telly.len(), cut), (1, false));
        assert_eq!(scan.alternatives(&telly[0], &sources, 50), (vec![], false));
        // Held to one, the duplicate left out is no cut.
        assert_eq!(
            scan.alternatives(&Query::MatchAll, &sources, 1),
            (telly, false)
        );
    }

    #[test]
    fn a_limit_below_the_cap_that_leaves_one_out_is_a_cut() {
        let schema = SchemaBuilder::new("c")
            .field("title", FieldKind::Text(TextOptions::default()))
            .synonyms(vec![vec![
                "tv".into(),
                "telly".into(),
                "Telly".into(),
                "box".into(),
            ]])
            .build()
            .unwrap();
        let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        let fields = DefaultFields::all(&schema);
        let scan = Scan::new(&compiler, &fields);
        let sources = BTreeSet::from([(0, 1), (0, 2), (0, 3)]);
        let count = |limit| {
            let (alternatives, cut) = scan.alternatives(&Query::MatchAll, &sources, limit);
            (alternatives.len(), cut)
        };
        assert_eq!(count(1), (1, true));
        assert_eq!(count(2), (2, false));
        assert_eq!(count(50), (2, false));
    }

    #[test]
    fn the_50_cap_over_sources_two_fields_joined_is_no_cut() {
        // Fields that match the same clauses join their sources, which may pass the 50-cap.
        let mut group: Vec<String> = vec!["tv".into()];
        group.extend((0..59).map(|i| format!("w{i}")));
        let schema = SchemaBuilder::new("c")
            .field("title", FieldKind::Text(TextOptions::default()))
            .synonyms(vec![group])
            .build()
            .unwrap();
        let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        let fields = DefaultFields::all(&schema);
        let scan = Scan::new(&compiler, &fields);
        let sources: BTreeSet<(usize, usize)> = (1..60).map(|i| (0, i)).collect();
        let count = |limit| {
            let (alternatives, cut) = scan.alternatives(&Query::MatchAll, &sources, limit);
            (alternatives.len(), cut)
        };
        assert_eq!(count(DEFAULT_SYNONYM_EXPANSIONS), (50, false));
        assert_eq!(count(49), (49, true));
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

    /// `title` (positions) and `body` (no positions, `en`), with groups whose terms are one word, a phrase, a term
    /// that compiles as another does (`Telly`), and the same term in two groups.
    fn cached_schema() -> crate::schema::CollectionSchema {
        SchemaBuilder::new("c")
            .field(
                "title",
                FieldKind::Text(TextOptions {
                    positions: true,
                    ..TextOptions::default()
                }),
            )
            .field(
                "body",
                FieldKind::Text(TextOptions {
                    analyzer: crate::schema::AnalyzerRef::builtin("en").unwrap(),
                    ..TextOptions::default()
                }),
            )
            .synonyms(vec![
                vec![
                    "tv".into(),
                    "telly".into(),
                    "Telly".into(),
                    "television set".into(),
                ],
                vec!["tv".into(), "the box".into()],
                vec!["running shoes".into(), "trainers".into()],
            ])
            .build()
            .unwrap()
    }

    /// ROADMAP 8.1.16: a compiler reuses the alternatives it compiled, per set of default fields, and compiles what a
    /// fresh compiler does: for every set of fields (weights and order included) and every limit, again and again.
    #[test]
    fn cached_alternatives_compile_as_a_fresh_compiler_does() {
        let schema = cached_schema();
        let registry = AnalyzerRegistry::builtin();
        let compiler = QueryCompiler::new(&schema, &registry).unwrap();
        let sets = [
            DefaultFields::all(&schema),
            DefaultFields::parse(&schema, &["title^3", "body"]).unwrap(),
            DefaultFields::parse(&schema, &["body", "title^3"]).unwrap(),
            DefaultFields::parse(&schema, &["title"]).unwrap(),
            DefaultFields::parse(&schema, &["body^0.5"]).unwrap(),
        ];
        let limits = [
            QueryLimits::default(),
            QueryLimits {
                max_terms: 9,
                ..QueryLimits::default()
            },
        ];
        let texts = [
            "cheap tv",
            "tv running shoes",
            "TV -box",
            "telly trainers tv",
        ];
        for round in 0..3 {
            for fields in &sets {
                for limits in &limits {
                    for text in texts {
                        let fresh = QueryCompiler::new(&schema, &registry).unwrap();
                        let want = fresh.compile(text, fields, limits);
                        let got = compiler.compile(text, fields, limits);
                        assert_eq!(got, want, "round {round}: {text} over {fields:?}");
                    }
                }
            }
        }
        // The weights reach the alternatives: `telly` is one token per field, a word over the fields' weights.
        let alternatives = |fields: &DefaultFields| {
            compiler
                .compile("tv", fields, &QueryLimits::default())
                .synonyms[0]
                .alternatives
                .clone()
        };
        assert_ne!(alternatives(&sets[0]), alternatives(&sets[1]));
    }

    /// ROADMAP 8.1.16: the cache keeps the last [`CACHED_FIELD_SETS`] sets of default fields, the least recently used
    /// one dropped first, and a set compiled again after it was dropped compiles as before.
    #[test]
    fn the_cache_keeps_the_last_sets_of_default_fields() {
        let schema = cached_schema();
        let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        let cached = || -> Vec<DefaultFields> {
            let sets = compiler.alternatives.sets.lock().unwrap();
            sets.iter().map(|s| s.fields.clone()).collect()
        };
        let weighted = |w: usize| DefaultFields::parse(&schema, &[format!("title^{w}")]).unwrap();
        let compile =
            |fields: &DefaultFields| compiler.compile("tv", fields, &QueryLimits::default());
        // A text without a run compiles no alternative, and caches nothing.
        let _ = compiler.compile("radio", &weighted(1), &QueryLimits::default());
        assert_eq!(cached(), []);
        let first = compile(&weighted(1));
        for w in 2..=CACHED_FIELD_SETS {
            compile(&weighted(w));
        }
        assert_eq!(
            cached(),
            (1..=CACHED_FIELD_SETS).map(weighted).collect::<Vec<_>>()
        );
        // Used again, the first set is the most recent; one more set drops the second.
        assert_eq!(compile(&weighted(1)), first);
        compile(&weighted(9));
        let mut want: Vec<DefaultFields> = (3..=CACHED_FIELD_SETS).map(weighted).collect();
        want.extend([weighted(1), weighted(9)]);
        assert_eq!(cached(), want);
        // Dropped, the second set compiles again as it did.
        let fresh = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        assert_eq!(
            compile(&weighted(2)),
            fresh.compile("tv", &weighted(2), &QueryLimits::default())
        );
    }

    /// A term is looked up within its group: an index past the group's end is no term of the next group.
    #[test]
    fn a_cached_term_is_looked_up_within_its_group() {
        let schema = cached_schema();
        let groups = schema.synonyms();
        let alternatives = Alternatives::new(groups.len(), DefaultFields::all(&schema));
        let term = |group, index| {
            alternatives
                .get(groups, group, index, || Some(Query::MatchAll))
                .cloned()
        };
        assert_eq!(term(1, 1), Some(Query::MatchAll));
        // Group 1 has 2 terms: index 2 is not group 2's first.
        assert_eq!(term(1, 2), None);
        assert_eq!(term(3, 0), None);
        assert_eq!(term(usize::MAX, 0), None);
        assert_eq!(term(0, usize::MAX), None);
        // Compiled once: a later call does not compile again.
        assert_eq!(
            alternatives.get(groups, 1, 1, || panic!("compiled twice")),
            Some(&Query::MatchAll)
        );
        // A term that compiles to nothing stays nothing.
        assert_eq!(alternatives.get(groups, 2, 0, || None), None);
        assert_eq!(
            alternatives.get(groups, 2, 0, || Some(Query::MatchAll)),
            None
        );
    }

    /// ROADMAP 8.1.16: a new set of default fields allocates no slot per term up front, only the slots of the groups
    /// its runs draw alternatives from, so a client naming new `fields` with every request costs each compilation
    /// what its own runs use, not every term of a large schema (SPEC §6.2.1: up to 10 000 groups of 64 terms).
    #[test]
    fn a_new_set_of_default_fields_allocates_only_the_groups_its_runs_use() {
        const GROUPS: usize = 2_000;
        const TERMS: usize = 32;
        let groups: Vec<Vec<String>> = (0..GROUPS)
            .map(|g| (0..TERMS).map(|t| format!("g{g}t{t}")).collect())
            .collect();
        let schema = SchemaBuilder::new("c")
            .field("title", FieldKind::Text(TextOptions::default()))
            .synonyms(groups)
            .build()
            .unwrap();
        let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        let filled = || -> Vec<(usize, usize)> {
            let sets = compiler.alternatives.sets.lock().unwrap();
            sets.iter().map(|s| s.allocated()).collect()
        };
        let limits = QueryLimits::default();
        // Two runs, of groups 7 and 1 999: their 2 groups' slots, 64 of the schema's 64 000 terms, in each set.
        for w in 1..=3 {
            let fields = DefaultFields::parse(&schema, &[format!("title^{w}")]).unwrap();
            let out = compiler.compile("g7t0 g1999t5", &fields, &limits);
            assert_eq!(out.synonyms.len(), 2);
            let cached = compiler.alternatives.sets.lock().unwrap();
            assert_eq!(cached.last().unwrap().groups.len(), GROUPS);
        }
        assert_eq!(filled(), [(2, 2 * TERMS); 3]);
        // A run of another group fills its group's slots in that set alone.
        let fields = DefaultFields::parse(&schema, &["title^3"]).unwrap();
        compiler.compile("g42t1", &fields, &limits);
        assert_eq!(filled(), [(2, 2 * TERMS), (2, 2 * TERMS), (3, 3 * TERMS)]);
    }

    /// Compilations on several threads share one cache, and each compiles what a fresh compiler does.
    #[test]
    fn threads_share_the_cache() {
        let schema = cached_schema();
        let registry = AnalyzerRegistry::builtin();
        let compiler = QueryCompiler::new(&schema, &registry).unwrap();
        let sets = [
            DefaultFields::all(&schema),
            DefaultFields::parse(&schema, &["title^2"]).unwrap(),
        ];
        let texts = ["cheap tv", "tv running shoes", "telly trainers tv"];
        let limits = QueryLimits::default();
        // Each by a compiler of its own, which compiles it alone.
        let want: Vec<Vec<_>> = sets
            .iter()
            .map(|fields| {
                let texts = texts.iter();
                let compile = |text: &&str| {
                    let fresh = QueryCompiler::new(&schema, &registry).unwrap();
                    fresh.compile(text, fields, &limits)
                };
                texts.map(compile).collect()
            })
            .collect();
        // Started together, two threads miss on each set at once: the re-check after the re-lock keeps one.
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|s| {
            for t in 0..4 {
                let (compiler, sets, want, limits, start) =
                    (&compiler, &sets, &want, &limits, &start);
                s.spawn(move || {
                    start.wait();
                    for i in 0..50 {
                        let set = (t + i) % sets.len();
                        let text = i % texts.len();
                        let got = compiler.compile(texts[text], &sets[set], limits);
                        assert_eq!(got, want[set][text]);
                    }
                });
            }
        });
        // One cached set per set of default fields, however the misses raced.
        let cached: Vec<DefaultFields> = {
            let held = compiler.alternatives.sets.lock().unwrap();
            held.iter().map(|s| s.fields.clone()).collect()
        };
        assert_eq!(cached.len(), sets.len(), "{cached:?}");
        assert!(sets.iter().all(|f| cached.contains(f)), "{cached:?}");
    }
}
