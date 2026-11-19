//! The blob pack's bytes (SPEC §9.3, ADR-0049; ROADMAP 9.2.1).
//!
//! 1. Golden files: `tests/fixtures/blob-pack/v1/` holds a new pack and a pack with one committed blob, built here
//!    from the layout functions. They are never regenerated once released (SPEC §15);
//!    `PERSIA_WRITE_BLOB_PACK_FIXTURES=1` writes a missing one.
//! 2. Round trips and hostile input of the slot, `BlobChunk` and `BlobIndex` codecs (proptest, deterministic).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::path::PathBuf;

use persia_blob::pack::layout::{
    self, ChunkHead, FIRST_FRAME, PackSuperblock, decode_chunk_payload, decode_frame,
    decode_index_payload, decode_prefix, encode_chunk_frame, encode_index_frame, new_pack_prefix,
};
use persia_blob::{BlobManifest, ChunkSize, UploadId, blob_id_of};
use persia_format::checksum::crc32c;
use persia_format::frame::FrameKind;
use persia_format::superblock::Slot;
use proptest::prelude::*;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/blob-pack/v1")
}

fn manifest_of(data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
    let crcs = data.chunks(chunk as usize).map(crc32c).collect();
    BlobManifest::new(
        blob_id_of(data),
        data.len() as u64,
        ChunkSize::from_encoding(chunk).unwrap(),
        UploadId::new(upload),
        crcs,
    )
    .unwrap()
}

const UUID: [u8; 16] = [0x11; 16];
const PACK_ID: u128 = 0x5050_4b31_0000_0000_0000_0000_0000_0001;
const GOLDEN_BLOB: &[u8] = b"persia blobs!";

/// A pack that never bound a blob.
fn golden_new() -> Vec<u8> {
    new_pack_prefix(UUID, PACK_ID).unwrap()
}

/// A pack with one committed blob, "persia blobs!" in three chunks of 5 under upload `u128::MAX`, bound by the
/// commit in slot B.
fn golden_one_blob() -> Vec<u8> {
    let mut file = new_pack_prefix(UUID, PACK_ID).unwrap();
    let m = manifest_of(GOLDEN_BLOB, 5, u128::MAX);
    let mut offsets = Vec::new();
    for (i, chunk) in GOLDEN_BLOB.chunks(5).enumerate() {
        offsets.push(file.len() as u64);
        let seq = i as u64 + 1;
        file.extend(encode_chunk_frame(seq, m.upload_id(), i as u32, chunk).unwrap());
    }
    let index_off = file.len() as u64;
    file.extend(encode_index_frame(4, &m, &offsets).unwrap());
    let sb = PackSuperblock {
        seq: 1,
        index_off,
        index_len: file.len() as u64 - index_off,
        log_end: file.len() as u64,
        generation: 1,
        blob_count: 1,
        next_frame_seq: 5,
        pack_id: PACK_ID,
    };
    file[8192..12288].copy_from_slice(&sb.encode());
    file
}

#[test]
fn golden_fixtures_are_byte_for_byte_stable() {
    let write = std::env::var_os("PERSIA_WRITE_BLOB_PACK_FIXTURES").is_some();
    for (name, bytes) in [
        ("new.pack", golden_new()),
        ("one-blob.pack", golden_one_blob()),
        ("rewritten-slot.bin", golden_rewritten_slot()),
    ] {
        let path = fixture_dir().join(name);
        if write && !path.exists() {
            std::fs::create_dir_all(fixture_dir()).unwrap();
            std::fs::write(&path, &bytes).unwrap();
        }
        let stored = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (PERSIA_WRITE_BLOB_PACK_FIXTURES=1 writes it)",
                path.display()
            )
        });
        assert_eq!(stored, bytes, "{name}: the encoding changed");
    }
}

#[test]
fn golden_one_blob_decodes() {
    let file = std::fs::read(fixture_dir().join("one-blob.pack")).unwrap();
    let p = decode_prefix(&file).unwrap();
    assert_eq!(&file[..8], b"PERSIAB\x01");
    assert_eq!((p.slot, p.header.db_uuid), (Slot::B, UUID));
    let sb = p.superblock;
    sb.check_bounds(file.len() as u64).unwrap();
    assert_eq!(
        (sb.seq, sb.generation, sb.blob_count, sb.next_frame_seq),
        (1, 1, 1, 5)
    );
    let start = sb.index_off as usize;
    let index = &file[start..start + sb.index_len as usize];
    let (seq, payload) = decode_frame(index, sb.index_off, FrameKind::BlobIndex).unwrap();
    assert_eq!(seq, 4);
    let (m, offsets) = decode_index_payload(payload, sb.index_off).unwrap();
    assert_eq!(m, manifest_of(GOLDEN_BLOB, 5, u128::MAX));
    let mut blob = Vec::new();
    for (i, &o) in offsets.iter().enumerate() {
        // Each chunk frame is 24 + 40 (32 + 5 or 3, padded) + 8 = 72 bytes.
        let frame = &file[o as usize..o as usize + 72];
        let (_, payload) = decode_frame(frame, o, FrameKind::BlobChunk).unwrap();
        let (head, bytes) = decode_chunk_payload(payload).unwrap();
        assert_eq!((head.upload_id, head.index), (m.upload_id(), i as u32));
        assert_eq!(head.crc, m.chunk_crcs()[i]);
        blob.extend_from_slice(bytes);
    }
    assert_eq!(blob, GOLDEN_BLOB);
}

#[test]
fn golden_one_blob_layout() {
    let file = golden_one_blob();
    assert_eq!(FIRST_FRAME, 12288);
    // Slot B at 8192: seq, index_off, index_len, log_end, generation, blob_count, next_frame_seq.
    let field =
        |i: usize| u64::from_le_bytes(file[8192 + 8 * i..8192 + 8 * i + 8].try_into().unwrap());
    assert_eq!(field(1), 12288 + 3 * 72);
    assert_eq!(field(3), file.len() as u64);
    assert_eq!(field(4), 1);
    // The first chunk frame: kind 6, then the chunk head.
    let f = &file[12288..];
    assert_eq!(&f[..4], b"PFR1");
    assert_eq!(f[4], 6);
    assert_eq!(&f[24..40], &u128::MAX.to_le_bytes());
    assert_eq!(&f[40..44], &0_u32.to_le_bytes());
    assert_eq!(&f[44..48], &5_u32.to_le_bytes());
    assert_eq!(&f[48..52], &crc32c(b"persi").to_le_bytes());
    assert_eq!(&f[52..56], &[0; 4]);
    assert_eq!(&f[56..61], b"persi");
    // The index frame: kind 7, manifest_len 88, the manifest, then three offsets.
    let i = &file[12288 + 3 * 72..];
    assert_eq!(i[4], 7);
    assert_eq!(&i[24..28], &88_u32.to_le_bytes());
    assert_eq!(&i[32..36], b"PBLM");
    assert_eq!(&i[120..128], &12288_u64.to_le_bytes());
    assert_eq!(&i[136..144], &(12288_u64 + 144).to_le_bytes());
}

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: if cfg!(feature = "slow") { 2048 } else { 128 },
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x9_2_1),
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn chunk_frames_round_trip(
        bytes in proptest::collection::vec(any::<u8>(), 1..2000),
        upload in any::<u128>(),
        index in any::<u32>(),
        seq in 1..u64::MAX,
    ) {
        let frame = encode_chunk_frame(seq, UploadId::new(upload), index, &bytes).unwrap();
        let (got_seq, payload) = decode_frame(&frame, FIRST_FRAME, FrameKind::BlobChunk).unwrap();
        prop_assert_eq!(got_seq, seq);
        let (head, got) = decode_chunk_payload(payload).unwrap();
        prop_assert_eq!(head, ChunkHead {
            upload_id: UploadId::new(upload),
            index,
            len: bytes.len() as u32,
            crc: crc32c(&bytes),
        });
        prop_assert_eq!(got, &bytes[..]);
    }

    #[test]
    fn index_frames_round_trip(
        data in proptest::collection::vec(any::<u8>(), 0..300),
        chunk in 1..64_u32,
        upload in any::<u128>(),
        seed in any::<u64>(),
    ) {
        let m = manifest_of(&data, chunk, upload);
        // Distinct offsets (the decoder rejects two chunks in one frame), in a seeded order: a permutation of
        // 0..n by a stride coprime with n.
        let n = u64::from(m.chunk_count());
        let stride = (1..=n.max(1)).rev().find(|s| gcd(*s, n.max(1)) == 1 && *s != 0).unwrap_or(1);
        let base = seed % 1000;
        let offsets: Vec<u64> = (0..n)
            .map(|i| FIRST_FRAME + 8 * (base * 1000 + (i * stride + seed) % n))
            .collect();
        let at = FIRST_FRAME + 8 * 1_000_000;
        let frame = encode_index_frame(seq_of(seed), &m, &offsets).unwrap();
        let (_, payload) = decode_frame(&frame, at, FrameKind::BlobIndex).unwrap();
        prop_assert_eq!(decode_index_payload(payload, at).unwrap(), (m, offsets));
    }

    /// Hostile bytes: the decoders never panic, and reject what cannot be a valid encoding: random bytes are not a
    /// frame (the CRC), a header overwritten by them fails its CRC, and a payload shorter than its head is refused. What
    /// they do accept re-encodes to the same bytes.
    #[test]
    fn hostile_bytes_are_rejected_or_round_trip(
        bytes in proptest::collection::vec(any::<u8>(), 0..400),
        at in any::<u64>(),
    ) {
        match decode_chunk_payload(&bytes) {
            Ok((head, chunk)) => {
                prop_assert!(bytes.len() > 32);
                prop_assert_eq!(u64::from(head.len), chunk.len() as u64);
            }
            Err(e) => prop_assert!(e.is_corruption()),
        }
        if bytes.len() < 32 {
            prop_assert!(decode_chunk_payload(&bytes).is_err());
        }
        if let Ok((m, offsets)) = decode_index_payload(&bytes, at) {
            let reencoded = encode_index_frame(1, &m, &offsets).unwrap();
            prop_assert_eq!(&reencoded[24..24 + bytes.len()], &bytes[..]);
        }
        // A random frame passes its CRC with probability 2^-32: treat a pass as a finding.
        prop_assert!(decode_frame(&bytes, at, FrameKind::BlobChunk).is_err());
        let mut prefix = new_pack_prefix(UUID, PACK_ID).unwrap();
        let end = bytes.len().min(prefix.len());
        prefix[..end].copy_from_slice(&bytes[..end]);
        if end > 0 && prefix[..end] != new_pack_prefix(UUID, PACK_ID).unwrap()[..end] {
            prop_assert!(decode_prefix(&prefix).is_err(), "a changed header passes its CRC");
        }
    }

    #[test]
    fn a_flipped_bit_in_a_frame_is_rejected(
        bytes in proptest::collection::vec(any::<u8>(), 1..200),
        bit in any::<u32>(),
    ) {
        let mut frame = encode_chunk_frame(3, UploadId::new(1), 0, &bytes).unwrap();
        let bit = bit as usize % (frame.len() * 8);
        frame[bit / 8] ^= 1 << (bit % 8);
        prop_assert!(decode_frame(&frame, FIRST_FRAME, FrameKind::BlobChunk)
            .and_then(|(_, p)| decode_chunk_payload(p).map(|_| ()))
            .is_err());
    }
}

fn seq_of(seed: u64) -> u64 {
    seed.clamp(1, u64::MAX - 1)
}

#[test]
fn layout_constants() {
    assert_eq!(layout::CHUNK_HEAD_LEN, 32);
    assert_eq!(layout::INDEX_HEAD_LEN, 8);
    assert_eq!(layout::FIRST_GENERATION, 1);
}

/// A committed slot of a rewritten pack (generation 3), every field distinct.
fn golden_rewritten_slot() -> Vec<u8> {
    PackSuperblock {
        seq: 9,
        index_off: 12288 + 4096,
        index_len: 120,
        log_end: 12288 + 4216,
        generation: 3,
        blob_count: 7,
        next_frame_seq: 40,
        pack_id: PACK_ID,
    }
    .encode()
}

#[test]
fn golden_rewritten_slot_decodes() {
    let bytes = std::fs::read(fixture_dir().join("rewritten-slot.bin")).unwrap();
    let mut prefix = golden_new();
    prefix[8192..12288].copy_from_slice(&bytes);
    let p = decode_prefix(&prefix).unwrap();
    assert_eq!(p.slot, Slot::B);
    assert_eq!(
        (p.superblock.generation, p.superblock.pack_id),
        (3, PACK_ID)
    );
    p.superblock.check_bounds(12288 + 4216).unwrap();
}

/// Slot fields from the edges of every rule.
fn slot() -> impl Strategy<Value = PackSuperblock> {
    let off = prop_oneof![
        3 => (0..64_u64).prop_map(|i| 12288 + 8 * i),
        1 => Just(0_u64),
        1 => Just(u64::MAX - 7),
        1 => any::<u64>(),
    ];
    let small = prop_oneof![3 => 0..4_u64, 1 => Just(u64::MAX), 1 => any::<u64>()];
    (
        small.clone(),
        off.clone(),
        prop_oneof![2 => (0..40_u64).prop_map(|i| 8 * i), 1 => any::<u64>()],
        off,
        small.clone(),
        small.clone(),
        small,
        any::<u128>(),
    )
        .prop_map(
            |(
                seq,
                index_off,
                index_len,
                log_end,
                generation,
                blob_count,
                next_frame_seq,
                pack_id,
            )| {
                PackSuperblock {
                    seq,
                    index_off,
                    index_len,
                    log_end,
                    generation,
                    blob_count,
                    next_frame_seq,
                    pack_id,
                }
            },
        )
}

proptest! {
    #![proptest_config(config())]

    /// A CRC-valid slot with any fields: it is picked by seq alone, and `check_bounds` passes only the states a
    /// writer produces.
    #[test]
    fn any_resealed_slot_is_checked_before_use(sb in slot(), file_len in 12288..20000_u64) {
        let mut prefix = golden_new();
        prefix[8192..12288].copy_from_slice(&sb.encode());
        match decode_prefix(&prefix) {
            // Slot A is the new pack's seq 0: equal seq with other contents is corruption (SPEC §4.3).
            Err(e) => prop_assert!(e.is_corruption() && sb.seq == 0),
            Ok(p) => prop_assert_eq!(p.slot, if sb.seq > 0 { Slot::B } else { Slot::A }),
        }
        if sb.check_bounds(file_len).is_ok() {
            prop_assert!(sb.generation >= 1 && sb.log_end <= file_len && sb.log_end % 8 == 0);
            if sb.seq == 0 {
                prop_assert_eq!((sb.log_end, sb.blob_count, sb.index_off, sb.next_frame_seq), (12288, 0, 0, 1));
            } else {
                prop_assert!(sb.blob_count >= 1);
                prop_assert!(sb.blob_count <= (sb.log_end - 12288) / layout::MIN_INDEX_FRAME_LEN);
                prop_assert_eq!(sb.index_off + sb.index_len, sb.log_end);
                prop_assert!(sb.next_frame_seq >= 2);
            }
        }
    }

    /// A prefix cut anywhere is corruption, never a panic.
    #[test]
    fn a_short_prefix_is_corrupt(len in 0..12288_usize) {
        let prefix = golden_one_blob();
        prop_assert!(decode_prefix(&prefix[..len]).unwrap_err().is_corruption());
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}
