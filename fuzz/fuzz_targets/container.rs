//! Fuzz opening a container (ROADMAP 2.4.7; SPEC §4.2-§4.4, §8.6): arbitrary file contents never panic; an open
//! fails only as corruption, an unsupported format, a corrupt tail, or (read-write) unknown `rocompat_flags`; a
//! read-only open never changes the file; a successful open yields frames contiguous from `wal_start` to `end`, in
//! one `frame_seq` run ending at `next_frame_seq`, each readable; a read-write open leaves the file exactly `end`
//! bytes long and accepts the next commit, which a reopen finds.
//!
//! Input: `mode u8, base u8, offset u16 LE, patch..`. `base` picks a valid container (new, committed, committed
//! with uncommitted frames after `log_end`), `patch` overwrites it from `offset` (growing it if needed). Mode bits:
//! 0 = truncate the file to `offset` first; 1 = fix `header_crc`; 2 and 3 = fix the CRC of slot A and B; 4 = fix the
//! CRC of every frame along the chain from the first. Random bytes almost never carry a valid CRC, so without these
//! the checks behind them would only see the seed corpus.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use persia_format::block::Compression;
use persia_format::checksum::{Crc32c, crc32c};
use persia_format::frame::{FrameKind, MIN_FRAME_LEN};
use persia_format::superblock::{FIRST_FRAME_OFFSET, SLOT_LEN};
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Commit, Container, ContainerFile, Error, Mode, ReadOnlyReason};

const MAX: usize = 1 << 16;
const NONE: Compression = Compression::None;

fn bases() -> &'static [Vec<u8>] {
    static BASES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    BASES.get_or_init(|| {
        let mut c = Container::create_on(SimFile::new(), [3; 16]).unwrap();
        let new = c.file().bytes();
        let w = c.append(FrameKind::WalChunk, b"put 1", NONE).unwrap();
        c.append(
            FrameKind::Segment,
            &[0x5A; 300],
            Compression::Zstd { level: 1 },
        )
        .unwrap();
        c.sync().unwrap();
        let m = c.append(FrameKind::Manifest, b"manifest", NONE).unwrap();
        c.commit(Commit {
            manifest: m,
            wal_start: w.offset,
            fence_token: 0,
        })
        .unwrap();
        let committed = c.file().bytes();
        c.append(FrameKind::WalChunk, b"put 2", NONE).unwrap();
        c.append(FrameKind::from_u8(77), b"unknown kind", NONE)
            .unwrap();
        c.sync().unwrap();
        vec![new, committed, c.file().bytes()]
    })
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Fixes the CRC of each frame along the chain from the first frame, while lengths stay in bounds.
fn fix_frame_crcs(b: &mut [u8]) {
    let mut at = FIRST_FRAME_OFFSET as usize;
    while let Some(payload_len) = u64_at(b, at + 8) {
        let Some(padded) = usize::try_from(payload_len)
            .ok()
            .and_then(|l| l.checked_next_multiple_of(8))
        else {
            return;
        };
        let Some(crc_at) = (at + 24).checked_add(padded).filter(|&c| c + 8 <= b.len()) else {
            return;
        };
        let crc = crc32c(&b[at + 4..crc_at]);
        b[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
        at = crc_at + 8;
        if at + MIN_FRAME_LEN > b.len() {
            return;
        }
    }
}

fn fix_slot_crc(b: &mut [u8], at: usize) {
    if let Some(slot) = b.get_mut(at..at + SLOT_LEN) {
        let crc = crc32c(&slot[..SLOT_LEN - 4]);
        slot[SLOT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    }
}

fn check_open(c: &Container<SimFile>, len: u64) {
    let sb = *c.superblock();
    assert!(sb.log_end <= c.end() && c.end() <= len);
    let mut next = sb.wal_start;
    for f in c.frames() {
        assert_eq!(f.offset, next, "frames are contiguous from wal_start");
        next = f.end();
        match c.read_frame(f.offset, MAX) {
            Ok(read) => assert_eq!(read.frame, *f),
            Err(err) => assert!(
                err.is_corruption()
                    || matches!(err, Error::Format(persia_format::Error::OutOfMemory { .. })),
                "a recovered frame fails to read only in its payload: {err:?}"
            ),
        }
    }
    assert_eq!(next, c.end(), "the frames end at end()");
    let n = c.frames().len() as u64;
    if let Some(first) = c.frames().first() {
        assert_eq!(first.frame_seq + n, c.next_frame_seq());
    }
    if sb.seq > 0 {
        let m = c.read_frame(sb.manifest_off, MAX);
        assert!(
            m.as_ref()
                .map_or(true, |m| m.frame.kind == FrameKind::Manifest),
            "{m:?}"
        );
    }
}

fn check_error(err: &Error, mode: Mode) {
    let ok = match err {
        Error::Format(e) => {
            e.is_corruption() || matches!(e, persia_format::Error::Unsupported { .. })
        }
        Error::CorruptTail { .. } => mode == Mode::ReadWrite,
        Error::OutOfMemory { .. } => true,
        Error::ReadOnly {
            reason: ReadOnlyReason::UnknownRocompatFlags { .. },
        } => mode == Mode::ReadWrite,
        _ => false,
    };
    assert!(ok, "unexpected {mode:?} open error: {err:?}");
}

fuzz_target!(|input: &[u8]| {
    let [mode, base, o0, o1, patch @ ..] = input else {
        return;
    };
    let bases = bases();
    let mut data = bases[usize::from(*base) % bases.len()].clone();
    let offset = usize::from(u16::from_le_bytes([*o0, *o1]));
    if mode & 1 != 0 {
        data.truncate(offset);
    } else {
        let end = offset + patch.len();
        if data.len() < end {
            data.resize(end, 0);
        }
        data[offset..end].copy_from_slice(patch);
    }
    if mode & 16 != 0 {
        fix_frame_crcs(&mut data);
    }
    if mode & 2 != 0 && data.len() >= 4096 {
        let mut crc = Crc32c::new();
        crc.update(&data[..12]);
        crc.update(&data[16..4096]);
        data[12..16].copy_from_slice(&crc.finish().to_le_bytes());
    }
    if mode & 4 != 0 {
        fix_slot_crc(&mut data, 4096);
    }
    if mode & 8 != 0 {
        fix_slot_crc(&mut data, 8192);
    }
    let len = data.len() as u64;

    let ro_file = SimFile::from_bytes(data.clone());
    match Container::open_on(ro_file.clone(), Mode::ReadOnly) {
        Ok(c) => check_open(&c, len),
        Err(err) => check_error(&err, Mode::ReadOnly),
    }
    assert_eq!(ro_file.bytes(), data, "a read-only open never writes");
    assert_eq!(ro_file.mutations(), 0);

    let rw_file = SimFile::from_bytes(data.clone());
    match Container::open_on(rw_file.clone(), Mode::ReadWrite) {
        Ok(mut c) => {
            check_open(&c, len);
            assert_eq!(
                rw_file.file_len().unwrap(),
                c.end(),
                "a writer truncates a torn tail"
            );
            assert_eq!(rw_file.durable_bytes(), rw_file.bytes());
            let seq = c.superblock().seq;
            let wal_start = c.superblock().wal_start;
            if c.next_frame_seq() < u64::MAX - 1 && seq < u64::MAX {
                let m = c.append(FrameKind::Manifest, b"next", NONE).unwrap();
                c.commit(Commit {
                    manifest: m,
                    wal_start,
                    fence_token: 0,
                })
                .unwrap();
                let r = Container::open_on(SimFile::from_bytes(rw_file.bytes()), Mode::ReadOnly)
                    .unwrap();
                assert_eq!(r.superblock().seq, seq + 1);
                assert_eq!(
                    r.read_frame(r.superblock().manifest_off, MAX)
                        .unwrap()
                        .payload,
                    b"next"
                );
            }
        }
        Err(err) => {
            check_error(&err, Mode::ReadWrite);
            assert_eq!(rw_file.bytes(), data, "a failed open changes nothing");
        }
    }
});
