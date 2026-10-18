//! Fuzz the frame scanner (ROADMAP 1.7.2.3; SPEC §4.4; ADR-0009): an arbitrary frame log never panics; yielded
//! frames are contiguous from the base, 8-aligned, and re-encode to their exact bytes; the scanner stops where
//! its `ScanEnd` says, and the `ScanEnd` (variant, offset and error) is what SPEC §4.4 and ADR-0009 give when
//! recomputed from the raw bytes; `Frame::decode` at that offset fails with the same error and moves nothing;
//! `Frame::payload` on a yielded frame returns at most its limit or fails as corruption or out of memory; a scan
//! that checks the `frame_seq` run (ADR-0013) yields a prefix of those frames and stops at a run break as corruption.
//! `StreamScanner` yields the same frames and end; its `reframe` (ROADMAP 2.4.17) copies every yielded frame exactly
//! as `Frame::reframe` does and fails at the stop offset with `check_frame`'s error, writing nothing or exactly the
//! renumbered header and stored payload.
//!
//! Input: `mode u8, data..`. Mode bit 0: fix the CRC of every frame along the chain from the start; bit 1: fix the
//! CRC of every frame found at an 8-aligned offset (in order). Random bytes almost never carry a valid CRC, so
//! without these the padding, `reserved` and `flags` checks would only see the seed corpus. Bit 2: base the log
//! near the top of `u64` instead of at 12288. Bits 3..: the payload limit is `16 << (mode >> 3)` bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::block::Compression;
use persia_format::checksum::crc32c;
use persia_format::frame::{
    FLAG_ZSTD, FRAME_MAGIC, Frame, FrameScanner, MIN_FRAME_LEN, ScanEnd, StreamScanner,
    encode_frame,
};
use persia_format::{Corruption, Error, Reader, Unsupported, Writer};

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// `24 + pad8(payload_len) + 8`, without overflow.
fn total_len(payload_len: u64) -> u128 {
    24 + u128::from(payload_len).div_ceil(8) * 8 + 8
}

/// Rewrites the CRC of the frame at `pos` if it has a magic and fits; returns its end.
fn fix_crc_at(b: &mut [u8], pos: usize) -> Option<usize> {
    let rest = b.get(pos..)?;
    if rest.len() < MIN_FRAME_LEN || rest[..4] != FRAME_MAGIC {
        return None;
    }
    let total = total_len(u64_at(rest, 8));
    if total > rest.len() as u128 {
        return None;
    }
    let end = pos + total as usize;
    let crc = crc32c(&b[pos + 4..end - 8]);
    b[end - 8..end - 4].copy_from_slice(&crc.to_le_bytes());
    Some(end)
}

/// The scan end SPEC §4.4 / ADR-0009 give for the input `rest`, at absolute offset `o`, whose first frame the
/// scanner did not accept. A valid frame there would be a scanner bug, reported as `None`.
fn expected_end(rest: &[u8], o: u64) -> Option<ScanEnd> {
    if rest.is_empty() {
        return Some(ScanEnd::Clean { offset: o });
    }
    let invalid = |error: Error, known_end: Option<usize>| {
        let after = known_end.map_or(rest, |n| &rest[n..]);
        Some(if after.iter().all(|&x| x == 0) {
            ScanEnd::TornTail { offset: o, error }
        } else {
            ScanEnd::Corrupt { offset: o, error }
        })
    };
    let corrupt = |offset, reason| Error::Corrupt { offset, reason };
    let available = rest.len() as u64;
    if rest.len() < MIN_FRAME_LEN {
        let reason = Corruption::UnexpectedEof {
            needed: 32,
            available,
        };
        return invalid(corrupt(o, reason), Some(rest.len()));
    }
    if rest[..4] != FRAME_MAGIC {
        return invalid(corrupt(o, Corruption::BadFrameMagic), None);
    }
    let payload_len = u64_at(rest, 8);
    let total = total_len(payload_len);
    if total > u128::from(available) {
        let needed = u64::try_from(total).unwrap_or(u64::MAX);
        let reason = Corruption::UnexpectedEof { needed, available };
        return invalid(corrupt(o + 8, reason), Some(rest.len()));
    }
    let n = total as usize;
    let crc_at = n - 8;
    let stored = u32::from_le_bytes(rest[crc_at..crc_at + 4].try_into().unwrap());
    let computed = crc32c(&rest[4..crc_at]);
    if stored != computed {
        let reason = Corruption::ChecksumMismatch { stored, computed };
        return invalid(corrupt(o + crc_at as u64, reason), Some(n));
    }
    let payload_end = 24 + payload_len as usize;
    for range in [payload_end..crc_at, crc_at + 4..n] {
        let start = range.start;
        if let Some(i) = rest[range].iter().position(|&x| x != 0) {
            let reason = Corruption::NonZeroReserved {
                value: rest[start + i],
            };
            return invalid(corrupt(o + (start + i) as u64, reason), Some(n));
        }
    }
    let flags = u32::from_le_bytes([rest[5], rest[6], rest[7], 0]);
    let unknown = flags & !FLAG_ZSTD;
    (unknown != 0).then(|| ScanEnd::Unsupported {
        offset: o,
        error: Error::Unsupported {
            offset: o + 5,
            reason: Unsupported::FrameFlags { flags: unknown },
        },
    })
}

/// The bytes a valid frame must have been, rebuilt from what the scanner yielded.
fn reencode(f: &Frame<'_>) -> Vec<u8> {
    let mut w = Writer::new();
    if f.is_compressed() {
        let stored = f.stored_payload();
        w.write_bytes(&FRAME_MAGIC);
        w.write_u8(f.kind().to_u8());
        w.write_bytes(&FLAG_ZSTD.to_le_bytes()[..3]);
        w.write_u64(stored.len() as u64);
        w.write_u64(f.frame_seq());
        w.write_bytes(stored);
        w.write_zeros((8 - stored.len() % 8) % 8);
        let crc = crc32c(&w.as_slice()[4..]);
        w.write_u32(crc);
        w.write_u32(0);
    } else {
        encode_frame(
            &mut w,
            f.kind(),
            f.frame_seq(),
            f.stored_payload(),
            Compression::None,
        )
        .expect("an uncompressed frame always encodes");
    }
    w.into_inner()
}

fuzz_target!(|input: &[u8]| {
    let Some((&mode, rest)) = input.split_first() else {
        return;
    };
    let mut data = rest.to_vec();
    if mode & 1 != 0 {
        let mut pos = 0;
        while let Some(end) = fix_crc_at(&mut data, pos) {
            pos = end;
        }
    }
    if mode & 2 != 0 {
        for pos in (0..data.len()).step_by(8) {
            fix_crc_at(&mut data, pos);
        }
    }
    let base = if mode & 4 == 0 {
        12288
    } else {
        (u64::MAX - data.len() as u64) & !7
    };
    let limit = 16_usize << (mode >> 3);

    let mut scanner = FrameScanner::new(&data, base);
    let mut next = base;
    let mut seen = Vec::new();
    for f in scanner.by_ref() {
        seen.push((f.offset(), f.frame_seq()));
        assert_eq!(f.offset(), next, "frames are contiguous from the base");
        assert_eq!(f.offset() % 8, 0, "frames start 8-aligned");
        let start = (f.offset() - base) as usize;
        let bytes = &data[start..(f.end() - base) as usize];
        assert_eq!(
            reencode(&f),
            bytes,
            "a yielded frame re-encodes to its bytes"
        );
        match f.payload(limit) {
            Ok(payload) => {
                assert!(payload.len() <= limit, "bomb limit exceeded");
                if !f.is_compressed() {
                    assert_eq!(&*payload, f.stored_payload());
                }
            }
            Err(err) => assert!(
                err.is_corruption() || matches!(err, Error::OutOfMemory { .. }),
                "payload fails only as corruption or OOM: {err:?}"
            ),
        }
        next = f.end();
    }
    assert!(scanner.next().is_none(), "the scanner is fused");
    let end = scanner.end().expect("a stopped scanner has an end").clone();
    assert_eq!(scanner.offset(), end.offset());
    assert_eq!(
        end.offset(),
        next,
        "the scan stops right after the last yielded frame"
    );

    let rest = &data[(next - base) as usize..];
    let expected = expected_end(rest, next).expect("the scanner stopped at a valid frame");
    assert_eq!(end, expected, "ScanEnd disagrees with SPEC §4.4 / ADR-0009");
    if let Some(error) = end.error() {
        assert_eq!(
            matches!(end, ScanEnd::Unsupported { .. }),
            matches!(error, Error::Unsupported { .. })
        );
        let mut r = Reader::with_base_offset(rest, next);
        assert_eq!(Frame::decode(&mut r).as_ref().err(), Some(error));
        assert_eq!(r.position(), 0, "a failed decode moves nothing");
    }
    check_seq_runs(&data, base, &seen, &end);
    check_stream(&data, base, &seen);
    check_reframe(&data, base, &seen, &end);
});

/// `StreamScanner` (ROADMAP 2.4.12) yields the frames (every header field) and the end of `FrameScanner` over the
/// same bytes, for every `frame_seq` check and a chunk length picked from the input.
fn check_stream(data: &[u8], base: u64, seen: &[(u64, u64)]) {
    let chunk = 1 + data.len() % 61;
    let end = base + data.len() as u64;
    let first = seen.first().map_or(1, |&(_, s)| s);
    let pairs = [
        (FrameScanner::new(data, base), StreamScanner::new(base, end)),
        (
            FrameScanner::new(data, base).contiguous(),
            StreamScanner::new(base, end).contiguous(),
        ),
        (
            FrameScanner::new(data, base).expect_frame_seq(first),
            StreamScanner::new(base, end).expect_frame_seq(first),
        ),
    ];
    for (mut slice, stream) in pairs {
        let mut stream = stream.with_chunk_len(chunk);
        let mut read = |offset: u64, buf: &mut [u8]| -> Result<(), ()> {
            assert!(
                !buf.is_empty() && buf.len() <= chunk.max(24),
                "bounded reads"
            );
            let start = (offset - base) as usize;
            buf.copy_from_slice(&data[start..start + buf.len()]);
            Ok(())
        };
        // Every header field: offset, total length, kind, `frame_seq` and flags (a valid frame has only known ones).
        let want: Vec<_> = slice
            .by_ref()
            .map(|f| {
                let flags = u32::from(f.is_compressed()) * FLAG_ZSTD;
                (f.offset(), f.total_len(), f.kind(), f.frame_seq(), flags)
            })
            .collect();
        let mut got = Vec::new();
        while let Some(h) = stream.next_frame(&mut read).unwrap() {
            got.push((
                h.offset(),
                h.total_len(),
                h.kind(),
                h.frame_seq(),
                h.flags(),
            ));
        }
        assert_eq!(got, want, "the stream scanner yields the same frames");
        assert_eq!(stream.end(), slice.end(), "and ends the same way");
    }
}

/// A checked scan (ADR-0013) yields a prefix of the unchecked frames and stops either where the unchecked scan did,
/// with the same end, or before the first frame whose `frame_seq` breaks the run, as corruption there.
fn check_seq_runs(data: &[u8], base: u64, seen: &[(u64, u64)], unchecked: &ScanEnd) {
    let first = seen.first().map_or(1, |&(_, s)| s);
    for checked in [
        FrameScanner::new(data, base).contiguous(),
        FrameScanner::new(data, base).expect_frame_seq(1),
        FrameScanner::new(data, base).expect_frame_seq(first),
    ] {
        let mut checked = checked;
        let got: Vec<(u64, u64)> = checked
            .by_ref()
            .map(|f| (f.offset(), f.frame_seq()))
            .collect();
        let end = checked.end().expect("a stopped scanner has an end").clone();
        assert_eq!(checked.offset(), end.offset());
        assert_eq!(&seen[..got.len()], &got[..], "checked frames are a prefix");
        if got.len() == seen.len() {
            assert_eq!(
                &end, unchecked,
                "a run that never breaks ends like the unchecked scan"
            );
        } else {
            let (offset, _) = seen[got.len()];
            assert!(
                matches!(
                    &end,
                    ScanEnd::Corrupt { offset: o, error: Error::Corrupt { offset: e, reason } }
                        if *o == offset && *e == offset + 16 && matches!(
                            reason,
                            persia_format::Corruption::FrameSeqMismatch { .. }
                                | persia_format::Corruption::FrameSeqOutOfRange { .. }
                        )
                ),
                "a run break is corruption at the frame: {end:?}"
            );
        }
    }
}

/// A positioned read, as `StreamScanner` takes it.
type ReadFn<'a> = dyn FnMut(u64, &mut [u8]) -> Result<(), ()> + 'a;

/// `StreamScanner::reframe` (ROADMAP 2.4.17) at every yielded frame writes exactly `Frame::reframe` of it and
/// returns its header; at the stop offset it returns `check_frame`'s error (the scan end's, for a frame that is
/// there) and writes nothing or exactly the copy's header and stored payload. Reads stay bounded.
fn check_reframe(data: &[u8], base: u64, seen: &[(u64, u64)], end: &ScanEnd) {
    let chunk = 1 + data.len() % 53;
    let limit = base + data.len() as u64;
    let mut read = |offset: u64, buf: &mut [u8]| -> Result<(), ()> {
        assert!(
            !buf.is_empty() && buf.len() <= chunk.max(24),
            "bounded reads"
        );
        let start = (offset - base) as usize;
        buf.copy_from_slice(&data[start..start + buf.len()]);
        Ok(())
    };
    let reframe = |read: &mut ReadFn<'_>, o: u64, seq: u64| {
        let mut written = Vec::new();
        let mut write = |bytes: &[u8]| -> Result<(), ()> {
            assert!(
                !bytes.is_empty() && bytes.len() <= chunk.max(24),
                "bounded writes"
            );
            written.extend_from_slice(bytes);
            Ok(())
        };
        let mut read = |o: u64, buf: &mut [u8]| read(o, buf);
        let result = StreamScanner::new(o, limit)
            .with_chunk_len(chunk)
            .reframe(seq, &mut read, &mut write)
            .unwrap();
        (result, written)
    };
    for &(offset, frame_seq) in seen {
        let start = (offset - base) as usize;
        let frame = Frame::decode(&mut Reader::with_base_offset(&data[start..], offset))
            .expect("a yielded frame decodes");
        let new_seq = !frame_seq;
        let (result, written) = reframe(&mut read, offset, new_seq);
        let header = result.expect("a yielded frame reframes");
        assert_eq!(
            (header.offset(), header.total_len(), header.frame_seq()),
            (offset, frame.total_len(), frame_seq)
        );
        let mut w = Writer::new();
        frame.reframe(&mut w, new_seq);
        assert_eq!(
            written,
            w.as_slice(),
            "the streamed copy is Frame::reframe's"
        );
    }
    let o = end.offset();
    let (result, written) = reframe(&mut read, o, 7);
    let checked = StreamScanner::new(o, limit)
        .with_chunk_len(chunk)
        .check_frame(&mut read)
        .unwrap();
    assert_eq!(result, checked, "reframe fails as check_frame does");
    let error = result.expect_err("the scan stopped at an invalid frame");
    if let Some(end_error) = end.error() {
        assert_eq!(&error, end_error, "the frame's own error");
    }
    if !written.is_empty() {
        let rest = &data[(o - base) as usize..];
        let payload_end = 24 + u64_at(rest, 8) as usize;
        assert_eq!(
            written.len(),
            payload_end,
            "header and stored payload, no trailer"
        );
        assert_eq!(written[..16], rest[..16]);
        assert_eq!(written[16..24], 7_u64.to_le_bytes());
        assert_eq!(written[24..], rest[24..payload_end]);
    }
}
