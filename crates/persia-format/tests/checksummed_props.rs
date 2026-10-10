//! Property tests for `Checksummed<T>` (ROADMAP 1.3.3) on regions with the SPEC's sizes: a container header
//! (4096 bytes, CRC at 12, SPEC §4.2) and a superblock slot (2048 bytes, CRC at 2044, SPEC §4.3). Opaque
//! payload bytes stand in for the fields, so every bit of the region is exercised.

// Test-only helpers index on purpose (see reader_writer_props.rs).
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use persia_format::checksummed::{Checksummed, ChecksummedLayout};
use persia_format::{Corruption, Error, Reader, Result, Writer};
use proptest::prelude::*;
use proptest::sample::Index;

/// `LEN - 4` opaque bytes around a CRC field at `CRC_OFFSET`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Opaque<const LEN: usize, const CRC_OFFSET: usize> {
    head: Vec<u8>,
    tail: Vec<u8>,
}

impl<const LEN: usize, const CRC_OFFSET: usize> Opaque<LEN, CRC_OFFSET> {
    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            head: bytes[..CRC_OFFSET].to_vec(),
            tail: bytes[CRC_OFFSET..LEN - 4].to_vec(),
        }
    }
}

impl<const LEN: usize, const CRC_OFFSET: usize> ChecksummedLayout for Opaque<LEN, CRC_OFFSET> {
    const LEN: usize = LEN;
    const CRC_OFFSET: usize = CRC_OFFSET;
    fn encode_fields(&self, w: &mut Writer) {
        w.write_bytes(&self.head);
        w.write_u32(0);
        w.write_bytes(&self.tail);
    }
    fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
        let head = r.read_bytes(CRC_OFFSET)?.to_vec();
        r.skip(4)?;
        let tail = r.rest().to_vec();
        Ok(Self { head, tail })
    }
}

type Header = Opaque<4096, 12>;
type Slot = Opaque<2048, 2044>;

fn encode<T: ChecksummedLayout + Clone>(value: &T) -> Vec<u8> {
    let mut w = Writer::new();
    Checksummed(value.clone()).encode(&mut w).unwrap();
    w.into_inner()
}

fn is_mismatch_at(result: &Result<impl core::fmt::Debug>, offset: u64) -> bool {
    matches!(result, Err(Error::Corrupt { offset: o, reason: Corruption::ChecksumMismatch { .. } }) if *o == offset)
}

/// Pseudo-random fill from a seed (xorshift64), so the sample is reproducible.
fn sample(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed.to_le_bytes()[0]
        })
        .collect()
}

fn assert_every_bit_flip_detected<T: ChecksummedLayout + Clone + core::fmt::Debug>(value: &T) {
    let bytes = encode(value);
    assert_eq!(bytes.len(), T::LEN);
    let crc_offset = u64::try_from(T::CRC_OFFSET).unwrap();
    for bit in 0..bytes.len() * 8 {
        let mut flipped = bytes.clone();
        flipped[bit / 8] ^= 1 << (bit % 8);
        let mut r = Reader::new(&flipped);
        let result = Checksummed::<T>::decode(&mut r);
        assert!(is_mismatch_at(&result, crc_offset), "bit {bit}: {result:?}");
        assert_eq!(r.position(), 0, "bit {bit}");
    }
}

#[test]
fn every_bit_flip_in_a_header_is_detected() {
    assert_every_bit_flip_detected(&Header::from_bytes(&sample(4096, 0x5045_5253_4941)));
}

#[test]
fn every_bit_flip_in_a_slot_is_detected() {
    assert_every_bit_flip_detected(&Slot::from_bytes(&sample(2048, 42)));
}

proptest! {
    #[test]
    fn header_round_trips_at_any_base(bytes in proptest::collection::vec(any::<u8>(), 4096), base in 0..u64::MAX / 2) {
        let value = Header::from_bytes(&bytes);
        let encoded = encode(&value);
        prop_assert_eq!(&encoded[..12], &bytes[..12]);
        prop_assert_eq!(&encoded[16..], &bytes[12..4092]);
        let mut r = Reader::with_base_offset(&encoded, base);
        prop_assert_eq!(Checksummed::<Header>::decode(&mut r).map(|c| c.0), Ok(value));
        prop_assert!(r.is_empty());
    }

    /// CRC32C detects every burst error of at most 32 bits, wherever it lands, CRC field included.
    #[test]
    fn bursts_up_to_32_bits_are_detected(bytes in proptest::collection::vec(any::<u8>(), 2048),
                                         start in any::<Index>(), mask in 1..=u32::MAX, base in 0..u64::MAX / 2) {
        let encoded = encode(&Slot::from_bytes(&bytes));
        let start_bit = start.index(encoded.len() * 8 - 31);
        let mut corrupted = encoded.clone();
        // Bit 0 of `mask` lands on `start_bit`, so the burst spans at most 32 bits.
        for i in 0..32 {
            if mask >> i & 1 == 1 {
                let bit = start_bit + i;
                corrupted[bit / 8] ^= 1 << (bit % 8);
            }
        }
        let mut r = Reader::with_base_offset(&corrupted, base);
        let result = Checksummed::<Slot>::decode(&mut r);
        prop_assert!(is_mismatch_at(&result, base + 2044), "{:?}", result);
        prop_assert_eq!(r.position(), 0);
    }

    #[test]
    fn short_input_is_eof(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let mut r = Reader::new(&bytes);
        let result = Checksummed::<Slot>::decode(&mut r);
        let is_eof = matches!(result, Err(Error::Corrupt { offset: 0, reason: Corruption::UnexpectedEof { needed: 2048, .. } }));
        prop_assert!(is_eof, "{:?}", result);
        prop_assert_eq!(r.position(), 0);
    }
}
