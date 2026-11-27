---
title: "Code"
description: "What the Persia DB crates export today, with links to the source: the public persia API (Db, Collection, search), the engine crates under it, and the on-disk codecs."
head:
  - tag: title
    content: "Source guide to the Rust crates that exist today | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Source guide to the Rust crates that exist today | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Source guide to the Rust crates that exist today | PersiaDB"
---

This page quotes code that is on `main`. %%PRE_ALPHA%%
`persia` is the crate an application calls: the embedded API (`Db`, `Collection`, `SearchRequest`, blobs, and `ErrorCode`). The crates under it (`persia-engine`, `persia-blob`, `persia-storage`, `persia-format`, `persia-analysis`) are library code in the same repository and are not stable APIs. The second half of this page quotes the on-disk codecs of `persia-format`, as engine internals.

## The public API

`persia` is the facade of [ADR-0063](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0063-public-surface-of-the-embedded-api.md). Its surface is committed as [`crates/persia/public-api.txt`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/public-api.txt), regenerated with every change that moves it, and `cargo doc -p persia --open` in a clone shows its documentation.

| Type | What it is |
|---|---|
| `Db` | One database file. `Db::open`, `OpenOptions::open` for the settings, `create_collection`, `create_collection_json`, `collection`, `collection_names`, `drop_collection`, `commit`, `refresh`, `backup`, `close`, and the blobs: `put_blob`, `get_blob`, `get_blob_range` |
| `Collection` | One collection. `put`, `put_batch`, `put_json`, `put_struct`, `put_with_blob`, `get`, `get_json`, `get_struct`, `delete`, `commit`, `search`, `search_json`, `schema` |
| `SearchRequest` | A search built in code: `SearchRequest::new(q)` and its builders (`filter`, `filter_json`, `soft_filter`, `sort`, `limit`, `after`, `facets`, `explain`, `relax`, ...) |
| `SearchResponse`, `Hit`, `Relaxation` | What a search returns: ranked hits, facet counts, and how the query was relaxed |
| `Error`, `ErrorCode` | One opaque error with a stable code (`ErrorCode::NotFound`, `AlreadyExists`, `Conflict`, `InvalidArgument`, `Corrupt`, ...) |
| `AsyncDb`, `AsyncCollection` | With the cargo feature `async`: the same calls as futures that need no runtime |

The schema, document, and request value types (`CollectionSchema`, `SchemaBuilder`, `Document`, `Value`, `Filter`, `Sort`, `FacetRequest`, `Durability`, `BlobRef`, and the rest) are re-exported from the engine under `persia::` (ADR-0063 item 2).

The sample opens a database in a temporary directory (the `tempfile` crate); any path works.

```rust
use persia::{Db, ErrorCode, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("app.persia"))?;
    let schema = r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#;
    let notes = db.create_collection_json(schema)?;
    notes.put_json(r#"{"id": "n1", "body": "an embedded search database"}"#)?;
    notes.put_json(r#"{"id": "n2", "body": "a single file"}"#)?;

    let page = notes.search(&SearchRequest::new("embedded"))?;
    let ids: Vec<&str> = page.hits.iter().map(|h| h.document.id()).collect();
    assert_eq!(ids, ["n1"]);

    // Every call returns `persia::Result`; its error carries a stable code.
    match db.create_collection_json(schema) {
        Err(e) => assert_eq!(e.code(), ErrorCode::AlreadyExists),
        Ok(_) => return Err("a second create of the same collection succeeded".into()),
    }
    assert_eq!(ErrorCode::AlreadyExists.as_str(), "PERSIA_ALREADY_EXISTS");

    db.close()?;
    Ok(())
}
```

%%ENGINE_CALLS%% [Blob storage](/guide/blobs/) is `put_blob` and the pack file.

## Crate layout

The workspace members are the directories under `crates/` plus `xtask`
([root Cargo.toml](https://github.com/tomiok/persiaDB/blob/main/Cargo.toml)).
What each file says about itself:

| Crate | Source | What the file says | In the tree |
|---|---|---|---|
| `persia` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs) | The public embedded API. | `Db`, `Collection`, `SearchRequest`, blobs, `Error` and `ErrorCode`, and `AsyncDb` with the `async` feature. |
| `persia-format` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/lib.rs) | On-disk primitives: encodings, frames, superblock, checksums. No I/O policy. | Encodings, checksums, block codecs and framing, the container header, the superblock, frames (including the streaming encoder), mmap, `BlobId`, and `BlobRef`. |
| `persia-engine` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/lib.rs) | Segments, indexing, WAL, query, scoring, relaxation. Synchronous. | %%ENGINE_NOW%% |
| `persia-storage` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/lib.rs) | `Storage` trait and backends (SPEC §10). | The trait, memory, local file, and the single-file container. No S3, GCS, or Azure. |
| `persia-analysis` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-analysis/src/lib.rs) | Tokenizers, normalizers, stemmers (SPEC §6.3). | Tokenizers, filters, `en` / `es` / `pt` / `fr` / `de` / `it` stemmers, and built-in analyzers. |
| `persia-blob` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-blob/src/lib.rs) | Content-addressed chunks, dedup, GC (SPEC §9). | %%BLOB_CELL%% |

`persia-server` is a placeholder binary that prints "not implemented (M11)" and exits with status 1. `persia-proto` is a placeholder library for M11. `persia-cli` is the `persia` command: `persia import` loads JSON Lines into a collection, `persia search` runs a request, `persia gc` collects unreferenced blobs, `persia verify` checks a whole database read-only, `persia stats` prints its statistics, `persia compact` rewrites a container without its dead frames, `persia inspect` and `persia dump` print a container, a segment, or a collection's documents, `persia container files` and `sweep` list and remove stale temporary files, and `persia bench` runs a local micro-benchmark.

<!-- engine-internals -->
The rest of this page quotes engine internals of `persia-format`, not API to call: an application reaches the format through `persia`, and these modules, types, and errors (`persia_format`'s own `Error`, not `persia::Error`) may change.

The public modules and re-exports of `persia-format` are the list in
[lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/lib.rs):

```text
pub mod align;
pub mod bitpack;
pub mod blob;
pub mod block;
pub mod checksum;
pub mod checksummed;
pub mod compress;
pub mod delta;
pub mod error;
pub mod for_codec;
pub mod frame;
pub mod hash;
pub mod header;
pub mod mmap;
pub mod offset;
pub mod postings;
pub mod reader;
pub mod source;
pub mod superblock;
pub mod varint;
pub mod writer;
pub mod zigzag;

pub use align::ALIGNMENT;
pub use blob::{BlobId, BlobRef};
pub use error::{Corruption, EncodeError, Error, Result, SuperblockField, Unsupported};
pub use offset::{Len, Offset};
pub use reader::Reader;
pub use source::ByteSource;
pub use writer::Writer;
```

Integer codecs (`delta`, `bitpack`, `postings`, `for_codec`), varints, zigzag, and the
bounds-checked `Reader` / `Writer` live in those modules. They are library primitives. Segment
writers in `persia-engine` use them (M5).

## Checksums

CRC32C is the framing checksum (SPEC §4.1). The wrapper is
[checksum.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/checksum.rs).
One-shot and streaming:

```text
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c::crc32c(data)
}
```

The streaming hasher is the same file, through `finish` (lines 51–70; `verify` follows):

```text
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Crc32c {
    state: u32,
}

impl Crc32c {
    /// A checksum of no bytes.
    pub const fn new() -> Self {
        Self { state: 0 }
    }

    /// Appends `data`.
    pub fn update(&mut self, data: &[u8]) {
        self.state = crc32c::crc32c_append(self.state, data);
    }

    /// The checksum of everything appended so far. Does not reset: more data can still be appended.
    pub const fn finish(self) -> u32 {
        self.state
    }
```

A mismatch is `Error::Corrupt` with `Corruption::ChecksumMismatch`, not a panic
([verify_crc32c](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/checksum.rs#L23-L25)).

xxh3-64 and xxh3-128, seed 0, are the content and routing hashes. They are a different tool from
CRC32C, and blob ids are SHA-256 on purpose
([hash.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/hash.rs),
[ADR-0002](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0002-blob-hash-sha256.md)):

```text
pub fn xxh3_64(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}
```

## Checksummed\<T\>

Fixed-size structures whose CRC sits inside the region use
[`Checksummed<T>`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/checksummed.rs#L98-L99).
The container header and the superblock slots use it (SPEC §4.2, §4.3; `header.rs`, `superblock.rs`). A type describes where the CRC field sits:

```text
pub trait ChecksummedLayout: Sized {
    /// Total size of the region in bytes, CRC field included.
    const LEN: usize;
    /// Position of the `u32` CRC32C field within the region.
    const CRC_OFFSET: usize;

    fn encode_fields(&self, w: &mut Writer);
    fn decode_fields(r: &mut Reader<'_>) -> Result<Self>;
}
```

```text
pub struct Checksummed<T>(pub T);
```

`encode` writes the fields, checksums every byte except the CRC field, and fills that field in.
`decode` checks the CRC before it interprets the fields
([checksummed.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/checksummed.rs#L42-L168)).
Frames are variable-length and do not use this helper. Encode, decode, and scan live in `frame.rs`.

## Block codecs

[compress.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/compress.rs)
defines the codec byte from SPEC §5.2 and
[ADR-0005](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0005-block-compression-codecs.md):

```text
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Codec {
    /// Stored as is.
    None = 0,
    /// One zstd frame that declares its content size.
    Zstd = 1,
}
```

Any other byte is `Error::Corrupt` with `Corruption::UnknownCodec`.

```text
pub fn compress_block(
    codec: Codec,
    level: i32,
    src: &[u8],
    out: &mut Vec<u8>,
) -> Result<usize, EncodeError> {
    match codec {
        Codec::None => {
            out.extend_from_slice(src);
            Ok(src.len())
        }
        Codec::Zstd => compress_zstd(level, src, out),
    }
}
```

Decompression takes the declared length and a caller-supplied maximum, and rejects a larger
length before it allocates
([decompress_block](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/compress.rs#L146-L153)):

```text
pub fn decompress_block(
    codec: Codec,
    src: &[u8],
    uncompressed_len: u64,
    max_len: usize,
    offset: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
```

A zstd payload has to be exactly one frame whose declared content size equals
`uncompressed_len`. The on-disk block header, the block table, and random-access reads live in `block.rs` (roadmap 1.5).
An allocation failure while decoding is `Error::OutOfMemory`, not corruption (roadmap 1.4.4).

<!-- /engine-internals -->
