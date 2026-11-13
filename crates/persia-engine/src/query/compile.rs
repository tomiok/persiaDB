//! User text → [`Query`] (SPEC §7.1.1): analysis per field, default AND, quoted phrases, `-` exclusion and
//! `field:` scoping. Never fails: syntax that does not parse is read as plain words.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use persia_analysis::{Analyzer, AnalyzerRegistry, QueryAnalysis, SynonymMap, TokenStream};

use crate::schema::{CollectionSchema, FieldId, FieldKind, SchemaHash};
use crate::segment::MAX_TERM_LEN;

use super::QueryError;
use super::ast::{
    BoolQuery, CombinedQuery, PhraseQuery, PhraseTerm, Query, TermQuery, WeightedTerm,
    boost_in_range, searchable,
};
use super::filter::Filter;
use super::limits::{QueryLimits, Usage};

mod synonyms;

pub use synonyms::CompiledSynonym;
use synonyms::{Captured, Scan};

/// The `text` fields plain words and phrases search, with their BM25F weights (the `fields` of SPEC §7.1).
///
/// Built for one schema ([`Self::all`], [`Self::parse`]). A field id is a position in that schema, so
/// [`QueryCompiler::compile`] debug-asserts the schema hash: fields built for another schema are a programming
/// error, not something to apply silently.
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultFields {
    fields: Vec<(FieldId, f64)>,
    /// [`CollectionSchema::hash`] of the schema `fields` were taken from.
    schema: SchemaHash,
}

impl DefaultFields {
    /// Every `text` field of `schema`, weighted by its `boost`: the default of SPEC §7.1.
    pub fn all(schema: &CollectionSchema) -> Self {
        Self::new(
            schema,
            schema
                .fields()
                .iter()
                .filter_map(|f| match f.kind() {
                    FieldKind::Text(o) => Some((f.id(), o.boost)),
                    _ => None,
                })
                .collect(),
        )
    }

    /// Parses `fields` entries, `"name"` (weighted by the schema's `boost`) or `"name^weight"`.
    ///
    /// ```
    /// use persia_engine::query::DefaultFields;
    /// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    ///
    /// let schema = SchemaBuilder::new("c")
    ///     .field("body", FieldKind::Text(TextOptions::default()))
    ///     .field("title", FieldKind::Text(TextOptions::default()))
    ///     .build()?;
    /// let fields = DefaultFields::parse(&schema, &["title^3", "body"])?;
    /// let weights: Vec<f64> = fields.fields().iter().map(|&(_, w)| w).collect();
    /// assert_eq!(weights, [3.0, 1.0]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`QueryError::InvalidFieldSpec`] for an empty list or a weight that is not a number,
    /// [`QueryError::UnknownField`], [`QueryError::WrongFieldType`] for a field that is not `text`,
    /// [`QueryError::DuplicateField`], [`QueryError::InvalidBoost`].
    pub fn parse<S: AsRef<str>>(
        schema: &CollectionSchema,
        specs: &[S],
    ) -> Result<Self, QueryError> {
        if specs.is_empty() {
            return Err(QueryError::InvalidFieldSpec {
                spec: String::new(),
            });
        }
        let mut fields: Vec<(FieldId, f64)> = Vec::with_capacity(specs.len());
        for spec in specs {
            let spec = spec.as_ref();
            let (name, weight) = match spec.split_once('^') {
                Some((name, weight)) => (
                    name,
                    Some(
                        weight
                            .parse::<f64>()
                            .map_err(|_| QueryError::InvalidFieldSpec {
                                spec: spec.to_owned(),
                            })?,
                    ),
                ),
                None => (spec, None),
            };
            let field = schema.field(name).ok_or_else(|| QueryError::UnknownField {
                field: name.to_owned(),
            })?;
            let FieldKind::Text(options) = field.kind() else {
                return Err(QueryError::WrongFieldType {
                    field: name.to_owned(),
                    field_type: field.field_type(),
                    what: "a default field",
                });
            };
            if fields.iter().any(|&(id, _)| id == field.id()) {
                return Err(QueryError::DuplicateField {
                    field: name.to_owned(),
                });
            }
            let weight = weight.unwrap_or(options.boost);
            boost_in_range(weight)?;
            fields.push((field.id(), weight));
        }
        Ok(Self::new(schema, fields))
    }

    /// The fields and their weights, in the order given.
    pub fn fields(&self) -> &[(FieldId, f64)] {
        &self.fields
    }

    fn new(schema: &CollectionSchema, fields: Vec<(FieldId, f64)>) -> Self {
        Self {
            fields,
            schema: schema.hash(),
        }
    }
}

/// How a one-part compilation's query holds its part ([`Compiled::parts`]).
enum OnePart<'a> {
    /// The query is the part.
    Itself,
    /// The query is this `bool`, requiring the part.
    Wrapped(&'a BoolQuery),
}

/// The result of [`QueryCompiler::compile`]: the query, and where in the user text each of its clauses came from
/// (ROADMAP 8.1.7), for the relaxation response's `original` and `effective` (SPEC §7.6).
///
/// ```
/// use persia_analysis::AnalyzerRegistry;
/// use persia_engine::query::{DefaultFields, QueryCompiler, QueryLimits};
/// use persia_engine::schema::{AnalyzerRef, FieldKind, SchemaBuilder, TextOptions};
///
/// let en = TextOptions { analyzer: AnalyzerRef::builtin("en").ok_or("no en")?, ..TextOptions::default() };
/// let schema = SchemaBuilder::new("c").field("title", FieldKind::Text(en)).build()?;
/// let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())?;
/// let compiled = compiler.compile("red  the -\"running shoes\"", &DefaultFields::all(&schema), &QueryLimits::default());
/// // `the` is a stopword: it compiles to nothing, so it is not a clause.
/// let clauses: Vec<&str> = (0..compiled.clauses.len()).filter_map(|i| compiled.source(i)).collect();
/// assert_eq!(clauses, ["red", "-\"running shoes\""]);
/// assert_eq!(compiled.clauses[1].span, 9..25);
/// assert!(compiled.clauses[1].negated);
/// assert_eq!(compiled.effective(), "red -\"running shoes\"");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Compiled {
    /// The query.
    pub query: Query,
    /// Whether part of the text was left out to stay within the [`QueryLimits`].
    pub truncated: bool,
    /// The user text as read: all of it, or the prefix [`QueryLimits::max_text_len`] keeps
    /// ([`text_within_limit`]). The response's `original` (SPEC §7.6).
    pub text: String,
    /// The clauses that compiled to part of [`Self::query`], in text order. A clause that analyzes to no token, or
    /// that the limits left out, is not one.
    pub clauses: Vec<CompiledClause>,
    /// The limits it was compiled with: a relaxed search holds its steps to them unless its plan sets others
    /// ([`crate::relax::RelaxPlan::with_limits`], SPEC §7.1.3).
    pub limits: QueryLimits,
    /// The runs of required words that are a term of the collection's synonym groups, in text order, each with what it
    /// expands to (SPEC §6.3.2, §7.6 L1). L0 does not search them: the relaxation cascade's L1 `synonyms` ORs each run
    /// with its alternatives. Empty when the collection has no synonyms or no run matches.
    pub synonyms: Vec<CompiledSynonym>,
    /// The one required part a compilation of one is (SPEC §7.1.1: that part is the query), kept apart from
    /// [`Self::query`] so that joining a filter to the query (ROADMAP 8.1.17, [`Self::with_filter`]) leaves the clause its
    /// part. `None` for any other compilation, whose parts are entries of the root `bool`.
    part: Option<Query>,
}

/// A clause of the user text that compiled to part of a [`Compiled::query`] (SPEC §7.1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompiledClause {
    /// Its bytes in [`Compiled::text`], and so in the user text, of which that is a prefix: from its `-` or
    /// `field:` to its last character (a phrase's closing quote), without the whitespace around it. Always on
    /// character boundaries.
    pub span: Range<usize>,
    /// Whether it excludes (`-`): its parts are the root's `must_not`, otherwise its `must`.
    pub negated: bool,
    /// Its parts, as indexes into the root's `must` (or `must_not` when [`Self::negated`]); [`Compiled::parts`]
    /// reads them. A word that splits into several spans has several parts; an excluded clause has one.
    pub parts: Range<usize>,
    /// Whether it is a stopword of a field it searches (SPEC §7.6, L2 drops such terms first): a default field analyzes
    /// it to no token because its analyzer's stopword filter removed one (the language analyzers `en`, `es`, `pt`,
    /// `fr`, `de`, `it` have one, after normalizing: `The`, `the,`, `ｔｈｅ`). Text the tokenizer finds no word in
    /// (`—`) is not one, whatever the field.
    /// Another field keeps it in the clause's parts: `the` over a `standard` title and an `en` body is a stopword. A
    /// `field:` clause is never one: the field it names would leave nothing to compile.
    pub stopword: bool,
    /// Its form in the user text (SPEC §7.1.1): a word, a `"phrase"`, or either after `field:`. The relaxation
    /// cascade's L3 `prefix` expands only words (SPEC §7.6): a phrase and a `field:` clause ask for that text exactly.
    pub kind: ClauseKind,
}

/// The form of a clause of the user text (SPEC §7.1.1), whether excluded (`-`) or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClauseKind {
    /// A word, over the default fields.
    Word,
    /// A `"phrase"` over the default fields, even one of a single token (compiled as a word).
    Phrase,
    /// A word or phrase after `field:`, in that field only.
    Field,
}

impl ClauseKind {
    fn of(clause: &Clause<'_>) -> Self {
        match (clause.field, clause.phrase) {
            (Some(_), _) => Self::Field,
            (None, true) => Self::Phrase,
            (None, false) => Self::Word,
        }
    }
}

impl Compiled {
    /// The text of clause `index` of [`Self::clauses`], as the user wrote it: its [`CompiledClause::span`] of
    /// [`Self::text`]. `None` past the last clause. It takes an index, not a [`CompiledClause`], so a clause of
    /// another compilation cannot be read against this text.
    pub fn source(&self, index: usize) -> Option<&str> {
        self.text.get(self.clauses.get(index)?.span.clone())
    }

    /// The text of the query as compiled (SPEC §7.6 `effective` at L0): its clauses' text, separated by one space.
    /// What was left out (stopwords, text past a limit) is not in it, so recompiling it gives the same query.
    pub fn effective(&self) -> String {
        let mut effective = String::new();
        for index in 0..self.clauses.len() {
            if !effective.is_empty() {
                effective.push(' ');
            }
            effective.push_str(self.source(index).unwrap_or(""));
        }
        effective
    }

    /// The queries clause `index` of [`Self::clauses`] compiled to: entries of the root `bool`'s `must` (or
    /// `must_not`), or, for a compilation of one required part, that part (SPEC §7.1.1: a single required part is the
    /// query itself, until a filter joins it, ROADMAP 8.1.17). `None` past the last clause, and for a one-part
    /// compilation whose [`Self::query`] was replaced by one that is neither the part nor a `bool` requiring it (its
    /// clauses no longer describe it). Like [`Self::source`], it takes an index so that it reads only this compilation's
    /// clauses.
    pub fn parts(&self, index: usize) -> Option<&[Query]> {
        let clause = self.clauses.get(index)?;
        if let Some(part) = &self.part {
            self.one_part()?;
            return (clause.parts == (0..1) && !clause.negated).then(|| std::slice::from_ref(part));
        }
        let root = self.root()?;
        let list = if clause.negated {
            &root.must_not
        } else {
            &root.must
        };
        list.get(clause.parts.clone())
    }

    /// The `bool` at the root of [`Self::query`] around the clauses' parts: its `must` and `must_not` hold them, and its
    /// `filter` a request's filter (SPEC §7.1, [`Self::with_filter`]). `None` when the query is not a `bool` around
    /// them: no clause compiled (`match_all`), the query is the one required part, joined to nothing, or it was
    /// replaced by one that does not hold that part.
    pub fn root(&self) -> Option<&BoolQuery> {
        match (&self.part, &self.query) {
            (Some(_), _) => match self.one_part()? {
                OnePart::Itself => None,
                OnePart::Wrapped(root) => Some(root),
            },
            (None, Query::Bool(root)) => Some(root),
            (None, _) => None,
        }
    }

    /// How [`Self::query`] holds a one-part compilation's part: as itself, or as the one `must` clause of a `bool` without
    /// `should` clauses (a request's filter, or an exclusion a caller added, beside it). `None` for anything else, and
    /// for a compilation of other than one part.
    fn one_part(&self) -> Option<OnePart<'_>> {
        let part = self.part.as_ref()?;
        if *part == self.query {
            return Some(OnePart::Itself);
        }
        match &self.query {
            Query::Bool(root)
                if root.should.is_empty()
                    && root.minimum_should_match == 0
                    && root.must.as_slice() == std::slice::from_ref(part) =>
            {
                Some(OnePart::Wrapped(root))
            }
            _ => None,
        }
    }

    /// This compilation with `filter` joined as the filter clauses of its root `bool` (SPEC §7.1, §7.1.2; ROADMAP
    /// 8.1.17), as a request's `filter` joins `q`: added to the root's filters, or, when the query is not a `bool`
    /// around the clauses ([`Self::root`]), the query wrapped as `bool { must: [query], filter }`. A one-part query
    /// that is itself a `bool` is wrapped too: a filter beside its `should` clauses would make them optional. The
    /// clauses keep their parts ([`Self::parts`]), so the relaxation levels read them as before. No filter, no change.
    /// The joined query is not checked against [`Self::limits`]: the filter adds its nodes and terms, and a level of
    /// depth (`bool` around the clauses, or the wrap), so it may exceed limits the compiled query was within, as any
    /// filter joined to a root does. A request holds the joined query to its own limits (SPEC §7.1.3:
    /// [`Query::validate`]).
    ///
    /// ```
    /// use persia_analysis::AnalyzerRegistry;
    /// use persia_engine::query::{DefaultFields, Filter, FilterValue, Query, QueryCompiler, QueryLimits};
    /// use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
    ///
    /// let schema = SchemaBuilder::new("c")
    ///     .field("title", FieldKind::Text(TextOptions::default()))
    ///     .field("brand", FieldKind::Keyword(KeywordOptions::default()))
    ///     .build()?;
    /// let brand = schema.field("brand").ok_or("no field")?.id();
    /// let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())?;
    /// let compiled = compiler.compile("shoes", &DefaultFields::all(&schema), &QueryLimits::default());
    /// let shoes = compiled.query.clone();
    /// let nike = Filter::Term { field: brand, value: FilterValue::Keyword("nike".into()) };
    /// let filtered = compiled.with_filter([nike.clone()]);
    /// let root = filtered.root().ok_or("no root")?;
    /// assert_eq!((root.must.as_slice(), root.filter.as_slice()), (std::slice::from_ref(&shoes), std::slice::from_ref(&nike)));
    /// // The clause is still its one part, without the filter.
    /// assert_eq!(filtered.parts(0), Some(std::slice::from_ref(&shoes)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn with_filter(mut self, filter: impl IntoIterator<Item = Filter>) -> Self {
        let mut filter = filter.into_iter().peekable();
        if filter.peek().is_none() {
            return self;
        }
        if self.root().is_none() {
            let query = std::mem::replace(&mut self.query, Query::MatchAll);
            self.query = Query::Bool(BoolQuery {
                must: vec![query],
                ..BoolQuery::default()
            });
        }
        if let Query::Bool(root) = &mut self.query {
            root.filter.extend(filter);
        }
        self
    }
}

/// Compiles user text into queries over one collection (SPEC §7.1.1). Holds the analyzer of every `text` field.
pub struct QueryCompiler {
    schema: CollectionSchema,
    /// By field id; `None` for fields that are not `text`.
    analyzers: Vec<Option<Arc<Analyzer>>>,
    /// By field id: the collection's synonym groups analyzed by the field's query-time analysis (SPEC §6.3.2), built
    /// the first time a query searches the field by default. Unused for fields that are not `text`.
    synonyms: Vec<OnceLock<SynonymMap>>,
}

impl std::fmt::Debug for QueryCompiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryCompiler")
            .field("collection", &self.schema.name())
            .finish_non_exhaustive()
    }
}

/// One clause of the user text: `[-][field:](word | "phrase")`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Clause<'t> {
    negated: bool,
    field: Option<FieldId>,
    phrase: bool,
    text: &'t str,
}

impl QueryCompiler {
    /// A compiler for `schema`, analyzing `text` fields with the analyzers `registry` has for them. Build one per
    /// schema and reuse it: the first compilation that searches a field by default analyzes the collection's synonym
    /// groups for it (SPEC §6.3.2), up to 1 MiB of terms, and later ones reuse them.
    ///
    /// # Errors
    /// [`QueryError::UnknownAnalyzer`] if `registry` lacks the analyzer (name and version) of a `text` field.
    pub fn new(schema: &CollectionSchema, registry: &AnalyzerRegistry) -> Result<Self, QueryError> {
        let analyzers = schema
            .fields()
            .iter()
            .map(|field| match field.kind() {
                FieldKind::Text(options) => registry
                    .get(options.analyzer.name(), options.analyzer.version())
                    .map(|a| Some(Arc::clone(a)))
                    .ok_or_else(|| QueryError::UnknownAnalyzer {
                        field: field.name().to_owned(),
                        analyzer: options.analyzer.to_string(),
                    }),
                _ => Ok(None),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            schema: schema.clone(),
            synonyms: analyzers.iter().map(|_| OnceLock::new()).collect(),
            analyzers,
        })
    }

    /// The synonym groups of the schema as `text` field `id` analyzes them at query time; `None` for another field.
    fn synonym_map(&self, id: FieldId) -> Option<&SynonymMap> {
        let analyzer = self.analyzer(id)?;
        let index = usize::try_from(id.get()).ok()?;
        Some(
            self.synonyms
                .get(index)?
                .get_or_init(|| SynonymMap::analyzed(self.schema.synonyms(), analyzer)),
        )
    }

    /// Compiles `text` (SPEC §7.1.1). Clauses are separated by whitespace and all required (default AND):
    /// - a word is analyzed with each of `fields`' analyzers. When they all split it into the same spans, each
    ///   span becomes one [`Query::Combined`] over the fields (a word may give several, as `e-mail` does);
    ///   otherwise each field requires its own tokens, and one of the fields must match;
    /// - `"a phrase"` becomes, per field, a [`Query::Phrase`] (fields without positions: every term required),
    ///   one of which must match; a phrase of at most one token per field is a word. Phrases score with the
    ///   schema `boost`: the weights of `fields` apply to words;
    /// - `field:word` and `field:"a phrase"` search that `text` or `keyword` field only, with [`Query::Term`]s
    ///   (a `keyword` field takes the text as is);
    /// - `-` before a clause excludes the documents it matches.
    ///
    /// Nothing fails: a `field:` naming no `text` or `keyword` field, a `"` without its closing quote and a lone `-`
    /// are plain text. Clauses that analyze to no token (stopwords) are left out, as are tokens longer than
    /// [`MAX_TERM_LEN`] (no document holds one); without any clause left the query is [`Query::MatchAll`]. Text
    /// beyond [`QueryLimits::max_text_len`] is cut at a character boundary and then back to the last clause that
    /// lies entirely inside the cut, so a split word or a phrase whose closing quote was cut off is left out
    /// ([`text_within_limit`]). The clauses from the first one that would exceed `limits` are left out too, with
    /// [`Compiled::truncated`] set. A single required clause is that clause, checked at depth 1, so a term fits
    /// `max_depth` 1; the clause count still includes a root bool. The query then validates against the schema
    /// and `limits` ([`Query::validate`]) whenever [`Query::MatchAll`] does. `fields` must be built for this
    /// compiler's schema; debug builds assert that.
    pub fn compile(&self, text: &str, fields: &DefaultFields, limits: &QueryLimits) -> Compiled {
        // Field ids are positions in the schema that assigned them (SPEC §6.2.1). Another schema's fields
        // would search this one's by coincidence of id.
        debug_assert!(
            fields.schema == self.schema.hash(),
            "DefaultFields were built for a different schema than this compiler"
        );
        let (text, mut truncated) = cut(text, limits.max_text_len, &self.schema);
        let mut tokens = TokenStream::new();
        let mut query = BoolQuery::default();
        let mut usage = Usage::default();
        // Set when the only accepted clause was checked at depth 1, because it is the query. Adding another
        // clause puts it under a bool, one level deeper, so it has to be checked again.
        let mut shallow = false;
        // The root Bool; a query that holds one clause is that clause, so this only over-counts clauses.
        let mut clauses: Vec<CompiledClause> = Vec::new();
        if usage.add(limits, 1, 0, 0).is_err() {
            return finish(
                BoolQuery::default(),
                truncated || !text.trim().is_empty(),
                text,
                limits,
                clauses,
                Vec::new(),
            );
        }
        // The runs of required words L1 scans for synonyms (SPEC §6.3.2), when the collection has any.
        let mut scan = Scan::new(self, fields);
        for (clause, span) in clause_spans(text, &self.schema) {
            let eligible =
                scan.scans() && !clause.negated && clause.field.is_none() && !clause.phrase;
            let kind = ClauseKind::of(&clause);
            let (parts, stopword, captured) = self.clause(clause, fields, &mut tokens, eligible);
            let part = match (clause.negated, parts.len()) {
                (_, 0) => {
                    scan.dropped(eligible, &captured);
                    continue;
                }
                (false, _) | (true, 1) => parts,
                (true, _) => vec![Query::Bool(BoolQuery {
                    must: parts,
                    ..BoolQuery::default()
                })],
            };
            if shallow {
                let Some(promoted) = self.promote(&query, &part, limits) else {
                    truncated = true;
                    break;
                };
                shallow = false;
                usage = promoted;
            } else {
                let mut trial = usage;
                let fits = part
                    .iter()
                    .try_for_each(|q| q.check(&self.schema, limits, &mut trial, 2));
                if let Err(err) = fits {
                    // A single required clause is the query, so a depth failure under the bool is retried
                    // at depth 1 before giving up (SPEC §7.1.1).
                    let alone = query.must.is_empty()
                        && query.must_not.is_empty()
                        && !clause.negated
                        && part.len() == 1
                        && matches!(err, QueryError::TooDeep { .. });
                    if alone && let Some(only) = part.first() {
                        let mut retry = Usage::default();
                        if retry.add(limits, 1, 0, 0).is_ok()
                            && only.check(&self.schema, limits, &mut retry, 1).is_ok()
                        {
                            shallow = true;
                            usage = retry;
                            clauses.push(CompiledClause {
                                span,
                                negated: false,
                                parts: 0..part.len(),
                                stopword,
                                kind,
                            });
                            scan.clause(eligible, clauses.len().saturating_sub(1), captured);
                            query.must.extend(part);
                            continue;
                        }
                    }
                    truncated = true;
                    break;
                }
                usage = trial;
            }
            let list = if clause.negated {
                &mut query.must_not
            } else {
                &mut query.must
            };
            clauses.push(CompiledClause {
                span,
                negated: clause.negated,
                parts: list.len()..list.len().saturating_add(part.len()),
                stopword,
                kind,
            });
            scan.clause(eligible, clauses.len().saturating_sub(1), captured);
            list.extend(part);
        }
        let synonyms = scan.finish(&query, &clauses, limits, usage.terms);
        finish(query, truncated, text, limits, clauses, synonyms)
    }

    /// The usage of `query`'s one clause, checked at depth 1 as the whole query, promoted under a root bool with `part`
    /// beside it, or `None` when they do not fit `limits` together.
    fn promote(&self, query: &BoolQuery, part: &[Query], limits: &QueryLimits) -> Option<Usage> {
        let mut trial = Usage::default();
        trial.add(limits, 1, 0, 0).ok()?;
        query
            .must
            .iter()
            .chain(&query.must_not)
            .chain(part)
            .try_for_each(|q| q.check(&self.schema, limits, &mut trial, 2))
            .ok()?;
        Some(trial)
    }

    /// The queries a clause requires (all of them), or none when it analyzes to no token, whether it is a stopword
    /// ([`CompiledClause::stopword`]), and, when `capture` is set, each default field's tokens of it for the synonym
    /// scan (SPEC §6.3.2).
    fn clause(
        &self,
        clause: Clause<'_>,
        fields: &DefaultFields,
        tokens: &mut TokenStream,
        capture: bool,
    ) -> (Vec<Query>, bool, Captured) {
        let mut captured = Captured::default();
        if let Some(field) = clause.field {
            let parts = self.field_clause(field, clause.text, tokens);
            return (parts.into_iter().collect(), false, captured);
        }
        let mut stopword = false;
        let mut per_field: Vec<FieldTokens> = Vec::with_capacity(fields.fields().len());
        for &(id, weight) in fields.fields() {
            let Some((tokens, analysis)) = self.analyzed(id, clause.text, tokens) else {
                if capture {
                    captured.push(&[], 0);
                }
                continue;
            };
            if capture {
                captured.push(&tokens, analysis.positions);
            }
            if tokens.is_empty() {
                stopword |= analysis.stopword;
                continue;
            }
            per_field.push(FieldTokens { id, weight, tokens });
        }
        if !clause.phrase || per_field.iter().all(|f| f.tokens.len() <= 1) {
            return (word(per_field), stopword, captured);
        }
        let should: Vec<Query> = per_field
            .into_iter()
            .filter_map(|f| self.text_query(f.id, f.tokens))
            .collect();
        (one_of(should).into_iter().collect(), stopword, captured)
    }

    /// `text` in one field: a `keyword` field's term as is, or a `text` field's terms ([`Self::text_query`]).
    /// `None` for no term.
    fn field_clause(&self, id: FieldId, text: &str, tokens: &mut TokenStream) -> Option<Query> {
        match self.schema.field_by_id(id)?.kind() {
            FieldKind::Keyword(_) => (!text.is_empty() && fits(text)).then(|| {
                Query::Term(TermQuery {
                    field: id,
                    term: text.to_owned(),
                })
            }),
            FieldKind::Text(_) => {
                let (analyzed, _) = self.analyzed(id, text, tokens)?;
                self.text_query(id, analyzed)
            }
            _ => None,
        }
    }

    /// The query of a `text` field's tokens: one is a term; more are a phrase, or every term required in a field
    /// without positions. `None` for no token.
    fn text_query(&self, id: FieldId, mut analyzed: Vec<QueryToken>) -> Option<Query> {
        let term = |term: String| Query::Term(TermQuery { field: id, term });
        analyzed.sort_by_key(|t| t.position);
        let first = analyzed.first()?.position;
        if analyzed.len() == 1 {
            return analyzed.pop().map(|t| term(t.term));
        }
        let positions = matches!(
            self.schema.field_by_id(id)?.kind(),
            FieldKind::Text(o) if o.positions
        );
        Some(if positions {
            Query::Phrase(PhraseQuery {
                field: id,
                terms: analyzed
                    .into_iter()
                    .map(|t| PhraseTerm {
                        term: t.term,
                        position: t.position.saturating_sub(first),
                    })
                    .collect(),
            })
        } else {
            Query::Bool(BoolQuery {
                must: analyzed.into_iter().map(|t| term(t.term)).collect(),
                ..BoolQuery::default()
            })
        })
    }

    /// The query-time tokens of `text` in `text` field `id` that may be in the index, in text order, and what else
    /// the analysis reports (a stopword removed, the positions taken); `None` if it is not a `text` field.
    fn analyzed(
        &self,
        id: FieldId,
        text: &str,
        tokens: &mut TokenStream,
    ) -> Option<(Vec<QueryToken>, QueryAnalysis)> {
        let analysis = self.analyzer(id)?.analyze_query_counted(text, tokens);
        Some((
            tokens
                .iter()
                .filter(|t| fits(&t.text))
                .map(|t| QueryToken {
                    span: (t.start, t.end),
                    position: t.position,
                    term: t.text.clone(),
                })
                .collect(),
            analysis,
        ))
    }

    fn analyzer(&self, id: FieldId) -> Option<&Analyzer> {
        let index = usize::try_from(id.get()).ok()?;
        self.analyzers.get(index)?.as_deref()
    }
}

/// The compilation of `text` whose clauses built `root`: no clause is [`Query::MatchAll`], and a single required
/// part is the query itself (SPEC §7.1.1).
fn finish(
    mut root: BoolQuery,
    truncated: bool,
    text: &str,
    limits: &QueryLimits,
    clauses: Vec<CompiledClause>,
    synonyms: Vec<CompiledSynonym>,
) -> Compiled {
    let query = match (root.must.len(), root.must_not.len()) {
        (0, 0) => Query::MatchAll,
        (1, 0) => root.must.pop().unwrap_or(Query::MatchAll),
        _ => Query::Bool(root),
    };
    let one = clauses.iter().map(|c| c.parts.len()).sum::<usize>() == 1
        && clauses.iter().all(|c| !c.negated);
    Compiled {
        part: one.then(|| query.clone()),
        query,
        truncated,
        text: text.to_owned(),
        clauses,
        limits: *limits,
        synonyms,
    }
}

/// One query-time token of a word or phrase.
#[derive(Debug)]
struct QueryToken {
    span: (usize, usize),
    position: u32,
    term: String,
}

/// The tokens a default field gives a word or phrase, and the field's weight.
#[derive(Debug)]
struct FieldTokens {
    id: FieldId,
    weight: f64,
    tokens: Vec<QueryToken>,
}

/// A word over the default fields that have tokens for it (SPEC §7.1.1). When the fields agree on how to split it
/// (no two of their token spans overlap unless equal; a field may lack a span, as for a stopword), each span is one
/// required [`Query::Combined`] over the fields that have it (`e-mail`: two). Otherwise one field's split must not
/// add requirements on another, so each field requires its own tokens and one of the fields must match.
fn word(per_field: Vec<FieldTokens>) -> Vec<Query> {
    let combined = |fields: Vec<WeightedTerm>| Query::Combined(CombinedQuery { fields });
    if agree(&per_field) {
        let mut groups: BTreeMap<(usize, usize), Vec<WeightedTerm>> = BTreeMap::new();
        for f in per_field {
            for token in f.tokens {
                groups.entry(token.span).or_default().push(WeightedTerm {
                    field: f.id,
                    term: token.term,
                    weight: f.weight,
                });
            }
        }
        return groups.into_values().map(combined).collect();
    }
    let should = per_field.into_iter().map(|f| {
        let mut must: Vec<Query> = f
            .tokens
            .into_iter()
            .map(|t| {
                combined(vec![WeightedTerm {
                    field: f.id,
                    term: t.term,
                    weight: f.weight,
                }])
            })
            .collect();
        match must.len() {
            1 => must.pop().unwrap_or(Query::MatchAll),
            _ => Query::Bool(BoolQuery {
                must,
                ..BoolQuery::default()
            }),
        }
    });
    one_of(should.collect()).into_iter().collect()
}

/// Whether the fields split a word the same way: within a field spans are distinct (so a group holds a field once),
/// and across fields no two spans overlap unless they are equal. Linear after the sort.
fn agree(per_field: &[FieldTokens]) -> bool {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for f in per_field {
        let own: Vec<(usize, usize)> = f.tokens.iter().map(|t| t.span).collect();
        // SPEC §6.3.1: one analysis has non-decreasing span starts and ends (tokenizers are strictly
        // increasing; a char filter may map several tokens onto one span). Equal spans are then adjacent,
        // but a field still must not be grouped twice, so duplicates are rejected whatever their order.
        debug_assert!(
            own.windows(2).all(|w| match w {
                [a, b] => a.0 <= b.0 && a.1 <= b.1,
                _ => true,
            }),
            "SPEC §6.3.1: span starts and ends of one analysis do not decrease"
        );
        let mut ordered = own.clone();
        ordered.sort_unstable();
        if ordered.windows(2).any(|w| match w {
            [a, b] => a == b,
            _ => false,
        }) {
            return false;
        }
        spans.extend(own);
    }
    spans.sort_unstable();
    spans.dedup();
    spans.windows(2).all(|w| match w {
        [a, b] => a.1 <= b.0,
        _ => true,
    })
}

/// A query matching any of `alternatives`: itself when there is one, `None` when there is none.
fn one_of(mut alternatives: Vec<Query>) -> Option<Query> {
    match alternatives.len() {
        0 | 1 => alternatives.pop(),
        _ => Some(Query::Bool(BoolQuery {
            should: alternatives,
            ..BoolQuery::default()
        })),
    }
}

/// Whether a term may be in a term dictionary (SPEC §5.3.1).
fn fits(term: &str) -> bool {
    !term.is_empty() && term.len() <= MAX_TERM_LEN
}

/// `text` cut to at most `max` bytes without splitting a word or a phrase (SPEC §7.1.1), and whether it was cut.
///
/// The cut is a character boundary, then the last clause that is not wholly inside it is dropped: a word the cut
/// divides, and a phrase whose closing quote lies past it (that quote is what would have closed it; without the
/// quote the words become required terms). Only `text[..max]` is read, plus one character to see if a word goes on.
fn cut<'t>(text: &'t str, max: usize, schema: &CollectionSchema) -> (&'t str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let prefix = text.get(..end).unwrap_or("");
    let continues = text
        .get(end..)
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| !c.is_whitespace());
    let spans = clause_spans(prefix, schema);
    let mut keep = 0;
    for (clause, span) in &spans {
        // A closed phrase ends on its quote. A word that runs to the cap and into the next character was split.
        let split_word = continues && span.end == end && !clause.phrase;
        if split_word {
            break;
        }
        keep = span.end;
    }
    // A `"` opened inside the cap and never closed there: the closing quote was cut off (or the quoted text
    // runs past the cap). Drop from that clause, same as dropping the split word it contains.
    if let Some(open) = unclosed_quote(prefix, schema) {
        keep = spans
            .iter()
            .map(|(_, span)| span.end)
            .filter(|clause_end| *clause_end <= open)
            .max()
            .unwrap_or(0);
    }
    (text.get(..keep).unwrap_or(""), true)
}

/// The prefix of `text` [`QueryCompiler::compile`] reads when the text limit is `max_text_len` bytes (SPEC §7.1.1).
///
/// Clause limits may drop more. `schema` decides which `field:` prefixes are real clauses.
#[doc(hidden)]
pub fn text_within_limit<'a>(
    text: &'a str,
    max_text_len: usize,
    schema: &CollectionSchema,
) -> &'a str {
    cut(text, max_text_len, schema).0
}

/// Splits user text into clauses: `[-][field:](word | "phrase")`, separated by whitespace. Each comes with its bytes
/// in `text`: from its first byte (a `-`, a field name or its body) to the first byte after it, not counting the
/// whitespace around it. Both ends are character boundaries, as whitespace and the ASCII `"` are whole characters.
fn clause_spans<'t>(text: &'t str, schema: &CollectionSchema) -> Vec<(Clause<'t>, Range<usize>)> {
    let mut clauses = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let start = suffix_offset(text, rest);
        let (negated, field, body) = clause_head(rest, schema);
        if let Some(inner) = body.strip_prefix('"')
            && let Some((phrase, after)) = inner.split_once('"')
        {
            clauses.push((
                Clause {
                    negated,
                    field,
                    phrase: true,
                    text: phrase,
                },
                start..suffix_offset(text, after),
            ));
            rest = after.trim_start();
            continue;
        }
        let word_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let (word, after) = body.split_at_checked(word_end).unwrap_or((body, ""));
        clauses.push((
            Clause {
                negated,
                field,
                phrase: false,
                text: word,
            },
            start..suffix_offset(text, after),
        ));
        rest = after.trim_start();
    }
    clauses
}

/// Where a clause-opening `"` in `text` is left without its closing quote. `None` when every such quote closes.
fn unclosed_quote(text: &str, schema: &CollectionSchema) -> Option<usize> {
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let clause_at = suffix_offset(text, rest);
        let (_, _, body) = clause_head(rest, schema);
        if let Some(inner) = body.strip_prefix('"') {
            if let Some((_, after)) = inner.split_once('"') {
                rest = after.trim_start();
                continue;
            }
            return Some(clause_at);
        }
        let word_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let after = body.get(word_end..).unwrap_or("");
        rest = after.trim_start();
    }
    None
}

/// `-` and `field:` only count when the clause continues after them. `body` is what remains to lex.
fn clause_head<'a>(
    mut body: &'a str,
    schema: &CollectionSchema,
) -> (bool, Option<FieldId>, &'a str) {
    let negated = match body.strip_prefix('-') {
        Some(after) if starts_clause(after) => {
            body = after;
            true
        }
        _ => false,
    };
    let mut field = None;
    // Only the clause's first word can name a field, so the search stops at whitespace or a quote.
    if let Some(colon) = body.find(|c: char| c == ':' || c == '"' || c.is_whitespace())
        && let Some((name, after_colon)) = body.split_at_checked(colon)
        && let Some(after) = after_colon.strip_prefix(':')
        && starts_clause(after)
        && let Some(f) = schema.field(name)
        && searchable(f.field_type())
    {
        field = Some(f.id());
        body = after;
    }
    (negated, field, body)
}

/// Byte index of `suffix` in `text` when `suffix` is a suffix of `text` (or of a prefix `text` was cut from,
/// which is still a suffix of that prefix). Empty `suffix` is the end.
fn suffix_offset(text: &str, suffix: &str) -> usize {
    text.len().saturating_sub(suffix.len())
}

/// Whether `text` starts a clause: not empty, not whitespace.
fn starts_clause(text: &str) -> bool {
    text.chars().next().is_some_and(|c| !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use persia_analysis::Tokenizer;

    use crate::schema::{FieldKind, SchemaBuilder, TextOptions};

    use super::*;

    fn schema() -> CollectionSchema {
        SchemaBuilder::new("c")
            .field("title", FieldKind::Text(TextOptions::default()))
            .build()
            .unwrap()
    }

    fn cut_at(text: &str, max: usize) -> (&str, bool) {
        cut(text, max, &schema())
    }

    #[test]
    fn cut_keeps_whole_characters_and_whole_clauses() {
        // A cut inside `ñ` must not keep a partial character, and a cut inside a word drops the word.
        assert_eq!(cut_at("añb", 2), ("", true));
        assert_eq!(cut_at("añb", 3), ("", true));
        assert_eq!(cut_at("añb", 4), ("añb", false));
        assert_eq!(cut_at("ñ", 0), ("", true));
        // The word before the cut character is kept.
        assert_eq!(cut_at("a ñb", 2), ("a", true));
        // A word that ends on the cap is kept; the next word is not in the prefix.
        assert_eq!(cut_at("red shoes", 3), ("red", true));
        assert_eq!(cut_at("red shoes", 4), ("red", true));
    }

    #[test]
    fn equal_spans_are_not_one_field_twice() {
        /// Two tokens, one span: starts do not decrease (SPEC §6.3.1 after a mapping), so the debug assert holds,
        /// and the field must still appear once.
        struct SameSpan;
        impl Tokenizer for SameSpan {
            fn tokenize(&self, text: &str, out: &mut TokenStream) {
                let end = text.len();
                out.push("one", 0, end, 0);
                out.push("two", 0, end, 1);
            }
        }
        let analyzer = Analyzer::builder(SameSpan).build();
        let mut stream = TokenStream::new();
        analyzer.analyze_query("ab", &mut stream);
        let tokens: Vec<QueryToken> = stream
            .iter()
            .map(|t| QueryToken {
                span: (t.start, t.end),
                position: t.position,
                term: t.text.clone(),
            })
            .collect();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].span, tokens[1].span);

        let schema = schema();
        let id = schema.field("title").unwrap().id();
        let queries = word(vec![FieldTokens {
            id,
            weight: 1.0,
            tokens,
        }]);
        let query = match queries.len() {
            1 => queries.into_iter().next().unwrap_or(Query::MatchAll),
            _ => Query::Bool(BoolQuery {
                must: queries,
                ..BoolQuery::default()
            }),
        };
        assert_eq!(query.validate(&schema, &QueryLimits::default()), Ok(()));
        assert!(
            no_repeated_field(&query),
            "equal spans put a field in one combined twice: {query:?}"
        );
    }

    fn no_repeated_field(query: &Query) -> bool {
        match query {
            Query::Combined(combined) => {
                let mut ids: Vec<FieldId> = combined.fields.iter().map(|f| f.field).collect();
                let count = ids.len();
                ids.sort_unstable();
                ids.dedup();
                ids.len() == count
            }
            Query::Bool(bool) => bool
                .must
                .iter()
                .chain(&bool.should)
                .chain(&bool.must_not)
                .all(no_repeated_field),
            Query::Boost(boost) => no_repeated_field(&boost.query),
            Query::ConstantScore(score) => no_repeated_field(&score.query),
            _ => true,
        }
    }
}
