//! The reader lease record's bytes (SPEC §9.3, ADR-0054 item 3; ROADMAP 9.2.16).
//!
//! 1. Golden files: `tests/fixtures/blob-lease/v1/` holds an opening record and an open one, built here from the
//!    codec. They are never regenerated once released (SPEC §15); `PERSIA_WRITE_BLOB_LEASE_FIXTURES=1` writes a
//!    missing one.
//! 2. Each decode check, in its order: length for the magic and version, magic, version, exact length, CRC32C, flags,
//!    reserved, the opening rule.
//! 3. Round trips (proptest): every record built by `opening` or `open` decodes to itself and re-encodes to the same
//!    bytes; an open record at seq 0 stays open; a zero `db_uuid` is the opening state.
//! 4. Hostile input: every byte flip and every truncation is rejected, and arbitrary bytes never panic.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::path::PathBuf;

use persia_blob::Error;
use persia_blob::pack::lease::{LEASE_LEN, LeaseRecord};
use persia_format::checksum::crc32c;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

const ID: u128 = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210;
const UUID: [u8; 16] = [0x11; 16];

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/blob-lease/v1")
}

fn golden() -> [(&'static str, LeaseRecord); 2] {
    [
        ("opening.lease", LeaseRecord::opening(ID, 4242)),
        ("open.lease", LeaseRecord::open(ID, 4242, UUID, 42)),
    ]
}

#[test]
fn golden_fixtures_are_byte_for_byte_stable() {
    for (name, record) in golden() {
        let path = fixture_dir().join(name);
        let bytes = record.encode();
        if !path.exists() && std::env::var_os("PERSIA_WRITE_BLOB_LEASE_FIXTURES").is_some() {
            std::fs::create_dir_all(fixture_dir()).unwrap();
            std::fs::write(&path, bytes).unwrap();
        }
        let stored = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (PERSIA_WRITE_BLOB_LEASE_FIXTURES=1 writes it)",
                path.display()
            )
        });
        assert_eq!(stored, bytes, "{name} differs from the encoder");
        assert_eq!(LeaseRecord::decode(&stored).unwrap(), record, "{name}");
    }
}

#[test]
fn the_layout_is_the_specs() {
    let bytes = LeaseRecord::open(ID, 4242, UUID, 42).encode();
    assert_eq!(&bytes[0..4], b"PRLS");
    assert_eq!(&bytes[4..8], &1_u32.to_le_bytes());
    assert_eq!(&bytes[8..12], &[0; 4]);
    assert_eq!(&bytes[12..16], &4242_u32.to_le_bytes());
    assert_eq!(&bytes[16..32], &UUID);
    assert_eq!(&bytes[32..48], &ID.to_le_bytes());
    assert_eq!(&bytes[48..56], &42_u64.to_le_bytes());
    assert_eq!(&bytes[56..60], &[0; 4]);
    assert_eq!(&bytes[60..64], &crc32c(&bytes[..60]).to_le_bytes());
    let opening = LeaseRecord::opening(ID, 4242).encode();
    assert_eq!(&opening[16..32], &[0; 16]);
    assert_eq!(&opening[48..56], &[0; 8]);
}

/// `bytes` with a valid CRC32C again, so a later check is reached.
fn resealed(mut bytes: [u8; LEASE_LEN]) -> [u8; LEASE_LEN] {
    let crc = crc32c(&bytes[..60]);
    bytes[60..64].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn corrupt_what(bytes: &[u8]) -> &'static str {
    match LeaseRecord::decode(bytes) {
        Err(Error::Corrupt { what, .. }) => what,
        other => panic!("not corruption: {other:?}"),
    }
}

#[test]
fn each_check_in_its_order() {
    let good = LeaseRecord::open(ID, 7, UUID, 9).encode();
    assert!(corrupt_what(&good[..7]).contains("magic and version"));
    let mut magic = good;
    magic[0] ^= 1;
    assert!(corrupt_what(&magic).contains("magic"));
    // The version comes before the length and the CRC: a later version may be longer.
    let mut v0 = good;
    v0[4..8].copy_from_slice(&0_u32.to_le_bytes());
    assert!(corrupt_what(&v0).contains("version 0"));
    let mut v2 = good.to_vec();
    v2[4..8].copy_from_slice(&2_u32.to_le_bytes());
    v2.extend([0; 16]);
    assert!(matches!(
        LeaseRecord::decode(&v2),
        Err(Error::UnsupportedVersion { version: 2, .. })
    ));
    assert!(corrupt_what(&good[..63]).contains("64 bytes"));
    let mut long = good.to_vec();
    long.push(0);
    assert!(corrupt_what(&long).contains("64 bytes"));
    let mut crc = good;
    crc[40] ^= 1;
    assert!(corrupt_what(&crc).contains("checksum"));
    let mut flags = good;
    flags[8] = 1;
    assert!(matches!(
        LeaseRecord::decode(&resealed(flags)),
        Err(Error::UnsupportedFlags { flags: 1, .. })
    ));
    let mut reserved = good;
    reserved[57] = 1;
    assert!(corrupt_what(&resealed(reserved)).contains("reserved"));
    // A zero `db_uuid` is the opening state, whose seq is 0: one encoding per state.
    let mut opening_with_seq = LeaseRecord::opening(ID, 7).encode();
    opening_with_seq[48] = 1;
    assert!(corrupt_what(&resealed(opening_with_seq)).contains("opening record with a seq"));
}

#[test]
fn the_states_have_one_encoding_each() {
    let opening = LeaseRecord::opening(ID, 1);
    assert!(opening.is_opening());
    assert_eq!(opening.manifest_seq(), None);
    assert_eq!(opening.db_uuid(), None);
    // An open record at seq 0 (a database that never committed) is open, not opening.
    let at_zero = LeaseRecord::open(ID, 1, UUID, 0);
    assert!(!at_zero.is_opening());
    assert_eq!(at_zero.manifest_seq(), Some(0));
    assert_eq!(LeaseRecord::decode(&at_zero.encode()).unwrap(), at_zero);
    // A database whose `db_uuid` is zero cannot say it is open: its handles stay opening (they hold GC back).
    assert_eq!(LeaseRecord::open(ID, 1, [0; 16], 5), opening);
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 2048 } else { 256 },
        rng_seed: RngSeed::Fixed(9216),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn every_record_round_trips(
        id in any::<u128>(),
        pid in any::<u32>(),
        uuid in any::<[u8; 16]>(),
        seq in any::<u64>(),
        opening in any::<bool>(),
    ) {
        let record = if opening { LeaseRecord::opening(id, pid) } else { LeaseRecord::open(id, pid, uuid, seq) };
        let bytes = record.encode();
        let decoded = LeaseRecord::decode(&bytes).unwrap();
        prop_assert_eq!(decoded, record);
        prop_assert_eq!(decoded.encode(), bytes);
        prop_assert_eq!(decoded.lease_id(), id);
        prop_assert_eq!(decoded.pid(), pid);
    }

    #[test]
    fn every_byte_flip_is_rejected(
        id in any::<u128>(),
        uuid in any::<[u8; 16]>(),
        seq in any::<u64>(),
        at in 0..LEASE_LEN,
        bit in 0..8_u32,
    ) {
        let mut bytes = LeaseRecord::open(id, 3, uuid, seq).encode();
        bytes[at] ^= 1 << bit;
        prop_assert!(LeaseRecord::decode(&bytes).is_err(), "flip at {} bit {}", at, bit);
    }

    #[test]
    fn arbitrary_bytes_never_panic_and_a_decoded_record_is_its_own_encoding(
        bytes in prop::collection::vec(any::<u8>(), 0..200),
    ) {
        if let Ok(record) = LeaseRecord::decode(&bytes) {
            prop_assert_eq!(&record.encode()[..], &bytes[..]);
        }
    }
}

#[test]
fn every_truncation_is_rejected() {
    let bytes = LeaseRecord::open(ID, 3, UUID, 77).encode();
    for len in 0..LEASE_LEN {
        assert!(LeaseRecord::decode(&bytes[..len]).is_err(), "{len}");
    }
}
