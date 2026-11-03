//! `persia inspect container` (ROADMAP 6.6.10): what recovery and replay will read from a container (SPEC §4.3,
//! §4.4, §8.2, §8.6), read-only and without the writer lock, so a live writer may have it open.
//!
//! The output is for people: `key  value` lines for the active superblock slot and what the read-only recovery found,
//! then one row per frame from `wal_start` to the end of the valid frames: its offset, length, `frame_seq`, kind, and
//! whether it lies after `log_end` (not committed: WAL frames synced since the last commit, or a commit cut before its
//! slot write, ROADMAP 6.6.9). A Manifest frame shows the `seq` it holds, and which one the slot names. With `--id`,
//! then every WAL record of those frames that puts or deletes that document id.
//!
//! A frame the container accepted (its CRC holds) whose payload does not decode is reported on its row, and the rest
//! is still listed: this is a debugging aid, and `persia verify` is the check that fails on such damage.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use persia_engine::document::Document;
use persia_engine::manifest::{MAX_MANIFEST_LEN, Manifest};
use persia_engine::wal::{MAX_CHUNK_LEN, WalOp, WalRecord, decode_chunk};
use persia_format::frame::{FrameKind, ScanEnd};
use persia_storage::container::{Container, FrameRef, Mode};

/// Opens the container at `path` read-only and writes its superblock, frames and (with `id`) the records naming `id`.
pub fn inspect(out: &mut dyn Write, path: &Path, id: Option<&str>) -> Result<()> {
    if !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    let c = Container::open(path, Mode::ReadOnly)
        .with_context(|| format!("opening {}", path.display()))?;
    write_superblock(out, &c)?;
    write_frames(out, &c)?;
    if let Some(id) = id {
        write_records(out, &c, id)?;
    }
    Ok(())
}

fn write_superblock(out: &mut dyn Write, c: &Container) -> Result<()> {
    let sb = c.superblock();
    let recovery = c.recovery();
    let tail = match &recovery.scan_end {
        ScanEnd::Clean { offset } => format!("clean at {offset}"),
        ScanEnd::TornTail { offset, error } => format!("torn at {offset}: {error}"),
        ScanEnd::Corrupt { offset, .. } => {
            format!("corrupt at {offset} (a read-write open quarantines it)")
        }
        ScanEnd::Unsupported { offset, error } => format!("unsupported frame at {offset}: {error}"),
    };
    let rows = [
        ("slot", format!("{:?}", c.active_slot())),
        ("seq", sb.seq.to_string()),
        ("wal_start", sb.wal_start.to_string()),
        ("log_end", sb.log_end.to_string()),
        (
            "manifest",
            format!("offset {} len {}", sb.manifest_off, sb.manifest_len),
        ),
        ("next_frame_seq", sb.next_frame_seq.to_string()),
        ("fence_token", sb.fence_token.to_string()),
        ("end", c.end().to_string()),
        ("tail", tail),
        ("stale_tail", recovery.stale_tail.to_string()),
    ];
    for (key, value) in rows {
        writeln!(out, "{key:<18}{value}")?;
    }
    Ok(())
}

fn write_frames(out: &mut dyn Write, c: &Container) -> Result<()> {
    let sb = c.superblock();
    writeln!(
        out,
        "\n{:>12} {:>10} {:>10}  {:<10} {:<11} note",
        "offset", "len", "frame_seq", "kind", "state"
    )?;
    for f in c.frames() {
        let state = if f.offset >= sb.log_end {
            "uncommitted"
        } else {
            "committed"
        };
        let note = if f.kind == FrameKind::Manifest {
            let committed = if f.offset == sb.manifest_off {
                ", the slot's"
            } else {
                ""
            };
            match manifest_seq(c, f) {
                Ok(seq) => format!("seq {seq}{committed}"),
                Err(e) => format!("unreadable{committed}: {e:#}"),
            }
        } else {
            String::new()
        };
        let row = format!(
            "{:>12} {:>10} {:>10}  {:<10} {:<11} {note}",
            f.offset,
            f.len,
            f.frame_seq,
            kind_name(f.kind),
            state
        );
        writeln!(out, "{}", row.trim_end())?;
    }
    Ok(())
}

/// The `seq` of the Manifest frame `f`.
fn manifest_seq(c: &Container, f: &FrameRef) -> Result<u64> {
    let read = c.read_frame(f.offset, MAX_MANIFEST_LEN)?;
    Ok(Manifest::decode(&read.payload)?.seq())
}

/// Every put or delete of `id` in the WAL frames from `wal_start` on, in log order.
fn write_records(out: &mut dyn Write, c: &Container, id: &str) -> Result<()> {
    writeln!(
        out,
        "\nrecords of id {id:?}\n{:>12} {:>7}  {:<11} {:<6} collection",
        "frame", "record", "state", "op"
    )?;
    let log_end = c.superblock().log_end;
    for f in c.frames().iter().filter(|f| f.kind == FrameKind::WalChunk) {
        let state = if f.offset >= log_end {
            "uncommitted"
        } else {
            "committed"
        };
        let unreadable = |out: &mut dyn Write, e: anyhow::Error| -> Result<()> {
            writeln!(
                out,
                "{:>12} {:>7}  {state:<11} unreadable: {e:#}",
                f.offset, "-"
            )?;
            Ok(())
        };
        let payload = match c.read_frame(f.offset, MAX_CHUNK_LEN) {
            Ok(read) => read.payload,
            Err(e) => {
                unreadable(out, e.into())?;
                continue;
            }
        };
        let records = match decode_chunk(&payload) {
            Ok(records) => records,
            Err(e) => {
                unreadable(out, e.into())?;
                continue;
            }
        };
        for (n, record) in records.enumerate() {
            let record = match record {
                Ok(record) => record,
                Err(e) => {
                    // The iterator ends after its first error.
                    unreadable(out, e.into())?;
                    break;
                }
            };
            if names(&record, id) {
                let op = match record.op() {
                    WalOp::Put => "put",
                    WalOp::Delete => "delete",
                };
                writeln!(
                    out,
                    "{:>12} {n:>7}  {state:<11} {op:<6} {:?}",
                    f.offset,
                    record.collection()
                )?;
            }
        }
    }
    Ok(())
}

/// Whether `record` puts or deletes the document `id`. A put whose document's id cannot be read names none; replay
/// would fail on it, and `persia verify` reports it.
fn names(record: &WalRecord<'_>, id: &str) -> bool {
    match record.op() {
        WalOp::Delete => record.deleted_id() == Some(id),
        WalOp::Put => Document::encoded_id(record.payload()).is_ok_and(|d| d == id),
    }
}

fn kind_name(kind: FrameKind) -> String {
    match kind {
        FrameKind::WalChunk => "wal".to_owned(),
        FrameKind::Segment => "segment".to_owned(),
        FrameKind::Manifest => "manifest".to_owned(),
        FrameKind::Padding => "padding".to_owned(),
        FrameKind::TombstoneSidecar => "tombstones".to_owned(),
        FrameKind::Unknown(k) => format!("{:#04x}", k.get()),
    }
}
