//! The pack's checksum scan (SPEC §9.5; ROADMAP 9.5.3; ADR-0053): what `persia verify` runs by default.
//!
//! Invariants:
//! 1. A sound pack scans clean: every committed frame counted once, orphan chunks among the chunks; no pack scans
//!    as `None`.
//! 2. A damaged byte in any committed chunk frame, bound or orphan, which an open does not read, fails the scan with
//!    a corruption naming that frame's offset; the blobs the open indexed are unchanged.
//! 5. An index frame naming a sound chunk frame that is not its manifest's chunk fails the scan at the index frame.
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

/// Rewrites the `BlobChunk` frame at `frame` in `bytes` with its payload changed by `edit`, then reseals it with the
/// format's own frame encoder (SPEC §4.4), so its frame checksum is valid again: only the chunk's own CRC32C (SPEC
/// §9.3) can tell.
fn reseal_chunk_frame(bytes: &mut [u8], frame: u64, edit: impl FnOnce(&mut Vec<u8>)) {
    use persia_format::Writer;
    use persia_format::block::Compression;
    use persia_format::frame::{FrameKind, encode_frame};

    let at = frame as usize;
    let len = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()) as usize;
    let seq = u64::from_le_bytes(bytes[at + 16..at + 24].try_into().unwrap());
    let total = chunk_frame_len((len - 32) as u32) as usize;
    let mut payload = bytes[at + 24..at + 24 + len].to_vec();
    edit(&mut payload);
    let mut w = Writer::new();
    encode_frame(
        &mut w,
        FrameKind::BlobChunk,
        seq,
        &payload,
        Compression::None,
    )
    .unwrap();
    assert_eq!(w.len(), total, "a resealed frame keeps its length");
    bytes[at..at + total].copy_from_slice(w.as_slice());
    // The frame itself is sound again: the frame-CRC path of the scan passes it.
    persia_blob::pack::layout::decode_frame(&bytes[at..at + total], frame, FrameKind::BlobChunk)
        .unwrap();
}

/// A file system holding `bytes` as the pack.
fn sim_with(bytes: &[u8]) -> SimFs {
    let fs = sim();
    let f = fs.create_new(Path::new(PACK)).unwrap();
    f.write_all_at(bytes, 0).unwrap();
    f.sync_data().unwrap();
    fs.sync_dir(Path::new("/db")).unwrap();
    fs
}

/// The chunk head's `crc` field is at bytes 24..28 of a `BlobChunk` payload (SPEC §9.3: `upload_id u128, chunk_idx
/// u32, len u32, crc u32, reserved u32`).
const CRC_IN_PAYLOAD: usize = 24;

/// An edit of a chunk frame's payload.
type Edit = fn(&mut Vec<u8>);

/// A chunk frame whose frame checksum is valid but whose stored chunk CRC32C does not match its bytes, in every chunk
/// frame of the pack, orphan included: the scan fails at that frame (SPEC §9.5: "every chunk's CRC32C, orphans
/// included"), with both kinds of mismatch: the stored CRC changed, and a data byte changed under the stored CRC.
#[test]
fn a_resealed_chunk_whose_stored_crc_is_wrong_fails_the_scan_at_its_frame() {
    let (fs, chunks) = pack_with_an_orphan();
    let bytes = fs.read(Path::new(PACK)).unwrap();
    let edits: [(&str, Edit); 3] = [
        ("stored crc", |p| p[CRC_IN_PAYLOAD] ^= 0x01),
        ("stored crc, high bit", |p| p[CRC_IN_PAYLOAD + 3] ^= 0x80),
        ("data under the stored crc", |p| {
            let last = p.len() - 1;
            p[last] ^= 0x01;
        }),
    ];
    for (i, &frame) in chunks.iter().enumerate() {
        for (what, edit) in edits {
            let mut damaged = bytes.clone();
            reseal_chunk_frame(&mut damaged, frame, edit);
            let store = open(&sim_with(&damaged), PackMode::ReadOnly);
            // An open does not read a chunk's bytes: the blob is indexed as before.
            assert_eq!(store.blob_ids().len(), 1, "{what} at {frame}");
            let err = store.scan_checksums().unwrap_err();
            assert!(err.is_corruption(), "{what} at {frame}: {err}");
            assert_eq!(
                err.corrupt_offset(),
                Some(frame),
                "{what} at {frame}: {err}"
            );
            // A read of the bound blob agrees: it fails on a damaged bound chunk, and only the scan finds the orphan's.
            let id = store.blob_ids()[0];
            let m = store.manifest(id).unwrap().unwrap();
            let read: Result<Vec<_>, _> = (0..m.chunk_count())
                .map(|c| persia_blob::read_chunk_checked(&store, &m, c))
                .collect();
            if i == 0 {
                read.unwrap();
            } else {
                assert!(read.unwrap_err().is_corruption(), "{what} at {frame}");
            }
        }
    }
}

/// Two damaged chunk frames: the scan, of a read-only or a read-write handle, names the first one, not a later one.
#[test]
fn the_scan_names_the_first_damaged_frame() {
    let (fs, chunks) = pack_with_an_orphan();
    let mut bytes = fs.read(Path::new(PACK)).unwrap();
    reseal_chunk_frame(&mut bytes, chunks[2], |p| p[CRC_IN_PAYLOAD] ^= 1);
    reseal_chunk_frame(&mut bytes, chunks[1], |p| p[CRC_IN_PAYLOAD] ^= 1);
    let fs = sim_with(&bytes);
    for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
        let err = open(&fs, mode).scan_checksums().unwrap_err();
        assert_eq!(err.corrupt_offset(), Some(chunks[1]), "{mode:?}: {err}");
    }
}

/// Rewrites the frame at `frame` in `bytes` (any kind) with its payload changed by `edit`, then reseals it, as
/// [`reseal_chunk_frame`] does. Returns its whole length.
fn reseal_frame(
    bytes: &mut [u8],
    frame: u64,
    kind: persia_format::frame::FrameKind,
    edit: impl FnOnce(&mut Vec<u8>),
) -> usize {
    use persia_format::Writer;
    use persia_format::block::Compression;
    use persia_format::frame::encode_frame;

    let at = frame as usize;
    let len = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()) as usize;
    let seq = u64::from_le_bytes(bytes[at + 16..at + 24].try_into().unwrap());
    let mut payload = bytes[at + 24..at + 24 + len].to_vec();
    edit(&mut payload);
    let mut w = Writer::new();
    encode_frame(&mut w, kind, seq, &payload, Compression::None).unwrap();
    let total = w.len();
    bytes[at..at + total].copy_from_slice(w.as_slice());
    total
}

/// Invariant 5: an index frame resealed to name another blob's valid chunk frame (its frame and chunk CRCs sound, so
/// an open indexes it) fails the default scan at the index frame: each named chunk's upload, index, length and CRC32C
/// are checked against the index's manifest (ROADMAP 9.5.3, ADR-0053).
#[test]
fn an_index_resealed_to_name_another_blobs_chunk_fails_the_scan_at_the_index() {
    use persia_blob::pack::layout::MIN_INDEX_FRAME_LEN;
    use persia_format::frame::FrameKind;

    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite);
    // Same length, so only the manifest tells the chunks apart: [a's chunk][a's index][b's chunk][b's index].
    for (data, upload) in [(&b"first"[..], 1), (&b"other"[..], 2)] {
        let m = manifest(data, upload);
        stage(&store, &m, data);
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    }
    drop(store);
    let a_chunk = FIRST_FRAME;
    let b_index = a_chunk + 2 * chunk_frame_len(5) + MIN_INDEX_FRAME_LEN + 8;
    let mut bytes = fs.read(Path::new(PACK)).unwrap();
    assert_eq!(&bytes[b_index as usize + 4], &FrameKind::BlobIndex.to_u8());
    reseal_frame(&mut bytes, b_index, FrameKind::BlobIndex, |p| {
        let last = p.len() - 8;
        p[last..].copy_from_slice(&a_chunk.to_le_bytes());
    });
    let fs = sim_with(&bytes);
    for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
        let store = open(&fs, mode);
        assert_eq!(store.blob_ids().len(), 2, "{mode:?}: the open indexes both");
        let err = store.scan_checksums().unwrap_err();
        assert!(err.is_corruption(), "{mode:?}: {err}");
        assert_eq!(err.corrupt_offset(), Some(b_index), "{mode:?}: {err}");
    }
}

// -- Property: a scan of any pack built by random stage and bind sequences (SPEC §9.3, §9.5) --
//
// Invariants:
// 3. A pack built only through `stage_chunk`, `bind` and read-write reopens scans clean, and its scan counts exactly
//    the committed frames (`12288..log_end`): the chunk frames staged before the last `Bound`, the index frames of
//    every `Bound`, and as orphans the committed chunk frames no bound index names (unbound uploads, duplicates
//    answered `AlreadyBound`, strays, chunks a bind did not use). Staged frames after `log_end` are not counted.
// 4. A read-only open and the writer's handle scan the same.

mod model {
    use std::collections::{HashMap, HashSet};

    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed};

    use super::{FIRST_FRAME, PackMode, PackStore, SimFs, SimFsFile, manifest, open, sim};
    use persia_blob::{BindOutcome, BlobId, BlobStore, Error, UploadId};

    const LENS: [usize; 6] = [1, 10, 4096, 65_536, 65_537, 140_000];
    const CHUNK: usize = 64 << 10;

    #[derive(Debug, Clone)]
    pub enum Op {
        /// Stages every chunk of blob `(seed, len)` under `upload`.
        Stage { upload: u8, seed: u8, len: usize },
        /// Binds blob `(seed, len)`'s manifest under `upload`.
        Bind { upload: u8, seed: u8, len: usize },
        /// Stages a 10-byte chunk `index` of `upload`.
        Stray { upload: u8, index: u32, seed: u8 },
        /// Drops the writer and reopens the pack read-write.
        Reopen,
    }

    pub fn op() -> impl Strategy<Value = Op> {
        let blob = (0..4_u8, 0..3_u8, 0..LENS.len());
        prop_oneof![
            4 => blob.clone().prop_map(|(upload, seed, l)| Op::Stage { upload, seed, len: LENS[l] }),
            4 => blob.prop_map(|(upload, seed, l)| Op::Bind { upload, seed, len: LENS[l] }),
            1 => (0..4_u8, 0..3_u32, any::<u8>())
                .prop_map(|(upload, index, seed)| Op::Stray { upload, index, seed }),
            1 => Just(Op::Reopen),
        ]
    }

    fn data(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| ((i * 31 + usize::from(seed) * 17 + 1) % 251) as u8)
            .collect()
    }

    fn upload_id(upload: u8) -> UploadId {
        UploadId::new(u128::from(upload) + 1)
    }

    /// A committed or staged frame, in append order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Frame {
        Chunk,
        Index,
    }

    #[derive(Default)]
    struct Model {
        frames: Vec<Frame>,
        /// Frames below `log_end`.
        committed: usize,
        /// Chunk frames (by position) a committed index names.
        named: HashSet<usize>,
        /// This handle's staged chunks: (upload, index) → (frame, bytes).
        staged: HashMap<(u8, u32), (usize, Vec<u8>)>,
        /// Bound uploads: upload → the chunk bytes of its blob.
        bound_uploads: HashMap<u8, Vec<Vec<u8>>>,
        bound_blobs: HashSet<BlobId>,
        created: bool,
    }

    /// What `stage_chunk` answers, in the model.
    #[derive(Debug, PartialEq, Eq)]
    enum Staged {
        Appended,
        AlreadyThere,
        Conflict,
    }

    impl Model {
        fn stage(&mut self, upload: u8, index: u32, bytes: &[u8]) -> Staged {
            let existing = self
                .staged
                .get(&(upload, index))
                .map(|(_, b)| b.as_slice())
                .or_else(|| {
                    self.bound_uploads
                        .get(&upload)
                        .and_then(|c| c.get(index as usize))
                        .map(Vec::as_slice)
                });
            match existing {
                Some(e) if e == bytes => Staged::AlreadyThere,
                Some(_) => Staged::Conflict,
                None => {
                    self.created = true;
                    self.frames.push(Frame::Chunk);
                    self.staged
                        .insert((upload, index), (self.frames.len() - 1, bytes.to_vec()));
                    Staged::Appended
                }
            }
        }

        fn scan(&self) -> Option<(u64, u64, u64, u64)> {
            self.created.then(|| {
                let committed = &self.frames[..self.committed];
                let chunks = committed.iter().filter(|f| **f == Frame::Chunk).count() as u64;
                let indexes = committed.len() as u64 - chunks;
                let orphans = chunks - self.named.len() as u64;
                (committed.len() as u64, chunks, orphans, indexes)
            })
        }
    }

    fn check_stage(
        model: &mut Model,
        store: &PackStore<SimFsFile>,
        upload: u8,
        index: u32,
        bytes: &[u8],
        ctx: &str,
    ) -> bool {
        let expected = model.stage(upload, index, bytes);
        let got = store.stage_chunk(upload_id(upload), index, bytes);
        match expected {
            Staged::Appended | Staged::AlreadyThere => {
                got.unwrap_or_else(|e| panic!("{ctx}: stage {upload}/{index}: {e}"));
                true
            }
            Staged::Conflict => {
                assert!(
                    matches!(got, Err(Error::StagedChunkConflict { .. })),
                    "{ctx}: stage {upload}/{index}: {got:?}"
                );
                false
            }
        }
    }

    fn bind(
        model: &mut Model,
        store: &PackStore<SimFsFile>,
        upload: u8,
        seed: u8,
        len: usize,
        ctx: &str,
    ) {
        let bytes = data(seed, len);
        let m = manifest_for(&bytes, upload);
        let got = store.bind(&m);
        if model.bound_blobs.contains(&m.blob_id()) {
            assert!(
                matches!(got, Ok(BindOutcome::AlreadyBound(_))),
                "{ctx}: {got:?}"
            );
            return;
        }
        if model.bound_uploads.contains_key(&upload) {
            assert!(
                matches!(got, Err(Error::InvalidManifest { .. })),
                "{ctx}: {got:?}"
            );
            return;
        }
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK).collect();
        let mut used = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            match model.staged.get(&(upload, i as u32)) {
                Some((frame, b)) if b.as_slice() == *c => used.push(*frame),
                _ => {
                    assert!(
                        matches!(got, Err(Error::ChunkNotStaged { .. })),
                        "{ctx}: {got:?}"
                    );
                    return;
                }
            }
        }
        assert!(matches!(got, Ok(BindOutcome::Bound)), "{ctx}: {got:?}");
        model.frames.push(Frame::Index);
        model.committed = model.frames.len();
        model.named.extend(used);
        model
            .bound_uploads
            .insert(upload, chunks.iter().map(|c| c.to_vec()).collect());
        model.bound_blobs.insert(m.blob_id());
        model.staged.retain(|(u, _), _| *u != upload);
    }

    fn manifest_for(bytes: &[u8], upload: u8) -> persia_blob::BlobManifest {
        manifest(bytes, u128::from(upload) + 1)
    }

    fn assert_scan(model: &Model, store: &PackStore<SimFsFile>, ctx: &str) {
        let scan = store
            .scan_checksums()
            .unwrap_or_else(|e| panic!("{ctx}: {e}"));
        let got = scan.map(|s| (s.frames, s.chunks, s.orphan_chunks, s.indexes));
        assert_eq!(
            got,
            model.scan(),
            "{ctx}: (frames, chunks, orphans, indexes)"
        );
        if let Some(s) = scan {
            let log_end = store.superblock().unwrap().log_end;
            assert_eq!(s.bytes, log_end - FIRST_FRAME, "{ctx}");
        }
        assert_eq!(store.blob_ids().len(), model.bound_blobs.len(), "{ctx}");
    }

    pub fn run(ops: &[Op]) {
        let fs: SimFs = sim();
        let mut store = open(&fs, PackMode::ReadWrite);
        let mut model = Model::default();
        for (step, op) in ops.iter().enumerate() {
            let ctx = format!("step {step} ({op:?}) of {ops:?}");
            match *op {
                Op::Stage { upload, seed, len } => {
                    for (i, c) in data(seed, len).chunks(CHUNK).enumerate() {
                        if !check_stage(&mut model, &store, upload, i as u32, c, &ctx) {
                            break;
                        }
                    }
                }
                Op::Bind { upload, seed, len } => bind(&mut model, &store, upload, seed, len, &ctx),
                Op::Stray {
                    upload,
                    index,
                    seed,
                } => {
                    check_stage(&mut model, &store, upload, index, &[seed; 10], &ctx);
                }
                Op::Reopen => {
                    drop(store);
                    store = open(&fs, PackMode::ReadWrite);
                    // Nothing after `log_end` was acknowledged: the open truncates it.
                    model.frames.truncate(model.committed);
                    model.staged.clear();
                }
            }
            assert_scan(&model, &store, &ctx);
        }
        let ctx = format!("a read-only open after {ops:?}");
        assert_scan(&model, &open(&fs, PackMode::ReadOnly), &ctx);
    }

    proptest! {
        #![proptest_config(Config {
            cases: if cfg!(feature = "slow") { 1024 } else { 64 },
            rng_seed: RngSeed::Fixed(0x9_5_3),
            failure_persistence: None,
            ..Config::default()
        })]

        #[test]
        fn a_scan_of_any_staged_and_bound_pack_is_clean_and_counts_its_frames(
            ops in proptest::collection::vec(op(), 1..24),
        ) {
            run(&ops);
        }
    }
}
