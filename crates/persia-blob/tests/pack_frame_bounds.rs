//! Frame lengths bounded before the pack open reads them (SPEC §4.4, §9.3; ADR-0049 item 9; ROADMAP 9.2.14):
//! adversarial packs whose frames claim lengths at, just above and far above the longest frame of their kind, lengths
//! that overflow `u64` or the frame's offset, and slots whose `log_end` lies.
//!
//! Invariants:
//! 1. A committed frame longer than the longest frame of its kind (a `BlobChunk` frame of a 64 MiB chunk; a
//!    `BlobIndex` frame naming every chunk frame before it) is `CorruptAt` "a committed frame longer than any frame of
//!    its kind" at its offset, found before any read of it: the open's longest read stays the 12288-byte prefix. A
//!    frame exactly at the bound passes that rule.
//! 2. A committed frame whose length overflows `u64`, overflows its offset, or runs past `log_end` is `Corrupt` at its
//!    offset, never a panic and never a read sized by it.
//! 3. A slot whose `log_end` lies (past the file, unaligned, inside a frame, past the committed frames) fails the open
//!    with `Corrupt`, and a read-write open writes nothing.
//! 4. After `log_end` the same frames end the roll-forward unread: a read-write open keeps every committed blob (and a
//!    whole tail bind before them), truncates the rest, reads only in the checksum pass's 64 KiB pieces, and a second
//!    open finds nothing more to do. A read-only open ignores them.
//! 5. Property: any one frame after a valid prefix, committed or as the tail, and any byte edits of a valid pack (CRCs
//!    resealed or not): the open succeeds or fails with a corruption-kind error, never panics, reads in bounded pieces,
//!    and every blob it serves reads back its own bytes or fails with `Corrupt`.
//!
//! Not under Miri: the three tests with 64 MiB frames (`a_committed_chunk_frame_of_a_64_mib_chunk_*`,
//! `a_committed_chunk_frame_one_byte_over_*`, `a_tail_bind_then_an_index_frame_at_or_over_*`) and the two proptests
//! are too slow for it; the rest are small.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    missing_docs
)] // Tests: inputs are generated or fixed.

mod pack_support;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use persia_blob::pack::layout::{
    FIRST_FRAME, PackSuperblock, chunk_frame_len, decode_prefix, encode_index_frame,
    new_pack_prefix,
};
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobStore, ChunkSize, Error, PackMode, PackStore, UploadId,
    blob_id_of, read_range,
};
use persia_format::BlobRef;
use persia_format::checksum::crc32c;
use persia_format::frame::{FRAME_MAGIC, FrameKind, frame_len};
use persia_storage::container::ContainerFile;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use persia_storage::vfs::{DirEntry, EntryKind, Vfs};
use proptest::prelude::*;

const PACK: &str = "/db/app.persia.blobs";
const DB_UUID: [u8; 16] = [9; 16];
/// The pack's header and both slots: what the open reads first, in one read.
const PREFIX: u64 = FIRST_FRAME;
/// The piece size of the checksum pass a roll-forward runs over a tail frame (`DEFAULT_SCAN_CHUNK`).
const SCAN_PIECE: u64 = 64 * 1024;
/// The bound's error (ROADMAP 9.2.14).
const TOO_LONG: &str = "a committed frame longer than any frame of its kind";
/// 64 MiB, the longest chunk (SPEC §9.1).
const MAX_CHUNK: u64 = 64 << 20;
/// The longest index frame after no chunk frame: an empty blob's (SPEC §9.3).
const EMPTY_INDEX_LEN: u64 = 120;

// --- A file system that records the longest read ---

/// `SimFs` whose files record the longest `read_exact_at` made on them.
#[derive(Debug, Clone)]
struct Counted {
    fs: SimFs,
    longest: Arc<AtomicU64>,
}

#[derive(Debug)]
struct CountedFile {
    file: SimFsFile,
    longest: Arc<AtomicU64>,
}

impl ContainerFile for CountedFile {
    fn file_len(&self) -> io::Result<u64> {
        self.file.file_len()
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.longest.fetch_max(buf.len() as u64, Ordering::Relaxed);
        self.file.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_all_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
    fn try_lock(&self) -> io::Result<bool> {
        self.file.try_lock()
    }
    fn unlock(&self) -> io::Result<()> {
        self.file.unlock()
    }
    fn locks_per_handle(&self) -> bool {
        self.file.locks_per_handle()
    }
}

impl Counted {
    fn new(fs: &SimFs) -> Self {
        Self {
            fs: fs.clone(),
            longest: Arc::default(),
        }
    }
    fn wrap(&self, file: SimFsFile) -> CountedFile {
        CountedFile {
            file,
            longest: Arc::clone(&self.longest),
        }
    }
    fn longest_read(&self) -> u64 {
        self.longest.load(Ordering::Relaxed)
    }
    fn open(&self, mode: PackMode) -> persia_blob::Result<PackStore<CountedFile>> {
        pack_support::open_pack(Arc::new(self.clone()), PACK, mode, DB_UUID)
    }
}

impl Vfs for Counted {
    type File = CountedFile;
    fn open(&self, path: &Path, write: bool) -> io::Result<CountedFile> {
        self.fs.open(path, write).map(|f| self.wrap(f))
    }
    fn create_new(&self, path: &Path) -> io::Result<CountedFile> {
        self.fs.create_new(path).map(|f| self.wrap(f))
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.fs.create_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.fs.rename(from, to)
    }
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.fs.hard_link(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.fs.remove_file(path)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.fs.sync_dir(dir)
    }
    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        self.fs.entry_kind(path)
    }
    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        self.fs.is_dir(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        self.fs.read_dir(dir)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.fs.canonicalize(path)
    }
    fn same_file(&self, file: &CountedFile, path: &Path) -> io::Result<bool> {
        self.fs.same_file(&file.file, path)
    }
    fn lock_beside(&self, path: &Path) -> io::Result<Option<CountedFile>> {
        self.fs.lock_beside(path).map(|f| f.map(|f| self.wrap(f)))
    }
}

// --- Building packs ---

fn sim() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    fs
}

fn manifest_of(data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
    let crcs = data.chunks(chunk as usize).map(crc32c).collect();
    BlobManifest::new(
        blob_id_of(data),
        data.len() as u64,
        ChunkSize::from_encoding(chunk).unwrap(),
        UploadId::new(upload),
        crcs,
    )
    .unwrap()
}

fn put(store: &PackStore<SimFsFile>, data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
    let m = manifest_of(data, chunk, upload);
    for (i, c) in data.chunks(chunk as usize).enumerate() {
        store.stage_chunk(m.upload_id(), i as u32, c).unwrap();
    }
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    m
}

/// The bytes of a pack the writer made with `blobs` (data, chunk size) bound in order, uploads 1, 2, ...
fn written_pack(blobs: &[(&[u8], u32)]) -> (Vec<u8>, Vec<BlobManifest>) {
    let fs = sim();
    let store =
        pack_support::open_pack(Arc::new(fs.clone()), PACK, PackMode::ReadWrite, DB_UUID).unwrap();
    let ms = (1..)
        .zip(blobs)
        .map(|(upload, &(data, chunk))| put(&store, data, chunk, upload))
        .collect();
    drop(store);
    (fs.read(Path::new(PACK)).unwrap(), ms)
}

/// A frame header: `kind`, no flags, `payload_len`, `frame_seq`.
fn header(kind: u8, payload_len: u64, seq: u64) -> [u8; 24] {
    let mut h = [0; 24];
    h[..4].copy_from_slice(&FRAME_MAGIC);
    h[4] = kind;
    h[8..16].copy_from_slice(&payload_len.to_le_bytes());
    h[16..24].copy_from_slice(&seq.to_le_bytes());
    h
}

/// A whole frame of `kind` numbered `seq` holding `payload`, its CRC32C valid: whatever its kind and length, every
/// check of SPEC §4.4 but the pack's own rules passes.
fn frame(kind: u8, seq: u64, payload: &[u8]) -> Vec<u8> {
    let mut f = header(kind, payload.len() as u64, seq).to_vec();
    f.extend_from_slice(payload);
    f.resize(f.len().next_multiple_of(8), 0);
    let crc = crc32c(&f[4..]);
    f.extend_from_slice(&crc.to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f
}

/// Rewrites the active slot of `bytes` with `edit` applied (its CRC recomputed), so the open takes it.
fn edit_slot(bytes: &mut [u8], edit: impl FnOnce(&mut PackSuperblock)) {
    let p = decode_prefix(bytes).unwrap();
    let mut sb = p.superblock;
    edit(&mut sb);
    let at = p.slot.offset() as usize;
    bytes[at..at + 4096].copy_from_slice(&sb.encode());
}

/// Commits everything after `12288` in a pack whose slot commits nothing yet: `frames` frames, the last one the index
/// frame at `index_off..log_end`, `blobs` index frames in all.
fn commit_all(bytes: &mut [u8], index_off: u64, blobs: u64, frames: u64) {
    let log_end = bytes.len() as u64;
    edit_slot(bytes, |sb| {
        sb.seq = 1;
        sb.index_off = index_off;
        sb.index_len = log_end - index_off;
        sb.log_end = log_end;
        sb.blob_count = blobs;
        sb.next_frame_seq = frames + 1;
    });
}

/// A new pack's header and slots: nothing committed.
fn new_pack() -> Vec<u8> {
    new_pack_prefix(DB_UUID, 7).unwrap()
}

/// The index frame of an empty blob of upload `upload`, numbered `seq`: 120 bytes, the longest index frame that names
/// no chunk frame.
fn empty_index(seq: u64, upload: u128) -> Vec<u8> {
    let f = encode_index_frame(seq, &manifest_of(b"", 4, upload), &[]).unwrap();
    assert_eq!(f.len() as u64, EMPTY_INDEX_LEN);
    f
}

/// A pack with the empty blob of upload 1 committed (its index the one frame, 12288..12408), and its id.
fn empty_blob_pack() -> (Vec<u8>, BlobId) {
    let mut bytes = new_pack();
    bytes.extend_from_slice(&empty_index(1, 1));
    commit_all(&mut bytes, PREFIX, 1, 1);
    (bytes, blob_id_of(b""))
}

fn with_file(bytes: &[u8]) -> SimFs {
    let fs = sim();
    let f = fs.create_new(Path::new(PACK)).unwrap();
    f.write_all_at(bytes, 0).unwrap();
    f.sync_data().unwrap();
    fs.sync_dir(Path::new("/db")).unwrap();
    fs
}

fn reference(m: &BlobManifest) -> BlobRef {
    BlobRef::new(
        m.blob_id(),
        m.size(),
        "a/b",
        m.chunk_size().get(),
        BTreeMap::new(),
    )
    .unwrap()
}

/// Opens the pack in `bytes` in `mode` on a fresh file system: the outcome, the longest read, and the file after.
fn open_bytes(
    bytes: &[u8],
    mode: PackMode,
) -> (persia_blob::Result<PackStore<CountedFile>>, u64, Vec<u8>) {
    let fs = with_file(bytes);
    let vfs = Counted::new(&fs);
    let opened = vfs.open(mode);
    let longest = vfs.longest_read();
    let after = fs.read(Path::new(PACK)).unwrap();
    (opened, longest, after)
}

/// Both opens fail with `CorruptAt` at `offset`, `what` if given; neither reads more than the prefix at once, and
/// neither writes.
fn assert_corrupt_unread(bytes: &[u8], offset: u64, what: Option<&str>, case: &str) {
    for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
        let (opened, longest, after) = open_bytes(bytes, mode);
        let err = opened.map(|_| ()).unwrap_err();
        assert!(err.is_corruption(), "{case} {mode:?}: {err:?}");
        assert_eq!(
            err.corrupt_offset(),
            Some(offset),
            "{case} {mode:?}: {err:?}"
        );
        if let Some(what) = what {
            assert!(
                matches!(err, Error::CorruptAt { what: w, .. } if w == what),
                "{case} {mode:?}: {err:?}"
            );
        }
        assert_eq!(longest, PREFIX, "{case} {mode:?}: the longest read");
        assert!(after == bytes, "{case} {mode:?}: the open wrote");
    }
}

// --- 1. Committed frames at and above their kind's bound ---

/// After no chunk frame, the longest index frame is an empty blob's, 120 bytes: one of exactly 120 bytes opens, one 8
/// bytes longer (its first 88 bytes a valid empty-blob index, so only its length is wrong) and one of 1 MiB are
/// `Corrupt` by the bound, before their read.
#[test]
fn a_committed_index_frame_after_no_chunk_is_bounded_at_120_bytes() {
    let (at_bound, id) = empty_blob_pack();
    for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
        let (opened, longest, _) = open_bytes(&at_bound, mode);
        assert_eq!(opened.unwrap().blob_ids(), [id], "{mode:?}");
        assert_eq!(longest, PREFIX, "{mode:?}");
    }
    let payload = &empty_index(1, 1)[24..112];
    for (case, len) in [("just above", 89), ("1 MiB", 1 << 20)] {
        let mut p = payload.to_vec();
        p.resize(len, 0);
        let mut bytes = new_pack();
        bytes.extend_from_slice(&frame(FrameKind::BlobIndex.to_u8(), 1, &p));
        assert!(bytes.len() as u64 - PREFIX > EMPTY_INDEX_LEN);
        commit_all(&mut bytes, PREFIX, 1, 1);
        assert_corrupt_unread(&bytes, PREFIX, Some(TOO_LONG), case);
    }
}

/// The bound counts every chunk frame before the index, orphans included: after an orphan chunk frame an index frame
/// of 1 MiB is within it (one inline 64 MiB chunk would fit, SPEC §9.1.1), so it is read and fails on its contents,
/// not its length.
#[test]
fn a_committed_index_frame_after_a_chunk_frame_is_not_cut_by_the_no_chunk_bound() {
    let mut bytes = new_pack();
    // An orphan chunk frame: never read by the open's scan, so its head need not be valid.
    bytes.extend_from_slice(&frame(FrameKind::BlobChunk.to_u8(), 1, &[0; 40]));
    let index_off = bytes.len() as u64;
    let mut p = empty_index(2, 1)[24..112].to_vec();
    p.resize(1 << 20, 0);
    bytes.extend_from_slice(&frame(FrameKind::BlobIndex.to_u8(), 2, &p));
    commit_all(&mut bytes, index_off, 1, 2);
    let (opened, longest, _) = open_bytes(&bytes, PackMode::ReadOnly);
    let err = opened.map(|_| ()).unwrap_err();
    assert!(err.is_corruption(), "{err:?}");
    assert_eq!(err.corrupt_offset(), Some(index_off));
    assert!(
        !matches!(err, Error::CorruptAt { what: TOO_LONG, .. }),
        "within the bound: {err:?}"
    );
    assert!(longest > SCAN_PIECE, "read whole: {longest}");
}

/// A committed chunk frame shorter than its 32-byte head plus one byte is `Corrupt`, whatever the bound allows.
#[test]
fn a_committed_chunk_frame_of_at_most_its_head_is_corrupt() {
    for len in [0, 1, 31, 32] {
        let mut bytes = new_pack();
        bytes.extend_from_slice(&frame(FrameKind::BlobChunk.to_u8(), 1, &vec![0; len]));
        let index_off = bytes.len() as u64;
        bytes.extend_from_slice(&empty_index(2, 1));
        commit_all(&mut bytes, index_off, 1, 2);
        assert_corrupt_unread(
            &bytes,
            PREFIX,
            Some("a committed chunk frame shorter than a chunk"),
            &format!("payload {len}"),
        );
    }
}

/// Writes `[a chunk frame claiming payload_len, seq 1][an empty blob's index, seq 2]` committed by the slot, with the
/// chunk frame's bytes zeros (the open's scan never reads them): a file of `payload_len` + ~12 KiB.
fn pack_with_a_long_chunk_frame(payload_len: u64) -> (SimFs, u64) {
    let mut prefix = new_pack();
    prefix.extend_from_slice(&header(FrameKind::BlobChunk.to_u8(), payload_len, 1));
    let index_off = PREFIX + frame_len(payload_len).unwrap();
    let index = empty_index(2, 1);
    let log_end = index_off + index.len() as u64;
    edit_slot(&mut prefix, |sb| {
        sb.seq = 1;
        sb.index_off = index_off;
        sb.index_len = index.len() as u64;
        sb.log_end = log_end;
        sb.blob_count = 1;
        sb.next_frame_seq = 3;
    });
    let fs = sim();
    let f = fs.create_new(Path::new(PACK)).unwrap();
    f.write_all_at(&prefix, 0).unwrap();
    f.set_len(index_off).unwrap();
    f.write_all_at(&index, index_off).unwrap();
    f.sync_data().unwrap();
    fs.sync_dir(Path::new("/db")).unwrap();
    (fs, log_end)
}

/// A committed chunk frame of a 64 MiB chunk is the longest a pack holds: one exactly that long passes the open (an
/// orphan its scan never reads), one claiming one byte more is `Corrupt` at its offset before any read of it, though
/// the file holds all its bytes. Not under Miri (a 64 MiB file).
#[test]
fn a_committed_chunk_frame_of_a_64_mib_chunk_is_the_longest_allowed() {
    let at_bound = 32 + MAX_CHUNK;
    assert_eq!(
        frame_len(at_bound).unwrap(),
        chunk_frame_len(MAX_CHUNK as u32)
    );
    let (fs, log_end) = pack_with_a_long_chunk_frame(at_bound);
    for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
        let vfs = Counted::new(&fs);
        let store = vfs.open(mode).unwrap();
        assert_eq!(store.blob_ids(), [blob_id_of(b"")], "{mode:?}");
        assert_eq!(store.recovery().truncated_from, None, "{mode:?}");
        assert_eq!(
            vfs.longest_read(),
            PREFIX,
            "{mode:?}: the chunk is not read"
        );
    }
    assert_eq!(
        fs.open(Path::new(PACK), false).unwrap().file_len().unwrap(),
        log_end
    );
}

/// See [`a_committed_chunk_frame_of_a_64_mib_chunk_is_the_longest_allowed`]. Not under Miri.
#[test]
fn a_committed_chunk_frame_one_byte_over_a_64_mib_chunk_is_corrupt_before_its_read() {
    let (fs, log_end) = pack_with_a_long_chunk_frame(32 + MAX_CHUNK + 1);
    for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
        let vfs = Counted::new(&fs);
        let err = vfs.open(mode).map(|_| ()).unwrap_err();
        assert!(
            matches!(
                err,
                Error::CorruptAt {
                    what: TOO_LONG,
                    offset: PREFIX,
                    ..
                }
            ),
            "{mode:?}: {err:?}"
        );
        assert_eq!(vfs.longest_read(), PREFIX, "{mode:?}");
        assert_eq!(
            fs.open(Path::new(PACK), false).unwrap().file_len().unwrap(),
            log_end,
            "{mode:?}: nothing truncated"
        );
    }
}

// --- 2. Lengths that overflow, or run past the committed frames ---

/// Payload lengths no frame in a small file can have: past `log_end`, at `u32::MAX`, past what `u64` holds once
/// padded and framed, and lengths whose total fits `u64` but overflows the frame's offset.
const WILD_LENGTHS: [u64; 10] = [
    129,
    4096,
    0xFFFF_FFFF,
    0x1_0000_0000,
    1 << 40,
    u64::MAX - 12288 - 39, // total fits u64, offset + total does not
    u64::MAX - 39,         // total = u64::MAX - 7
    u64::MAX - 32,         // padded, the total overflows
    u64::MAX - 7,
    u64::MAX,
];

/// The kinds a pack holds, and two it does not.
const KINDS: [u8; 4] = [6, 7, 3, 0xEE];

/// A committed 160-byte region at 12288 whose first frame header claims `payload_len` (the slot naming the region as
/// one index frame): every such frame is `Corrupt` at 12288, unread.
#[test]
fn a_committed_frame_whose_length_overflows_or_runs_past_log_end_is_corrupt() {
    for kind in KINDS {
        for len in WILD_LENGTHS {
            let mut bytes = new_pack();
            bytes.extend_from_slice(&header(kind, len, 1));
            bytes.resize(PREFIX as usize + 160, 0);
            commit_all(&mut bytes, PREFIX, 1, 1);
            assert_corrupt_unread(&bytes, PREFIX, None, &format!("kind {kind} len {len}"));
        }
    }
}

/// After a committed blob, a second committed frame of a kind no pack holds is `Corrupt` at its offset, even when its
/// length is that of an index frame.
#[test]
fn a_committed_frame_of_another_kind_after_a_valid_prefix_is_corrupt() {
    for kind in [1, 2, 3, 4, 5, 0, 8, 0xFF] {
        let (mut bytes, _) = empty_blob_pack();
        let at = bytes.len() as u64;
        bytes.extend_from_slice(&frame(kind, 2, &[0; 88]));
        edit_slot(&mut bytes, |sb| {
            sb.seq = 2;
            sb.index_off = at;
            sb.index_len = EMPTY_INDEX_LEN;
            sb.log_end = at + EMPTY_INDEX_LEN;
            sb.blob_count = 2;
            sb.next_frame_seq = 3;
        });
        assert_corrupt_unread(
            &bytes,
            at,
            Some("a committed frame of a kind no pack holds"),
            &format!("kind {kind}"),
        );
    }
}

/// A valid committed prefix (a two-chunk blob) followed by a committed index frame over its bound is `Corrupt` at the
/// frame, the earlier blob notwithstanding: the bound for the second index counts the two chunk frames.
#[test]
fn a_valid_prefix_then_an_oversize_committed_index_frame_is_corrupt() {
    let (mut bytes, _) = written_pack(&[(b"0123456789abcdef", 8)]);
    let p = decode_prefix(&bytes).unwrap().superblock;
    let at = bytes.len() as u64;
    // Over max_index_frame_len(2): over 64 MiB, so claimed, past the file (not over the file: check_fits first).
    bytes.extend_from_slice(&header(
        FrameKind::BlobIndex.to_u8(),
        MAX_CHUNK + 1024,
        p.next_frame_seq,
    ));
    bytes.resize(at as usize + 128, 0);
    edit_slot(&mut bytes, |sb| {
        sb.seq += 1;
        sb.index_off = at;
        sb.index_len = 128;
        sb.log_end = at + 128;
        sb.blob_count = 2;
        sb.next_frame_seq = p.next_frame_seq + 1;
    });
    assert_corrupt_unread(&bytes, at, None, "past the file");
    // And one within the file but over the bound for no chunk: a second empty-blob pack's frame, made 128 bytes long.
    let (mut bytes, _) = empty_blob_pack();
    let at = bytes.len() as u64;
    let mut payload = empty_index(2, 2)[24..112].to_vec();
    payload.resize(96, 0);
    bytes.extend_from_slice(&frame(FrameKind::BlobIndex.to_u8(), 2, &payload));
    edit_slot(&mut bytes, |sb| {
        sb.seq = 2;
        sb.index_off = at;
        sb.index_len = 128;
        sb.log_end = at + 128;
        sb.blob_count = 2;
        sb.next_frame_seq = 3;
    });
    assert_corrupt_unread(&bytes, at, Some(TOO_LONG), "within the file");
}

// --- 3. A lying log_end ---

/// A lie told by a slot: its name, where the open reports it, the edit, and how many zero bytes to append first.
type Lie = (&'static str, u64, Box<dyn Fn(&mut PackSuperblock)>, usize);

/// A slot whose `log_end` (with `index_off`/`index_len` lying along, where its rules ask) is past the file, at the
/// edge of `u64`, unaligned, below the frames, inside the last frame, or past the committed frames into zeros:
/// `Corrupt` at the slot or the frame, never a panic, a read sized by it, or a write.
#[test]
fn a_lying_log_end_fails_the_open() {
    let (base, _) = written_pack(&[(b"0123456789abcdef", 8), (b"second", 4)]);
    let p = decode_prefix(&base).unwrap();
    let sb = p.superblock;
    let slot = p.slot.offset();
    let len = base.len() as u64;
    assert_eq!(sb.log_end, len);
    let lies: Vec<Lie> = vec![
        (
            "past the file",
            slot,
            Box::new(move |s| s.log_end = len + 8),
            0,
        ),
        (
            "past the file by a lot",
            slot,
            Box::new(move |s| s.log_end = 1 << 40),
            0,
        ),
        ("u64::MAX", slot, Box::new(|s| s.log_end = u64::MAX), 0),
        (
            "u64::MAX aligned",
            slot,
            Box::new(|s| s.log_end = u64::MAX - 7),
            0,
        ),
        ("unaligned", slot, Box::new(move |s| s.log_end = len - 4), 0),
        (
            "below the frames",
            slot,
            Box::new(|s| s.log_end = FIRST_FRAME),
            0,
        ),
        (
            "the index frame overflowing",
            slot,
            Box::new(move |s| s.index_len = u64::MAX - s.index_off + 1),
            0,
        ),
        (
            "inside the last frame",
            sb.index_off,
            Box::new(move |s| {
                s.log_end = len - 8;
                s.index_len -= 8;
            }),
            0,
        ),
        (
            "past the committed frames, into zeros",
            sb.log_end,
            Box::new(move |s| {
                s.log_end = len + 128;
                s.index_off = len;
                s.index_len = 128;
            }),
            128,
        ),
    ];
    for (case, offset, lie, grow) in lies {
        let mut bytes = base.clone();
        bytes.resize(bytes.len() + grow, 0);
        edit_slot(&mut bytes, |s| lie(s));
        for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
            let (opened, longest, after) = open_bytes(&bytes, mode);
            let err = opened.map(|_| ()).unwrap_err();
            assert!(err.is_corruption(), "{case} {mode:?}: {err:?}");
            assert_eq!(
                err.corrupt_offset(),
                Some(offset),
                "{case} {mode:?}: {err:?}"
            );
            assert!(longest <= PREFIX, "{case} {mode:?}: {longest}");
            assert!(after == bytes, "{case} {mode:?}: the open wrote");
        }
    }
}

// --- 4. The same frames after log_end ---

/// After a committed blob, each frame of [`KINDS`] claiming each of [`WILD_LENGTHS`], or a whole sealed one past its
/// bound, is the end of the roll-forward: a read-write open truncates it, keeps the committed blob, reads in bounded
/// pieces and leaves nothing for the next open; a read-only open ignores it.
#[test]
fn a_tail_frame_of_any_length_is_truncated_unread() {
    let (prefix, id) = empty_blob_pack();
    let mut tails: Vec<(String, Vec<u8>)> = Vec::new();
    for kind in KINDS {
        for len in WILD_LENGTHS {
            let mut t = header(kind, len, 2).to_vec();
            t.resize(160, 0);
            tails.push((format!("kind {kind} claiming {len}"), t));
        }
    }
    let mut payload = empty_index(2, 2)[24..112].to_vec();
    payload.resize(89, 0);
    tails.push(("an index just over".into(), frame(7, 2, &payload)));
    payload.resize(1 << 20, 0);
    tails.push(("an index of 1 MiB".into(), frame(7, 2, &payload)));
    tails.push(("a frame of another kind".into(), frame(3, 2, &[0; 88])));
    for (case, tail) in tails {
        let mut bytes = prefix.clone();
        bytes.extend_from_slice(&tail);
        let (opened, longest, after) = open_bytes(&bytes, PackMode::ReadOnly);
        assert_eq!(opened.unwrap().blob_ids(), [id], "{case}");
        assert_eq!(
            longest, PREFIX,
            "{case}: a reader does not look past log_end"
        );
        assert!(after == bytes, "{case}");

        let fs = with_file(&bytes);
        let vfs = Counted::new(&fs);
        let store = vfs.open(PackMode::ReadWrite).unwrap();
        assert_eq!(store.blob_ids(), [id], "{case}");
        assert_eq!(store.recovery().rolled_forward, 0, "{case}");
        assert_eq!(
            store.recovery().truncated_from,
            Some(bytes.len() as u64),
            "{case}"
        );
        assert!(
            vfs.longest_read() <= SCAN_PIECE,
            "{case}: {}",
            vfs.longest_read()
        );
        drop(store);
        assert!(
            fs.read(Path::new(PACK)).unwrap() == prefix,
            "{case}: cut at log_end"
        );
        let again = vfs.open(PackMode::ReadWrite).unwrap();
        assert_eq!(again.recovery().truncated_from, None, "{case}");
        assert_eq!(again.blob_ids(), [id], "{case}");
    }
}

/// The longest index frame after four chunk frames (SPEC §9.3): an 8-byte head, an inline manifest of one 64 MiB
/// chunk (`80 + 64 MiB`, the largest manifest of at most four chunks), and four offsets.
const INDEX_BOUND_AFTER_FOUR_CHUNKS: u64 = 24 + 8 + (MAX_CHUNK + 80) + 4 * 8 + 8;

/// A whole tail bind (a damaged newer slot, ADR-0049 item 9: four chunk frames in all before it) followed by a sealed
/// index frame that continues the `frame_seq` run: the bind is rolled forward and the frame is truncated. At the bound
/// (four chunk frames before it) the frame is read whole and fails on its contents; 8 bytes over, it ends the run
/// unread, read only in the checksum pass's pieces. The next open finds the rolled-forward blob committed. Not under
/// Miri (64 MiB frames).
#[test]
fn a_tail_bind_then_an_index_frame_at_or_over_its_bound_rolls_forward_the_bind_only() {
    let (base, ms) = written_pack(&[(b"0123456789abcdef", 8), (b"second", 4)]);
    let p = decode_prefix(&base).unwrap();
    let next = p.superblock.next_frame_seq;
    let bind_end = base.len() as u64;
    for (case, total) in [
        ("at the bound", INDEX_BOUND_AFTER_FOUR_CHUNKS),
        ("over the bound", INDEX_BOUND_AFTER_FOUR_CHUNKS + 8),
    ] {
        let mut bytes = base.clone();
        // The newer slot rots: the open falls back to the slot of the first bind, so the second bind is the tail.
        bytes[p.slot.offset() as usize + 100] ^= 1;
        let mut payload = empty_index(next, 3)[24..112].to_vec();
        payload.resize((total - 32) as usize, 0);
        let f = frame(7, next, &payload);
        assert_eq!(f.len() as u64, total, "{case}");
        bytes.extend_from_slice(&f);
        drop(f);
        let fs = with_file(&bytes);
        let file_len = bytes.len() as u64;
        drop(bytes);
        let vfs = Counted::new(&fs);
        let store = vfs.open(PackMode::ReadWrite).unwrap();
        assert_eq!(store.recovery().rolled_forward, 1, "{case}");
        assert_eq!(store.recovery().truncated_from, Some(file_len), "{case}");
        if total > INDEX_BOUND_AFTER_FOUR_CHUNKS {
            assert!(
                vfs.longest_read() <= SCAN_PIECE,
                "{case}: {}",
                vfs.longest_read()
            );
        } else {
            assert_eq!(vfs.longest_read(), total, "{case}: read whole");
        }
        for (m, data) in ms.iter().zip([&b"0123456789abcdef"[..], b"second"]) {
            assert_eq!(
                read_range(&store, &reference(m), 0..m.size()).unwrap(),
                data,
                "{case}"
            );
        }
        assert_eq!(store.superblock().unwrap().log_end, bind_end, "{case}");
        drop(store);
        let again = vfs.open(PackMode::ReadWrite).unwrap();
        assert_eq!(again.recovery().rolled_forward, 0, "{case}");
        assert_eq!(again.recovery().truncated_from, None, "{case}");
        assert_eq!(again.blob_ids().len(), 2, "{case}");
    }
}

// --- 5. Properties ---

fn config() -> ProptestConfig {
    let default = if cfg!(feature = "slow") { 1024 } else { 96 };
    ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(default),
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x9214),
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// Payload lengths near the bounds and the edges of the integer types, and some anywhere.
fn payload_len() -> impl Strategy<Value = u64> {
    prop_oneof![
        3 => prop::sample::select(vec![
            0, 1, 31, 32, 33, 80, 81, 88, 89, 96, 97, 4096, 65_536, 65_537, 200_000,
            MAX_CHUNK + 32, MAX_CHUNK + 33, 0xFFFF_FFFF, u64::MAX - 39, u64::MAX - 7, u64::MAX,
        ]),
        2 => 0..300_000_u64,
        1 => any::<u64>(),
    ]
}

/// The bytes after the prefix, each the `fill` byte, as many as the claimed frame needs, cut at `cut` bytes (or all
/// of them, up to 300 KB).
#[derive(Debug, Clone)]
struct Crafted {
    on_empty_blob: bool,
    kind: u8,
    payload_len: u64,
    fill: u8,
    cut: Option<usize>,
    committed: bool,
}

fn crafted() -> impl Strategy<Value = Crafted> {
    (
        any::<bool>(),
        prop::sample::select(vec![6_u8, 7, 1, 3, 4, 0xEE]),
        payload_len(),
        prop_oneof![Just(0_u8), any::<u8>()],
        prop::option::weighted(0.2, 0..400_000_usize),
        any::<bool>(),
    )
        .prop_map(
            |(on_empty_blob, kind, payload_len, fill, cut, committed)| Crafted {
                on_empty_blob,
                kind,
                payload_len,
                fill,
                cut,
                committed,
            },
        )
}

/// The open's outcome is an index or a corruption-kind error: what a damaged pack may cause, never a caller-side or
/// I/O error.
fn check_err(e: &Error, case: &str) {
    assert!(
        e.is_corruption() || matches!(e, Error::PackUnsupported(_)),
        "{case}: {e:?}"
    );
}

proptest! {
    #![proptest_config(config())]

    /// Invariants 1, 2 and 4 on one generated frame: committed, every one fails the open (the slot names it as the
    /// last index frame, which no frame the generator makes is) and nothing longer than the prefix is read; as the tail,
    /// a read-write open keeps the prefix's blobs, truncates the frame and reads in bounded pieces.
    #[test]
    fn any_frame_after_a_valid_prefix_is_corrupt_or_truncated_with_bounded_reads(c in crafted()) {
        let (mut bytes, ids, seq) = if c.on_empty_blob {
            let (b, id) = empty_blob_pack();
            (b, vec![id], 2)
        } else {
            (new_pack(), vec![], 1)
        };
        let prefix_len = bytes.len();
        let whole = frame_len(c.payload_len)
            .and_then(|t| usize::try_from(t).ok())
            .filter(|&t| t <= 300_000);
        let frame_off = bytes.len() as u64;
        match whole {
            Some(total) if c.cut.is_none() => {
                let payload = vec![c.fill; c.payload_len as usize];
                let f = frame(c.kind, seq, &payload);
                prop_assert_eq!(f.len(), total);
                bytes.extend_from_slice(&f);
            }
            _ => {
                bytes.extend_from_slice(&header(c.kind, c.payload_len, seq));
                let n = c.cut.unwrap_or(256);
                bytes.resize(bytes.len() + n, c.fill);
                bytes.resize(bytes.len().next_multiple_of(8), 0);
            }
        }
        let committed = c.committed;
        if committed {
            let log_end = bytes.len() as u64;
            edit_slot(&mut bytes, |sb| {
                sb.seq += 1;
                sb.index_off = frame_off;
                sb.index_len = log_end - frame_off;
                sb.log_end = log_end;
                sb.blob_count = ids.len() as u64 + 1;
                sb.next_frame_seq = seq + 1;
            });
        }
        let case = format!("{c:?}");
        for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
            let (opened, longest, after) = open_bytes(&bytes, mode);
            if committed {
                let e = opened.map(|_| ()).expect_err(&case);
                check_err(&e, &case);
                prop_assert_eq!(longest, PREFIX, "{} {:?}", case, mode);
                prop_assert!(after == bytes, "{} {:?}: the open wrote", case, mode);
                continue;
            }
            let store = opened.unwrap_or_else(|e| panic!("{case} {mode:?}: {e:?}"));
            prop_assert_eq!(store.blob_ids(), ids.clone(), "{} {:?}", case, mode);
            prop_assert!(longest <= SCAN_PIECE.max(PREFIX), "{} {:?}: {}", case, mode, longest);
            if mode == PackMode::ReadWrite {
                prop_assert!(after[..] == bytes[..prefix_len], "{} {:?}: cut at log_end", case, mode);
            } else {
                prop_assert!(after == bytes, "{} {:?}: a reader wrote", case, mode);
            }
        }
    }
}

/// A pack's bytes and the blobs it holds, with their data.
type Template = (Vec<u8>, Vec<(BlobManifest, Vec<u8>)>);

/// A valid pack of three blobs whose newest slot rotted: two committed, and the third's whole bind after `log_end`,
/// which a read-write open rolls forward. What the edits of [`any_edits_of_a_valid_pack_open_or_fail_as_corrupt`]
/// start from, with each blob's data.
fn edit_template() -> &'static Template {
    static T: std::sync::OnceLock<Template> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let blobs: [(&[u8], u32); 3] = [
            (b"0123456789abcdef", 8),
            (b"second", 4),
            (b"a third blob, after log_end", 16),
        ];
        let (mut bytes, ms) = written_pack(&blobs);
        let p = decode_prefix(&bytes).unwrap();
        bytes[p.slot.offset() as usize + 200] ^= 0x40;
        let data = blobs.iter().map(|(d, _)| d.to_vec());
        (bytes, ms.into_iter().zip(data).collect())
    })
}

/// Where the frames of `bytes` are (offset, total length), walked by their lengths from 12288.
fn frames_of(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut frames = Vec::new();
    let mut at = PREFIX as usize;
    while at + 24 <= bytes.len() {
        let len = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap());
        let Some(total) = frame_len(len)
            .and_then(|t| usize::try_from(t).ok())
            .filter(|&t| t <= bytes.len() - at)
        else {
            break;
        };
        frames.push((at, total));
        at += total;
    }
    frames
}

/// Recomputes both slots' and every frame's CRC32C, so edits reach the field rules.
fn reseal(bytes: &mut [u8]) {
    for slot in [4096, 8192] {
        let crc = crc32c(&bytes[slot..slot + 4092]);
        bytes[slot + 4092..slot + 4096].copy_from_slice(&crc.to_le_bytes());
    }
    for (at, total) in frames_of(bytes) {
        let end = at + total - 8;
        let crc = crc32c(&bytes[at + 4..end]);
        bytes[end..end + 4].copy_from_slice(&crc.to_le_bytes());
    }
}

/// Every blob the store serves that the template bound reads back its own bytes or fails as corrupt (or unsupported); any other blob
/// a damaged index names reads without a panic.
fn check_reads<F: ContainerFile + 'static>(
    store: &PackStore<F>,
    blobs: &[(BlobManifest, Vec<u8>)],
    case: &str,
) {
    for id in store.blob_ids() {
        let Ok(Some(m)) = store.manifest(id) else {
            continue;
        };
        assert_eq!(m.blob_id(), id, "{case}");
        let Ok(r) = BlobRef::new(
            m.blob_id(),
            m.size(),
            "a/b",
            m.chunk_size().get(),
            BTreeMap::new(),
        ) else {
            continue;
        };
        let got = read_range(store, &r, 0..m.size().min(1 << 16));
        match blobs.iter().find(|(b, _)| b.blob_id() == id) {
            Some((_, data)) => match got {
                Ok(bytes) => assert!(bytes == *data, "{case}: blob {id:?} read other bytes"),
                // Unknown frame flags in an otherwise valid chunk frame are a newer writer's (SPEC §4.4).
                Err(e) => assert!(
                    e.is_corruption() || matches!(e, Error::PackUnsupported(_)),
                    "{case}: {e:?}"
                ),
            },
            None => {
                if let Err(e) = got {
                    assert!(
                        e.is_corruption() || matches!(e, Error::PackUnsupported(_)),
                        "{case}: {e:?}"
                    );
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(config())]

    /// Invariant 5 on byte edits of a valid pack with a tail bind, CRCs resealed or not: both opens succeed or fail
    /// with a corruption-kind error (or another database's or a read-only pack's, for header edits), every blob served
    /// reads back its own bytes or fails as corrupt or unsupported, and a recovered pack reopens to the same blobs with nothing more
    /// to do. Reads stay within the file.
    #[test]
    fn any_edits_of_a_valid_pack_open_or_fail_as_corrupt(
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
        seal in any::<bool>(),
    ) {
        let (template, blobs) = edit_template();
        let mut bytes = template.clone();
        for (at, value) in &edits {
            let at = at.index(bytes.len());
            bytes[at] = *value;
        }
        if seal {
            reseal(&mut bytes);
        }
        let case = format!("{edits:?} seal {seal}");
        let allowed = |e: &Error| {
            e.is_corruption()
                || matches!(
                    e,
                    Error::PackUnsupported(_) | Error::PackOfAnotherDatabase { .. } | Error::PackReadOnly
                )
        };
        let (opened, longest, after) = open_bytes(&bytes, PackMode::ReadOnly);
        prop_assert!(longest <= bytes.len() as u64, "{}: {}", case, longest);
        prop_assert!(after == bytes, "{}: a reader wrote", case);
        match opened {
            Ok(store) => check_reads(&store, blobs, &case),
            Err(e) => prop_assert!(allowed(&e), "{}: {:?}", case, e),
        }
        let fs = with_file(&bytes);
        let vfs = Counted::new(&fs);
        let store = match vfs.open(PackMode::ReadWrite) {
            Ok(store) => store,
            Err(e) => {
                prop_assert!(allowed(&e), "{}: {:?}", case, e);
                return Ok(());
            }
        };
        prop_assert!(vfs.longest_read() <= bytes.len() as u64, "{}", case);
        check_reads(&store, blobs, &case);
        let mut ids = store.blob_ids();
        ids.sort();
        drop(store);
        let again = vfs.open(PackMode::ReadWrite).unwrap_or_else(|e| panic!("{case}: a recovered pack: {e:?}"));
        prop_assert_eq!(again.recovery().rolled_forward, 0, "{}", case);
        prop_assert_eq!(again.recovery().truncated_from, None, "{}", case);
        let mut again_ids = again.blob_ids();
        again_ids.sort();
        prop_assert_eq!(again_ids, ids, "{}", case);
    }
}
