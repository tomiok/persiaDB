//! Queries: the programmatic AST, the compiler of user text into it, the filter DSL and the limits every query is
//! checked against (SPEC §7.1, ROADMAP 7.1).
//!
//! A [`Query`] built by hand is checked with [`Query::validate`]; [`QueryCompiler::compile`] turns user text into a
//! query that always validates and never fails (SPEC §7.1.1), and [`Filter::from_json`] parses and validates the
//! filter DSL (SPEC §7.1.2).
//!
//! ```
//! use persia_analysis::AnalyzerRegistry;
//! use persia_engine::query::{DefaultFields, Filter, Query, QueryCompiler, QueryLimits};
//! use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
//!
//! let schema = SchemaBuilder::new("products")
//!     .field("title", FieldKind::Text(TextOptions { positions: true, ..TextOptions::default() }))
//!     .field("brand", FieldKind::Keyword(KeywordOptions::default()))
//!     .field("price", FieldKind::F64 { stored: true })
//!     .build()?;
//! let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())?;
//! let limits = QueryLimits::default();
//!
//! // Default AND, a quoted phrase, an exclusion and a field; unbalanced quotes are plain text.
//! let compiled = compiler.compile(r#"red "running shoes" -kids brand:nike "#, &DefaultFields::all(&schema), &limits);
//! assert!(!compiled.truncated);
//! let Query::Bool(query) = &compiled.query else { unreachable!() };
//! assert_eq!((query.must.len(), query.must_not.len()), (3, 1));
//! compiled.query.validate(&schema, &limits)?;
//!
//! let filter = Filter::from_json(r#"{"and": [{"term": ["brand", "nike"]}, {"range": ["price", {"lte": 100}]}]}"#, &schema, &limits)?;
//! assert!(matches!(filter, Filter::And(ref all) if all.len() == 2));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod ast;
mod compile;
mod filter;
mod limits;

use crate::schema::{FieldId, FieldType};

pub use ast::{
    BoolQuery, BoostQuery, CombinedQuery, ConstantScoreQuery, FuzzyQuery, PhraseQuery, PhraseTerm,
    PrefixQuery, Query, TermQuery, WeightedTerm,
};
pub use compile::{Compiled, DefaultFields, QueryCompiler};
pub use filter::{Filter, FilterValue, MAX_FILTER_JSON_LEN};
pub use limits::{DEFAULT_MAX_EXPANSIONS, QueryLimits};

/// Why a query, a filter or the options of a compilation are invalid. User text itself never fails to compile.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// A field name the schema does not have.
    #[error("unknown field {field:?}")]
    UnknownField {
        /// The name.
        field: String,
    },
    /// A field id the schema does not have.
    #[error("unknown field id {field}")]
    UnknownFieldId {
        /// The id.
        field: FieldId,
    },
    /// A query or filter on a field of a type it does not apply to.
    #[error("{what} does not apply to field {field:?} of type {field_type}")]
    WrongFieldType {
        /// The field.
        field: String,
        /// Its type.
        field_type: FieldType,
        /// The query or filter (`"a phrase"`, `"a range filter"`, ...).
        what: &'static str,
    },
    /// A phrase on a `text` field that does not index positions.
    #[error("field {field:?} does not index positions, so it cannot match a phrase")]
    NoPositions {
        /// The field.
        field: String,
    },
    /// A term longer than [`crate::segment::MAX_TERM_LEN`]: no document can hold it.
    #[error(
        "field {field:?}: a term of {len} bytes, at most {}",
        crate::segment::MAX_TERM_LEN
    )]
    TermTooLong {
        /// The field.
        field: String,
        /// The term's length in bytes.
        len: usize,
    },
    /// An empty prefix, which would expand to every term of the field.
    #[error("field {field:?}: empty prefix")]
    EmptyPrefix {
        /// The field.
        field: String,
    },
    /// A phrase without terms, or whose positions decrease.
    #[error("field {field:?}: a phrase needs at least one term, in non-decreasing positions")]
    InvalidPhrase {
        /// The field.
        field: String,
    },
    /// A combined query without fields.
    #[error("a combined query needs at least one field")]
    EmptyCombined,
    /// A field named twice where each may appear once (a combined query, `fields`).
    #[error("field {field:?} given twice")]
    DuplicateField {
        /// The field.
        field: String,
    },
    /// A boost or field weight that is not finite or outside `0 < value ≤ 1000` (SPEC §6.2.2).
    #[error("boost {value} must be finite, above 0 and at most 1000")]
    InvalidBoost {
        /// The value.
        value: f64,
    },
    /// A constant score that is not finite or below 0.
    #[error("constant score {value} must be finite and at least 0")]
    InvalidScore {
        /// The value.
        value: f64,
    },
    /// A fuzzy edit distance other than 1 or 2 (SPEC §7.6, L4).
    #[error("edit distance {distance} must be 1 or 2")]
    InvalidDistance {
        /// The distance.
        distance: u8,
    },
    /// A prefix or fuzzy query that may expand to no term (`max_expansions` 0).
    #[error("max_expansions must be at least 1")]
    NoExpansions,
    /// More leaf terms than [`QueryLimits::max_terms`].
    #[error("more than {limit} terms")]
    TooManyTerms {
        /// The limit.
        limit: usize,
    },
    /// More query and filter nodes than [`QueryLimits::max_clauses`].
    #[error("more than {limit} clauses")]
    TooManyClauses {
        /// The limit.
        limit: usize,
    },
    /// Prefix and fuzzy queries that may expand to more terms, together, than [`QueryLimits::max_expansions`].
    #[error("more than {limit} expansions")]
    TooManyExpansions {
        /// The limit.
        limit: usize,
    },
    /// Nesting deeper than [`QueryLimits::max_depth`].
    #[error("nested deeper than {limit} levels")]
    TooDeep {
        /// The limit.
        limit: usize,
    },
    /// A `text` field whose analyzer the registry lacks.
    #[error("field {field:?}: unknown analyzer {analyzer}")]
    UnknownAnalyzer {
        /// The field.
        field: String,
        /// The analyzer (`name@version`).
        analyzer: String,
    },
    /// A malformed entry of `fields` (`"name"` or `"name^boost"`, SPEC §7.1), or an empty list.
    #[error("invalid fields entry {spec:?}")]
    InvalidFieldSpec {
        /// The entry (empty for an empty list).
        spec: String,
    },
    /// Filter JSON that is not valid JSON or not of the shape of SPEC §7.1.2.
    #[error("filter: {0}")]
    FilterSyntax(String),
    /// A filter value of the wrong type for its field, or outside the field's range.
    #[error("filter on field {field:?}: expected {expected}, found {found}")]
    FilterValue {
        /// The field.
        field: String,
        /// What the field takes.
        expected: &'static str,
        /// What was given.
        found: String,
    },
    /// A range filter without bounds, or a bound of another type than the field.
    #[error("range filter on field {field:?}: {problem}")]
    InvalidRange {
        /// The field.
        field: String,
        /// What is wrong.
        problem: &'static str,
    },
}
