---
title: "Typo-tolerant search and zero results"
description: "How Persia DB handles typo-tolerant embedded search in Rust: query relaxation and zero results, minimum_should_match, drop_terms, and where fuzzy matching and BM25F stand today."
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
              "text": "Partly. The compiler does not turn a typo into a fuzzy clause, and L4 fuzzy does not build a query, so a typo is not rewritten. An explicit fuzzy or prefix clause expands over the collection terms (ROADMAP 7.4.12). L2 drop_terms can return hits for a query of two or more terms by requiring fewer of them. A one-term typo still returns no hits. %%RELAXATION_CELL%% CollectionViews::search and CollectionViews::search_relaxed run over a collection snapshot. There is no search call on the database handle."
            }
          },
          {
            "@type": "Question",
            "name": "What is query relaxation?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Query relaxation is the cascade in SPEC section 7.6. CollectionViews::search_relaxed walks a RelaxPlan from the exact query, then the later levels, and stops at the first level with enough hits (min_results, default 1) unless fill is set. Hits from a stricter step rank above hits from a looser one. The response is a Relaxation value. %%RELAXATION_NOW%%"
            }
          },
          {
            "@type": "Question",
            "name": "Are filters relaxed when a search has zero results?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Not by default. SPEC section 7.6 keeps filters strict unless relax.filters is last_resort. FilterRelaxation::LastResort does not run (ROADMAP 8.2.6). L2 drop_terms copies the original filter onto every step. The filter DSL parses and validates (term, terms, range, exists, and, or, not). Evaluating a filter against documents is library code (ROADMAP 7.4.5). Filter clauses do not add to the BM25F score."
            }
          },
          {
            "@type": "Question",
            "name": "Is BM25F scoring implemented?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The formula is. TermScorer scores one query word from a document frequency, a term frequency, and a norm byte, using collection statistics. Static rank and score upper bounds are in the same module. Tests check it against a reference scorer. CollectionViews::search walks postings and returns a ranked page. This project does not publish search benchmarks."
            }
          },
          {
            "@type": "Question",
            "name": "Can I call Db::open and search for a fuzzy query?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. persia::Db is not in the crate, and persia_engine::db::Database has no search method. Database::open, put, and commit exist. Ranked search is CollectionViews::search, and the cascade is CollectionViews::search_relaxed. Code samples that call Db::search on this site are illustrative."
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

Typo-tolerant search, in this design, is not a separate fuzzy index. A query that would otherwise come back empty is loosened one step at a time until it has enough hits. That cascade is [query relaxation](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#76-zero-results-avoidance-default-on) (SPEC §7.6). It is on by default. Fuzzy edit distance is one step of it, after an exact match, synonyms, and dropping terms. The fuzzy step is not written. Dropping terms is.

The product goal is embedded full-text search in Rust that, by default, never returns zero results ([SPEC §1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#1-goals-and-non-goals)). What that library shape is, and what you can call today, is [What is an embedded search database?](/guide/embedded-search/). This page is the search behavior: how a typo is handled, how [BM25F](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#72-scoring-bm25f) and filters fit, and which of those pieces are in the repository. Filters, facets, sort, and pages are [Faceted search](/guide/faceted-search/).

## What typo-tolerant search means here

A user types `runing shoes` and expects shoes, not an empty page. The specified behavior is:

1. Run the query as written. Every analyzed term is required (default AND).
2. If the hit count is below `min_results` (default 1), try the next relaxation level.
3. Stop at the first level that has enough hits, unless `fill` is set, in which case looser levels may complete the page.
4. Rank hits from a stricter level above hits from a looser level.
5. Say what changed: the level, the steps applied, the original text, the effective text, and any corrected terms.

Filters stay as the caller wrote them. A brand or a price range is not dropped to manufacture a hit, unless the query sets `relax.filters` to `"last_resort"`. A query can turn the cascade off with `relax: false`.

[Meilisearch](https://www.meilisearch.com/docs) is a released server whose typo tolerance is on by default. Persia DB's specified default is this cascade, in a library, with the response naming the step. The mechanisms differ. That comparison, which is not a benchmark, is [Persia DB vs SQLite FTS5, Tantivy, and Meilisearch](/guide/comparison/).

%%RELAXATION_NOW%% `RelaxationConfig` in `persia-engine` stores the collection defaults from SPEC §6.2.3 (`enabled`, `min_results`, `levels`, `fallback`, `fill`, `filters`, `budget_ms`). Schema JSON parses them. `RelaxPlan::new` reads that config and a `RelaxOptions` override. The zero-result regression gate (ROADMAP 8.4) is not written.

## Query relaxation

`CollectionViews::search_relaxed` is the cascade (ROADMAP 8.1.2). It takes a `Query` or a `Compiled` query, a `SearchRequest`, a `RelaxPlan`, and an `Execution`, and returns `RelaxedResults`: the page, facet counts, and a `Relaxation`.

`RelaxPlan` is the ordered list of levels (ROADMAP 8.1.1). `RelaxLevel` names them: `exact`, `synonyms`, `drop_terms`, `prefix`, `fuzzy`, `fallback`. L0 is always first. A level's transform runs only when the cascade reaches it, so a query that already has `min_results` hits at L0 builds nothing more. Each later step searches with every earlier step excluded (`must_not`), so a document is a hit of the first step that matches it, once, at that step's score.

`SearchRequest` has no `relax` field. The plan is a separate argument. `RelaxOptions::disabled` is `relax: false`: L0 only. `RelaxOptions` can also override `min_results`, `levels`, `fallback`, `fill`, `filters`, and `budget_ms`. Wiring that object through the request is still open (ROADMAP 8.3.1). `fill` already changes the plan: with it set, the cascade keeps going until the page window is full (ROADMAP 8.1.2). The collection default is `fill: false`. What remains of ROADMAP 8.3.2 is that request option.

Ranking is step first, then the request's sort (ROADMAP 8.1.3). Every hit of a stricter step ranks above every hit of a looser one, at any score. `RelaxedHit` carries `level` (`RelaxLevel`), `step` (0 is L0), and `rank_score` (`relax::rank_score`). The hit's own score is its step's. A cursor from this search records the step. `CollectionViews::search` rejects a cursor from a step after exact (`RequestError::CursorStep`). Pages and that cursor are [Faceted search](/guide/faceted-search/).

The response is `Relaxation` (ROADMAP 8.1.5):

| Field | Meaning |
|---|---|
| `level` | Level of the last step built and not skipped. `RelaxLevel::Exact` when L0 was enough, or no later step fit. |
| `applied` | What each step after L0 applied, in build order. Empty when none did. A step cut to fit the limits ends with `Relaxation::CUT` (`:cut`). One that cannot fit ends with `Relaxation::SKIPPED` (`:skipped`) and is not searched. |
| `original` | The user text (`Compiled::text`). Absent for a programmatic query. |
| `effective` | Text of the last step. At L0, the clauses that compiled. At L2, the clauses that step keeps. Absent when `original` is, and when the level renders no text. |
| `corrected` | `Vec<Correction>`, each `{ original, corrected }`. Empty until the fuzzy level synthesizes a correction (ROADMAP 8.2.4.3). |
| `truncated` | The budget passed before the cascade finished. |

`relaxation.relaxed()` is true when a step after L0 was built and not skipped.

A time budget, `budget_ms`, defaults to 50. It starts after the exact level, so that level is never cut by it. Past it, the completed steps come back with `truncated: true` (ROADMAP 8.1.4). A step in progress is dropped whole. A request deadline is separate and applies to every step, including exact. [Faceted search](/guide/faceted-search/) is that deadline (`Execution::deadline`). `Relaxation.truncated` is distinct from `Compiled.truncated`, which means the compiler cut the user's text to fit the query limits. SPEC §7.1.1 calls that cut `query_truncated`.

`RelaxPlan::with_limits` holds every step after L0 to a `QueryLimits` (ROADMAP 8.1.8, SPEC §7.1.3). Prefix and fuzzy expansions are cut to the remaining budget, largest first, and the cut is named in `applied`. A step that still does not fit is skipped. Synonym expansions are not cut that way (ROADMAP 8.1.13). L1 builds no step yet, so that leaf does not change a result today.

The JSON below is the shape in SPEC §7.6, including a fuzzy correction. This engine does not produce it. `corrected` stays empty, and L4 builds no query.

```json
{
  "level": "fuzzy",
  "applied": ["drop_terms:75%", "fuzzy"],
  "original": "zapatilas rojs",
  "effective": "zapatillas rojas",
  "corrected": { "zapatilas": "zapatillas", "rojs": "rojas" }
}
```

The same table of levels, and the schema JSON that carries `relaxation`, is the [feature reference](/guide/features/#zero-results-relaxation). The diagram on [Features](/features/#zero-results-relaxation) is this cascade.

## The relaxation cascade

Each level is specified in SPEC §7.6. The names are the ones `RelaxLevel::name` and the schema use.

| Level | Name | What it changes | In the tree |
|---|---|---|---|
| L0 | `exact` | Every analyzed term is required. | The user-text compiler builds this shape (ROADMAP 7.1). It is the first step `search_relaxed` runs. |
| L1 | `synonyms` | A term expands through the collection's synonym groups. Within a group the terms are alternatives. Expansions per matched term are capped (default 50). | `SynonymMap` in `persia-analysis` looks up analyzed tokens (ROADMAP 4.2.5). The cascade level builds no step (ROADMAP 8.2.1). |
| L2 | `drop_terms` | The share of terms that must match steps from 100% to 75%, to 50%, then to one term. Least important terms are the ones a step reports dropped first. | Library code (ROADMAP 8.2.2.1–8.2.2.3). [minimum_should_match](#minimum_should_match) and [how terms are dropped](#how-terms-are-dropped). |
| L3 | `prefix` | The last term, and other terms of length at least 3, expand through an FST prefix scan. Expansions are capped (default 50). | An explicit prefix clause expands over collection terms (ROADMAP 7.4.12). The cascade does not build that step (ROADMAP 8.2.3). |
| L4 | `fuzzy` | Edit distance 1 for a term of length 4–7, distance 2 for length 8 or more, via a Levenshtein automaton intersected with the term dictionary. Corrected terms are reported. | An explicit fuzzy clause expands the same way (ROADMAP 7.4.12). The cascade step, the automaton, and corrected terms are not written (ROADMAP 8.2.4). |
| L5 | `fallback` | Top documents by static rank, or by recency when the collection has no static rank. `none` allows an empty page. | The cascade level is not written (ROADMAP 8.2.5). `Fallback::None` is the schema value that would allow an empty page. |

## Never return an empty page

Never returning an empty page is a product rule (SPEC §1, §7.6), not an optional plugin. It is not finished.

What can return hits today, through `search_relaxed`:

- The exact query, when enough documents hold every required term.
- A looser L2 step, when the query has at least two required terms and `minimum_should_match` of that step is low enough that some documents hold that many of them. A typo is not rewritten. Documents that hold the other terms can still match. [How terms are dropped](#how-terms-are-dropped) is why the response text and the hits are not the same set of words.

What still comes back empty:

- A query of one required term that matches nothing. L2 skips it (ROADMAP 8.2.2.3). Fuzzy and the fallback build no step, so there is nowhere left to go.
- A multi-term query whose documents do not hold enough of the terms for the rungs L2 actually builds.
- Any miss that the design would have caught with synonyms, a prefix, a fuzzy correction, or the static-rank fallback. Those levels build no step.
- A query with `RelaxOptions::disabled`, or a collection whose `RelaxationConfig.enabled` is false. L0 only, and an exact miss stays empty.

Filters are not a way out of an empty page. [minimum_should_match](#minimum_should_match) keeps them on every L2 step. `FilterRelaxation::LastResort` (`relax.filters: "last_resort"`) does not run (ROADMAP 8.2.6).

The quality gate is not written either: a synthetic typo suite, a zero-result rate before and after relaxation, and a regression on that rate (ROADMAP 8.4). A tuning guide is planned as ROADMAP 8.4.4 (`docs/relaxation.md`). This project does not publish a zero-result rate.

There is still no search on the database handle. `persia::Db` is not in the crate. `Database` has `open`, `put`, and `commit`, and no `search`. The calls are `CollectionViews::search` and `CollectionViews::search_relaxed` on a snapshot. [Getting started](/guide/getting-started/) separates that from the illustrative `Db::search` samples.

## minimum_should_match

`BoolQuery.minimum_should_match` is a count of `should` clauses a document must match, beside every `must` and `filter` (SPEC §7.1, ROADMAP 8.1.6). The default is 0. With 0, a document must match at least one `should` only when there is no `must` or `filter` and there is some `should`. Above 0, it must match that many `should` clauses. More than there are `should` clauses matches nothing. The count changes what matches. It does not change the score. `must` and `should` score. `filter` and `must_not` do not.

L2 is that count, applied as a ladder over the query's required terms (ROADMAP 8.2.2.2). A term is one required clause: of a compiled query, a clause that is not excluded (`-`), whether it is a phrase, a word split in two, or one word over several fields; of a programmatic query, a `must` clause of a root `bool` that has no `should` and a `minimum_should_match` of 0. Any other programmatic query is left alone.

The rungs after L0, whose count is `n`, are 75%, then 50%, then one term. A percentage `p` is the count `max(1, floor(p · n / 100))`. A count that repeats the previous rung is not built again, and 100% is L0, so it is never a `drop_terms` step. Two terms therefore produce one step, `drop_terms:75%`, with `minimum_should_match` 1. `applied` is `drop_terms:75%`, `drop_terms:50%`, or `drop_terms:1`, for the rung that first produced that count.

Each step is a `bool` whose `should` clauses are those terms, most important first, with that `minimum_should_match`, plus L0's `must_not` and `filter`. Excluded clauses stay excluded. Filters are not relaxed. A query of fewer than two terms skips the level: no count is below L0's, and dropping the only term would match every document the filters and excluded clauses leave.

A long query can still lose a later rung. Each step excludes the earlier steps, and that repeated exclusion can blow past `max_terms` (ROADMAP 8.2.2.4, not done). A skipped step is named in `applied` and is not searched.

## How terms are dropped

Importance decides which terms a step reports as kept, not which documents match (ROADMAP 8.2.2.1). It compares lexicographically, with nothing to tune: a stopword is less important than any other term, then a lower IDF, then a later position in the query. Positions do not tie. The last word tends to qualify the earlier ones, so `red running shoes` keeps `shoes` over `red` when the two are equally rare.

IDF is about the BM25F figure, over the statistics the search scores with. A `term` uses its own document frequency. A `combined` clause uses the largest `df` of its fields. A `phrase` sums its terms. A `bool` sums its `must` clauses and adds the largest of its `should` clauses. `boost` and `constant_score` use their inner query. `prefix`, `fuzzy`, and `match_all` are 0, the least important of a programmatic query's terms. `filter` and `must_not` do not score, so they do not add.

The step's `effective` text is the `k` most important terms and every excluded clause, in text order, one space apart. That string names what the step keeps. It is not a query that compiles to the step. The step matches any `k` of the terms. A programmatic query's step has no `effective`.

A term no document holds (`df` 0, often a typo) currently ranks as important, so `effective` reports it kept unless it is a stopword. A stopword is still dropped first. Whether a missing term should rank last is open (ROADMAP 8.2.2.5). The hits do not have to hold that term: `minimum_should_match` is a count of any of the terms. For `runing shoes`, L0 requires both words and matches nothing if `runing` is absent. The one L2 step requires one of the two. Documents that hold `shoes` can come back. The response does not say the typo was corrected, and `corrected` is empty.

## Compiling a query

Before any relaxation, user text becomes a query (SPEC §7.1, §7.1.1). That compiler is in the tree (ROADMAP 7.1). It compiles text. `CollectionViews::search` is what returns hits for the exact query. `CollectionViews::search_relaxed` is what may then loosen it.

`QueryCompiler::compile` never fails on user text. Syntax that does not parse is read as plain text. Clauses are split on whitespace and all required (default AND). A clause is an optional `-`, an optional `field:`, and a word or a `"phrase"`. `-` excludes the documents the clause would match. `field:` limits it to that `text` or `keyword` field. A `"` without a closing quote is an ordinary character. Text past `max_text_len` (default 8 KiB) is left out on a clause boundary, so a cut word does not become a required term that matches nothing. With no clause left, the query is `match_all`.

Words are analyzed with each field's query-time analyzer. Fields that agree on token spans become one `combined` clause per span: one query word, several fields, scored as one BM25F term. A phrase on a field with positions stays a phrase. On a field without positions it becomes a boolean that requires each token.

Filters are a separate JSON object (SPEC §7.1.2): `term`, `terms`, `range`, `exists`, `and`, `or`, and `not`. They are written by programs, so a bad filter is an error. User text is not. `Filter::from_json` parses and validates them against the schema. Evaluating a filter against a segment or the memtable is library code (ROADMAP 7.4.5). [Faceted search](/guide/faceted-search/) is that path.

Limits (SPEC §7.1.3), implemented as `QueryLimits`, reject a hand-built query or a filter that exceeds them. Defaults: 1024 leaf terms, 1024 clauses, 1024 expansions summed across prefix and fuzzy clauses, depth 32, and 8 KiB of `q`. The prefix and fuzzy clauses the engine itself would build use a per-clause cap of 50 (`DEFAULT_MAX_EXPANSIONS`). `RelaxPlan::with_limits` applies those limits to steps after L0, as [query relaxation](#query-relaxation) describes.

This is library code in `persia-engine`. It compiles text. Opening a database and returning hits are `Database` and `CollectionViews`, not this compiler:

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

The AST can also represent `prefix` and `fuzzy` clauses. `Query::validate` accepts a fuzzy distance of 1 or 2. The compiler does not turn a typo into one of those clauses. A typo stays the term the analyzer produced, and under default AND it is required at L0. Fuzzy matching as a cascade level is L4, which is not written. An explicit `prefix` or `fuzzy` clause plans with `SearchPlan::with_terms`, which expands it over the collection terms (ROADMAP 7.4.12). L0 of `search_relaxed` searches the query it was given, so that explicit clause runs as the exact step. It does not make L4 run: the cascade builds L4's step itself, and that transform is empty. Matching, sorts, pages, and facets over a collection snapshot are library code (`CollectionViews::search`). There is no search call on the database handle.

## BM25F scoring and filters

Ranking is BM25F (SPEC §7.2). For a term `t` and a document `d`, over fields `f` with weights `w_f`:

```text
tf~(t,d) = Σ_f  w_f · tf(t,d,f) / (1 − b_f + b_f · len_f(d) / avglen_f)
score(t,d) = idf(t) · tf~ · (k1 + 1) / (k1 + tf~)
idf(t) = ln(1 + (N − df + 0.5) / (df + 0.5))
```

`N`, `df`, and `avglen` are collection-level. `TermScorer` computes one query word from those statistics plus a document's term frequency and norm byte in each field. A `combined` clause is one word over several `text` fields; `df` is the max of the per-field document frequencies. A `term` on a `keyword` field uses weight 1, `b` 0, and `k1` 1.2. The total adds a static rank when the schema has one: `weight · log1p(value)` or `weight · linear(value)` (SPEC §6.2.3). `must` and `should` clauses score. `filter` and `must_not` clauses do not, so a filter never inflates BM25F. An L2 step scores the terms a document actually matched.

`StatisticsProvider` is the trait those numbers come from. `SegmentStatistics` sums the segments of one snapshot, deleted documents included until a merge drops them. `SearchStatistics` adds the memtable's live documents. A snapshot computes the segment part once. L2 reads the same statistics for term importance.

The same function, given a block's maximum term frequency and minimum norm, is an upper bound used by MaxScore and block-max WAND (SPEC §7.3, ROADMAP 7.3.3 and 7.4.4) to skip documents that cannot reach the top k.

`persia-engine/tests/reference` evaluates the formula on the quantized field lengths. The scorer must match it within `1e-9 · idf·(k1+1)` (SPEC §7.2, ROADMAP 7.3.4). A differential suite over random corpora and full queries compares ranked order to that reference (ROADMAP 7.8.1). An nDCG gate in CI is still open (ROADMAP 7.8.4). [SPEC §16.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#16-quality-requirements) lists latency figures and labels them as targets, not promises. This project does not publish search benchmarks.

`TermScorer` scores one query word. Walking postings, applying tombstones, collecting a top-k, filtering, faceting, and sorting are the search path. %%MATCHING_CELL%%

## What runs today

| Piece | Status |
|---|---|
| User-text compiler, query AST, filter DSL, query limits | Library code (ROADMAP 7.1). |
| Collection statistics | Library code (ROADMAP 7.2). |
| BM25F scorer, static rank, score upper bounds | Library code (ROADMAP 7.3), checked against the reference scorer. |
| `RelaxationConfig`, `RelaxPlan`, `RelaxOptions` | Library code. Schema JSON parses the config. `RelaxPlan::new` applies a query's options over it. |
| `SynonymMap` | Looked up in `persia-analysis` (ROADMAP 4.2.5). The L1 cascade level does not call it (ROADMAP 8.2.1). |
| `BoolQuery.minimum_should_match` and L2 `drop_terms` | Library code (ROADMAP 8.1.6, 8.2.2.1–8.2.2.3). |
| Term, phrase, boolean, prefix, and fuzzy matching | %%MATCHING_CELL%% An explicit prefix or fuzzy clause expands (ROADMAP 7.4.12). L3 and L4 of the cascade do not. |
| Filter evaluation, facets, sort, explain | %%MATCHING_CELL%% Explain is library code (ROADMAP 7.7). Which relaxation level a hit matched is not part of the explanation yet (ROADMAP 8.3.4). `RelaxedHit.level` already says it. |
| Relaxation cascade | %%RELAXATION_CELL%% |
| `persia::Db::open` and `search` | Not in the crate (M10). |

`persia_engine::db::Database` has `open`, `put`, and `commit`; searching goes through `CollectionViews::search` or `search_relaxed` on a snapshot, not through `Database`. The public `persia` crate exports no `Db`. %%ENGINE_CALLS%%

The call below is the intended API from SPEC §7.1 and §7.6. It is illustrative: these methods are not in the `persia` crate, and the fuzzy correction in the comment is not what the engine runs.

```rust
// ILLUSTRATIVE — these methods are not in the persia crate.
// The engine cascade does not rewrite a typo. See the sections above.
let db = persia::Db::open("app.persia")?;
let products = db.create_collection(schema)?;
products.put(doc)?;
products.commit()?;

let results = products.search("runing shoes")?;
let relaxation = &results.relaxation;
```

[Getting started](/guide/getting-started/) separates that sketch from the crates you can depend on. [Code](/guide/code/) links the modules that exist. The planned method list is the [embedded API](/guide/embedded-api/).

## FAQ

### Does Persia DB do typo-tolerant search today?

Partly. The compiler does not turn a typo into a fuzzy clause, and L4 fuzzy does not build a query, so a typo is not rewritten. An explicit fuzzy or prefix clause expands over the collection terms (ROADMAP 7.4.12). L2 `drop_terms` can return hits for a query of two or more terms by requiring fewer of them. A one-term typo still returns no hits. %%RELAXATION_CELL%% `CollectionViews::search` and `CollectionViews::search_relaxed` run over a collection snapshot. There is no search call on the database handle.

### What is query relaxation?

The cascade in SPEC §7.6. `CollectionViews::search_relaxed` walks a `RelaxPlan` from the exact query, then the later levels, and stops at the first level with enough hits (`min_results`, default 1) unless `fill` is set. Hits from a stricter step rank above hits from a looser one. The response is a `Relaxation` value. %%RELAXATION_NOW%%

### Are filters relaxed when a search has zero results?

Not by default. SPEC §7.6 keeps filters strict unless `relax.filters` is `"last_resort"`. `FilterRelaxation::LastResort` does not run (ROADMAP 8.2.6). L2 `drop_terms` copies the original filter onto every step. The filter DSL parses and validates (`term`, `terms`, `range`, `exists`, `and`, `or`, `not`). Evaluating a filter against documents is library code (ROADMAP 7.4.5). Filter clauses do not add to the BM25F score.

### Is BM25F scoring implemented?

The formula is. `TermScorer` scores one query word from a document frequency, a term frequency, and a norm byte, using collection statistics. Static rank and score upper bounds are in the same module. Tests check it against a reference scorer. `CollectionViews::search` walks postings and returns a ranked page. This project does not publish search benchmarks.

### Can I call Db::open and search for a fuzzy query?

No. `persia::Db` is not in the crate, and `Database` has no `search` method. `Database::open`, `put`, and `commit` exist. Ranked search is `CollectionViews::search`, and the cascade is `CollectionViews::search_relaxed`. Code samples that call `Db::search` on this site are illustrative.

## Read next

- [Faceted search](/guide/faceted-search/) — filters, facets, sort, cursor pages, deadlines, and explain. The deadline is not the relaxation budget.
- [What is an embedded search database?](/guide/embedded-search/) — in-process full-text search, one file, and no server.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
- [Feature reference](/guide/features/) — the cascade table, the schema sketch, and the status of each capability.
- [Features](/features/) — the short status table, including the relaxation diagram.
- [Getting started](/guide/getting-started/) — crates you can call, and the API that is still illustrative.
- [FAQ](/guide/faq/) — production readiness, the file format, crash safety, and the license.
- [How it works](/architecture/) — the query path from analyzed text to the cascade.
- [Roadmap](/roadmap/) — milestone status. The live counter is PROGRESS.md.
