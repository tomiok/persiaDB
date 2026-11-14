//! L4 `fuzzy` (SPEC §7.6, ROADMAP 8.2.4): the query with its words also matching the terms within a few edits.
//!
//! One step, listed as `fuzzy`, built from L0's query: every term of at least [`MIN_LEN`] characters becomes a `fuzzy`
//! query of [`DEFAULT_MAX_EXPANSIONS`] terms at the distance its length allows ([`distance`]), which planning expands
//! over the views' terms (each segment's FST intersected with the automaton, SPEC §7.1) and keeps by collection `df`.
//! The terms rewritten are L3's ([`super::words`]): words of the user text, in `text` fields whose analyzer does not
//! stack tokens. Excluded clauses, phrases and `field:` clauses stay exact, as they ask for their text exactly; so do
//! a stacking field's terms, whose grams would add their scores together.

use super::words::Words;
use super::{LevelInput, LevelTransform, Step};
use crate::query::{Compiled, DEFAULT_MAX_EXPANSIONS, FuzzyQuery, Query};
use crate::schema::FieldId;
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
        Ok(query
            .map(|query| Step {
                query,
                applied: "fuzzy".to_owned(),
                effective: input.compiled.map(Compiled::effective),
                alternatives: None,
            })
            .into_iter()
            .collect())
    }
}

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
