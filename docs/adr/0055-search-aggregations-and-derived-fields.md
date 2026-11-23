# ADR-0055: Search aggregations and derived fields

- Status: proposed
- Date: 2026-11-22
- Related: SPEC §5.1, §5.3, §5.3.5, §6.2.3, §6.4.1, §7.5, §15; ROADMAP M36; ADR-0001

## Context

Users want to compute over the documents a search returns (a count, a sum, an average, and the same metrics
grouped by a field) on normalized data, not only to rank documents. Persia already has typed schema fields
(SPEC §6) and facets that walk the hits of a query (SPEC §7.5). A facet counts, per value, the live documents
the query matches, from the segment's keyword column, over the hit set the query defines, including every
relaxation step the cascade ran. Segments are immutable once written (SPEC §5).

The engine is synchronous and talks to bytes only through `Storage`. An aggregation is the same kind of
computation: deterministic, with no network and no I/O beyond `Storage`.

SPEC §5.3 already stores per-document columns for `keyword`, `i64`, `f64`, `datetime`, and `bool`
(`Columns`, section kind 8, SPEC §5.3.5), and filters, sorts, and facets read them. Those columns are the
doc values an aggregation reads. A second copy of the same values is not a different layout.

What is not decided yet is everything above those columns: which metrics exist, how numbers overflow, how a
group is ordered, whether a value may be computed from other fields, and whether text may be turned into a
typed field at index time. Any on-disk bytes those features add have to be reserved before the v1.0 format
freeze (M15, SPEC §15), or a later addition breaks readers of the frozen format.

## Decision

Three layers, all in the engine. Each one is deterministic. None of them performs network I/O or reads
anything except through `Storage`. Implementation is post-MVP (ROADMAP M36). This record does not change
SPEC.md. The design leaf writes the SPEC text.

### Aggregations over the hits of a query

An aggregation is computed over the same snapshot as the search, and over the same hit set a facet counts
(SPEC §7.5): the live documents the query matches, each document once at the step that counted it, including
every relaxation step the cascade ran when the query relaxed. A document the search leaves out, because it
is deleted or hidden by a newer copy, stays out. The page size does not limit the set.

The metrics are `count`, `sum`, `avg`, `min`, `max`, percentiles, and a histogram. A group-by on a `keyword`
field, or on another typed field that has a column (`i64`, `f64`, `bool`, `datetime`), holds those metrics
inside each bucket. A metric may take a field or a computed expression.

Group-by reuses facet membership: a document counts once per distinct value (SPEC §7.5). The v1.1 leaf
specifies a keyword field; the other typed columns use the same rules. Buckets are ordered deterministically,
by the requested metric descending (`count` when the request names none) and then by the key (keyword bytes
ascending, or the column's value order for a typed field). The request sets `size` with the same bounds as a
facet (1 to 1 000, default 10, SPEC §7.5). Buckets past `size` are not returned.

Percentiles are exact over the included values, nearest-rank, with no interpolation and no sketch. For a
percentile `p` in `0..=100` and `n` included values in their total order, the result is the value at index
`ceil(p / 100 * n) - 1`, clamped into `0..n`. `p = 0` is the minimum and `p = 100` is the maximum. A
histogram takes explicit, strictly increasing edges from the request. A value falls in
`[edge_i, edge_{i+1})`, and the last edge is inclusive. Values below the first edge and above the last are
`underflow` and `overflow`, counted apart from missing values. The number of buckets has the same hard
maximum as facet `size` (1 000). An edge list that is not strictly increasing is an error.

### Numeric semantics

`count` is a `u64` of the hits in the set being aggregated (the whole hit set, or one bucket). It saturates
at `u64::MAX` and does not wrap.

`sum`, `min`, and `max` of an `i64` field stay in `i64`. The sum accumulates in `i128` and is clamped to
`i64::MIN` or `i64::MAX` only when the finished sum does not fit in `i64`. The clamp does not depend on
addition order, and the sum does not wrap. `min` and `max` of `i64` values need no clamp.

`sum`, `min`, and `max` of an `f64` field stay in `f64`. The sum is added in snapshot order: committed
segments oldest first, as the manifest lists them, then the memtable, and ascending docid inside a view.
A partial sum that is not finite is replaced by `f64::MAX` or `-f64::MAX` of the same sign. The relaxation
score already saturates a product above `f64::MAX` at `f64::MAX` (SPEC §6.2.3). A positive aggregate
overflow uses that same bound, and a negative one uses `-f64::MAX`, so a sum is never infinite and never
NaN.

`avg` is always `f64`, including for an `i64` field. An `i64` average divides the exact `i128` sum, before
the `i64` clamp, by the included count. An `f64` average divides the `f64` sum. Division is round-to-nearest.
An `i64` that is not exactly an `f64` becomes the nearest `f64`. An `f64` sum that saturated is not the
mathematical sum past the bound, and the average of it is not either.

A metric that clamped reports `saturated: true`. Otherwise `saturated` is false. An `i64` `sum` can be
saturated while its `avg` is not, because the average still divides the exact `i128` sum. An `f64` `avg` is
saturated when the `f64` sum it divides was clamped. If an `i128` accumulation itself overflows, it
saturates at `i128::MIN` or `i128::MAX`, the `i64` sum is the clamp of that, and both `sum` and `avg` are
saturated.

A typed `f64` column does not hold a NaN or an infinity: writers reject them (SPEC §6.4.1), and a column key
that decodes to one is corruption (SPEC §5.2.1). A computed expression may still produce one. A NaN or an
infinity is not accumulated. It is counted in `non_finite`, separate from `missing`, and it does not poison
`sum`, `min`, `max`, or `avg`.

A hit with no value for the field is skipped for that metric and counted in `missing`. It still counts in
`count` of the hit set. When every value is missing or non-finite, the included set is empty.

An empty included set has `count` 0 and `sum` 0. `avg`, `min`, `max`, and every percentile are null. A
histogram over an empty set has 0 in every bucket and in `underflow` and `overflow`. `missing` and
`non_finite` are still reported.

### Computed fields

A computed field is a pure expression evaluated per document. It may be the input of an aggregation or a
field on the returned hit. The language is arithmetic (`+`, `-`, `*`, `/`), comparisons
(`<`, `<=`, `>`, `>=`, `==`, `!=`), references to numeric fields (`i64`, `f64`), numeric literals, `abs`,
and `round`. There are no loops, no recursion beyond the expression tree, no calls other than `abs` and
`round`, and no I/O.

`/` always yields `f64` (`1/2` is `0.5`). `+`, `-`, and `*` of two `i64` values stay in `i64`, accumulated
in `i128` and saturated to `i64` at the end. If either side is `f64`, the operation is `f64` and saturates
at `±f64::MAX` as a sum does. `abs` of `i64::MIN` saturates at `i64::MAX`. `round` is half away from zero,
to the nearest integer, as `f64`. Division by zero, a missing field, and a non-finite intermediate yield
null for that document. Null is omitted from a returned hit and counts as `missing` when it is aggregated.
A comparison yields a `bool` or null. A `bool` may be returned. It is not an input of `sum`, `avg`, `min`,
or `max`.

Evaluation is bounded. An expression of more than 64 AST nodes is rejected before any document is read, and
each document visits each node a constant number of times. The same document and the same expression always
produce the same value.

### Index-time extractors

An extractor fills a derived typed field from text when the document is indexed, not when it is queried.
Two forms are in scope: a regex capture, and number parsing with a locale stored on the field.
`"1.500,50"` under `es` and `"1,500.50"` under `en` are the same value, `1500.50`. The derived field is an
ordinary `i64`, `f64`, or `keyword` field, written into the `Columns` section, and an aggregation reads that
column. A capture that does not match, a number that does not parse, and a number that does not fit the
field leave the derived field missing. The source text is unchanged. A merge copies the column, as it copies
any other column, and does not run the extractor again, so the segment stays reproducible after the
extractor definition changes. Changing an extractor is a schema change in the sense of SPEC §6.2.4: readers
keep the values that were indexed, and a new definition needs a reindex.

The pattern language and the locale set are fixed by the implementation leaf. v1.2 includes at least `en`
and `es`. If the matcher is not written in this repository, that leaf justifies the crate against the
allow-list (ADR-0001). This ADR adds no dependency.

### Storage, and the format freeze

Aggregations read per-segment columnar values. They do not decode stored documents (`DocStore`). For
numeric and keyword fields those values are the `Columns` section SPEC §5.3.5 already defines.

Any bytes aggregations write that §5.3.5 does not already define (a new section kind, a stored sketch, a
column encoding the freeze does not name) are an on-disk format addition. They are gated by a segment
feature flag (`incompat_flags` or `rocompat_flags`, SPEC §5.1, §15). A new `section_kind` is corruption
unless a flag the reader knows allows it (SPEC §5.3). No aggregation bit is assigned yet.

The layout and the flag are designed before the v1.0 format freeze (M15) and recorded in SPEC then, so the
addition can land later without breaking compatibility. A v1.0 reader knows the bit. Old segments leave it
clear and keep their bytes. A later writer sets it only when it writes the reserved layout. The golden
fixture is written when that layout is implemented, together with the rest of the format discipline (SPEC,
this ADR, the flag, the fixture). The aggregations themselves are post-MVP. This decision assigns no bit
and does not edit SPEC.md.

### Out of scope

AI-backed transforms are rejected for the engine: translation, summarization, LLM extraction, and RAG. They
need a network or a model, and they are not deterministic. They belong in an optional layer outside the
engine, in the server or an SDK, and would be their own ADR if they are proposed. ROADMAP M27 (vector and
hybrid search) is a separate milestone. This record does not cover it.

User-defined functions loaded as WASM are a possible later option. This ADR does not decide them. The
expression language above is closed.

## Consequences

- v1.0 reserves the flag and the layout (ROADMAP 36.1.1, design only). The metrics, the expressions, and the
  extractors are post-MVP.
- A metric agrees with a naive reference over the same snapshot, including saturation, missing values,
  non-finite values, and empty sets. An `f64` sum follows the snapshot's view order, so it has one result.
- A saturated metric is distinguishable from a true bound, because `saturated` is set.
- Extractors run at index time, so an aggregation reads a column and a repeated query on an immutable
  segment returns the same value. Changing an extractor is a reindex.
- SDKs return the engine's result. They do not recompute it.
- SPEC.md stays as it is until 36.1.1 writes the flag and the layout. This decision changes no format byte.

## Alternatives considered

- Query-time extraction from stored text. Rejected: it is slow, and it is not reproducible. It decodes
  stored documents on the query path, and the result changes if the extractor or the locale changes under a
  segment that cannot be rewritten.
- Aggregating from stored documents, with no columns. Rejected: every metric pays to decode `DocStore`.
  Facets and sorts already avoid that by reading `Columns`.
- Doing it only in the SDKs. Rejected: every SDK reimplements the metrics, and none of them shares the
  engine's snapshot or the hit set facets already define (SPEC §7.5), so the numbers and the hits can
  disagree.
- Approximate sketches as the default for percentiles. Rejected here. The metrics are exact over the hit
  set, so a reference implementation can match them. A sketch would be a later layout, behind the reserved
  flag, and is not decided here.
