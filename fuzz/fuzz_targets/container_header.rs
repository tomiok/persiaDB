//! Fuzz the container header and superblock slots (ROADMAP 1.6.1.2, 1.6.3; SPEC §4.2, §4.3; ADR-0007): arbitrary
//! bytes never panic; the header fails only as corruption or unsupported, slots only as corruption; a failed
//! decode moves nothing; an accepted header or slot re-encodes to the same bytes; the active slot is the one the
//! SPEC §4.3 table picks from the two slots decoded on their own; `check_bounds` on it never panics and fails only as
//! `InvalidSuperblock`.
//!
//! Input: `mode u8, data..`, `data` laid out as a container prefix (header at 0, slots at 4096 and 8192). Mode
//! bits, applied in this order: 3 = write the magic and `format_major` 1; 0 = fix `header_crc`; 1 and 2 = fix the
//! CRC of slot A and B. Random bytes almost never carry a valid CRC, so without them field validation and
//! slot selection would only see the seed corpus.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::checksum::{Crc32c, crc32c};
use persia_format::header::{Access, FORMAT_MAJOR, HEADER_LEN, Header, MAGIC};
use persia_format::superblock::{ActiveSuperblock, SLOT_LEN, SLOTS_LEN, Slot, Superblock};
use persia_format::{Corruption, EncodeError, Error, Reader, Writer};

fn fix_slot_crc(slot: &mut [u8]) {
    if slot.len() == SLOT_LEN {
        let crc = crc32c(&slot[..SLOT_LEN - 4]);
        slot[SLOT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    }
}

fn check_header(data: &[u8]) {
    let mut r = Reader::new(data);
    match Header::decode(&mut r) {
        Ok(header) => {
            assert_eq!(r.position(), HEADER_LEN);
            let mut w = Writer::new();
            match header.access() {
                Access::ReadWrite => {
                    header.encode(&mut w).expect("accepted header re-encodes");
                    assert_eq!(
                        w.as_slice(),
                        &data[..HEADER_LEN],
                        "accepted header re-encodes differently"
                    );
                }
                Access::ReadOnly => {
                    assert_eq!(header.rocompat_flags.to_le_bytes(), data[24..32]);
                    assert!(matches!(
                        header.encode(&mut w),
                        Err(EncodeError::UnknownFlags { .. })
                    ));
                }
            }
        }
        Err(err) => {
            assert_eq!(r.position(), 0, "a failed decode must not move the cursor");
            assert!(
                err.is_corruption() || matches!(err, Error::Unsupported { .. }),
                "unexpected error class: {err:?}"
            );
        }
    }
}

fn check_slots(slots: &[u8]) {
    let mut r = Reader::new(slots);
    let result = Superblock::read_active(&mut r);
    let Some(both) = slots.get(..SLOTS_LEN) else {
        assert!(
            result.is_err_and(|e| e.is_corruption()),
            "short slots region accepted"
        );
        assert_eq!(r.position(), 0);
        return;
    };
    let decode = |s: &[u8]| Superblock::decode(&mut Reader::new(s)).ok();
    let (a, b) = (decode(&both[..SLOT_LEN]), decode(&both[SLOT_LEN..]));
    for (sb, bytes) in [(a, &both[..SLOT_LEN]), (b, &both[SLOT_LEN..])] {
        if let Some(sb) = sb {
            let mut w = Writer::new();
            sb.encode(&mut w).expect("slot re-encodes");
            assert_eq!(w.as_slice(), bytes, "accepted slot re-encodes differently");
        }
    }
    // A restatement of the SPEC §4.3 table over independently decoded slots; the property test
    // `selection_matches_truth_table` also checks slot validity with a reference decoder.
    let expected = match (a, b) {
        (Some(a), Some(b)) if a.seq == b.seq && a != b => None,
        (Some(a), Some(b)) if b.seq > a.seq => Some((Slot::B, b)),
        (Some(a), _) => Some((Slot::A, a)),
        (None, Some(b)) => Some((Slot::B, b)),
        (None, None) => None,
    };
    match result {
        Ok(active) => {
            assert_eq!(Some((active.slot, active.superblock)), expected);
            assert_eq!(r.position(), SLOTS_LEN);
            check_bounds(&active, slots);
        }
        Err(err) => {
            assert_eq!(expected, None, "valid selection rejected: {err:?}");
            assert!(err.is_corruption());
            assert_eq!(r.position(), 0);
        }
    }
}

fuzz_target!(|input: &[u8]| {
    let Some((&mode, rest)) = input.split_first() else {
        return;
    };
    let mut data = rest.to_vec();
    if mode & 8 != 0 && data.len() >= 10 {
        data[..8].copy_from_slice(&MAGIC);
        data[8..10].copy_from_slice(&FORMAT_MAJOR.to_le_bytes());
    }
    if mode & 1 != 0 && data.len() >= HEADER_LEN {
        let mut crc = Crc32c::new();
        crc.update(&data[..12]);
        crc.update(&data[16..HEADER_LEN]);
        data[12..16].copy_from_slice(&crc.finish().to_le_bytes());
    }
    if mode & 2 != 0 {
        fix_slot_crc(
            data.get_mut(HEADER_LEN..HEADER_LEN + SLOT_LEN)
                .unwrap_or_default(),
        );
    }
    if mode & 4 != 0 {
        fix_slot_crc(
            data.get_mut(HEADER_LEN + SLOT_LEN..HEADER_LEN + SLOTS_LEN)
                .unwrap_or_default(),
        );
    }
    check_header(&data);
    check_slots(data.get(HEADER_LEN..).unwrap_or_default());
});

/// `check_bounds` (SPEC §4.3) never panics and fails only as `InvalidSuperblock`; an accepted length is at least
/// `log_end`, and every longer one is accepted too (only `log_end <= len` depends on the length).
fn check_bounds(active: &ActiveSuperblock, slots: &[u8]) {
    let len_hint = slots
        .get(SLOTS_LEN..SLOTS_LEN + 8)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
    let sb = active.superblock;
    for len in [
        0,
        len_hint,
        sb.log_end,
        sb.log_end.saturating_sub(1),
        u64::MAX,
    ] {
        match active.check_bounds(len) {
            Ok(()) => {
                assert!(sb.log_end <= len && sb.wal_start <= sb.log_end);
                assert!(active.check_bounds(u64::MAX).is_ok());
            }
            Err(err) => assert!(
                matches!(
                    err.corruption(),
                    Some((_, Corruption::InvalidSuperblock { .. }))
                ),
                "{err:?}"
            ),
        }
    }
}
