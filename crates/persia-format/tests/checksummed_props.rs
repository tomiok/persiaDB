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

// Edge layouts: CRC first, CRC last in a small region, and a region that is only the CRC field.
type CrcFirst = Opaque<64, 0>;
type CrcLast = Opaque<64, 60>;
type CrcOnly = Opaque<4, 0>;

/// What `decode` must return for arbitrary `LEN` bytes, computed from the definition: CRC32C over every byte
/// except the 4-byte LE field at `CRC_OFFSET` (SPEC §4.2/§4.3), mismatch reported at the field's absolute offset.
fn reference_decode<T: ChecksummedLayout>(bytes: &[u8], base: u64) -> Result<()> {
    let crc_end = T::CRC_OFFSET + 4;
    let stored = u32::from_le_bytes(bytes[T::CRC_OFFSET..crc_end].try_into().unwrap());
    let mut covered = bytes[..T::CRC_OFFSET].to_vec();
    covered.extend_from_slice(&bytes[crc_end..T::LEN]);
    let computed = persia_format::checksum::crc32c(&covered);
    if stored == computed {
        Ok(())
    } else {
        Err(Error::Corrupt {
            offset: base + u64::try_from(T::CRC_OFFSET).unwrap(),
            reason: Corruption::ChecksumMismatch { stored, computed },
        })
    }
}

/// Decoding arbitrary bytes agrees exactly with the reference (error kind, offset, stored and computed),
/// moves the cursor by `LEN` on success and not at all on failure, and leaves trailing bytes alone.
fn check_against_reference<T: ChecksummedLayout + core::fmt::Debug>(
    bytes: &[u8],
    base: u64,
) -> core::result::Result<(), TestCaseError> {
    let mut r = Reader::with_base_offset(bytes, base);
    let result = Checksummed::<T>::decode(&mut r).map(|_| ());
    prop_assert_eq!(&result, &reference_decode::<T>(bytes, base));
    prop_assert_eq!(r.position(), if result.is_ok() { T::LEN } else { 0 });
    Ok(())
}

/// Like `Opaque`, but writes an arbitrary value as the CRC placeholder.
#[derive(Debug, Clone)]
struct Dirty<const LEN: usize, const CRC_OFFSET: usize>(Opaque<LEN, CRC_OFFSET>, u32);

impl<const LEN: usize, const CRC_OFFSET: usize> ChecksummedLayout for Dirty<LEN, CRC_OFFSET> {
    const LEN: usize = LEN;
    const CRC_OFFSET: usize = CRC_OFFSET;
    fn encode_fields(&self, w: &mut Writer) {
        w.write_bytes(&self.0.head);
        w.write_u32(self.1);
        w.write_bytes(&self.0.tail);
    }
    fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
        Opaque::decode_fields(r).map(|o| Self(o, 0))
    }
}

/// Flips the bits of `mask` starting at `start_bit` (a burst of at most 32 bits).
fn apply_burst(bytes: &mut [u8], start_bit: usize, mask: u32) {
    for i in 0..32 {
        if mask >> i & 1 == 1 {
            let bit = start_bit + i;
            bytes[bit / 8] ^= 1 << (bit % 8);
        }
    }
}

#[test]
fn every_bit_flip_in_edge_layouts_is_detected() {
    assert_every_bit_flip_detected(&CrcFirst::from_bytes(&sample(64, 1)));
    assert_every_bit_flip_detected(&CrcLast::from_bytes(&sample(64, 2)));
    assert_every_bit_flip_detected(&CrcOnly::from_bytes(&[]));
}

proptest! {
    /// Arbitrary (mostly invalid) input: `decode` is exactly the reference, for every layout shape.
    #[test]
    fn arbitrary_bytes_decode_like_the_reference(mut bytes in proptest::collection::vec(any::<u8>(), 4096..4200),
                                                 fix in 0..6_usize, base in 0..u64::MAX / 2) {
        // Random bytes almost never carry a valid CRC; make one layout's region valid so both paths run.
        match fix {
            0 => set_valid_crc::<Header>(&mut bytes),
            1 => set_valid_crc::<Slot>(&mut bytes),
            2 => set_valid_crc::<CrcFirst>(&mut bytes),
            3 => set_valid_crc::<CrcLast>(&mut bytes),
            4 => set_valid_crc::<CrcOnly>(&mut bytes),
            _ => {}
        }
        check_against_reference::<Header>(&bytes, base)?;
        check_against_reference::<Slot>(&bytes, base)?;
        check_against_reference::<CrcFirst>(&bytes, base)?;
        check_against_reference::<CrcLast>(&bytes, base)?;
        check_against_reference::<CrcOnly>(&bytes, base)?;
        if fix < 5 {
            let ok = [
                Checksummed::<Header>::decode(&mut Reader::new(&bytes)).is_ok(),
                Checksummed::<Slot>::decode(&mut Reader::new(&bytes)).is_ok(),
                Checksummed::<CrcFirst>::decode(&mut Reader::new(&bytes)).is_ok(),
                Checksummed::<CrcLast>::decode(&mut Reader::new(&bytes)).is_ok(),
                Checksummed::<CrcOnly>::decode(&mut Reader::new(&bytes)).is_ok(),
            ];
            prop_assert!(ok[fix], "the fixed-up region must decode");
        }
    }

    /// Valid regions, then corrupted by a burst of at most 32 bits anywhere (CRC field included): the mismatch
    /// carries the exact stored/computed values, for a CRC in the middle (header) and at the end (slot).
    #[test]
    fn bursts_report_exact_values(bytes in proptest::collection::vec(any::<u8>(), 4096),
                                  start in any::<Index>(), mask in 1..=u32::MAX, base in 0..u64::MAX / 2) {
        for encoded in [encode(&Header::from_bytes(&bytes)), encode(&Slot::from_bytes(&bytes[..2048]))] {
            let mut corrupted = encoded.clone();
            apply_burst(&mut corrupted, start.index(encoded.len() * 8 - 31), mask);
            let result = if encoded.len() == 4096 {
                check_against_reference::<Header>(&corrupted, base)?;
                Checksummed::<Header>::decode(&mut Reader::new(&corrupted)).map(|_| ())
            } else {
                check_against_reference::<Slot>(&corrupted, base)?;
                Checksummed::<Slot>::decode(&mut Reader::new(&corrupted)).map(|_| ())
            };
            prop_assert!(result.is_err(), "burst went undetected");
        }
    }

    /// Encoding appends exactly the region after whatever the writer holds, with the fields untouched and a
    /// CRC that the reference accepts at the writer's absolute position.
    #[test]
    fn encode_appends_a_region_the_reference_accepts(prefix in proptest::collection::vec(any::<u8>(), 0..64),
                                                     payload in proptest::collection::vec(any::<u8>(), 60),
                                                     base in 0..u64::MAX / 2) {
        let mut w = Writer::with_base_offset(base);
        w.write_bytes(&prefix);
        let value = CrcLast::from_bytes(&payload);
        Checksummed(value.clone()).encode(&mut w).unwrap();
        let out = w.into_inner();
        prop_assert_eq!(&out[..prefix.len()], &prefix[..]);
        let region = &out[prefix.len()..];
        prop_assert_eq!(region.len(), 64);
        prop_assert_eq!(&region[..60], &payload[..]);
        let at = base + u64::try_from(prefix.len()).unwrap();
        prop_assert_eq!(reference_decode::<CrcLast>(region, at), Ok(()));
        let mut r = Reader::with_base_offset(&out, base);
        r.skip(prefix.len()).unwrap();
        prop_assert_eq!(Checksummed::<CrcLast>::decode(&mut r).map(|c| c.0), Ok(value));
        prop_assert!(r.is_empty());
    }

    /// The placeholder `encode_fields` writes for the CRC never influences the output.
    #[test]
    fn placeholder_is_ignored(payload in proptest::collection::vec(any::<u8>(), 4092), placeholder: u32) {
        let value = Header::from_bytes(&payload);
        prop_assert_eq!(encode(&Dirty(value.clone(), placeholder)), encode(&value));
        let value = CrcFirst::from_bytes(&payload[..60]);
        prop_assert_eq!(encode(&Dirty(value.clone(), placeholder)), encode(&value));
    }

    /// Any wrong length from `encode_fields` is `LayoutLen { actual }` and leaves the writer untouched.
    #[test]
    fn wrong_layout_length_is_rejected(head_len in 0..80_usize, tail_len in 0..80_usize,
                                       prefix in proptest::collection::vec(any::<u8>(), 0..8)) {
        let value = CrcLast { head: vec![0x5a; head_len], tail: vec![0xa5; tail_len] };
        let actual = head_len + 4 + tail_len;
        let mut w = Writer::new();
        w.write_bytes(&prefix);
        let result = Checksummed(value).encode(&mut w);
        if actual == 64 {
            // Any split of 60 payload bytes around the field is still a valid 64-byte region.
            prop_assert_eq!(result, Ok(()));
            prop_assert_eq!(w.len(), prefix.len() + 64);
        } else {
            prop_assert_eq!(result, Err(persia_format::EncodeError::LayoutLen { expected: 64, actual }));
            prop_assert_eq!(w.as_slice(), &prefix[..]);
        }
    }

    /// Same input, same bytes.
    #[test]
    fn encoding_is_deterministic(payload in proptest::collection::vec(any::<u8>(), 2044), base in 0..u64::MAX / 2) {
        let value = Slot::from_bytes(&payload);
        let mut a = Writer::with_base_offset(base);
        let mut b = Writer::with_base_offset(base);
        Checksummed(value.clone()).encode(&mut a).unwrap();
        Checksummed(value).encode(&mut b).unwrap();
        prop_assert_eq!(a.as_slice(), b.as_slice());
    }
}

/// Overwrites the CRC field of the `T` region at the start of `bytes` with the checksum of the rest.
fn set_valid_crc<T: ChecksummedLayout>(bytes: &mut [u8]) {
    let crc_end = T::CRC_OFFSET + 4;
    let mut covered = bytes[..T::CRC_OFFSET].to_vec();
    covered.extend_from_slice(&bytes[crc_end..T::LEN]);
    let crc = persia_format::checksum::crc32c(&covered);
    bytes[T::CRC_OFFSET..crc_end].copy_from_slice(&crc.to_le_bytes());
}
