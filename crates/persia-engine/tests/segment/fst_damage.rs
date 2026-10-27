//! Damaged FSTs behind a valid CRC (SPEC §5.3.1, §5.3.6): the `fst` crate does not validate what it reads, so a
//! `TermDict` or `IdMap` payload with crafted bytes and a recomputed CRC used to make `Segment::open` panic (fuzz
//! crash `crash-5bac96d5`). The reader now checks the FST's structure first: such damage is a corruption `Err`.

use persia_engine::segment::{ReadError, SectionKind, Segment, SegmentCorruption};
use persia_format::ByteSource;
use persia_format::checksum::crc32c;

use super::read::read_back;
use super::{fixture_path, schema};

/// The golden segment, and the payload ranges (CRC excluded) of its raw sections (the FSTs).
fn golden() -> (Vec<u8>, Vec<(SectionKind, std::ops::Range<usize>)>) {
    let bytes = std::fs::read(fixture_path()).unwrap();
    let segment = Segment::open(ByteSource::new(bytes.clone().into()), &schema()).unwrap();
    let raw = segment
        .sections()
        .unwrap()
        .into_iter()
        .filter(|s| s.stream.is_none())
        .map(|s| {
            let start = usize::try_from(s.offset).unwrap();
            let end = start + usize::try_from(s.len).unwrap() - 4;
            (s.kind, start..end)
        })
        .collect::<Vec<_>>();
    assert!(raw.iter().any(|(k, _)| *k == SectionKind::TermDict));
    assert!(raw.iter().any(|(k, _)| *k == SectionKind::IdMap));
    (bytes, raw)
}

/// `bytes` with the payload at `range` edited by `edit`, and its CRC recomputed.
fn damaged(bytes: &[u8], range: &std::ops::Range<usize>, edit: impl FnOnce(&mut [u8])) -> Vec<u8> {
    let mut b = bytes.to_vec();
    edit(&mut b[range.clone()]);
    let crc = crc32c(&b[range.clone()]);
    b[range.end..range.end + 4].copy_from_slice(&crc.to_le_bytes());
    b
}

fn open(bytes: &[u8]) -> Result<Segment, ReadError> {
    Segment::open(ByteSource::new(bytes.to_vec().into()), &schema())
}

fn assert_fst_corruption(bytes: &[u8], what: &str) {
    match open(bytes) {
        Err(ReadError::Section {
            reason: SegmentCorruption::Fst,
            ..
        }) => {}
        other => panic!("{what}: expected an FST corruption, got {other:?}"),
    }
}

/// Structural damage `fst::Map::new` lets through (it checks little more than the length and version) is rejected
/// by `Segment::open`.
#[test]
fn structural_damage_is_an_fst_corruption() {
    let (bytes, raw) = golden();
    for (kind, range) in &raw {
        let n = range.len();
        let (count_at, root_at) = (n - 20, n - 12);
        let root = u64::from_le_bytes(bytes[range.start + root_at..][..8].try_into().unwrap());
        let set = |at: usize, v: u64| {
            damaged(&bytes, range, |p| {
                p[at..at + 8].copy_from_slice(&v.to_le_bytes());
            })
        };
        // The fuzz crash: a root address `fst` accepts but that points into the middle of the nodes.
        for r in [root - 1, root - 2, 17, 16] {
            assert_fst_corruption(&set(root_at, r), &format!("{kind:?} root {r}"));
        }
        assert_fst_corruption(&set(root_at, u64::MAX), &format!("{kind:?} huge root"));
        assert_fst_corruption(&set(count_at, 1 << 40), &format!("{kind:?} key count"));
        assert_fst_corruption(&set(0, 2), &format!("{kind:?} version"));
        // The root's state byte: every encoding of a node, pointing anywhere its bytes say.
        for v in 0..=u8::MAX {
            // Whatever open does with it, neither it nor a full read panics.
            let _ = read_back(&schema(), &damaged(&bytes, range, |p| p[root_at - 1] = v));
        }
    }
}

/// Every byte of each FST payload, set to a few values with the CRC recomputed: open and a full read never panic,
/// and whatever open rejects is a corruption.
#[test]
fn damaged_fst_bytes_never_panic() {
    let (bytes, raw) = golden();
    for (kind, range) in &raw {
        for at in 0..range.len() {
            let old = bytes[range.start + at];
            for v in [0, u8::MAX, old ^ 0x01, old ^ 0x80, old.wrapping_sub(1)] {
                if v == old {
                    continue;
                }
                let b = damaged(&bytes, range, |p| p[at] = v);
                match read_back(&schema(), &b) {
                    Ok(_) => {}
                    Err(e) => assert!(e.is_corruption(), "{kind:?} byte {at} = {v}: {e}"),
                }
            }
        }
    }
}
