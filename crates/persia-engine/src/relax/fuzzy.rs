//! L4 `fuzzy` (SPEC §7.6, ROADMAP 8.2.4): the query with its words also matching the terms within a few edits.
//!
//! One step, listed as `fuzzy`, built from L0's query: every term of at least [`MIN_LEN`] characters becomes a `fuzzy`
//! query of [`DEFAULT_MAX_EXPANSIONS`] terms at the distance its length allows ([`distance`]), which planning expands
//! over the views' terms (each segment's FST intersected with the automaton, SPEC §7.1) and keeps by collection `df`.
//! The terms rewritten are L3's ([`super::words`]): words of the user text, in `text` fields whose analyzer does not
//! stack tokens. Excluded clauses, phrases and `field:` clauses stay exact, as they ask for their text exactly; so do
//! a stacking field's terms, whose grams would add their scores together.

use core::ops::Bound;

use super::words::Words;
use super::{Correction, LevelInput, LevelTransform, Step};
use crate::query::{ClauseKind, Compiled, DEFAULT_MAX_EXPANSIONS, FuzzyQuery, Query};
use crate::schema::FieldId;
use crate::search::{Expansion, expand};
use crate::segment::ReadError;

/// The fewest characters (Unicode scalar values of the analyzed term) L4 matches fuzzily (SPEC §7.6): a shorter term
/// is one edit from too many others to say much.
pub(crate) const MIN_LEN: usize = 4;

/// The fewest characters of a term L4 matches within two edits; shorter ones from [`MIN_LEN`] get one (SPEC §7.6).
pub(crate) const TWO_EDITS_LEN: usize = 8;

/// The edit distance L4 matches `term` within (SPEC §7.6): none below [`MIN_LEN`] characters, 1 up to 7, and 2 from
/// [`TWO_EDITS_LEN`]. Characters are Unicode scalar values of the analyzed term, as L3 counts them: `café` is 4.
pub(crate) fn distance(term: &str) -> Option<u8> {
    match term.chars().take(TWO_EDITS_LEN).count() {
        TWO_EDITS_LEN => Some(2),
        n if n >= MIN_LEN => Some(1),
        _ => None,
    }
}

/// L4 `fuzzy` (SPEC §7.6, ROADMAP 8.2.4, ADR-0042).
///
/// A term is a field's analyzed term of a word, as at L3 ([`super::words`]). One of a `text` field whose analyzer does
/// not stack tokens, and of at least [`MIN_LEN`] characters, becomes a `fuzzy` of [`DEFAULT_MAX_EXPANSIONS`] at its
/// [`distance`]; the query's other clauses, exclusions and filters stay as they are. A rewritten `combined` becomes a
/// `bool` of `should` clauses, its exact fields as one `combined` and a `fuzzy` per rewritten field, each boosted by
/// its `fields` weight's ratio to the schema boost when they differ. A step that rewrites nothing is not built.
#[derive(Debug)]
pub(crate) struct Fuzzy;

impl LevelTransform for Fuzzy {
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        let leaf = |field: FieldId, term: &str, _last: bool| {
            distance(term).map(|distance| fuzzy(field, term, distance))
        };
        let words = Words {
            expands: input.expands,
            leaf: &leaf,
        };
        let query = match input.compiled {
            Some(compiled) => words.of_compiled(compiled),
            None => words.of_query(input.original),
        };
        let Some(query) = query else {
            return Ok(Vec::new());
        };
        let (effective, corrected) = match input.compiled {
            Some(compiled) => {
                let found = corrections(compiled, input)?;
                let text = effective(compiled, &found);
                (Some(text), found.into_iter().map(|(_, c)| c).collect())
            }
            None => (None, Vec::new()),
        };
        Ok(vec![Step {
            query,
            applied: "fuzzy".to_owned(),
            effective,
            alternatives: None,
            corrected,
        }])
    }
}

/// The corrections of `compiled`'s words, each with its clause's index, in text order (SPEC §7.6 `corrected`, ROADMAP
/// 8.2.4.3, ADR-0042).
///
/// A word is corrected when it is a required word clause of one part (a `term`, or a `combined` over the default
/// fields: one token), no field of it holds its term (collection `df` 0, so no document does), and some field L4
/// rewrites has a term within the word's distance that a document holds. Its correction is the term a field's
/// `fuzzy` keeps first (SPEC §7.1: the highest collection `df`, ties by bytes), the highest `df` over the fields, ties
/// to the earlier field: it is searched by the step under any cut (§7.1.3 keeps the most frequent). Each such field
/// is walked as planning walks it ([`expand`], keeping one term). Without the views' terms
/// ([`LevelInput::terms`]), nothing is corrected.
///
/// # Errors
/// A `df` or a term walk that cannot be read; an interrupt of the walk.
fn corrections(
    compiled: &Compiled,
    input: &LevelInput<'_>,
) -> Result<Vec<(usize, Correction)>, ReadError> {
    let Some(terms) = input.terms else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (index, clause) in compiled.clauses.iter().enumerate() {
        if clause.negated || clause.kind != ClauseKind::Word {
            continue;
        }
        let Some([part]) = compiled.parts(index) else {
            continue;
        };
        let fields: Vec<(FieldId, &str)> = match part {
            Query::Term(t) => vec![(t.field, t.term.as_str())],
            Query::Combined(c) => c
                .fields
                .iter()
                .map(|f| (f.field, f.term.as_str()))
                .collect(),
            _ => continue,
        };
        // A word a document holds matched as typed: L4 widens it, but does not report it replaced.
        let mut held = false;
        for &(field, term) in &fields {
            if input.stats.df(field, term.as_bytes())? > 0 {
                held = true;
                break;
            }
        }
        if held {
            continue;
        }
        let mut best: Option<(u64, Vec<u8>)> = None;
        for &(field, term) in &fields {
            if (input.expands)(field).is_none() || term.is_empty() {
                continue;
            }
            let Some(d) = distance(term) else {
                continue;
            };
            let automaton = Expansion::fuzzy(term, d);
            let all = (Bound::Unbounded, Bound::Unbounded);
            let kept = expand(field, all, &automaton, 1, terms, input.stats)?;
            if let Some((found, df)) = kept.into_iter().next()
                && df > 0
                && best.as_ref().is_none_or(|(top, _)| df > *top)
            {
                best = Some((df, found));
            }
        }
        // Index terms of `text` fields are UTF-8; one that is not is not reported.
        if let Some((_, found)) = best
            && let Ok(corrected) = String::from_utf8(found)
            && let Some(original) = compiled.source(index)
        {
            out.push((
                index,
                Correction {
                    original: original.to_owned(),
                    corrected,
                },
            ));
        }
    }
    Ok(out)
}

/// L0's text with each corrected word replaced by its correction (SPEC §7.6 `effective`): every clause, excluded ones
/// included, in text order, one space apart, as [`Compiled::effective`] renders them.
fn effective(compiled: &Compiled, corrections: &[(usize, Correction)]) -> String {
    let mut text = String::new();
    for index in 0..compiled.clauses.len() {
        if !text.is_empty() {
            text.push(' ');
        }
        match corrections.iter().find(|(i, _)| *i == index) {
            Some((_, c)) => text.push_str(&c.corrected),
            None => text.push_str(compiled.source(index).unwrap_or("")),
        }
    }
    text
}

/// A `fuzzy` of `term` at `distance`, keeping [`DEFAULT_MAX_EXPANSIONS`].
fn fuzzy(field: FieldId, term: &str, distance: u8) -> Query {
    Query::Fuzzy(FuzzyQuery {
        field,
        term: term.to_owned(),
        distance,
        max_expansions: DEFAULT_MAX_EXPANSIONS,
    })
}

#[cfg(test)]
mod tests;
