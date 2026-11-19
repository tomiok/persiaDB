//! Opening a pack (SPEC §9.3; ADR-0049 item 9, 10; ROADMAP 9.2.4): the index rebuilt by a scan of the committed
//! frames, the tail after `log_end` truncated by a read-write open, corruption below `log_end` reported, and readers
//! catching up with `refresh`.
//!
//! Invariants:
//! 1. Everything after `log_end` (a torn frame, staged chunks, the index frame of a cut bind, garbage) is truncated by
//!    a read-write open and ignored by a read-only one, which writes nothing.
//! 2. Damage below `log_end` to a header, slot, frame header or index frame fails the open with `Corrupt`; damage to
//!    a chunk's bytes fails only the read of that chunk. Never a panic.
//! 3. After a crash anywhere in a random session, the index the scan rebuilds holds every `Bound` blob, and nothing
//!    but blobs whose bind was attempted, each wholly readable.
//! 4. `refresh` finds what the writer committed since, and reports a replaced pack (another generation).
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_blob::pack::layout::{FIRST_FRAME, PackSuperblock, decode_prefix};
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobStore, ChunkSize, Error, PackMode, PackStore, UploadId,
    blob_id_of, read_range,
};
use persia_format::BlobRef;
use persia_format::checksum::crc32c;
use persia_storage::container::ContainerFile;
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use proptest::prelude::*;

const PACK: &str = "/db/app.persia.blobs";

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

fn sim() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    fs
}

fn open(fs: &SimFs, mode: PackMode) -> persia_blob::Result<PackStore<SimFsFile>> {
    PackStore::open_in(Arc::new(fs.clone()), PACK, mode, [9; 16])
}

fn stage(store: &PackStore<SimFsFile>, m: &BlobManifest, data: &[u8]) -> persia_blob::Result<()> {
    for (i, c) in data.chunks(m.chunk_size().get() as usize).enumerate() {
        store.stage_chunk(m.upload_id(), i as u32, c)?;
    }
    Ok(())
}

fn put(store: &PackStore<SimFsFile>, data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
    let m = manifest_of(data, chunk, upload);
    stage(store, &m, data).unwrap();
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    m
}

fn read_all<S: BlobStore>(store: &S, m: &BlobManifest) -> persia_blob::Result<Vec<u8>> {
    read_range(store, &reference(m), 0..m.size())
}

/// A pack with two blobs, its bytes, and their manifests.
fn two_blobs() -> (Vec<u8>, BlobManifest, BlobManifest) {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let a = put(&store, b"0123456789abcdef", 8, 1);
    let b = put(&store, b"second", 4, 2);
    drop(store);
    (fs.read(Path::new(PACK)).unwrap(), a, b)
}

/// A file system holding `bytes` as the pack, durably.
fn with_pack(bytes: &[u8]) -> SimFs {
    let fs = sim();
    let f = fs.create_new(Path::new(PACK)).unwrap();
    f.write_all_at(bytes, 0).unwrap();
    f.sync_data().unwrap();
    fs.sync_dir(Path::new("/db")).unwrap();
    fs
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/blob-pack/v1")
        .join(name)
}

#[test]
fn the_golden_packs_open_and_read() {
    let fs = with_pack(&std::fs::read(fixture("one-blob.pack")).unwrap());
    let m = manifest_of(b"persia blobs!", 5, u128::MAX);
    for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
        let store = PackStore::open_in(Arc::new(fs.clone()), PACK, mode, [0x11; 16]).unwrap();
        assert_eq!(store.header().unwrap().db_uuid, [0x11; 16]);
        assert_eq!(store.blob_ids(), [m.blob_id()]);
        assert_eq!(read_all(&store, &m).unwrap(), b"persia blobs!");
    }
    let fs = with_pack(&std::fs::read(fixture("new.pack")).unwrap());
    let store =
        PackStore::open_in(Arc::new(fs.clone()), PACK, PackMode::ReadWrite, [0x11; 16]).unwrap();
    assert_eq!(store.blob_ids(), Vec::<BlobId>::new());
    // A new pack takes its first blob.
    let m = put(&store, b"first", 4, 1);
    assert_eq!(read_all(&store, &m).unwrap(), b"first");
}

#[test]
fn a_read_write_open_truncates_everything_after_log_end() {
    let (bytes, a, b) = two_blobs();
    let log_end = decode_prefix(&bytes).unwrap().superblock.log_end;
    assert_eq!(log_end, bytes.len() as u64);
    let tails: [Vec<u8>; 3] = [
        vec![0; 100],                           // a length that reached the disk before its data
        vec![0xA5; 37], // garbage (no quarantine: nothing after log_end was acknowledged)
        bytes[FIRST_FRAME as usize..].to_vec(), // whole valid frames with stale frame_seqs
    ];
    for tail in tails {
        let mut file = bytes.clone();
        file.extend(&tail);
        let fs = with_pack(&file);
        let ro = open(&fs, PackMode::ReadOnly).unwrap();
        assert_eq!(ro.recovery().truncated_from, None);
        assert_eq!(read_all(&ro, &a).unwrap(), b"0123456789abcdef");
        assert_eq!(
            fs.read(Path::new(PACK)).unwrap(),
            file,
            "a read-only open writes nothing"
        );
        let rw = open(&fs, PackMode::ReadWrite).unwrap();
        assert_eq!(rw.recovery().truncated_from, Some(file.len() as u64));
        assert_eq!(fs.read(Path::new(PACK)).unwrap(), bytes);
        assert_eq!(read_all(&rw, &b).unwrap(), b"second");
        // The truncation is durable before the handle writes: a crash right away keeps it.
        let crashed = fs.crash(CrashLoss::All);
        assert_eq!(crashed.read(Path::new(PACK)).unwrap(), bytes);
    }
}

#[test]
fn a_bind_cut_before_its_slot_is_rolled_forward_by_the_writer() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    put(&store, b"kept", 4, 1);
    let m = manifest_of(b"cut short", 4, 2);
    stage(&store, &m, b"cut short").unwrap();
    let staged = manifest_of(b"staged, never bound", 4, 3);
    // Calls of a bind: index append, sync, slot write, sync. Fail the slot write.
    fs.set_fault(Some(SimFault::FailAt(fs.mutations() + 2)));
    assert!(store.bind(&m).unwrap_err().is_bind_outcome_unknown());
    drop(store);
    // Everything written reached the disk except the slot; then a staged chunk of another upload.
    let crashed = fs.crash(CrashLoss::Nothing);
    let after = crashed.read(Path::new(PACK)).unwrap().len();
    let ro = open(&crashed, PackMode::ReadOnly).unwrap();
    assert_eq!(
        ro.manifest(m.blob_id()).unwrap(),
        None,
        "a reader never writes, nor rolls forward"
    );
    drop(ro);
    let store = open(&crashed, PackMode::ReadWrite).unwrap();
    assert_eq!(store.recovery().rolled_forward, 1);
    assert_eq!(store.recovery().truncated_from, None);
    assert_eq!(crashed.read(Path::new(PACK)).unwrap().len(), after);
    // The orphan is a whole blob: what a lost slot of an acknowledged bind would also have left.
    assert_eq!(store.manifest(m.blob_id()).unwrap(), Some(m.clone()));
    assert_eq!(read_all(&store, &m).unwrap(), b"cut short");
    stage(&store, &staged, b"staged, never bound").unwrap();
    drop(store);
    // Committed durably: a crash right away keeps it, and staged chunks after it are truncated.
    let again = crashed.crash(CrashLoss::Nothing);
    let store = open(&again, PackMode::ReadWrite).unwrap();
    assert_eq!(store.recovery().rolled_forward, 0);
    assert!(store.recovery().truncated_from.is_some());
    assert_eq!(store.blob_ids().len(), 2);
    let store = open(&again.crash(CrashLoss::All), PackMode::ReadOnly).unwrap();
    assert_eq!(read_all(&store, &m).unwrap(), b"cut short");
}

#[test]
fn a_damaged_newest_slot_does_not_lose_its_acknowledged_blob() {
    let (bytes, a, b) = two_blobs();
    // b's bind was acknowledged after its slot (B) synced; the slot then rots.
    let p = decode_prefix(&bytes).unwrap();
    assert_eq!(p.slot, persia_format::superblock::Slot::A);
    let mut rotted = bytes.clone();
    rotted[4096 + 100] ^= 1;
    let fs = with_pack(&rotted);
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    assert_eq!(store.recovery().rolled_forward, 1);
    for (m, data) in [(&a, &b"0123456789abcdef"[..]), (&b, b"second")] {
        assert_eq!(read_all(&store, m).unwrap(), data);
    }
}

#[test]
fn a_pack_of_another_database_is_refused() {
    let (bytes, _, _) = two_blobs();
    let fs = with_pack(&bytes);
    for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
        let err = PackStore::open_in(Arc::new(fs.clone()), PACK, mode, [8; 16])
            .map(|_| ())
            .unwrap_err();
        assert!(
            matches!(err, Error::PackOfAnotherDatabase { found } if found == [9; 16]),
            "{err:?}"
        );
    }
    assert_eq!(fs.read(Path::new(PACK)).unwrap(), bytes);
}

#[test]
fn a_read_only_open_beside_a_writer_sees_the_committed_blobs_only() {
    let fs = sim();
    let writer = open(&fs, PackMode::ReadWrite).unwrap();
    let a = put(&writer, b"committed", 4, 1);
    let staged = manifest_of(b"staged only", 4, 2);
    stage(&writer, &staged, b"staged only").unwrap();
    let reader = open(&fs, PackMode::ReadOnly).unwrap();
    assert_eq!(reader.blob_ids(), [a.blob_id()]);
    assert!(reader.read_chunk(&staged, 0).unwrap_err().is_corruption());
    // The writer goes on; the reader catches up.
    assert_eq!(writer.bind(&staged).unwrap(), BindOutcome::Bound);
    assert_eq!(reader.manifest(staged.blob_id()).unwrap(), None);
    assert!(reader.refresh().unwrap());
    assert_eq!(read_all(&reader, &staged).unwrap(), b"staged only");
    assert!(!reader.refresh().unwrap());
    assert!(!writer.refresh().unwrap());
}

#[test]
fn refresh_opens_a_pack_created_after_the_reader() {
    let fs = sim();
    let reader = open(&fs, PackMode::ReadOnly).unwrap();
    assert!(!reader.refresh().unwrap());
    let writer = open(&fs, PackMode::ReadWrite).unwrap();
    let m = put(&writer, b"new", 4, 1);
    assert!(reader.refresh().unwrap());
    assert_eq!(read_all(&reader, &m).unwrap(), b"new");
}

#[test]
fn refresh_reports_a_replaced_pack() {
    let (bytes, a, _) = two_blobs();
    let fs = with_pack(&bytes);
    let reader = open(&fs, PackMode::ReadOnly).unwrap();
    // What a rewrite (ROADMAP 9.5.2) leaves: another file, generation + 1, renamed over the pack.
    let mut rewritten = bytes.clone();
    let mut sb = decode_prefix(&bytes).unwrap().superblock;
    sb.generation = 2;
    rewritten[4096..8192].copy_from_slice(&sb.encode());
    rewritten[8192..12288].copy_from_slice(&sb.encode());
    let tmp = Path::new("/db/app.persia.blobs.compact");
    let f = fs.create_new(tmp).unwrap();
    f.write_all_at(&rewritten, 0).unwrap();
    f.sync_data().unwrap();
    fs.rename(tmp, Path::new(PACK)).unwrap();
    assert!(matches!(
        reader.refresh(),
        Err(Error::PackReplaced {
            opened: 1,
            found: 2
        })
    ));
    // The reader still reads its own file.
    assert_eq!(read_all(&reader, &a).unwrap(), b"0123456789abcdef");
    let reopened = open(&fs, PackMode::ReadOnly).unwrap();
    assert_eq!(reopened.superblock().unwrap().generation, 2);
}

#[test]
fn a_failed_refresh_keeps_what_the_reader_had() {
    let fs = sim();
    let writer = open(&fs, PackMode::ReadWrite).unwrap();
    let a = put(&writer, b"first", 4, 1);
    let reader = open(&fs, PackMode::ReadOnly).unwrap();
    let b = put(&writer, b"second", 4, 2);
    let c = put(&writer, b"third", 4, 3);
    drop(writer);
    // Damage c's index frame (committed, the last): the refresh scan indexes b, then fails at c.
    let mut bytes = fs.read(Path::new(PACK)).unwrap();
    let sb = decode_prefix(&bytes).unwrap().superblock;
    let c_index = sb.index_off as usize;
    bytes[c_index + 40] ^= 1;
    let f = fs.open(Path::new(PACK), true).unwrap();
    f.write_all_at(&bytes, 0).unwrap();
    assert!(reader.refresh().unwrap_err().is_corruption());
    assert_eq!(reader.blob_ids(), [a.blob_id()]);
    assert_eq!(reader.manifest(b.blob_id()).unwrap(), None);
    assert_eq!(reader.manifest(c.blob_id()).unwrap(), None);
    assert_eq!(read_all(&reader, &a).unwrap(), b"first");
}

/// Re-seals slot B of `bytes` (the active one after two binds) with `edit` applied.
fn edit_slot(bytes: &[u8], edit: impl FnOnce(&mut PackSuperblock)) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let p = decode_prefix(bytes).unwrap();
    let mut sb = p.superblock;
    edit(&mut sb);
    let at = if p.slot == persia_format::superblock::Slot::A {
        4096
    } else {
        8192
    };
    out[at..at + 4096].copy_from_slice(&sb.encode());
    out
}

#[test]
fn damage_below_log_end_fails_the_open() {
    let (bytes, a, _) = two_blobs();
    let sb = decode_prefix(&bytes).unwrap().superblock;
    let first = FIRST_FRAME as usize;
    let index = sb.index_off as usize;
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    let flip = |at: usize| {
        let mut b = bytes.clone();
        b[at] ^= 0x10;
        b
    };
    cases.push(("header", flip(100)));
    cases.push(("chunk frame magic", flip(first)));
    cases.push(("chunk frame length", flip(first + 9)));
    cases.push(("chunk frame seq", flip(first + 16)));
    cases.push(("index frame payload", flip(index + 30)));
    cases.push(("index frame crc", flip(sb.log_end as usize - 8)));
    cases.push(("blob_count", edit_slot(&bytes, |s| s.blob_count += 1)));
    cases.push((
        "index_off",
        edit_slot(&bytes, |s| s.index_off = FIRST_FRAME),
    ));
    cases.push((
        "next_frame_seq",
        edit_slot(&bytes, |s| s.next_frame_seq += 1),
    ));
    cases.push((
        "log_end inside a frame",
        edit_slot(&bytes, |s| s.log_end -= 8),
    ));
    cases.push((
        "log_end past the file",
        edit_slot(&bytes, |s| s.log_end += 8),
    ));
    cases.push(("short file", bytes[..12000].to_vec()));
    for (what, file) in cases {
        for mode in [PackMode::ReadOnly, PackMode::ReadWrite] {
            let fs = with_pack(&file);
            let err = open(&fs, mode).map(|_| ()).unwrap_err();
            assert!(err.is_corruption(), "{what}: {err:?}");
            assert_eq!(
                fs.read(Path::new(PACK)).unwrap(),
                file,
                "{what}: nothing written"
            );
        }
    }
    // A chunk's bytes are checked when read, not at open.
    let damaged = flip(first + 24 + 32);
    let store = open(&with_pack(&damaged), PackMode::ReadOnly).unwrap();
    assert!(read_all(&store, &a).unwrap_err().is_corruption());
}

#[test]
fn a_newer_format_is_unsupported_not_corrupt() {
    let (bytes, _, _) = two_blobs();
    let mut newer = bytes.clone();
    newer[8] = 2; // format_major
    let err = open(&with_pack(&newer), PackMode::ReadOnly)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::PackUnsupported(_)), "{err:?}");
}

#[test]
fn a_container_is_not_a_pack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(persia_storage::container::Container::create(&path).unwrap());
    let err = PackStore::open(&path, PackMode::ReadOnly, [0; 16])
        .map(|_| ())
        .unwrap_err();
    assert!(err.is_corruption(), "{err:?}");
}

// --- Random sessions with a crash (proptest) ---

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: if cfg!(feature = "slow") { 512 } else { 64 },
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x9_2_4),
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

#[derive(Debug, Clone)]
enum Step {
    /// Stage all of blob `i`'s chunks, then bind it.
    Put(usize),
    /// Stage some chunks of blob `i` and never bind them.
    Stage(usize),
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    proptest::collection::vec(
        prop_oneof![3 => (0..6_usize).prop_map(Step::Put), 1 => (0..6_usize).prop_map(Step::Stage)],
        1..10,
    )
}

fn blob(i: usize) -> (Vec<u8>, u32) {
    let data: Vec<u8> = (0..(i * 37 % 200)).map(|b| (b * (i + 3)) as u8).collect();
    (data, 1 + (i as u32 * 7) % 40)
}

proptest! {
    #![proptest_config(config())]

    /// A crash at a random call of a random session, with a random crash image: the index rebuilt by the scan is
    /// every Bound blob plus, at most, blobs whose bind was attempted, each wholly readable.
    #[test]
    fn the_rebuilt_index_matches_what_was_committed(
        steps in steps(),
        at in 0..200_u64,
        apply in any::<bool>(),
        loss in prop_oneof![Just(CrashLoss::All), Just(CrashLoss::Nothing), any::<u64>().prop_map(CrashLoss::Seeded)],
    ) {
        let fs = sim();
        let store = open(&fs, PackMode::ReadWrite).unwrap();
        let k = fs.mutations() + at;
        fs.set_fault(Some(if apply { SimFault::ApplyThenFail(k) } else { SimFault::FailAt(k) }));
        let mut bound = BTreeSet::new();
        let mut attempted = BTreeSet::new();
        let mut upload = 0_u128;
        for step in &steps {
            upload += 1;
            let (i, put) = match step { Step::Put(i) => (*i, true), Step::Stage(i) => (*i, false) };
            let (data, chunk) = blob(i);
            let m = manifest_of(&data, chunk, upload);
            let staged = if put { stage(&store, &m, &data) } else {
                data.chunks(chunk as usize).take(1).try_for_each(|c| store.stage_chunk(m.upload_id(), 0, c))
            };
            if staged.is_err() { break; }
            if put {
                attempted.insert(m.blob_id());
                match store.bind(&m) {
                    Ok(BindOutcome::Bound) => { bound.insert(m.blob_id()); }
                    Ok(BindOutcome::AlreadyBound(first)) => prop_assert!(bound.contains(&first.blob_id())),
                    Err(_) => break,
                }
            }
        }
        drop(store);
        let crashed = fs.crash(loss);
        let store = open(&crashed, PackMode::ReadWrite).unwrap();
        let found: BTreeSet<_> = store.blob_ids().into_iter().collect();
        prop_assert!(bound.is_subset(&found), "lost a Bound blob");
        prop_assert!(found.is_subset(&attempted), "a blob nobody bound");
        for i in 0..6 {
            let (data, chunk) = blob(i);
            if let Some(m) = store.manifest(blob_id_of(&data)).unwrap() {
                prop_assert_eq!(m.chunk_size().get(), chunk);
                prop_assert_eq!(read_all(&store, &m).unwrap(), data);
            }
        }
        // A read-only reopen rebuilds the same index.
        drop(store);
        let again = open(&crashed, PackMode::ReadOnly).unwrap();
        prop_assert_eq!(again.blob_ids().into_iter().collect::<BTreeSet<_>>(), found);
    }

    /// Any byte of a committed pack rewritten: the open and every read return a value or an error, never panic, and
    /// a blob read back whole is its bytes (the CRCs and the manifest checks catch the rest).
    #[test]
    fn a_damaged_pack_never_panics(at in 0..1000_usize, value in any::<u8>()) {
        let (mut bytes, a, b) = two_blobs();
        let at = at % bytes.len();
        bytes[at] = value;
        if let Ok(store) = open(&with_pack(&bytes), PackMode::ReadOnly) {
            for (m, data) in [(&a, &b"0123456789abcdef"[..]), (&b, b"second")] {
                if let Ok(got) = read_all(&store, m) {
                    prop_assert_eq!(got, data);
                }
            }
        }
    }
}

#[test]
fn refresh_reports_a_recreated_pack_of_the_same_generation() {
    let fs = sim();
    let writer = open(&fs, PackMode::ReadWrite).unwrap();
    put(&writer, b"old", 4, 1);
    drop(writer);
    let reader = open(&fs, PackMode::ReadOnly).unwrap();
    // The pack is removed and a new one created: generation 1 again, another pack id.
    fs.remove_file(Path::new(PACK)).unwrap();
    let writer = open(&fs, PackMode::ReadWrite).unwrap();
    put(&writer, b"new", 4, 2);
    assert_eq!(writer.superblock().unwrap().generation, 1);
    assert!(matches!(
        reader.refresh(),
        Err(Error::PackReplaced {
            opened: 1,
            found: 1
        })
    ));
}

// --- Review fix-ups: crashes during recovery, partial roll-forward, refresh after a roll-forward ---

/// A pack whose tail holds a staged chunk, then the whole of a bind cut at its slot write (blob `b"cut"`), then
/// garbage: a read-write open rolls the bind forward and truncates the garbage. Returns the bytes and the two blobs.
fn pack_with_a_cut_bind() -> (Vec<u8>, BlobManifest, BlobManifest) {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let a = put(&store, b"committed", 4, 1);
    store
        .stage_chunk(UploadId::new(9), 0, b"never bound")
        .unwrap();
    let b = manifest_of(b"cut bind", 4, 2);
    stage(&store, &b, b"cut bind").unwrap();
    // A bind's calls: index append, sync, slot write, slot sync.
    fs.set_fault(Some(SimFault::FailAt(fs.mutations() + 2)));
    assert!(store.bind(&b).unwrap_err().is_bind_outcome_unknown());
    drop(store);
    let mut bytes = fs.crash(CrashLoss::Nothing).read(Path::new(PACK)).unwrap();
    bytes.extend([0xA5; 40]);
    (bytes, a, b)
}

/// The `frame_seq` a frame appended after every valid frame of `bytes` from its committed `log_end` would take.
fn next_tail_seq(bytes: &[u8]) -> (u64, u64) {
    use persia_format::frame::FrameScanner;
    let sb = decode_prefix(bytes).unwrap().superblock;
    let mut scanner = FrameScanner::new(&bytes[sb.log_end as usize..], sb.log_end)
        .expect_frame_seq(sb.next_frame_seq);
    let mut end = sb.log_end;
    for frame in scanner.by_ref() {
        end = frame.end();
    }
    (scanner.next_frame_seq().unwrap(), end)
}

#[test]
fn a_crash_during_recovery_recovers_the_same_blobs_idempotently() {
    let (bytes, a, b) = pack_with_a_cut_bind();
    // Recovery's mutating calls: truncate, sync, roll-forward slot write, its sync.
    for k in 0..4_u64 {
        for apply in [false, true] {
            let fs = with_pack(&bytes);
            let at = fs.mutations() + k;
            fs.set_fault(Some(if apply {
                SimFault::ApplyThenFail(at)
            } else {
                SimFault::FailAt(at)
            }));
            let first = open(&fs, PackMode::ReadWrite);
            fs.set_fault(None);
            assert!(first.is_err(), "call {k}: the open sees its own fault");
            for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(k)] {
                let what = format!("call {k}, applied {apply}, {loss:?}");
                let crashed = fs.crash(loss);
                let store =
                    open(&crashed, PackMode::ReadWrite).unwrap_or_else(|e| panic!("{what}: {e}"));
                for (m, data) in [(&a, &b"committed"[..]), (&b, b"cut bind")] {
                    assert_eq!(read_all(&store, m).unwrap(), data, "{what}");
                }
                drop(store);
                let again = open(&crashed, PackMode::ReadWrite).unwrap();
                assert_eq!(again.recovery().rolled_forward, 0, "{what}: idempotent");
                assert_eq!(again.blob_ids().len(), 2, "{what}");
            }
        }
    }
}

#[test]
fn roll_forward_keeps_the_earlier_of_two_tail_binds_when_the_second_is_incomplete() {
    let (bytes, a, b) = pack_with_a_cut_bind();
    let (seq, end) = next_tail_seq(&bytes);
    let mut file = bytes[..end as usize].to_vec();
    // A second index frame after the cut bind, CRC-valid, whose chunk is not its own: the committed blob's first
    // chunk frame, of another upload.
    let c = manifest_of(b"none", 4, 3);
    file.extend(persia_blob::pack::layout::encode_index_frame(seq, &c, &[FIRST_FRAME]).unwrap());
    let fs = with_pack(&file);
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    assert_eq!(store.recovery().rolled_forward, 1);
    assert_eq!(store.recovery().truncated_from, Some(file.len() as u64));
    assert_eq!(store.manifest(c.blob_id()).unwrap(), None);
    assert_eq!(read_all(&store, &a).unwrap(), b"committed");
    assert_eq!(read_all(&store, &b).unwrap(), b"cut bind");
    assert_eq!(fs.read(Path::new(PACK)).unwrap().len() as u64, end);
}

#[test]
fn refresh_finds_what_a_writer_rolled_forward_after_a_damaged_newest_slot() {
    let (bytes, a, b) = two_blobs();
    let mut rotted = bytes.clone();
    rotted[4096 + 100] ^= 1;
    let fs = with_pack(&rotted);
    let reader = open(&fs, PackMode::ReadOnly).unwrap();
    assert_eq!(reader.blob_ids(), [a.blob_id()]);
    let writer = open(&fs, PackMode::ReadWrite).unwrap();
    assert_eq!(writer.recovery().rolled_forward, 1);
    assert!(reader.refresh().unwrap());
    assert_eq!(read_all(&reader, &b).unwrap(), b"second");
    assert_eq!(read_all(&reader, &a).unwrap(), b"0123456789abcdef");
}

#[test]
fn refresh_rejects_the_same_seq_with_other_contents() {
    let (bytes, _, _) = two_blobs();
    let fs = with_pack(&bytes);
    let reader = open(&fs, PackMode::ReadOnly).unwrap();
    let p = decode_prefix(&bytes).unwrap();
    // The active slot rewritten with its own seq and another blob_count: two commits never share a seq.
    let mut sb = p.superblock;
    sb.blob_count -= 1;
    let at = if p.slot == persia_format::superblock::Slot::A { 4096 } else { 8192 };
    let f = fs.open(Path::new(PACK), true).unwrap();
    f.write_all_at(&sb.encode(), at).unwrap();
    assert!(reader.refresh().unwrap_err().is_corruption());
}
