---
title: "Code"
description: "What the Persia DB crates export today, with links to the source. The public Db API does not exist yet. The engine has a WAL, commit, and segment merging, and nothing searches."
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
`persia-format` is the on-disk codec crate. `persia-storage`, `persia-engine`, and `persia-analysis` also export library code, summarized below.
`persia` does not. The `Db::open` sketch on the [embedded API](/guide/embedded-api/) page is
illustrative: `persia::Db` is not in the tree. %%ENGINE_CALLS%%

## Crate layout

The workspace members are the directories under `crates/` plus `xtask`
([root Cargo.toml](https://github.com/tomiok/persiaDB/blob/main/Cargo.toml)).
What each file says about itself:

| Crate | Source | What the file says | In the tree |
|---|---|---|---|
| `persia-format` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/lib.rs) | On-disk primitives: encodings, frames, superblock, checksums. No I/O policy. | Encodings, checksums, block codecs and framing, the container header, the superblock, frames (including the streaming encoder), mmap, `BlobId`, and `BlobRef`. |
| `persia` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs) | The public embedded API (`Db`, `Collection`). | The module docs only. No `Db` type. |
| `persia-engine` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/lib.rs) | Segments, indexing, WAL, query, scoring, relaxation. Synchronous. | %%ENGINE_NOW%% |
| `persia-storage` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/lib.rs) | `Storage` trait and backends (SPEC §10). | The trait, memory, local file, and the single-file container. No S3, GCS, or Azure. |
| `persia-analysis` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-analysis/src/lib.rs) | Tokenizers, normalizers, stemmers (SPEC §6.3). | Tokenizers, filters, `en` / `es` / `pt` / `fr` / `de` / `it` stemmers, and built-in analyzers. |
| `persia-blob` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-blob/src/lib.rs) | Content-addressed chunks, dedup, GC (SPEC §9). | Module docs only. |

`persia-server` is an empty `main`. `persia-cli` lists and sweeps temporary and quarantine files, and it inspects and dumps one segment (`persia inspect segment`, `persia dump segment`, with `--verify` on inspect). It does not search. `persia-proto` is a stub library.
The public modules and re-exports of `persia-format` are the list in
[lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/lib.rs):

```rust
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
writers in `persia-engine` use them (M5). There is still no query.

## Checksums

CRC32C is the framing checksum (SPEC §4.1). The wrapper is
[checksum.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/checksum.rs).
One-shot and streaming:

```rust
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c::crc32c(data)
}
```

The streaming hasher is the same file, through `finish` (lines 51–70; `verify` follows):

```rust
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

```rust
pub fn xxh3_64(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}
```

## Checksummed\<T\>

Fixed-size structures whose CRC sits inside the region use
[`Checksummed<T>`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/checksummed.rs#L98-L99).
The container header and the superblock slots use it (SPEC §4.2, §4.3; `header.rs`, `superblock.rs`). A type describes where the CRC field sits:

```rust
pub trait ChecksummedLayout: Sized {
    /// Total size of the region in bytes, CRC field included.
    const LEN: usize;
    /// Position of the `u32` CRC32C field within the region.
    const CRC_OFFSET: usize;

    fn encode_fields(&self, w: &mut Writer);
    fn decode_fields(r: &mut Reader<'_>) -> Result<Self>;
}
```

```rust
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

```rust
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

```rust
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

```rust
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

## What is not here

Opening `app.persia`, putting a document, and searching are the embedded API in the design.
The crate that will own them currently contains this, and nothing else
([crates/persia/src/lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs)):

```rust
//! Persia DB: an embedded, single-file, cloud-native search database.
//!
//! This is the public embedded API (`Db`, `Collection`).
#![forbid(unsafe_code)]
```
