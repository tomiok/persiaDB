//! The tombstone sidecar encoding (ROADMAP 6.4.2; SPEC §8.3, ADR-0030).
//!
//! Invariants:
//! 1. Golden files: the sidecars below encode to exactly the bytes in `tests/fixtures/tombstones/v1/` and decode back
//!    to themselves. They are never regenerated once released (SPEC §15); `PERSIA_WRITE_TOMBSTONE_FIXTURES=1` writes
//!    the missing ones.
//! 2. Round trip: any valid sidecar decodes to itself, and its encoding is stable.
//! 3. Corruption: any truncation or extension of a valid encoding is rejected as corruption, never a panic.
//! 4. `decode` never panics and fails only with `Corrupt` (or `UnsupportedVersion` for an encoding version above 1):
//!    every single bit flip; every header field and every part of the roaring bitmap (cookie, container count, keys,
//!    array values, runs, bitmap cardinality) rewritten and re-sealed with a valid CRC; any byte rewritten and
//!    re-sealed either fails that way or decodes to a valid sidecar that round-trips.
//! 5. The bitmap is not canonical (ADR-0030 §3): a valid portable bitmap in another layout (run containers with and
//!    without offsets, a no-run cookie with arbitrary offsets, run-shaped data in array or bitmap containers)
//!    decodes to the set it describes.
//! 6. `check_follows` (SPEC §8.3, §8.7.2): `prev ∪ added` at `prev_version + 1` always follows; dropping any docid
//!    of `prev`, or any other version, is `Invalid`.
//! 7. A header claiming up to 2^16 containers (or `u32::MAX`) over a tiny body fails as `Corrupt`, quickly.
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

use persia_engine::manifest::{MAX_SEGMENT_DOCS, SegmentId};
use persia_engine::tombstone::{SidecarError, TombstoneSidecar};
use persia_format::checksum::crc32c;
use proptest::prelude::*;
use roaring::RoaringBitmap;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tombstones/v1")
}

/// One deleted document: an array container.
fn golden_sparse() -> TombstoneSidecar {
    let id = SegmentId::new(0x0123_4567_89ab_cdef_0011_2233_4455_6677);
    TombstoneSidecar::new(id, 1, 10, RoaringBitmap::from_iter([3])).unwrap()
}

/// A run container (a bulk delete), a bitmap container and an array container, at version 7.
fn golden_mixed() -> TombstoneSidecar {
    let mut deleted = RoaringBitmap::new();
    deleted.insert_range(0..70_000);
    deleted.extend((131_072..196_608).step_by(3));
    deleted.extend([200_000, 250_001]);
    TombstoneSidecar::new(SegmentId::new(u128::MAX), 7, 300_000, deleted).unwrap()
}

#[test]
fn golden_fixtures_decode_and_reencode_byte_for_byte() {
    let write = std::env::var_os("PERSIA_WRITE_TOMBSTONE_FIXTURES").is_some();
    for (name, sidecar) in [
        ("sparse.tomb", golden_sparse()),
        ("mixed.tomb", golden_mixed()),
    ] {
        let path = fixture_dir().join(name);
        let encoded = sidecar.encode();
        if write && !path.exists() {
            std::fs::create_dir_all(fixture_dir()).unwrap();
            std::fs::write(&path, &encoded).unwrap();
        }
        let stored = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (PERSIA_WRITE_TOMBSTONE_FIXTURES=1 writes it)",
                path.display()
            )
        });
        assert_eq!(
            TombstoneSidecar::decode(&stored).unwrap(),
            sidecar,
            "{name}"
        );
        assert_eq!(encoded, stored, "{name}: encoding changed");
    }
}

#[test]
fn sparse_golden_bytes() {
    let bytes = golden_sparse().encode();
    let mut expected = b"PTMB".to_vec();
    expected.extend(1_u32.to_le_bytes()); // encoding version
    expected.extend(0x0123_4567_89ab_cdef_0011_2233_4455_6677_u128.to_le_bytes());
    expected.extend(1_u64.to_le_bytes()); // sidecar version
    expected.extend(10_u32.to_le_bytes()); // doc_count
    expected.extend(1_u32.to_le_bytes()); // deleted
    // Portable roaring: no-run cookie, one container (key 0, cardinality 1), its offset, the value 3.
    expected.extend(12_346_u32.to_le_bytes());
    expected.extend(1_u32.to_le_bytes());
    expected.extend([0, 0, 0, 0]);
    expected.extend(16_u32.to_le_bytes());
    expected.extend(3_u16.to_le_bytes());
    let crc = crc32c(&expected);
    expected.extend(crc.to_le_bytes());
    assert_eq!(bytes, expected);
}

fn sidecar() -> impl Strategy<Value = TombstoneSidecar> {
    let docids = prop_oneof![
        prop::collection::vec(0..1_000_u32, 1..50),
        prop::collection::vec(0..300_000_u32, 1..2_000),
        (0..200_000_u32, 1..80_000_u32).prop_map(|(s, n)| (s..s + n).collect()),
    ];
    (any::<u128>(), 1..u64::MAX, docids, 0..1_000_u32).prop_map(|(id, version, docids, extra)| {
        let deleted: RoaringBitmap = docids.into_iter().collect();
        let doc_count = deleted.max().unwrap() + 1 + extra;
        TombstoneSidecar::new(SegmentId::new(id), version, doc_count, deleted).unwrap()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn round_trips(s in sidecar()) {
        let bytes = s.encode();
        let back = TombstoneSidecar::decode(&bytes).unwrap();
        prop_assert_eq!(&back, &s);
        prop_assert_eq!(back.encode(), bytes);
    }

    #[test]
    fn truncations_and_extensions_are_corruption(s in sidecar(), cut in any::<prop::sample::Index>(), extra in 1..9_usize) {
        let bytes = s.encode();
        let short = &bytes[..cut.index(bytes.len())];
        prop_assert!(TombstoneSidecar::decode(short).unwrap_err().is_corruption());
        let mut long = bytes.clone();
        long.extend(std::iter::repeat_n(0, extra));
        prop_assert!(TombstoneSidecar::decode(&long).unwrap_err().is_corruption());
    }
}

// Hand-built encodings: a header and a portable roaring bitmap written field by field, so each test can break
// exactly one rule and re-seal the CRC (SPEC §8.3).

/// The sidecar header fields, written as given (no validation).
#[derive(Clone, Copy)]
struct Head {
    encoding: u32,
    segment: u128,
    version: u64,
    doc_count: u32,
    deleted: u32,
}

/// A valid header for `deleted` docids of a five-container segment (keys 0..=4).
fn head(deleted: u32) -> Head {
    Head {
        encoding: 1,
        segment: 42,
        version: 3,
        doc_count: 5 << 16,
        deleted,
    }
}

/// A full sidecar: the header, `bitmap` as the deleted docids, and a valid CRC32C trailer.
fn seal(h: Head, bitmap: &[u8]) -> Vec<u8> {
    let mut b = b"PTMB".to_vec();
    b.extend(h.encoding.to_le_bytes());
    b.extend(h.segment.to_le_bytes());
    b.extend(h.version.to_le_bytes());
    b.extend(h.doc_count.to_le_bytes());
    b.extend(h.deleted.to_le_bytes());
    b.extend_from_slice(bitmap);
    let crc = crc32c(&b);
    b.extend(crc.to_le_bytes());
    b
}

/// Recomputes the trailer of `bytes` after a change to the bytes it covers.
fn reseal(mut bytes: Vec<u8>) -> Vec<u8> {
    let end = bytes.len() - 4;
    let crc = crc32c(&bytes[..end]);
    bytes[end..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

#[derive(Clone)]
enum Body {
    /// 16-bit values, as stored.
    Array(Vec<u16>),
    /// 1024 words, as stored.
    Bitmap(Vec<u64>),
    /// `(start, length - 1)` pairs, as stored.
    Run(Vec<(u16, u16)>),
}

#[derive(Clone)]
struct Container {
    key: u16,
    /// The cardinality the description states (1..=65536).
    card: u32,
    body: Body,
}

fn array(key: u16, values: &[u16]) -> Container {
    Container {
        key,
        card: values.len() as u32,
        body: Body::Array(values.to_vec()),
    }
}

fn bitmap(key: u16, values: impl IntoIterator<Item = u16>) -> Container {
    let mut words = vec![0_u64; 1024];
    for v in values {
        words[usize::from(v) / 64] |= 1 << (v % 64);
    }
    let card = words.iter().map(|w| w.count_ones()).sum();
    Container {
        key,
        card,
        body: Body::Bitmap(words),
    }
}

fn run(key: u16, runs: &[(u16, u16)]) -> Container {
    let card = runs.iter().map(|&(_, len)| u32::from(len) + 1).sum();
    Container {
        key,
        card,
        body: Body::Run(runs.to_vec()),
    }
}

fn body_len(c: &Container) -> u32 {
    match &c.body {
        Body::Array(v) => 2 * v.len() as u32,
        Body::Bitmap(_) => 8192,
        Body::Run(r) => 2 + 4 * r.len() as u32,
    }
}

/// The portable-format cookie of a bitmap with run containers (12347); the count sits in its high half.
const RUN_COOKIE: u32 = 0x303B;

/// The cookie layout and the offsets to write.
#[derive(Clone, Copy, PartialEq)]
enum Layout {
    /// Cookie 12347 with the count in its high half and a run flag per container; offsets from 4 containers.
    Runs,
    /// Cookie 12346, a `u32` count and correct offsets; no run containers.
    NoRuns,
    /// Like `NoRuns`, but every offset is `0xFFFF_FFFF` (the reader does not use them).
    NoRunsBadOffsets,
}

/// The portable roaring serialization of `cs`, in `layout`.
fn portable(cs: &[Container], layout: Layout) -> Vec<u8> {
    let n = cs.len();
    let mut b = Vec::new();
    let has_offsets = layout != Layout::Runs || n >= 4;
    let mut header_len = if layout == Layout::Runs {
        assert!((1..=65536).contains(&n));
        b.extend((RUN_COOKIE | ((n as u32 - 1) << 16)).to_le_bytes());
        let mut flags = vec![0_u8; n.div_ceil(8)];
        for (i, c) in cs.iter().enumerate() {
            if matches!(c.body, Body::Run(_)) {
                flags[i / 8] |= 1 << (i % 8);
            }
        }
        b.extend(&flags);
        4 + flags.len() as u32
    } else {
        b.extend(12346_u32.to_le_bytes());
        b.extend((n as u32).to_le_bytes());
        8
    };
    header_len += 4 * n as u32 + if has_offsets { 4 * n as u32 } else { 0 };
    for c in cs {
        b.extend(c.key.to_le_bytes());
        b.extend((c.card.wrapping_sub(1) as u16).to_le_bytes());
    }
    if has_offsets {
        let mut offset = header_len;
        for c in cs {
            let o = if layout == Layout::NoRunsBadOffsets {
                u32::MAX
            } else {
                offset
            };
            b.extend(o.to_le_bytes());
            offset += body_len(c);
        }
    }
    for c in cs {
        match &c.body {
            Body::Array(v) => v.iter().for_each(|x| b.extend(x.to_le_bytes())),
            Body::Bitmap(w) => w.iter().for_each(|x| b.extend(x.to_le_bytes())),
            Body::Run(r) => {
                b.extend((r.len() as u16).to_le_bytes());
                for (start, len) in r {
                    b.extend(start.to_le_bytes());
                    b.extend(len.to_le_bytes());
                }
            }
        }
    }
    b
}

/// The docids `cs` describe (from the bodies, not the stated cardinalities).
fn docids(cs: &[Container]) -> RoaringBitmap {
    let mut set = RoaringBitmap::new();
    for c in cs {
        let base = u32::from(c.key) << 16;
        match &c.body {
            Body::Array(v) => set.extend(v.iter().map(|&x| base | u32::from(x))),
            Body::Bitmap(w) => {
                for (i, word) in w.iter().enumerate() {
                    for bit in 0..64 {
                        if word >> bit & 1 == 1 {
                            set.insert(base | (i as u32 * 64 + bit));
                        }
                    }
                }
            }
            Body::Run(r) => {
                for &(start, len) in r {
                    let s = base | u32::from(start);
                    set.insert_range(s..=s + u32::from(len));
                }
            }
        }
    }
    set
}

/// Every container kind, with offsets in the run layout (5 containers ≥ 4): keys 0..=4.
fn mixed_runs() -> Vec<Container> {
    vec![
        array(0, &[1, 5, 9]),
        run(1, &[(0, 9), (20, 0)]),
        bitmap(2, (0..10_000).step_by(2)),
        run(3, &[(65530, 5)]),
        array(4, &[7]),
    ]
}

/// The same docids as [`mixed_runs`] without run containers.
fn mixed_no_runs() -> Vec<Container> {
    let mut key1: Vec<u16> = (0..10).collect();
    key1.push(20);
    vec![
        array(0, &[1, 5, 9]),
        array(1, &key1),
        bitmap(2, (0..10_000).step_by(2)),
        array(3, &[65530, 65531, 65532, 65533, 65534, 65535]),
        array(4, &[7]),
    ]
}

/// A sidecar over `cs` in `layout`, with a header that agrees with the bodies.
fn sidecar_of(cs: &[Container], layout: Layout) -> Vec<u8> {
    seal(head(docids(cs).len() as u32), &portable(cs, layout))
}

#[track_caller]
fn assert_corrupt(bytes: &[u8], case: &str) {
    match TombstoneSidecar::decode(bytes) {
        Err(SidecarError::Corrupt { .. }) => {}
        other => panic!("{case}: expected Corrupt, got {other:?}"),
    }
}

/// `decode` on arbitrary input fails only as `Corrupt` / `UnsupportedVersion`, and whatever it accepts is a valid
/// sidecar that round-trips.
fn check_decode_contract(bytes: &[u8]) -> Result<(), TestCaseError> {
    match TombstoneSidecar::decode(bytes) {
        Err(SidecarError::Corrupt { .. }) => {}
        Err(SidecarError::UnsupportedVersion { version }) => prop_assert!(version > 1),
        Err(e @ SidecarError::Invalid { .. }) => {
            prop_assert!(false, "decode returned a build error: {e:?}");
        }
        Ok(s) => {
            prop_assert!(s.version() >= 1);
            prop_assert!((1..=MAX_SEGMENT_DOCS).contains(&s.doc_count()));
            prop_assert_eq!(u64::from(s.deleted_count()), s.deleted().len());
            prop_assert!((1..=s.doc_count()).contains(&s.deleted_count()));
            prop_assert!(s.deleted().max().unwrap() < s.doc_count());
            let again = TombstoneSidecar::decode(&s.encode()).unwrap();
            prop_assert_eq!(again, s);
        }
    }
    Ok(())
}

#[test]
fn the_hand_built_encoder_matches_the_writer() {
    // Guards the helpers above: in the writer's own layout they produce the writer's bytes.
    let cs = mixed_runs();
    let s = TombstoneSidecar::new(SegmentId::new(42), 3, 5 << 16, docids(&cs)).unwrap();
    assert_eq!(sidecar_of(&cs, Layout::Runs), s.encode());
    let h = Head {
        encoding: 1,
        segment: 0x0123_4567_89ab_cdef_0011_2233_4455_6677,
        version: 1,
        doc_count: 10,
        deleted: 1,
    };
    assert_eq!(
        seal(h, &portable(&[array(0, &[3])], Layout::NoRuns)),
        golden_sparse().encode()
    );
}

#[test]
fn every_bit_flip_of_the_golden_sidecars_is_corruption() {
    // The unit tests flip bit 0 of each byte; here every bit of both golden encodings (the CRC covers all).
    for (name, s) in [("sparse", golden_sparse()), ("mixed", golden_mixed())] {
        let mut b = s.encode();
        for i in 0..b.len() {
            for bit in 0..8 {
                b[i] ^= 1 << bit;
                if !matches!(
                    TombstoneSidecar::decode(&b),
                    Err(SidecarError::Corrupt { .. })
                ) {
                    panic!("{name}: bit {bit} of byte {i} not detected as corruption");
                }
                b[i] ^= 1 << bit;
            }
        }
    }
}

#[test]
fn every_header_field_rewritten_and_resealed_is_rejected() {
    let cs = mixed_runs();
    let n = docids(&cs).len() as u32;
    let bm = portable(&cs, Layout::Runs);
    let ok = |h: Head| TombstoneSidecar::decode(&seal(h, &bm));
    let corrupt = |h: Head, case: &str| assert_corrupt(&seal(h, &bm), case);

    // The base decodes; segment_id is free (the open path checks it against the manifest).
    let base = ok(head(n)).unwrap();
    assert_eq!((base.version(), base.doc_count()), (3, 5 << 16));
    for segment in [0, 1, u128::MAX] {
        assert_eq!(
            ok(Head { segment, ..head(n) }).unwrap().segment_id(),
            SegmentId::new(segment)
        );
    }

    // Magic: every byte, every other value, re-sealed.
    let good = seal(head(n), &bm);
    for i in 0..4 {
        for v in (0..=255_u8).filter(|&v| v != good[i]) {
            let mut b = good.clone();
            b[i] = v;
            assert_corrupt(&reseal(b), "magic");
        }
    }

    // Encoding version: 0 is corruption, anything above 1 is unsupported.
    corrupt(
        Head {
            encoding: 0,
            ..head(n)
        },
        "encoding 0",
    );
    for encoding in [2, 3, 0x100, u32::MAX] {
        assert_eq!(
            ok(Head {
                encoding,
                ..head(n)
            }),
            Err(SidecarError::UnsupportedVersion { version: encoding })
        );
    }

    // Sidecar version 0; any other version decodes.
    corrupt(
        Head {
            version: 0,
            ..head(n)
        },
        "version 0",
    );
    for version in [1, 2, u64::MAX] {
        assert_eq!(ok(Head { version, ..head(n) }).unwrap().version(), version);
    }

    // doc_count: 0, above 2^31, or not above the largest docid (262_151).
    for doc_count in [
        0,
        MAX_SEGMENT_DOCS + 1,
        u32::MAX,
        1,
        n - 1,
        131_072,
        262_151,
    ] {
        corrupt(
            Head {
                doc_count,
                ..head(n)
            },
            &format!("doc_count {doc_count}"),
        );
    }
    for doc_count in [262_152, MAX_SEGMENT_DOCS] {
        assert!(
            ok(Head {
                doc_count,
                ..head(n)
            })
            .is_ok(),
            "doc_count {doc_count}"
        );
    }

    // deleted: 0, above doc_count, or anything but the bitmap's cardinality.
    for deleted in [0, 1, n - 1, n + 1, (5 << 16) + 1, u32::MAX] {
        corrupt(head(deleted), &format!("deleted {deleted}"));
    }
}

#[test]
fn every_bitmap_rule_broken_and_resealed_is_corruption() {
    let base = mixed_runs();
    let n = docids(&base).len() as u32;
    // Each case keeps the header's `deleted` at the base cardinality, then also tries the bodies' cardinality, so
    // only the bitmap is wrong.
    let with = |i: usize, c: Container| {
        let mut cs = base.clone();
        cs[i] = c;
        cs
    };
    let bitmap_card = |card: u32| {
        let mut c = bitmap(2, (0..10_000).step_by(2));
        c.card = card;
        c
    };
    let array_card = |card: u32| {
        let mut c = array(0, &[1, 5, 9]);
        c.card = card;
        c
    };
    let cases: Vec<(&str, Vec<Container>)> = vec![
        ("keys out of order", {
            let mut cs = base.clone();
            cs.swap(1, 2);
            cs
        }),
        ("duplicate key", with(2, bitmap(1, (0..10_000).step_by(2)))),
        ("key above doc_count", with(4, array(5, &[7]))),
        ("array unsorted", with(0, array(0, &[5, 1, 9]))),
        ("array duplicated", with(0, array(0, &[1, 5, 5]))),
        ("array longer than stated", with(0, array_card(2))),
        ("array shorter than stated", with(0, array_card(4))),
        (
            "zero runs",
            with(
                3,
                Container {
                    key: 3,
                    card: 1,
                    body: Body::Run(vec![]),
                },
            ),
        ),
        ("overlapping runs", with(1, run(1, &[(0, 9), (5, 5)]))),
        ("contiguous runs", with(1, run(1, &[(0, 9), (10, 0)]))),
        ("runs out of order", with(1, run(1, &[(20, 0), (0, 9)]))),
        ("duplicate runs", with(1, run(1, &[(0, 9), (0, 9)]))),
        ("run past 65535", with(3, run(3, &[(65535, 1)]))),
        ("bitmap cardinality low", with(2, bitmap_card(4999))),
        ("bitmap cardinality high", with(2, bitmap_card(5001))),
        ("bitmap cardinality 65536", with(2, bitmap_card(65536))),
        // A stated cardinality ≤ 4096 makes the reader expect an array.
        ("bitmap stated as an array", with(2, bitmap_card(4096))),
    ];
    for (case, cs) in cases {
        let bm = portable(&cs, Layout::Runs);
        assert_corrupt(&seal(head(n), &bm), case);
        let d = docids(&cs).len() as u32;
        if (1..=5 << 16).contains(&d) {
            assert_corrupt(&seal(head(d), &bm), case);
        }
    }

    // The same faults in the no-run layout.
    let base_nr = mixed_no_runs();
    for (case, i, c) in [
        (
            "array unsorted",
            1,
            array(1, &[0, 2, 1, 3, 4, 5, 6, 7, 8, 9, 20]),
        ),
        ("array duplicated", 4, array(4, &[7, 7])),
        ("bitmap cardinality", 2, bitmap_card(4999)),
        ("keys out of order", 4, array(1, &[7])),
    ] {
        let mut cs = base_nr.clone();
        cs[i] = c;
        assert_corrupt(&seal(head(n), &portable(&cs, Layout::NoRuns)), case);
    }

    // The cookie: unknown values, and a valid cookie of the other layout.
    let good = portable(&base, Layout::Runs);
    for cookie in [0_u32, 1, 12345, 12348, 0x3039_0000, 0x303A_0000, u32::MAX] {
        let mut bm = good.clone();
        bm[..4].copy_from_slice(&cookie.to_le_bytes());
        assert_corrupt(&seal(head(n), &bm), &format!("cookie {cookie:#x}"));
    }
    let mut bm = good.clone();
    bm[..4].copy_from_slice(&12346_u32.to_le_bytes());
    assert_corrupt(&seal(head(n), &bm), "run body under a no-run cookie");
    let nr = portable(&base_nr, Layout::NoRuns);
    let mut bm = nr.clone();
    bm[..4].copy_from_slice(&(RUN_COOKIE | (4 << 16)).to_le_bytes());
    assert_corrupt(&seal(head(n), &bm), "no-run body under a run cookie");

    // The container count: every wrong value of the run cookie's high half, and a range of no-run counts.
    for count in (1..=65536_u32).filter(|&c| c != 5) {
        let mut bm = good.clone();
        bm[..4].copy_from_slice(&(RUN_COOKIE | ((count - 1) << 16)).to_le_bytes());
        assert_corrupt(&seal(head(n), &bm), &format!("run count {count}"));
    }
    for count in [0_u32, 1, 4, 6, 7, 100, 65535, 65536, 65537, u32::MAX] {
        let mut bm = nr.clone();
        bm[4..8].copy_from_slice(&count.to_le_bytes());
        assert_corrupt(&seal(head(n), &bm), &format!("no-run count {count}"));
    }
    // An empty bitmap: a valid serialization, but the header promises at least one docid.
    let empty = [12346_u32.to_le_bytes(), 0_u32.to_le_bytes()].concat();
    for deleted in [0, 1] {
        assert_corrupt(&seal(head(deleted), &empty), "empty bitmap");
    }
}

#[test]
fn a_run_container_whose_stated_cardinality_disagrees_never_yields_an_inconsistent_sidecar() {
    // The portable format states a cardinality for run containers too, but the runs define the set. roaring 0.11
    // reads the runs and ignores the stated value, which is safe because `decode` then checks the header's
    // `deleted` against the real cardinality. Either outcome is acceptable; an inconsistent sidecar is not.
    let base = mixed_runs();
    let n = docids(&base).len() as u32;
    for card in [1, 10, 12, 65536] {
        let mut cs = base.clone();
        cs[1].card = card;
        match TombstoneSidecar::decode(&seal(head(n), &portable(&cs, Layout::Runs))) {
            Err(SidecarError::Corrupt { .. }) => {}
            Ok(s) => {
                assert_eq!(s.deleted(), &docids(&base), "stated {card}");
                assert_eq!(u64::from(s.deleted_count()), s.deleted().len());
            }
            Err(e) => panic!("stated {card}: {e:?}"),
        }
    }
}

#[test]
fn portable_bitmaps_in_other_layouts_decode() {
    // ADR-0030 §3: the reader accepts any valid portable form. Each decodes to the set its bodies describe, equals
    // the sidecar built from that set, and round-trips through the writer's form.
    let cases: Vec<(&str, Vec<Container>, Layout)> = vec![
        (
            "one run container, no offsets",
            vec![run(0, &[(0, 99)])],
            Layout::Runs,
        ),
        (
            "three containers, runs, no offsets",
            vec![
                run(0, &[(0, 99)]),
                array(1, &[3]),
                bitmap(2, (0..12_000).step_by(2)),
            ],
            Layout::Runs,
        ),
        (
            "four containers, runs, offsets",
            vec![
                run(0, &[(0, 0), (2, 0)]),
                array(1, &[3]),
                bitmap(2, 0..4097),
                run(4, &[(0, 65535)]),
            ],
            Layout::Runs,
        ),
        ("every kind, runs, offsets", mixed_runs(), Layout::Runs),
        (
            "run cookie, no container flagged as run",
            vec![array(0, &[1, 2, 3]), bitmap(1, 0..5000)],
            Layout::Runs,
        ),
        ("no-run cookie, offsets", mixed_no_runs(), Layout::NoRuns),
        (
            "no-run cookie, arbitrary offsets",
            mixed_no_runs(),
            Layout::NoRunsBadOffsets,
        ),
        (
            "a full container as a bitmap",
            vec![bitmap(0, 0..=65535)],
            Layout::NoRuns,
        ),
        (
            "a full container as one run",
            vec![run(0, &[(0, 65535)])],
            Layout::Runs,
        ),
        (
            "a dense array the writer would store as a run",
            vec![array(3, &(0..4096).collect::<Vec<_>>())],
            Layout::NoRuns,
        ),
    ];
    for (case, cs, layout) in cases {
        let expected = docids(&cs);
        let bytes = sidecar_of(&cs, layout);
        let s = TombstoneSidecar::decode(&bytes).unwrap_or_else(|e| panic!("{case}: {e:?}"));
        assert_eq!(s.deleted(), &expected, "{case}");
        assert_eq!(u64::from(s.deleted_count()), expected.len(), "{case}");
        assert_eq!(
            (s.segment_id(), s.version()),
            (SegmentId::new(42), 3),
            "{case}"
        );
        let built =
            TombstoneSidecar::new(s.segment_id(), s.version(), s.doc_count(), expected).unwrap();
        assert_eq!(s, built, "{case}");
        assert_eq!(TombstoneSidecar::decode(&s.encode()).unwrap(), s, "{case}");
        // Bit flips in the foreign layout are corruption too.
        let mut b = bytes.clone();
        for i in (0..bytes.len()).step_by(7) {
            b[i] ^= 0x10;
            assert_corrupt(&b, case);
            b[i] ^= 0x10;
        }
    }
}

#[test]
fn a_header_claiming_many_containers_over_a_tiny_body_fails_fast() {
    // A claim of 2^16 containers (the most the format allows) or more must fail on the missing bytes. Unsafe is
    // denied, so there is no allocator hook: this asserts the outcome, and each case reads well under 1 MiB.
    let tails: [&[u8]; 3] = [&[], &[0; 8], &[0xFF; 64]];
    for tail in tails {
        for count in [65536_u32, 65537, 1 << 24, u32::MAX] {
            let mut bm = [12346_u32.to_le_bytes(), count.to_le_bytes()].concat();
            bm.extend_from_slice(tail);
            assert_corrupt(&seal(head(1), &bm), &format!("no-run count {count}"));
        }
        // The run cookie's largest count: 2^16 containers.
        let mut bm = 0xFFFF_303B_u32.to_le_bytes().to_vec();
        bm.extend_from_slice(tail);
        assert_corrupt(&seal(head(1), &bm), "run count 65536");
        let mut bm = 0xFFFF_303B_u32.to_le_bytes().to_vec();
        bm.extend(std::iter::repeat_n(0xFF, 8192));
        bm.extend_from_slice(tail);
        assert_corrupt(&seal(head(1), &bm), "run count 65536 with its flags");
    }
    // 2^16 descriptions and offsets present, but no container body.
    let mut bm = [12346_u32.to_le_bytes(), 65536_u32.to_le_bytes()].concat();
    for key in 0..=u16::MAX {
        bm.extend(key.to_le_bytes());
        bm.extend(0_u16.to_le_bytes());
    }
    bm.extend(std::iter::repeat_n(0, 4 * 65536));
    let h = Head {
        doc_count: MAX_SEGMENT_DOCS,
        ..head(65536)
    };
    assert_corrupt(&seal(h, &bm), "65536 descriptions, no bodies");
}

/// Sets for `check_follows`: the previous deletions, the new ones, the previous version and spare documents.
fn succession() -> impl Strategy<Value = (RoaringBitmap, RoaringBitmap, u64, u32)> {
    let set = || {
        prop_oneof![
            prop::collection::vec(0..2_000_u32, 0..100)
                .prop_map(|v| v.into_iter().collect::<RoaringBitmap>()),
            (0..140_000_u32, 0..70_000_u32).prop_map(|(s, n)| (s..s + n).collect()),
        ]
    };
    (
        set(),
        set(),
        prop_oneof![Just(0), 0..u64::MAX - 1, Just(u64::MAX - 1)],
        0..100_u32,
    )
        .prop_filter("a sidecar deletes at least one docid", |(p, a, _, _)| {
            !(p | a).is_empty()
        })
}

fn build(version: u64, deleted: RoaringBitmap, extra: u32) -> TombstoneSidecar {
    let doc_count = deleted.max().unwrap() + 1 + extra;
    TombstoneSidecar::new(SegmentId::new(5), version, doc_count, deleted).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_bit_flip_is_corruption(s in sidecar(), at in any::<prop::sample::Index>(), bit in 0..8_u8) {
        let mut bytes = s.encode();
        let i = at.index(bytes.len());
        bytes[i] ^= 1 << bit;
        prop_assert!(
            matches!(TombstoneSidecar::decode(&bytes), Err(SidecarError::Corrupt { .. })),
            "bit {} of byte {}", bit, i
        );
    }

    #[test]
    fn resealed_byte_rewrites_fail_cleanly_or_decode_to_a_valid_sidecar(
        s in sidecar(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
    ) {
        let mut bytes = s.encode();
        let covered = bytes.len() - 4;
        for (at, value) in edits {
            // Anywhere after the magic, which is checked before the CRC.
            bytes[4 + at.index(covered - 4)] = value;
        }
        check_decode_contract(&reseal(bytes))?;
    }

    #[test]
    fn resealed_bitmap_header_rewrites_fail_cleanly_or_decode_to_a_valid_sidecar(
        s in sidecar(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
    ) {
        // Concentrated on the bitmap's own header: cookie, count, run flags, descriptions, offsets.
        let mut bytes = s.encode();
        let window = (bytes.len() - 4 - 40).min(64);
        for (at, value) in edits {
            bytes[40 + at.index(window)] = value;
        }
        check_decode_contract(&reseal(bytes))?;
    }

    #[test]
    fn a_union_at_the_next_version_follows((prev, added, prev_version, extra) in succession()) {
        let next = build(prev_version + 1, &prev | &added, extra);
        prop_assert_eq!(next.check_follows(prev_version, &prev), Ok(()));
        let back = TombstoneSidecar::decode(&next.encode()).unwrap();
        prop_assert_eq!(back.check_follows(prev_version, &prev), Ok(()));
    }

    #[test]
    fn dropping_a_previous_deletion_does_not_follow(
        (prev, added, prev_version, extra) in succession(),
        pick in any::<prop::sample::Index>(),
    ) {
        prop_assume!(!prev.is_empty());
        let dropped = prev.select(pick.index(prev.len() as usize) as u32).unwrap();
        let mut deleted = &prev | &added;
        deleted.remove(dropped);
        prop_assume!(!deleted.is_empty());
        let next = build(prev_version + 1, deleted, extra);
        prop_assert!(
            matches!(next.check_follows(prev_version, &prev), Err(SidecarError::Invalid { .. })),
            "dropped docid {}", dropped
        );
    }

    #[test]
    fn any_other_version_step_does_not_follow(
        (prev, added, prev_version, extra) in succession(),
        version in prop_oneof![1..u64::MAX, Just(u64::MAX), Just(1_u64)],
        near in 0..3_u8,
    ) {
        let deleted = &prev | &added;
        // A random version and a near miss: the same version, two above, one below.
        let near = match near {
            0 => prev_version,
            1 => prev_version.saturating_add(2),
            _ => prev_version.saturating_sub(1),
        };
        for v in [version, near] {
            if v == 0 || v == prev_version + 1 {
                continue;
            }
            let next = build(v, deleted.clone(), extra);
            prop_assert!(
                matches!(next.check_follows(prev_version, &prev), Err(SidecarError::Invalid { .. })),
                "version {} after {}", v, prev_version
            );
        }
        // Nothing follows the last version: the next one would overflow.
        let last = build(u64::MAX, deleted, extra);
        prop_assert!(matches!(last.check_follows(u64::MAX, &prev), Err(SidecarError::Invalid { .. })), "u64::MAX has no successor");
    }
}
