//! User text → [`Query`] (SPEC §7.1.1): analysis per field, default AND, quoted phrases, `-` exclusion and
//! `field:` scoping. Never fails: syntax that does not parse is read as plain words.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_analysis::{Analyzer, AnalyzerRegistry, TokenStream};

use crate::schema::{CollectionSchema, FieldId, FieldKind, SchemaHash};
use crate::segment::MAX_TERM_LEN;

use super::QueryError;
use super::ast::{
    BoolQuery, CombinedQuery, PhraseQuery, PhraseTerm, Query, TermQuery, WeightedTerm,
    boost_in_range, searchable,
};
use super::limits::{QueryLimits, Usage};

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

/// The result of [`QueryCompiler::compile`].
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    /// The query.
    pub query: Query,
    /// Whether part of the text was left out to stay within the [`QueryLimits`].
    pub truncated: bool,
}

/// Compiles user text into queries over one collection (SPEC §7.1.1). Holds the analyzer of every `text` field.
pub struct QueryCompiler {
    schema: CollectionSchema,
    /// By field id; `None` for fields that are not `text`.
    analyzers: Vec<Option<Arc<Analyzer>>>,
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
    /// A compiler for `schema`, analyzing `text` fields with the analyzers `registry` has for them.
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
            .collect::<Result<_, _>>()?;
        Ok(Self {
            schema: schema.clone(),
            analyzers,
        })
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
        if usage.add(limits, 1, 0, 0).is_err() {
            return Compiled {
                query: Query::MatchAll,
                truncated: !text.trim().is_empty(),
            };
        }
        for clause in lex(text, &self.schema) {
            let parts = self.clause(clause, fields, &mut tokens);
            let part = match (clause.negated, parts.len()) {
                (_, 0) => continue,
                (false, _) | (true, 1) => parts,
                (true, _) => vec![Query::Bool(BoolQuery {
                    must: parts,
                    ..BoolQuery::default()
                })],
            };
            if shallow {
                // Promote the depth-1 clause under a root bool and count `part` beside it.
                let mut trial = Usage::default();
                let promoted = trial.add(limits, 1, 0, 0).is_ok()
                    && query
                        .must
                        .iter()
                        .chain(&query.must_not)
                        .try_for_each(|q| q.check(&self.schema, limits, &mut trial, 2))
                        .is_ok()
                    && part
                        .iter()
                        .try_for_each(|q| q.check(&self.schema, limits, &mut trial, 2))
                        .is_ok();
                if !promoted {
                    truncated = true;
                    break;
                }
                shallow = false;
                usage = trial;
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
                            query.must.extend(part);
                            continue;
                        }
                    }
                    truncated = true;
                    break;
                }
                usage = trial;
            }
            if clause.negated {
                query.must_not.extend(part);
            } else {
                query.must.extend(part);
            }
        }
        let query = match (query.must.len(), query.must_not.len()) {
            (0, 0) => Query::MatchAll,
            (1, 0) => query.must.pop().unwrap_or(Query::MatchAll),
            _ => Query::Bool(query),
        };
        Compiled { query, truncated }
    }

    /// The queries a clause requires (all of them), or none when it analyzes to no token.
    fn clause(
        &self,
        clause: Clause<'_>,
        fields: &DefaultFields,
        tokens: &mut TokenStream,
    ) -> Vec<Query> {
        if let Some(field) = clause.field {
            return self
                .field_clause(field, clause.text, tokens)
                .into_iter()
                .collect();
        }
        let per_field: Vec<FieldTokens> = fields
            .fields()
            .iter()
            .filter_map(|&(id, weight)| {
                let tokens = self.analyzed(id, clause.text, tokens)?;
                (!tokens.is_empty()).then_some(FieldTokens { id, weight, tokens })
            })
            .collect();
        if !clause.phrase || per_field.iter().all(|f| f.tokens.len() <= 1) {
            return word(per_field);
        }
        let should: Vec<Query> = per_field
            .into_iter()
            .filter_map(|f| self.text_query(f.id, f.tokens))
            .collect();
        one_of(should).into_iter().collect()
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
                let analyzed = self.analyzed(id, text, tokens)?;
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

    /// The query-time tokens of `text` in `text` field `id` that may be in the index, in text order; `None` if
    /// it is not a `text` field.
    fn analyzed(
        &self,
        id: FieldId,
        text: &str,
        tokens: &mut TokenStream,
    ) -> Option<Vec<QueryToken>> {
        self.analyzer(id)?.analyze_query(text, tokens);
        Some(
            tokens
                .iter()
                .filter(|t| fits(&t.text))
                .map(|t| QueryToken {
                    span: (t.start, t.end),
                    position: t.position,
                    term: t.text.clone(),
                })
                .collect(),
        )
    }

    fn analyzer(&self, id: FieldId) -> Option<&Analyzer> {
        let index = usize::try_from(id.get()).ok()?;
        self.analyzers.get(index)?.as_deref()
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
    for (clause, clause_end) in &spans {
        // A closed phrase ends on its quote. A word that runs to the cap and into the next character was split.
        let split_word = continues && *clause_end == end && !clause.phrase;
        if split_word {
            break;
        }
        keep = *clause_end;
    }
    // A `"` opened inside the cap and never closed there: the closing quote was cut off (or the quoted text
    // runs past the cap). Drop from that clause, same as dropping the split word it contains.
    if let Some(open) = unclosed_quote(prefix, schema) {
        keep = spans
            .iter()
            .map(|(_, clause_end)| *clause_end)
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

/// Splits user text into clauses: `[-][field:](word | "phrase")`, separated by whitespace.
fn lex<'t>(text: &'t str, schema: &CollectionSchema) -> Vec<Clause<'t>> {
    clause_spans(text, schema)
        .into_iter()
        .map(|(clause, _)| clause)
        .collect()
}

/// `(clause, exclusive end byte)` in `text`. The end is the first byte after the clause, not counting the
/// whitespace that follows it.
fn clause_spans<'t>(text: &'t str, schema: &CollectionSchema) -> Vec<(Clause<'t>, usize)> {
    let mut clauses = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
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
                suffix_offset(text, after),
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
            suffix_offset(text, after),
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
