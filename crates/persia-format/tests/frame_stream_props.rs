//! Property tests for streamed frames (ROADMAP 1.7.5; SPEC §4.4 "Streaming and ranged access"). Golden header
//! bytes, the `FrameTooLarge` boundary and each header error path with exact offsets are unit tests in
//! `src/frame.rs`; `FrameHeader::decode` against `Frame::decode` on mutated logs is in `frame_props.rs`.
//!
//! Invariants:
//! 1. Streaming equivalence: for any kind, `frame_seq`, payload (empty and every padding width included) and any
//!    split of it into chunks (empty chunks included), `header() ++ chunks ++ finish()` is byte-for-byte what
//!    `encode_frame(.., Compression::None)` writes; `total_len()` is that length and `frame_len(payload_len)`;
//!    `remaining()` is the declared length minus the bytes given so far; `header()` decodes with
//!    `FrameHeader::decode` to the declared fields with no flags; the frame decodes back at any base.
//! 2. Overrun: an `update` that would take the payload past its declared length fails with
//!    `PayloadLenMismatch { declared, actual: bytes so far + chunk }` and changes nothing: `remaining()` is the
//!    same and the correct rest still yields the exact frame (the rejected bytes are not in the CRC).
//! 3. Underrun: `finish` after fewer bytes than declared fails with `PayloadLenMismatch { declared, actual }`,
//!    `actual` being the bytes given.

// Test-only helpers index, cast and unwrap on purpose (see reader_writer_props.rs).
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::cast_possible_truncation
)]

use persia_format::block::Compression;
use persia_format::frame::{
    FRAME_HEADER_LEN, Frame, FrameEncoder, FrameHeader, FrameKind, encode_frame, frame_len,
};
use persia_format::{EncodeError, Reader, Writer};
use proptest::prelude::*;
use proptest::sample::Index;

// ---------------------------------------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------------------------------------

/// Assigned kinds mostly, any byte (unknown kinds, 0 included) sometimes.
fn kind() -> impl Strategy<Value = FrameKind> {
    prop_oneof![3 => 1_u8..=5, 1 => any::<u8>()].prop_map(FrameKind::from_u8)
}

fn seq() -> impl Strategy<Value = u64> {
    prop_oneof![any::<u64>(), 0_u64..4, Just(u64::MAX)]
}

/// Payloads of every padding width, empty and exact multiples of 8 included.
fn payload(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..max),
        (0..max / 8).prop_flat_map(|n| proptest::collection::vec(any::<u8>(), n * 8)),
        proptest::collection::vec(any::<u8>(), 0..9),
    ]
}

/// Cut points into a payload of any length; repeated cuts (and cuts at 0 or the end) make empty chunks.
fn cuts() -> impl Strategy<Value = Vec<Index>> {
    proptest::collection::vec(any::<Index>(), 0..10)
}

/// The chunks `cuts` splits `payload` into, in order; their concatenation is `payload`.
fn split<'a>(payload: &'a [u8], cuts: &[Index]) -> Vec<&'a [u8]> {
    let mut at: Vec<usize> = cuts.iter().map(|i| i.index(payload.len() + 1)).collect();
    at.sort_unstable();
    let mut chunks = Vec::new();
    let mut start = 0;
    for end in at.into_iter().chain([payload.len()]) {
        chunks.push(&payload[start..end]);
        start = end;
    }
    chunks
}

fn encode(kind: FrameKind, seq: u64, payload: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    encode_frame(&mut w, kind, seq, payload, Compression::None).unwrap();
    w.into_inner()
}

// ---------------------------------------------------------------------------------------------------------
// Property bodies
// ---------------------------------------------------------------------------------------------------------

fn check_streaming_equivalence(
    kind: FrameKind,
    seq: u64,
    payload: &[u8],
    cuts: &[Index],
    base: u64,
) -> Result<(), TestCaseError> {
    let declared = payload.len() as u64;
    let expected = encode(kind, seq, payload);
    let mut enc = FrameEncoder::new(kind, seq, declared).unwrap();
    prop_assert_eq!(Some(enc.total_len()), frame_len(declared));
    prop_assert_eq!(enc.remaining(), declared);

    let h = FrameHeader::decode(&mut Reader::with_base_offset(enc.header(), base)).unwrap();
    prop_assert_eq!(h.kind(), kind);
    prop_assert_eq!(h.flags(), 0);
    prop_assert_eq!(h.payload_len(), declared);
    prop_assert_eq!(h.frame_seq(), seq);
    prop_assert_eq!(h.total_len(), enc.total_len());

    let mut out = enc.header().to_vec();
    let mut given = 0_u64;
    for chunk in split(payload, cuts) {
        enc.update(chunk).unwrap();
        given += chunk.len() as u64;
        prop_assert_eq!(enc.remaining(), declared - given);
        out.extend_from_slice(chunk);
    }
    let total = enc.total_len();
    out.extend_from_slice(&enc.finish().unwrap());
    prop_assert_eq!(out.len() as u64, total);
    prop_assert_eq!(&out, &expected);

    let mut r = Reader::with_base_offset(&out, base);
    let f = Frame::decode(&mut r).unwrap();
    prop_assert!(r.is_empty());
    prop_assert_eq!((f.kind(), f.frame_seq()), (kind, seq));
    prop_assert!(!f.is_compressed());
    prop_assert_eq!(f.stored_payload(), payload);
    prop_assert_eq!(f.total_len(), total);
    Ok(())
}

/// Invariant 2: the rejected chunk (`junk`, of `extra` bytes too many) is offered after `cut` good bytes.
fn check_overrun(
    payload: &[u8],
    cut: usize,
    extra: usize,
    junk: u8,
    base: u64,
) -> Result<(), TestCaseError> {
    let declared = payload.len() as u64;
    let mut enc = FrameEncoder::new(FrameKind::Segment, 7, declared).unwrap();
    enc.update(&payload[..cut]).unwrap();
    let over = vec![junk; payload.len() - cut + extra];
    prop_assert_eq!(
        enc.update(&over),
        Err(EncodeError::PayloadLenMismatch {
            declared,
            actual: declared + extra as u64,
        })
    );
    prop_assert_eq!(enc.remaining(), declared - cut as u64);
    enc.update(&payload[cut..]).unwrap();
    prop_assert_eq!(enc.remaining(), 0);
    let mut out = enc.header().to_vec();
    out.extend_from_slice(payload);
    out.extend_from_slice(&enc.finish().unwrap());
    prop_assert_eq!(&out, &encode(FrameKind::Segment, 7, payload));
    prop_assert!(Frame::decode(&mut Reader::with_base_offset(&out, base)).is_ok());
    Ok(())
}

/// Invariant 3: `finish` after `given < payload.len()` bytes.
fn check_underrun(payload: &[u8], given: usize) -> Result<(), TestCaseError> {
    let declared = payload.len() as u64;
    let mut enc = FrameEncoder::new(FrameKind::Manifest, 1, declared).unwrap();
    enc.update(&payload[..given]).unwrap();
    prop_assert_eq!(enc.remaining(), declared - given as u64);
    prop_assert_eq!(
        enc.finish(),
        Err(EncodeError::PayloadLenMismatch {
            declared,
            actual: given as u64,
        })
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Suites
// ---------------------------------------------------------------------------------------------------------

proptest! {
    #[test]
    fn streamed_frame_equals_encode_frame(
        kind in kind(),
        seq in seq(),
        payload in payload(600),
        cuts in cuts(),
        base in any::<u64>().prop_map(|b| b >> 1),
    ) {
        check_streaming_equivalence(kind, seq, &payload, &cuts, base)?;
    }

    #[test]
    fn overrun_is_rejected_and_changes_nothing(
        payload in payload(300),
        cut in any::<Index>(),
        extra in prop_oneof![Just(1_usize), 1_usize..40],
        junk in any::<u8>(),
    ) {
        let cut = cut.index(payload.len() + 1);
        check_overrun(&payload, cut, extra, junk, 12288)?;
    }

    #[test]
    fn underrun_is_rejected_at_finish(
        payload in payload(300).prop_filter("a byte to leave out", |p| !p.is_empty()),
        given in any::<Index>(),
    ) {
        check_underrun(&payload, given.index(payload.len()))?;
    }
}

/// The header is exactly `FRAME_HEADER_LEN` bytes and every tail is a multiple of 8 bytes with the padding,
/// so a frame streamed at an aligned offset leaves the next one aligned (SPEC §4.1).
#[test]
fn streamed_tail_keeps_alignment() {
    for len in 0..=64_u64 {
        let mut enc = FrameEncoder::new(FrameKind::Padding, 0, len).unwrap();
        assert_eq!(enc.header().len(), FRAME_HEADER_LEN);
        enc.update(&vec![1; len as usize]).unwrap();
        let tail = enc.finish().unwrap();
        assert_eq!((FRAME_HEADER_LEN as u64 + len + tail.len() as u64) % 8, 0);
        assert!(tail.len() >= 8 && tail.len() < 16, "{len}: {}", tail.len());
    }
}

#[cfg(feature = "slow")]
proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn slow_streamed_frame_equals_encode_frame(
        kind in kind(),
        seq in seq(),
        payload in payload(8192),
        cuts in proptest::collection::vec(any::<Index>(), 0..64),
        base in any::<u64>().prop_map(|b| b >> 1),
    ) {
        check_streaming_equivalence(kind, seq, &payload, &cuts, base)?;
    }
}
