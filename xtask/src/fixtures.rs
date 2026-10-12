//! `cargo xtask fixtures`: write the golden container fixtures in `tests/fixtures/format/` (SPEC §15).
//!
//! Like generated code (ADR-0003), the fixtures are produced here, committed, and `--check` fails if regenerating
//! them would change a byte. `persia-format`'s `tests/fixtures.rs` opens every file and checks what it must hold,
//! so a reader change that breaks an old container is caught even when the writer changed along with it.
//!
//! Every fixture is built from fixed inputs, so the bytes depend only on the format code and on zstd's output for
//! the one compressed frame. Frame payloads are opaque: the WAL, segment and manifest formats do not exist yet, so
//! only the container layer (header, superblock slots, frames; SPEC §4.2–4.4) may read these files.
//!
//! A format version is frozen once released (SPEC §15); then its fixtures must stay byte-for-byte as they are and
//! a new version gets its own directory.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use persia_format::Writer;
use persia_format::block::Compression;
use persia_format::frame::{FrameKind, encode_frame, frame_len};
use persia_format::header::Header;
use persia_format::superblock::{FIRST_FRAME_OFFSET, SLOT_LEN, Superblock};

const COMMAND: &str = "cargo xtask fixtures";
/// Where the fixtures live, relative to the repository root.
pub(crate) const DIR: &str = "tests/fixtures/format";
/// The `db_uuid` of every fixture.
const DB_UUID: [u8; 16] = *b"persia-fixtures!";
/// Fixed so the compressed frame is reproducible (the default level could change with zstd).
const ZSTD_LEVEL: i32 = 3;
/// A kind byte no build assigns yet: readers must skip the frame (SPEC §4.4).
const UNKNOWN_KIND: u8 = 0x7F;

/// One fixture file: its name in [`DIR`] and its bytes.
#[derive(Debug)]
pub(crate) struct Fixture {
    pub(crate) name: &'static str,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) fn run(root: &Path, args: &[String]) -> Result<bool> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => bail!("usage: {COMMAND} [--check]"),
    };
    let dir = root.join(DIR);
    let fixtures = generate()?;
    if check {
        return check_dir(&dir, &fixtures);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {DIR}"))?;
    for f in &fixtures {
        std::fs::write(dir.join(f.name), &f.bytes)
            .with_context(|| format!("writing {DIR}/{}", f.name))?;
    }
    println!("wrote {} fixtures to {DIR}", fixtures.len());
    Ok(true)
}

/// Whether `dir` holds exactly `fixtures`, byte for byte. Reports every difference.
fn check_dir(dir: &Path, fixtures: &[Fixture]) -> Result<bool> {
    let mut ok = true;
    for f in fixtures {
        match std::fs::read(dir.join(f.name)) {
            Ok(bytes) if bytes == f.bytes => {}
            Ok(_) => {
                eprintln!("{DIR}/{} differs from what the generator writes", f.name);
                ok = false;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("{DIR}/{} is missing", f.name);
                ok = false;
            }
            Err(e) => return Err(e).with_context(|| format!("reading {DIR}/{}", f.name)),
        }
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {DIR}"))? {
        let name = entry?.file_name();
        if !fixtures.iter().any(|f| name == f.name) {
            eprintln!(
                "{DIR}/{} is not written by the generator",
                name.to_string_lossy()
            );
            ok = false;
        }
    }
    if !ok {
        eprintln!(
            "run `{COMMAND}` and commit the result (never for a released format version, SPEC §15)"
        );
    }
    Ok(ok)
}

/// Every fixture, in a fixed order.
pub(crate) fn generate() -> Result<Vec<Fixture>> {
    let header = Header::new(DB_UUID);
    let created = Superblock {
        log_end: FIRST_FRAME_OFFSET,
        wal_start: FIRST_FRAME_OFFSET,
        ..Superblock::default()
    };
    let log = committed_log()?;

    let mut torn = log.bytes.clone();
    torn.extend_from_slice(&torn_append()?);

    // A crash while commit 2 wrote slot A: only the first half of the page reached the disk, so its CRC fails and
    // slot B (commit 1) stays active (SPEC §4.3, §4.5).
    let next = Superblock {
        seq: 2,
        ..log.superblock
    };
    let mut torn_slot = encode_slot(&next)?;
    let old_slot = encode_slot(&created)?;
    let half = SLOT_LEN / 2;
    torn_slot.truncate(half);
    torn_slot.extend_from_slice(old_slot.get(half..).context("slot shorter than SLOT_LEN")?);

    Ok(vec![
        Fixture {
            name: "empty.persia",
            bytes: prefix(&header, &old_slot, &old_slot)?,
        },
        Fixture {
            name: "frames.persia",
            bytes: [
                prefix(&header, &old_slot, &encode_slot(&log.superblock)?)?,
                log.bytes.clone(),
            ]
            .concat(),
        },
        Fixture {
            name: "torn-tail.persia",
            bytes: [
                prefix(&header, &old_slot, &encode_slot(&log.superblock)?)?,
                torn,
            ]
            .concat(),
        },
        Fixture {
            name: "torn-slot.persia",
            bytes: [
                prefix(&header, &torn_slot, &encode_slot(&log.superblock)?)?,
                log.bytes,
            ]
            .concat(),
        },
    ])
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

    let page = u64::from(Header::new(DB_UUID).page_size);
    let header_len = 24;
    let min_frame = frame_len(0).context("frame length overflow")?;
    // The Padding frame must end where the Segment frame header starts, `header_len` before a page boundary.
    let mut target = w.offset().next_multiple_of(page) - header_len;
    if target - w.offset() < min_frame {
        target += page;
    }
    let pad_payload = usize::try_from(target - w.offset() - min_frame)?;
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
        },
    })
}

/// A WAL chunk whose append was cut short by a crash: its header claims more payload than the file holds, so a
/// scan ends with a torn tail where it starts (SPEC §4.4).
fn torn_append() -> Result<Vec<u8>> {
    let mut w = Writer::new();
    encode_frame(
        &mut w,
        FrameKind::WalChunk,
        8,
        &[0xA5; 256],
        Compression::None,
    )?;
    let mut bytes = w.into_inner();
    bytes.truncate(100);
    Ok(bytes)
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
            check_dir(&root.join(DIR), &generate().unwrap()).unwrap(),
            "run `{COMMAND}`"
        );
    }
}
