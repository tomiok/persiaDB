---
title: "Key concepts"
description: "The names Persia uses for a database, a segment, a commit, and relaxation."
head:
  - tag: title
    content: "Database, segment, commit, and query relaxation | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Database, segment, commit, and query relaxation | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Database, segment, commit, and query relaxation | PersiaDB"
---

Names below are the ones used in code and in the SPEC. The short definitions are
[SPEC §2](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#2-terminology).

| Term | Meaning |
|---|---|
| **Database** | One Persia DB: collections plus blobs. Opened by a filesystem path or a URL. |
| **Collection** | Documents that share a schema, analyzers, and ranking config. |
| **Document** | A record with a string `id`, typed fields, and an optional blob reference. |
| **Segment** | An immutable, self-contained index for one collection (SPEC §5). Once written, it is never edited. |
| **Memtable** | The in-memory buffer of recent writes. Searches see it, so a `put` is visible before the next flush. |
| **WAL** | Write-ahead log of operations not yet flushed into a segment. |
| **Manifest** | A versioned snapshot: live segments per collection, schemas, and the WAL position still needed. |
| **Commit** | Atomic publication of a new manifest. Readers move from one manifest to the next; they never see a mix. |
| **Frame** | One checksummed record in the local container (SPEC §4.4): WAL chunk, segment, manifest, padding, or tombstone sidecar. |
| **Blob** | An opaque client file, addressed by the SHA-256 of its bytes and stored in chunks (SPEC §9, ADR-0002). |
| **Shard** | An independent database that owns one hash range of document ids (SPEC §11). |
| **Relaxation** | A controlled loosening of a query so the default experience is not an empty page (SPEC §7.6). The cascade is not implemented. See [Typo-tolerant search](/guide/typo-tolerant-search/). |

A few more words that show up in the roadmap and in contributor docs
([`docs/architecture.md`](https://github.com/tomiok/persiaDB/blob/main/docs/architecture.md)):

| Term | Meaning |
|---|---|
| **Leaf** | One ROADMAP checkbox. One leaf is one issue and one pull request. |
| **Milestone** | `M0` through `M15` are the MVP (v1.0). `M16` and later are post-MVP and name a target version. A milestone starts only after the milestones it depends on are done, unless the roadmap says otherwise. |
| **ADR** | An architecture decision record in [`docs/adr/`](https://github.com/tomiok/persiaDB/blob/main/docs/adr/). The SPEC says what the system does; an ADR says why. |
| **Relaxation level** | One step of the cascade, `L0` (exact) through `L5` (fallback). |

Internal document ids are segment-local. Deletions never rewrite a segment: they live in a tombstone
sidecar referenced by the manifest (SPEC §8.3). The exact sidecar bytes are still an ADR (ROADMAP 6.4.2,
SPEC §18.3).
