//! Opens every golden container fixture in `tests/fixtures/format/` (ROADMAP 1.9.2; SPEC §4.2–4.4, §15).
//!
//! The fixtures are written by `cargo xtask fixtures`. These tests only read them: each file must open through the
//! container layer (header, active superblock slot, frame scan) and hold what its entry in [`EXPECTED`] says.
//! The 12288-byte prefix of `empty.persia` is also checked byte by byte against values written down here, so a
//! writer and reader that drift together still fail.

// Test-only helpers index and unwrap on purpose (see reader_writer_props.rs).
#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use persia_format::frame::{Frame, FrameKind, FrameScanner, ScanEnd};
use persia_format::header::{Access, HEADER_LEN, Header, MAGIC};
use persia_format::superblock::{ActiveSuperblock, FIRST_FRAME_OFFSET, SLOT_LEN, Slot, Superblock};
use persia_format::{Corruption, Error, Reader};

/// Largest decoded payload any fixture frame may have.
const MAX_PAYLOAD: usize = 1 << 20;
/// ROADMAP 1.9.2: fixtures are small enough to commit.
const MAX_FIXTURE_LEN: usize = 100 * 1024;
const DB_UUID: [u8; 16] = *b"persia-fixtures!";

/// What one fixture holds.
struct Expected {
    name: &'static str,
    slot: Slot,
    seq: u64,
    /// `(kind byte, compressed)` of every frame from [`FIRST_FRAME_OFFSET`], in order.
    frames: &'static [(u8, bool)],
    /// Whether an interrupted append follows `log_end`.
    torn_tail: bool,
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

const EXPECTED: &[Expected] = &[
    Expected {
        name: "empty.persia",
        slot: Slot::A,
        seq: 0,
        frames: &[],
        torn_tail: false,
    },
    Expected {
        name: "frames.persia",
        slot: Slot::B,
        seq: 1,
        frames: LOG,
        torn_tail: false,
    },
    Expected {
        name: "torn-tail.persia",
        slot: Slot::B,
        seq: 1,
        frames: LOG,
        torn_tail: true,
    },
    Expected {
        name: "torn-slot.persia",
        slot: Slot::B,
        seq: 1,
        frames: LOG,
        torn_tail: false,
    },
];

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/format")
}

fn read_fixture(name: &str) -> Vec<u8> {
    let path = fixtures_dir().join(name);
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (run `cargo xtask fixtures`)", path.display()))
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
fn directory_holds_exactly_the_expected_fixtures() {
    let mut found: Vec<String> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    found.sort();
    let mut expected: Vec<&str> = EXPECTED.iter().map(|e| e.name).collect();
    expected.sort_unstable();
    assert_eq!(found, expected, "every fixture needs an entry in EXPECTED");
}

#[test]
fn fixtures_are_small() {
    for e in EXPECTED {
        let len = read_fixture(e.name).len();
        assert!(len < MAX_FIXTURE_LEN, "{}: {len} bytes", e.name);
    }
}

#[test]
fn every_fixture_opens_and_holds_what_it_should() {
    for e in EXPECTED {
        let bytes = read_fixture(e.name);
        let opened = open(&bytes).unwrap_or_else(|err| panic!("{}: {err}", e.name));
        check(e, &bytes, &opened);
    }
}

fn check(e: &Expected, bytes: &[u8], o: &Opened<'_>) {
    let name = e.name;
    assert_eq!(o.header, Header::new(DB_UUID), "{name}: header");
    assert_eq!(o.header.access(), Access::ReadWrite, "{name}");
    assert_eq!(
        (o.active.slot, o.active.superblock.seq),
        (e.slot, e.seq),
        "{name}: active slot"
    );

    let kinds: Vec<(u8, bool)> = o
        .frames
        .iter()
        .map(|f| (f.kind().to_u8(), f.is_compressed()))
        .collect();
    assert_eq!(kinds, e.frames, "{name}: frames");

    let sb = o.active.superblock;
    let file_end = u64::try_from(bytes.len()).unwrap();
    let log_end = o.frames.last().map_or(FIRST_FRAME_OFFSET, Frame::end);
    assert_eq!(
        sb.log_end, log_end,
        "{name}: log_end is the end of the last frame"
    );
    assert_eq!(o.end.offset(), log_end, "{name}: scan end");
    if e.torn_tail {
        assert!(
            matches!(o.end, ScanEnd::TornTail { .. }),
            "{name}: {:?}",
            o.end
        );
        assert!(file_end > log_end, "{name}");
    } else {
        assert_eq!(o.end, ScanEnd::Clean { offset: file_end }, "{name}");
    }

    // Recovery scans from `log_end` (SPEC §4.4) and must stop at the same place.
    let from_log_end = usize::try_from(sb.log_end).unwrap();
    let mut tail = FrameScanner::new(&bytes[from_log_end..], sb.log_end);
    assert_eq!(
        tail.by_ref().count(),
        0,
        "{name}: no valid frame after log_end"
    );
    assert_eq!(tail.end(), Some(&o.end), "{name}");

    if sb.seq == 0 {
        assert_eq!(sb, created(), "{name}: a new container's slot (SPEC §4.3)");
        return;
    }
    let manifest = frame_at(o, sb.manifest_off).unwrap_or_else(|| panic!("{name}: manifest_off"));
    assert_eq!(manifest.kind(), FrameKind::Manifest, "{name}");
    assert_eq!(
        manifest.total_len(),
        sb.manifest_len,
        "{name}: manifest_len"
    );
    let wal = frame_at(o, sb.wal_start).unwrap_or_else(|| panic!("{name}: wal_start"));
    assert_eq!(wal.kind(), FrameKind::WalChunk, "{name}");
    assert!(
        wal.payload(MAX_PAYLOAD)
            .unwrap()
            .starts_with(b"wal: put doc 2"),
        "{name}"
    );

    // `page_size` is advisory: the writer aligned the Segment payload to it (ADR-0008).
    let page = u64::from(o.header.page_size);
    for f in o.frames.iter().filter(|f| f.kind() == FrameKind::Segment) {
        assert_eq!(
            (f.offset() + 24) % page,
            0,
            "{name}: segment payload alignment"
        );
        assert_eq!(f.payload(MAX_PAYLOAD).unwrap().len(), 1024, "{name}");
    }
}

fn frame_at<'a>(o: &Opened<'a>, offset: u64) -> Option<Frame<'a>> {
    o.frames.iter().copied().find(|f| f.offset() == offset)
}

fn created() -> Superblock {
    Superblock {
        log_end: FIRST_FRAME_OFFSET,
        wal_start: FIRST_FRAME_OFFSET,
        ..Superblock::default()
    }
}

#[test]
fn torn_slot_is_invalid_and_slot_b_wins() {
    let bytes = read_fixture("torn-slot.persia");
    let a = &bytes[HEADER_LEN..HEADER_LEN + SLOT_LEN];
    let err = Superblock::decode(&mut Reader::new(a)).unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    // Only the first half of the page is new: commit 2's seq is there, the old CRC is not its CRC.
    assert_eq!(u64_at(a, 0), 2);
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
    let b = read_fixture("empty.persia");
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
        assert!(s[48..4092].iter().all(|&x| x == 0), "{slot:?} reserved");
        assert_eq!(u32_at(s, 4092), 0x7329_F26E, "{slot:?} crc");
    }
}

/// Every fixture is a container this build created: they share the header page, and slot A of the committed ones
/// is the slot of a new container (commit 1 went to slot B, SPEC §4.5).
#[test]
fn fixtures_share_the_new_container_prefix() {
    let empty = read_fixture("empty.persia");
    for e in EXPECTED {
        let b = read_fixture(e.name);
        assert_eq!(
            b[..HEADER_LEN],
            empty[..HEADER_LEN],
            "{}: header page",
            e.name
        );
        if e.name != "torn-slot.persia" {
            assert_eq!(b[4096..8192], empty[4096..8192], "{}: slot A", e.name);
        }
    }
}

/// Damaging the golden prefix is detected: guards against a fixture test that would accept anything.
#[test]
fn damaged_fixture_does_not_open() {
    let mut b = read_fixture("frames.persia");
    b[40] ^= 1; // db_uuid, covered by header_crc
    assert!(matches!(open(&b), Err(Error::Corrupt { .. })));

    let mut b = read_fixture("frames.persia");
    b[4096 + 100] = 1; // slot A reserved
    b[8192] ^= 1; // slot B seq
    match open(&b) {
        Err(Error::Corrupt { reason, .. }) => assert_eq!(reason, Corruption::NoValidSuperblock),
        other => panic!("{:?}", other.err()),
    }
}
