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
              "text": "Persia DB is an embedded, single-file, cloud-native search database written in Rust. The design is “SQLite for search”: open a file, index documents, and get ranked results that, by default, never come back empty. The same segment bytes are specified to live in a local file or in object storage (S3, GCS, or Azure Blob), and to be served by stateless query nodes. It is pre-alpha. The design contract is SPEC.md. When this site and the SPEC disagree, the SPEC wins.\n\nWhat the repository contains today is library code, not a search you can call. persia-format implements the on-disk codecs. persia-storage implements the Storage trait, memory and local-file backends, and the single-file container. persia-engine implements collection schema, documents, and manifest types. persia-analysis implements tokenizers and built-in analyzers. None of that indexes a document into a segment or runs a query. The public persia crate exports no Db and no Collection. Live status is PROGRESS.md.\n\nWhat an embedded search database is, and how embedded full-text search in Rust fits this design, is explained at https://tomiok.github.io/persiaDB/guide/embedded-search/."
            }
          },
          {
            "@type": "Question",
            "name": "Is Persia DB production ready?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. Persia DB is pre-alpha, and it does not answer a query. The crates are unpublished (publish = false, version 0.0.0). There is no crates.io release and no compatibility promise. The on-disk format stays unstable until v1.0 (roadmap M15, SPEC §15). The first release a caller is supposed to use is v0.1 (M10), and that milestone is blocked on the write path, query, relaxation, and blobs.\n\nPROGRESS.md is the live counter: %%PROGRESS_SUMMARY%%. A number on this page is that snapshot. For a search feature in production today, use an existing engine. The comparison page says which one fits which job, and it is not a benchmark."
            }
          },
          {
            "@type": "Question",
            "name": "How is Persia DB different from SQLite FTS5, Tantivy, and Meilisearch?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The longer page is Persia DB vs SQLite FTS5, Tantivy, and Meilisearch. It is not a benchmark, and this project does not publish speed numbers.\n\nSQLite FTS5 is a full-text module inside SQLite, a C library. You search with SQL, and the index lives in the SQLite database file. It is a released part of SQLite.\n\nTantivy is a Rust full-text search library. Its index is a directory of segment files, and indexing and serving are your code. It is a released library. Persia DB does not use Tantivy. ADR-0001 forbids depending on another search engine.\n\nMeilisearch is a standalone search server written in Rust. Clients use HTTP, and typo tolerance is on by default. It is a released server. The Persia DB server is planned (milestone M11) and is not written: persia-server is an empty main function.\n\nThe Persia DB design is an embedded Rust library, one local .persia file, the same segment bytes in object storage, and query relaxation on by default. Those search features are specified in the SPEC. They are not shipped."
            }
          },
          {
            "@type": "Question",
            "name": "Is Persia DB embedded or a server?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The design is an embedded library: in your process, open a file, with no daemon and no config file for the library (SPEC §14). An optional gRPC and HTTP server is planned (milestone M11) and is not implemented. See Server, cloud, and Kubernetes. The longer explanation of that shape is https://tomiok.github.io/persiaDB/guide/embedded-search/.\n\nWhat you can build against today is still a library, not that server. persia-format encodes and decodes on-disk bytes. persia-storage opens a single-file container and implements memory and local-file object stores. The persia command lists and removes temporary and quarantine files next to a container or a local store (persia container files, persia container sweep, persia store files, persia store sweep). It does not import documents or search. There is no Db::open."
            }
          },
          {
            "@type": "Question",
            "name": "What languages and SDKs does Persia DB support?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The implementation language is Rust, and nothing is published for other languages. The embedded API crate persia exports no types, so there is no Rust database API to wrap yet.\n\nEmbedded Go and TypeScript bindings are a design only (ADR-0018, Embedded SDKs). There is no Go module and no npm package. The directories sdk/persia and sdk/node do not exist. Rust and Go network clients (persia-client, persia-go) are milestone M14, after the server (M11) and sharding (M13). Those directories do not exist either."
            }
          },
          {
            "@type": "Question",
            "name": "What file format does Persia DB use?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "A local database is specified as two files (SPEC §4 and §9). The file name.persia is the container: a 12 KiB prefix (a 4 KiB header and two 4 KiB superblock slots) and then append-only frames. Integers are little-endian. Headers and frames use CRC32C. The file name.persia.blobs is a planned content-addressed pack for client bytes such as PDF or images. BlobId and BlobRef exist as types in persia-format. The blob store is milestone M9 and has not been started. persia-blob exports no API.\n\nImplemented today: the container header, superblock, frames (including the streaming encoder), checksums, integer codecs, zstd block compression, and block framing, in persia-format. Opening, appending, committing, recovering, and compacting a container file are implemented in persia-storage. Segment sections (the term dictionary, postings, and stored documents) are milestone M5 and are not written. S3, GCS, and Azure Blob backends are milestone M12 and are not written. Format version 1.0 is a draft until the v1.0 release (SPEC §15). Details are on File format and storage."
            }
          },
          {
            "@type": "Question",
            "name": "Is Persia DB crash-safe?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "The single-file container commit is implemented and crash-tested. SPEC §4.5 orders the writes: data frames, then the manifest frame, then the inactive superblock slot, with an fdatasync at each step. A crash before the new superblock is durable leaves the previous commit. Opening the container runs recovery, keeps that commit, and truncates a torn tail. Create and compaction are specified to leave either no new file or a complete one. Tests inject faults at those steps and kill the process. Recovery must return the old state or the new state, and not a mix of the two.\n\nThat guarantee is the container, not a search database. There is no document write path yet: no memtable, no engine WAL replay, and no index commit (milestone M6). You cannot store a searchable document and recover it. Checksums cover the bytes that exist, and fuzz targets cover the parsers that exist."
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
              "text": "You cannot add Persia DB as a database yet. There is no cargo add persia. Getting started separates the library code you can call from the illustrative Db::open and search API, which is not in the crate.\n\nA git dependency can use persia-format for the byte codecs and persia-storage for the container and the memory and local stores. Collection schema types live in persia-engine. They validate a schema. They do not index or search. The milestone that is supposed to ship the embedded API is v0.1 (M10)."
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

What the repository contains today is library code, not a search you can call. `persia-format` implements the on-disk codecs. `persia-storage` implements the `Storage` trait, memory and local-file backends, and the single-file container. `persia-engine` implements collection schema, documents, and manifest types. `persia-analysis` implements tokenizers and built-in analyzers. None of that indexes a document into a segment or runs a query. The public `persia` crate exports no `Db` and no `Collection`. Live status is [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md).

What an embedded search database is, and how embedded full-text search in Rust fits this design, is [What is an embedded search database?](/guide/embedded-search/).

## Is Persia DB production ready?

No. Persia DB is pre-alpha, and it does not answer a query. The crates are unpublished (`publish = false`, version 0.0.0). There is no crates.io release and no compatibility promise. The on-disk format stays unstable until v1.0 (roadmap M15, SPEC §15). The first release a caller is supposed to use is v0.1 (M10), and that milestone is blocked on the write path, query, relaxation, and blobs.

[PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) is the live counter: %%PROGRESS_SUMMARY%%. A number on this page is that snapshot. For a search feature in production today, use an existing engine. The [comparison page](/guide/comparison/) says which one fits which job, and it is not a benchmark.

## How is Persia DB different from SQLite FTS5, Tantivy, and Meilisearch?

The longer page is [Persia DB vs SQLite FTS5, Tantivy, and Meilisearch](/guide/comparison/). It is not a benchmark, and this project does not publish speed numbers.

SQLite FTS5 is a full-text module inside SQLite, a C library. You search with SQL, and the index lives in the SQLite database file. It is a released part of SQLite.

Tantivy is a Rust full-text search library. Its index is a directory of segment files, and indexing and serving are your code. It is a released library. Persia DB does not use Tantivy. [ADR-0001](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0001-own-engine-and-allowed-dependencies.md) forbids depending on another search engine.

Meilisearch is a standalone search server written in Rust. Clients use HTTP, and typo tolerance is on by default. It is a released server. The Persia DB server is planned (milestone M11) and is not written: `persia-server` is an empty main function.

The Persia DB design is an embedded Rust library, one local .persia file, the same segment bytes in object storage, and query relaxation on by default. Those search features are specified in the SPEC. They are not shipped.

## Is Persia DB embedded or a server?

The design is an embedded library: in your process, open a file, with no daemon and no config file for the library (SPEC §14). An optional gRPC and HTTP server is planned (milestone M11) and is not implemented. See [Server, cloud, and Kubernetes](/guide/server/). [What is an embedded search database?](/guide/embedded-search/) is the longer explanation of that shape.

What you can build against today is still a library, not that server. `persia-format` encodes and decodes on-disk bytes. `persia-storage` opens a single-file container and implements memory and local-file object stores. The `persia` command lists and removes temporary and quarantine files next to a container or a local store (`persia container files`, `persia container sweep`, `persia store files`, `persia store sweep`). It does not import documents or search. There is no `Db::open`.

## What languages and SDKs does Persia DB support?

The implementation language is Rust, and nothing is published for other languages. The embedded API crate `persia` exports no types, so there is no Rust database API to wrap yet.

Embedded Go and TypeScript bindings are a design only ([ADR-0018](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0018-embedded-sdk-bindings.md), [Embedded SDKs](/guide/sdks/)). There is no Go module and no npm package. The directories `sdk/persia` and `sdk/node` do not exist. Rust and Go network clients (`persia-client`, `persia-go`) are milestone M14, after the server (M11) and sharding (M13). Those directories do not exist either.

## What file format does Persia DB use?

A local database is specified as two files (SPEC §4 and §9). The file `name.persia` is the container: a 12 KiB prefix (a 4 KiB header and two 4 KiB superblock slots) and then append-only frames. Integers are little-endian. Headers and frames use CRC32C. The file `name.persia.blobs` is a planned content-addressed pack for client bytes such as PDF or images. `BlobId` and `BlobRef` exist as types in `persia-format`. The blob store is milestone M9 and has not been started. `persia-blob` exports no API.

Implemented today: the container header, superblock, frames (including the streaming encoder), checksums, integer codecs, zstd block compression, and block framing, in `persia-format`. Opening, appending, committing, recovering, and compacting a container file are implemented in `persia-storage`. Segment sections (the term dictionary, postings, and stored documents) are milestone M5 and are not written. S3, GCS, and Azure Blob backends are milestone M12 and are not written. Format version 1.0 is a draft until the v1.0 release (SPEC §15). Details are on [File format and storage](/guide/file-format/).

## Is Persia DB crash-safe?

The single-file container commit is implemented and crash-tested. SPEC §4.5 orders the writes: data frames, then the manifest frame, then the inactive superblock slot, with an fdatasync at each step. A crash before the new superblock is durable leaves the previous commit. Opening the container runs recovery, keeps that commit, and truncates a torn tail. Create and compaction are specified to leave either no new file or a complete one. Tests inject faults at those steps and kill the process. Recovery must return the old state or the new state, and not a mix of the two.

That guarantee is the container, not a search database. There is no document write path yet: no memtable, no engine WAL replay, and no index commit (milestone M6). You cannot store a searchable document and recover it. Checksums cover the bytes that exist, and fuzz targets cover the parsers that exist.

## What license is Persia DB under?

Apache License, Version 2.0. The text is the [LICENSE](https://github.com/tomiok/persiaDB/blob/main/LICENSE) file in the repository root. Copyright and attribution are in [NOTICE](https://github.com/tomiok/persiaDB/blob/main/NOTICE): Copyright 2026 The Persia DB Authors. There is no second project license. The workspace Cargo.toml sets `license` to Apache-2.0. The [license page](/guide/license/) describes what that text says about embedding the library in an application. It is a description of the license, not legal advice.

## How do I get started with Persia DB?

You cannot add Persia DB as a database yet. There is no `cargo add persia`. [Getting started](/guide/getting-started/) separates the library code you can call from the illustrative `Db::open` and `search` API, which is not in the crate.

A git dependency can use `persia-format` for the byte codecs and `persia-storage` for the container and the memory and local stores. Collection schema types live in `persia-engine`. They validate a schema. They do not index or search. The milestone that is supposed to ship the embedded API is v0.1 (M10).

## How can I contribute to Persia DB?

Read [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) and [CLAUDE.md](https://github.com/tomiok/persiaDB/blob/main/CLAUDE.md), then pick one leaf from [ROADMAP.md](https://github.com/tomiok/persiaDB/blob/main/ROADMAP.md). [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) lists what is ready next. One leaf is one pull request. The [contributing page](/guide/contributing/) lists the checks a pull request has to pass, including format, clippy, tests, and the dependency and progress checks. By participating you agree to the [Code of Conduct](https://github.com/tomiok/persiaDB/blob/main/CODE_OF_CONDUCT.md). Report security issues privately, as described in [SECURITY.md](https://github.com/tomiok/persiaDB/blob/main/SECURITY.md).
