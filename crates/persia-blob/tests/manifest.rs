//! The blob manifest encoding (ROADMAP 9.1.3; SPEC §9.1.1, ADR-0035).
//!
//! Invariants:
//! 1. Golden files: the manifests below encode to exactly the bytes in `tests/fixtures/blob-manifest/v1/` and decode
//!    back to themselves. They are never regenerated once released (SPEC §15); `PERSIA_WRITE_BLOB_MANIFEST_FIXTURES=1`
//!    writes the missing ones.
//! 2. Round trip: any valid manifest decodes to itself, its encoding has the length `encoded_len_for` predicts
//!    (also for chunk sizes up to 64 MiB, blobs past 4 GiB, any blob id and upload ids at the edges), and the
//!    accessors agree with the chunking (SPEC §9.2). `encoded_len_for` is `76 + 4·n`, and refuses `n > u32::MAX`.
//! 3. Corruption: any truncation or extension of a valid encoding, re-sealed or not, is `Corrupt`, never a panic.
//! 4. `decode` never panics and fails only with `Corrupt`, `UnsupportedVersion` (version > 1) or `UnsupportedFlags`
//!    (a flag bit this build does not know): every single bit flip of a valid encoding is `Corrupt`; every header
//!    field rewritten and re-sealed (magic, version, flags, blob id, size, chunk size, upload id, CRC list) is
//!    rejected with the kind SPEC §9.1.1 gives it, or decodes when the rules allow it; any byte rewritten (re-sealed
//!    or not) either fails that way or decodes to a manifest that re-encodes to exactly those bytes (one encoding
//!    per value). A reference verdict written from SPEC §9.1.1 (`spec_verdict`) predicts the exact outcome.
//! 5. Validation order (SPEC §9.1.1, ADR-0035 §5): length, magic, trailer, version, flags, chunk size, chunk count,
//!    CRC list length, empty-blob id; when two rules break, the earlier one is reported.
//! 6. `BlobManifest::new` refuses a CRC list of the wrong length and an empty blob whose id is not SHA-256("") with
//!    `InvalidManifest`, and a blob of more than `u32::MAX` chunks with `InvalidChunking(TooManyChunks)`.
//! 7. A header claiming up to `u32::MAX` chunks over a tiny body fails as `Corrupt`, quickly: the length is checked
//!    before the CRC list is read. Unsafe is denied, so there is no allocator hook: this asserts the outcome.
//! 8. `Error::is_corruption` is true for `Corrupt` only.
//! 9. A manifest checks against what it was looked up by (SPEC §9.4): another `blob_id`, or a `BlobRef` with another
//!    id, size or chunk size, is `Corrupt`.
//!
//! The order tests tell `Corrupt` rules apart by their `what` text (`rule_of`): that is the only place the rule
//! shows. A reworded message fails `rule_of` loudly; update its table, not the assertion.
//!
//! Seeds: proptest's default config; `PROPTEST_RNG_SEED` and `PROPTEST_CASES` override it.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::path::PathBuf;

use persia_blob::{BlobId, BlobManifest, ChunkSize, Error, UploadId, blob_id_of, chunk_all};
use persia_format::blob::BlobRefDefect;
use persia_format::checksum::crc32c;
use proptest::prelude::*;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/blob-manifest/v1")
}

/// The manifest the chunker gives for `data`.
fn manifest_of(data: &[u8], chunk_size: u32, upload_id: u128) -> BlobManifest {
    let mut crcs = Vec::new();
    let blob = chunk_all(data, ChunkSize::from_encoding(chunk_size).unwrap(), |c| {
        crcs.push(c.crc32c());
        Ok(())
    })
    .unwrap();
    BlobManifest::new(
        blob.blob_id,
        blob.size,
        blob.chunk_size,
        UploadId::new(upload_id),
        crcs,
    )
    .unwrap()
}

/// The empty blob: no chunks.
fn golden_empty() -> BlobManifest {
    manifest_of(b"", 4 << 20, 0x0123_4567_89ab_cdef_0011_2233_4455_6677)
}

/// Three chunks of a 13-byte blob, the last one short.
fn golden_three() -> BlobManifest {
    manifest_of(b"persia blobs!", 5, u128::MAX)
}

/// One default-size (4 MiB) chunk and a short second one.
fn golden_default() -> BlobManifest {
    let data: Vec<u8> = (0..(4_u32 << 20) + 1000).map(|i| (i % 251) as u8).collect();
    manifest_of(&data, 4 << 20, 1)
}

#[test]
fn golden_fixtures_decode_and_reencode_byte_for_byte() {
    let write = std::env::var_os("PERSIA_WRITE_BLOB_MANIFEST_FIXTURES").is_some();
    for (name, manifest) in [
        ("empty.blm", golden_empty()),
        ("three.blm", golden_three()),
        ("default.blm", golden_default()),
    ] {
        let path = fixture_dir().join(name);
        let encoded = manifest.encode();
        if write && !path.exists() {
            std::fs::create_dir_all(fixture_dir()).unwrap();
            std::fs::write(&path, &encoded).unwrap();
        }
        let stored = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (PERSIA_WRITE_BLOB_MANIFEST_FIXTURES=1 writes it)",
                path.display()
            )
        });
        assert_eq!(BlobManifest::decode(&stored).unwrap(), manifest, "{name}");
        assert_eq!(stored, encoded, "{name}: the encoding changed");
    }
}

#[test]
fn golden_three_layout() {
    let m = golden_three();
    let bytes = m.encode();
    assert_eq!(bytes.len(), 76 + 3 * 4);
    assert_eq!(&bytes[..4], b"PBLM");
    assert_eq!(&bytes[4..8], &1_u32.to_le_bytes());
    assert_eq!(&bytes[8..12], &[0; 4]);
    assert_eq!(&bytes[12..44], blob_id_of(b"persia blobs!").as_bytes());
    assert_eq!(&bytes[44..52], &13_u64.to_le_bytes());
    assert_eq!(&bytes[52..56], &5_u32.to_le_bytes());
    assert_eq!(&bytes[56..72], &u128::MAX.to_le_bytes());
    let crc = crc32c;
    assert_eq!(&bytes[72..76], &crc(b"persi").to_le_bytes());
    assert_eq!(&bytes[76..80], &crc(b"a blo").to_le_bytes());
    assert_eq!(&bytes[80..84], &crc(b"bs!").to_le_bytes());
    assert_eq!(&bytes[84..], &crc(&bytes[..84]).to_le_bytes());
    assert_eq!(m.chunk_range(2), Some(10..13));
    assert_eq!(
        m.upload_id().to_string(),
        "ffffffffffffffffffffffffffffffff"
    );
}

proptest! {
    #[test]
    fn round_trips(
        data in proptest::collection::vec(any::<u8>(), 0..512),
        chunk in 1_u32..=64,
        upload in any::<u128>(),
    ) {
        let m = manifest_of(&data, chunk, upload);
        let bytes = m.encode();
        prop_assert_eq!(bytes.len(), m.encoded_len());
        prop_assert_eq!(
            bytes.len(),
            BlobManifest::encoded_len_for(data.len() as u64, ChunkSize::from_encoding(chunk).unwrap()).unwrap()
        );
        prop_assert_eq!(BlobManifest::decode(&bytes).unwrap(), m);
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_dir().join(name)).unwrap()
}

const GOLDEN_FILES: [&str; 3] = ["empty.blm", "three.blm", "default.blm"];

/// SHA-256 of no bytes, written out so the reference below does not lean on `blob_id_of`.
const SHA256_EMPTY: [u8; 32] = [
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
    0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

/// Any valid manifest: chunk sizes from 1 byte to 64 MiB, 0..=80 chunks (so blobs past 4 GiB: 80 × 64 MiB is 5 GiB), a last chunk of any
/// length, any blob id (the codec cannot check it against bytes it does not hold) and upload ids at the edges.
fn manifest() -> impl Strategy<Value = BlobManifest> {
    let chunk = prop_oneof![
        1..=64_u32,
        Just(ChunkSize::DEFAULT.get()),
        Just(ChunkSize::MAX - 1),
        Just(ChunkSize::MAX),
        1..=ChunkSize::MAX,
    ];
    let upload = prop_oneof![
        Just(0_u128),
        Just(1),
        Just(1 << 127),
        Just(u128::MAX),
        any::<u128>(),
    ];
    (
        chunk,
        0..=80_u32,
        any::<prop::sample::Index>(),
        any::<[u8; 32]>(),
        upload,
        prop::collection::vec(any::<u32>(), 80),
    )
        .prop_map(|(chunk, n, last, id, upload, crcs)| {
            let size = if n == 0 {
                0
            } else {
                let last = u64::try_from(last.index(usize::try_from(chunk).unwrap())).unwrap();
                u64::from(n - 1) * u64::from(chunk) + 1 + last
            };
            let id = if size == 0 {
                blob_id_of(&[])
            } else {
                BlobId::from_bytes(id)
            };
            let crcs = crcs[..usize::try_from(n).unwrap()].to_vec();
            BlobManifest::new(
                id,
                size,
                ChunkSize::from_encoding(chunk).unwrap(),
                UploadId::new(upload),
                crcs,
            )
            .unwrap()
        })
}

// Hand-built encodings: every field written as given (no validation), so a test can break exactly one rule and
// re-seal the CRC (SPEC §9.1.1).

/// The fields of a blob manifest, as stored.
#[derive(Clone, Debug)]
struct Head {
    magic: [u8; 4],
    version: u32,
    flags: u32,
    blob_id: [u8; 32],
    size: u64,
    chunk_size: u32,
    upload_id: u128,
    crcs: Vec<u32>,
}

impl Head {
    fn of(m: &BlobManifest) -> Self {
        Self {
            magic: *b"PBLM",
            version: 1,
            flags: 0,
            blob_id: *m.blob_id().as_bytes(),
            size: m.size(),
            chunk_size: m.chunk_size().get(),
            upload_id: m.upload_id().get(),
            crcs: m.chunk_crcs().to_vec(),
        }
    }

    /// The fields and a valid CRC32C trailer.
    fn seal(&self) -> Vec<u8> {
        let mut b = self.magic.to_vec();
        b.extend(self.version.to_le_bytes());
        b.extend(self.flags.to_le_bytes());
        b.extend(self.blob_id);
        b.extend(self.size.to_le_bytes());
        b.extend(self.chunk_size.to_le_bytes());
        b.extend(self.upload_id.to_le_bytes());
        for crc in &self.crcs {
            b.extend(crc.to_le_bytes());
        }
        let crc = crc32c(&b);
        b.extend(crc.to_le_bytes());
        b
    }
}

/// 13 bytes in chunks of 5 (`golden_three`): the base the header tests break.
fn three() -> Head {
    Head::of(&golden_three())
}

/// The empty blob (`golden_empty`).
fn empty() -> Head {
    Head::of(&golden_empty())
}

/// Recomputes the trailer of `bytes` after a change to the bytes it covers (inputs under 4 bytes have none).
fn reseal(mut bytes: Vec<u8>) -> Vec<u8> {
    if let Some(end) = bytes.len().checked_sub(4) {
        let crc = crc32c(&bytes[..end]);
        bytes[end..].copy_from_slice(&crc.to_le_bytes());
    }
    bytes
}

/// The rule of SPEC §9.1.1 a `Corrupt` error reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Short,
    Magic,
    Checksum,
    Version0,
    ChunkSize,
    TooManyChunks,
    CrcCount,
    EmptyId,
}

/// A decode outcome, by kind (and rule, for `Corrupt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Corrupt(Rule),
    Version(u32),
    Flags(u32),
    Valid,
}

/// The `what` texts of `BlobManifest::decode`, one per rule.
fn rule_of(what: &str) -> Rule {
    match what {
        "shorter than a header and a trailer" => Rule::Short,
        "not a blob manifest (bad magic)" => Rule::Magic,
        "checksum mismatch" => Rule::Checksum,
        "encoding version 0" => Rule::Version0,
        "chunk_size outside 1..=64 MiB" => Rule::ChunkSize,
        "more than u32::MAX chunks" => Rule::TooManyChunks,
        "the chunk CRCs do not match the chunk count" => Rule::CrcCount,
        "an empty blob whose id is not the SHA-256 of no bytes" => Rule::EmptyId,
        other => panic!("unknown Corrupt text {other:?}: add it to rule_of"),
    }
}

/// The verdict of a `decode` result; a build-side error from `decode` is a bug.
fn verdict_of(r: &Result<BlobManifest, Error>) -> Verdict {
    match r {
        Ok(_) => Verdict::Valid,
        Err(Error::Corrupt { what, .. }) => Verdict::Corrupt(rule_of(what)),
        Err(Error::UnsupportedVersion { version, .. }) => Verdict::Version(*version),
        Err(Error::UnsupportedFlags { flags, .. }) => Verdict::Flags(*flags),
        Err(e) => panic!("decode returned a non-decode error: {e:?}"),
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// SPEC §9.1.1's reading rules, in its order, on raw bytes: the reference `decode` is checked against.
fn spec_verdict(b: &[u8]) -> Verdict {
    if b.len() < 76 {
        return Verdict::Corrupt(Rule::Short);
    }
    if b[..4] != *b"PBLM" {
        return Verdict::Corrupt(Rule::Magic);
    }
    let end = b.len() - 4;
    if le32(b, end) != crc32c(&b[..end]) {
        return Verdict::Corrupt(Rule::Checksum);
    }
    match le32(b, 4) {
        0 => return Verdict::Corrupt(Rule::Version0),
        1 => {}
        v => return Verdict::Version(v),
    }
    let flags = le32(b, 8);
    if flags != 0 {
        return Verdict::Flags(flags);
    }
    let size = le64(b, 44);
    let chunk = le32(b, 52);
    if chunk == 0 || chunk > 64 << 20 {
        return Verdict::Corrupt(Rule::ChunkSize);
    }
    let n = size.div_ceil(u64::from(chunk));
    if n > u64::from(u32::MAX) {
        return Verdict::Corrupt(Rule::TooManyChunks);
    }
    if u64::try_from(end - 72).unwrap() != 4 * n {
        return Verdict::Corrupt(Rule::CrcCount);
    }
    if size == 0 && b[12..44] != SHA256_EMPTY {
        return Verdict::Corrupt(Rule::EmptyId);
    }
    Verdict::Valid
}

/// Decodes `bytes`: the outcome is `spec_verdict`'s, errors are corruption exactly when `Corrupt`, and an accepted
/// manifest holds the bytes' fields and re-encodes to exactly `bytes` (one encoding per value).
fn check_decode(bytes: &[u8]) -> Result<Verdict, String> {
    let result = BlobManifest::decode(bytes);
    let got = verdict_of(&result);
    let want = spec_verdict(bytes);
    if got != want {
        return Err(format!(
            "decode gave {got:?}, SPEC §9.1.1 says {want:?} ({} bytes)",
            bytes.len()
        ));
    }
    match result {
        Err(e) => {
            if e.is_corruption() != matches!(e, Error::Corrupt { .. }) {
                return Err(format!("is_corruption wrong for {e:?}"));
            }
        }
        Ok(m) => {
            let fields_match = m.blob_id().as_bytes()[..] == bytes[12..44]
                && m.size() == le64(bytes, 44)
                && m.chunk_size().get() == le32(bytes, 52)
                && m.upload_id().get() == u128::from_le_bytes(bytes[56..72].try_into().unwrap())
                && m.chunk_crcs().len() == usize::try_from(m.chunk_count()).unwrap()
                && m.chunk_crcs()
                    .iter()
                    .enumerate()
                    .all(|(i, &crc)| crc == le32(bytes, 72 + 4 * i));
            if !fields_match {
                return Err(format!("decoded fields differ from the bytes: {m:?}"));
            }
            if m.encode() != bytes || m.encoded_len() != bytes.len() {
                return Err(format!("{m:?} does not re-encode to its input"));
            }
            if BlobManifest::encoded_len_for(m.size(), m.chunk_size()).unwrap() != bytes.len() {
                return Err(format!("encoded_len_for disagrees for {m:?}"));
            }
        }
    }
    Ok(want)
}

/// `check_decode` outside a proptest; also returns the verdict.
#[track_caller]
fn decode_verdict(bytes: &[u8], case: &str) -> Verdict {
    check_decode(bytes).unwrap_or_else(|e| panic!("{case}: {e}"))
}

/// `h` sealed decodes to `want`, as SPEC §9.1.1 says.
#[track_caller]
fn expect(h: &Head, want: Verdict, case: &str) {
    assert_eq!(decode_verdict(&h.seal(), case), want, "{case}");
}

#[test]
fn the_hand_built_encoder_and_the_reference_match_the_writer() {
    // Guards the helpers above: they reproduce the writer's bytes, and the reference accepts every golden file.
    assert_eq!(three().seal(), golden_three().encode());
    assert_eq!(empty().seal(), golden_empty().encode());
    assert_eq!(*blob_id_of(&[]).as_bytes(), SHA256_EMPTY);
    for name in GOLDEN_FILES {
        assert_eq!(decode_verdict(&fixture(name), name), Verdict::Valid);
    }
    assert_eq!(
        BlobManifest::decode(&fixture("default.blm"))
            .unwrap()
            .chunk_count(),
        2
    );
}

#[test]
fn encoded_len_for_is_76_plus_4_per_chunk_up_to_u32_max_chunks() {
    let cs = |c| ChunkSize::from_encoding(c).unwrap();
    let max = cs(ChunkSize::MAX);
    for (size, chunk, n) in [
        (0, cs(1), 0_u64),
        (0, max, 0),
        (1, cs(1), 1),
        (1, max, 1),
        (13, cs(5), 3),
        (15, cs(5), 3),
        (16, cs(5), 4),
        (u64::from(ChunkSize::MAX), max, 1),
        (u64::from(ChunkSize::MAX) + 1, max, 2),
        (u64::from(u32::MAX), cs(1), u64::from(u32::MAX)),
        (max.max_blob_size(), max, u64::from(u32::MAX)),
    ] {
        let want = usize::try_from(76 + 4 * n).unwrap();
        assert_eq!(
            BlobManifest::encoded_len_for(size, chunk).unwrap(),
            want,
            "size {size}, chunk {}",
            chunk.get()
        );
    }
    for (size, chunk) in [
        (u64::from(u32::MAX) + 1, cs(1)),
        (u64::MAX, cs(1)),
        (max.max_blob_size() + 1, max),
        (u64::MAX, max),
    ] {
        match BlobManifest::encoded_len_for(size, chunk) {
            Err(Error::InvalidChunking(BlobRefDefect::TooManyChunks {
                size: s,
                chunk_size: c,
            })) => assert_eq!((s, c), (size, chunk.get())),
            other => panic!("size {size}, chunk {}: {other:?}", chunk.get()),
        }
    }
}

#[test]
fn every_truncation_and_extension_of_the_goldens_is_corruption() {
    for name in GOLDEN_FILES {
        let b = fixture(name);
        for cut in 0..b.len() {
            let short = &b[..cut];
            let want = if cut < 76 {
                Rule::Short
            } else {
                Rule::Checksum
            };
            assert_eq!(
                decode_verdict(short, name),
                Verdict::Corrupt(want),
                "{name}: cut at {cut}"
            );
            // Re-sealed, a cut past the header is a CRC list of the wrong length.
            let want = if cut < 76 {
                Rule::Short
            } else {
                Rule::CrcCount
            };
            assert_eq!(
                decode_verdict(&reseal(short.to_vec()), name),
                Verdict::Corrupt(want),
                "{name}: cut at {cut}, re-sealed"
            );
        }
        for extra in 1..=16 {
            for fill in [0x00, 0xFF, 0x5A] {
                let mut long = b.clone();
                long.extend(std::iter::repeat_n(fill, extra));
                assert!(
                    matches!(decode_verdict(&long, name), Verdict::Corrupt(_)),
                    "{name}: {extra} bytes of {fill:#x} appended"
                );
                assert_eq!(
                    decode_verdict(&reseal(long), name),
                    Verdict::Corrupt(Rule::CrcCount),
                    "{name}: {extra} bytes of {fill:#x} appended, re-sealed"
                );
            }
            // The trailer repeated: the last 4 bytes are a CRC, just not of what they follow.
            let mut long = b.clone();
            long.extend_from_slice(&b[b.len() - 4..]);
            assert!(matches!(decode_verdict(&long, name), Verdict::Corrupt(_)));
        }
    }
}

#[test]
fn every_bit_flip_of_the_goldens_is_corruption() {
    // CRC32C detects every single-bit error; a flip in the magic is reported as the magic (checked first).
    for name in GOLDEN_FILES {
        let mut b = fixture(name);
        for i in 0..b.len() {
            for bit in 0..8 {
                b[i] ^= 1 << bit;
                let want = if i < 4 { Rule::Magic } else { Rule::Checksum };
                assert_eq!(
                    decode_verdict(&b, name),
                    Verdict::Corrupt(want),
                    "{name}: bit {bit} of byte {i}"
                );
                b[i] ^= 1 << bit;
            }
        }
    }
}

#[test]
fn every_single_byte_rewrite_of_the_goldens_matches_the_spec() {
    // Exhaustive: every byte, every other value, as it is and re-sealed.
    for name in GOLDEN_FILES {
        let good = fixture(name);
        let mut accepted = 0;
        for i in 0..good.len() {
            for v in (0..=255_u8).filter(|&v| v != good[i]) {
                let mut b = good.clone();
                b[i] = v;
                let case = format!("{name}: byte {i} = {v:#x}");
                assert!(matches!(
                    decode_verdict(&b, &case),
                    Verdict::Corrupt(Rule::Magic | Rule::Checksum)
                ));
                if i < good.len() - 4
                    && decode_verdict(&reseal(b), &format!("{case}, re-sealed")) == Verdict::Valid
                {
                    accepted += 1;
                }
            }
        }
        // Free fields (blob id unless empty, upload id, chunk CRCs) re-sealed are accepted: the test reaches `Ok`.
        assert!(accepted >= 16 * 255, "{name}: only {accepted} accepted");
    }
}

#[test]
fn every_header_field_rewritten_and_resealed_is_rejected() {
    let corrupt = |h: &Head, rule, case: &str| expect(h, Verdict::Corrupt(rule), case);
    expect(&three(), Verdict::Valid, "base");
    expect(&empty(), Verdict::Valid, "empty base");

    // Magic: every byte, every other value, re-sealed.
    for i in 0..4 {
        for v in (0..=255_u8).filter(|&v| v != b"PBLM"[i]) {
            let mut h = three();
            h.magic[i] = v;
            corrupt(&h, Rule::Magic, &format!("magic byte {i} = {v:#x}"));
        }
    }

    // Version: 0 is corruption, above 1 unsupported.
    corrupt(
        &Head {
            version: 0,
            ..three()
        },
        Rule::Version0,
        "version 0",
    );
    for version in [2, 3, 0x100, 1 << 31, u32::MAX] {
        expect(
            &Head { version, ..three() },
            Verdict::Version(version),
            &format!("version {version}"),
        );
    }

    // Flags: every bit is unknown in v1, and so is any combination.
    for flags in (0..32).map(|bit| 1_u32 << bit).chain([3, u32::MAX]) {
        expect(
            &Head { flags, ..three() },
            Verdict::Flags(flags),
            &format!("flags {flags:#x}"),
        );
        expect(
            &Head { flags, ..empty() },
            Verdict::Flags(flags),
            &format!("empty, flags {flags:#x}"),
        );
    }

    // chunk_size: out of range; in range but another chunk count than the CRC list (13 bytes: 5 and 6 give 3).
    for chunk_size in [0, ChunkSize::MAX + 1, u32::MAX] {
        corrupt(
            &Head {
                chunk_size,
                ..three()
            },
            Rule::ChunkSize,
            &format!("chunk_size {chunk_size}"),
        );
        corrupt(
            &Head {
                chunk_size,
                ..empty()
            },
            Rule::ChunkSize,
            &format!("empty, chunk_size {chunk_size}"),
        );
    }
    for chunk_size in [1, 2, 4, 7, 12, 13, 14, ChunkSize::MAX] {
        corrupt(
            &Head {
                chunk_size,
                ..three()
            },
            Rule::CrcCount,
            &format!("chunk_size {chunk_size}"),
        );
    }
    for chunk_size in [5, 6] {
        let m = BlobManifest::decode(
            &Head {
                chunk_size,
                ..three()
            }
            .seal(),
        )
        .unwrap();
        assert_eq!(m.chunk_size().get(), chunk_size);
    }
    for chunk_size in [1, ChunkSize::MAX] {
        expect(
            &Head {
                chunk_size,
                ..empty()
            },
            Verdict::Valid,
            &format!("empty, chunk_size {chunk_size}"),
        );
    }

    // size: a count above u32::MAX; another count than the CRC list; 11..=15 bytes are 3 chunks of 5.
    let max_count = u64::from(u32::MAX);
    for (size, chunk_size) in [
        (max_count * 5 + 1, 5),
        (u64::MAX, 5),
        (max_count + 1, 1),
        (u64::MAX, 1),
        (max_count * u64::from(ChunkSize::MAX) + 1, ChunkSize::MAX),
        (u64::MAX, ChunkSize::MAX),
    ] {
        for h in [three(), empty()] {
            corrupt(
                &Head {
                    size,
                    chunk_size,
                    ..h
                },
                Rule::TooManyChunks,
                &format!("size {size}, chunk_size {chunk_size}"),
            );
        }
    }
    for size in 0..=25_u64 {
        let want = if (11..=15).contains(&size) {
            Verdict::Valid
        } else {
            Verdict::Corrupt(Rule::CrcCount)
        };
        expect(&Head { size, ..three() }, want, &format!("size {size}"));
    }
    // The empty base is in 4 MiB chunks: the last size is u32::MAX of them.
    for size in [
        1,
        5,
        u64::from(u32::MAX),
        max_count * u64::from(ChunkSize::DEFAULT.get()),
    ] {
        corrupt(
            &Head { size, ..empty() },
            Rule::CrcCount,
            &format!("empty, size {size}"),
        );
    }

    // The CRC list: one CRC too few or too many, or bytes that are not whole CRCs.
    for n in [0, 1, 2, 4, 5, 64] {
        let crcs = (0..n).collect();
        corrupt(
            &Head { crcs, ..three() },
            Rule::CrcCount,
            &format!("{n} CRCs"),
        );
        let crcs = (0..n).collect();
        if n > 0 {
            corrupt(
                &Head { crcs, ..empty() },
                Rule::CrcCount,
                &format!("empty, {n} CRCs"),
            );
        }
    }
    let good = three().seal();
    for extra in 1..=3 {
        let mut b = good[..good.len() - 4].to_vec();
        b.extend(std::iter::repeat_n(0, extra + 4));
        assert_eq!(
            decode_verdict(&reseal(b), "partial CRC"),
            Verdict::Corrupt(Rule::CrcCount),
            "{extra} stray bytes"
        );
        let mut b = good[..good.len() - 4 - extra].to_vec();
        b.extend([0; 4]);
        assert_eq!(
            decode_verdict(&reseal(b), "partial CRC"),
            Verdict::Corrupt(Rule::CrcCount),
            "{extra} bytes short"
        );
    }

    // The empty blob: only the SHA-256 of no bytes.
    let mut one_bit_off = SHA256_EMPTY;
    one_bit_off[31] ^= 1;
    for blob_id in [
        [0; 32],
        [0xFF; 32],
        *blob_id_of(b"x").as_bytes(),
        one_bit_off,
    ] {
        corrupt(
            &Head { blob_id, ..empty() },
            Rule::EmptyId,
            "empty blob, another id",
        );
    }

    // Free fields: any blob id of a non-empty blob, any upload id, any chunk CRCs.
    for blob_id in [[0; 32], [0xFF; 32], SHA256_EMPTY] {
        expect(&Head { blob_id, ..three() }, Verdict::Valid, "blob id");
    }
    for upload_id in [0, 1, 1 << 127, u128::MAX] {
        let h = Head {
            upload_id,
            ..three()
        };
        expect(&h, Verdict::Valid, "upload id");
        let m = BlobManifest::decode(&h.seal()).unwrap();
        assert_eq!(m.upload_id(), UploadId::new(upload_id));
        assert_eq!(m.upload_id().to_string(), format!("{upload_id:032x}"));
    }
    for crcs in [vec![0; 3], vec![u32::MAX; 3], vec![1, 2, 3]] {
        let h = Head {
            crcs: crcs.clone(),
            ..three()
        };
        assert_eq!(BlobManifest::decode(&h.seal()).unwrap().chunk_crcs(), crcs);
    }
}

#[test]
fn rules_are_checked_in_spec_order() {
    // SPEC §9.1.1: length, magic, trailer, version, flags, chunk size, chunk count, CRC list length, empty-blob id.
    let unsealed = |h: &Head| {
        let mut b = h.seal();
        let end = b.len() - 1;
        b[end] ^= 0x80;
        b
    };
    let v = |b: &[u8], case: &str| decode_verdict(b, case);

    // Length before magic and trailer.
    assert_eq!(v(&[], "empty input"), Verdict::Corrupt(Rule::Short));
    assert_eq!(
        v(&[b'X'; 75], "75 bytes of junk"),
        Verdict::Corrupt(Rule::Short)
    );
    assert_eq!(
        v(&reseal(three().seal()[..75].to_vec()), "75 bytes, sealed"),
        Verdict::Corrupt(Rule::Short)
    );

    // Magic before trailer, version and everything after.
    let mut bad = three().seal();
    bad[0] = b'X';
    assert_eq!(v(&bad, "magic + CRC"), Verdict::Corrupt(Rule::Magic));
    let h = Head {
        magic: *b"PBLX",
        version: 2,
        flags: 1,
        chunk_size: 0,
        ..three()
    };
    assert_eq!(v(&h.seal(), "magic + all"), Verdict::Corrupt(Rule::Magic));

    // Trailer before version and flags: a damaged version or flags reads as corruption.
    for h in [
        Head {
            version: 2,
            ..three()
        },
        Head {
            version: 0,
            ..three()
        },
        Head {
            flags: 1,
            ..three()
        },
        Head {
            chunk_size: 0,
            ..three()
        },
        Head {
            crcs: vec![],
            ..three()
        },
    ] {
        assert_eq!(
            v(&unsealed(&h), "CRC + field"),
            Verdict::Corrupt(Rule::Checksum),
            "{h:?}"
        );
    }

    // Version before flags and the rest.
    let h = Head {
        version: 2,
        flags: 1,
        chunk_size: 0,
        crcs: vec![],
        ..three()
    };
    assert_eq!(v(&h.seal(), "version 2 + all"), Verdict::Version(2));
    let h = Head {
        version: 0,
        flags: 1,
        ..three()
    };
    assert_eq!(
        v(&h.seal(), "version 0 + flags"),
        Verdict::Corrupt(Rule::Version0)
    );

    // Flags before every layout rule.
    for h in [
        Head {
            flags: 1,
            crcs: vec![],
            ..three()
        },
        Head {
            flags: 1,
            chunk_size: 0,
            ..three()
        },
        Head {
            flags: 1,
            size: u64::MAX,
            chunk_size: 1,
            ..three()
        },
        Head {
            flags: 1,
            blob_id: [0; 32],
            ..empty()
        },
    ] {
        assert_eq!(v(&h.seal(), "flags + rule"), Verdict::Flags(1), "{h:?}");
    }

    // Chunk size before the count, the count before the CRC list, the list before the empty-blob id.
    let h = Head {
        chunk_size: 0,
        size: u64::MAX,
        crcs: vec![1],
        blob_id: [0; 32],
        ..empty()
    };
    assert_eq!(
        v(&h.seal(), "chunk 0 + rest"),
        Verdict::Corrupt(Rule::ChunkSize)
    );
    let h = Head {
        chunk_size: 1,
        size: u64::MAX,
        crcs: vec![1],
        ..three()
    };
    assert_eq!(
        v(&h.seal(), "count + list"),
        Verdict::Corrupt(Rule::TooManyChunks)
    );
    let h = Head {
        crcs: vec![1],
        blob_id: [0; 32],
        ..empty()
    };
    assert_eq!(
        v(&h.seal(), "list + empty id"),
        Verdict::Corrupt(Rule::CrcCount)
    );
}

#[test]
fn new_rejects_inconsistent_manifests() {
    let id = blob_id_of(b"persia blobs!");
    let five = ChunkSize::from_encoding(5).unwrap();
    let up = UploadId::new(9);
    let invalid = |r: Result<BlobManifest, Error>, case: &str| match r {
        Err(e @ Error::InvalidManifest { .. }) => assert!(!e.is_corruption(), "{case}"),
        other => panic!("{case}: expected InvalidManifest, got {other:?}"),
    };

    // One CRC per chunk: 13 bytes in chunks of 5 are 3 chunks.
    for n in [0, 1, 2, 4, 100] {
        invalid(
            BlobManifest::new(id, 13, five, up, vec![0; n]),
            &format!("{n} CRCs for 3 chunks"),
        );
    }
    invalid(
        BlobManifest::new(blob_id_of(&[]), 0, five, up, vec![0]),
        "a CRC for an empty blob",
    );
    // The empty blob: only the SHA-256 of no bytes, also when the CRC list is wrong too.
    for bad in [BlobId::from_bytes([0; 32]), id, blob_id_of(b"x")] {
        invalid(BlobManifest::new(bad, 0, five, up, vec![]), "empty blob id");
        invalid(
            BlobManifest::new(bad, 0, five, up, vec![7]),
            "empty id + CRC",
        );
    }

    // More than u32::MAX chunks, whatever the CRC list.
    let max = ChunkSize::from_encoding(ChunkSize::MAX).unwrap();
    for (size, chunk) in [
        (
            u64::from(u32::MAX) + 1,
            ChunkSize::from_encoding(1).unwrap(),
        ),
        (u64::MAX, ChunkSize::from_encoding(1).unwrap()),
        (max.max_blob_size() + 1, max),
        (u64::MAX, max),
    ] {
        for crcs in [vec![], vec![0; 3]] {
            match BlobManifest::new(id, size, chunk, up, crcs) {
                Err(
                    e @ Error::InvalidChunking(BlobRefDefect::TooManyChunks {
                        size: s,
                        chunk_size: c,
                    }),
                ) => {
                    assert_eq!((s, c), (size, chunk.get()));
                    assert!(!e.is_corruption());
                }
                other => panic!("size {size}: expected TooManyChunks, got {other:?}"),
            }
        }
    }

    // What `new` accepts, `decode` reads back.
    for m in [
        BlobManifest::new(id, 13, five, up, vec![1, 2, 3]).unwrap(),
        BlobManifest::new(BlobId::from_bytes([0; 32]), 1, max, up, vec![0]).unwrap(),
        BlobManifest::new(blob_id_of(&[]), 0, max, UploadId::new(u128::MAX), vec![]).unwrap(),
    ] {
        assert_eq!(BlobManifest::decode(&m.encode()).unwrap(), m);
    }
}

#[test]
fn a_header_claiming_u32_max_chunks_over_a_tiny_body_fails_fast() {
    // Every pair gives exactly u32::MAX chunks: a 16 GiB CRC list, of which at most a few KiB are present. The
    // length is checked before the list is read, so each case fails on it at once.
    let max = ChunkSize::from_encoding(ChunkSize::MAX).unwrap();
    let max_count = u64::from(u32::MAX);
    for (size, chunk_size) in [
        (max_count, 1),
        (max_count * 5, 5),
        (
            max_count * u64::from(ChunkSize::DEFAULT.get()),
            ChunkSize::DEFAULT.get(),
        ),
        (max.max_blob_size(), ChunkSize::MAX),
    ] {
        assert_eq!(
            ChunkSize::from_encoding(chunk_size)
                .unwrap()
                .chunk_count(size)
                .unwrap(),
            u32::MAX
        );
        for n in [0, 1, 3, 1024] {
            let h = Head {
                size,
                chunk_size,
                crcs: vec![0xAB; n],
                ..three()
            };
            expect(
                &h,
                Verdict::Corrupt(Rule::CrcCount),
                &format!("size {size}, chunk {chunk_size}, {n} CRCs"),
            );
        }
        for stray in [1, 2, 3, 4097] {
            let mut b = Head {
                size,
                chunk_size,
                crcs: vec![],
                ..three()
            }
            .seal();
            b.splice(72..72, std::iter::repeat_n(0xFF, stray));
            assert_eq!(
                decode_verdict(&reseal(b), "stray bytes"),
                Verdict::Corrupt(Rule::CrcCount),
                "size {size}, {stray} stray bytes"
            );
        }
    }
}

#[test]
fn is_corruption_is_true_for_corrupt_only() {
    let cases = [
        (
            Error::Corrupt {
                encoding: "blob manifest",
                what: "x",
            },
            true,
        ),
        (
            Error::UnsupportedVersion {
                encoding: "blob manifest",
                version: 2,
            },
            false,
        ),
        (
            Error::UnsupportedFlags {
                encoding: "blob manifest",
                flags: 1,
            },
            false,
        ),
        (Error::InvalidManifest { what: "x" }, false),
        (
            Error::InvalidChunking(BlobRefDefect::ChunkSize { chunk_size: 0 }),
            false,
        ),
        (
            Error::InvalidChunking(BlobRefDefect::TooManyChunks {
                size: u64::MAX,
                chunk_size: 1,
            }),
            false,
        ),
        (Error::Read(std::io::Error::other("x")), false),
        (Error::ChunkerFailed, false),
        (
            Error::ChunkSizeBelowMin {
                chunk_size: 1,
                min: ChunkSize::MIN,
            },
            false,
        ),
    ];
    for (e, corrupt) in &cases {
        assert_eq!(e.is_corruption(), *corrupt, "{e:?}");
        // No wildcard: a new variant must be added to the table above.
        match e {
            Error::Corrupt { .. }
            | Error::UnsupportedVersion { .. }
            | Error::UnsupportedFlags { .. }
            | Error::InvalidManifest { .. }
            | Error::InvalidChunking(_)
            | Error::Read(_)
            | Error::ChunkerFailed
            | Error::ChunkSizeBelowMin { .. } => {}
        }
    }
}

/// Header fields from the edges of every rule, the CRC list sized to match more often than not, and a trailer that
/// is valid nine times in ten.
fn head() -> impl Strategy<Value = (Head, bool)> {
    let magic = prop_oneof![8 => Just(*b"PBLM"), 1 => any::<[u8; 4]>()];
    let version = prop_oneof![6 => Just(1_u32), 1 => Just(0), 1 => Just(2), 1 => Just(u32::MAX), 1 => any::<u32>()];
    let flags =
        prop_oneof![6 => Just(0_u32), 1 => (0..32_u32).prop_map(|b| 1 << b), 1 => any::<u32>()];
    let chunk = prop_oneof![
        6 => 1..=8_u32,
        1 => Just(0),
        1 => Just(ChunkSize::MAX),
        1 => Just(ChunkSize::MAX + 1),
        1 => Just(u32::MAX),
        1 => any::<u32>(),
    ];
    let size = prop_oneof![
        6 => 0..=40_u64,
        1 => Just(u64::from(u32::MAX)),
        1 => Just(u64::from(u32::MAX) + 1),
        1 => Just(u64::MAX),
        1 => any::<u64>(),
    ];
    let id = prop_oneof![Just(SHA256_EMPTY), any::<[u8; 32]>()];
    (
        (magic, version, flags, id),
        (size, chunk, any::<u128>()),
        (0..=3_u8, 0..12_usize, any::<u32>()),
        prop::bool::weighted(0.9),
    )
        .prop_map(
            |(
                (magic, version, flags, blob_id),
                (size, chunk_size, upload_id),
                (fit, n, seed),
                sealed,
            )| {
                // Usually one CRC per chunk when the count is small, otherwise any number.
                let count = size.div_ceil(u64::from(chunk_size.max(1)));
                let n = match usize::try_from(count) {
                    Ok(c) if fit > 0 && c <= 64 => c,
                    _ => n,
                };
                let crcs = (0..n).map(|i| seed.wrapping_add(i as u32)).collect();
                let h = Head {
                    magic,
                    version,
                    flags,
                    blob_id,
                    size,
                    chunk_size,
                    upload_id,
                    crcs,
                };
                (h, sealed)
            },
        )
}

/// Bytes after a CRC-breaking change: the trailer's top bit flipped.
fn unseal(mut b: Vec<u8>) -> Vec<u8> {
    let end = b.len() - 1;
    b[end] ^= 0x80;
    b
}

proptest! {
    #[test]
    fn round_trips_across_chunk_sizes_and_ids(m in manifest()) {
        let bytes = m.encode();
        prop_assert_eq!(bytes.len(), m.encoded_len());
        prop_assert_eq!(bytes.len(), BlobManifest::encoded_len_for(m.size(), m.chunk_size()).unwrap());
        prop_assert_eq!(bytes.len(), 76 + 4 * m.chunk_crcs().len());
        let back = BlobManifest::decode(&bytes).unwrap();
        prop_assert_eq!(&back, &m);
        prop_assert_eq!(back.encode(), bytes.clone());
        prop_assert_eq!(Head::of(&m).seal(), bytes);
        // The accessors agree with the chunking: contiguous ranges that cover the blob, none past the last.
        let n = m.chunk_count();
        prop_assert_eq!(u64::from(n), m.size().div_ceil(u64::from(m.chunk_size().get())));
        if n > 0 {
            prop_assert_eq!(m.chunk_range(0).unwrap().start, 0);
            prop_assert_eq!(m.chunk_range(n - 1).unwrap().end, m.size());
            prop_assert_eq!(m.chunk_range(n - 1).unwrap().start, u64::from(n - 1) * u64::from(m.chunk_size().get()));
        }
        prop_assert_eq!(m.chunk_range(n), None);
    }

    #[test]
    fn truncations_and_extensions_are_corruption(
        m in manifest(),
        cut in any::<prop::sample::Index>(),
        extra in prop::collection::vec(any::<u8>(), 1..16),
        sealed in any::<bool>(),
    ) {
        let bytes = m.encode();
        let short = bytes[..cut.index(bytes.len())].to_vec();
        let long = [bytes.clone(), extra].concat();
        for b in [short, long] {
            let b = if sealed { reseal(b) } else { b };
            let verdict = check_decode(&b).map_err(TestCaseError::fail)?;
            prop_assert!(matches!(verdict, Verdict::Corrupt(_)), "{:?}", verdict);
        }
    }

    #[test]
    fn any_bit_flip_is_corruption(m in manifest(), at in any::<prop::sample::Index>(), bit in 0..8_u8) {
        let mut bytes = m.encode();
        let i = at.index(bytes.len());
        bytes[i] ^= 1 << bit;
        let want = if i < 4 { Rule::Magic } else { Rule::Checksum };
        prop_assert_eq!(check_decode(&bytes).map_err(TestCaseError::fail)?, Verdict::Corrupt(want), "bit {} of byte {}", bit, i);
    }

    #[test]
    fn resealed_byte_rewrites_match_the_spec(
        m in manifest(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
        header_only in any::<bool>(),
    ) {
        // Anywhere the CRC covers (magic included), or concentrated on the header.
        let mut bytes = m.encode();
        let covered = if header_only { 72 } else { bytes.len() - 4 };
        for (at, value) in edits {
            bytes[at.index(covered)] = value;
        }
        check_decode(&reseal(bytes)).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn structured_headers_match_the_spec((h, sealed) in head()) {
        let b = h.seal();
        let b = if sealed { b } else { unseal(b) };
        check_decode(&b).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn arbitrary_bytes_match_the_spec(
        body in prop::collection::vec(any::<u8>(), 0..160),
        magic in any::<bool>(),
        sealed in any::<bool>(),
    ) {
        let mut b = body;
        if magic && b.len() >= 4 {
            b[..4].copy_from_slice(b"PBLM");
        }
        let b = if sealed { reseal(b) } else { b };
        check_decode(&b).map_err(TestCaseError::fail)?;
    }
}

#[cfg(feature = "slow")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100_000))]

    #[test]
    fn slow_structured_headers_match_the_spec((h, sealed) in head()) {
        let b = h.seal();
        let b = if sealed { b } else { unseal(b) };
        check_decode(&b).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn slow_resealed_byte_rewrites_match_the_spec(
        m in manifest(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
    ) {
        let mut bytes = m.encode();
        let covered = bytes.len() - 4;
        for (at, value) in edits {
            bytes[at.index(covered)] = value;
        }
        check_decode(&reseal(bytes)).map_err(TestCaseError::fail)?;
    }
}

#[test]
fn checks_against_the_lookup() {
    use persia_format::BlobRef;
    use std::collections::BTreeMap;

    let m = golden_three();
    let id = m.blob_id();
    let other = BlobId::from_bytes([9; 32]);
    m.check_blob_id(id).unwrap();
    let e = m.check_blob_id(other).unwrap_err();
    assert!(e.is_corruption(), "{e}");

    let blob_ref = |id, size, chunk| BlobRef::new(id, size, "a/b", chunk, BTreeMap::new()).unwrap();
    m.check_ref(&blob_ref(id, 13, 5)).unwrap();
    for (r, what) in [
        (blob_ref(other, 13, 5), "names another blob"),
        (blob_ref(id, 12, 5), "size differs"),
        (blob_ref(id, 13, 4), "chunk size differs"),
    ] {
        match m.check_ref(&r) {
            Err(Error::Corrupt {
                encoding,
                what: got,
            }) => {
                assert_eq!(encoding, "blob manifest");
                assert!(got.contains(what), "{got}");
            }
            other => panic!("expected Corrupt for {what}, got {other:?}"),
        }
    }
}
