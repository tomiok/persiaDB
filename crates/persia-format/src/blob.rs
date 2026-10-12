//! Blob identifiers and blob references (SPEC §9.1; ADR-0002, ADR-0011).
//!
//! These are value types only: computing a [`BlobId`] (SHA-256 of the blob's bytes, ADR-0002) and storing chunks
//! is `persia-blob`'s job. They live here so documents and segments can carry a [`BlobRef`] without depending on
//! the blob store.
//!
//! A [`BlobRef`] is validated when it is built, so [`BlobRef::encode`] cannot fail, and the same rules are checked
//! by [`BlobRef::decode`], so every valid value has exactly one encoding.

use core::fmt;
use core::str::FromStr;
use std::collections::BTreeMap;

use crate::error::{Corruption, EncodeError, Error, Result, Unsupported, to_u64};
use crate::reader::Reader;
use crate::varint;
use crate::writer::Writer;

/// The content address of a blob: the SHA-256 of its full byte content (SPEC §9.1, ADR-0002).
///
/// Its text form ([`fmt::Display`], [`FromStr`]) is 64 lowercase hex characters, as used in storage keys
/// (SPEC §9.4), JSON and logs. Ids order by their bytes.
///
/// ```
/// use persia_format::BlobId;
///
/// // SHA-256 of the empty input.
/// let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// let id: BlobId = hex.parse()?;
/// assert_eq!(id.as_bytes()[0], 0xe3);
/// assert_eq!(id.to_string(), hex);
/// assert!("E3B0".parse::<BlobId>().is_err()); // wrong length, and only lowercase is canonical
/// # Ok::<(), persia_format::blob::ParseBlobIdError>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobId([u8; BlobId::LEN]);

impl BlobId {
    /// Length of an id in bytes.
    pub const LEN: usize = 32;
    /// Length of an id's hex text form.
    pub const HEX_LEN: usize = 64;

    /// The id with these SHA-256 bytes. The caller computed them from the blob's content (ADR-0002).
    pub const fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    /// The SHA-256 bytes.
    pub const fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobId({self})")
    }
}

/// Why a string is not a [`BlobId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseBlobIdError {
    /// The string is not [`BlobId::HEX_LEN`] bytes long.
    #[error("blob id must be {expected} hex characters, got {len} bytes", expected = BlobId::HEX_LEN)]
    Length {
        /// The string's length in bytes.
        len: usize,
    },
    /// The byte at `index` is not a lowercase hex digit.
    #[error("blob id has a byte that is not a lowercase hex digit at index {index}")]
    Digit {
        /// Byte index of the first offending character.
        index: usize,
    },
}

impl FromStr for BlobId {
    type Err = ParseBlobIdError;

    /// Parses the canonical text form: exactly 64 lowercase hex digits.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.as_bytes();
        if s.len() != Self::HEX_LEN {
            return Err(ParseBlobIdError::Length { len: s.len() });
        }
        let mut bytes = [0_u8; Self::LEN];
        let digit = |index: usize| {
            s.get(index)
                .copied()
                .and_then(hex_digit)
                .ok_or(ParseBlobIdError::Digit { index })
        };
        for (i, out) in bytes.iter_mut().enumerate() {
            let hi = i.saturating_mul(2);
            *out = (digit(hi)? << 4) | digit(hi.saturating_add(1))?;
        }
        Ok(Self(bytes))
    }
}

/// The value of a lowercase hex digit.
fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => c.checked_sub(b'0'),
        b'a'..=b'f' => c.checked_sub(b'a').and_then(|d| d.checked_add(10)),
        _ => None,
    }
}

/// A [`BlobRef`] rule that does not hold (SPEC §9.1, ADR-0011).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlobRefDefect {
    /// `chunk_size` is outside `1..=`[`BlobRef::MAX_CHUNK_SIZE`].
    #[error("chunk size {chunk_size} outside 1..={max}", max = BlobRef::MAX_CHUNK_SIZE)]
    ChunkSize {
        /// The chunk size.
        chunk_size: u32,
    },
    /// `ceil(size / chunk_size)` does not fit in a `u32`.
    #[error("{size} bytes in chunks of {chunk_size} need more than u32::MAX chunks")]
    TooManyChunks {
        /// The blob size.
        size: u64,
        /// The chunk size.
        chunk_size: u32,
    },
    /// `mime` is outside `1..=`[`BlobRef::MAX_MIME_LEN`] bytes.
    #[error("mime length {len} outside 1..={max}", max = BlobRef::MAX_MIME_LEN)]
    MimeLen {
        /// The length in bytes.
        len: u64,
    },
    /// `mime` has a byte outside printable ASCII (`0x20..=0x7E`).
    #[error("mime byte {byte:#04x} is not printable ASCII")]
    MimeByte {
        /// The offending byte.
        byte: u8,
    },
    /// `user_meta` has more than [`BlobRef::MAX_META_ENTRIES`] entries.
    #[error("{count} user_meta entries, at most {max} allowed", max = BlobRef::MAX_META_ENTRIES)]
    MetaCount {
        /// The entry count.
        count: u64,
    },
    /// A `user_meta` key is outside `1..=`[`BlobRef::MAX_META_KEY_LEN`] bytes.
    #[error("user_meta key length {len} outside 1..={max}", max = BlobRef::MAX_META_KEY_LEN)]
    MetaKeyLen {
        /// The length in bytes.
        len: u64,
    },
    /// A `user_meta` value is longer than [`BlobRef::MAX_META_VALUE_LEN`] bytes.
    #[error("user_meta value length {len} exceeds {max}", max = BlobRef::MAX_META_VALUE_LEN)]
    MetaValueLen {
        /// The length in bytes.
        len: u64,
    },
    /// Encoded `user_meta` keys are not strictly increasing by bytes (decoding only: a map is always sorted).
    #[error("user_meta keys are not strictly increasing")]
    MetaKeyOrder,
    /// An encoded `user_meta` key or value is not UTF-8 (decoding only).
    #[error("user_meta string is not UTF-8")]
    InvalidUtf8,
    /// An encoded `version` of 0, which no writer produces (decoding only).
    #[error("blob reference version 0")]
    ZeroVersion,
}

/// A document's reference to a blob: `{ blob_id, size, mime, chunk_size, user_meta? }` (SPEC §9.1).
///
/// Every `BlobRef` satisfies the value rules of SPEC §9.1 (checked by [`BlobRef::new`] and [`BlobRef::decode`]).
/// An empty `user_meta` is the same value as an absent one.
///
/// ```
/// use std::collections::BTreeMap;
/// use persia_format::{BlobId, BlobRef, Reader, Writer};
///
/// let id = BlobId::from_bytes([7; 32]);
/// let meta = BTreeMap::from([("filename".to_owned(), "cat.jpg".to_owned())]);
/// let blob = BlobRef::new(id, 10 << 20, "image/jpeg", 4 << 20, meta)?;
/// assert_eq!(blob.chunk_count(), 3);
///
/// let mut w = Writer::new();
/// blob.encode(&mut w);
/// assert_eq!(w.len(), blob.encoded_len());
/// assert_eq!(BlobRef::decode(&mut Reader::new(w.as_slice()))?, blob);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobRef {
    blob_id: BlobId,
    size: u64,
    mime: String,
    chunk_size: u32,
    user_meta: BTreeMap<String, String>,
}

impl BlobRef {
    /// The encoding version this build writes and reads.
    pub const VERSION: u8 = 1;
    /// Largest valid `chunk_size`: 64 MiB.
    pub const MAX_CHUNK_SIZE: u32 = 64 << 20;
    /// Largest `mime` length in bytes.
    pub const MAX_MIME_LEN: usize = 255;
    /// Most `user_meta` entries.
    pub const MAX_META_ENTRIES: usize = 32;
    /// Largest `user_meta` key length in bytes.
    pub const MAX_META_KEY_LEN: usize = 128;
    /// Largest `user_meta` value length in bytes.
    pub const MAX_META_VALUE_LEN: usize = 1024;
    /// Largest encoding of any valid `BlobRef`, in bytes.
    pub const MAX_ENCODED_LEN: usize = 37_295;

    /// A reference to the blob `blob_id` of `size` bytes stored in chunks of `chunk_size`.
    ///
    /// # Errors
    /// [`EncodeError::InvalidBlobRef`] if a value rule of SPEC §9.1 does not hold.
    pub fn new(
        blob_id: BlobId,
        size: u64,
        mime: impl Into<String>,
        chunk_size: u32,
        user_meta: BTreeMap<String, String>,
    ) -> Result<Self, EncodeError> {
        let mime = mime.into();
        let invalid = |defect| EncodeError::InvalidBlobRef { defect };
        check_chunking(size, chunk_size).map_err(invalid)?;
        check_mime_len(mime.len()).map_err(invalid)?;
        check_mime_bytes(mime.as_bytes()).map_err(|(_, d)| invalid(d))?;
        check_meta_count(to_u64(user_meta.len())).map_err(invalid)?;
        for (key, value) in &user_meta {
            check_key_len(to_u64(key.len())).map_err(invalid)?;
            check_value_len(to_u64(value.len())).map_err(invalid)?;
        }
        Ok(Self {
            blob_id,
            size,
            mime,
            chunk_size,
            user_meta,
        })
    }

    /// The blob's content address.
    pub const fn blob_id(&self) -> BlobId {
        self.blob_id
    }

    /// The blob's length in bytes.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The blob's media type, as given.
    pub fn mime(&self) -> &str {
        &self.mime
    }

    /// The length of every chunk but the last.
    pub const fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// Client annotations; empty when absent.
    pub const fn user_meta(&self) -> &BTreeMap<String, String> {
        &self.user_meta
    }

    /// `ceil(size / chunk_size)`: 0 for an empty blob.
    pub fn chunk_count(&self) -> u32 {
        // `new` and `decode` checked that the count fits.
        u32::try_from(chunk_count(self.size, self.chunk_size)).unwrap_or(u32::MAX)
    }

    /// Bytes [`BlobRef::encode`] appends, at most [`BlobRef::MAX_ENCODED_LEN`].
    pub fn encoded_len(&self) -> usize {
        // Every term is bounded by the value rules, so no sum can saturate.
        let meta = self.user_meta.iter().fold(0_usize, |acc, (k, v)| {
            acc.saturating_add(varint::encoded_len(to_u64(k.len())))
                .saturating_add(k.len())
                .saturating_add(varint::encoded_len(to_u64(v.len())))
                .saturating_add(v.len())
        });
        (1 + BlobId::LEN + 1)
            .saturating_add(varint::encoded_len(self.size))
            .saturating_add(varint::encoded_len(u64::from(self.chunk_size)))
            .saturating_add(self.mime.len())
            .saturating_add(varint::encoded_len(to_u64(self.user_meta.len())))
            .saturating_add(meta)
    }

    /// Appends the encoding of SPEC §9.1.
    pub fn encode(&self, w: &mut Writer) {
        w.write_u8(Self::VERSION);
        w.write_bytes(self.blob_id.as_bytes());
        w.write_varint_u64(self.size);
        w.write_varint_u32(self.chunk_size);
        // The value rules bound every length below, so the conversions cannot fail.
        w.write_u8(u8::try_from(self.mime.len()).unwrap_or(u8::MAX));
        w.write_bytes(self.mime.as_bytes());
        w.write_varint_u32(len_u32(self.user_meta.len()));
        for (key, value) in &self.user_meta {
            w.write_varint_u32(len_u32(key.len()));
            w.write_bytes(key.as_bytes());
            w.write_varint_u32(len_u32(value.len()));
            w.write_bytes(value.as_bytes());
        }
    }

    /// Decodes one reference and advances past it; on error the reader does not move.
    ///
    /// # Errors
    /// - [`Error::Unsupported`] with [`Unsupported::BlobRefVersion`] for a `version` above [`BlobRef::VERSION`];
    /// - [`Error::Corrupt`] for `version` 0, input that ends early, a non-canonical varint, or a value rule that
    ///   does not hold ([`Corruption::InvalidBlobRef`] at the offending field or byte).
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let mut c = r.clone();
        let blob_ref = Self::decode_fields(&mut c)?;
        *r = c;
        Ok(blob_ref)
    }

    fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
        let at = r.offset();
        let version = r.read_u8()?;
        if version == 0 {
            // No writer ever produces 0: a zeroed region, not a newer format.
            return Err(corrupt(at, BlobRefDefect::ZeroVersion));
        }
        if version != Self::VERSION {
            return Err(Error::Unsupported {
                offset: at,
                reason: Unsupported::BlobRefVersion { version },
            });
        }
        let blob_id = BlobId(r.read_array()?);
        let size = r.read_varint_u64()?;
        let at = r.offset();
        let chunk_size = r.read_varint_u32()?;
        check_chunking(size, chunk_size).map_err(|d| corrupt(at, d))?;

        let at = r.offset();
        let mime_len = r.read_u8()?;
        check_mime_len(usize::from(mime_len)).map_err(|d| corrupt(at, d))?;
        let at = r.offset();
        let mime = r.read_bytes(usize::from(mime_len))?;
        check_mime_bytes(mime).map_err(|(i, d)| corrupt(at.saturating_add(to_u64(i)), d))?;
        // Printable ASCII: every byte is its own char.
        let mime: String = mime.iter().copied().map(char::from).collect();

        let at = r.offset();
        let count = u64::from(r.read_varint_u32()?);
        check_meta_count(count).map_err(|d| corrupt(at, d))?;
        let mut user_meta = BTreeMap::new();
        let mut prev: Option<&str> = None;
        for _ in 0..count {
            let at = r.offset();
            let key = read_string(r, check_key_len)?;
            if prev.is_some_and(|p| p >= key) {
                return Err(corrupt(at, BlobRefDefect::MetaKeyOrder));
            }
            prev = Some(key);
            let value = read_string(r, check_value_len)?;
            user_meta.insert(key.to_owned(), value.to_owned());
        }
        Ok(Self {
            blob_id,
            size,
            mime,
            chunk_size,
            user_meta,
        })
    }
}

/// Reads a varint length checked by `check_len`, then that many UTF-8 bytes.
fn read_string<'a>(
    r: &mut Reader<'a>,
    check_len: fn(u64) -> Result<(), BlobRefDefect>,
) -> Result<&'a str> {
    let at = r.offset();
    let len = u64::from(r.read_varint_u32()?);
    check_len(len).map_err(|d| corrupt(at, d))?;
    // The checked length is at most a few KiB; were it not, `read_bytes` would fail.
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    let at = r.offset();
    let bytes = r.read_bytes(len)?;
    core::str::from_utf8(bytes).map_err(|e| {
        corrupt(
            at.saturating_add(to_u64(e.valid_up_to())),
            BlobRefDefect::InvalidUtf8,
        )
    })
}

/// A length the value rules already bounded, as written in a `varint u32`.
fn len_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

fn corrupt(offset: u64, defect: BlobRefDefect) -> Error {
    Error::Corrupt {
        offset,
        reason: Corruption::InvalidBlobRef { defect },
    }
}

/// `ceil(size / chunk_size)`, or `u64::MAX` for a zero `chunk_size`.
fn chunk_count(size: u64, chunk_size: u32) -> u64 {
    match core::num::NonZeroU64::new(u64::from(chunk_size)) {
        Some(d) => size.div_ceil(d.get()),
        None => u64::MAX,
    }
}

fn check_chunking(size: u64, chunk_size: u32) -> Result<(), BlobRefDefect> {
    if chunk_size == 0 || chunk_size > BlobRef::MAX_CHUNK_SIZE {
        return Err(BlobRefDefect::ChunkSize { chunk_size });
    }
    if chunk_count(size, chunk_size) > u64::from(u32::MAX) {
        return Err(BlobRefDefect::TooManyChunks { size, chunk_size });
    }
    Ok(())
}

fn check_mime_len(len: usize) -> Result<(), BlobRefDefect> {
    if (1..=BlobRef::MAX_MIME_LEN).contains(&len) {
        Ok(())
    } else {
        Err(BlobRefDefect::MimeLen { len: to_u64(len) })
    }
}

/// Fails with the index of the first byte outside printable ASCII.
fn check_mime_bytes(mime: &[u8]) -> Result<(), (usize, BlobRefDefect)> {
    match mime
        .iter()
        .enumerate()
        .find(|&(_, b)| !(0x20..=0x7e).contains(b))
    {
        Some((i, &byte)) => Err((i, BlobRefDefect::MimeByte { byte })),
        None => Ok(()),
    }
}

fn check_meta_count(count: u64) -> Result<(), BlobRefDefect> {
    if count <= to_u64(BlobRef::MAX_META_ENTRIES) {
        Ok(())
    } else {
        Err(BlobRefDefect::MetaCount { count })
    }
}

fn check_key_len(len: u64) -> Result<(), BlobRefDefect> {
    if (1..=to_u64(BlobRef::MAX_META_KEY_LEN)).contains(&len) {
        Ok(())
    } else {
        Err(BlobRefDefect::MetaKeyLen { len })
    }
}

fn check_value_len(len: u64) -> Result<(), BlobRefDefect> {
    if len <= to_u64(BlobRef::MAX_META_VALUE_LEN) {
        Ok(())
    } else {
        Err(BlobRefDefect::MetaValueLen { len })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::string_slice)]

    use super::*;

    fn id() -> BlobId {
        BlobId::from_bytes(core::array::from_fn(|i| u8::try_from(i).unwrap()))
    }

    fn meta(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    fn encode(b: &BlobRef) -> Vec<u8> {
        let mut w = Writer::new();
        b.encode(&mut w);
        assert_eq!(w.len(), b.encoded_len());
        w.into_inner()
    }

    fn invalid(defect: BlobRefDefect) -> EncodeError {
        EncodeError::InvalidBlobRef { defect }
    }

    /// The decode error of `bytes` read at absolute offset 100.
    fn decode_err(bytes: &[u8]) -> Error {
        let mut r = Reader::with_base_offset(bytes, 100);
        let err = BlobRef::decode(&mut r).unwrap_err();
        assert_eq!(r.position(), 0, "a failed decode moves nothing");
        err
    }

    fn corrupt_at(offset: u64, defect: BlobRefDefect) -> Error {
        corrupt(offset, defect)
    }

    #[test]
    fn golden_encoding() {
        let b = BlobRef::new(id(), 300, "a/b", 4 << 20, meta(&[("k", "v")])).unwrap();
        let mut expected = vec![1];
        expected.extend(0..32_u8);
        expected.extend([0xac, 0x02]); // size 300
        expected.extend([0x80, 0x80, 0x80, 0x02]); // chunk_size 4 MiB
        expected.extend([3, b'a', b'/', b'b']);
        expected.extend([1, 1, b'k', 1, b'v']);
        assert_eq!(encode(&b), expected);
        assert_eq!(BlobRef::decode(&mut Reader::new(&expected)).unwrap(), b);
    }

    #[test]
    fn golden_minimal_encoding() {
        let b = BlobRef::new(BlobId::from_bytes([0xff; 32]), 0, "x", 1, BTreeMap::new()).unwrap();
        let mut expected = vec![1];
        expected.extend([0xff; 32]);
        expected.extend([0, 1, 1, b'x', 0]);
        assert_eq!(encode(&b), expected);
        assert_eq!(b.chunk_count(), 0);
    }

    #[test]
    fn decode_consumes_exactly_one_reference() {
        let b = BlobRef::new(id(), 5, "text/plain", 2, BTreeMap::new()).unwrap();
        let mut bytes = encode(&b);
        let len = bytes.len();
        bytes.extend([0xde, 0xad]);
        let mut r = Reader::new(&bytes);
        assert_eq!(BlobRef::decode(&mut r).unwrap(), b);
        assert_eq!(r.position(), len);
        assert_eq!(b.chunk_count(), 3);
    }

    #[test]
    fn largest_value_has_max_encoded_len() {
        let key = |i: usize| format!("{i:0>128}");
        let user_meta = (0..BlobRef::MAX_META_ENTRIES)
            .map(|i| (key(i), "v".repeat(BlobRef::MAX_META_VALUE_LEN)))
            .collect();
        let chunk_size = BlobRef::MAX_CHUNK_SIZE;
        let size = u64::from(chunk_size) * u64::from(u32::MAX);
        let mime = "m".repeat(BlobRef::MAX_MIME_LEN);
        let b = BlobRef::new(id(), size, mime, chunk_size, user_meta).unwrap();
        assert_eq!(b.chunk_count(), u32::MAX);
        let bytes = encode(&b);
        assert_eq!(bytes.len(), BlobRef::MAX_ENCODED_LEN);
        assert_eq!(BlobRef::decode(&mut Reader::new(&bytes)).unwrap(), b);
    }

    #[test]
    fn new_enforces_chunking_rules() {
        let new = |size, chunk_size| BlobRef::new(id(), size, "a/b", chunk_size, BTreeMap::new());
        assert_eq!(
            new(0, 0),
            Err(invalid(BlobRefDefect::ChunkSize { chunk_size: 0 }))
        );
        let chunk_size = BlobRef::MAX_CHUNK_SIZE + 1;
        assert_eq!(
            new(0, chunk_size),
            Err(invalid(BlobRefDefect::ChunkSize { chunk_size }))
        );
        assert_eq!(new(u64::from(u32::MAX), 1).unwrap().chunk_count(), u32::MAX);
        let size = u64::from(u32::MAX) + 1;
        assert_eq!(
            new(size, 1),
            Err(invalid(BlobRefDefect::TooManyChunks {
                size,
                chunk_size: 1
            }))
        );
        assert_eq!(
            new(u64::MAX, BlobRef::MAX_CHUNK_SIZE).unwrap_err(),
            invalid(BlobRefDefect::TooManyChunks {
                size: u64::MAX,
                chunk_size: BlobRef::MAX_CHUNK_SIZE
            })
        );
    }

    #[test]
    fn new_enforces_mime_rules() {
        let new = |mime: String| BlobRef::new(id(), 0, mime, 1, BTreeMap::new());
        assert_eq!(
            new(String::new()),
            Err(invalid(BlobRefDefect::MimeLen { len: 0 }))
        );
        assert!(new("m".repeat(255)).is_ok());
        assert_eq!(
            new("m".repeat(256)),
            Err(invalid(BlobRefDefect::MimeLen { len: 256 }))
        );
        assert!(new("text/plain; charset=utf-8 ~".to_owned()).is_ok());
        for byte in [0x00, 0x1f, 0x7f] {
            assert_eq!(
                new(format!("a/{}", char::from(byte))),
                Err(invalid(BlobRefDefect::MimeByte { byte }))
            );
        }
        assert_eq!(
            new("a/é".to_owned()),
            Err(invalid(BlobRefDefect::MimeByte { byte: 0xc3 }))
        );
    }

    #[test]
    fn new_enforces_user_meta_rules() {
        let new = |m| BlobRef::new(id(), 0, "a/b", 1, m);
        let many = |n: usize| (0..n).map(|i| (format!("{i:02}"), String::new())).collect();
        assert!(new(many(32)).is_ok());
        assert_eq!(
            new(many(33)),
            Err(invalid(BlobRefDefect::MetaCount { count: 33 }))
        );
        assert_eq!(
            new(meta(&[("", "v")])),
            Err(invalid(BlobRefDefect::MetaKeyLen { len: 0 }))
        );
        assert!(new(meta(&[(&"k".repeat(128), &"v".repeat(1024))])).is_ok());
        assert_eq!(
            new(meta(&[(&"k".repeat(129), "")])),
            Err(invalid(BlobRefDefect::MetaKeyLen { len: 129 }))
        );
        assert_eq!(
            new(meta(&[("k", &"v".repeat(1025))])),
            Err(invalid(BlobRefDefect::MetaValueLen { len: 1025 }))
        );
    }

    /// `1 | id | size=0 | chunk_size=1 | mime "a"` followed by `tail`.
    fn header_then(tail: &[u8]) -> Vec<u8> {
        let mut bytes = vec![1];
        bytes.extend([0; 32]);
        bytes.extend([0, 1, 1, b'a']);
        bytes.extend(tail);
        bytes
    }

    #[test]
    fn newer_version_is_unsupported_and_zero_is_corrupt() {
        let mut bytes = header_then(&[0]);
        bytes[0] = 0;
        assert_eq!(
            decode_err(&bytes),
            corrupt_at(100, BlobRefDefect::ZeroVersion)
        );
        for version in [2, 0xff] {
            let mut bytes = header_then(&[0]);
            bytes[0] = version;
            assert_eq!(
                decode_err(&bytes),
                Error::Unsupported {
                    offset: 100,
                    reason: Unsupported::BlobRefVersion { version }
                }
            );
        }
    }

    #[test]
    fn decode_rejects_bad_chunking_at_chunk_size() {
        let mut bytes = vec![1];
        bytes.extend([0; 32]);
        bytes.extend([0, 0, 1, b'a', 0]);
        assert_eq!(
            decode_err(&bytes),
            corrupt_at(134, BlobRefDefect::ChunkSize { chunk_size: 0 })
        );
        // size 2^32 in chunks of 1.
        let mut bytes = vec![1];
        bytes.extend([0; 32]);
        bytes.extend([0x80, 0x80, 0x80, 0x80, 0x10, 1, 1, b'a', 0]);
        assert_eq!(
            decode_err(&bytes),
            corrupt_at(
                138,
                BlobRefDefect::TooManyChunks {
                    size: 1 << 32,
                    chunk_size: 1
                }
            )
        );
    }

    #[test]
    fn decode_rejects_bad_mime_at_the_field() {
        let mut bytes = vec![1];
        bytes.extend([0; 32]);
        bytes.extend([0, 1, 0, 0]);
        assert_eq!(
            decode_err(&bytes),
            corrupt_at(135, BlobRefDefect::MimeLen { len: 0 })
        );
        let mut bytes = vec![1];
        bytes.extend([0; 32]);
        bytes.extend([0, 1, 3, b'a', 0x80, b'b', 0]);
        assert_eq!(
            decode_err(&bytes),
            corrupt_at(137, BlobRefDefect::MimeByte { byte: 0x80 })
        );
    }

    #[test]
    fn decode_rejects_bad_user_meta_at_the_field() {
        // Offsets: the header is 37 bytes, so the count is at 137.
        assert_eq!(
            decode_err(&header_then(&[33])),
            corrupt_at(137, BlobRefDefect::MetaCount { count: 33 })
        );
        assert_eq!(
            decode_err(&header_then(&[1, 0, 0])),
            corrupt_at(138, BlobRefDefect::MetaKeyLen { len: 0 })
        );
        assert_eq!(
            decode_err(&header_then(&[1, 0x81, 0x01])),
            corrupt_at(138, BlobRefDefect::MetaKeyLen { len: 129 })
        );
        assert_eq!(
            decode_err(&header_then(&[1, 1, b'k', 0x81, 0x08])),
            corrupt_at(140, BlobRefDefect::MetaValueLen { len: 1025 })
        );
        assert_eq!(
            decode_err(&header_then(&[1, 1, 0xff, 0])),
            corrupt_at(139, BlobRefDefect::InvalidUtf8)
        );
        assert_eq!(
            decode_err(&header_then(&[1, 1, b'k', 1, 0xc3])),
            corrupt_at(141, BlobRefDefect::InvalidUtf8)
        );
        // The offset points at the first invalid byte, not at the start of the string.
        assert_eq!(
            decode_err(&header_then(&[1, 3, b'a', b'b', 0xff, 0])),
            corrupt_at(141, BlobRefDefect::InvalidUtf8)
        );
        // Counts and lengths are varint u32.
        assert_eq!(
            decode_err(&header_then(&[0x80, 0x80, 0x80, 0x80, 0x10])),
            Error::Corrupt {
                offset: 137,
                reason: Corruption::VarintOverflow { bits: 32 }
            }
        );
        // Equal and decreasing keys.
        assert_eq!(
            decode_err(&header_then(&[2, 1, b'k', 0, 1, b'k', 0])),
            corrupt_at(141, BlobRefDefect::MetaKeyOrder)
        );
        assert_eq!(
            decode_err(&header_then(&[2, 2, b'k', b'k', 0, 1, b'k', 0])),
            corrupt_at(142, BlobRefDefect::MetaKeyOrder)
        );
        // A key that is a prefix of the next one sorts first.
        let ok = header_then(&[2, 1, b'k', 0, 2, b'k', b'k', 0]);
        let b = BlobRef::decode(&mut Reader::new(&ok)).unwrap();
        assert_eq!(b.user_meta(), &meta(&[("k", ""), ("kk", "")]));
    }

    #[test]
    fn decode_rejects_overlong_varints() {
        let mut bytes = vec![1];
        bytes.extend([0; 32]);
        bytes.extend([0x80, 0x00, 1, 1, b'a', 0]);
        assert_eq!(
            decode_err(&bytes),
            Error::Corrupt {
                offset: 133,
                reason: Corruption::OverlongVarint
            }
        );
    }

    #[test]
    fn every_truncation_is_unexpected_eof() {
        let b = BlobRef::new(id(), 300, "a/b", 4 << 20, meta(&[("k", "v"), ("x", "")])).unwrap();
        let bytes = encode(&b);
        for len in 0..bytes.len() {
            let err = decode_err(&bytes[..len]);
            assert!(
                matches!(
                    err.corruption(),
                    Some((_, Corruption::UnexpectedEof { .. }))
                ),
                "prefix of {len} bytes: {err:?}"
            );
        }
    }

    #[test]
    fn blob_id_text_form() {
        let hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        assert_eq!(id().to_string(), hex);
        assert_eq!(format!("{:?}", id()), format!("BlobId({hex})"));
        assert_eq!(hex.parse::<BlobId>(), Ok(id()));
        assert_eq!(
            hex[1..].parse::<BlobId>(),
            Err(ParseBlobIdError::Length { len: 63 })
        );
        assert_eq!(
            format!("{hex}0").parse::<BlobId>(),
            Err(ParseBlobIdError::Length { len: 65 })
        );
        for (index, bad) in [(0, 'A'), (63, 'F'), (10, 'g'), (31, ' ')] {
            let mut s = hex.to_owned();
            s.replace_range(index..=index, &bad.to_string());
            assert_eq!(s.parse::<BlobId>(), Err(ParseBlobIdError::Digit { index }));
        }
        // A multi-byte char keeps the byte length at 64 only if it replaces two ASCII bytes.
        let s = format!("é{}", &hex[2..]);
        assert_eq!(
            s.parse::<BlobId>(),
            Err(ParseBlobIdError::Digit { index: 0 })
        );
        assert!(BlobId::from_bytes([0; 32]) < BlobId::from_bytes([1; 32]));
    }
}
