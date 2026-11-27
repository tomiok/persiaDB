---
title: "Persia DB FAQ"
description: "What Persia DB is, whether it is production ready, how it differs from SQLite FTS5, Tantivy, and Meilisearch, plus crash safety and the Apache-2.0 license."
head:
  - tag: title
    content: "Persia DB FAQ: embedded search, SQLite FTS5, Tantivy, license"
  - tag: meta
    attrs:
      property: og:title
      content: "Persia DB FAQ: embedded search, SQLite FTS5, Tantivy, license"
  - tag: script
    attrs:
      type: application/ld+json
    content: |
      {
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "name": "Persia DB FAQ",
        "url": "https://tomiok.github.io/persiaDB/guide/faq/",
        "mainEntity": [
          {
            "@type": "Question",
            "name": "What is Persia DB?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Persia DB is an embedded, single-file, cloud-native search database written in Rust. The design is “SQLite for search”: open a file, index documents, and get ranked results that, by default, never come back empty. The same segment bytes are specified to live in a local file or in object storage (S3, GCS, or Azure Blob), and to be served by stateless query nodes. It is pre-alpha. The design contract is SPEC.md. When this site and the SPEC disagree, the SPEC wins.\n\nThe public API is the persia crate: Db::open opens a file, a Collection takes documents, and Collection::search returns ranked hits. It is a Git dependency, not a crates.io release. %%LIBRARY_NOW%% Live status is PROGRESS.md.\n\nWhat an embedded search database is, and how embedded full-text search in Rust fits this design, is explained at https://tomiok.github.io/persiaDB/guide/embedded-search/."
            }
          },
          {
            "@type": "Question",
            "name": "Is Persia DB production ready?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. Persia DB is pre-alpha. Its public API (Db, Collection, search) is in the persia crate, with no stability promise yet. The crates are unpublished (publish = false, version 0.0.0). There is no crates.io release and no compatibility promise. The on-disk format stays unstable until v1.0 (roadmap M15, SPEC §15). %%M10_BLOCKED%%\n\nPROGRESS.md is the live counter: %%PROGRESS_SUMMARY%%. A number on this page is that snapshot. For a search feature in production today, use an existing engine. The comparison page says which one fits which job, and it is not a benchmark."
            }
          },
          {
            "@type": "Question",
            "name": "How is Persia DB different from SQLite FTS5, Tantivy, and Meilisearch?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The longer page is Persia DB vs SQLite FTS5, Tantivy, and Meilisearch. It is not a benchmark, and this project does not publish speed numbers.\n\nSQLite FTS5 is a full-text module inside SQLite, a C library. You search with SQL, and the index lives in the SQLite database file. It is a released part of SQLite.\n\nTantivy is a Rust full-text search library. Its index is a directory of segment files, and indexing and serving are your code. It is a released library. Persia DB does not use Tantivy. ADR-0001 forbids depending on another search engine.\n\nMeilisearch is a standalone search server written in Rust. Clients use HTTP, and typo tolerance is on by default. It is a released server. The Persia DB server is planned (milestone M11) and is not written: persia-server is a placeholder binary that exits with not implemented (M11).\n\nThe Persia DB design is an embedded Rust library, one local .persia file, the same segment bytes in object storage, and query relaxation on by default. The query compiler, the filter DSL, and a BM25F scorer run behind Collection::search. %%RELAXATION_CELL%% How a hit is scored is explained at https://tomiok.github.io/persiaDB/guide/ranking/. Facets, sort, and paging are explained at https://tomiok.github.io/persiaDB/guide/faceted-search/. The cascade is explained at https://tomiok.github.io/persiaDB/guide/typo-tolerant-search/. Fuzzy search and did-you-mean are explained at https://tomiok.github.io/persiaDB/guide/fuzzy-search/. Synonym groups and prefix expansion are explained at https://tomiok.github.io/persiaDB/guide/synonym-and-prefix-search/."
            }
          },
          {
            "@type": "Question",
            "name": "Is Persia DB embedded or a server?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The design is an embedded library: in your process, open a file, with no daemon and no config file for the library (SPEC §14). An optional gRPC and HTTP server is planned (milestone M11) and is not implemented. See Server, cloud, and Kubernetes. The longer explanation of that shape is https://tomiok.github.io/persiaDB/guide/embedded-search/.\n\nWhat you can build against today is that library: the persia crate (Db::open, Collection::search), as a Git dependency. The persia command works on the same files: persia import loads JSON Lines into a collection, persia search runs a request, persia verify checks a database read-only, persia gc collects unreferenced blobs, and persia compact, persia stats, persia inspect, persia dump, and persia container files and sweep maintain and print them."
            }
          },
          {
            "@type": "Question",
            "name": "Do filters, facets, sort, and paging exist?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes. %%MATCHING_CELL%% A SearchRequest takes filter, sort, limit, after (a cursor), and facets, and Collection::search runs it. The longer page is https://tomiok.github.io/persiaDB/guide/faceted-search/. How a hit is scored is https://tomiok.github.io/persiaDB/guide/ranking/. Typo tolerance is separate. %%RELAXATION_CELL%%"
            }
          },
          {
            "@type": "Question",
            "name": "How does Persia DB rank search results?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "With BM25F (SPEC section 7.2). Each text field has a weight, k1, and b. The terms that score are the field analyzer's terms. Tokenizers, stemming, and stopwords are explained at https://tomiok.github.io/persiaDB/guide/text-analysis/. The score uses the collection's document count, document frequency, and average field length. Collection::search scores over a snapshot of the collection. Proximity is off unless the request sets SearchRequest::proximity. A relaxed search orders by cascade step first, then by the request sort. The longer page is https://tomiok.github.io/persiaDB/guide/ranking/. %%MATCHING_CELL%%"
            }
          },
          {
            "@type": "Question",
            "name": "What languages and SDKs does Persia DB support?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The implementation language is Rust. The Rust API is the persia crate (a Git dependency), and nothing is published for other languages.\n\nEmbedded bindings are a design only (ADR-0018, Embedded SDKs), scheduled for v1.0 as ROADMAP 14.6, over the persia facade. They share one C ABI, persia-ffi and persia.h. Go uses cgo (and an optional persia_purego tag), TypeScript uses N-API (@persiadb/node), and Python uses cffi or ctypes. There is no Go module, no npm package, and no Python package yet. The directories sdk/persia and sdk/node do not exist. Rust and Go network clients (persia-client, persia-go) are 14.2 and 14.3, compatibility is 14.4, and the TypeScript network client is 14.5, after the server (M11) and sharding (M13). Those directories do not exist either."
            }
          },
          {
            "@type": "Question",
            "name": "What file format does Persia DB use?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "A local database is specified as two files (SPEC §4 and §9). The file name.persia is the container: a 12 KiB prefix (a 4 KiB header and two 4 KiB superblock slots) and then append-only frames. Integers are little-endian. Headers and frames use CRC32C. The file name.persia.blobs is the local content-addressed pack for client bytes such as PDF or images. It is created on the first blob write. https://tomiok.github.io/persiaDB/guide/blobs/ describes that pack, the remote layout, and garbage collection. BlobId and BlobRef are re-exported under persia::. %%BLOB_NOW%%\n\nImplemented today: the container header, superblock, frames (including the streaming encoder), checksums, integer codecs, zstd block compression, and block framing, in persia-format. Opening, appending, committing, recovering, and compacting a container file are implemented in persia-storage. %%ENGINE_SHORT%% S3, GCS, and Azure Blob backends are milestone M12 and are not written. Format version 1.0 is a draft until the v1.0 release (SPEC §15). Details are on File format and storage."
            }
          },
          {
            "@type": "Question",
            "name": "Is Persia DB crash-safe?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The single-file container commit is implemented and crash-tested. SPEC §4.5 orders the writes: data frames, then the manifest frame, then the inactive superblock slot, with an fdatasync at each step. A crash before the new superblock is durable leaves the previous commit. Opening the container runs recovery, keeps that commit, and truncates a torn tail. Create and compaction are specified to leave either no new file or a complete one. Tests inject faults at those steps and kill the process. Recovery must return the old state or the new state, and not a mix of the two.\n\n%%WRITE_PATH_NOW%% Checksums cover the bytes that exist, and fuzz targets cover the parsers that exist. The WAL, commit, writer lock, and persia verify are described at https://tomiok.github.io/persiaDB/guide/durability/."
            }
          },
          {
            "@type": "Question",
            "name": "What license is Persia DB under?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Apache License, Version 2.0. The text is the LICENSE file in the repository root. Copyright and attribution are in NOTICE: Copyright 2026 The Persia DB Authors. There is no second project license. The workspace Cargo.toml sets license to Apache-2.0. The license page describes what that text says about embedding the library in an application. It is a description of the license, not legal advice."
            }
          },
          {
            "@type": "Question",
            "name": "How do I get started with Persia DB?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Add the persia crate as a Git dependency (there is no crates.io release and no cargo add persia), then open a file with Db::open, create a collection, put documents, and call Collection::search. Getting started is that walkthrough, at https://tomiok.github.io/persiaDB/guide/getting-started/.\n\n%%ENGINE_CALLS%% %%M10_BLOCKED%%"
            }
          },
          {
            "@type": "Question",
            "name": "How can I contribute to Persia DB?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Read SPEC.md and CLAUDE.md, then pick one leaf from ROADMAP.md. PROGRESS.md lists what is ready next. One leaf is one pull request. The contributing page lists the checks a pull request has to pass, including format, clippy, tests, and the dependency and progress checks. By participating you agree to the Code of Conduct. Report security issues privately, as described in SECURITY.md."
            }
          }
        ]
      }
---

Persia DB is pre-alpha. These answers describe the repository as of
[PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md):
%%PROGRESS_SUMMARY%%. That file is the live counter.
[SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) wins if a page disagrees with it.
Planned work is marked as planned. Nothing here is a benchmark.

## What is Persia DB?

Persia DB is an embedded, single-file, cloud-native search database written in Rust. The design is "SQLite for search": open a file, index documents, and get ranked results that, by default, never come back empty. The same segment bytes are specified to live in a local file or in object storage (S3, GCS, or Azure Blob), and to be served by stateless query nodes. It is pre-alpha. The design contract is [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md). When this site and the SPEC disagree, the SPEC wins.

The public API is the persia crate: Db::open opens a file, a Collection takes documents, and Collection::search returns ranked hits. It is a Git dependency, not a crates.io release. %%LIBRARY_NOW%% Live status is [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md).

What an embedded search database is, and how embedded full-text search in Rust fits this design, is [What is an embedded search database?](/guide/embedded-search/).

## Is Persia DB production ready?

No. Persia DB is pre-alpha. Its public API (`Db`, `Collection`, `Collection::search`) is in the `persia` crate, with no stability promise yet. The crates are unpublished (`publish = false`, version 0.0.0). There is no crates.io release and no compatibility promise. The on-disk format stays unstable until v1.0 (roadmap M15, SPEC §15). %%M10_BLOCKED%%

[PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) is the live counter: %%PROGRESS_SUMMARY%%. A number on this page is that snapshot. For a search feature in production today, use an existing engine. The [comparison page](/guide/comparison/) says which one fits which job, and it is not a benchmark.

## How is Persia DB different from SQLite FTS5, Tantivy, and Meilisearch?

The longer page is [Persia DB vs SQLite FTS5, Tantivy, and Meilisearch](/guide/comparison/). It is not a benchmark, and this project does not publish speed numbers.

SQLite FTS5 is a full-text module inside SQLite, a C library. You search with SQL, and the index lives in the SQLite database file. It is a released part of SQLite.

Tantivy is a Rust full-text search library. Its index is a directory of segment files, and indexing and serving are your code. It is a released library. Persia DB does not use Tantivy. [ADR-0001](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0001-own-engine-and-allowed-dependencies.md) forbids depending on another search engine.

Meilisearch is a standalone search server written in Rust. Clients use HTTP, and typo tolerance is on by default. It is a released server. The Persia DB server is planned (milestone M11) and is not written: `persia-server` is a placeholder binary that exits with "not implemented (M11)".

The Persia DB design is an embedded Rust library, one local .persia file, the same segment bytes in object storage, and query relaxation on by default. The query compiler, the filter DSL, and a BM25F scorer run behind `Collection::search`. %%RELAXATION_CELL%% How a hit is scored is [BM25F ranking](/guide/ranking/). Facets, sort, and paging are [Faceted search](/guide/faceted-search/). The cascade is [Typo-tolerant search](/guide/typo-tolerant-search/). Fuzzy matching, did-you-mean corrections, and the fallback are [Fuzzy search](/guide/fuzzy-search/). Synonym groups and prefix expansion are [Synonym and prefix search](/guide/synonym-and-prefix-search/).

## Do filters, facets, sort, and paging exist?

Yes. %%MATCHING_CELL%% A `SearchRequest` takes `filter`, `sort`, `limit`, `after` (a cursor), and `facets`, and `Collection::search` runs it. The filter DSL, and how a filter joins a text query without changing the score, is [Filtered full-text search](/guide/filtered-search/). Facets, sort, and paging are [Faceted search](/guide/faceted-search/). How a hit is scored is [BM25F ranking](/guide/ranking/). Typo tolerance is separate: %%RELAXATION_CELL%% See [Typo-tolerant search](/guide/typo-tolerant-search/), [Fuzzy search](/guide/fuzzy-search/), and [Synonym and prefix search](/guide/synonym-and-prefix-search/).

## How does Persia DB rank search results?

With BM25F (SPEC §7.2). Each `text` field has a weight, `k1`, and `b`. The terms that score are the field analyzer's terms. Tokenizers, stemming, and stopwords are [Text analysis](/guide/text-analysis/). The score uses the collection's document count, document frequency, and average field length. `Collection::search` scores over a snapshot of the collection. Proximity is off unless the request sets `SearchRequest::proximity`. A relaxed search orders by cascade step first, then by the request sort. The longer page is [BM25F ranking](/guide/ranking/). %%MATCHING_CELL%%

## Is Persia DB embedded or a server?

The design is an embedded library: in your process, open a file, with no daemon and no config file for the library (SPEC §14). An optional gRPC and HTTP server is planned (milestone M11) and is not implemented. See [Server, cloud, and Kubernetes](/guide/server/). [What is an embedded search database?](/guide/embedded-search/) is the longer explanation of that shape.

What you can build against today is that library: the `persia` crate (`Db::open`, `Collection::search`), as a Git dependency. The `persia` command works on the same files: `persia import` loads JSON Lines into a collection, `persia search` runs a request, `persia verify` checks a database read-only, `persia gc` collects unreferenced blobs, and `persia compact`, `persia stats`, `persia inspect`, `persia dump`, and `persia container files` and `sweep` maintain and print them.

## What languages and SDKs does Persia DB support?

The implementation language is Rust. The Rust API is the `persia` crate (a Git dependency), and nothing is published for other languages.

Embedded bindings are a design only ([ADR-0018](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0018-embedded-sdk-bindings.md), [Embedded SDKs](/guide/sdks/)), scheduled for v1.0 as ROADMAP 14.6, over the `persia` facade. They share one C ABI, `persia-ffi` and `persia.h`. Go uses cgo (and an optional `persia_purego` tag), TypeScript uses N-API (`@persiadb/node`), and Python uses cffi or ctypes. There is no Go module, no npm package, and no Python package yet. The directories `sdk/persia` and `sdk/node` do not exist. Rust and Go network clients (`persia-client`, `persia-go`) are 14.2 and 14.3, compatibility is 14.4, and the TypeScript network client is 14.5, after the server (M11) and sharding (M13). Those directories do not exist either.

## What file format does Persia DB use?

A local database is specified as two files (SPEC §4 and §9). The file `name.persia` is the container: a 12 KiB prefix (a 4 KiB header and two 4 KiB superblock slots) and then append-only frames. Integers are little-endian. Headers and frames use CRC32C. The file `name.persia.blobs` is the local content-addressed pack for client bytes such as PDF or images. It is created on the first blob write. [Blob storage](/guide/blobs/) is that pack, the remote layout, and garbage collection. `BlobId` and `BlobRef` are re-exported under `persia::`. %%BLOB_NOW%%

Implemented today: the container header, superblock, frames (including the streaming encoder), checksums, integer codecs, zstd block compression, and block framing, in `persia-format`. Opening, appending, committing, recovering, and compacting a container file are implemented in `persia-storage`. %%ENGINE_SHORT%% S3, GCS, and Azure Blob backends are milestone M12 and are not written. Format version 1.0 is a draft until the v1.0 release (SPEC §15). Details are on [File format and storage](/guide/file-format/).

## Is Persia DB crash-safe?

The single-file container commit is implemented and crash-tested. SPEC §4.5 orders the writes: data frames, then the manifest frame, then the inactive superblock slot, with an fdatasync at each step. A crash before the new superblock is durable leaves the previous commit. Opening the container runs recovery, keeps that commit, and truncates a torn tail. Create and compaction are specified to leave either no new file or a complete one. Tests inject faults at those steps and kill the process. Recovery must return the old state or the new state, and not a mix of the two.

%%WRITE_PATH_NOW%% Checksums cover the bytes that exist, and fuzz targets cover the parsers that exist. The WAL, commit, writer lock, and `persia verify` are [Durability and crash recovery](/guide/durability/).

## What license is Persia DB under?

Apache License, Version 2.0. The text is the [LICENSE](https://github.com/tomiok/persiaDB/blob/main/LICENSE) file in the repository root. Copyright and attribution are in [NOTICE](https://github.com/tomiok/persiaDB/blob/main/NOTICE): Copyright 2026 The Persia DB Authors. There is no second project license. The workspace Cargo.toml sets `license` to Apache-2.0. The [license page](/guide/license/) describes what that text says about embedding the library in an application. It is a description of the license, not legal advice.

## How do I get started with Persia DB?

Add the `persia` crate as a Git dependency (there is no crates.io release and no `cargo add persia`), then open a file with `Db::open`, create a collection, put documents, and call `Collection::search`. [Getting started](/guide/getting-started/) is that walkthrough.

%%ENGINE_CALLS%% %%M10_BLOCKED%%

## How can I contribute to Persia DB?

Read [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) and [CLAUDE.md](https://github.com/tomiok/persiaDB/blob/main/CLAUDE.md), then pick one leaf from [ROADMAP.md](https://github.com/tomiok/persiaDB/blob/main/ROADMAP.md). [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) lists what is ready next. One leaf is one pull request. The [contributing page](/guide/contributing/) lists the checks a pull request has to pass, including format, clippy, tests, and the dependency and progress checks. By participating you agree to the [Code of Conduct](https://github.com/tomiok/persiaDB/blob/main/CODE_OF_CONDUCT.md). Report security issues privately, as described in [SECURITY.md](https://github.com/tomiok/persiaDB/blob/main/SECURITY.md).
