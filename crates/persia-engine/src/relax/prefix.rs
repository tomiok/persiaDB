//! L3 `prefix` (SPEC §7.6, ROADMAP 8.2.3): the query with its words also matching the terms they start.
//!
//! One step, listed as `prefix`, built from L0's query (after a tier step `prefix:last` that expands the last term
//! alone, when L2 did not take its matches first, ROADMAP 8.2.7): the last term, and every other term of at least
//! [`MIN_LEN`] characters, becomes a `prefix` query of [`DEFAULT_MAX_EXPANSIONS`] terms, which planning expands over
//! the views' terms (an FST range scan per segment, SPEC §7.1) and keeps by collection `df`. Only words of the user
//! text are expanded: excluded clauses, phrases and `field:` clauses are not, as they ask for their text exactly. Nor
//! are the terms of a field whose analyzer stacks tokens (`edge_ngram`, SPEC §6.3.1): its index already holds every
//! prefix of a word, so the term matches the documents a prefix would, and a prefix would only add the stacked
//! grams' scores together.

use super::words::Words;
use super::{LevelInput, LevelTransform, Step};
use crate::query::{Compiled, DEFAULT_MAX_EXPANSIONS, PrefixQuery, Query};
use crate::schema::FieldId;
use crate::segment::ReadError;

/// The fewest characters (Unicode scalar values) of an analyzed term that L3 expands when it is not the last term
/// (SPEC §7.6): a shorter prefix would match too many terms to say much. The last term is expanded whatever its
/// length: it is the word a user may still be typing.
pub(crate) const MIN_LEN: usize = 3;

/// L3 `prefix` (SPEC §7.6, ROADMAP 8.2.3, ADR-0040).
///
/// A term is a field's analyzed term of a word: one per field of a `combined` (a word over the default fields), or a
/// `term` of a programmatic query. A term of a `text` field whose analyzer does not stack tokens is expanded when it is
/// the last term or has at least [`MIN_LEN`] characters. The last term is the last part of the last required clause
/// of the text, when that clause is a word (of a word the fields split apart, each field's last token), or the last
/// `must` clause of a programmatic root `bool` (a programmatic `term` or `combined` alone is its own last term).
///
/// Expanded, a `combined` becomes a `bool` of `should` clauses: the fields it keeps exact, as one `combined`, and a
/// `prefix` per expanded field. A `prefix` scores with its field's schema `boost` as the field weight (SPEC §7.2), as a
/// `term` does, so it is boosted only by the ratio of a `fields` weight to that boost, when they differ. So a document matches the word when it holds the term
/// in an exact field or a term starting with it in an expanded one, and each expanded field scores its own prefix
/// (SPEC §7.1: the kept terms share the largest `df`). A step that expands nothing is not built.
#[derive(Debug)]
pub(crate) struct Prefix;

impl LevelTransform for Prefix {
    /// The tier step that expands the last term alone (`prefix:last`, ROADMAP 8.2.7, ADR-0046), when another term is
    /// expanded too and L2's one-term rung was not built before it, then the step that expands every term it may
    /// (`prefix`). After that rung the tier step would find nothing: its matches hold the other words as typed, so the
    /// rung took them. A word the analyzer splits (`spider-ma`) is one clause, which L2 builds no step for, so its tier
    /// step is built under the default levels too.
    fn steps(&self, input: &LevelInput<'_>) -> Result<Vec<Step>, ReadError> {
        let Some(full) = step(input, false, "prefix") else {
            return Ok(Vec::new());
        };
        if input.drops_terms {
            return Ok(vec![full]);
        }
        match step(input, true, "prefix:last") {
            Some(last) if last.query != full.query => Ok(vec![last, full]),
            _ => Ok(vec![full]),
        }
    }
}

/// L3's step listed as `applied` (SPEC §7.6): expanding the last term alone (`last_only`), or every term it may. `None`
/// when it expands no term.
fn step(input: &LevelInput<'_>, last_only: bool, applied: &str) -> Option<Step> {
    let leaf = |field: FieldId, term: &str, last: bool| {
        (last || !last_only && term.chars().count() >= MIN_LEN).then(|| prefix(field, term))
    };
    let words = Words {
        expands: input.expands,
        leaf: &leaf,
    };
    let query = match input.compiled {
        Some(compiled) => words.of_compiled(compiled),
        None => words.of_query(input.original),
    }?;
    Some(Step {
        query,
        applied: applied.to_owned(),
        effective: input.compiled.map(Compiled::effective),
        alternatives: None,
        corrected: Vec::new(),
    })
}

fn prefix(field: FieldId, term: &str) -> Query {
    Query::Prefix(PrefixQuery {
        field,
        prefix: term.to_owned(),
        max_expansions: DEFAULT_MAX_EXPANSIONS,
    })
}

#[cfg(test)]
mod tests;
