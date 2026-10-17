//! Opens every golden container fixture in `tests/fixtures/format/` (ROADMAP 1.9.2; SPEC §4.2–4.4, §15).
//!
//! The fixtures are written by `cargo xtask fixtures`, one directory per format version. These tests only read
//! them: each file must open through the container layer (header, active superblock slot, frame scan) and hold
//! what its entry in [`V1_0`] says. The 12288-byte prefix of `empty.persia` and the first frame of `frames.persia`
//! are also checked byte by byte against values written down here, so a writer and reader that drift together
//! still fail.

// Test-only helpers index and unwrap on purpose (see reader_writer_props.rs).
#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use persia_format::frame::{FRAME_HEADER_LEN, Frame, FrameKind, FrameScanner, ScanEnd};
use persia_format::header::{Access, HEADER_LEN, Header, MAGIC};
use persia_format::superblock::{ActiveSuperblock, FIRST_FRAME_OFFSET, SLOT_LEN, Slot, Superblock};
use persia_format::{Corruption, Error, Reader};

/// Largest decoded payload any fixture frame may have.
const MAX_PAYLOAD: usize = 1 << 20;
/// ROADMAP 1.9.2: fixtures are small enough to commit.
const MAX_FIXTURE_LEN: usize = 100 * 1024;
const DB_UUID: [u8; 16] = *b"persia-fixtures!";

/// How the scan of a fixture ends (SPEC §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Clean,
    TornTail,
    Corrupt,
}

/// What one fixture holds.
struct Expected {
    name: &'static str,
    slot: Slot,
    seq: u64,
    /// `(kind byte, compressed)` of every frame from [`FIRST_FRAME_OFFSET`] to the active `log_end`, in order.
    committed: &'static [(u8, bool)],
    /// Valid frames after `log_end`: durable but not committed (SPEC §4.4, §8.2).
    after_log_end: &'static [(u8, bool)],
    end: End,
    /// Whether slot A is still the slot of a new container (commit 1 went to slot B, SPEC §4.5).
    slot_a_is_new: bool,
}

/// The committed log of `frames.persia`: `WalChunk`, `TombstoneSidecar`, an unknown kind, `Padding`, `Segment`, a
/// compressed `WalChunk`, `Manifest`.
const LOG: &[(u8, bool)] = &[
    (1, false),
    (5, false),
    (0x7F, false),
    (4, false),
    (2, false),
    (1, true),
    (3, false),
];

const COMMIT_1: Expected = Expected {
    name: "frames.persia",
    slot: Slot::B,
    seq: 1,
    committed: LOG,
    after_log_end: &[],
    end: End::Clean,
    slot_a_is_new: true,
};

/// Format version 1.0 (unreleased: `cargo xtask fixtures` still writes it).
const V1_0: &[Expected] = &[
    Expected {
        name: "empty.persia",
        slot: Slot::A,
        seq: 0,
        committed: &[],
        ..COMMIT_1
    },
    COMMIT_1,
    // An append that runs past the end of the file.
    Expected {
        name: "torn-tail.persia",
        end: End::TornTail,
        ..COMMIT_1
    },
    // An append whose length grew but whose data is zeros.
    Expected {
        name: "torn-zeros.persia",
        end: End::TornTail,
        ..COMMIT_1
    },
    // A damaged frame followed by a valid one.
    Expected {
        name: "corrupt-frame.persia",
        end: End::Corrupt,
        ..COMMIT_1
    },
    // Commit 2's frames are durable, its slot A write was torn.
    Expected {
        name: "torn-slot.persia",
        after_log_end: &[(1, false), (3, false)],
        slot_a_is_new: false,
        ..COMMIT_1
    },
];

/// Every fixture directory and what it must hold. Released versions stay here forever (SPEC §15).
const VERSIONS: &[(&str, &[Expected])] = &[("v1.0", V1_0)];

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/format")
}

fn read_fixture(version: &str, name: &str) -> Vec<u8> {
    let path = fixtures_root().join(version).join(name);
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (run `cargo xtask fixtures`)", path.display()))
}

fn all_fixtures() -> impl Iterator<Item = (&'static str, &'static Expected)> {
    VERSIONS
        .iter()
        .flat_map(|(v, expected)| expected.iter().map(move |e| (*v, e)))
}

fn sorted_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A container opened the way recovery does: header, active slot, then frames scanned from the first frame offset.
struct Opened<'a> {
    header: Header,
    active: ActiveSuperblock,
    frames: Vec<Frame<'a>>,
    end: ScanEnd,
}

fn open(bytes: &[u8]) -> Result<Opened<'_>, Error> {
    let mut r = Reader::new(bytes);
    let header = Header::decode(&mut r)?;
    let active = Superblock::read_active(&mut r)?;
    let start = usize::try_from(FIRST_FRAME_OFFSET).unwrap();
    let mut scanner = FrameScanner::new(&bytes[start..], FIRST_FRAME_OFFSET);
    let frames: Vec<Frame<'_>> = scanner.by_ref().collect();
    for f in &frames {
        // Unknown kinds are skipped, never interpreted (SPEC §4.4).
        if f.kind().is_known() {
            f.payload(MAX_PAYLOAD)?;
        }
    }
    let end = scanner.end().cloned().unwrap();
    Ok(Opened {
        header,
        active,
        frames,
        end,
    })
}

#[test]
fn directories_hold_exactly_the_expected_fixtures() {
    let versions: Vec<&str> = VERSIONS.iter().map(|(v, _)| *v).collect();
    assert_eq!(
        sorted_names(&fixtures_root()),
        versions,
        "every version needs an entry in VERSIONS"
    );
    for (version, expected) in VERSIONS {
        let mut names: Vec<&str> = expected.iter().map(|e| e.name).collect();
        names.sort_unstable();
        assert_eq!(
            sorted_names(&fixtures_root().join(version)),
            names,
            "{version}"
        );
    }
}

#[test]
fn fixtures_are_small() {
    for (v, e) in all_fixtures() {
        let len = read_fixture(v, e.name).len();
        assert!(len < MAX_FIXTURE_LEN, "{v}/{}: {len} bytes", e.name);
    }
}

#[test]
fn every_fixture_opens_and_holds_what_it_should() {
    for (v, e) in all_fixtures() {
        let bytes = read_fixture(v, e.name);
        let opened = open(&bytes).unwrap_or_else(|err| panic!("{v}/{}: {err}", e.name));
        check(&format!("{v}/{}", e.name), e, &bytes, &opened);
    }
}

fn kinds(frames: &[Frame<'_>]) -> Vec<(u8, bool)> {
    frames
        .iter()
        .map(|f| (f.kind().to_u8(), f.is_compressed()))
        .collect()
}

fn check(name: &str, e: &Expected, bytes: &[u8], o: &Opened<'_>) {
    assert_eq!(o.header, Header::new(DB_UUID), "{name}: header");
    assert_eq!(o.header.access(), Access::ReadWrite, "{name}");
    assert_eq!(
        (o.active.slot, o.active.superblock.seq),
        (e.slot, e.seq),
        "{name}: active slot"
    );

    let sb = o.active.superblock;
    let (committed, after) = o.frames.split_at(e.committed.len().min(o.frames.len()));
    assert_eq!(kinds(committed), e.committed, "{name}: committed frames");
    assert_eq!(
        kinds(after),
        e.after_log_end,
        "{name}: frames after log_end"
    );
    // Frames are numbered 1, 2, ... in append order; the slot names the next one (SPEC §4.4; ADR-0013).
    let seqs: Vec<u64> = o.frames.iter().map(Frame::frame_seq).collect();
    let expected: Vec<u64> = (1..).take(o.frames.len()).collect();
    assert_eq!(seqs, expected, "{name}: frame_seq run");
    assert_eq!(
        sb.next_frame_seq,
        u64::try_from(committed.len()).unwrap() + 1,
        "{name}: next_frame_seq"
    );
    let log_end = committed.last().map_or(FIRST_FRAME_OFFSET, Frame::end);
    assert_eq!(
        sb.log_end, log_end,
        "{name}: log_end is the end of the last committed frame"
    );

    let file_end = u64::try_from(bytes.len()).unwrap();
    let valid_end = o.frames.last().map_or(FIRST_FRAME_OFFSET, Frame::end);
    assert_eq!(o.end.offset(), valid_end, "{name}: scan end");
    let end = match o.end {
        ScanEnd::Clean { .. } => End::Clean,
        ScanEnd::TornTail { .. } => End::TornTail,
        ScanEnd::Corrupt { .. } => End::Corrupt,
        ScanEnd::Unsupported { .. } => panic!("{name}: {:?}", o.end),
    };
    assert_eq!(end, e.end, "{name}: {:?}", o.end);
    assert_eq!(end == End::Clean, valid_end == file_end, "{name}");

    // The checks recovery runs (SPEC §4.3, §8.6; ADR-0013): the slot fits the file, the committed WAL range is one
    // `frame_seq` run ending at `next_frame_seq - 1` exactly at `log_end`.
    assert_eq!(
        o.active.check_bounds(file_end),
        Ok(()),
        "{name}: check_bounds"
    );
    let (wal_start, log_end) = (
        usize::try_from(sb.wal_start).unwrap(),
        usize::try_from(sb.log_end).unwrap(),
    );
    let mut wal = FrameScanner::new(&bytes[wal_start..log_end], sb.wal_start).contiguous();
    let wal_frames = wal.by_ref().count();
    assert_eq!(
        wal.end(),
        Some(&ScanEnd::Clean { offset: sb.log_end }),
        "{name}: committed WAL range"
    );
    if wal_frames > 0 {
        assert_eq!(
            wal.next_frame_seq(),
            Some(sb.next_frame_seq),
            "{name}: WAL run ends at next_frame_seq - 1"
        );
    }

    // Recovery scans from `log_end` (SPEC §4.4), checking the run from `next_frame_seq`, and must find the same frames
    // and stop at the same place as the unchecked scan from the first frame.
    let mut tail =
        FrameScanner::new(&bytes[log_end..], sb.log_end).expect_frame_seq(sb.next_frame_seq);
    let tail_frames: Vec<Frame<'_>> = tail.by_ref().collect();
    assert_eq!(tail_frames, after, "{name}: recovery scan from log_end");
    assert_eq!(tail.end(), Some(&o.end), "{name}");

    if sb.seq == 0 {
        assert_eq!(sb, created(), "{name}: a new container's slot (SPEC §4.3)");
        return;
    }
    let manifest =
        frame_at(committed, sb.manifest_off).unwrap_or_else(|| panic!("{name}: manifest_off"));
    assert_eq!(manifest.kind(), FrameKind::Manifest, "{name}");
    assert_eq!(
        manifest.total_len(),
        sb.manifest_len,
        "{name}: manifest_len"
    );
    let wal = frame_at(committed, sb.wal_start).unwrap_or_else(|| panic!("{name}: wal_start"));
    assert_eq!(wal.kind(), FrameKind::WalChunk, "{name}");
    assert!(
        wal.payload(MAX_PAYLOAD)
            .unwrap()
            .starts_with(b"wal: put doc 2"),
        "{name}"
    );

    // `page_size` is advisory: the writer aligned the Segment payload to it (ADR-0008).
    let page = u64::from(o.header.page_size);
    let header_len = u64::try_from(FRAME_HEADER_LEN).unwrap();
    for f in o.frames.iter().filter(|f| f.kind() == FrameKind::Segment) {
        assert_eq!(
            (f.offset() + header_len) % page,
            0,
            "{name}: segment payload alignment"
        );
        assert_eq!(f.payload(MAX_PAYLOAD).unwrap().len(), 1024, "{name}");
    }
}

fn frame_at<'a>(frames: &[Frame<'a>], offset: u64) -> Option<Frame<'a>> {
    frames.iter().copied().find(|f| f.offset() == offset)
}

fn created() -> Superblock {
    Superblock {
        log_end: FIRST_FRAME_OFFSET,
        wal_start: FIRST_FRAME_OFFSET,
        next_frame_seq: 1,
        ..Superblock::default()
    }
}

/// Slot A of `torn-slot.persia` is commit 2's slot with only its first half written: it fails its CRC, and what
/// survives points at commit 2's Manifest after commit 1's `log_end`.
#[test]
fn torn_slot_is_invalid_and_points_past_log_end() {
    let bytes = read_fixture("v1.0", "torn-slot.persia");
    let a = &bytes[HEADER_LEN..HEADER_LEN + SLOT_LEN];
    let err = Superblock::decode(&mut Reader::new(a)).unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    let o = open(&bytes).unwrap();
    assert_eq!(u64_at(a, 0), 2, "seq");
    let manifest = frame_at(&o.frames, u64_at(a, 8)).unwrap();
    assert_eq!(manifest.kind(), FrameKind::Manifest);
    assert!(manifest.offset() >= o.active.superblock.log_end);
    assert_eq!(u64_at(a, 24), o.end.offset(), "log_end of commit 2");
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The golden 12288-byte prefix (ADR-0008): header page, slot A at 4096, slot B at 8192, every field at its absolute
/// offset (SPEC §4.2, §4.3). CRCs are written down, not recomputed.
#[test]
fn golden_prefix_of_a_new_container() {
    let b = read_fixture("v1.0", "empty.persia");
    assert_eq!(b.len(), 12288);
    assert_eq!(FIRST_FRAME_OFFSET, 12288);

    assert_eq!(&b[0..8], b"PERSIA\0\x01");
    assert_eq!(b[0..8], MAGIC);
    assert_eq!(&b[8..12], &[1, 0, 0, 0], "format_major 1, format_minor 0");
    assert_eq!(u32_at(&b, 12), 0xDA83_6092, "header_crc");
    assert_eq!(u64_at(&b, 16), 0, "incompat_flags");
    assert_eq!(u64_at(&b, 24), 0, "rocompat_flags");
    assert_eq!(&b[32..48], &DB_UUID);
    assert_eq!(u32_at(&b, 48), 4096, "page_size");
    assert!(b[52..4096].iter().all(|&x| x == 0), "header reserved");

    for (slot, at) in [(Slot::A, 4096), (Slot::B, 8192)] {
        assert_eq!(slot.offset(), at as u64);
        let s = &b[at..at + 4096];
        assert_eq!(u64_at(s, 0), 0, "{slot:?} seq");
        assert_eq!(u64_at(s, 8), 0, "{slot:?} manifest_off");
        assert_eq!(u64_at(s, 16), 0, "{slot:?} manifest_len");
        assert_eq!(u64_at(s, 24), 12288, "{slot:?} log_end");
        assert_eq!(u64_at(s, 32), 12288, "{slot:?} wal_start");
        assert_eq!(u64_at(s, 40), 0, "{slot:?} fence_token");
        assert_eq!(u64_at(s, 48), 1, "{slot:?} next_frame_seq");
        assert!(s[56..4092].iter().all(|&x| x == 0), "{slot:?} reserved");
        assert_eq!(u32_at(s, 4092), 0x57F6_CFF5, "{slot:?} crc");
    }
}

/// The first frame of `frames.persia`, byte by byte (SPEC §4.4), CRC written down.
#[test]
fn golden_first_frame() {
    let b = read_fixture("v1.0", "frames.persia");
    let f = &b[12288..12288 + 48];
    assert_eq!(&f[0..4], b"PFR1", "frame_magic");
    assert_eq!(f[4], 1, "kind WalChunk");
    assert_eq!(&f[5..8], &[0, 0, 0], "flags");
    assert_eq!(u64_at(f, 8), 14, "payload_len");
    assert_eq!(u64_at(f, 16), 1, "frame_seq");
    assert_eq!(&f[24..38], b"wal: put doc 1");
    assert_eq!(&f[38..40], &[0, 0], "padding to 8 bytes");
    assert_eq!(u32_at(f, 40), 0x1C9E_8C3F, "crc32c over kind..padding");
    assert_eq!(u32_at(f, 44), 0, "reserved");
}

/// Every fixture is a container this build created: they share the header page, and slot A of the committed ones
/// is the slot of a new container unless a later commit tore it.
#[test]
fn fixtures_share_the_new_container_prefix() {
    let empty = read_fixture("v1.0", "empty.persia");
    for (v, e) in all_fixtures() {
        let b = read_fixture(v, e.name);
        assert_eq!(
            b[..HEADER_LEN],
            empty[..HEADER_LEN],
            "{v}/{}: header page",
            e.name
        );
        assert_eq!(
            b[4096..8192] == empty[4096..8192],
            e.slot_a_is_new,
            "{v}/{}: slot A",
            e.name
        );
    }
}

/// Damaging a fixture is detected: guards against a fixture test that would accept anything.
#[test]
fn damaged_fixture_does_not_open() {
    let mut b = read_fixture("v1.0", "frames.persia");
    b[40] ^= 1; // db_uuid, covered by header_crc
    assert!(matches!(open(&b), Err(Error::Corrupt { .. })));

    let mut b = read_fixture("v1.0", "frames.persia");
    b[4096 + 100] = 1; // slot A reserved
    b[8192] ^= 1; // slot B seq
    match open(&b) {
        Err(Error::Corrupt { reason, .. }) => assert_eq!(reason, Corruption::NoValidSuperblock),
        other => panic!("{:?}", other.err()),
    }
}
