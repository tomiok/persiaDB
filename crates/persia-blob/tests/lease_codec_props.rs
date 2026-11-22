//! The reader lease record's decoder against hostile and torn bytes (SPEC §9.3, ADR-0054 items 3 and 6; ROADMAP
//! 9.2.16). `lease_format.rs` pins the golden bytes and each check alone; this file pins how the checks combine.
//!
//! Invariants:
//! 1. The checks run in the ADR's order: at least 8 bytes, magic, version (0 corrupt, above 1 unsupported), exactly
//!    64 bytes, CRC32C, flags, reserved, the opening rule. For every combination of violations the first one in that
//!    order is the error reported, with its kind (`Corrupt`, `UnsupportedVersion`, `UnsupportedFlags`).
//! 2. Lengths 0..=8, 63, 65, 4096 and 4097 are each reported by the check that owns them: below 8 bytes as short
//!    whatever they hold, from 8 bytes on by the version (a later version may be longer) and then as not 64 bytes.
//! 3. Every single-bit flip and every single-byte change of a record is detected, and reported by the check of the
//!    field it hit: magic, version, or (anywhere else, the CRC itself included) the checksum.
//! 4. A change made with a valid CRC again (by construction) decodes only to the record the bytes encode: every
//!    decoded record is canonical (`encode(decode(b)) == b`), each field's change is reported by its own check, and a
//!    structured generator of sealed records decodes exactly when flags and reserved are zero and an all-zero
//!    `db_uuid` has seq 0.
//! 5. Torn rewrites: any byte-wise mix of two records the protocol writes in turn (the opening one, then open ones at
//!    rising seqs) decodes to one of the two, or fails; never to a third record (a stale or invented seq).
//!
//! Seeds: fixed (`RngSeed::Fixed` and a fixed xorshift seed), printed on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    missing_docs
)] // Tests: inputs are generated or fixed.

use persia_blob::Error;
use persia_blob::pack::lease::{LEASE_LEN, LEASE_READ_MAX, LeaseRecord};
use persia_format::checksum::crc32c;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

const ID: u128 = 0x0f1e_2d3c_4b5a_6978_8796_a5b4_c3d2_e1f0;
const UUID: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// What a decode reported: the check that refused it, or the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Short,
    Magic,
    Version0,
    Unsupported(u32),
    Length,
    Checksum,
    Flags(u32),
    Reserved,
    OpeningSeq,
    Decoded(LeaseRecord),
}

fn outcome(bytes: &[u8]) -> Outcome {
    match LeaseRecord::decode(bytes) {
        Ok(record) => Outcome::Decoded(record),
        Err(Error::Corrupt { encoding, what }) => {
            assert_eq!(encoding, "reader lease", "{what}");
            match what {
                "shorter than its magic and version" => Outcome::Short,
                "wrong magic" => Outcome::Magic,
                "version 0" => Outcome::Version0,
                "not 64 bytes long" => Outcome::Length,
                "checksum mismatch" => Outcome::Checksum,
                "reserved bytes are not zero" => Outcome::Reserved,
                "an opening record with a seq" => Outcome::OpeningSeq,
                other => panic!("an unknown corruption: {other}"),
            }
        }
        Err(Error::UnsupportedVersion { encoding, version }) => {
            assert_eq!(encoding, "reader lease");
            Outcome::Unsupported(version)
        }
        Err(Error::UnsupportedFlags { encoding, flags }) => {
            assert_eq!(encoding, "reader lease");
            Outcome::Flags(flags)
        }
        Err(other) => panic!("not a decode error: {other:?}"),
    }
}

fn seal(bytes: &mut [u8; LEASE_LEN]) {
    let crc = crc32c(&bytes[..60]);
    bytes[60..64].copy_from_slice(&crc.to_le_bytes());
}

/// The records every check is run against: both states, and the seq's extremes.
fn records() -> Vec<LeaseRecord> {
    vec![
        LeaseRecord::opening(ID, 4242),
        LeaseRecord::opening(0, 0),
        LeaseRecord::open(ID, 1, UUID, 0),
        LeaseRecord::open(ID, u32::MAX, UUID, u64::MAX),
        LeaseRecord::open(u128::MAX, 7, [0xFF; 16], 1 << 32),
    ]
}

// -- 1. the order of the checks --

/// One violation a test can put into a record, in the ADR's order of checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Violation {
    Magic,
    Version0,
    Version2,
    Length,
    Checksum,
    Flags,
    Reserved,
    OpeningSeq,
}

const ORDER: [Violation; 8] = [
    Violation::Magic,
    Violation::Version0,
    Violation::Version2,
    Violation::Length,
    Violation::Checksum,
    Violation::Flags,
    Violation::Reserved,
    Violation::OpeningSeq,
];

/// An opening record with every violation of `set` in it. The CRC is made valid again after the fields, unless the
/// checksum is one of the violations.
fn violating(set: &[Violation]) -> Vec<u8> {
    let mut bytes = LeaseRecord::opening(ID, 9).encode();
    for v in set {
        match v {
            Violation::Magic => bytes[0] ^= 0xFF,
            Violation::Version0 => bytes[4..8].copy_from_slice(&0_u32.to_le_bytes()),
            Violation::Version2 => bytes[4..8].copy_from_slice(&2_u32.to_le_bytes()),
            Violation::Flags => bytes[8..12].copy_from_slice(&0x8000_0001_u32.to_le_bytes()),
            Violation::Reserved => bytes[59] = 0x40,
            Violation::OpeningSeq => bytes[55] = 0x01,
            Violation::Length | Violation::Checksum => {}
        }
    }
    seal(&mut bytes);
    if set.contains(&Violation::Checksum) {
        bytes[60] ^= 0x01;
    }
    let mut out = bytes.to_vec();
    if set.contains(&Violation::Length) {
        out.push(0);
    }
    out
}

fn expected_first(set: &[Violation]) -> Outcome {
    let first = ORDER.iter().find(|v| set.contains(v));
    match first {
        None => Outcome::Decoded(LeaseRecord::opening(ID, 9)),
        Some(Violation::Magic) => Outcome::Magic,
        Some(Violation::Version0) => Outcome::Version0,
        Some(Violation::Version2) => Outcome::Unsupported(2),
        Some(Violation::Length) => Outcome::Length,
        Some(Violation::Checksum) => Outcome::Checksum,
        Some(Violation::Flags) => Outcome::Flags(0x8000_0001),
        Some(Violation::Reserved) => Outcome::Reserved,
        Some(Violation::OpeningSeq) => Outcome::OpeningSeq,
    }
}

#[test]
fn for_every_combination_of_violations_the_first_check_in_the_adrs_order_reports() {
    // Two version violations cannot coexist: each subset holds at most one of them.
    let others: Vec<Violation> = ORDER
        .iter()
        .copied()
        .filter(|v| !matches!(v, Violation::Version0 | Violation::Version2))
        .collect();
    let mut cases = 0;
    for version in [None, Some(Violation::Version0), Some(Violation::Version2)] {
        for mask in 0_u32..(1 << others.len()) {
            let mut set: Vec<Violation> = (0..others.len())
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| others[i])
                .collect();
            set.extend(version);
            let bytes = violating(&set);
            assert_eq!(outcome(&bytes), expected_first(&set), "violations {set:?}");
            cases += 1;
        }
    }
    assert_eq!(cases, 3 * 64);
}

// -- 2. lengths --

#[test]
fn each_length_is_reported_by_the_check_that_owns_it() {
    let lengths: Vec<usize> = (0..=8)
        .chain([63, 65, 4096, 4097, LEASE_READ_MAX as usize])
        .collect();
    let good = LeaseRecord::open(ID, 3, UUID, 77).encode();
    for &len in &lengths {
        // The valid record's prefix, padded with zeros past its end.
        let mut bytes = good.to_vec();
        bytes.resize(len, 0);
        let mut v2 = bytes.clone();
        let mut v0 = bytes.clone();
        let mut garbage = vec![0xA5; len];
        if len >= 8 {
            v2[4..8].copy_from_slice(&2_u32.to_le_bytes());
            v0[4..8].copy_from_slice(&0_u32.to_le_bytes());
            garbage[..4].copy_from_slice(b"PRLS");
        }
        let (version_1, version_2, version_0, unversioned) = if len < 8 {
            (
                Outcome::Short,
                Outcome::Short,
                Outcome::Short,
                Outcome::Short,
            )
        } else {
            (
                Outcome::Length,
                Outcome::Unsupported(2),
                Outcome::Version0,
                // "PRLS" then 0xA5A5A5A5 as the version: above 1.
                Outcome::Unsupported(0xA5A5_A5A5),
            )
        };
        assert_eq!(outcome(&bytes), version_1, "version 1, {len} bytes");
        assert_eq!(outcome(&v2), version_2, "version 2, {len} bytes");
        assert_eq!(outcome(&v0), version_0, "version 0, {len} bytes");
        assert_eq!(outcome(&garbage), unversioned, "garbage, {len} bytes");
        // Below 8 bytes nothing is read, so a wrong magic is still short; from 8 bytes on it is the magic.
        let mut magic = bytes.clone();
        if let Some(b) = magic.first_mut() {
            *b ^= 0xFF;
        }
        let wrong_magic = if len < 8 {
            Outcome::Short
        } else {
            Outcome::Magic
        };
        assert_eq!(outcome(&magic), wrong_magic, "wrong magic, {len} bytes");
    }
    assert_eq!(
        outcome(&good),
        Outcome::Decoded(LeaseRecord::decode(&good).unwrap())
    );
}

// -- 3. single-bit and single-byte changes --

/// The check a change confined to byte `at` of a 64-byte record must be reported by, the record's version byte being
/// `version` afterwards.
fn owner(at: usize, changed: &[u8; LEASE_LEN]) -> Outcome {
    match at {
        0..4 => Outcome::Magic,
        4..8 => match u32::from_le_bytes(changed[4..8].try_into().unwrap()) {
            0 => Outcome::Version0,
            v => {
                assert!(v > 1, "a changed version is never 1 again");
                Outcome::Unsupported(v)
            }
        },
        _ => Outcome::Checksum,
    }
}

#[test]
fn every_single_bit_flip_is_reported_by_the_check_of_its_field() {
    for record in records() {
        let good = record.encode();
        for at in 0..LEASE_LEN {
            for bit in 0..8 {
                let mut bytes = good;
                bytes[at] ^= 1 << bit;
                assert_eq!(
                    outcome(&bytes),
                    owner(at, &bytes),
                    "{record:?}: byte {at}, bit {bit}"
                );
            }
        }
    }
}

#[test]
fn every_single_byte_change_is_reported_by_the_check_of_its_field() {
    // CRC32C detects every burst of at most 32 bits: no change of one byte passes it.
    for record in records() {
        let good = record.encode();
        for at in 0..LEASE_LEN {
            for x in 1..=u8::MAX {
                let mut bytes = good;
                bytes[at] ^= x;
                assert_eq!(
                    outcome(&bytes),
                    owner(at, &bytes),
                    "{record:?}: byte {at} ^ {x:#04x}"
                );
            }
        }
    }
}

// -- 4. resealed changes: by construction --

/// What decoding `bytes` (a record with byte `at` changed, then sealed) must give, read field by field.
fn resealed_expectation(at: usize, bytes: &[u8; LEASE_LEN]) -> Outcome {
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    match at {
        0..4 => Outcome::Magic,
        4..8 => owner(at, bytes),
        8..12 => Outcome::Flags(u32_at(8)),
        56..60 => Outcome::Reserved,
        _ => {
            // pid, db_uuid, lease_id, manifest_seq: any value is a record, except an all-zero `db_uuid` with a seq.
            let uuid: [u8; 16] = bytes[16..32].try_into().unwrap();
            let id = u128::from_le_bytes(bytes[32..48].try_into().unwrap());
            let seq = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
            let pid = u32_at(12);
            if uuid == [0; 16] {
                if seq == 0 {
                    Outcome::Decoded(LeaseRecord::opening(id, pid))
                } else {
                    Outcome::OpeningSeq
                }
            } else {
                Outcome::Decoded(LeaseRecord::open(id, pid, uuid, seq))
            }
        }
    }
}

#[test]
fn a_resealed_change_decodes_only_to_the_record_its_bytes_encode() {
    for record in records() {
        let good = record.encode();
        for at in 0..60 {
            for x in 1..=u8::MAX {
                let mut bytes = good;
                bytes[at] ^= x;
                seal(&mut bytes);
                let got = outcome(&bytes);
                assert_eq!(
                    got,
                    resealed_expectation(at, &bytes),
                    "{record:?}: byte {at} ^ {x:#04x}"
                );
                if let Outcome::Decoded(decoded) = got {
                    assert_eq!(decoded.encode(), bytes, "canonical: byte {at} ^ {x:#04x}");
                    assert_ne!(decoded, record, "a changed byte is another record");
                }
            }
        }
    }
}

/// A sealed 64-byte record built field by field, valid or not.
#[derive(Debug, Clone)]
struct Raw {
    flags: u32,
    pid: u32,
    uuid: [u8; 16],
    id: u128,
    seq: u64,
    reserved: u32,
}

impl Raw {
    fn bytes(&self) -> [u8; LEASE_LEN] {
        let mut out = [0_u8; LEASE_LEN];
        out[0..4].copy_from_slice(b"PRLS");
        out[4..8].copy_from_slice(&1_u32.to_le_bytes());
        out[8..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..16].copy_from_slice(&self.pid.to_le_bytes());
        out[16..32].copy_from_slice(&self.uuid);
        out[32..48].copy_from_slice(&self.id.to_le_bytes());
        out[48..56].copy_from_slice(&self.seq.to_le_bytes());
        out[56..60].copy_from_slice(&self.reserved.to_le_bytes());
        seal(&mut out);
        out
    }
}

/// Mostly zero where zero is the rule, so every branch of the decoder is reached often.
fn mostly_zero_u32() -> impl Strategy<Value = u32> {
    prop_oneof![3 => Just(0_u32), 1 => any::<u32>(), 1 => (0..32_u32).prop_map(|b| 1 << b)]
}

fn raw() -> impl Strategy<Value = Raw> {
    (
        mostly_zero_u32(),
        any::<u32>(),
        prop_oneof![1 => Just([0_u8; 16]), 2 => any::<[u8; 16]>()],
        any::<u128>(),
        prop_oneof![1 => Just(0_u64), 2 => any::<u64>()],
        mostly_zero_u32(),
    )
        .prop_map(|(flags, pid, uuid, id, seq, reserved)| Raw {
            flags,
            pid,
            uuid,
            id,
            seq,
            reserved,
        })
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 20_000 } else { 1024 },
        rng_seed: RngSeed::Fixed(5410),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn a_sealed_record_decodes_exactly_when_its_fields_follow_the_rules_and_then_canonically(raw in raw()) {
        let bytes = raw.bytes();
        let opening = raw.uuid == [0; 16];
        let expected = if raw.flags != 0 {
            Outcome::Flags(raw.flags)
        } else if raw.reserved != 0 {
            Outcome::Reserved
        } else if opening && raw.seq != 0 {
            Outcome::OpeningSeq
        } else if opening {
            Outcome::Decoded(LeaseRecord::opening(raw.id, raw.pid))
        } else {
            Outcome::Decoded(LeaseRecord::open(raw.id, raw.pid, raw.uuid, raw.seq))
        };
        let got = outcome(&bytes);
        prop_assert_eq!(got, expected);
        if let Outcome::Decoded(r) = got {
            prop_assert_eq!(r.encode(), bytes);
            prop_assert_eq!(r.lease_id(), raw.id);
            prop_assert_eq!(r.pid(), raw.pid);
            prop_assert_eq!(r.is_opening(), opening);
            prop_assert_eq!(r.db_uuid(), (!opening).then_some(raw.uuid));
            prop_assert_eq!(r.manifest_seq(), (!opening).then_some(raw.seq));
        }
    }
}

// -- 5. torn rewrites --

/// A deterministic xorshift generator: the seed is printed by every assertion that uses it.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Every mix of `old` and `new` that takes each differing byte from one of them: all of them when there are at most
/// `exhaustive_bits` differing bytes, else `samples` drawn from `seed`. Each decodes to `old`, `new`, or fails.
fn check_mixes(old: LeaseRecord, new: LeaseRecord, exhaustive_bits: u32, samples: u64, seed: u64) {
    let (a, b) = (old.encode(), new.encode());
    let differing: Vec<usize> = (0..LEASE_LEN).filter(|&i| a[i] != b[i]).collect();
    let n = differing.len() as u32;
    let mut rng = XorShift(seed);
    let masks: Box<dyn Iterator<Item = u64>> = if n <= exhaustive_bits {
        Box::new(0..(1_u64 << n))
    } else {
        Box::new((0..samples).map(move |_| rng.next()))
    };
    let mut mixes = 0_u64;
    for mask in masks {
        let mut mix = a;
        for (bit, &i) in differing.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                mix[i] = b[i];
            }
        }
        match outcome(&mix) {
            Outcome::Decoded(r) => assert!(
                r == old || r == new,
                "seed {seed}: a torn mix of {old:?} and {new:?} decoded to {r:?} (mask {mask:#x})"
            ),
            Outcome::Checksum => {}
            other => panic!(
                "seed {seed}: a torn mix failed outside the checksum: {other:?} (mask {mask:#x})"
            ),
        }
        mixes += 1;
    }
    assert!(mixes > 0);
}

#[test]
fn a_torn_rewrite_reads_as_the_old_record_the_new_one_or_unreadable() {
    let (exhaustive, samples) = if cfg!(feature = "slow") {
        (22, 2_000_000)
    } else {
        (16, 40_000)
    };
    let pid = 31_337;
    let opening = LeaseRecord::opening(ID, pid);
    // The rewrites a handle makes: open after opening, then refreshes to later seqs (small steps, a carry across
    // bytes, a jump across the high bytes).
    let open = |seq| LeaseRecord::open(ID, pid, UUID, seq);
    let pairs = [
        (opening, open(0)),
        (opening, open(5)),
        (open(5), open(6)),
        (open(0xFF), open(0x100)),
        (open(0xFFFF_FFFF), open(0x1_0000_0000)),
        (open(1), open(u64::MAX)),
    ];
    for (k, (old, new)) in pairs.into_iter().enumerate() {
        let seed = 0x9E37_79B9_7F4A_7C15 ^ k as u64;
        check_mixes(old, new, exhaustive, samples, seed);
        check_mixes(new, old, exhaustive, samples, seed.rotate_left(17));
    }
}
