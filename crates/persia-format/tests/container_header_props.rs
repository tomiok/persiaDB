//! Property tests for the container header and superblock slots (ROADMAP 1.6; SPEC §4.2, §4.3; ADR-0007).
//! Decoding is checked against naive reference validators written straight from the SPEC tables, and slot
//! selection against the SPEC §4.3 truth table under random corruption of either slot.

// Test-only helpers index on purpose (see reader_writer_props.rs).
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use persia_format::checksum::{Crc32c, crc32c};
use persia_format::header::{FORMAT_MAJOR, HEADER_LEN, Header, MAGIC};
use persia_format::superblock::{ActiveSuperblock, SLOT_LEN, SLOTS_LEN, Slot, Superblock};
use persia_format::{Corruption, Error, Reader, Unsupported, Writer};
use proptest::prelude::*;
use proptest::sample::Index;

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn header_crc(b: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(&b[..12]);
    crc.update(&b[16..HEADER_LEN]);
    crc.finish()
}

/// What SPEC §4.2 says about `b` (at least `HEADER_LEN` bytes): the decoded header, or the class of the first
/// failing check as `(offset, is_unsupported)`.
fn reference_header(b: &[u8]) -> Result<Header, (u64, bool)> {
    if b[..8] != MAGIC {
        return Err((0, false));
    }
    if u16_at(b, 8) != FORMAT_MAJOR {
        return Err((8, true));
    }
    if u32_at(b, 12) != header_crc(b) {
        return Err((12, false));
    }
    if u64_at(b, 16) != 0 {
        return Err((16, true));
    }
    let page_size = u32_at(b, 48);
    if ![4096, 8192, 16384, 32768, 65536].contains(&page_size) {
        return Err((48, false));
    }
    if let Some(at) = b[52..HEADER_LEN].iter().position(|&x| x != 0) {
        return Err((52 + at as u64, false));
    }
    Ok(Header {
        format_minor: u16_at(b, 10),
        incompat_flags: 0,
        rocompat_flags: u64_at(b, 24),
        db_uuid: b[32..48].try_into().unwrap(),
        page_size,
    })
}

/// What SPEC §4.3 says about one slot: valid iff the CRC matches and the reserved bytes are zero.
fn reference_slot(b: &[u8]) -> Option<Superblock> {
    let valid = u32_at(b, SLOT_LEN - 4) == crc32c(&b[..SLOT_LEN - 4])
        && b[48..SLOT_LEN - 4].iter().all(|&x| x == 0);
    valid.then(|| Superblock {
        seq: u64_at(b, 0),
        manifest_off: u64_at(b, 8),
        manifest_len: u64_at(b, 16),
        log_end: u64_at(b, 24),
        wal_start: u64_at(b, 32),
        fence_token: u64_at(b, 40),
    })
}

/// The SPEC §4.3 selection table; `Err` carries the expected corruption.
fn reference_select(
    a: Option<Superblock>,
    b: Option<Superblock>,
) -> Result<ActiveSuperblock, Corruption> {
    let pick = |slot, superblock| Ok(ActiveSuperblock { slot, superblock });
    match (a, b) {
        (Some(a), Some(b)) if a.seq == b.seq && a != b => {
            Err(Corruption::SuperblockConflict { seq: a.seq })
        }
        (Some(a), Some(b)) if b.seq > a.seq => pick(Slot::B, b),
        (Some(a), _) => pick(Slot::A, a),
        (None, Some(b)) => pick(Slot::B, b),
        (None, None) => Err(Corruption::NoValidSuperblock),
    }
}

fn valid_header() -> impl Strategy<Value = Header> {
    (
        any::<u16>(),
        any::<[u8; 16]>(),
        prop::sample::select(vec![4096_u32, 8192, 16384, 32768, 65536]),
    )
        .prop_map(|(format_minor, db_uuid, page_size)| Header {
            format_minor,
            incompat_flags: 0,
            rocompat_flags: 0,
            db_uuid,
            page_size,
        })
}

fn encode_header(h: &Header) -> Vec<u8> {
    let mut w = Writer::new();
    h.encode(&mut w).unwrap();
    w.into_inner()
}

fn superblock(seq: impl Strategy<Value = u64>) -> impl Strategy<Value = Superblock> {
    (seq, any::<[u64; 5]>()).prop_map(|(seq, f)| Superblock {
        seq,
        manifest_off: f[0],
        manifest_len: f[1],
        log_end: f[2],
        wal_start: f[3],
        fence_token: f[4],
    })
}

fn encode_slot(sb: &Superblock) -> Vec<u8> {
    let mut w = Writer::new();
    sb.encode(&mut w).unwrap();
    w.into_inner()
}

/// How a slot is damaged: torn writes, bit rot, and garbage that still carries a valid CRC.
#[derive(Debug, Clone)]
enum SlotDamage {
    Keep,
    FlipBit(Index, u8),
    SetByte(Index, u8),
    /// Never written.
    Zeroed,
    /// A torn write: only a prefix of the new contents landed on top of zeros.
    Torn(Index),
    /// A byte set, then the CRC recomputed: only the reserved-bytes check can catch it.
    SetByteFixCrc(Index, u8),
}

fn slot_damage() -> impl Strategy<Value = SlotDamage> {
    prop_oneof![
        3 => Just(SlotDamage::Keep),
        2 => (any::<Index>(), 0_u8..8).prop_map(|(i, b)| SlotDamage::FlipBit(i, b)),
        1 => (any::<Index>(), any::<u8>()).prop_map(|(i, v)| SlotDamage::SetByte(i, v)),
        1 => Just(SlotDamage::Zeroed),
        1 => any::<Index>().prop_map(SlotDamage::Torn),
        2 => (any::<Index>(), any::<u8>()).prop_map(|(i, v)| SlotDamage::SetByteFixCrc(i, v)),
    ]
}

fn damage(mut b: Vec<u8>, d: &SlotDamage) -> Vec<u8> {
    match d {
        SlotDamage::Keep => {}
        SlotDamage::FlipBit(i, bit) => b[i.index(SLOT_LEN)] ^= 1 << bit,
        SlotDamage::SetByte(i, v) => b[i.index(SLOT_LEN)] = *v,
        SlotDamage::Zeroed => b.fill(0),
        SlotDamage::Torn(i) => b[i.index(SLOT_LEN)..].fill(0),
        SlotDamage::SetByteFixCrc(i, v) => {
            b[i.index(SLOT_LEN - 4)] = *v;
            let crc = crc32c(&b[..SLOT_LEN - 4]);
            b[SLOT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
        }
    }
    b
}

proptest! {
    #[test]
    fn header_round_trips(h in valid_header(), base in 0_u64..1 << 40) {
        let bytes = encode_header(&h);
        prop_assert_eq!(bytes.len(), HEADER_LEN);
        let mut r = Reader::with_base_offset(&bytes, base);
        prop_assert_eq!(Header::decode(&mut r), Ok(h));
        prop_assert!(r.is_empty());
    }

    /// Every single-bit flip is caught, and reported as SPEC §4.2's check order says.
    #[test]
    fn header_bit_flip_is_always_rejected(h in valid_header(), at in 0..HEADER_LEN, bit in 0_u8..8) {
        let mut bytes = encode_header(&h);
        bytes[at] ^= 1 << bit;
        let result = Header::decode(&mut Reader::new(&bytes));
        match at {
            0..8 => prop_assert_eq!(result, Err(Error::Corrupt { offset: 0, reason: Corruption::BadMagic })),
            8..10 => {
                let unsupported = matches!(result, Err(Error::Unsupported { offset: 8, reason: Unsupported::FormatMajor { .. } }));
                prop_assert!(unsupported, "{:?}", result);
            }
            _ => {
                let mismatch = matches!(result, Err(Error::Corrupt { offset: 12, reason: Corruption::ChecksumMismatch { .. } }));
                prop_assert!(mismatch, "{:?}", result);
            }
        }
    }

    /// Arbitrary field bytes behind a valid magic, major and CRC agree with the reference validator: same
    /// header, or the same failing check.
    #[test]
    fn header_matches_reference(
        fields in prop::collection::vec(any::<u8>(), 44),
        reserved_at in prop::option::of((52..HEADER_LEN, 1_u8..)),
        known_major in any::<bool>(),
        fix_crc in any::<bool>(),
    ) {
        let mut bytes = vec![0; HEADER_LEN];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..52].copy_from_slice(&fields);
        if known_major {
            bytes[8..10].copy_from_slice(&FORMAT_MAJOR.to_le_bytes());
        }
        // Mostly-valid flags and page sizes, so later checks are reached too.
        if fields[0] & 1 == 0 {
            bytes[16..24].fill(0);
        }
        if fields[1] & 1 == 0 {
            bytes[48..52].copy_from_slice(&(4096_u32 << (fields[2] % 5)).to_le_bytes());
        }
        if let Some((at, v)) = reserved_at {
            bytes[at] = v;
        }
        if fix_crc {
            let crc = header_crc(&bytes);
            bytes[12..16].copy_from_slice(&crc.to_le_bytes());
        }
        let got = Header::decode(&mut Reader::new(&bytes));
        match reference_header(&bytes) {
            Ok(h) => prop_assert_eq!(got, Ok(h)),
            Err((offset, unsupported)) => {
                let err = got.unwrap_err();
                let actual = match err {
                    Error::Unsupported { offset, .. } => (offset, true),
                    _ => (err.corruption().unwrap().0, false),
                };
                prop_assert_eq!(actual, (offset, unsupported), "{:?}", err);
            }
        }
    }

    #[test]
    fn slot_round_trips(sb in superblock(any::<u64>()), base in 0_u64..1 << 40) {
        let bytes = encode_slot(&sb);
        prop_assert_eq!(bytes.len(), SLOT_LEN);
        prop_assert_eq!(reference_slot(&bytes), Some(sb));
        let mut r = Reader::with_base_offset(&bytes, base);
        prop_assert_eq!(Superblock::decode(&mut r), Ok(sb));
        prop_assert!(r.is_empty());
    }

    /// Slot selection under random damage to either slot agrees with the SPEC §4.3 table. Seqs are drawn from
    /// a small range so equal seqs (identical or conflicting) come up often.
    #[test]
    fn selection_matches_truth_table(
        a in superblock(0_u64..4),
        b in superblock(0_u64..4),
        same in any::<bool>(),
        da in slot_damage(),
        db in slot_damage(),
        base in 0_u64..1 << 40,
    ) {
        let b = if same { a } else { b };
        let slot_a = damage(encode_slot(&a), &da);
        let slot_b = damage(encode_slot(&b), &db);
        let expected = reference_select(reference_slot(&slot_a), reference_slot(&slot_b));

        let bytes = [slot_a, slot_b].concat();
        let mut r = Reader::with_base_offset(&bytes, base);
        let got = Superblock::read_active(&mut r);
        match expected {
            Ok(active) => {
                prop_assert_eq!(got, Ok(active));
                prop_assert!(r.is_empty());
            }
            Err(reason) => {
                let offset = match reason {
                    Corruption::SuperblockConflict { .. } => base + SLOT_LEN as u64,
                    _ => base,
                };
                prop_assert_eq!(got, Err(Error::Corrupt { offset, reason }));
                prop_assert_eq!(r.position(), 0);
            }
        }
    }

    /// The slots region shorter than `SLOTS_LEN` is never read as valid, even when slot A is intact.
    #[test]
    fn truncated_slots_are_rejected(a in superblock(any::<u64>()), len in 0..SLOTS_LEN) {
        let bytes = [encode_slot(&a), encode_slot(&a)].concat();
        let got = Superblock::read_active(&mut Reader::new(&bytes[..len]));
        let eof = matches!(got, Err(Error::Corrupt { offset: 0, reason: Corruption::UnexpectedEof { .. } }));
        prop_assert!(eof, "{:?}", got);
    }
}
