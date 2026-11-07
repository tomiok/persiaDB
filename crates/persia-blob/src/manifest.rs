//! The blob manifest: a blob's chunk list, whose durable write binds a verified `blob_id` to the chunks staged under
//! an upload id (SPEC §9.1.1, §9.2; ADR-0034, ADR-0035).
//!
//! ```text
//! 0   4    magic       bytes "PBLM"
//! 4   4    version     u32 = 1 (the encoding)
//! 8   4    flags       u32, all bits reserved (0)
//! 12  32   blob_id     SHA-256 of the blob's bytes
//! 44  8    size        u64, the blob's length in bytes
//! 52  4    chunk_size  u32, 1..=64 MiB
//! 56  16   upload_id   u128, the upload the chunks were staged under
//! 72  4·n  chunk_crc   u32 per chunk, n = ceil(size / chunk_size)
//! ..  4    crc32c      u32 over every byte before it
//! ```
//!
//! ```
//! use persia_blob::{BlobManifest, ChunkSize, UploadId, blob_id_of};
//! use persia_format::checksum::crc32c;
//!
//! let data = b"hello, chunks";
//! let crcs = data.chunks(5).map(crc32c).collect();
//! let m = BlobManifest::new(blob_id_of(data), 13, ChunkSize::new(5)?, UploadId::new(7), crcs)?;
//! let bytes = m.encode();
//! assert_eq!(bytes.len(), BlobManifest::encoded_len_for(13, ChunkSize::new(5)?)?);
//! assert_eq!(BlobManifest::decode(&bytes)?, m);
//! assert_eq!(m.chunk_range(2), Some(10..13));
//! # Ok::<(), persia_blob::Error>(())
//! ```

use core::fmt;
use core::ops::Range;

use persia_format::checksum::crc32c;
use persia_format::widen::{u64_from_usize, usize_from_u32};
use persia_format::{BlobId, BlobRef, Reader, Writer};

use crate::chunking::ChunkSize;
use crate::error::{Error, Result};
use crate::hash::blob_id_of;

/// `"PBLM"`: the first bytes of every blob manifest.
pub const MAGIC: [u8; 4] = *b"PBLM";
/// The encoding version this build writes and reads.
pub const VERSION: u32 = 1;
/// The name errors give the encoding.
const ENCODING: &str = "blob manifest";
/// The `flags` bits this build knows: none (ADR-0034 item 8 reserves the field for ROADMAP 9.3.3).
pub const KNOWN_FLAGS: u32 = 0;

/// Magic, version, flags, blob id, size, chunk size, upload id.
const HEAD_LEN: usize = 72;
/// The CRC32C trailer.
const TRAILER_LEN: usize = 4;
/// One chunk's CRC32C.
const CRC_LEN: usize = 4;

/// The upload a blob's chunks were staged under: random, 128 bits (SPEC §9.2, ADR-0034). Its text form, used in
/// remote keys (SPEC §9.4), is 32 lowercase hex digits.
///
/// ```
/// assert_eq!(persia_blob::UploadId::new(0xab).to_string(), "000000000000000000000000000000ab");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UploadId(u128);

impl UploadId {
    /// The upload id `id`.
    pub const fn new(id: u128) -> Self {
        Self(id)
    }

    /// The id's value.
    pub const fn get(self) -> u128 {
        self.0
    }
}

impl fmt::Display for UploadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// A blob's chunk list (see the module docs). Every value satisfies the rules of SPEC §9.1.1, checked by
/// [`BlobManifest::new`] and [`BlobManifest::decode`], so each has exactly one encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobManifest {
    blob_id: BlobId,
    size: u64,
    chunk_size: ChunkSize,
    upload_id: UploadId,
    chunk_crcs: Vec<u32>,
}

impl BlobManifest {
    /// The manifest of the blob `blob_id` of `size` bytes, staged under `upload_id` in chunks of `chunk_size` whose
    /// CRC32Cs are `chunk_crcs`, in chunk order.
    ///
    /// # Errors
    /// - [`Error::InvalidChunking`] when the blob needs more than `u32::MAX` chunks;
    /// - [`Error::InvalidManifest`] when `chunk_crcs` does not hold one CRC per chunk, or `size` is 0 and `blob_id`
    ///   is not the SHA-256 of no bytes.
    pub fn new(
        blob_id: BlobId,
        size: u64,
        chunk_size: ChunkSize,
        upload_id: UploadId,
        chunk_crcs: Vec<u32>,
    ) -> Result<Self> {
        let count = chunk_size.chunk_count(size)?;
        if u64_from_usize(chunk_crcs.len()) != u64::from(count) {
            return Err(Error::InvalidManifest {
                what: "not one CRC per chunk",
            });
        }
        if size == 0 && blob_id != blob_id_of(&[]) {
            return Err(Error::InvalidManifest {
                what: "an empty blob whose id is not the SHA-256 of no bytes",
            });
        }
        Ok(Self {
            blob_id,
            size,
            chunk_size,
            upload_id,
            chunk_crcs,
        })
    }

    /// The blob's id.
    pub const fn blob_id(&self) -> BlobId {
        self.blob_id
    }

    /// The blob's length in bytes.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The chunk size the blob was stored with.
    pub const fn chunk_size(&self) -> ChunkSize {
        self.chunk_size
    }

    /// The upload the chunks were staged under.
    pub const fn upload_id(&self) -> UploadId {
        self.upload_id
    }

    /// One CRC32C per chunk, in chunk order.
    pub fn chunk_crcs(&self) -> &[u32] {
        &self.chunk_crcs
    }

    /// The number of chunks: `ceil(size / chunk_size)`.
    pub fn chunk_count(&self) -> u32 {
        // `new` and `decode` checked that the count fits.
        u32::try_from(self.chunk_crcs.len()).unwrap_or(u32::MAX)
    }

    /// The byte range of chunk `index` in the blob; `None` past the last chunk.
    pub fn chunk_range(&self, index: u32) -> Option<Range<u64>> {
        self.chunk_size.chunk_range(self.size, index)
    }

    /// Checks that this manifest is the one of the blob `blob_id`: what a reader that looked the manifest up by an id
    /// (its key, SPEC §9.4, or a `BlobIndex` entry, §9.3) must check before serving chunks, since a manifest's
    /// checksums cover only its own bytes.
    ///
    /// # Errors
    /// [`Error::Corrupt`] when the manifest names another blob.
    pub fn check_blob_id(&self, blob_id: BlobId) -> Result<()> {
        if self.blob_id == blob_id {
            Ok(())
        } else {
            Err(Error::Corrupt {
                encoding: ENCODING,
                what: "the manifest names another blob than the one looked up",
            })
        }
    }

    /// Checks that this manifest describes the blob `blob_ref` refers to: same id, size and chunk size (SPEC §9.4).
    ///
    /// # Errors
    /// [`Error::Corrupt`] for the first field that differs.
    pub fn check_ref(&self, blob_ref: &BlobRef) -> Result<()> {
        self.check_blob_id(blob_ref.blob_id())?;
        let corrupt = |what| {
            Err(Error::Corrupt {
                encoding: ENCODING,
                what,
            })
        };
        if self.size != blob_ref.size() {
            return corrupt("the manifest's size differs from the blob reference's");
        }
        if self.chunk_size.get() != blob_ref.chunk_size() {
            return corrupt("the manifest's chunk size differs from the blob reference's");
        }
        Ok(())
    }

    /// The length of the version-1, no-flags encoding of the manifest of a blob of `size` bytes in chunks of
    /// `chunk_size`: `76 + 4·n`. A reader fetches the whole object; once it has read the version and flags it can use
    /// this as a sanity bound (a later layout, ROADMAP 9.3.3, may be longer).
    ///
    /// # Errors
    /// - [`Error::InvalidChunking`] when the blob needs more than `u32::MAX` chunks;
    /// - [`Error::InvalidManifest`] when the length does not fit in a `usize` (a 32-bit target).
    pub fn encoded_len_for(size: u64, chunk_size: ChunkSize) -> Result<usize> {
        let count = usize_from_u32(chunk_size.chunk_count(size)?);
        count
            .checked_mul(CRC_LEN)
            .and_then(|crcs| crcs.checked_add(HEAD_LEN + TRAILER_LEN))
            .ok_or(Error::InvalidManifest {
                what: "longer than this platform can address",
            })
    }

    /// The length of [`BlobManifest::encode`]'s output.
    pub fn encoded_len(&self) -> usize {
        // At most 2^32 CRCs: no overflow where a `Vec` of them exists.
        self.chunk_crcs
            .len()
            .saturating_mul(CRC_LEN)
            .saturating_add(HEAD_LEN + TRAILER_LEN)
    }

    /// The encoding of SPEC §9.1.1.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.write_bytes(&MAGIC);
        w.write_u32(VERSION);
        w.write_u32(KNOWN_FLAGS);
        w.write_bytes(self.blob_id.as_bytes());
        w.write_u64(self.size);
        w.write_u32(self.chunk_size.get());
        w.write_u128(self.upload_id.get());
        for &crc in &self.chunk_crcs {
            w.write_u32(crc);
        }
        let crc = crc32c(w.as_slice());
        w.write_u32(crc);
        w.into_inner()
    }

    /// Decodes a blob manifest: the length, the magic, the CRC32C trailer, the version and flags, then the rest.
    /// It allocates no more than its input's length.
    ///
    /// # Errors
    /// - [`Error::Corrupt`] for anything SPEC §9.1.1 does not allow, version 0 included;
    /// - [`Error::UnsupportedVersion`] for an encoding version above [`VERSION`];
    /// - [`Error::UnsupportedFlags`] for a `flags` bit this build does not know (a newer writer's layout).
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = |what| Error::Corrupt {
            encoding: ENCODING,
            what,
        };
        let short = || corrupt("shorter than a header and a trailer");
        if bytes.len() < HEAD_LEN + TRAILER_LEN {
            return Err(short());
        }
        if bytes.get(..MAGIC.len()) != Some(&MAGIC[..]) {
            return Err(corrupt("not a blob manifest (bad magic)"));
        }
        // The trailer ends every version, so a damaged version or flags field reads as corruption.
        let body_end = bytes.len().checked_sub(TRAILER_LEN).ok_or_else(short)?;
        let (covered, trailer) = bytes.split_at(body_end);
        let stored = Reader::new(trailer).read_u32().map_err(|_| short())?;
        if stored != crc32c(covered) {
            return Err(corrupt("checksum mismatch"));
        }
        let mut r = Reader::new(covered.get(MAGIC.len()..).unwrap_or_default());
        let eof = |_| short();
        match r.read_u32().map_err(eof)? {
            0 => return Err(corrupt("encoding version 0")),
            VERSION => {}
            version => {
                return Err(Error::UnsupportedVersion {
                    encoding: ENCODING,
                    version,
                });
            }
        }
        let flags = r.read_u32().map_err(eof)?;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(Error::UnsupportedFlags {
                encoding: ENCODING,
                flags,
            });
        }
        let blob_id = BlobId::from_bytes(r.read_array().map_err(eof)?);
        let size = r.read_u64().map_err(eof)?;
        let chunk_size = ChunkSize::new(r.read_u32().map_err(eof)?)
            .map_err(|_| corrupt("chunk_size outside 1..=64 MiB"))?;
        let upload_id = UploadId::new(r.read_u128().map_err(eof)?);
        let count = chunk_size
            .chunk_count(size)
            .map_err(|_| corrupt("more than u32::MAX chunks"))?;
        let rest = r.rest();
        // Checked before allocating: the CRC list is exactly what is left.
        if u64_from_usize(rest.len()) != u64::from(count).saturating_mul(u64_from_usize(CRC_LEN)) {
            return Err(corrupt("the chunk CRCs do not match the chunk count"));
        }
        // The length check above leaves no remainder.
        let chunk_crcs: Vec<u32> = rest
            .as_chunks::<CRC_LEN>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        if size == 0 && blob_id != blob_id_of(&[]) {
            return Err(corrupt(
                "an empty blob whose id is not the SHA-256 of no bytes",
            ));
        }
        Ok(Self {
            blob_id,
            size,
            chunk_size,
            upload_id,
            chunk_crcs,
        })
    }
}
