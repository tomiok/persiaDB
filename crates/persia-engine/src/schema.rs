//! Collection schemas: field types and options, ranking and relaxation settings, validation, the strict JSON form,
//! the schema hash and evolution rules (SPEC §6.2, ADR-0022).
//!
//! A [`CollectionSchema`] is valid by construction: it is only built by [`SchemaBuilder::build`],
//! [`CollectionSchema::from_json`] (or serde) and [`CollectionSchema::evolve`], which all validate.
//!
//! ```
//! use persia_engine::schema::{CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
//!
//! let schema = SchemaBuilder::new("products")
//!     .field("title", FieldKind::Text(TextOptions { boost: 3.0, ..TextOptions::default() }))
//!     .field("brand", FieldKind::Keyword(KeywordOptions { facet: true, ..KeywordOptions::default() }))
//!     .build()?;
//! // Ids follow the names' byte order, not the order they were added in.
//! assert_eq!(schema.field("brand").map(|f| f.id().get()), Some(0));
//!
//! let same = CollectionSchema::from_json(&schema.to_json()?)?;
//! assert_eq!(same, schema);
//! assert_eq!(same.hash(), schema.hash());
//! # Ok::<(), persia_engine::schema::SchemaError>(())
//! ```

mod canonical;
mod config;
mod evolution;
mod field;
mod json;
mod validate;

pub(crate) use validate::field_name_problem;

use std::collections::BTreeMap;
use std::fmt;

pub use canonical::SchemaHash;
pub use config::{
    Fallback, FilterRelaxation, Level, Levels, RankFn, RankingConfig, RelaxationConfig, StaticRank,
};
pub use evolution::EvolutionError;
pub use field::{
    AnalyzerRef, BUILTIN_ANALYZERS, BuiltinAnalyzer, DEFAULT_ANALYZER, FieldKind, FieldType,
    KeywordOptions, TextOptions,
};

/// Most fields a collection may declare (SPEC §6.2.1).
pub const MAX_FIELDS: usize = 1024;
/// Longest collection or field name, in bytes (SPEC §6.2.1).
pub const MAX_NAME_LEN: usize = 64;
/// Most synonym groups per collection (SPEC §6.2.1).
pub const MAX_SYNONYM_GROUPS: usize = 10_000;
/// Most terms in one synonym group (SPEC §6.2.1).
pub const MAX_SYNONYM_GROUP_LEN: usize = 64;
/// Longest synonym term, in bytes (SPEC §6.2.1).
pub const MAX_SYNONYM_TERM_LEN: usize = 256;
/// Most bytes of synonym terms per collection, all groups together (SPEC §6.2.1).
pub const MAX_SYNONYM_BYTES: usize = 1 << 20;
/// Longest JSON definition [`CollectionSchema::from_json`] reads, in bytes (SPEC §6.2.1).
pub const MAX_SCHEMA_JSON_LEN: usize = 4 << 20;

/// The number segments use for a field (SPEC §6.2.1). Ids of a schema are exactly `0..field_count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldId(u32);

impl FieldId {
    /// The id as a number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for FieldId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One field of a [`CollectionSchema`].
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    id: FieldId,
    name: String,
    kind: FieldKind,
}

impl Field {
    /// The field's id.
    pub fn id(&self) -> FieldId {
        self.id
    }

    /// The field's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The field's type and options.
    pub fn kind(&self) -> &FieldKind {
        &self.kind
    }

    /// The field's type.
    pub fn field_type(&self) -> FieldType {
        self.kind.field_type()
    }
}

/// A validated collection schema (SPEC §6.2).
#[derive(Debug, Clone, PartialEq)]
pub struct CollectionSchema {
    name: String,
    /// Indexed by field id.
    fields: Vec<Field>,
    by_name: BTreeMap<String, FieldId>,
    ranking: RankingConfig,
    relaxation: RelaxationConfig,
    synonyms: Vec<Vec<String>>,
    strict: bool,
}

impl CollectionSchema {
    /// Parses and validates a JSON collection definition (SPEC §6.2), rejecting unknown and duplicate keys.
    ///
    /// The entry point for untrusted input: it rejects text over [`MAX_SCHEMA_JSON_LEN`] before parsing. The serde
    /// `Deserialize` impl is for trusted, already-bounded sources: it cannot see the input size, and a
    /// `serde_json::Value` has already merged duplicate keys.
    ///
    /// # Errors
    /// [`SchemaError::Json`] for malformed or non-conforming JSON, another variant for a rule of SPEC §6.2.1–§6.2.3.
    pub fn from_json(json: &str) -> Result<Self, SchemaError> {
        json::from_json(json)
    }

    /// [`Self::from_json`] for a schema the engine stored (SPEC §8.7): every option must be given.
    pub(crate) fn from_stored_json(json: &str) -> Result<Self, SchemaError> {
        json::from_stored_json(json)
    }

    /// The schema as JSON with every option written out, ids and analyzer versions included (ADR-0022).
    ///
    /// # Errors
    /// [`SchemaError::Json`] if serialization fails, which a valid schema does not cause.
    pub fn to_json(&self) -> Result<String, SchemaError> {
        json::to_json(self)
    }

    /// The collection's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Fields in id order.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// The field named `name`.
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.by_name.get(name).and_then(|id| self.field_by_id(*id))
    }

    /// The field with id `id`.
    pub fn field_by_id(&self, id: FieldId) -> Option<&Field> {
        self.fields.get(usize::try_from(id.get()).ok()?)
    }

    /// Ranking settings.
    pub fn ranking(&self) -> &RankingConfig {
        &self.ranking
    }

    /// Default relaxation settings for queries on this collection.
    pub fn relaxation(&self) -> &RelaxationConfig {
        &self.relaxation
    }

    /// Synonym groups, as declared.
    pub fn synonyms(&self) -> &[Vec<String>] {
        &self.synonyms
    }

    /// Whether documents with fields outside the schema are rejected (SPEC §6.4).
    pub fn strict(&self) -> bool {
        self.strict
    }

    /// The schema hash segments record (SPEC §6.2.5).
    pub fn hash(&self) -> SchemaHash {
        canonical::hash(&self.name, &self.fields)
    }

    /// The hash of this schema as it was when it had its first `field_count` fields, or `None` if it has fewer.
    ///
    /// Evolution only appends fields and never changes what the hash covers (SPEC §6.2.4), so this is the hash
    /// of every earlier version: a segment indexed before fields were added verifies against the current schema
    /// and the field count it records (SPEC §6.2.5).
    pub fn hash_at(&self, field_count: usize) -> Option<SchemaHash> {
        Some(canonical::hash(&self.name, self.fields.get(..field_count)?))
    }

    /// The canonical encoding of the index definition that [`Self::hash`] covers (SPEC §6.2.5).
    pub fn canonical_index_definition(&self) -> Vec<u8> {
        canonical::encode(&self.name, &self.fields)
    }

    /// Assembles a schema from parts that already satisfy every rule; ids must be the positions.
    fn assemble(parts: Parts, fields: Vec<Field>) -> Self {
        let by_name = fields.iter().map(|f| (f.name.clone(), f.id)).collect();
        Self {
            name: parts.name,
            fields,
            by_name,
            ranking: parts.ranking,
            relaxation: parts.relaxation,
            synonyms: parts.synonyms,
            strict: parts.strict,
        }
    }
}

/// The collection-level settings of a schema, before validation.
#[derive(Debug, Clone, PartialEq)]
struct Parts {
    name: String,
    ranking: RankingConfig,
    relaxation: RelaxationConfig,
    synonyms: Vec<Vec<String>>,
    strict: bool,
}

/// Builds a [`CollectionSchema`] in code; field ids are assigned from the names (SPEC §6.2.1).
#[derive(Debug, Clone)]
pub struct SchemaBuilder {
    parts: Parts,
    fields: Vec<(String, FieldKind)>,
}

impl SchemaBuilder {
    /// A builder for collection `name` with no fields and default settings.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            parts: Parts {
                name: name.into(),
                ranking: RankingConfig::default(),
                relaxation: RelaxationConfig::default(),
                synonyms: Vec::new(),
                strict: true,
            },
            fields: Vec::new(),
        }
    }

    /// Adds a field.
    #[must_use]
    pub fn field(mut self, name: impl Into<String>, kind: FieldKind) -> Self {
        self.fields.push((name.into(), kind));
        self
    }

    /// Sets the ranking settings.
    #[must_use]
    pub fn ranking(mut self, ranking: RankingConfig) -> Self {
        self.parts.ranking = ranking;
        self
    }

    /// Sets the default relaxation settings.
    #[must_use]
    pub fn relaxation(mut self, relaxation: RelaxationConfig) -> Self {
        self.parts.relaxation = relaxation;
        self
    }

    /// Sets the synonym groups.
    #[must_use]
    pub fn synonyms(mut self, synonyms: Vec<Vec<String>>) -> Self {
        self.parts.synonyms = synonyms;
        self
    }

    /// Sets whether unknown document fields are rejected (default `true`, SPEC §6.4).
    #[must_use]
    pub fn strict(mut self, strict: bool) -> Self {
        self.parts.strict = strict;
        self
    }

    /// Validates and builds the schema.
    ///
    /// # Errors
    /// The first rule of SPEC §6.2.1–§6.2.3 the definition breaks.
    pub fn build(self) -> Result<CollectionSchema, SchemaError> {
        validate::build(
            self.parts,
            self.fields.into_iter().map(|(n, k)| (n, None, k)),
        )
    }
}

/// Why a collection or field name is invalid (SPEC §6.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameProblem {
    /// The name is empty.
    #[error("empty")]
    Empty,
    /// The name is longer than [`MAX_NAME_LEN`] bytes.
    #[error("{len} bytes, at most {MAX_NAME_LEN}")]
    TooLong {
        /// Its length in bytes.
        len: usize,
    },
    /// The name has a character outside the allowed set, or starts with one not allowed first.
    #[error("must match {pattern}")]
    InvalidChars {
        /// The pattern names must match.
        pattern: &'static str,
    },
    /// The name is reserved for the engine.
    #[error("reserved (`id`, `blob` and names starting with `_`)")]
    Reserved,
}

/// A collection definition that breaks a rule of SPEC §6.2.1–§6.2.3.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SchemaError {
    /// Malformed JSON, a wrong value type, an unknown or duplicate key, or a missing required key.
    #[error("invalid schema JSON: {0}")]
    Json(String),
    /// The collection name breaks the naming rule.
    #[error("invalid collection name {name:?}: {problem}")]
    CollectionName {
        /// The name given.
        name: String,
        /// What is wrong with it.
        problem: NameProblem,
    },
    /// A field name breaks the naming rule.
    #[error("invalid field name {name:?}: {problem}")]
    FieldName {
        /// The name given.
        name: String,
        /// What is wrong with it.
        problem: NameProblem,
    },
    /// Two fields have the same name.
    #[error("field {name:?} is declared twice")]
    DuplicateField {
        /// The repeated name.
        name: String,
    },
    /// More than [`MAX_FIELDS`] fields.
    #[error("{count} fields, at most {MAX_FIELDS}")]
    TooManyFields {
        /// How many were declared.
        count: usize,
    },
    /// Some fields have an `id` and others do not, or the ids are not exactly `0..field_count`.
    #[error("field ids must be absent or exactly 0..{field_count}: {problem}")]
    FieldIds {
        /// How many fields there are.
        field_count: usize,
        /// What is wrong.
        problem: String,
    },
    /// A numeric option outside its range, or not finite.
    #[error("{option} = {value} is outside {allowed}")]
    OutOfRange {
        /// Where the option is, e.g. `fields.title.boost`.
        option: String,
        /// The value given.
        value: String,
        /// The valid range.
        allowed: &'static str,
    },
    /// A text field names an analyzer that is not built in, or a version of it that does not exist.
    #[error("field {field:?}: unknown analyzer {analyzer:?}")]
    UnknownAnalyzer {
        /// The field.
        field: String,
        /// The analyzer as given.
        analyzer: String,
    },
    /// `ranking.static_rank.field` is not a field of the schema, or not an `i64`, `f64` or `datetime` field.
    #[error("ranking.static_rank.field {field:?}: {problem}")]
    StaticRankField {
        /// The field named.
        field: String,
        /// What is wrong.
        problem: &'static str,
    },
    /// `relaxation.levels` repeats a level or lists them out of cascade order.
    #[error("relaxation.levels: {problem}")]
    Levels {
        /// What is wrong.
        problem: &'static str,
    },
    /// A synonym group breaks a limit.
    #[error("synonyms[{group}]: {problem}")]
    Synonyms {
        /// Index of the group (or the group count, for too many groups).
        group: usize,
        /// What is wrong.
        problem: String,
    },
}
