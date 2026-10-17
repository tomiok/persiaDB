//! `cargo xtask fixtures`: write the golden container fixtures of the current format version (SPEC §15).
//!
//! Like generated code (ADR-0003), the fixtures are produced here, committed, and `--check` fails if regenerating
//! them would change a byte. `persia-format`'s `tests/fixtures.rs` opens every file and checks what it must hold,
//! so a reader change that breaks an old container is caught even when the writer changed along with it.
//!
//! Each format version has its own directory, `tests/fixtures/format/v<major>.<minor>/`. This command writes and
//! checks only the directory of the version this build writes ([`FORMAT_MAJOR`].[`FORMAT_MINOR`]). Once that
//! version is released it is frozen (SPEC §15): its directory is never regenerated, the version bump moves the
//! generator to a new directory, and the reader test keeps opening the old one.
//!
//! Every fixture is built from fixed inputs, so the bytes depend only on the format code and on zstd's output for
//! the one compressed frame: a zstd upgrade may change that frame, which `--check` reports. That is harmless before
//! release (regenerate) and impossible after it (the directory is no longer checked here). Frame payloads are
//! opaque: the WAL, segment and manifest formats do not exist yet, so only the container layer (header, superblock
//! slots, frames; SPEC §4.2–4.4) may read these files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use persia_format::Writer;
use persia_format::block::Compression;
use persia_format::frame::{FRAME_HEADER_LEN, FrameKind, MIN_FRAME_LEN, encode_frame};
use persia_format::header::{FORMAT_MAJOR, FORMAT_MINOR, Header};
use persia_format::superblock::{FIRST_FRAME_OFFSET, SLOT_LEN, Superblock};

const COMMAND: &str = "cargo xtask fixtures";
/// The `db_uuid` of every fixture.
const DB_UUID: [u8; 16] = *b"persia-fixtures!";
/// Fixed so the compressed frame is reproducible (the default level could change with zstd).
const ZSTD_LEVEL: i32 = 3;
/// A kind byte no build assigns yet: readers must skip the frame (SPEC §4.4).
const UNKNOWN_KIND: u8 = 0x7F;

/// One fixture file: its name in [`dir`] and its bytes.
#[derive(Debug)]
pub(crate) struct Fixture {
    pub(crate) name: &'static str,
    pub(crate) bytes: Vec<u8>,
}

/// The fixture directory of the format version this build writes, relative to the repository root.
pub(crate) fn dir() -> PathBuf {
    PathBuf::from(format!(
        "tests/fixtures/format/v{FORMAT_MAJOR}.{FORMAT_MINOR}"
    ))
}

pub(crate) fn run(root: &Path, args: &[String]) -> Result<bool> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => bail!("usage: {COMMAND} [--check]"),
    };
    let fixtures = generate()?;
    if check {
        return check_dir(root, &fixtures);
    }
    let rel = dir();
    let dir = root.join(&rel);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", rel.display()))?;
    for f in &fixtures {
        std::fs::write(dir.join(f.name), &f.bytes)
            .with_context(|| format!("writing {}/{}", rel.display(), f.name))?;
    }
    println!("wrote {} fixtures to {}", fixtures.len(), rel.display());
    Ok(true)
}

/// Whether the current version's directory holds exactly `fixtures`, byte for byte. Reports every difference.
fn check_dir(root: &Path, fixtures: &[Fixture]) -> Result<bool> {
    let rel = dir();
    let dir = root.join(&rel);
    let rel = rel.display();
    let mut ok = true;
    for f in fixtures {
        match std::fs::read(dir.join(f.name)) {
            Ok(bytes) if bytes == f.bytes => {}
            Ok(_) => {
                eprintln!("{rel}/{} differs from what the generator writes", f.name);
                ok = false;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("{rel}/{} is missing", f.name);
                ok = false;
            }
            Err(e) => return Err(e).with_context(|| format!("reading {rel}/{}", f.name)),
        }
    }
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let name = entry?.file_name();
                if !fixtures.iter().any(|f| name == f.name) {
                    eprintln!(
                        "{rel}/{} is not written by the generator",
                        name.to_string_lossy()
                    );
                    ok = false;
                }
            }
        }
        // Every fixture was already reported missing.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("listing {rel}")),
    }
    if !ok {
        eprintln!("run `{COMMAND}` and commit the result");
    }
    Ok(ok)
}

/// Every fixture, in a fixed order.
pub(crate) fn generate() -> Result<Vec<Fixture>> {
    let header = Header::new(DB_UUID);
    let created = encode_slot(&Superblock {
        log_end: FIRST_FRAME_OFFSET,
        wal_start: FIRST_FRAME_OFFSET,
        next_frame_seq: 1,
        ..Superblock::default()
    })?;
    let log = committed_log()?;
    let commit_1 = encode_slot(&log.superblock)?;
    let committed = prefix(&header, &created, &commit_1)?;
    let with_tail = |tail: &[u8]| [committed.as_slice(), &log.bytes, tail].concat();

    // An append cut short: the frame header claims more payload than the file holds.
    let mut runs_past_end = frame(FrameKind::WalChunk, 8, &[0xA5; 256], Compression::None)?;
    runs_past_end.truncate(100);

    // An append whose file length grew but whose data never reached the disk: after the header, zeros to the end.
    let mut zero_filled = frame(FrameKind::WalChunk, 8, &[0xA5; 256], Compression::None)?;
    zero_filled
        .get_mut(FRAME_HEADER_LEN..)
        .context("frame shorter than its header")?
        .fill(0);

    // A damaged frame followed by a valid one: corruption, not a torn tail (SPEC §4.4).
    let mut damaged = frame(FrameKind::WalChunk, 8, b"wal: put doc 3", Compression::None)?;
    *damaged
        .get_mut(FRAME_HEADER_LEN)
        .context("frame shorter than its header")? ^= 0x01;
    damaged.extend(frame(
        FrameKind::WalChunk,
        9,
        b"wal: put doc 4",
        Compression::None,
    )?);

    Ok(vec![
        Fixture {
            name: "empty.persia",
            bytes: prefix(&header, &created, &created)?,
        },
        Fixture {
            name: "frames.persia",
            bytes: with_tail(&[]),
        },
        Fixture {
            name: "torn-tail.persia",
            bytes: with_tail(&runs_past_end),
        },
        Fixture {
            name: "torn-zeros.persia",
            bytes: with_tail(&zero_filled),
        },
        Fixture {
            name: "corrupt-frame.persia",
            bytes: with_tail(&damaged),
        },
        Fixture {
            name: "torn-slot.persia",
            bytes: torn_slot(&header, &created, &commit_1, &log)?,
        },
    ])
}

/// A crash during step 3 of commit 2 (SPEC §4.5): its `WalChunk` and `Manifest` frames are durable after commit 1's
/// `log_end`, but only the first half of its slot A page reached the disk, so slot A fails its CRC and slot B
/// (commit 1) stays active (SPEC §4.3). Recovery finds commit 2's frames after `log_end`.
fn torn_slot(header: &Header, created: &[u8], commit_1: &[u8], log: &Log) -> Result<Vec<u8>> {
    let mut w = Writer::with_base_offset(log.superblock.log_end);
    encode_frame(
        &mut w,
        FrameKind::WalChunk,
        8,
        b"wal: put doc 3",
        Compression::None,
    )?;
    let manifest_off = w.offset();
    encode_frame(
        &mut w,
        FrameKind::Manifest,
        9,
        b"manifest: segment frame 5, commit 2",
        Compression::None,
    )?;
    let log_end = w.offset();
    let commit_2 = Superblock {
        seq: 2,
        manifest_off,
        manifest_len: log_end - manifest_off,
        log_end,
        next_frame_seq: 10,
        ..log.superblock
    };
    let half = SLOT_LEN / 2;
    let mut slot_a = encode_slot(&commit_2)?;
    slot_a.truncate(half);
    slot_a.extend_from_slice(created.get(half..).context("slot shorter than SLOT_LEN")?);
    Ok([
        prefix(header, &slot_a, commit_1)?,
        log.bytes.clone(),
        w.into_inner(),
    ]
    .concat())
}

/// The 12288-byte prefix: header page, slot A, slot B (SPEC §4.2; ADR-0008).
fn prefix(header: &Header, slot_a: &[u8], slot_b: &[u8]) -> Result<Vec<u8>> {
    let mut w = Writer::new();
    header.encode(&mut w)?;
    w.write_bytes(slot_a);
    w.write_bytes(slot_b);
    ensure!(
        u64::try_from(w.len())? == FIRST_FRAME_OFFSET,
        "prefix is {} bytes",
        w.len()
    );
    Ok(w.into_inner())
}

fn encode_slot(sb: &Superblock) -> Result<Vec<u8>> {
    let mut w = Writer::new();
    sb.encode(&mut w)?;
    Ok(w.into_inner())
}

fn frame(kind: FrameKind, seq: u64, payload: &[u8], compression: Compression) -> Result<Vec<u8>> {
    let mut w = Writer::new();
    encode_frame(&mut w, kind, seq, payload, compression)?;
    Ok(w.into_inner())
}

/// Frames from [`FIRST_FRAME_OFFSET`] and the superblock that commits them.
struct Log {
    bytes: Vec<u8>,
    superblock: Superblock,
}

/// One commit's worth of frames, covering every kind: a consumed WAL chunk, a tombstone sidecar, a frame of an
/// unknown kind, Padding that puts the Segment payload on a page boundary (`page_size` is advisory, ADR-0008), a
/// compressed WAL chunk still needed, and the Manifest last (SPEC §4.5).
fn committed_log() -> Result<Log> {
    let mut w = Writer::with_base_offset(FIRST_FRAME_OFFSET);
    encode_frame(
        &mut w,
        FrameKind::WalChunk,
        1,
        b"wal: put doc 1",
        Compression::None,
    )?;
    encode_frame(
        &mut w,
        FrameKind::TombstoneSidecar,
        2,
        b"tombstones: doc 7",
        Compression::None,
    )?;
    encode_frame(
        &mut w,
        FrameKind::from_u8(UNKNOWN_KIND),
        3,
        b"written by a newer build",
        Compression::None,
    )?;

    // The Padding frame ends where the Segment frame starts: the first offset with room for an empty frame whose
    // header ends on a page boundary. Frames are 8-byte multiples and so is the page size, so this is exact.
    let page = u64::from(Header::new(DB_UUID).page_size);
    let header_len = u64::try_from(FRAME_HEADER_LEN)?;
    let min_frame = u64::try_from(MIN_FRAME_LEN)?;
    let start = w.offset();
    let target = (start + min_frame + header_len).next_multiple_of(page) - header_len;
    let pad_payload = usize::try_from(target - start - min_frame)?;
    encode_frame(
        &mut w,
        FrameKind::Padding,
        4,
        &vec![0; pad_payload],
        Compression::None,
    )?;
    ensure!(w.offset() == target, "padding ended at {}", w.offset());

    let segment: Vec<u8> = (0..1024u32)
        .map(|i| u8::try_from(i.wrapping_mul(31) % 251).unwrap_or_default())
        .collect();
    encode_frame(&mut w, FrameKind::Segment, 5, &segment, Compression::None)?;

    let wal_start = w.offset();
    let wal = b"wal: put doc 2 {\"title\": \"persia\"}\n".repeat(64);
    encode_frame(
        &mut w,
        FrameKind::WalChunk,
        6,
        &wal,
        Compression::Zstd { level: ZSTD_LEVEL },
    )?;

    let manifest_off = w.offset();
    encode_frame(
        &mut w,
        FrameKind::Manifest,
        7,
        b"manifest: segment frame 5",
        Compression::None,
    )?;
    let log_end = w.offset();
    Ok(Log {
        bytes: w.into_inner(),
        superblock: Superblock {
            seq: 1,
            manifest_off,
            manifest_len: log_end - manifest_off,
            log_end,
            wal_start,
            fence_token: 0,
            next_frame_seq: 8,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_eq!(a.len(), b.len());
        for (a, b) in a.iter().zip(&b) {
            assert_eq!((a.name, &a.bytes), (b.name, &b.bytes));
        }
    }

    #[test]
    fn committed_fixtures_are_current() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        assert!(
            check_dir(root, &generate().unwrap()).unwrap(),
            "run `{COMMAND}`"
        );
    }
}
