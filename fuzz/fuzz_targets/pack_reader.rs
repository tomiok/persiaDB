//! Fuzz the blob pack reader (ROADMAP 9.2.5; SPEC §9.3, §17, ADR-0049): opening a pack file of arbitrary bytes and
//! reading every blob it indexes never panics, a read-write open's recovery is idempotent, and a recovered pack takes
//! a new blob and reads it back. The checksum scan `persia verify` runs over it (ROADMAP 9.5.3) never panics, its counts
//! agree with the index, and a recovered pack scans the same after another open.
//!
//! The first input byte picks the mode: 0, the rest is the whole file; with bit 3 set, the rest is the frames after a
//! valid pack prefix, each frame found renumbered and resealed, committed by the slot (bit 1) or left as the tail a
//! read-write open rolls forward, so arbitrary frames reach the open's scan and recovery (ROADMAP 9.2.14); otherwise
//! the rest is a list of 3-byte edits (`offset u16`, `value u8`) applied to a valid two-blob pack, whose slot and frame
//! CRCs are then recomputed when the mode is even, so mutations reach the field rules instead of stopping at a
//! checksum.
#![no_main]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use persia_blob::pack::PackScan;
use persia_blob::pack::layout::{PackSuperblock, new_pack_prefix};
use persia_blob::{
    BindOutcome, BlobManifest, BlobStore, ChunkSize, Error, PackMode, PackStore, UploadId,
    blob_id_of, read_range,
};
use persia_format::BlobRef;
use persia_format::checksum::crc32c;
use persia_format::frame::FRAME_MAGIC;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Container, ContainerFile};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const PACK: &str = "/app.persia.blobs";

fn manifest_of(data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
    let crcs = data.chunks(chunk as usize).map(crc32c).collect();
    BlobManifest::new(
        blob_id_of(data),
        data.len() as u64,
        ChunkSize::from_encoding(chunk).expect("chunk size"),
        UploadId::new(upload),
        crcs,
    )
    .expect("manifest")
}

/// The read-write container whose writer proof opens the pack read-write (ROADMAP 9.2.7): one for the whole run, so
/// it outlives every pack opened with its proof.
fn writer() -> &'static Container<SimFile> {
    static WRITER: OnceLock<Container<SimFile>> = OnceLock::new();
    WRITER.get_or_init(|| Container::create_on(SimFile::new(), [1; 16]).expect("container"))
}

fn open(fs: &SimFs, mode: PackMode) -> persia_blob::Result<PackStore<SimFsFile>> {
    let vfs = Arc::new(fs.clone());
    match mode {
        PackMode::ReadWrite => {
            let proof = writer().writer_proof().expect("a read-write container");
            PackStore::open_writer_in(vfs, PACK, &proof)
        }
        PackMode::ReadOnly => PackStore::open_in(vfs, PACK, [1; 16]),
    }
}

/// Stages and binds `data`; the manifest and what `bind` answered.
fn put(
    store: &PackStore<SimFsFile>,
    data: &[u8],
    chunk: u32,
    upload: u128,
) -> persia_blob::Result<(BlobManifest, BindOutcome)> {
    let m = manifest_of(data, chunk, upload);
    for (i, c) in data.chunks(chunk as usize).enumerate() {
        store.stage_chunk(m.upload_id(), i as u32, c)?;
    }
    let outcome = store.bind(&m)?;
    Ok((m, outcome))
}

/// A valid pack, and where its frames are.
type Template = (Vec<u8>, Vec<(usize, usize)>);

/// Where the frames of `bytes` are (offset, total length), from 12288 to the last whole header.
fn frames_of(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut frames = Vec::new();
    let mut at = 12288;
    while at + 24 <= bytes.len() {
        let len = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().expect("8 bytes"));
        // A length no frame in the file can have ends the walk (no overflow on an arbitrary one).
        let Some(total) = usize::try_from(len)
            .ok()
            .and_then(|len| len.checked_next_multiple_of(8))
            .and_then(|len| len.checked_add(32))
            .filter(|&total| total <= bytes.len() - at)
        else {
            break;
        };
        frames.push((at, total));
        at += total;
    }
    frames
}

/// A valid pack with two committed blobs.
fn template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let fs = SimFs::new();
        let store = open(&fs, PackMode::ReadWrite).expect("open");
        assert_eq!(
            put(&store, b"0123456789abcdef", 8, 1).expect("put").1,
            BindOutcome::Bound
        );
        assert_eq!(
            put(&store, b"second", 4, 2).expect("put").1,
            BindOutcome::Bound
        );
        drop(store);
        let bytes = fs.read(Path::new(PACK)).expect("pack");
        let frames = frames_of(&bytes);
        (bytes, frames)
    })
}

/// A valid pack with one committed blob and, after `log_end`, a staged chunk and the whole of a bind cut at its slot
/// write: what a read-write open rolls forward (ADR-0049 item 9).
fn cut_template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let fs = SimFs::new();
        let store = open(&fs, PackMode::ReadWrite).expect("open");
        assert_eq!(
            put(&store, b"0123456789abcdef", 8, 1).expect("put").1,
            BindOutcome::Bound
        );
        store
            .stage_chunk(UploadId::new(9), 0, b"staged")
            .expect("stage");
        let m = manifest_of(b"cut bind", 4, 2);
        for (i, c) in b"cut bind".chunks(4).enumerate() {
            store
                .stage_chunk(m.upload_id(), i as u32, c)
                .expect("stage");
        }
        // A bind's calls: index append, sync, slot write, slot sync: fail the slot write.
        fs.set_fault(Some(SimFault::FailAt(fs.mutations() + 2)));
        assert!(store.bind(&m).is_err());
        drop(store);
        let bytes = fs
            .crash(CrashLoss::Nothing)
            .read(Path::new(PACK))
            .expect("pack");
        let frames = frames_of(&bytes);
        (bytes, frames)
    })
}

/// A new pack's header and slots, then `frames` padded to 8 (mode bit 3, ROADMAP 9.2.14): every frame found by its
/// length gets the frame magic, `frame_seq` 1, 2, ... and a valid CRC, so arbitrary kinds, lengths and payloads reach
/// the open's frame rules. With `committed`, slot A commits them when the last one ends the file (`blob_count` the
/// frames of kind `BlobIndex`), so the open scans them; otherwise a read-write open's roll-forward reads them.
fn arbitrary_frames(frames: &[u8], committed: bool) -> Vec<u8> {
    let mut bytes = new_pack_prefix([1; 16], 7).expect("a new pack's prefix");
    bytes.extend_from_slice(frames);
    bytes.resize(bytes.len().next_multiple_of(8), 0);
    // The walk reads only the lengths, which neither the magic nor the numbering changes.
    let found = frames_of(&bytes);
    for (seq, &(at, _)) in (1_u64..).zip(&found) {
        bytes[at..at + 4].copy_from_slice(&FRAME_MAGIC);
        bytes[at + 16..at + 24].copy_from_slice(&seq.to_le_bytes());
    }
    reseal(&mut bytes, &found);
    if committed
        && let Some(&(last, total)) = found.last()
        && last + total == bytes.len()
    {
        let indexes = found.iter().filter(|&&(at, _)| bytes[at + 4] == 7).count();
        let sb = PackSuperblock {
            seq: 1,
            index_off: last as u64,
            index_len: total as u64,
            log_end: bytes.len() as u64,
            blob_count: indexes as u64,
            next_frame_seq: found.len() as u64 + 1,
            ..PackSuperblock::new_pack(7)
        };
        bytes[4096..8192].copy_from_slice(&sb.encode());
    }
    bytes
}

/// Recomputes both slots' and every template frame's CRC32C.
fn reseal(bytes: &mut [u8], frames: &[(usize, usize)]) {
    for slot in [4096, 8192] {
        let crc = crc32c(&bytes[slot..slot + 4092]);
        bytes[slot + 4092..slot + 4096].copy_from_slice(&crc.to_le_bytes());
    }
    for &(at, total) in frames {
        let end = at + total - 8;
        let crc = crc32c(&bytes[at + 4..end]);
        bytes[end..end + 4].copy_from_slice(&crc.to_le_bytes());
    }
}

fn reference(m: &BlobManifest) -> Option<BlobRef> {
    BlobRef::new(
        m.blob_id(),
        m.size(),
        "a/b",
        m.chunk_size().get(),
        BTreeMap::new(),
    )
    .ok()
}

fn read_every_blob(store: &PackStore<SimFsFile>) {
    for id in store.blob_ids() {
        if let Ok(Some(m)) = store.manifest(id) {
            assert_eq!(m.blob_id(), id, "a manifest is checked against its id");
            if let Some(r) = reference(&m) {
                // A read fails only on damaged bytes, a newer format or an I/O error, never on a caller-side rule.
                if let Err(e) = read_range(store, &r, 0..m.size().min(1 << 20)) {
                    assert!(
                        e.is_corruption()
                            || matches!(e, Error::PackUnsupported(_) | Error::PackIo { .. }),
                        "unexpected read error: {e:?}"
                    );
                }
            }
        }
    }
}

/// The checksum scan `persia verify` runs (ROADMAP 9.5.3, SPEC §9.5): it never panics, fails only on damaged bytes, a
/// newer format or an I/O error, and a clean scan counts what the open indexed: every committed frame is a chunk or an
/// index, each committed index binds one blob, orphans are chunks, and the frames span `12288..log_end`. Its outcome:
/// the counts, or the offset of the damage it reported.
fn scan(store: &PackStore<SimFsFile>) -> Result<Option<PackScan>, Option<u64>> {
    match store.scan_checksums() {
        Ok(Some(s)) => {
            assert_eq!(s.frames, s.chunks + s.indexes, "{s:?}");
            assert!(s.orphan_chunks <= s.chunks, "{s:?}");
            assert_eq!(
                s.indexes,
                store.blob_ids().len() as u64,
                "each committed index binds one blob: {s:?}"
            );
            let log_end = store.superblock().expect("a scanned pack").log_end;
            assert_eq!(s.bytes, log_end - 12288, "{s:?}");
            Ok(Some(s))
        }
        Ok(None) => Ok(None),
        Err(e) => {
            assert!(
                e.is_corruption() || matches!(e, Error::PackUnsupported(_) | Error::PackIo { .. }),
                "unexpected scan error: {e:?}"
            );
            Err(e.corrupt_offset())
        }
    }
}
fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    // mode 0: the rest is the whole file. Bit 3: the rest is frames after a valid prefix, committed if bit 1 is set.
    // Otherwise bit 0 clear reseals the CRCs, bit 1 picks the template with a cut bind after log_end, bit 2 resizes the
    // file by the first two bytes of the rest (append or truncate).
    let file = if mode == 0 {
        rest.to_vec()
    } else if mode & 8 != 0 {
        arbitrary_frames(rest, mode & 2 != 0)
    } else {
        let (template, frames) = if mode & 2 == 0 {
            template()
        } else {
            cut_template()
        };
        let mut bytes = template.clone();
        let mut edits = rest;
        if mode & 4 != 0
            && let Some((len, tail)) = edits.split_first_chunk::<2>()
        {
            let len = u16::from_le_bytes(*len);
            let n = usize::from(len & 0x7fff);
            if len & 0x8000 != 0 {
                bytes.truncate(bytes.len().saturating_sub(n).max(1));
            } else {
                // Zeros: what a length that reached the disk before its data reads as (SPEC §4.4).
                bytes.resize(bytes.len() + n, 0);
            }
            edits = tail;
        }
        for edit in edits.as_chunks::<3>().0 {
            let at = usize::from(u16::from_le_bytes([edit[0], edit[1]])) % bytes.len();
            bytes[at] = edit[2];
        }
        if mode & 1 == 0 && bytes.len() >= template.len() {
            reseal(&mut bytes, frames);
        }
        bytes
    };
    let fs = SimFs::new();
    let f = fs.create_new(Path::new(PACK)).expect("create");
    f.write_all_at(&file, 0).expect("write");
    if let Ok(store) = open(&fs, PackMode::ReadOnly) {
        read_every_blob(&store);
        // The checksum scan `persia verify` runs (ROADMAP 9.5.3) reads every committed frame.
        let _ = scan(&store);
        let _ = store.refresh();
    }
    let Ok(store) = open(&fs, PackMode::ReadWrite) else {
        return;
    };
    read_every_blob(&store);
    let scanned = scan(&store);
    let ids: std::collections::BTreeSet<_> = store.blob_ids().into_iter().collect();
    drop(store);
    // Recovery is idempotent: the recovered pack opens to the same index, with nothing left to roll forward or cut.
    let store = open(&fs, PackMode::ReadWrite).expect("a recovered pack reopens");
    assert_eq!(
        store.recovery().rolled_forward,
        0,
        "a second open rolls nothing forward"
    );
    assert_eq!(
        store.recovery().truncated_from,
        None,
        "a second open truncates nothing"
    );
    assert_eq!(
        store
            .blob_ids()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        ids
    );
    // A second open recovers nothing more, so it scans the same frames to the same outcome.
    assert_eq!(
        scan(&store),
        scanned,
        "the scan of a recovered pack is stable"
    );
    // And takes a new blob, which reads back (a damaged pack may already hold its id, or claim its upload).
    if let Ok((m, BindOutcome::Bound)) = put(&store, b"fuzzed in", 4, u128::MAX - 1)
        && let Some(r) = reference(&m)
    {
        assert_eq!(
            read_range(&store, &r, 0..m.size()).expect("a new blob reads back"),
            b"fuzzed in"
        );
    }
});
