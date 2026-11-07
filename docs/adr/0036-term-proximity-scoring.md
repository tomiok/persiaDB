# ADR-0036: Term proximity scoring

- Status: accepted
- Date: 2026-11-07
- Related: SPEC §7.2, §7.3, §7.7; ROADMAP 7.3.5, 7.4.4, 7.4.8, 7.8.4, 5.3.4; ROADMAP 32.5.1 (collection ranking defaults)

## Context

BM25F (SPEC §7.2) scores each query word on its own: `running shoes` ranks a page that says "running" at the top and
"shoes" at the bottom the same as one that says "running shoes". Positions have been indexed since 5.3.4 for every
`text` field with `positions: true`, and phrases already use them. ROADMAP 7.3.5 adds a proximity signal. It must
keep pruning exact: MaxScore, block-max WAND and block-max AND (§7.3) skip a document only when an upper bound of its
score, summed in query order, is at most the threshold. Pruned and exhaustive results must stay the same hits, with
the same scores bit for bit.

Four choices: what to measure (the minimum span of all words, or the distance of word pairs), the bonus formula, the
queries it applies to, and whether it is on by default.

## Decision

1. **Pair distance, not minimum span.** The signal is the distance between *adjacent required words*: two `must`
   clauses of one `bool` that are next to each other in query order and are both words (a `term` on a `text` field,
   or a `combined`, §7.1, not under `boost`: a `fuzzy` term's weight is one). Pairs are what the literature finds most robust (Tao & Zhai 2007: minimum pair distance
   beats span measures; BM25TP, Rasolofo & Savoy 2003; Büttcher et al. 2006). A minimum span needs every word in one
   window of one field, so it says nothing about long queries and nothing when one word is only in another field.
   A pair degrades one pair at a time. It is also cheap: per pair, one merge of two sorted position lists.

2. **Distance.** In a field `f` that holds both words' terms (`t_{i,f}`, `t_{j,f}`, §7.2) and has positions, for an
   occurrence of the first at `p` and of the second at `q`: `d = q − p` when `q > p` (in query order), else
   `d = p − q + 1` (reversed, or stacked at one position, costs one more). Two occurrences of the same term need
   `p ≠ q`. `d_f` is the least `d` over all occurrence pairs. The closeness is `π(d) = 1 / d²` for `d ≤ W`, else 0,
   with `W = 5`: adjacency gives 1, two apart 0.25, beyond five apart nothing. The pair's closeness is the largest
   over the fields that hold both terms (`max_f π(d_f)`), so field weights count once, in BM25F.

3. **Bonus.**

   ```
   bonus(d) = Σ_{adjacent word pairs (i, j), in query order}  (λ · min(idf_i, idf_j)) · max_f π(d_f(i, j))
   ```

   `idf` is §7.2's for the word (its `df` the largest over its fields). `min` makes a pair worth what its rarer partner
   is not: `the shoes` earns little. `λ = 1`: an adjacent pair adds about what a word with frequency 1 at
   average length adds before saturation. It is added to the `bool`'s score after its `must` clauses and before its
   `should` clauses: `score = (Σ must, query order) + bonus + (Σ matching should, query order)`. `boost` scales it
   with its `bool`.

4. **Where it applies.** Only to pairs of required words in a `bool`, which is what user text compiles to (§7.1.1,
   default AND). It does not apply to `should`, `filter` or `must_not` clauses, to a `phrase` (already exact),
   to `prefix`/`fuzzy` expansions, to `keyword` terms, to fields without positions, or under `constant_score`.
   A `bool` with fewer than two adjacent words gets no bonus. A single word query has no `bool` (§7.1.1) and is
   unchanged. Relaxation's levels (M8) build their own queries; whether they get proximity is decided there.

5. **On by default where positions exist.** Positions are opt-in per field (`positions: false` by default,
   §6.2.2), so collections that never asked for them see no change in ranking or cost. A field with positions gets
   proximity. A request's `"proximity": false` (`SearchPlan::without_proximity`: the switch is the plan's, as
   scoring is) turns it off, and the pruning proptest and benchmarks run both ways. A collection-level setting (`ranking.proximity: {"weight": λ}` or `false`)
   would be a schema option, so a new manifest `version` (SPEC §8.7: "a schema option added later is a new
   manifest version"): it is ROADMAP 32.5.1 (v1.1), with 7.4.11's collection tie-break, so 7.3.5 does not change
   the on-disk format.

6. **Pruning stays exact.** Let `B = Σ_pairs (λ · min(idf_i, idf_j))`, in pair order: the bonus with every `π = 1`.
   - Each pair term satisfies `(λ·m)·π ≤ (λ·m)·1 = λ·m`, since `0 ≤ π ≤ 1` and a correctly rounded product is
     monotonic in each factor. Floating-point addition of non-negative terms is monotonic, so summed in the same
     pair order, `bonus ≤ B` exactly, not just within rounding.
   - A `bool`'s whole-list bound (`max_score`) and block bound (`block_bound`) are its clauses' bounds summed in
     query order with `B` added at the bonus's place (after the `must` clauses). Each addend of the bound is at
     least the matching addend of the score, so by monotonicity again, `score ≤ bound` for every document in the
     range, exactly. This is the argument §7.2 already makes for `max_tf`/`min_norm`, with one more addend.
   - Block-max AND (7.4.8) sums the `must` clauses' block bounds, then adds `B`. ReqOpt's "`must` side alone at most
     the threshold" uses `Σ must bounds + B`: a document no `should` clause holds scores `Σ must + bonus`, which is at
     most that.
   - MaxScore and block-max WAND see a `bool` with proximity only as a clause of a disjunction, through its bounds
     above. Their own sums of clause bounds are unchanged.
   - `B` is a constant per `bool` per plan (collection `idf`s), the same in every view, so the cross-view threshold
     rules (§7.3) are unchanged.
   The cost is looser bounds: a range is skipped only when its BM25F bound is at most `threshold − B`. Benchmarks
   measure it (7.8.3 `search_top10/and`, `search_request`), with proximity on and off.

7. **Evaluation cost.** The bonus needs positions, so it is computed only for a document that is scored, after its
   bound did not let it be skipped: for each adjacent pair and each field holding both terms, the two position lists
   are read and merged (stopping at `d = 1`). Fields without positions read nothing.

8. **Explain** (§7.7): a `bool` with pairs lists its bonus between its `must` and `should` clauses (`proximity`),
   per pair its first word's index, `λ · min(idf)`, the field, `d`, `π` and their product, also when it is 0. The
   root's score stays the hit's, bit for bit.

9. **Reference and tests.** The reference engine (`tests/reference`) computes the bonus by scanning token
   indices (its positions), independently of the postings. The differential suite covers queries with adjacent
   words, proximity on and off. A proptest checks pruned (both strategies) == exhaustive with proximity, and a unit
   test checks `bonus ≤ B` at the boundary cases (`π = 1`, many pairs, large `idf`). The nDCG@10 gate (7.8.4) reports
   the change; its baseline is updated in the same PR with the queries that moved.

## Alternatives considered

- **Minimum span of all words** (Clarke et al.; Tao & Zhai's span measures): see 1. It also needs a sliding window
  over every word's positions per document, and has no natural bound per pair.
- **BM25TP's saturated accumulation** (sum over all occurrence pairs, then `acc·(k1+1)/(acc+K)`): rewards repeated
  near co-occurrence, but bounding it needs `(k1+1)·min(1, idf)` per word, a looser `B`, and more position work per
  document. The minimum distance is what Tao & Zhai found most effective.
- **Proximity as a separate ranking rule** (a lexicographic tier, M32): that is ROADMAP 32's model, v1.1. Here it is
  an additive BM25F signal, so pruning keeps working.
- **Opt-in per request**: no ranking change for anyone, but almost nobody turns it on. Positions are already the
  opt-in.
- **A collection-level weight now**: needs a manifest version (5). Deferred, not rejected.

## Consequences

- With positions, AND queries rank documents where the words stand together higher. Phrases are unchanged.
- Scores of queries with two or more required words in fields with positions change. Scores were never promised
  stable across versions before v1.0; the nDCG@10 baseline moves with this PR.
- Pruning gets looser by `B` on those queries. Queries over fields without positions are unaffected.
- `λ` and `W` are constants of the scorer (SPEC §7.2), changed only with the nDCG gate updated in the same PR.

## Results (2026-11-07)

- nDCG@10 (7.8.4 gate): unchanged, every row and the mean (0.8824). Under AND each multi-word query of the 7.8.2
  corpus matches 1 to 3 documents and only 5 of those matches have their words apart; none reorders. ROADMAP 15.1.6
  adds judged queries proximity can decide. The bonus does apply: the top hit of `running shoes` gets `1.867`
  (`min(idf)`, the words adjacent in the title).
- Cost (docs/benchmarks.md): `search_top10/and` 423 → 586 µs pruned, 413 → 872 µs exhaustive; `search_suite/and`
  286 → 571 µs. A candidate whose clauses plus `B` cannot beat the threshold is skipped before its positions are
  read (item 6's bound, used per document as well as per range). Reading positions through the words' own postings
  is ROADMAP 15.2.6.
