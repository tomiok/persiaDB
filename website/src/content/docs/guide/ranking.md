---
title: "BM25F ranking"
description: "How PersiaDB scores search in Rust with BM25F: field boosts, collection statistics, static rank, score-led top-k pruning, and proximity off by default."
head:
  - tag: title
    content: "BM25F ranking in Rust: how PersiaDB scores search results | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "BM25F ranking in Rust: how PersiaDB scores search results | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "BM25F ranking in Rust: how PersiaDB scores search results | PersiaDB"
  - tag: script
    attrs:
      type: application/ld+json
    content: |
      {
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "name": "BM25F ranking",
        "url": "https://tomiok.github.io/persiaDB/guide/ranking/",
        "mainEntity": [
          {
            "@type": "Question",
            "name": "Does Persia DB rank search results with BM25F in Rust today?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "As library code, yes. TermScorer scores one query word from collection statistics, a term frequency, and a norm byte. CollectionViews::search returns a page over a collection snapshot. The default order is that score, descending. There is no search method on persia::Db or on Database. %%MATCHING_CELL%%"
            }
          },
          {
            "@type": "Question",
            "name": "What statistics does BM25F use?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Document count N, per-term document frequency df, and average field length. They are collection-level: each segment's stored counts, deleted documents included until a merge drops them, plus the memtable's live documents. A snapshot computes the segment part once. Summing the same numbers across shards is specified as global_stats and is not written (milestone M13)."
            }
          },
          {
            "@type": "Question",
            "name": "Do field boosts change the score?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes. A text field's boost is its BM25F weight, default 1.0, and must be greater than 0 and at most 1000. k1 defaults to 1.2 and b to 0.75. A query fields entry can replace the boost for the words it searches. A keyword term uses weight 1, b 0, and k1 1.2. boost, k1, and b are query-time settings: changing them does not change the schema hash."
            }
          },
          {
            "@type": "Question",
            "name": "Is term proximity on by default?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. SearchPlan is built with the bonus off. SearchPlan::with_proximity turns it on for adjacent required words in text fields that store positions. A relaxed search plans every step without it. A request has no proximity field (ROADMAP 10.1.7). It stays opt-in until judged queries show a gain (ROADMAP 15.1.6, ADR-0036)."
            }
          },
          {
            "@type": "Question",
            "name": "How do typos and sorts change the order?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "On a relaxed search, hits compare by cascade step first, then by the request sort. A stricter step ranks above a looser one at any score. Typo tiers are those steps. They do not change a BM25F score. Inside a fuzzy step, the edit-distance weight still does. The default sort is score descending, then segment order with the memtable last, then docid. L5 fallback ignores that sort and ranks by static rank, then by where the index holds the document. %%RELAXATION_CELL%%"
            }
          },
          {
            "@type": "Question",
            "name": "Can I call Db::open and read a BM25F score?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. persia::Db is not in the crate, and Database has no search method. The engine call is CollectionViews::search on a snapshot. explain true attaches a per-hit score tree whose root score is the hit's. Code samples that call Db::search on this site are illustrative."
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

BM25F is how Persia DB scores a word that appears in more than one field ([SPEC §7.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#72-scoring-bm25f)). Each `text` field has its own weight, saturation, and length normalization. The length-normalized frequencies are summed across fields first, and the saturation step runs once on that sum, so a title and a body need not count the same word the same way.

%%MATCHING_CELL%% There is no `search` on `persia::Db` or on `Database` (M10). `persia::Db` is not in the crate. `persia_engine::db::Database` has `open`, `put`, and `commit`, and no `search`. The call that returns a ranked page is `CollectionViews::search` (or `search_with`, when you pass threads, a deadline, or cancellation) on a collection snapshot. `TermScorer` is the per-word formula. Filters, facets, the request sort, cursors, and `explain` are [Faceted search](/guide/faceted-search/). How a typo moves a hit above or below another is [Typo-tolerant search](/guide/typo-tolerant-search/), [Fuzzy search](/guide/fuzzy-search/), and [Synonym and prefix search](/guide/synonym-and-prefix-search/).

## How a word is scored

For a term `t` and a document `d`, over fields `f` with weights `w_f`:

```text
tf~(t,d) = Σ_f  w_f · tf(t,d,f) / (1 − b_f + b_f · len_f(d) / avglen_f)
score(t,d) = idf(t) · tf~ · (k1 + 1) / (k1 + tf~)
idf(t) = ln(1 + (N − df + 0.5) / (df + 0.5))
```

`TermScorer` evaluates the score as `idf·(k1+1) − idf·(k1+1)·k1 / (k1 + tf~)`, then takes `max(0, …)`. Every floating-point step of that form is monotonic in `tf~`, so a block's maximum frequency and minimum norm are an upper bound of every score in the block. A positive `tf~` can still round to 0. Whether a document matched is which postings it is on. A score of 0 is also what a document that misses the term scores.

`k1` is the fields' weighted mean `Σ w_f · k1_f / Σ w_f`, or their common `k1` when they agree. `idf` uses `df` at most `N`, so it stays positive.

The reference scorer in `persia-engine/tests/reference` evaluates the formula on the quantized lengths. The engine must match it within `1e-9 · idf·(k1+1)` (SPEC §7.2, ROADMAP 7.3.4). A differential suite compares ranked order to that reference (ROADMAP 7.8.1, 7.8.6). An nDCG@10 gate runs in CI against a recorded baseline, with the default plan, proximity off (ROADMAP 7.8.4, [SPEC §16.1](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#161-testing-normative)). The tolerance is one unit of the baseline's last recorded digit. This page does not publish that baseline. [SPEC §16.2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#162-performance-targets-to-be-validated-by-benchmarks--not-promises) lists latency targets and labels them as targets, not promises.

## Field weights

A `text` field's `boost` is `w_f`. The default is `1.0`. The legal range is `0 < boost ≤ 1000`. `k1` defaults to `1.2` (`0 ≤ k1 ≤ 10`). `b` defaults to `0.75` (`0 ≤ b ≤ 1`). `positions` defaults to false (SPEC §6.2.2).

```json
{
  "title": { "type": "text", "boost": 3.0, "k1": 1.2, "b": 0.75, "positions": true },
  "body": { "type": "text", "boost": 1.0 },
  "sales": { "type": "i64" }
}
```

A query's `fields` list can replace that boost for the words it searches: `"title"` keeps the schema boost, `"title^3"` sets the weight (SPEC §7.1). The same range applies. A `combined` clause is one query word over several `text` fields, each with its own analyzed term and its own weight. Its `df` is the largest of those fields' document frequencies.

`boost`, `k1`, `b`, and `ranking` are query-time settings. Changing them does not change the schema hash, so a collection can be tuned without a reindex (SPEC §6.2.4, §6.2.5). `positions` is part of the hash: turning it on indexes new bytes and needs a new collection.

A `term` on a `keyword` field uses weight `1`, `b` `0` (no norms), and `k1` `1.2`. A document that holds the value once scores `idf`. One that holds it `n` times has term frequency `n`. A keyword has no boost of its own. Facets count keyword fields. A filter scores nothing, whatever field it reads. The operators and the join are [Filtered full-text search](/guide/filtered-search/).

## Collection statistics

`N`, `df`, and `avglen` are collection-level (SPEC §7.2, ROADMAP 7.2).

| Statistic | What it counts |
|---|---|
| `N` | The segments' `doc_count` values, plus the memtable's live documents. |
| `df` | Documents holding the term: the segments' term `df`, plus the memtable's live documents that hold it. At most `N`. |
| `avglen_f` | `sum_field_len / docs_with_field` over documents that have the field. `1` when none has it. |

Segment statistics include deleted documents until a merge drops them (SPEC §5.5). A merge of segments with no deletions leaves every score unchanged. A merge that drops deletions can move scores. The memtable adds only its live documents, the same documents the segment it flushes to will count. A snapshot computes the segment half once (`SegmentStatistics`). A search adds the memtable view (`SearchStatistics`).

`StatisticsProvider` is the trait the scorer reads. Summing the same numbers across shards is `global_stats: true` (SPEC §11.5). That provider is milestone M13 and is not written. A search today scores with the snapshot it is given.

## Clauses

`must` and `should` clauses score. `filter` and `must_not` score nothing, so a filter does not inflate BM25F. A `bool` sums its `must` clauses, then its proximity bonus when the plan asked for one, then the `should` clauses the document matched, in query order. A document's score does not depend on which segment holds it. `boost` multiplies its query's score. `constant_score` gives every match its constant. `match_all` scores 0.

A `phrase` is one term over its field, with the schema `boost` as the weight. Term frequency is the phrase frequency: distinct starts where every term sits at the right offset. `idf` is the sum of the terms' own `idf` values. A phrase of one term scores as a `term` query. When an analyzer stacks a token on itself at one position, a start counts once. A phrase needs positions. On a field without them, the compiler requires each token instead of scoring a phrase. Phrases are exact positions; the proximity bonus below is a separate, optional signal on required words.

A `prefix` or a `fuzzy` scores the sum of the terms it kept. They share one `idf`, from the document frequency of the most frequent term kept. A `fuzzy` term is then multiplied by `(distance + 1 − d) / (distance + 1)`, where `d` is that term's edit distance. An exact term weighs 1. Each further edit weighs strictly less. A prefix is not multiplied that way. How those terms are chosen is [Fuzzy search](/guide/fuzzy-search/) and [Synonym and prefix search](/guide/synonym-and-prefix-search/). A prefix uses the field's schema `boost` as its weight. When the query's field weight differs from that boost, the prefix sits under a boost of their ratio, clamped into the legal boost range. [Synonym and prefix search](/guide/synonym-and-prefix-search/) is that case.

## Field length

`len_f(d)` is the length the norm byte stores, not the exact token count (SPEC §5.3.4, §7.2). Lengths below 40 are exact. From 40 tokens on, the stored length is up to 12.5% lower. `TermScorer` keeps a 256-entry table per field, one factor `w_f / (1 − b_f + b_f · len/avglen)` per norm byte. The denominator is at least `f64::EPSILON`, so a damaged norm byte does not divide by zero. A smaller norm byte is a shorter field, and a shorter field scores higher when `b` is above 0. A `keyword` has no norm byte.

## Static rank

SPEC §6.2.3 and §7.2 define a static rank the collection can add to a text score: `weight · fn(v)`. `field` is an `i64`, `f64`, or `datetime`. `weight` defaults to `1.0` (`0 ≤ weight ≤ 1000`). `fn` is `"log1p"` (the default) or `"linear"`. Both functions see `max(v, 0)`. `log1p` is `ln(1 + max(v, 0))`. `linear` is `max(v, 0)`. A `datetime` is microseconds since the epoch. A missing field, or a value that is not finite, contributes 0. A product above `f64::MAX` saturates at `f64::MAX`. The contribution is finite and never negative. `score::static_rank` is that function (ROADMAP 7.3.2). A `saturation` function, `v / (v + pivot)`, is ROADMAP 27.6.1 and is not in the schema.

```json
{ "ranking": { "static_rank": { "field": "sales", "weight": 0.1, "fn": "log1p" } } }
```

SPEC §7.2 writes the total as the sum of the term scores plus this contribution. The score on a hit from `CollectionViews::search` is the query score from the matcher: the clause sum, and the proximity bonus when the plan asked for it. That path does not call `static_rank`.

L5 fallback has no query words, so its hit score is the contribution (ROADMAP 8.2.5). Ties break by index location, most recent first: the memtable, then segments from the last the manifest lists to the first, each by docid descending. That recency is where the index holds the document, not when it was written. With no static rank, or with `weight` 0, every L5 score is 0, recency alone orders the hits, and the response sets `fallback` to `recency`. Otherwise it sets `static_rank`. The request's `sort` does not order these hits. Each hit still carries the request's sort values, and a `score` key on an L5 hit is this contribution. [Fuzzy search](/guide/fuzzy-search/#fallback-when-nothing-matches) is the rest of L5.

## Score bounds and top-k

Per view, the matcher walks postings and keeps a top-k. The default strategy is block-max WAND (`Pruning::BlockMaxWand`). `Pruning::MaxScore` uses whole-list bounds only (SPEC §7.3, ROADMAP 7.3.3, 7.4.4). A term or a block stores `max_tf` and `min_norm`. Substituted for a document's frequency and norm, they are an upper bound of every score in that range, because the score is monotonic in `tf~`. A document is skipped only when that bound, summed in query order, is at most the worst hit kept. With further sort keys, only a document scoring strictly below that hit is skipped. Pruned and exhaustive top-k are the same hits, scores bit for bit.

A phrase has no block bounds, so block-max WAND uses its whole-list bound. Clauses under `constant_score` are walked exhaustively. A `bool` with several `must` clauses, or with `should` beside `must`, takes the threshold itself and sums its clauses' bounds. With a proximity bonus, that sum includes the bonus's upper bound, so the skip stays exact. Scoring a document whose frequency is above its block's `max_tf`, or whose norm is below its block's `min_norm`, is corruption: the search fails instead of ranking that document above a bound another document was skipped by. Blocks that pruning skips are not read. `Segment::verify` checks those bounds.

Tombstones are dropped by the postings iterator before a document is scored, so a deleted document cannot enter the heap or raise the threshold. Block bounds may still cover deleted documents. That only prunes less.

With a facet, every match is visited, and pruning does not apply. Without one, a score-led sort may skip documents that cannot enter the page. Each view keeps its own top-k. Views merge in manifest order, the memtable last, so the page does not depend on which view finishes first. A view can share the worst score it is keeping so another view skips more. The rules for which ties that share may drop are SPEC §7.3.

## Sort and tie-break

The default sort is score descending. Every hit carries its score, including when the sort is a column. After the sort keys, ties break by segment order (the manifest's), the memtable last, then docid (SPEC §7.3, §7.4). Inside one view, under the score alone, the same rule is score descending, then docid ascending, because the view is walked in docid order. Scores compare with `-0.0` equal to `0.0`.

A `Sort` has at most 16 keys. A key is the score, or a field with a column: `keyword`, `i64`, `f64`, `datetime`, or `bool`. `text` and `stored` have no column. `order` is ascending or descending. `missing` is `last` (the default) or `first`.

Pruning applies when the first key is the score descending. A later key, such as score then a timestamp, can still reorder a tie, so only a document scoring strictly below the worst kept hit is skipped. Its later columns are not read. A numeric or `datetime` key with `missing: last` can skip FOR blocks whose keys all rank after the worst hit that view keeps. A sort led by the score ascending, by a keyword or bool, or by a column with `missing: first` visits every match. Either way the hits are the ones a full scan would keep.

A collection-wide default tie-break would be a schema option and a new manifest version. It is not part of v1 (ROADMAP 32.5.1, with the collection proximity weight). The request shape, cursors, and deadlines are [Faceted search](/guide/faceted-search/#sort).

On a relaxed search the cascade orders by step first, then by this sort. That interaction is [below](#relaxation-typo-tiers-and-order).

## Term proximity

A `bool` can add a bonus for adjacent required words: two `must` clauses next to each other in query order that are both words, a `term` or a `combined`, and not under `boost` (SPEC §7.2, [ADR-0036](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0036-term-proximity-scoring.md), ROADMAP 7.3.5). In a `text` field that stores positions and holds both terms, an occurrence of the first word at `p` and the second at `q` has distance `q − p` when `q > p`. Reversed, or stacked at one position, costs one more (`p − q + 1`). The two occurrences of the same term must be different positions. The pair uses the smallest such distance. Closeness is `1 / d²` for `d ≤ 5` and 0 beyond. Across fields, the pair keeps the best field. The bonus is the sum, in pair order, of `λ · min(idf of the two words) · closeness`, with `λ = 1`. It is added after the `must` clauses and before the matching `should` clauses. `boost` scales that sum with the `bool`.

The bonus is off unless the caller asks. `SearchPlan::new` leaves it off. `SearchPlan::with_proximity` turns it on. `SearchPlan::without_proximity` turns it off. A single word is not a `bool`, so it is unchanged. The bonus does not apply to a `phrase`, a `should`, a `filter`, a `must_not`, a `prefix`, a `fuzzy`, a `keyword` term, or a field without positions. `λ` and the window of 5 are constants of the scorer. Changing them changes ranking and goes through the nDCG@10 gate (SPEC §16.1).

With `B` the same sum at closeness 1, the bonus is at most `B`. Pruning adds `B` where the score adds the bonus, so a skipped document is still one that cannot beat the threshold. A candidate whose clauses plus `B` cannot beat the threshold is skipped before its positions are read.

A request cannot ask for the bonus. `SearchRequest` has no proximity field (`"proximity"` is ROADMAP 10.1.7). `CollectionViews::search_relaxed` plans every step without it. A cursor records whether its plan scored the bonus. A later page under the other setting is `RequestError::CursorProximity`, because the bonus changes scores and therefore the order (SPEC §7.4). A collection setting such as `ranking.proximity` would be a schema option and a new manifest version. It is ROADMAP 32.5.1, not part of v1.

The bonus stays opt-in until judged queries it can decide show a gain (ROADMAP 15.1.6). SPEC §7.2 records that its measured cost on AND queries buys no nDCG@10 gain on the current judged corpus. This page does not quote that measurement.

This builds a plan. It does not open a database or return hits. It compiles against the current crate:

```rust
use persia_engine::query::{BoolQuery, Query, TermQuery};
use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
use persia_engine::search::SearchPlan;
use persia_engine::stats::SegmentStatistics;
use std::sync::Arc;

let text = FieldKind::Text(TextOptions {
    positions: true,
    ..TextOptions::default()
});
let schema = Arc::new(SchemaBuilder::new("c").field("body", text).build()?);
let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
let term = |t: &str| Query::Term(TermQuery { field: body, term: t.into() });
let and = Query::Bool(BoolQuery {
    must: vec![term("red"), term("shoes")],
    ..BoolQuery::default()
});
let plan = SearchPlan::new(&and, &schema, &SegmentStatistics::new(&schema, &[]))?;
assert!(!plan.proximity());
let plan = plan.with_proximity();
assert!(plan.proximity());
```

## Explain

`SearchRequest.explain` defaults to false. Set it and each hit on the page carries an `Explanation` (SPEC §7.7, ROADMAP 7.7). The tree follows the query and keeps the clauses that matched that document.

| Node | What it records |
|---|---|
| Word | `idf`, `k1`, `tf~`, and per field that holds the word: the field, the analyzed term, the frequency, the norm byte, the length it stands for, the factor, and `tf · factor`. |
| Phrase | Its terms, phrase frequency, norm byte, length, factor, `tf~`, `k1`, and the summed `idf`. |
| Bool | Every `must` clause, then the proximity bonus when the plan scored pairs, then the `should` clauses that matched. |
| Boost | Its factor. For a fuzzy term, that factor is the edit-distance weight. |
| Constant | Its score. |
| `match_all` | 0. |
| Static rank | An L5 hit: the contribution, and the field value it came from. `None` when the document lacks it, or when the collection has no static rank. |

Filters and `must_not` are left out. They score nothing. The root score is the hit's score, bit for bit: the same postings and norms, summed in the same order. Explain covers the page's hits only, about one unpruned search of each view that holds a hit on the page. A deadline is read during that pass too.

There is no JSON search response on `persia::Db`. The Rust value is `persia_engine::search::Explanation`. The JSON of that tree is specified to arrive with the request and response JSON (ROADMAP 10.1). Highlighted snippets are not this tree. Highlights are post-v1.

A hit of `search_relaxed` wraps that tree in `Explanation::Relaxed` (ROADMAP 8.3.4). `RelaxedExplanation` names the `level`, the `step` (0 is L0), and that step's `applied` entry. L0 has no `applied`. The node's score is the inner tree's. L5's inner node is `Explanation::StaticRank`. `RelaxedHit.level` states the same level on every hit, explained or not.

## Relaxation, typo tiers, and order

Within one step, BM25F orders the hits, then the request sort breaks ties. Across steps, the step comes first (ROADMAP 8.1.3). Every hit of a stricter step ranks above every hit of a looser one, at any score. No offset added to an `f64` can do that for every score, so the engine does not add one. Each hit keeps its step's own score.

`RelaxedHit.rank_score` is a separate number for a client that sorts by one value: `2^−(step+1) · (1 + g(score))`, where `g(s) = 1 − 1/(1 + s)`, rounded at each step (`relax::rank_score`). Step `i`'s rank scores lie in `[2^−(i+1), 2^−i]`. The page order is still step, then the request sort. The rank score can tie where two scores round alike, and where one step's lowest meets the next step's highest. The hit's `step` is what separates those.

Typo and exactness tiers are extra steps of that cascade, strictest first (ROADMAP 8.2.7). They add nothing to the score. A word matched as typed ranks above one matched by a completion because it is an earlier step, before BM25F. Inside a fuzzy step, the edit-distance weight still multiplies the term, so an exact term outranks a correction at the same frequency and length. When a rewritten term has 8 or more characters, L4 builds `fuzzy:1` and then `fuzzy`. When L3 expands a term besides the last one, it can build `prefix:last` before `prefix`. L0 does not rank a word above its stem: the index holds the stems alone. The tier pages are [Fuzzy search](/guide/fuzzy-search/#typo-tiers) and [Synonym and prefix search](/guide/synonym-and-prefix-search/#prefix-search).

L1 through L4 and L6 score their words with this formula and then follow the request sort. L5 does not: its order is static rank, then index-location recency, and its hits still sit below every earlier step because the step comes first. L6, the opt-in filter step, ranks by the request sort. %%RELAXATION_CELL%% Each step prunes on its own. Thresholds are not shared across steps.

## What is still planned

| Piece | Status |
|---|---|
| `TermScorer`, collection statistics, score bounds | Library code (ROADMAP 7.2, 7.3). |
| Ranked page over a snapshot | %%MATCHING_CELL%% |
| Proximity bonus | Library code, off unless `SearchPlan::with_proximity` (ROADMAP 7.3.5). |
| `static_rank` as the L5 score | Library code (ROADMAP 7.3.2, 8.2.5). A query hit's score is the query score. |
| `explain` | Library code (ROADMAP 7.7, 8.3.4). |
| `persia::Db::search` | Not in the crate (M10). |
| A request field `"proximity"` | ROADMAP 10.1.7. |
| Proximity on by default | Waits on judged queries (ROADMAP 15.1.6). |
| Collection tie-break and a proximity weight in `ranking` | ROADMAP 32.5.1. Needs a new manifest version. |
| Static rank `saturation` | ROADMAP 27.6.1. |
| `global_stats` across shards | SPEC §11.5. Milestone M13. |
| Highlighted fragments | Post-v1. |
| JSON for the score tree | With the request and response JSON (ROADMAP 10.1). |

## FAQ

### Does Persia DB rank search results with BM25F in Rust today?

As library code, yes. `TermScorer` scores one query word from collection statistics, a term frequency, and a norm byte. `CollectionViews::search` returns a page over a collection snapshot. The default order is that score, descending. There is no `search` method on `persia::Db` or on `Database`. %%MATCHING_CELL%%

### What statistics does BM25F use?

Document count `N`, per-term document frequency, and average field length. They are collection-level: each segment's stored counts, deleted documents included until a merge drops them, plus the memtable's live documents. A snapshot computes the segment part once. Summing the same numbers across shards is `global_stats` and is not written (M13).

### Do field boosts change the score?

Yes. A `text` field's `boost` is its BM25F weight, default `1.0`, in `(0, 1000]`. `k1` defaults to `1.2` and `b` to `0.75`. A query `fields` entry (`"title^3"`) replaces the boost for the words it searches. A `keyword` term uses weight 1, `b` 0, and `k1` 1.2. `boost`, `k1`, and `b` are query-time settings: changing them does not change the schema hash.

### Is term proximity on by default?

No. `SearchPlan` is built with the bonus off. `SearchPlan::with_proximity` turns it on for adjacent required words in `text` fields that store positions. A relaxed search plans every step without it. A request has no proximity field (ROADMAP 10.1.7). It stays opt-in until judged queries show a gain (ROADMAP 15.1.6, ADR-0036).

### How do typos and sorts change the order?

On a relaxed search, hits compare by cascade step first, then by the request sort. A stricter step ranks above a looser one at any score. Typo tiers are those steps. They do not change a BM25F score. Inside a fuzzy step, the edit-distance weight still does. The default sort is score descending, then segment order with the memtable last, then docid. L5 fallback ignores that sort and ranks by static rank, then by where the index holds the document. %%RELAXATION_CELL%%

### Can I call Db::open and read a BM25F score?

No. `persia::Db` is not in the crate, and `Database` has no `search` method. The engine call is `CollectionViews::search` on a snapshot. `explain: true` attaches a per-hit score tree whose root score is the hit's. Code samples that call `Db::search` on this site are illustrative.

## Read next

- [Filtered full-text search](/guide/filtered-search/) — filters restrict hits and add nothing to this score.
- [Faceted search](/guide/faceted-search/) — filters, the request sort, cursor pages, deadlines, and explain. The deadline is not the relaxation budget.
- [Typo-tolerant search](/guide/typo-tolerant-search/) — the cascade, step-first ranking, and `minimum_should_match`.
- [Fuzzy search and did-you-mean](/guide/fuzzy-search/) — the edit-distance weight, typo tiers, and L5 static rank.
- [Synonym and prefix search](/guide/synonym-and-prefix-search/) — L1 groups and L3 prefix expansion, including the prefix field weight.
- [Comparison](/guide/comparison/) — next to SQLite FTS5, Tantivy, and Meilisearch. Not a benchmark.
- [What is an embedded search database?](/guide/embedded-search/) — in-process search, one file, and no `persia::Db` yet.
- [Feature reference](/guide/features/) — the status table and the schema sketch that carries `ranking`.
- [Getting started](/guide/getting-started/) — crates you can call, and the API that is still illustrative.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
