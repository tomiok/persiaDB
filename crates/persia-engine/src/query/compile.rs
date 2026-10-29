//! User text → [`Query`] (SPEC §7.1.1): analysis per field, default AND, quoted phrases, `-` exclusion and
//! `field:` scoping. Never fails: syntax that does not parse is read as plain words.

use std::collections::BTreeMap;
use std::sync::Arc;

use persia_analysis::{Analyzer, AnalyzerRegistry, TokenStream};

use crate::schema::{CollectionSchema, FieldId, FieldKind};
use crate::segment::MAX_TERM_LEN;

use super::QueryError;
use super::ast::{
    BoolQuery, CombinedQuery, PhraseQuery, PhraseTerm, Query, TermQuery, WeightedTerm,
    boost_in_range, searchable,
};
use super::limits::{QueryLimits, Usage};

/// The `text` fields plain words and phrases search, with their BM25F weights (the `fields` of SPEC §7.1).
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultFields(Vec<(FieldId, f64)>);

impl DefaultFields {
    /// Every `text` field of `schema`, weighted by its `boost`: the default of SPEC §7.1.
    pub fn all(schema: &CollectionSchema) -> Self {
        Self(
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
        Ok(Self(fields))
    }

    /// The fields and their weights, in the order given.
    pub fn fields(&self) -> &[(FieldId, f64)] {
        &self.0
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
    /// beyond [`QueryLimits::max_text_len`] and the clauses from the first one that would exceed `limits` on are
    /// left out, with [`Compiled::truncated`] set (the root counts as a clause at depth 1 even when the query is
    /// a single clause). The query then validates against the schema and `limits`
    /// ([`Query::validate`]) whenever [`Query::MatchAll`] does. `fields` should come from the same schema:
    /// fields that are not its `text` fields are skipped.
    pub fn compile(&self, text: &str, fields: &DefaultFields, limits: &QueryLimits) -> Compiled {
        let (text, mut truncated) = cut(text, limits.max_text_len);
        let mut tokens = TokenStream::new();
        let mut query = BoolQuery::default();
        let mut usage = Usage::default();
        // The root Bool; a query that holds one clause is that clause, so this only over-counts.
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
            let mut trial = usage;
            let fits = part
                .iter()
                .try_for_each(|q| q.check(&self.schema, limits, &mut trial, 2));
            if fits.is_err() {
                truncated = true;
                break;
            }
            usage = trial;
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
        let own = f.tokens.iter().map(|t| t.span);
        // Tokens of one analysis have strictly increasing starts (SPEC §6.3.1), so equal spans are adjacent.
        if own.clone().zip(own.clone().skip(1)).any(|(a, b)| a == b) {
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

/// `text` cut to at most `max` bytes at a character boundary, and whether it was cut.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (text.get(..end).unwrap_or_default(), true)
}

/// Splits user text into clauses: `[-][field:](word | "phrase")`, separated by whitespace.
fn lex<'t>(text: &'t str, schema: &CollectionSchema) -> Vec<Clause<'t>> {
    let mut clauses = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let mut body = rest;
        // `-` and `field:` only count when something follows them in the clause.
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
            && let Some((name, after)) = body.split_at_checked(colon)
            && let Some(after) = after.strip_prefix(':')
            && starts_clause(after)
            && let Some(f) = schema.field(name)
            && searchable(f.field_type())
        {
            field = Some(f.id());
            body = after;
        }
        if let Some(inner) = body.strip_prefix('"')
            && let Some((phrase, after)) = inner.split_once('"')
        {
            clauses.push(Clause {
                negated,
                field,
                phrase: true,
                text: phrase,
            });
            rest = after.trim_start();
            continue;
        }
        let end = body.find(char::is_whitespace).unwrap_or(body.len());
        let (word, after) = body.split_at_checked(end).unwrap_or((body, ""));
        clauses.push(Clause {
            negated,
            field,
            phrase: false,
            text: word,
        });
        rest = after.trim_start();
    }
    clauses
}

/// Whether `text` starts a clause: not empty, not whitespace.
fn starts_clause(text: &str) -> bool {
    text.chars().next().is_some_and(|c| !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_keeps_whole_characters() {
        assert_eq!(cut("añb", 2), ("a", true));
        assert_eq!(cut("añb", 3), ("añ", true));
        assert_eq!(cut("añb", 4), ("añb", false));
        assert_eq!(cut("ñ", 0), ("", true));
    }
}
