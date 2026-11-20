//! The pack's checksum scan (SPEC §9.5; ROADMAP 9.5.3; ADR-0053): what `persia verify` runs by default.
//!
//! Invariants:
//! 1. A sound pack scans clean: every committed frame counted once, orphan chunks among the chunks; no pack scans
//!    as `None`.
//! 2. A damaged byte in any committed chunk frame, bound or orphan, which an open does not read, fails the scan with
//!    a corruption naming that frame's offset; the blobs the open indexed are unchanged.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    missing_docs
)] // Tests: inputs are fixed.

mod pack_support;

use std::path::Path;
use std::sync::Arc;

use persia_blob::pack::layout::{FIRST_FRAME, chunk_frame_len};
use persia_blob::{
    BindOutcome, BlobManifest, BlobStore, ChunkSize, PackMode, PackStore, UploadId, blob_id_of,
};
use persia_format::checksum::crc32c;
use persia_storage::container::ContainerFile;
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const PACK: &str = "/db/app.persia.blobs";
const UUID: [u8; 16] = [9; 16];

fn sim() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    fs
}

fn open(fs: &SimFs, mode: PackMode) -> PackStore<SimFsFile> {
    pack_support::open_pack(Arc::new(fs.clone()), PACK, mode, UUID).unwrap()
}

fn manifest(data: &[u8], upload: u128) -> BlobManifest {
    let chunk = ChunkSize::new(ChunkSize::MIN).unwrap();
    let crcs = data.chunks(chunk.get() as usize).map(crc32c).collect();
    BlobManifest::new(
        blob_id_of(data),
        data.len() as u64,
        chunk,
        UploadId::new(upload),
        crcs,
    )
    .unwrap()
}

fn stage(store: &PackStore<SimFsFile>, m: &BlobManifest, data: &[u8]) {
    for (i, c) in data.chunks(m.chunk_size().get() as usize).enumerate() {
        store.stage_chunk(m.upload_id(), i as u32, c).unwrap();
    }
}

/// A pack of `[orphan chunk][bound chunk 0][bound chunk 1][index]`: an upload staged and never bound, then a
/// two-chunk blob bound after it, so the orphan is committed (below `log_end`). Returns the file system and the offsets
/// of the three chunk frames.
fn pack_with_an_orphan() -> (SimFs, [u64; 3]) {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite);
    let orphan = vec![7_u8; 1000];
    stage(&store, &manifest(&orphan, 1), &orphan);
    let data: Vec<u8> = (0..(64 << 10) + 500).map(|i| (i % 251) as u8).collect();
    let m = manifest(&data, 2);
    stage(&store, &m, &data);
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    let first = FIRST_FRAME;
    let second = first + chunk_frame_len(1000);
    let third = second + chunk_frame_len(64 << 10);
    (fs, [first, second, third])
}

#[test]
fn a_sound_pack_scans_clean_and_counts_its_orphans() {
    let fs = sim();
    assert_eq!(
        open(&fs, PackMode::ReadOnly).scan_checksums().unwrap(),
        None
    );
    let (fs, _) = pack_with_an_orphan();
    for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
        let scan = open(&fs, mode).scan_checksums().unwrap().unwrap();
        assert_eq!(scan.frames, 4, "{mode:?}");
        assert_eq!(scan.chunks, 3, "{mode:?}");
        assert_eq!(scan.orphan_chunks, 1, "{mode:?}");
        assert_eq!(scan.indexes, 1, "{mode:?}");
        let log_end = open(&fs, PackMode::ReadOnly).superblock().unwrap().log_end;
        assert_eq!(scan.bytes, log_end - FIRST_FRAME, "{mode:?}");
    }
}

#[test]
fn a_damaged_chunk_fails_the_scan_at_its_frame() {
    let (fs, chunks) = pack_with_an_orphan();
    let bytes = fs.read(Path::new(PACK)).unwrap();
    for (frame, len) in chunks.into_iter().zip([1000_u64, 64 << 10, 500]) {
        // A byte of the chunk's data, and one of its head (32 bytes after the frame header).
        for at in [frame + 24 + 32 + len / 2, frame + 24 + 4] {
            let mut damaged = bytes.clone();
            damaged[at as usize] ^= 0x20;
            let fs = sim();
            let f = fs.create_new(Path::new(PACK)).unwrap();
            f.write_all_at(&damaged, 0).unwrap();
            f.sync_data().unwrap();
            fs.sync_dir(Path::new("/db")).unwrap();
            // An open does not read a chunk's bytes: it indexes the blob as before.
            let store = open(&fs, PackMode::ReadOnly);
            assert_eq!(store.blob_ids().len(), 1, "byte {at}");
            let err = store.scan_checksums().unwrap_err();
            assert!(err.is_corruption(), "byte {at}: {err}");
            assert_eq!(err.corrupt_offset(), Some(frame), "byte {at}: {err}");
        }
    }
}
