//! User text → [`Query`] (SPEC §7.1.1): analysis per field, default AND, quoted phrases, `-` exclusion and
//! `field:` scoping. Never fails: syntax that does not parse is read as plain words.

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
    /// - a word is analyzed with each of `fields`' analyzers; the tokens of one span of the word become one
    ///   [`Query::Combined`] over the fields that produced them (a word may give several, as `e-mail` does);
    /// - `"a phrase"` becomes, per field, a [`Query::Phrase`] (fields without positions: every term required),
    ///   one of which must match; a phrase of at most one token per field is a word;
    /// - `field:word` and `field:"a phrase"` search that `text` or `keyword` field only, with [`Query::Term`]s
    ///   (a `keyword` field takes the text as is);
    /// - `-` before a clause excludes the documents it matches.
    ///
    /// Nothing fails: a `field:` naming no `text` or `keyword` field, a `"` without its closing quote and a lone `-`
    /// are plain text. Clauses that analyze to no token (stopwords) are left out, as are tokens longer than
    /// [`MAX_TERM_LEN`] (no document holds one); without any clause left the query is [`Query::MatchAll`]. Text
    /// beyond [`QueryLimits::max_text_len`] and the clauses from the first one that would exceed `limits` on are
    /// left out, with [`Compiled::truncated`] set. The query then validates against the schema and `limits`
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
        match (clause.field, clause.phrase) {
            (Some(field), _) => self
                .field_clause(field, clause.text, tokens)
                .into_iter()
                .collect(),
            (None, false) => self.word(clause.text, fields, tokens),
            (None, true) => {
                let per_field: Vec<(FieldId, Vec<String>)> = fields
                    .fields()
                    .iter()
                    .filter_map(|&(id, _)| Some((id, self.terms(id, clause.text, tokens)?)))
                    .filter(|(_, terms)| !terms.is_empty())
                    .collect();
                if per_field.iter().all(|(_, terms)| terms.len() <= 1) {
                    return self.word(clause.text, fields, tokens);
                }
                let mut should: Vec<Query> = per_field
                    .into_iter()
                    .filter_map(|(id, _)| self.field_clause(id, clause.text, tokens))
                    .collect();
                match should.len() {
                    0 => Vec::new(),
                    1 => should.pop().into_iter().collect(),
                    _ => vec![Query::Bool(BoolQuery {
                        should,
                        ..BoolQuery::default()
                    })],
                }
            }
        }
    }

    /// A word over the default fields: one [`Query::Combined`] per token span, in text order.
    fn word(&self, word: &str, fields: &DefaultFields, tokens: &mut TokenStream) -> Vec<Query> {
        // (start, end, repeat of that span in its field) → the fields' terms of that token.
        let mut groups: Vec<((usize, usize, usize), Vec<WeightedTerm>)> = Vec::new();
        for &(id, weight) in fields.fields() {
            let Some(analyzer) = self.analyzer(id) else {
                continue;
            };
            analyzer.analyze_query(word, tokens);
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for token in tokens.iter().filter(|t| fits(&t.text)) {
                let span = (token.start, token.end);
                let repeat = spans.iter().filter(|&&s| s == span).count();
                spans.push(span);
                let key = (token.start, token.end, repeat);
                let term = WeightedTerm {
                    field: id,
                    term: token.text.clone(),
                    weight,
                };
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    // The repeat count keeps a field once per group.
                    Some((_, terms)) => terms.push(term),
                    None => groups.push((key, vec![term])),
                }
            }
        }
        groups.sort_by_key(|&(key, _)| key);
        groups
            .into_iter()
            .map(|(_, fields)| Query::Combined(CombinedQuery { fields }))
            .collect()
    }

    /// `text` in one field: a `keyword` field's term as is, or a `text` field's terms (one: a term; more: a phrase,
    /// or every term required without positions). `None` for no term.
    fn field_clause(&self, id: FieldId, text: &str, tokens: &mut TokenStream) -> Option<Query> {
        let field = self.schema.field_by_id(id)?;
        let term = |term: String| Query::Term(TermQuery { field: id, term });
        match field.kind() {
            FieldKind::Keyword(_) => {
                (!text.is_empty() && fits(text)).then(|| term(text.to_owned()))
            }
            FieldKind::Text(options) => {
                let analyzer = self.analyzer(id)?;
                analyzer.analyze_query(text, tokens);
                let mut kept: Vec<(u32, String)> = tokens
                    .iter()
                    .filter(|t| fits(&t.text))
                    .map(|t| (t.position, t.text.clone()))
                    .collect();
                kept.sort_by_key(|&(position, _)| position);
                let first = kept.first().map(|&(p, _)| p)?;
                if kept.len() == 1 {
                    return kept.pop().map(|(_, t)| term(t));
                }
                Some(if options.positions {
                    Query::Phrase(PhraseQuery {
                        field: id,
                        terms: kept
                            .into_iter()
                            .map(|(position, term)| PhraseTerm {
                                term,
                                position: position.saturating_sub(first),
                            })
                            .collect(),
                    })
                } else {
                    Query::Bool(BoolQuery {
                        must: kept.into_iter().map(|(_, t)| term(t)).collect(),
                        ..BoolQuery::default()
                    })
                })
            }
            _ => None,
        }
    }

    /// The terms `text` analyzes to in `text` field `id`; `None` if it is not one.
    fn terms(&self, id: FieldId, text: &str, tokens: &mut TokenStream) -> Option<Vec<String>> {
        let analyzer = self.analyzer(id)?;
        analyzer.analyze_query(text, tokens);
        Some(
            tokens
                .iter()
                .filter(|t| fits(&t.text))
                .map(|t| t.text.clone())
                .collect(),
        )
    }

    fn analyzer(&self, id: FieldId) -> Option<&Analyzer> {
        let index = usize::try_from(id.get()).ok()?;
        self.analyzers.get(index)?.as_deref()
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
