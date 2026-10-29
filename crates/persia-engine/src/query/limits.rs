//! Query limits (SPEC §7.1.3): how large a query or filter may be.

use super::QueryError;

/// The `max_expansions` of the prefix and fuzzy queries the compiler and the relaxation cascade build (SPEC §7.6).
pub const DEFAULT_MAX_EXPANSIONS: u32 = 50;

/// The most a query may hold (SPEC §7.1.3). A programmatic query or a filter over a limit is rejected with an
/// explicit [`QueryError`]; user text is cut instead ([`super::Compiled::truncated`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_field_names)] // `max_` reads as the limit it is.
pub struct QueryLimits {
    /// Most leaf terms: one per term, prefix and fuzzy query, per field of a combined query, per phrase term, and
    /// per value of a filter (a `term` value, each `terms` value, each range bound).
    pub max_terms: usize,
    /// Most nodes: queries and filters, at every level.
    pub max_clauses: usize,
    /// Most terms the prefix and fuzzy queries may expand to, together (the sum of their `max_expansions`).
    pub max_expansions: usize,
    /// Deepest nesting: a leaf is 1, a node is one more than its deepest child.
    pub max_depth: usize,
    /// Longest user text [`super::QueryCompiler::compile`] reads, in bytes; the rest is left out.
    pub max_text_len: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_terms: 1024,
            max_clauses: 1024,
            max_expansions: 1024,
            max_depth: 32,
            max_text_len: 8 << 10,
        }
    }
}

/// What a query or filter holds, counted against [`QueryLimits`] as it is walked.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Usage {
    pub(super) terms: usize,
    pub(super) clauses: usize,
    pub(super) expansions: usize,
}

impl Usage {
    /// Counts one node with `terms` leaf terms and `expansions` expansions at `depth` (1 for the root).
    pub(super) fn add(
        &mut self,
        limits: &QueryLimits,
        depth: usize,
        terms: usize,
        expansions: usize,
    ) -> Result<(), QueryError> {
        if depth > limits.max_depth {
            return Err(QueryError::TooDeep {
                limit: limits.max_depth,
            });
        }
        self.clauses = self.clauses.saturating_add(1);
        if self.clauses > limits.max_clauses {
            return Err(QueryError::TooManyClauses {
                limit: limits.max_clauses,
            });
        }
        self.terms = self.terms.saturating_add(terms);
        if self.terms > limits.max_terms {
            return Err(QueryError::TooManyTerms {
                limit: limits.max_terms,
            });
        }
        self.expansions = self.expansions.saturating_add(expansions);
        if self.expansions > limits.max_expansions {
            return Err(QueryError::TooManyExpansions {
                limit: limits.max_expansions,
            });
        }
        Ok(())
    }
}
