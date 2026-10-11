---
title: "Code"
description: "What persia-format actually exports today, with links to the source. The embedded Db API is still a stub."
---

This page quotes code that is on `main`. It does not describe a database you can open.
`persia-format` is the only crate with an implementation. The others document the job they will
do and stop there. The `Db::open` sketch on the [embedded API](/guide/embedded-api/) page is
illustrative: those methods are not in the tree.

## Crate layout

The workspace members are the directories under `crates/` plus `xtask`
([root Cargo.toml](https://github.com/tomiok/persiaDB/blob/main/Cargo.toml)).
What each file says about itself:

| Crate | Source | What the file says | In the tree |
|---|---|---|---|
| `persia-format` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/lib.rs) | On-disk primitives: encodings, frames, superblock, checksums. No I/O policy. | Encodings, checksums, and block codecs. Frames and the superblock are not written yet. |
| `persia` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs) | The public embedded API (`Db`, `Collection`). | The module docs only. No `Db` type. |
| `persia-engine` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/lib.rs) | Segments, indexing, WAL, query, scoring, relaxation. Synchronous. | Module docs only. |
| `persia-storage` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/lib.rs) | `Storage` trait and backends (SPEC §10). | Module docs only. |
| `persia-analysis` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-analysis/src/lib.rs) | Tokenizers, normalizers, stemmers (SPEC §6.3). | Module docs only. |
| `persia-blob` | [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-blob/src/lib.rs) | Content-addressed chunks, dedup, GC (SPEC §9). | Module docs only. |

`persia-server` and `persia-cli` are empty `main` functions. `persia-proto` is a stub library.
The public modules and re-exports of `persia-format` are the list in
[lib.rs lines 10–30](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/lib.rs#L10-L30):

```rust
pub mod align;
pub mod bitpack;
pub mod checksum;
pub mod checksummed;
pub mod compress;
pub mod delta;
pub mod error;
pub mod for_codec;
pub mod hash;
pub mod offset;
pub mod postings;
pub mod reader;
pub mod varint;
pub mod writer;
pub mod zigzag;

pub use align::ALIGNMENT;
pub use error::{Corruption, EncodeError, Error, Result};
pub use offset::{Len, Offset};
pub use reader::Reader;
pub use writer::Writer;
```

Integer codecs (`delta`, `bitpack`, `postings`, `for_codec`), varints, zigzag, and the
bounds-checked `Reader` / `Writer` live in those modules. They are library primitives. They are
not wired into a segment writer (that is roadmap M5).

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
The header and the superblock slots are the intended callers (SPEC §4.2, §4.3). Those two
structures are not encoded yet. The helper is. A type describes where the CRC field sits:

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
Frames are variable-length and do not use this helper. The frame scanner is roadmap 1.7.

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
`uncompressed_len`. The on-disk block header, the block table, and random-access reads are
roadmap 1.5. They are not in this file. One follow-up is still open: allocation failure is
reported as corruption until a resource error class exists (roadmap 1.4.4).

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
