---
title: "Typo-tolerant search and zero results"
description: "How Persia DB designs typo-tolerant embedded search in Rust: query relaxation so a fuzzy query avoids zero results, and where BM25F and filters fit today."
head:
  - tag: title
    content: "Typo-tolerant search in Rust: query relaxation and zero results | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Typo-tolerant search in Rust: query relaxation and zero results | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Typo-tolerant search in Rust: query relaxation and zero results | PersiaDB"
  - tag: script
    attrs:
      type: application/ld+json
    content: |
      {
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "name": "Typo-tolerant search and zero results",
        "url": "https://tomiok.github.io/persiaDB/guide/typo-tolerant-search/",
        "mainEntity": [
          {
            "@type": "Question",
            "name": "Does Persia DB do typo-tolerant search today?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. Nothing runs a search, and the relaxation cascade has not been started (milestone M8). A typo in user text stays a required term. The query AST can represent a fuzzy clause, and Query::validate accepts an edit distance of 1 or 2, but no code lists the terms within that distance or returns hits."
            }
          },
          {
            "@type": "Question",
            "name": "What is query relaxation?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Query relaxation is the cascade in SPEC section 7.6. If a query has fewer hits than min_results (default 1), the engine is specified to walk exact match, synonyms, dropped terms, prefix, fuzzy edit distance, then a fallback, and to stop at the first level with enough hits. The response names the level. That cascade is not implemented. RelaxationConfig stores the collection defaults."
            }
          },
          {
            "@type": "Question",
            "name": "Are filters relaxed when a search has zero results?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Not by default. SPEC section 7.6 keeps filters strict unless relax.filters is last_resort. The filter DSL parses and validates (term, terms, range, exists, and, or, not). Evaluating a filter against documents is ROADMAP 7.4.5 and has not been started. Filter clauses do not add to the BM25F score."
            }
          },
          {
            "@type": "Question",
            "name": "Is BM25F scoring implemented?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The formula is. TermScorer scores one query word from a document frequency, a term frequency, and a norm byte, using collection statistics. Static rank and score upper bounds are in the same module. Tests check it against a reference scorer. It does not walk postings or return a ranked hit list. This project does not publish search benchmarks."
            }
          },
          {
            "@type": "Question",
            "name": "Can I call Db::open and search for a fuzzy query?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. persia::Db is not in the crate, and persia_engine::db::Database has no search method. Database::open, put, and commit exist. Code samples that call Db::search on this site are illustrative."
            }
          }
        ]
      }
---

> **Status: pre-alpha.** %%PRE_ALPHA%%
> Counts below are read from [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) when the site is built:
> %%PROGRESS_SUMMARY%%.
> When this page and [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) disagree, the SPEC wins.
> Nothing here is a benchmark.

Typo-tolerant search, in this design, is not a separate fuzzy index. A query that would otherwise come back empty is loosened one step at a time until it has enough hits. That cascade is [query relaxation](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#76-zero-results-avoidance-default-on) (SPEC §7.6). It is on by default. Fuzzy edit distance is one step of it, after an exact match, synonyms, and dropping terms.

The product goal is embedded full-text search in Rust that, by default, never returns zero results ([SPEC §1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#1-goals-and-non-goals)). What that library shape is, and what you can call today, is [What is an embedded search database?](/guide/embedded-search/). This page is the search behavior: how a typo is supposed to be handled, how [BM25F](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#72-scoring-bm25f) and filters fit, and which of those pieces are in the repository.

## What typo-tolerant search means here

A user types `runing shoes` and expects shoes, not an empty page. The specified behavior is:

1. Run the query as written. Every analyzed term is required (default AND).
2. If the hit count is below `min_results` (default 1), try the next relaxation level.
3. Stop at the first level that has enough hits, unless `fill` is set, in which case looser levels may complete the page.
4. Rank hits from a stricter level above hits from a looser level.
5. Say what changed: the level, the steps applied, the original text, the effective text, and any corrected terms.

Filters stay as the caller wrote them. A brand or a price range is not dropped to manufacture a hit, unless the query sets `relax.filters` to `"last_resort"`. A query can turn the cascade off with `relax: false`.

[Meilisearch](https://www.meilisearch.com/docs) is a released server whose typo tolerance is on by default. Persia DB's planned default is this cascade, in a library, with the response naming the step. The mechanisms differ. That comparison, which is not a benchmark, is [Persia DB vs SQLite FTS5, Tantivy, and Meilisearch](/guide/comparison/).

The cascade has not been started. Milestone M8 (zero-results avoidance) is blocked on the query milestone and has no leaves done. ROADMAP 8.1 through 8.4 — the orchestrator, the levels, the response metadata, and the zero-result regression gate — are unchecked. `RelaxationConfig` in `persia-engine` stores the collection defaults from SPEC §6.2.3 (`enabled`, `min_results`, `levels`, `fallback`, `fill`, `filters`, `budget_ms`). Schema JSON parses them. Nothing walks the levels at search time.

## The relaxation cascade

Each level is specified in SPEC §7.6. The names are the ones the response and the schema use.

| Level | Name | What it changes | In the tree |
|---|---|---|---|
| L0 | `exact` | Every analyzed term is required. | The user-text compiler builds this shape (ROADMAP 7.1). Nothing matches it against postings. |
| L1 | `synonyms` | A term expands through the collection's synonym groups. Within a group the terms are alternatives. Expansions per matched term are capped (default 50). | `SynonymMap` in `persia-analysis` looks up analyzed tokens (ROADMAP 4.2.5). The search level that would call it (ROADMAP 8.2.1) is not written. |
| L2 | `drop_terms` | The share of terms that must match steps from 100% to 75%, to 50%, then to one term. Low-IDF terms go first. | Not started (ROADMAP 8.2.2). |
| L3 | `prefix` | The last term, and other terms of length at least 3, expand through an FST prefix scan. Expansions are capped (default 50). | The AST has a `Prefix` node. No scan runs (ROADMAP 8.2.3). |
| L4 | `fuzzy` | Edit distance 1 for a term of length 4–7, distance 2 for length 8 or more, via a Levenshtein automaton intersected with the term dictionary. Corrected terms are reported. | The AST has a `Fuzzy` node with distance 1 or 2. No automaton runs (ROADMAP 8.2.4). |
| L5 | `fallback` | Top documents by static rank, or by recency when the collection has no static rank. `none` allows an empty page. | Not started (ROADMAP 8.2.5). |

Other rules from the same section, all still design:

- A time budget, `budget_ms`, defaults to 50 ms. Past it, the best results so far come back with `truncated: true`. That flag is distinct from `query_truncated`, which means the compiler cut the user's text to fit the query limits (SPEC §7.1.1).
- Holding the queries a level builds to those same limits is pending (ROADMAP 8.1.8, SPEC §7.1.3). A level is specified to drop the least important expansions first and to name that cut in `applied`.
- The response object below is the shape in SPEC §7.6. No function returns it.

```json
{
  "level": "fuzzy",
  "applied": ["drop_terms:75%", "fuzzy"],
  "original": "zapatilas rojs",
  "effective": "zapatillas rojas",
  "corrected": { "zapatilas": "zapatillas", "rojs": "rojas" }
}
```

The same table, and the schema JSON that carries `relaxation`, is the [feature reference](/guide/features/#zero-results-relaxation). The diagram on [Features](/features/#zero-results-relaxation) is this cascade.

## Compiling a query

Before any relaxation, user text becomes a query (SPEC §7.1, §7.1.1). That compiler is in the tree (ROADMAP 7.1). It does not search.

`QueryCompiler::compile` never fails on user text. Syntax that does not parse is read as plain text. Clauses are split on whitespace and all required (default AND). A clause is an optional `-`, an optional `field:`, and a word or a `"phrase"`. `-` excludes the documents the clause would match. `field:` limits it to that `text` or `keyword` field. A `"` without a closing quote is an ordinary character. Text past `max_text_len` (default 8 KiB) is left out on a clause boundary, so a cut word does not become a required term that matches nothing. With no clause left, the query is `match_all`.

Words are analyzed with each field's query-time analyzer. Fields that agree on token spans become one `combined` clause per span: one query word, several fields, scored as one BM25F term. A phrase on a field with positions stays a phrase. On a field without positions it becomes a boolean that requires each token.

Filters are a separate JSON object (SPEC §7.1.2): `term`, `terms`, `range`, `exists`, `and`, `or`, and `not`. They are written by programs, so a bad filter is an error. User text is not. `Filter::from_json` parses and validates them against the schema. Nothing yet evaluates a filter against a segment (ROADMAP 7.4.5).

Limits (SPEC §7.1.3), implemented as `QueryLimits`, reject a hand-built query or a filter that exceeds them. Defaults: 1024 leaf terms, 1024 clauses, 1024 expansions summed across prefix and fuzzy clauses, depth 32, and 8 KiB of `q`. The prefix and fuzzy clauses the engine itself would build use a per-clause cap of 50 (`DEFAULT_MAX_EXPANSIONS`).

This is library code in `persia-engine`. It compiles text. It does not open a database and it does not return hits:

```rust
use persia_analysis::AnalyzerRegistry;
use persia_engine::query::{DefaultFields, Filter, QueryCompiler, QueryLimits};
use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};

let schema = SchemaBuilder::new("products")
    .field("title", FieldKind::Text(TextOptions { positions: true, ..TextOptions::default() }))
    .field("brand", FieldKind::Keyword(KeywordOptions::default()))
    .field("price", FieldKind::F64 { stored: true })
    .build()?;
let compiler = QueryCompiler::new(&schema, &AnalyzerRegistry::builtin())?;
let limits = QueryLimits::default();
let compiled = compiler.compile(
    r#"red "running shoes" -kids"#,
    &DefaultFields::all(&schema),
    &limits,
);
compiled.query.validate(&schema, &limits)?;

let filter = Filter::from_json(
    r#"{"range": ["price", {"lte": 100}]}"#,
    &schema,
    &limits,
)?;
```

The AST can also represent `prefix` and `fuzzy` clauses. `Query::validate` accepts a fuzzy distance of 1 or 2. The compiler does not turn a typo into one of those clauses. A typo stays the term the analyzer produced, and under default AND it is required. Fuzzy matching is L4 of the cascade, which is not written. Executing any of these clauses — term iterators, phrases, boolean combination, top-k — is ROADMAP 7.4 and has not been started.

## BM25F scoring and filters

Ranking is BM25F (SPEC §7.2). For a term `t` and a document `d`, over fields `f` with weights `w_f`:

```text
tf~(t,d) = Σ_f  w_f · tf(t,d,f) / (1 − b_f + b_f · len_f(d) / avglen_f)
score(t,d) = idf(t) · tf~ · (k1 + 1) / (k1 + tf~)
idf(t) = ln(1 + (N − df + 0.5) / (df + 0.5))
```

`N`, `df`, and `avglen` are collection-level. `TermScorer` computes one query word from those statistics plus a document's term frequency and norm byte in each field. A `combined` clause is one word over several `text` fields; `df` is the max of the per-field document frequencies. A `term` on a `keyword` field uses weight 1, `b` 0, and `k1` 1.2. The total adds a static rank when the schema has one: `weight · log1p(value)` or `weight · linear(value)` (SPEC §6.2.3). `must` and `should` clauses score. `filter` and `must_not` clauses do not, so a filter never inflates BM25F.

`StatisticsProvider` is the trait those numbers come from. `SegmentStatistics` sums the segments of one snapshot, deleted documents included until a merge drops them. `SearchStatistics` adds the memtable's live documents. A snapshot computes the segment part once.

The same function, given a block's maximum term frequency and minimum norm, is an upper bound used later by MaxScore and block-max WAND (SPEC §7.3). The bound is implemented (ROADMAP 7.3.3). The iterators that would use it are not (ROADMAP 7.4.4).

`persia-engine/tests/reference` evaluates the formula on the quantized field lengths. The scorer must match it within `1e-9 · idf·(k1+1)` (SPEC §7.2, ROADMAP 7.3.4). A differential suite over random corpora and full queries is ROADMAP 7.8.1 and is not written. [SPEC §16.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#16-quality-requirements) lists latency figures and labels them as targets, not promises. This project does not publish search benchmarks.

What scoring does not do: walk postings, apply tombstones, collect a top-k, facet, or sort. A score for one document you already hold is not a search result.

## What runs today

| Piece | Status |
|---|---|
| User-text compiler, query AST, filter DSL, query limits | Library code (ROADMAP 7.1). |
| Collection statistics | Library code (ROADMAP 7.2). |
| BM25F scorer, static rank, score upper bounds | Library code (ROADMAP 7.3), checked against the reference scorer. |
| `RelaxationConfig` and `SynonymMap` | Stored and looked up. Not called from a search. |
| Matching, top-k, filter evaluation, facets, sort, explain | Not started (ROADMAP 7.4–7.7). |
| Relaxation cascade | Not started (M8). |
| `persia::Db::open` and `search` | Not in the crate (M10). |

`persia_engine::db::Database` has `open`, `put`, and `commit`. It has no `search`. The public `persia` crate exports no `Db`. %%ENGINE_CALLS%%

The call below is the intended API from SPEC §7.1 and §7.6. It is illustrative: these methods are not in the `persia` crate.

```rust
// ILLUSTRATIVE — these methods are not in the persia crate.
let db = persia::Db::open("app.persia")?;
let products = db.create_collection(schema)?;
products.put(doc)?;
products.commit()?;

// A typo is specified to return hits. The response says which level ran.
let results = products.search("runing shoes")?;
let relaxation = &results.relaxation;
```

[Getting started](/guide/getting-started/) separates that sketch from the crates you can depend on. [Code](/guide/code/) links the modules that exist. The planned method list is the [embedded API](/guide/embedded-api/).

## FAQ

### Does Persia DB do typo-tolerant search today?

No. Nothing runs a search, and the relaxation cascade has not been started (M8). A typo in user text stays a required term. The query AST can represent a fuzzy clause, and `Query::validate` accepts an edit distance of 1 or 2, but no code lists the terms within that distance or returns hits.

### What is query relaxation?

The cascade in SPEC §7.6. If a query has fewer hits than `min_results` (default 1), the engine is specified to walk exact match, synonyms, dropped terms, prefix, fuzzy edit distance, then a fallback, and to stop at the first level with enough hits. The response names the level. That cascade is not implemented. `RelaxationConfig` stores the collection defaults.

### Are filters relaxed when a search has zero results?

Not by default. SPEC §7.6 keeps filters strict unless `relax.filters` is `"last_resort"`. The filter DSL parses and validates (`term`, `terms`, `range`, `exists`, `and`, `or`, `not`). Evaluating a filter against documents is ROADMAP 7.4.5 and has not been started. Filter clauses do not add to the BM25F score.

### Is BM25F scoring implemented?

The formula is. `TermScorer` scores one query word from a document frequency, a term frequency, and a norm byte, using collection statistics. Static rank and score upper bounds are in the same module. Tests check it against a reference scorer. It does not walk postings or return a ranked hit list. This project does not publish search benchmarks.

### Can I call Db::open and search for a fuzzy query?

No. `persia::Db` is not in the crate, and `Database` has no `search` method. `Database::open`, `put`, and `commit` exist. Code samples that call `Db::search` on this site are illustrative.

## Read next

- [What is an embedded search database?](/guide/embedded-search/) — in-process full-text search, one file, and no server.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
- [Feature reference](/guide/features/) — the cascade table, the schema sketch, and the status of each capability.
- [Features](/features/) — the short status table, including the relaxation diagram.
- [Getting started](/guide/getting-started/) — crates you can call, and the API that is still illustrative.
- [FAQ](/guide/faq/) — production readiness, the file format, crash safety, and the license.
- [How it works](/architecture/) — the query path from analyzed text to the cascade.
- [Roadmap](/roadmap/) — milestone status. The live counter is PROGRESS.md.
