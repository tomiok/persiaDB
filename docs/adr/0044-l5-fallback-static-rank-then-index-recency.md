# ADR-0044: L5 `fallback`: the collection's static rank, then index recency, flagged in the response

- Status: proposed (amended by ADR-0047: `filters: "last_resort"` relaxes filters in one step after L5, which excludes L5's step alone)
- Date: 2026-11-14
- Related: SPEC §6.2.3, §7.1.3, §7.3, §7.4, §7.5, §7.6, §7.7, §8.5; ROADMAP 7.4.11, 8.1.3, 8.1.4, 8.1.22, 8.2.5,
  8.2.6, 8.2.8, 8.2.9, 8.3.2, 8.3.4; ADR-0037, ADR-0043

## Context

SPEC §7.6's L5 row says: "Per collection: `static_rank` (top by static rank / recency), `none`", and §6.2.3 adds that
without `ranking.static_rank` the fallback returns "the most recently indexed documents". Until now the plan listed
L5 but its transform built no step, so a query no level could relax returned nothing. Several things were open:

- Whether filters and excluded clauses still apply, and how L5 avoids returning what earlier levels already did.
- What "recency" is. Nothing on disk records when a document was written: a segment's docids are in write order,
  and segments are listed in commit order, but a merged segment is added after the kept segments (§8.5), so the
  only order there is, location, is not write order once a merge commits.
- How L5 orders its hits beside a request's sort, cursors, facets, the budget and the limits.
- What the response says, and how "clearly flagged" shows.
- How to make it cheap: L5 has no words to score, and scoring every document would be a full scan per query.

## Decision

1. **One step, `fallback`, that only selects.** Its query is L0's root `must_not` and `filter` around `match_all`
   (`match_all` alone without either): a compiled query's excluded clauses and the request's filter, a programmatic
   root `bool`'s `must_not` and `filter`, nothing for any other root. Filters are not relaxed at L5 (`filters:
   "last_resort"` adds one step after L5 that drops them, ADR-0047), and excluded clauses stay excluded: a user who wrote `-blue` never gets blue
   documents back. `fallback: "none"` builds no L5, as before.
2. **Earlier hits are excluded as at every step** (ADR-0037 item 2): the cascade puts the steps searched before L5
   in its `must_not`, so L5 returns the documents no stricter step matched, once. Deleted and replaced documents are
   never matches (§7.3), so L5 never returns one, nor a document the filter drops.
3. **L5 ranks by its own order, whatever the request's sort**: static rank descending (SPEC §6.2.3's
   `weight · fn(v)`, the hit's score), then recency. The request's sort has nothing to rank here (every text score
   would be 0), and a column sort such as `price` ascending is not "the collection's best documents". The hits still
   state the request's sort values (a `score` key is the static rank), so a cursor validates as usual; a cursor from
   an L5 hit pages by L5's order from its location, re-reading the static rank there (same snapshot and memtable
   view, which §7.4 enforces). Each hit's rank score is its step's, so L5 ranks below every earlier step.
4. **Recency is index location, reversed**: the memtable first, then segments from the last the manifest lists to
   the first, each by docid descending. It is deterministic per snapshot and memtable view, so every page agrees, and
   an upsert, a new document in the memtable, is the most recent. It is *not* write time: once a merge commits, its
   documents (older) rank above segments flushed after its inputs. SPEC §6.2.3 and §7.6 say so explicitly. A static
   rank orders the same whatever segments hold the documents; only ties may move with a merge. A durable
   per-document index order copied through merges is ROADMAP 8.2.8 (v1.1). Without a static rank, or with weight 0,
   every score is 0 and recency alone orders.
5. **Cheap, nothing scored.** With a static rank, each segment visits its FOR blocks best first, by the best key a
   block can hold (the static rank of its bound, the minimum plus `2^width − 1`, at its last docid), and stops at the
   first that cannot beat the worst hit kept: no later block can. Within a block, each candidate's column key is
   compared with the block's smallest key whose static rank reaches the worst kept hit's (a binary search over the
   block's key range, again only when that hit changes), so a losing document costs a key read, not a `log1p`. The
   static rank never decreases with the key, so both bounds are exact. An `f64` bound is capped at `f64::MAX`'s key
   (stored floats are finite), as the loose bound may reach the keys of `+∞` and NaN. The memtable has no bounds and
   is walked in docid order. The FOR bound is loose: values spread over their range give every block the same one,
   and then every key is read (ROADMAP 8.2.9). By recency alone, views are read newest first, each from its last
   docid back in windows that double (a fresh matcher advanced to the window's start), and the walk stops once it
   holds the hits the page needs. The first window is 64 candidates, so a page of 10 whose candidates all match reads
   one; filters or exclusions that reject candidates make it read larger ones. With facets, every candidate is
   visited and counted, as for any step (§7.5). Views are walked one after another, not on the pool: the recency
   walk's early stop needs the order.
6. **The response.** `level` is `fallback` when L5 was built and not skipped, `applied` lists `fallback` (`:cut` or
   `:skipped` when §7.1.3's limits, counting the steps it excludes, cut or skip it), `effective` is the excluded
   clauses alone (empty when there are none: no word was searched), `corrected` is empty. The explicit marker is
   `Relaxation::fallback`, `"fallback": "static_rank" | "recency"` in JSON: set when L5 is the hit step (ADR-0043),
   so it says both that the hits are the collection's ranking rather than matches of the words, and how they were
   ordered. It is absent when L5 was built but found nothing (a filter no document passes). The facade (`persia`,
   ROADMAP 10.x) and the server's response (§12) carry the field as the engine states it, as they carry the rest of
   `relaxation`.
7. **Cascade rules unchanged.** L5 is built only when the cascade reaches it, collects only the hits still missing
   (`fill` lets it complete a page below the earlier hits), is rebuilt on cursor pages, honours the budget at its
   reads and is dropped whole when the budget passes during it (`truncated`), and turns the request's deadline and
   cancellation into errors. An explained L5 hit is `Explanation::StaticRank { score, value }` (§7.7): its plan,
   whose score is 0, does not explain it.

## Consequences

- By default (`fallback: "static_rank"`), a query no level relaxes now returns the collection's top documents,
  flagged. Tests that assumed L5 builds nothing set `fallback: none` or expect the L5 hits.
- The differential suite (`tests/differential.rs`, invariant 5) models L5: its schema has no static rank, so the
  reference checks the step's matches and recency order. `tests/relax_fallback.rs` checks static rank and recency
  against an oracle over random puts, upserts, deletes and commits, with filters, an excluded word and L2 before.
- `search_relaxed/l5/recency` and `search_relaxed/l5/static_rank` measure the step (docs/benchmarks.md).
- A cursor from an L5 hit reads one column value to find its place; a request sorted by a column still pays one
  column read per L5 hit for its sort values.
- After a merge, "recency" may surprise: merged documents count as newer. ROADMAP 8.2.8 is the fix; until then the
  SPEC states it.

## Alternatives considered

- **A per-document index sequence now** (a hidden column written at flush and copied by merges): true write-order
  recency, merge-stable. It is an on-disk format change (SPEC, version or feature flag, ADR, golden fixture), out of
  this leaf's scope; deferred to ROADMAP 8.2.8 (v1.1).
- **Order L5 by the request's sort**, with the static rank as the score: a score sort would break ties by location
  ascending (oldest first), the opposite of recency, and a column sort would not be "top by static rank".
- **Document id order for ties**: merge-stable but not recency, which SPEC promises.
- **Score every document by static rank** (a full scan): simple, but O(documents) per query; block bounds and the
  backward recency walk read a few blocks or windows for a page.
- **A boolean marker**: it would not say how the hits were ordered; `"static_rank" | "recency"` says both.
- **Relax filters at L5**: filters are hard constraints (permissions, tenancy); relaxing them is ROADMAP 8.2.6's
  explicit opt-in, one step after L5 (ADR-0047).
