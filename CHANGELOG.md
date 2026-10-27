# Changelog

All notable, user-visible changes to Persia DB are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). On-disk format versions are tracked separately
(SPEC §15) and are called out under **Format**, a project-specific section added to the standard ones.

## [Unreleased]

### Added
- `persia-engine`: the memtable (ROADMAP 6.1, SPEC §8.2–§8.4). `Memtable` indexes `put`s and `delete`s in memory
  with the segment builder's analysis, so it has the terms, positions and norms of the segment it will flush to. An
  upsert replaces the earlier version inside the memtable; a delete is recorded even for an id the memtable lacks
  (`IdState::Deleted`). `Memtable::view` gives a `MemtableView`: one writer, any number of readers, each view seeing
  exactly the writes before it (MVCC), never holding the lock across a search. `FlushPolicy` is the size/idle trigger
  (64 MiB / 10 s, SPEC §8.2) as a pure function of the size, the last write and an injected `now`. Bench:
  `memtable/put` 78.9 K documents/s, `memtable/postings_freq` 45.4 M postings/s (docs/benchmarks.md).
- `persia-engine`: the `view` module's `PostingsIterator` and `SearchView` traits (ROADMAP 6.1.2): postings, norms,
  field statistics, documents and id lookup over a `SegmentView` (a segment and its tombstones) or a `MemtableView`;
  norms come through a `NormReader` (a segment's `NormsCursor`, or the memtable's norms copied once per view),
  deleted documents left out, so the query engine treats the memtable as one more segment. `FieldStats` is `Default`.
- `persia-format`: `postings::block_len` and `postings::tail_len` give the length of a postings block's encoding
  without encoding it, and `BlockWriter::drain_output_into` moves the encoded blocks into a `Writer` without
  allocating a buffer per call (ROADMAP 5.1.7).
- `persia-engine`: `BlockCache` and `Segment::open_with_cache` (ROADMAP 5.3.11, SPEC §10.6). A segment reads every
  block-framed section through an LRU cache of decoded blocks, bounded in bytes, which several segments may share
  (`BlockCache::DEFAULT_CAPACITY`, 8 MiB, for all the segments opened with `Segment::open`, ROADMAP 5.3.15). Term
  lookups, posting lists and document fetches no longer decode and re-check the CRC of a block another reader
  decoded; `BlockCache::stats` reports hits and misses. A dropped segment's blocks leave the cache. When blocks are
  re-read (docs/benchmarks.md, rows measured together), `segment_read` runs postings with frequencies 6.4x, `advance` 4.4x, document fetches 2.6x and term lookups
  1.3x faster, and `segment_records/id_of_1000` 5.8x; positions 13.5x, together with the per-document decoding of
  ROADMAP 5.3.13 (the benchmark does not separate the two). The record cache it replaces kept 8 blocks per segment.
- `persia-format`: `BlockReader::validated` and `BlockReader::reopen` (ROADMAP 5.3.10): a reader keeps what `open`
  validated (a `Copy` `ValidatedTable`) and rebuilds the reader of the same, immutable section bytes in O(1), without
  checking the table CRC and block ends again (blocks are still checked when decoded, so other bytes cannot cause a
  panic). `Segment::open` validates every block table once; lookups no longer cost a pass over the table (record
  lookups in a 50k-document segment of 1 KiB blocks: 2x faster, bench `segment_records/id_of_1000`).
- `persia` CLI: `persia inspect segment <file> --schema <schema.json> [--verify]` prints a segment's footer, its
  sections with their sizes (on disk, uncompressed, blocks) and per-field statistics, and with `--verify` runs
  `Segment::verify` and fails if the segment is corrupt (see its entry for what it checks and its memory cost; ROADMAP
  5.4.2). `persia dump segment <file> --schema <schema.json> --field <f> [--term <t>]` lists a field's terms with
  `df`, `ttf` and `max_tf`, or one term's postings block by block: docid, id, frequency and positions (ROADMAP 5.4.3). The CLI now depends on `persia-engine` and `persia-format` (ADR-0020, amended).
- `persia-engine`: `Segment::verify` (ROADMAP 5.4.1) reads a whole segment and checks what no lookup can: every block
  and raw CRC; each term's `df`, `ttf`, `max_tf` and `min_norm` against its list and each Skip entry's bounds against
  its block; the `skip_off` chain and the end of the Skip section; minimal bit widths and FOR minimums; norms and field
  statistics against the term frequencies; keyword columns against the postings; stored values against the columns;
  the `IdList`, `IdMap`, `BlobRefs` and `DocStore` against each other; `min_id`/`max_id`; sections that hold nothing;
  and column values no document can hold (a NaN, infinite or `-0.0` `f64`, a `datetime` out of range, ROADMAP 5.4.4).
  Not checked: stored `text` values against the postings (that needs re-analysis). It decodes every block; it reads
  numeric keys from their FOR blocks as it checks them, and decodes each keyword column's starts and ords once (8
  bytes per document and per ord), so its memory grows with the segment size times the number of keyword fields.
  `Segment::sections` lists the section table with each block-framed section's stream size.
- `persia-engine`: the segment reader (SPEC §5, ROADMAP 5.3). `Segment::open` checks the footer, section table, padding,
  section presence, every block table and raw CRC, and the schema hash at the field count the segment records, so a
  segment written before fields were added opens with the evolved schema. `Segment::terms` gives a `TermIndex` (term
  lookup, `range`, `prefix` and automaton `search` enumeration); `Postings` iterates a term's documents with
  `next`, `advance` (skipping blocks through the Skip entries), `freq`, `positions`, `block_max` and
  `shallow_advance` (block-max WAND), and leaves out deleted documents when given the segment's `Tombstones`
  (`Segment::tombstones`, then `Postings::with_tombstones`). `norms`, `numeric_column`, `bool_column` and
  `keyword_column` are typed doc-value readers. `docid_of` finds a docid through the `IdMap` FST; `document`,
  `document_by_id`, `id_of` and `blob_ref` read records through a small block cache. Corrupt bytes are a `ReadError`, never a panic.
- `persia-engine`: segments (SPEC §5, ADR-0027). `SegmentBuilder` indexes documents in memory (interned terms, one
  posting log per field, columns, `DocStore` rows and ids; `size_in_bytes` for flush thresholds) and `SegmentWriter`
  writes a deterministic segment: term dictionary (FST), term info, postings, frequencies, skip entries, positions,
  norms, columns, `DocStore`, `IdMap`, `IdList`, `BlobRefs` and the footer. A Criterion benchmark (`segment`).
- `persia-analysis`: `TokenStream::size_in_bytes`, the heap bytes a stream keeps between uses (for memory accounting).
- `persia-analysis`: allocation-free analysis for every built-in analyzer once its `TokenStream` has grown: `Scratch`
  buffers lent by `TokenStream::rewrite_with_scratch`, and `Stemmer::stem_into`, which the built-in stemmers implement
  in them (its default goes through `stem`). A counting-allocator test checks it (ADR-0026). Still allocating: tokens
  longer than 1020 bytes (their strings are not kept), tokens holding `Σ` (lowercased with `str::to_lowercase`), long
  runs of combining marks (buffered by the normalization iterators), and char filters.
- `persia-analysis`: built-in `fr`, `de` and `it` analyzers at version 1 (SPEC §6.3.3): `ElisionFilter` (`FRENCH_ELISIONS`, `ITALIAN_ELISIONS`), `FrenchStemmer`, `GermanStemmer`
  and `ItalianStemmer` (Snowball 3.1.1, checked against the full Snowball vocabularies; French and Italian remove
  elisions such as `l'` and `nell'`) and `FRENCH_STOPWORDS`, `GERMAN_STOPWORDS`, `ITALIAN_STOPWORDS`. Schemas may name them.
- `persia-analysis`: `FoldingFilter` skips the allocation for non-ASCII tokens with nothing to fold (a pre-scan, about
  3× faster on such text, same output).
- `persia-analysis`: a Criterion benchmark of every built-in analyzer and of folding (tokens per second, baselines in
  `docs/benchmarks.md`), and an `analyzers` fuzz target over the built-ins and char filters.
- `persia-engine`: a stored schema naming an analyzer version this build lacks (written by a newer build, e.g.
  `es@2`) is `ManifestError::UnsupportedAnalyzer` (`SchemaError::UnsupportedAnalyzer`), not corruption (SPEC §6.2.2).
- `persia-analysis`: a Unicode version bump gives a new version to exactly the built-in analyzers whose output it
  changes, found by fingerprints of their output over every Unicode scalar value; superseded versions keep working
  (SPEC §6.3.1, ADR-0025).
- `persia-analysis`: char filters (`CharFilter`, `CharFilterOutput`, `AnalyzerBuilder::char_filter`; SPEC §6.3.1). A
  filter keeps or replaces pieces of the value, and the analyzer maps token spans back to the original value, so
  highlighting with them works whatever the filters did.
- Roadmap: post-MVP milestones M16–M30 (v1.1 through v3.0). M0–M15 stay the MVP and v1.0. The progress percentage and burn-up still count only those leaves.
- Docs: the first embedded binding release follows the v1.0 tag (M15, then M16). ROADMAP 10.5.2's `v0.1.0-alpha` tag stays the core's notice that the format is unstable.
- Website: `llms.txt` lists every documentation page for agents, and each HTML page links to it. `llms-full.txt` is those pages in Markdown. URLs include the `/persiaDB/` base.
- Website: looping hero video on the landing page, with a pause control. Visitors who prefer reduced motion, and browsers without JavaScript, see the poster still.
- `persia-analysis`: analyzers (SPEC §6.3). `Analyzer::builder` chains char filters, a tokenizer and token filters. `analyze` gives index-time tokens and
  `analyze_query` query-time tokens, without filters that stack tokens such as edge n-grams (§6.3.4), and
  `SynonymMap::analyzed` analyzes synonym terms the same way. The built-ins `standard`, `keyword`, `whitespace`,
  `en`, `es` and `pt` at version 1 (`builtin_analyzer`, `BUILTIN_ANALYZERS`; chains in
  §6.3.3), and `AnalyzerRegistry` to look analyzers up by name and version. `ApostropheFilter` maps `‘` and `’` to
  `'` for the English stemmer.
- `persia-engine`: an `analyzer` given without a version binds to the field's current version on schema evolution
  (SPEC §6.2.4); `AnalyzerRef::is_pinned` tells whether a definition named the version.
- `persia-analysis`: tokenizers (SPEC §6.3.1). `Token` carries its text, byte span and position; `StandardTokenizer`
  splits at Unicode word boundaries (UAX #29: numbers, apostrophes, emails and URLs as specified, one token per
  character for Han, Hiragana and Thai), `WhitespaceTokenizer` at Unicode white space, and `KeywordTokenizer` keeps
  the whole value. `TokenLengthGuard` truncates (default) or drops tokens longer than a limit (255 bytes by default).
- `persia-analysis`: token filters (SPEC §6.3.2) behind a `TokenFilter` trait: `NfkcFilter`, `LowercaseFilter`
  (Unicode default mapping, no Turkish tailoring), `FoldingFilter` (accents and diacritics, with a preserved set such
  as `ñ`), `StopwordFilter` with embedded English, Spanish and Portuguese lists, and `EdgeNgramFilter` for prefix
  indexing. `SynonymMap` finds the longest synonym terms in an analyzed query and lists their alternatives (§7.6 L1).
- `persia-analysis`: stemmers (SPEC §6.3.2) behind a `Stemmer` trait: `EnglishStemmer` (Porter2), `SpanishStemmer` and
  `PortugueseStemmer`, the Snowball 3 algorithms checked against the full Snowball test vocabularies, and
  `StemmerFilter` to stem a token stream, removing tokens whose stem is empty.
- `persia-engine`: the manifest (`manifest::Manifest`, SPEC §8.7; ADR-0024): per collection its schema and live
  segments with their tombstone sidecars, plus `seq`, `fence_token` and `wal_start`. A checksummed, versioned binary
  encoding (`encode`/`decode`, corrupt input is an error naming the offset) and `ManifestHash`. Commits are planned
  as diffs (`ManifestDiff`): `apply` checks the transition (no fence, WAL or tombstone regression, schema changes
  only through evolution, fresh segment ids) and `diff` recovers the diff between two manifests.
- `persia-engine`: documents (`document::Document`, SPEC §6.4; ADR-0023). Values per field type (strings or lists of
  strings for `text`/`keyword`, one value for numerics, `bool` and `datetime`, any JSON for `stored`), validation
  against a schema with precise errors (field and list index), and limits (id, keyword length, field and value counts, nesting,
  size). JSON input rejects duplicate keys; `datetime` takes integer microseconds or RFC 3339 and is written as RFC
  3339 UTC. Non-strict collections keep unknown keys as extra stored fields. `f64` values are finite and `-0.0` is
  stored as `0.0`, so range filters never miss zero.
- `persia-engine`: collection schemas (`schema::CollectionSchema`, SPEC §6.2; ADR-0022). Field types and per-type options
  (analyzer, boost, positions, `k1`, `b`, facet, stored), ranking (`static_rank`) and relaxation defaults, and synonyms.
  The JSON definition is parsed strictly: unknown or duplicate keys and options foreign to a field's type are errors.
  Names, limits and ranges are validated. A definition may give explicit field ids; missing ones are assigned in byte
  order of the field names, and evolution keeps existing ids and gives added fields the next ones. A schema hash (XXH3-128 of a canonical
  encoding of what is indexed) ignores query-time settings. Evolution accepts added fields and rejects changes to a
  field's type, analyzer, positions, facet or stored flag.
- Website: homepage title no longer repeats the site name; share image, JSON-LD, edit links, and last-updated dates; distinct titles for the feature, roadmap, and architecture pairs; longer meta descriptions.
- Development environment: `docker-compose.dev.yml` runs MinIO (the community `pgsty/minio` build), fake-gcs-server and Azurite on localhost, every image pinned by digest; `just dev-up` / `just dev-down`. CI checks the pins resolve.
- Design for embedded Go and TypeScript SDKs (ADR-0018, `docs/sdk/`). Nothing is implemented. Go links a future C ABI with cgo; TypeScript's first package is an N-API addon, with WASM for the browser and edge deferred. The network clients in SPEC §13.2 are unchanged.
- Workspace skeleton (stub crates).
- Apache-2.0 license.
- Project website under `website/` (Astro Starlight), for GitHub Pages at `https://tomiok.github.io/persiaDB/`.
- Website: a Getting started page (what works today versus the illustrative `Db` API) and a For AI agents page. `llms.txt` and `llms-full.txt` follow the llms.txt convention and are linked from the README and the site footer.
- `persia-format`: read-only memory mapping of immutable file ranges (`MmapFile`) and `ByteSource`, one buffer type for
  mapped, heap and cached remote bytes that keeps error offsets absolute (SPEC §4.7; ADR-0010).
- `persia-format`: `FrameEncoder` streams a frame whose payload is too large to buffer (declared length, incremental
  CRC, length verified on finish), and `FrameHeader` parses a frame's 24-byte header so range readers can size reads
  (SPEC §4.4).
- `persia-format`: `BlobId` (SHA-256, lowercase hex text form) and `BlobRef { blob_id, size, mime, chunk_size, user_meta }`
  with validated limits and a canonical binary encoding (SPEC §9.1; ADR-0011).
- `persia-storage`: the `Storage` trait with validated `Key`/`KeyPrefix`, `ObjectMeta`, `PutOutcome`, `Capabilities`,
  an error taxonomy (`ErrorKind`: not found, conflict, transient, corrupt, I/O, invalid input, unsupported), length-checked
  ranged reads returning `ByteSource` (`ByteSource::for_range`), and a retry policy for transient errors (exponential
  backoff with full jitter, injectable clock) (SPEC §10.1).
- `persia-storage`: `MemoryStorage`, the in-memory backend with atomic `put_if_absent`, and `FaultyStorage`, a wrapper
  that injects errors, latency, torn and dropped writes, lost responses and crashes (optionally losing unsynced writes,
  in order or per object) on a scripted or seeded schedule; a shared conformance
  suite checks every backend against SPEC §10.1.
- `persia-storage`: `LocalStorage`, the local filesystem backend (one file per object): atomic, durable `put` (temp file,
  fsync, rename, directory fsync) and `put_if_absent` (hard link), positioned reads, and file names encoded so every key
  round-trips on case-insensitive and normalizing filesystems and `a` and `a/b` can coexist (SPEC §10.1.1; ADR-0012).
- `persia-storage`: `Container`, the single-file container (SPEC §4): creation that never replaces a file, an
  exclusive writer lock, read-only opens, frame appends with group `fdatasync`, commits that write the inactive
  superblock slot, and recovery on open (superblock checks, validation of the committed WAL frames, torn-tail
  truncation; a corrupt tail is truncated, quarantined first, or refused, per ADR-0014 below). A failed write or sync poisons the handle.
  `SimFile` simulates crashes (lost, kept or sector-torn unsynced writes) for crash tests.
- `persia-storage`: container compaction (SPEC §4.6): `space_usage` reports live and dead bytes against the default
  40% dead-space ratio, and `compact` rewrites the live frames, the WAL range and the uncommitted tail into a new
  file (renumbered, payloads copied as stored, a new Manifest built from where the committed frames moved; the returned
  `Relocation` also covers the uncommitted tail) that is synced
  and renamed over the container; the old file is never written. `persia-format`: `Frame::reframe`.
- `persia-storage`: recovery reads the container in bounded 64 KiB chunks (`persia-format`: `StreamScanner`), so
  opening a container with a large uncommitted Segment frame or a long torn tail no longer needs that much memory.
- `persia-storage`: `LocalStorage::sweep_temporaries` removes the `@tmp.*` files that crashed `put`s left, skipping
  those a live `put` holds (each `put` now locks its temporary file); `temporaries` lists them. Never automatic
  (ADR-0019).
- `persia-storage`: `Container::side_files` lists a container's temporary and quarantine files by path, and
  `remove_quarantine_files` removes the quarantine copies through a read-write handle.
- `persia` CLI: `persia container files|sweep [--quarantine] <path>` and `persia store files|sweep [--force] <dir>`
  list and remove those files, one tab-separated line per file. `--quarantine` removes copies that may hold acknowledged
  writes, and `--force` (where locks cannot be checked, Windows) can make a racing `put` fail: inspect the listing first.
- `persia-storage`: compaction copies each frame in bounded 64 KiB chunks, validating it on the way, so compacting a
  container with a large Segment frame no longer needs that much memory, and a small frame is copied with one write
  (`benches/compaction.rs`). `persia-format`: `StreamScanner::reframe`.
- `persia-storage`: `Container::append_stream` appends a frame whose payload is written in chunks (`FrameWriter`).
- `persia-storage`: a read-write open whose frames after the last commit end in corruption no longer always fails
  (ADR-0014): zeros and stale frames are truncated, other bytes are copied to a `<name>.quarantine.*` file first, and
  only frames that may hold acknowledged writes still fail the open, unless `Container::open_with(.., Repair::Quarantine)`.
- `persia-storage`: `vfs::Vfs`, the file-system operations of `LocalStorage` and the container's path API, with
  `RealFs` and `SimFs`, a simulated file system that loses unsynced data and directory entries in a crash;
  `LocalStorage::open_in` and `Container::create_in` / `open_in` run on any of them.
- `persia-storage`: `Container::sweep_stale_files` removes the temporary files a crashed create or compaction left next
  to the container.
- `persia-storage`: opt-in mapped reads of a container's committed frames (`Container::enable_mapped_reads`, one
  window over `12288..log_end` remapped after each commit and compaction, `pread` for everything else), and
  `Container::read_frame_source`, which returns a payload as a `ByteSource`: without copying it for an uncompressed
  frame the mapping covers; read into a buffer for other uncompressed frames, decompressed into one for compressed
  frames (SPEC §4.7; ADR-0017).

### Changed
- `persia-engine`: `SegmentWriter::finish` writes every section straight into the segment buffer as it encodes it,
  instead of holding each section and copying it in (ROADMAP 5.1.6): postings are grouped by term as 4-byte indices,
  term lists are measured in one pass from their bit widths and varint lengths, without encoding them (writing the
  `TermInfo` records, ROADMAP 5.1.7), and encoded in one pass per stream that writes them, FOR columns and FSTs are
  generated into the output. Its documentation states the memory a flush needs beyond the builder, for the flush
  threshold of M6; on a 20 000-document segment of 9.6 MB the heap peak above the builder (counting both buffers of
  each reallocation) drops from 43.0 MB to 21.0 MB (`tests/flush_memory.rs`, a counting allocator, ADR-0026).
  `segment/finish`, single runs measured together: 99.0 K documents/s, against 100.3 K before 5.1.6 and 96.2 K when
  the lists were encoded twice. Output is byte for byte the same.
- `persia-format`: `Writer::clear` empties a writer and keeps its buffer.
- `persia-engine` (breaking: new `SegmentError::TermTooLong` variant): a `text` field's tokens are limited to
  `MAX_TERM_LEN` (32 KiB, the `keyword` value limit) whatever the analyzer (ROADMAP 5.4.5, SPEC §5.3.1):
  `SegmentBuilder::add` rejects a document whose analyzer yields a longer token with `SegmentError::TermTooLong`, and
  `Segment::open` treats a `TermDict` path longer than that as corruption in a `text` field as in a `keyword` one, so
  the cost of walking any term dictionary at open is bounded. The built-in analyzers cut tokens at 255 bytes, so no
  segment they wrote changes. ADR-0027 records the rule (decision 12).
- `persia-engine`: `Segment::open` reads through one process-wide cache, `BlockCache::shared` (8 MiB), instead of an
  8 MiB cache per segment, so however many segments a process opens, the cache keeps at most 8 MiB of decoded blocks
  for all of them (ROADMAP 5.3.15; a block a reader is reading outlives its eviction: up to four per posting list,
  one per norms cursor); `Segment::open_with_cache` still takes any cache, and `Segment::cache` returns the one in use.
- `persia-engine` (breaking): `Segment::norms` returns `Norms<'_>`, which reads the `Norms` section a framed block at a
  time through the segment's block cache (ROADMAP 5.3.14) instead of copying and re-checking all `doc_count` bytes on
  each call. `Norms::get` and `Norms::length` return a `Result`: a corrupt block is reported when a norm in it is read.
- `persia-engine`: `NumericColumn::cursor`, `KeywordColumn::cursor` and `Norms::cursor` (ROADMAP 5.3.16) return
  readers for scans (`NumericCursor`, `KeywordCursor`, `NormsCursor`) that keep the block they last decoded to
  themselves: no lock and no allocation per value, and `KeywordCursor::ords` returns a slice of a buffer it reuses.
  Scanning every document of `segment_columns` (docs/benchmarks.md): 214 M numeric keys/s (`NumericColumn::key`: 32 M)
  and 59.5 M keyword ord lists/s (`KeywordColumn::ords`: 10.8 M); `NormsCursor` 1.2 G norms/s, where `Norms::get`, one
  cache lookup per call, reads 9.8 M/s (`segment_read/norms_*`): scoring should read norms through a cursor.
- `persia-format`: `ForBlock::decode_into` unpacks a whole FOR block into a `Vec`, about twice as fast as `iter`
  (`for_block/decode_into`), and `read_for_block` checks a block whose values may overflow with the same unpacker.
- `persia-engine`: `Postings::positions` decodes only the current document's positions (ROADMAP 5.3.13), reading its
  block's positions run forward and skipping the packed groups of documents passed over by their width byte, instead
  of decoding the whole run. Memory now follows one document's term frequency, and the 2^26-positions-per-block limit
  (a resource error beyond it) is gone.
- `persia-engine` (breaking): `NumericColumn` and `KeywordColumn` borrow their segment (`NumericColumn<'s>`,
  `KeywordColumn<'s>`) and read numeric keys and keyword ords from their FOR blocks on demand, through the block cache,
  instead of decoding a whole column (8 bytes per document) when opened (ROADMAP 5.3.12). Opening one decodes its
  presence bitmap and, for a keyword column, its dictionary. So `NumericColumn::key`/`get` and `KeywordColumn::ords`/
  `values` return a `Result` (a corrupt FOR block is reported when one of its values is read), and `ords`/`values`
  return a `Vec`. Each FOR stream keeps its last decoded block (a numeric column has one stream, a keyword column
  two: starts and ords), so a scan decodes each block once; a full scan is still slower than indexing the old `Vec` (30 M against 491 M numeric keys/s, `segment_columns`; ROADMAP 5.3.16).
- `persia-analysis` (breaking): `Tokenizer::tokenize`, `TokenFilter::apply`, `Analyzer::analyze`/`analyze_query` and
  `TokenLengthGuard::apply` take a `TokenStream` instead of `Vec<Token>`, and `SynonymMap::new` takes any term that
  converts into `Vec<Token>`. `TokenStream` keeps the strings of dropped and rewritten tokens, within bounds (at most
  4096 spare strings of up to 1020 bytes, and 4096 token slots once cleared), so analyzing value after value stops
  growing its buffers once it has grown; values beyond those bounds allocate again.
- `persia-engine`: the built-in analyzer list now comes from `persia-analysis` (`BUILTIN_ANALYZERS` lists name and
  version). `fr@1`, `de@1` and `it@1` now name the analyzers of ROADMAP 4.4.10 (see Added); files an earlier
  development build wrote with those names meant other chains (nothing was released with them) and must be reindexed.

### Fixed
- `persia-storage`: a read-only `Container::open` beside a writer in another process no longer fails spuriously: with an
  I/O error when the writer's recovery truncated a torn tail under its scan (and perhaps appended again), or with a
  false "superblock does not fit the container" when the writer committed between its look at the file length and its
  read of the slots (SPEC §4.5.1).
- `persia-storage`: directories are synced on Windows too (`FlushFileBuffers` on a directory handle), so an acknowledged
  local-backend write or container create survives a machine crash there (SPEC §4.5.2).
- `persia-storage`: on Windows the container writer lock is on a `<name>.lock` file beside the container, so readers
  are no longer locked out while a writer is open (SPEC §4.5.1).
- `persia-storage`: a local-backend object whose path is longer than `PATH_MAX` allows (1023 bytes on macOS and the BSDs, 4095 on Linux) is `InvalidInput`
  (`KeyDefect::PathTooLong`) instead of an I/O error.
- `persia-engine`: `Segment::open` checks the structure of each `TermDict` and `IdMap` FST before the `fst` crate reads
  it, so a crafted FST behind a valid CRC is a corruption error instead of a panic or an endless stream (SPEC §5.3).
  An `IdMap` or `keyword` `TermDict` with a path longer than the longest id or keyword value is corruption too, so a
  small FST cannot make open stream for hours (SPEC §5.3.1, §5.3.6); a `text` field's `TermDict` too since its terms
  have a length bound (ROADMAP 5.4.5, see Changed).
- Website: the article column is centered between the left nav and the narrow "On this page" rail, at Starlight's 45rem reading width.
- Website: the "On this page" rail is a fixed width at the viewport edge, and long headings wrap inside it.

### Format
- Segment format 1.0 (SPEC §5.1, §5.3; ADR-0027): footer with a `PSEG` locator (up to 1 MiB; statistics, section
  table, id range, CRC32C), section kinds 1–12 (all block-framed but the two FSTs, which carry a CRC32C trailer),
  56-byte TermInfo records (with per-term `max_tf` and `min_norm`), `tf − 1` frequencies aligned with postings blocks, 40-byte skip entries, per-block position
  runs with a gap of 100 between values, Lucene `SmallFloat` norms, tagged columns (FOR numeric, roaring bool,
  dictionary + ords keyword) and record arrays for rows, ids and blob references. Golden segment in
  `tests/fixtures/segment/v1.0/`.
- Manifest encoding version 1 (SPEC §8.7; ADR-0024): magic `PMNF`, fixed-width little-endian scalar fields, and
  variable-length segment entries (id, doc count, size, object or frame location, optional tombstone sidecar),
  schemas as JSON with pinned ids and analyzer versions, CRC32C trailer. Golden files in `tests/fixtures/manifest/v1/`.
- Document encoding for `DocStore` rows (SPEC §6.4.2; ADR-0023): a deterministic CBOR subset (definite lengths,
  shortest arguments, 64-bit floats, sorted keys), `array(5)[version 1, id, blob reference, fields by id, extra
  fields]`. Decoders reject anything non-canonical. Pinned by golden vectors.
- Integer codecs for segment sections (SPEC §4.1, §5.2.1; ADR-0004): zigzag signed varints, delta coding of
  sorted docids, 128-value bit-packed blocks (SIMD-BP128 layout), postings full and tail blocks, and
  frame-of-reference blocks with order-preserving keys for numeric columns.
- Checksum and hash primitives (SPEC §4.1): CRC32C (one-shot and streaming), xxh3-64/128 (seed 0), and
  `Checksummed<T>` for fixed-size structures whose CRC32C field covers the rest of the region (SPEC §4.2, §4.3).
- Block codecs (SPEC §5.2; ADR-0005): codec byte `0 = None`, `1 = Zstd` (one zstd frame with declared content
  size), with a decompression-bomb guard on every decode.
- Block framing of segment sections (SPEC §5.2; ADR-0006): 16-byte block header (CRC32C, codec, lengths), fixed-size
  blocks over a byte stream, and a CRC-protected block table that records the block size (the bomb limit) and
  each block's end; corrupt blocks are reported with their index. The section table has no `codec` field.
  A block table can be opened from the section's tail alone and blocks decoded from separately fetched bytes
  (object storage). Decoding errors now separate corruption from caller errors and from running out of memory
  (an unallocatable decompression buffer is no longer reported as corrupt data).
- Container header and superblock (SPEC §4.2, §4.3; ADR-0007): `Header` (magic, format version, feature flags,
  database id, page size) and double-buffered `Superblock` slots with CRC32C, and selection of the active slot. An
  unknown `format_major` or `incompat_flags` bit is a new `Unsupported` error class rather than corruption; unknown
  `rocompat_flags` bits open the container read-only; reserved bytes must be zero; two valid slots with the same
  `seq` but different contents are corruption. Each slot is a full 4096-byte page (A at 4096, B at 8192, first frame
  at 12288) so one bad sector cannot lose both (ADR-0008); `page_size` is an advisory alignment hint; a writer never
  lowers `format_minor`.
- Frames (SPEC §4.4; ADR-0009): encode and validate the checksummed records after the superblock, with 8-byte
  padding, a kind registry whose unknown kinds are skipped, and optional zstd payloads that carry their uncompressed
  length and decode under a caller-supplied bomb limit. A frame scanner stops at the first invalid frame and reports a
  clean end, a torn tail (only zeros or nothing follow), corruption (non-zero bytes follow) or a frame with unknown
  flags, which is `Unsupported` and never truncated.
- `frame_seq` (SPEC §4.4; ADR-0013): numbers a container's frames contiguously from 1, continuing across rewrites;
  the superblock gains `next_frame_seq` at offset 48, and recovery treats a frame that breaks the run as corruption.
  The container reader checks the superblock's offsets against the file (SPEC §4.3).

<!--
Sections per release, in this order, only when non-empty:
### Added / ### Changed / ### Deprecated / ### Removed / ### Fixed / ### Security / ### Format
-->

[Unreleased]: https://github.com/tomiok/persiaDB/commits/main
