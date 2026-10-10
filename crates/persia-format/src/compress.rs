//! Block compression codecs (SPEC §4.1, §5.2; ADR-0005).
//!
//! Every block declares its [`Codec`] in a header byte (SPEC §5.2). Decompression is bounded twice: the caller
//! states the uncompressed length it expects (from the unit's header) and the largest length it is willing to
//! allocate; anything larger, smaller, or otherwise malformed is [`Error::Corrupt`], never a panic or an
//! unbounded allocation (SPEC §17, decompression bombs).
//!
//! zstd contexts are cached per thread, so compressing or decoding many small blocks does not allocate a
//! context each time.

use std::cell::RefCell;
use std::io::Cursor;
use std::thread::LocalKey;

use zstd::zstd_safe::{self, CCtx, DCtx};

use crate::error::{Corruption, EncodeError, Error, Result, to_u64};

/// How a unit's payload is stored (SPEC §5.2). The discriminant is the on-disk `codec` byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Codec {
    /// Stored as is.
    None = 0,
    /// One zstd frame that declares its content size.
    Zstd = 1,
}

impl Codec {
    /// The on-disk byte.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Parses the on-disk byte read at absolute position `offset`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] with [`Corruption::UnknownCodec`] for an unassigned value.
    ///
    /// ```
    /// use persia_format::compress::Codec;
    ///
    /// assert_eq!(Codec::from_u8(1, 0), Ok(Codec::Zstd));
    /// assert!(Codec::from_u8(7, 0).is_err());
    /// ```
    pub const fn from_u8(byte: u8, offset: u64) -> Result<Self> {
        match byte {
            0 => Ok(Self::None),
            1 => Ok(Self::Zstd),
            codec => Err(Error::Corrupt {
                offset,
                reason: Corruption::UnknownCodec { codec },
            }),
        }
    }
}

/// zstd level used when the caller has no preference (zstd's own default).
pub const DEFAULT_ZSTD_LEVEL: i32 = zstd_safe::CLEVEL_DEFAULT;

/// The zstd frame magic number, little-endian (RFC 8878 §3.1.1).
const ZSTD_MAGIC: [u8; 4] = 0xfd2f_b528_u32.to_le_bytes();

thread_local! {
    static CCTX: RefCell<Option<CCtx<'static>>> = const { RefCell::new(None) };
    static DCTX: RefCell<Option<DCtx<'static>>> = const { RefCell::new(None) };
}

/// Appends `src` encoded with `codec` to `out` and returns the number of bytes appended.
///
/// `level` is the zstd compression level (ignored for [`Codec::None`]); [`DEFAULT_ZSTD_LEVEL`] is a good
/// choice. Whether a block is worth compressing at all is the caller's decision.
///
/// # Errors
/// - [`EncodeError::InvalidCompressionLevel`] when `level` is outside what zstd accepts.
/// - [`EncodeError::CompressionFailed`] when zstd fails internally (out of memory).
///
/// On error `out` is left as it was.
///
/// ```
/// use persia_format::compress::{Codec, DEFAULT_ZSTD_LEVEL, compress_block, decompress_block};
///
/// # if cfg!(miri) { return Ok(()); } // zstd is C behind FFI, which Miri cannot run.
/// let text = b"persia persia persia persia persia persia persia persia".repeat(10);
/// let mut packed = Vec::new();
/// compress_block(Codec::Zstd, DEFAULT_ZSTD_LEVEL, &text, &mut packed)?;
/// assert!(packed.len() < text.len());
///
/// let mut unpacked = Vec::new();
/// decompress_block(Codec::Zstd, &packed, text.len() as u64, 16 * 1024, 0, &mut unpacked)?;
/// assert_eq!(unpacked, text);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
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

/// Rejects a zstd level outside what zstd accepts.
pub(crate) fn check_zstd_level(level: i32) -> Result<(), EncodeError> {
    let (min, max) = (zstd_safe::min_c_level(), zstd_safe::max_c_level());
    if (min..=max).contains(&level) {
        Ok(())
    } else {
        Err(EncodeError::InvalidCompressionLevel { level, min, max })
    }
}

fn compress_zstd(level: i32, src: &[u8], out: &mut Vec<u8>) -> Result<usize, EncodeError> {
    check_zstd_level(level)?;
    out.try_reserve(zstd_safe::compress_bound(src.len()))
        .map_err(|_| EncodeError::CompressionFailed)?;
    // zstd writes into `out`'s spare capacity after `start` and sets its length only on success, so `out` is
    // untouched on error and no bytes are zero-filled first.
    let start = to_u64(out.len());
    let mut dst = Cursor::new(&mut *out);
    dst.set_position(start);
    with_context(&CCTX, CCtx::try_create, |cctx| {
        cctx.compress(&mut dst, src, level)
    })
    .ok_or(EncodeError::CompressionFailed)?
    .map_err(|_| EncodeError::CompressionFailed)
}

/// Decodes a `codec` payload `src` that must expand to exactly `uncompressed_len` bytes, and appends them to
/// `out`.
///
/// `uncompressed_len` comes from the unit's header, so it is untrusted (a `u64`, as stored): it is checked
/// against `max_len`, the most the caller will allocate (e.g. the block size; a `usize`, as it bounds memory),
/// *before* any work is done. `offset` is reported in every error: the absolute position of `src`, or of the
/// unit's header if the caller prefers errors to point at its `uncompressed_len` field.
///
/// # Errors
/// [`Error::Corrupt`] at `offset`, with:
/// - [`Corruption::UncompressedTooLarge`] when `uncompressed_len > max_len`, or when a zstd payload that passes
///   every other check needs an output buffer that cannot be allocated;
/// - [`Corruption::UncompressedLenMismatch`] when the payload expands (or, for zstd, declares it expands) to a
///   different length;
/// - [`Corruption::InvalidCompressedData`] when a zstd payload is not exactly one well-formed zstd frame with
///   a declared content size, or zstd cannot decode it (including a frame that needs a dictionary).
///
/// On error `out` is left as it was.
pub fn decompress_block(
    codec: Codec,
    src: &[u8],
    uncompressed_len: u64,
    max_len: usize,
    offset: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    let corrupt = |reason| Error::Corrupt { offset, reason };
    let len = usize::try_from(uncompressed_len)
        .ok()
        .filter(|&len| len <= max_len)
        .ok_or(corrupt(Corruption::UncompressedTooLarge {
            len: uncompressed_len,
            max: to_u64(max_len),
        }))?;
    match codec {
        Codec::None => {
            if src.len() != len {
                return Err(corrupt(Corruption::UncompressedLenMismatch {
                    expected: uncompressed_len,
                    actual: to_u64(src.len()),
                }));
            }
            out.extend_from_slice(src);
            Ok(())
        }
        Codec::Zstd => decompress_zstd(src, len, max_len, out).map_err(corrupt),
    }
}

/// Checks the frame structure first, so a hostile payload is rejected before `len` bytes are allocated for it.
fn decompress_zstd(
    src: &[u8],
    len: usize,
    max_len: usize,
    out: &mut Vec<u8>,
) -> Result<(), Corruption> {
    // Exactly one frame: the magic rules out skippable and legacy frames, the size rules out trailing bytes.
    if !src.starts_with(&ZSTD_MAGIC) || zstd_safe::find_frame_compressed_size(src) != Ok(src.len())
    {
        return Err(Corruption::InvalidCompressedData);
    }
    match zstd_safe::get_frame_content_size(src) {
        Ok(Some(declared)) if declared == to_u64(len) => {}
        Ok(Some(declared)) => {
            return Err(Corruption::UncompressedLenMismatch {
                expected: to_u64(len),
                actual: declared,
            });
        }
        Ok(None) | Err(_) => return Err(Corruption::InvalidCompressedData),
    }
    // A length within the caller's limit can still be unallocatable (above `isize::MAX`, or out of memory):
    // that is an error too, never a capacity-overflow panic or an abort.
    out.try_reserve_exact(len)
        .map_err(|_| Corruption::UncompressedTooLarge {
            len: to_u64(len),
            max: to_u64(max_len),
        })?;
    // The output slice is exactly `len` long, a hard bound that does not rely on zstd enforcing the declared
    // size. Zero-filling it first costs nothing measurable (`benches/compress.rs`).
    let start = out.len();
    let end = start
        .checked_add(len)
        .ok_or(Corruption::InvalidCompressedData)?;
    out.resize(end, 0);
    let written = out
        .get_mut(start..)
        .and_then(|dst| with_context(&DCTX, DCtx::try_create, |dctx| dctx.decompress(dst, src)));
    match written {
        Some(Ok(n)) if n == len => Ok(()),
        // `None`: zstd could not allocate its context. Not corruption, but this crate's errors have no resource
        // class yet, and Rust aborts on out-of-memory elsewhere anyway. A short `Ok(n)` is unreachable while
        // zstd enforces the declared size; it is still never returned as success.
        Some(Ok(n)) => {
            out.truncate(start);
            Err(Corruption::UncompressedLenMismatch {
                expected: to_u64(len),
                actual: to_u64(n),
            })
        }
        Some(Err(_)) | None => {
            out.truncate(start);
            Err(Corruption::InvalidCompressedData)
        }
    }
}

/// Runs `f` with this thread's cached context, creating it on first use. During thread teardown (the cache is
/// gone or in use) a temporary context is used instead, so this never panics. `None` if `create` fails.
fn with_context<C, R>(
    cache: &'static LocalKey<RefCell<Option<C>>>,
    create: fn() -> Option<C>,
    f: impl FnOnce(&mut C) -> R,
) -> Option<R> {
    let mut f = Some(f);
    let cached = cache.try_with(|cell| {
        let mut slot = cell.try_borrow_mut().ok()?;
        let ctx = match &mut *slot {
            Some(ctx) => ctx,
            empty => empty.insert(create()?),
        };
        f.take().map(|f| f(ctx))
    });
    match (cached, f) {
        (Ok(Some(result)), _) => Some(result),
        (_, Some(f)) => create().map(|mut ctx| f(&mut ctx)),
        // Unreachable: `f` is only taken by a run that returns `Some`.
        (_, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden frame: zstd 1.5.7, level 3, of `b"persia "` × 8 (56 bytes). Decoding it must work forever, whatever
    // zstd version later encodes the same input to.
    // Single-segment frame, 1-byte content size (0x38 = 56), one RLE-free compressed block.
    const GOLDEN_FRAME: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x38, 0x6d, 0x00, 0x00, 0x38, 0x70, 0x65, 0x72, 0x73, 0x69,
        0x61, 0x20, 0x01, 0x00, 0x2a, 0x2a, 0x46,
    ];

    fn zstd(src: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        compress_block(Codec::Zstd, DEFAULT_ZSTD_LEVEL, src, &mut out).unwrap();
        out
    }

    fn corrupt(reason: Corruption) -> Result<()> {
        Err(Error::Corrupt { offset: 40, reason })
    }

    #[test]
    fn codec_bytes_are_stable() {
        for (codec, byte) in [(Codec::None, 0), (Codec::Zstd, 1)] {
            assert_eq!(codec.to_u8(), byte);
            assert_eq!(Codec::from_u8(byte, 0), Ok(codec));
        }
        for byte in 2..=u8::MAX {
            assert_eq!(
                Codec::from_u8(byte, 9),
                Err(Error::Corrupt {
                    offset: 9,
                    reason: Corruption::UnknownCodec { codec: byte }
                })
            );
        }
    }

    #[test]
    fn none_round_trips_and_checks_len() {
        let mut out = vec![7];
        assert_eq!(compress_block(Codec::None, 0, b"abc", &mut out), Ok(3));
        assert_eq!(out, b"\x07abc");
        let mut dec = vec![1];
        decompress_block(Codec::None, b"abc", 3, 3, 40, &mut dec).unwrap();
        assert_eq!(dec, b"\x01abc");
        assert_eq!(
            decompress_block(Codec::None, b"abc", 2, 16, 40, &mut dec),
            corrupt(Corruption::UncompressedLenMismatch {
                expected: 2,
                actual: 3
            })
        );
        assert_eq!(dec, b"\x01abc");
    }

    #[test]
    fn bomb_guard_runs_before_decoding() {
        // Garbage payloads: the length check must fire first, for both codecs.
        for codec in [Codec::None, Codec::Zstd] {
            let mut out = Vec::new();
            assert_eq!(
                decompress_block(codec, b"junk", 17, 16, 40, &mut out),
                corrupt(Corruption::UncompressedTooLarge { len: 17, max: 16 })
            );
            assert_eq!(
                decompress_block(codec, b"junk", u64::MAX, 1 << 20, 40, &mut out),
                corrupt(Corruption::UncompressedTooLarge {
                    len: u64::MAX,
                    max: 1 << 20
                })
            );
            assert_eq!(out, b"");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI, which Miri cannot run")]
    fn zstd_round_trips_and_appends() {
        let text = b"persia ".repeat(8);
        let packed = zstd(&text);
        assert_eq!(packed.get(..4), Some(&ZSTD_MAGIC[..]));
        let mut out = vec![9];
        decompress_block(Codec::Zstd, &packed, 56, 56, 40, &mut out).unwrap();
        assert_eq!(out.get(1..), Some(&text[..]));

        let empty = zstd(b"");
        let mut out = Vec::new();
        decompress_block(Codec::Zstd, &empty, 0, 0, 40, &mut out).unwrap();
        assert_eq!(out, b"");
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI, which Miri cannot run")]
    fn lying_frame_and_zero_limit_are_rejected() {
        let text = b"persia ".repeat(8);
        // The frame's 1-byte content size (offset 5) patched to agree with a lying header of 48.
        let mut lying = zstd(&text);
        assert_eq!(lying.get(5), Some(&56));
        *lying.get_mut(5).unwrap() = 48;
        let mut out = vec![4];
        assert_eq!(
            decompress_block(Codec::Zstd, &lying, 48, 64, 40, &mut out),
            corrupt(Corruption::InvalidCompressedData)
        );
        // A zero limit with a zero header: the frame's own declared size still has to agree.
        assert_eq!(
            decompress_block(Codec::Zstd, &zstd(&text), 0, 0, 40, &mut out),
            corrupt(Corruption::UncompressedLenMismatch {
                expected: 0,
                actual: 56
            })
        );
        assert_eq!(out, [4]);
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI, which Miri cannot run")]
    fn zstd_requires_declared_content_size() {
        let text = b"persia ".repeat(8);
        let mut cctx = CCtx::create();
        cctx.set_parameter(zstd_safe::CParameter::ContentSizeFlag(false))
            .unwrap();
        let mut packed = vec![0; zstd_safe::compress_bound(text.len())];
        let n = cctx.compress2(&mut packed[..], &text).unwrap();
        packed.truncate(n);
        assert!(matches!(
            zstd_safe::get_frame_content_size(&packed),
            Ok(None)
        ));
        let mut out = Vec::new();
        assert_eq!(
            decompress_block(Codec::Zstd, &packed, 56, 56, 40, &mut out),
            corrupt(Corruption::InvalidCompressedData)
        );
        assert_eq!(out, b"");
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI, which Miri cannot run")]
    fn golden_frame_decodes() {
        let mut out = Vec::new();
        decompress_block(Codec::Zstd, GOLDEN_FRAME, 56, 56, 0, &mut out).unwrap();
        assert_eq!(out, b"persia ".repeat(8));
    }

    #[test]
    #[cfg_attr(miri, ignore = "zstd is C behind FFI, which Miri cannot run")]
    fn zstd_rejects_wrong_len_trailing_bytes_and_bad_level() {
        let text = b"persia ".repeat(8);
        let packed = zstd(&text);
        let mut out = vec![1, 2];
        for (len, max) in [(55, 64), (57, 64)] {
            assert_eq!(
                decompress_block(Codec::Zstd, &packed, len, max, 40, &mut out),
                corrupt(Corruption::UncompressedLenMismatch {
                    expected: len,
                    actual: 56
                })
            );
        }
        let mut trailing = packed.clone();
        trailing.push(0);
        let mut two_frames = packed.clone();
        two_frames.extend_from_slice(&packed);
        let truncated = packed.get(..packed.len() - 1).unwrap();
        // An empty skippable frame (RFC 8878 §3.1.2) after, or instead of, the data frame.
        let skippable = [0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0];
        let mut then_skippable = packed.clone();
        then_skippable.extend_from_slice(&skippable);
        let cases: [&[u8]; 7] = [
            &trailing,
            &two_frames,
            truncated,
            b"",
            &ZSTD_MAGIC,
            &then_skippable,
            &skippable,
        ];
        for bad in cases {
            assert_eq!(
                decompress_block(Codec::Zstd, bad, 56, 64, 40, &mut out),
                corrupt(Corruption::InvalidCompressedData),
                "{bad:x?}"
            );
        }
        assert_eq!(out, [1, 2]);

        let (min, max) = (zstd_safe::min_c_level(), zstd_safe::max_c_level());
        for level in [min - 1, max + 1] {
            let mut out = vec![3];
            assert_eq!(
                compress_block(Codec::Zstd, level, b"x", &mut out),
                Err(EncodeError::InvalidCompressionLevel { level, min, max })
            );
            assert_eq!(out, [3]);
        }
    }
}
