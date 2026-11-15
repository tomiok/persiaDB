# ADR-0045: A request's `relax` options: every key, merged one at a time, and cursors that record the settings

- Status: proposed (amended by ADR-0047: `filters: "last_resort"` plans the filter step, item 6)
- Date: 2026-11-15
- Related: SPEC §6.2.1, §6.2.3, §7.1.3, §7.4, §7.5, §7.6; ROADMAP 8.1.2, 8.1.4, 8.1.9, 8.1.11, 8.2.6, 8.3.1, 8.3.2,
  10.1.7; ADR-0022,
  ADR-0037, ADR-0043

## Context

SPEC §7.6 said a query's `relax` object takes the keys of §6.2.3's `relaxation`, each overriding the collection's
setting, and that `relax: false` runs L0 only. The engine already merged `RelaxOptions` over `RelaxationConfig` in
`RelaxPlan::new`. Several things were still open:

- the JSON form of a request's `relax`, and whether some keys are for the collection only;
- whether the merge is per key or replaces the collection's settings whole;
- what `relax: false` returns, and how a search without a cascade fits in;
- what a cursor does on a page whose options differ from the page that made it. Cursors recorded the snapshot, the
  memtable view, the proximity setting and the hit's step, not the relax options. So a page with other options
  silently built the other plan's steps. Its hits were not a slice of the cursor's ranking, and the error came only
  when a step no longer existed.

## Decision

1. **The form is `false`, `true` or an object of every `relaxation` key.** `false` is `{"enabled": false}`, and
   `true` is `{"enabled": true}`, which turns on a cascade that the collection turned off. The object takes
   `enabled`, `min_results`, `levels`, `fallback`, `fill`, `filters` and `budget_ms`, and no key is for the collection
   only. A request may want more hits, other levels, no fallback, `fill`, or a longer budget, and each of these is
   per query in nature. The limits a step is held to (§7.1.3) are the engine's, not a key. The JSON is read as
   strictly as a collection definition (ADR-0022): an unknown or repeated key, `null`, another shape, or more than
   64 KiB is an error (`RelaxOptions::from_json`, `RelaxError::Json`).
2. **The merge is per key: the request's value, else the collection's, else the default.** A collection definition
   that leaves a key out already has §6.2.3's default. `levels` is one key, and a list replaces the collection's list
   whole. Merging lists would make a request unable to remove a level.
3. **Every value given is checked, also when the cascade is off.** `min_results` must be in `1–10 000` and
   `budget_ms` in `1–60 000` (`RelaxationConfig::MIN_RESULTS`, `BUDGET_MS`), and custom levels must be distinct and in
   cascade order. A value that breaks its rule is `RelaxError::OutOfRange` or `RelaxError::Levels`, naming the key.
   `RelaxOptions::validate` checks a request on its own, and `RelaxPlan::new` checks the merged settings again.
4. **`relax: false` is L0 alone, stated as exact.** So is `levels: []` with `fallback: "none"`. The response has
   `level: "exact"`, no `applied`, no `fallback` and `truncated: false`, and its page may be empty. The plain
   `search` (a `SearchPlan` and a request, no cascade) is `relax: false`. A request that wants relaxation runs
   `search_relaxed` with the merged `RelaxPlan`. The engine has no request JSON yet (ROADMAP 10.1, 11.x): its callers
   build `RelaxOptions`, or parse them with `RelaxOptions::from_json`, and pass the plan.
5. **A cursor records the relaxation settings, and a page under other settings is an error.** The settings
   (`RelaxSettings`) are the levels the plan runs (L5 included), `min_results`, `fill` and `filters`: everything that
   decides which steps are built and where the cascade stops. They are `None` for a plan of L0 alone and for a plain
   search, so cursors from those two page each other, as before. `budget_ms` is not recorded. A budget only
   truncates a page, and the cursor's last step already lets the next page build the steps again. A page that differs
   is `RequestError::CursorRelax`. It is checked in `check_request` with the cursor's origin (snapshot, collection,
   memtable view) and its proximity setting. Before it runs, `StepCursor`'s own checks run: step ≤ last, hit step
   between them, and a step after L0 given to a search of L0 alone is `RequestError::CursorStep`. With equal
   settings, `StepCursor` pages exactly as before. A cursor whose step a plan no longer builds can still come from
   other limits (`RelaxPlan::with_limits`), which are not recorded, and pages nothing.
6. **`filters: "last_resort"` is accepted and part of the settings.** §6.2.3 allows it. Until ROADMAP 8.2.6 no
   level dropped a filter, so it searched as `"never"`. Since ADR-0047 it plans one last step, L6 `filters`, which
   the cursor's recorded levels include.
7. **`fill` is a request key with no rule of its own (ROADMAP 8.3.2).** The cascade has filled since ROADMAP 8.1.2
   (ADR-0037 item 1). A request sets it as it sets any key, and the cursor records it (item 5), so a page never mixes
   a filling ranking with one that stops at `min_results`. With `fill`, a page builds steps until it holds
   `offset + limit` hits from the top or the steps run out. L5 fills below the word levels. A deeper page may build,
   state and count (facets) more steps than the first page. The pages are still slices of the ranking that one page
   of every hit gives. A budget that passes while a page fills truncates that page only, and the next page builds the
   remaining steps again. Facets under `fill` keep SPEC §7.5's interim rule, which counts the steps each page built:
   choosing which matches a facet counts is ROADMAP 8.1.9, and the cost of a cursor page searching the earlier steps
   again to count them is ROADMAP 8.1.11.

## Consequences

- `Cursor` has a new public field, `relax`. Code that builds a cursor by hand must set it (`None` for a plain search).
- A step-0 cursor from a cascade that relaxes no longer pages a plain search, and a plain search's cursor no longer
  pages a relaxing cascade. Both are `CursorRelax`. Before, they paged only by luck: a cascade with `fill`, or with a
  `min_results` above L0's hits, relaxes past L0 on the next page, which a plain search does not.
- A request that restates the collection's settings searches exactly as one that gives none, cursors included: the
  settings are the merged ones. A proptest in `db::relaxed::tests::options` checks it.
- `RelaxOptions::from_json` gives the server (ROADMAP 11.x) and the SDKs the one parser of the form.

## Alternatives considered

- **Keep paging under other options, unrecorded** (exact only when unchanged, like other plan options, ROADMAP
  10.1.7): this matches how the proximity setting would have worked without its check. But a relaxed page under
  another `min_results` or `fill` is wrong in silence, and the cause is easy to miss: a client that changes `fill`
  between pages gets repeated or missing hits.
- **Record the settings in `StepCursor`**: it is `None` for a step-0 hit of a cascade that stopped at L0, which is
  how a plain search's cursor reads. A cursor from such a hit must still record the settings (`fill` relaxes past L0
  on the next page), so they live beside it on `Cursor`.
- **Record `budget_ms` too**: two pages that differ only in budget build the same steps. Only a page that was
  truncated differs, and the cursor's last step already handles that. Rejecting them would make a client that raises
  the budget on retry fail.
- **Some keys collection-only** (`levels`, `filters`): a request that wants exact matching only, or a smaller
  cascade, would then need a second collection.
