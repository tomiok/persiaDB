# Relaxation tuning guide

Persia DB answers a query that finds too few hits by relaxing it, step by step, until it finds enough: the
*relaxation cascade*. It is on by default, so a search does not come back empty unless the collection, its filters
or your settings leave nothing to return, or the time budget passes first (`truncated`, below). This guide explains the levels, the settings that tune them, how to read
what a response says it did, and how to watch relaxation in production and in tests. The authoritative rules are
in [SPEC §7.6](../SPEC.md) (the cascade), §6.2.3 (the settings), §7.1.3 (limits) and §12.4 (observability). Where this
guide and the SPEC differ, the SPEC is right.

## How the cascade runs

1. **L0 `exact`** runs the query as compiled: every word required, each analyzed as its field does.
2. If L0 finds fewer than `min_results` hits, the next level builds its *steps* (queries) from L0's query and
   searches them, each step excluding every document an earlier step already returned. The cascade stops as soon
   as the hits found reach `min_results` (with `fill`, also the page), or when the levels run out.
3. Every hit of a stricter step ranks above every hit of a looser one, whatever their scores. Each hit says which
   level and step matched it (`level`, `step`), and carries a `rank_score` for clients that order by one number.

A level that has nothing to do for a query (for example L2 for a one-word query) builds no step, so `applied` does not
list it (unlike a step built and then skipped for the limits, listed `:skipped`). Each level builds from L0's query,
not from the previous level's steps.

## The levels

| Level | `applied` | What it does | Builds no step when |
|---|---|---|---|
| L0 `exact` | (none) | All words required, analyzed. | never |
| L1 `synonyms` | `synonyms` | Each run of words that is a synonym term (§6.3.2) may match any term of its groups (at most 50 alternatives per run). | the text has no synonym term, or the query is programmatic |
| L2 `drop_terms` | `drop_terms:75%`, `:50%`, `:1` | `minimum_should_match` steps over the `n` words: `max(1, ⌊p·n/100⌋)` of them for `p` = 75, 50, then 1 word. Words no document holds, stopwords and low-IDF words are the ones reported dropped first. | fewer than 2 required words, or a required clause of a programmatic query holds a filter |
| L3 `prefix` | `prefix:last`, `prefix` | The last word is completed when it has 2 characters or more, and every other word when it has 3 or more (characters of the analyzed term), each to its 50 most frequent completions. `prefix:last` first completes the last word alone, when another word is completed too and L2's last rung did not run before it. | no word qualifies |
| L4 `fuzzy` | `fuzzy:1`, `fuzzy` | Each word of 4 characters or more matches terms within 1 edit (4–7 characters) or 2 edits (8 or more), 50 per word; a word no document holds is reported in `corrected`. `fuzzy:1` first caps every distance at 1 when some word may take 2. | no word has 4 characters |
| L5 `fallback` | `fallback` | The collection's own ranking of the documents every filter of L0 keeps (hard and soft), the excluded words do not match and no earlier step returned: by static rank (§6.2.3), then recency. | `fallback: "none"`, or a filter sits in a clause L5 drops (a programmatic query's filtered `must` clause), which it could not apply |
| L6 `filters` | `filters` | L0's query without its soft filters (below), the hard ones kept, only when every earlier step found fewer than `min_results` hits. | `filters` is not `"last_resort"`, nothing to drop, or no word searched |

Excluded words (`-word`) are never relaxed: every level keeps them excluded, L5 and L6 included. Phrases (`"..."`)
and `field:` clauses ask for their text exactly, so L3 and L4 never complete or correct them; L2 may still drop one,
as it drops a word.

A step that does not fit the engine's query limits (§7.1.3: 1 024 terms, clauses and expansions by default) is cut
(`:cut`, fewer expansions or alternatives) or skipped (`:skipped`). Each step counts the steps it excludes against
those limits, so a long query late in the cascade is cut sooner than the same step alone.

## The settings

A collection sets the defaults in its definition (`relaxation`, §6.2.3); a query overrides them key by key with
`relax` (§7.6). `relax: false` runs L0 alone.

| Key | Default | What it changes |
|---|---|---|
| `enabled` | `true` | `false` runs L0 alone. |
| `min_results` | `1` | How many hits stop the cascade (1–10 000). With 1, a query relaxes only when it has no hit. A larger value relaxes queries that found a few hits, adding looser hits below them. |
| `levels` | `"default"` | Which of L1–L4 run, as a list in cascade order (`["synonyms", "drop_terms", "prefix", "fuzzy"]` is the default). The order cannot change; leave a level out to skip it. L0 always runs; L5 is `fallback`. |
| `fallback` | `"static_rank"` | `"none"` removes L5: a query whose words match nothing anywhere may come back empty. |
| `fill` | `false` | `true` keeps building steps until the page is full (`offset + limit` hits), not only until `min_results`. A page then may show looser hits under strict ones; a deeper page may relax further than the first. |
| `filters` | `"never"` | `"last_resort"` plans L6. |
| `budget_ms` | `50` | Time the cascade may spend after L0 (1–60 000). When it passes, the response holds the steps completed so far and `truncated: true`. |

### Filters: `filter` and `soft_filter`

A request's `filter` is **hard**: no level ever drops it, L6 included. Put tenancy, permissions and any other
constraint that must hold there. A `soft_filter` is a preference: L6 (`filters: "last_resort"`) may drop it, and
only it, when every other step found fewer than `min_results` hits. Without `last_resort`, a `soft_filter` holds at
every level like a `filter`. When L6 found the hits, the response says `filters_dropped: true`, and each such hit has
`level: "filters"`.

Every level but L6 keeps every filter L0 applies, hard and soft: L5 ranks only the documents they all keep, and L2
and L5 build no step rather than drop a clause that holds a filter. A programmatic query's root filters are hard
unless its caller marks them soft (`RelaxedQuery::with_soft_filter`). A query put in place of a compiled one
(`Compiled::with_query`) is joined to the request's `filter` and `soft_filter` again, and the filters its own root
holds are the caller's, so hard, also one equal to the `soft_filter`.

### Choosing settings

- **Keep the defaults** for general search boxes: no empty pages, and the response says how each hit was found.
- **Strict catalogs or legal search** (no hit beats a wrong hit): `fallback: "none"`, and consider leaving out
  `drop_terms`, which returns documents missing some of the words. Watch the still-empty rate (below).
- **Typo-heavy input** (mobile, voice): the default levels run L2 before L4, so a query of several words with one typo
  is often answered by dropping the misspelled word rather than correcting it: L2's one-word rung finds hits, and L4
  never runs. On the larger synthetic suite (below: `--features slow`, 5 seeds, the `typo 2w+` rows), the first
  relaxed hit of a typo query of two or three words holds every intended word 51.6–61.8% of the time with the default
  levels, and 100% with `levels: ["synonyms", "prefix", "fuzzy"]` (no `drop_terms`). But without L2, a query with a
  word no document holds beside the others falls through to the fallback, whose first hit holds the intended words
  0.7–1.4% of the time (`drop` rows; 0.8–2.0% for `typo+drop`), against 75.5–83.6% with the default levels. One-word
  typo queries skip L2 and are corrected either way (97.6–100%). The test prints these tables; measure your own
  queries before choosing.
- **Autocomplete-like input**: L3 completes the last word from 2 characters; a single letter stays exact (a one-letter
  completion matches too much to rank).
- **Pages that must be full**: `fill: true`, ranking strict hits first.
- **Latency-sensitive services**: lower `budget_ms`, and watch `truncated`. A truncated cascade can return fewer hits
  than `min_results`, even none, when the budget passes before the level that would have found them (the fallback
  among them).

## Reading a response

`relaxation` states what the cascade did, the same on every page of a query (unless `fill` or the budget changed it):

- `level`: the last level the cascade built and did not skip (`exact` when L0 had enough).
- `applied`: every step after L0, in order, with `:cut` or `:skipped` where the limits changed it.
- `original`: the text as read; `effective`: the text of the *hit step*, the last step that found a hit (at L2 the words
  it kept, at L4 with corrections in place); `corrected`: each word L4 replaced, when L4 is the hit step.
- `fallback`: the order L5 ranked by (`static_rank` or `recency`), when L5 found the hits: the explicit sign that some
  hits do not match the words.
- `filters_dropped`: `true` when L6 found the hits.
- `truncated`: the budget cut a step of this page. A step searched only to count facets does not count (below).

Each hit has its `level`, `step` and `rank_score`. `explain: true` adds which step matched it.

Facets (§7.5) count the matches of every step up to the hit step that a page holding every hit builds, each document
once, at its first step, so every page of a query counts the same, with `fill` or without, by offset or by cursor. L6
counts when it is built: its matches are documents the `soft_filter` rejects. With `fill`, a page builds the steps
past its own hits only to count them; when the budget passes in one of those, the page is not truncated, but its
facets are partial, and the response does not say so.

## Metrics

The engine counts each collection's relaxed *queries* (searches without a cursor: a cursor's page continues a query
already counted) since the database was opened, without a lock:

- `queries`, `relaxed` (the cascade built a step after L0), `empty` (no step found a hit, L0 included);
- per level, the queries whose hit step is of that level (`hits_at`, `levels()`): the level histogram;
- `relaxed_ratio()` and `empty_ratio()`: the share relaxed and the zero-result rate after relaxation.

Read them as `RelaxationStats` from `CollectionSnapshot::relaxation_stats()` or, summed over the collections,
`Database::relaxation_stats()` (a dropped collection's counts stay in the sum). A read-only handle has no database
sum yet; its collections count from 0 again when a refresh reloads it in full (ROADMAP 11.4.8). Every relaxed search
also emits one `tracing` event at debug level, `relaxed search`, with its collection, `level`, `applied`, page `hits`
and `truncated`.

How to read them:

- A high `relaxed_ratio` means users often type what the index does not hold: check analyzers (stemming, folding)
  and synonyms before widening relaxation.
- Many hits at `fallback` mean the word levels rarely match: the results are the collection's ranking, not answers.
- A non-zero `empty_ratio` comes from an empty collection, `fallback: "none"`, filters that keep no document, `relax: false`, a
  truncated cascade (`truncated`), a fallback step skipped for the limits (`fallback:skipped`, after many
  expanded steps it must exclude), or none built for a filter it could not apply (the levels table).

## The quality gate

`crates/persia-engine/tests/relax_quality.rs` measures relaxation on synthetic suites
(`persia_testutil::synthetic`): a corpus of generated words, and queries of 1 to 3 words taken from a document,
then changed so that nothing matches them as typed: one word misspelled (one edit, as L4 undoes), one word added that
no document holds beside the others (as L2 drops), or both. The ground truth of a query is every document holding all
the words it was built from. Each run prints, per kind of query, then split into one-word (`1w`) and longer (`2w+`)
queries, and overall:

- `zero before`: the share with no hit at L0 alone (100% by construction);
- `zero after`: the share with no hit after relaxation;
- `first relaxed ok`: of the queries whose first hit a step after L0 found, the share whose first hit is in the
  ground truth.

The gate fails when (a) with the default settings any query has no hit, (b) with `fallback: "none"` more than 2%
have none, or (c) the default settings' first-relaxed-hit precision falls more than 2 points below its row in
`crates/persia-engine/tests/relax_quality/baseline.txt` (a rise passes). A small suite (200 documents, 150 queries)
runs in every `cargo test`; a larger one (1 000 documents, 600 queries, 5 seeds) under `--features slow`, which also
prints, without gating it, the measure without `drop_terms` (the numbers above). The budget runs on a clock that
never moves, so the result does not depend on the machine. Beside it, `tests/relax_zero_results.rs` checks that every
synthetic query finds a hit with the default settings over several corpus shapes, query lengths and index layouts
(memtable, segments, a reopened database).

What the gate sees: removing L2 or L4 fails it, and so does an L2 or L4 that stops matching what it should. It cannot
see L1 (the synthetic corpus has no synonyms) or L3 removed (L4 corrects the same queries), and with the default
levels no query reaches L5, since L2's one-word rung always finds a hit.

To read a failure: the printed tables show which kind of query moved, and the message names the check. A precision
drop usually means a level now matches documents missing a word the query meant, or ranks them first; a zero-result
failure means a level stopped finding hits it used to. If the change is intended, record the baseline again as the
header of `baseline.txt` says: run the fast and slow gates with `--nocapture`, replace the rows with the ones they
print, and say in the PR which suites moved and why. `PERSIA_QUALITY_SEED` runs the small suite on another seed (its
precision then has no baseline row, so only (a) and (b) are checked).

## Costs

Only measured numbers, from [`docs/benchmarks.md`](./benchmarks.md) (4 segments of 50 000 documents, 10 hits, one
thread): a query whose L0 has enough hits costs the same with relaxation on or off (`search_relaxed/l0` and `off`).
Each relaxed search measured costs more, as its levels plan and search their steps: about 0.9 ms reaching L1, 1.4 ms
reaching an L2 step and 2.6 ms reaching L4 on the bench's queries (`l1`, `l2`, `l4`). Completing a 2-letter last word
walks every term starting with it: the L3 bench of one, at the largest `min_results`, takes 14.9 ms in all
(`l3/two_letters`). A one-letter last word is not completed, so it costs no walk (`l3`: 26 µs, reaching L5).
Compiling a query with a 50-alternative synonym run costs about 19 µs (`compile/synonyms`), its alternatives cached
per set of default fields. Measure your own corpus before tuning for speed.
