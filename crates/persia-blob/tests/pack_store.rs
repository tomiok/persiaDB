//! The pack's write path (SPEC §9.3, §18.9; ADR-0049; ROADMAP 9.2.2): lazy durable creation, staging, binding with
//! dedup, and per-blob durability.
//!
//! Invariants:
//! 1. No file until the first write; the first write creates the pack with no temporary file left behind.
//! 2. A staged chunk is never overwritten; `bind` dedups by id and writes nothing for a duplicate; it refuses a
//!    manifest whose chunks this handle did not stage as described.
//! 3. Round trip: any blobs at any chunk sizes, staged in any interleaving, read back byte for byte after a reopen.
//! 4. Crash at every mutating call (each write, sync, link, remove, directory sync), with every crash image: the pack
//!    reopens, every blob is wholly readable or absent, and every `Bound` answer survives.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use persia_blob::{
    BindOutcome, BlobManifest, BlobStore, ChunkSize, Error, PackMode, PackStore, UploadId,
    blob_id_of, read_range,
};
use persia_format::BlobRef;
use persia_format::checksum::crc32c;
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
        "application/octet-stream",
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

fn stage_all<F>(store: &PackStore<F>, m: &BlobManifest, data: &[u8]) -> persia_blob::Result<()>
where
    F: persia_storage::container::ContainerFile + 'static,
{
    for (i, c) in data.chunks(m.chunk_size().get() as usize).enumerate() {
        store.stage_chunk(m.upload_id(), i as u32, c)?;
    }
    Ok(())
}

fn store_blob<F>(store: &PackStore<F>, data: &[u8], chunk: u32, upload: u128) -> BlobManifest
where
    F: persia_storage::container::ContainerFile + 'static,
{
    let m = manifest_of(data, chunk, upload);
    stage_all(store, &m, data).unwrap();
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    m
}

fn read_all<S: BlobStore>(store: &S, m: &BlobManifest) -> persia_blob::Result<Vec<u8>> {
    read_range(store, &reference(m), 0..m.size())
}

#[test]
fn the_pack_is_created_by_the_first_write_and_leaves_no_temporary_file() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    assert!(!store.exists());
    assert_eq!(store.manifest(blob_id_of(b"x")).unwrap(), None);
    assert!(fs.read(Path::new(PACK)).is_none());
    store.stage_chunk(UploadId::new(1), 0, b"abc").unwrap();
    assert!(store.exists());
    let names: Vec<_> = fs.files().into_iter().map(|(p, _)| p).collect();
    assert_eq!(names, [Path::new(PACK)]);
    // The name is durable before anything is appended: a crash now keeps an empty pack.
    let crashed = fs.crash(CrashLoss::All);
    let reopened = open(&crashed, PackMode::ReadOnly).unwrap();
    assert!(reopened.exists());
    assert_eq!(reopened.superblock().unwrap().seq, 0);
}

#[test]
fn an_empty_blob_creates_the_pack_and_binds() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let m = store_blob(&store, b"", 1 << 16, 3);
    drop(store);
    let store = open(&fs.crash(CrashLoss::All), PackMode::ReadOnly).unwrap();
    assert_eq!(store.manifest(m.blob_id()).unwrap(), Some(m.clone()));
    assert_eq!(read_all(&store, &m).unwrap(), b"");
}

#[test]
fn a_bound_blob_is_durable_before_bind_returns() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let data: Vec<u8> = (0..1000_u32).map(|i| i as u8).collect();
    let m = store_blob(&store, &data, 64, 1);
    // Nothing unsynced survives: the blob must already be on "disk".
    let crashed = fs.crash(CrashLoss::All);
    let store = open(&crashed, PackMode::ReadOnly).unwrap();
    assert_eq!(read_all(&store, &m).unwrap(), data);
    assert_eq!(store.superblock().unwrap().blob_count, 1);
}

#[test]
fn a_second_upload_of_the_same_bytes_is_a_duplicate_and_writes_nothing() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let first = store_blob(&store, b"same bytes", 4, 1);
    let second = manifest_of(b"same bytes", 3, 2);
    stage_all(&store, &second, b"same bytes").unwrap();
    let before = fs.read(Path::new(PACK)).unwrap();
    let mutations = fs.mutations();
    assert_eq!(
        store.bind(&second).unwrap(),
        BindOutcome::AlreadyBound(first.clone())
    );
    assert_eq!(fs.mutations(), mutations);
    assert_eq!(fs.read(Path::new(PACK)).unwrap(), before);
    store.discard_upload(second.upload_id()).unwrap();
    // A discarded upload cannot be bound any more.
    assert!(matches!(
        store.bind(&manifest_of(b"other", 3, 2)),
        Err(Error::ChunkNotStaged { .. })
    ));
    assert_eq!(read_all(&store, &first).unwrap(), b"same bytes");
}

#[test]
fn a_staged_chunk_is_never_overwritten() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let upload = UploadId::new(9);
    store.stage_chunk(upload, 0, b"abcd").unwrap();
    store.stage_chunk(upload, 0, b"abcd").unwrap();
    assert!(matches!(
        store.stage_chunk(upload, 0, b"abce"),
        Err(Error::StagedChunkConflict { index: 0, .. })
    ));
    let m = manifest_of(b"abcd", 4, 9);
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    // Bound: the same rule, against the bound chunk.
    store.stage_chunk(upload, 0, b"abcd").unwrap();
    assert!(matches!(
        store.stage_chunk(upload, 0, b"zzzz"),
        Err(Error::StagedChunkConflict { .. })
    ));
    assert_eq!(read_all(&store, &m).unwrap(), b"abcd");
}

#[test]
fn bind_refuses_chunks_not_staged_as_the_manifest_describes() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let data = b"0123456789";
    let m = manifest_of(data, 4, 1);
    store.stage_chunk(m.upload_id(), 0, b"0123").unwrap();
    store.stage_chunk(m.upload_id(), 2, b"89").unwrap();
    assert!(matches!(
        store.bind(&m),
        Err(Error::ChunkNotStaged { index: 1, .. })
    ));
    store.stage_chunk(m.upload_id(), 1, b"456X").unwrap();
    assert!(matches!(
        store.bind(&m),
        Err(Error::ChunkNotStaged { index: 1, .. })
    ));
    let short = manifest_of(b"0123456", 4, 2);
    store.stage_chunk(short.upload_id(), 0, b"0123").unwrap();
    store.stage_chunk(short.upload_id(), 1, b"4567").unwrap();
    assert!(matches!(
        store.bind(&short),
        Err(Error::ChunkNotStaged { index: 1, .. })
    ));
    assert_eq!(store.superblock().unwrap().blob_count, 0);
    assert_eq!(store.blob_ids(), Vec::new());
}

#[test]
fn a_read_only_store_writes_nothing() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadOnly).unwrap();
    assert!(!store.exists());
    assert!(matches!(
        store.stage_chunk(UploadId::new(1), 0, b"a"),
        Err(Error::PackReadOnly)
    ));
    let m = manifest_of(b"", 4, 1);
    assert!(matches!(store.bind(&m), Err(Error::PackReadOnly)));
    assert!(!store.contains(&reference(&m)).unwrap());
    assert_eq!(fs.files(), Vec::new());
}

#[test]
fn a_failed_write_poisons_the_handle() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    store_blob(&store, b"first", 4, 1);
    fs.set_fault(Some(SimFault::FailAt(fs.mutations())));
    assert!(matches!(
        store.stage_chunk(UploadId::new(2), 0, b"x"),
        Err(Error::PackIo { .. })
    ));
    assert!(store.is_poisoned());
    fs.set_fault(None);
    assert!(matches!(
        store.stage_chunk(UploadId::new(2), 0, b"x"),
        Err(Error::PackPoisoned)
    ));
}

#[test]
fn a_real_file_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = persia_blob::pack_path(&dir.path().join("app.persia"));
    let store = PackStore::open(&path, PackMode::ReadWrite, [1; 16]).unwrap();
    let data: Vec<u8> = (0..200_000_u32).map(|i| (i % 251) as u8).collect();
    let m = store_blob(&store, &data, 65_536, 77);
    drop(store);
    let store = PackStore::open(&path, PackMode::ReadWrite, [1; 16]).unwrap();
    assert_eq!(store.header().unwrap().db_uuid, [1; 16]);
    assert_eq!(read_all(&store, &m).unwrap(), data);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "only the pack"
    );
}

// --- Round trips (proptest) ---

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: if cfg!(feature = "slow") { 512 } else { 48 },
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x9_2_2),
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

fn blobs() -> impl Strategy<Value = Vec<(Vec<u8>, u32)>> {
    proptest::collection::vec(
        (proptest::collection::vec(any::<u8>(), 0..600), 1..80_u32),
        1..6,
    )
}

proptest! {
    #![proptest_config(config())]

    /// Any blobs at any chunk sizes, their chunks staged interleaved in a seeded order, read back after a reopen.
    #[test]
    fn stage_bind_read_round_trips(blobs in blobs(), order in any::<u64>()) {
        let fs = sim();
        let store = open(&fs, PackMode::ReadWrite).unwrap();
        let manifests: Vec<_> = blobs
            .iter()
            .enumerate()
            .map(|(i, (data, chunk))| manifest_of(data, *chunk, i as u128 + 1))
            .collect();
        let mut work: Vec<(usize, u32)> = manifests
            .iter()
            .enumerate()
            .flat_map(|(b, m)| (0..m.chunk_count()).map(move |c| (b, c)))
            .collect();
        // A seeded shuffle.
        let mut s = order | 1;
        for i in (1..work.len()).rev() {
            s ^= s << 13; s ^= s >> 7; s ^= s << 17;
            work.swap(i, (s % (i as u64 + 1)) as usize);
        }
        for (b, c) in work {
            let (data, chunk) = &blobs[b];
            let at = c as usize * *chunk as usize;
            let bytes = &data[at..(at + *chunk as usize).min(data.len())];
            store.stage_chunk(manifests[b].upload_id(), c, bytes).unwrap();
        }
        let mut bound = BTreeMap::new();
        for (m, (data, _)) in manifests.iter().zip(&blobs) {
            match store.bind(m).unwrap() {
                BindOutcome::Bound => { bound.insert(m.blob_id(), (m.clone(), data.clone())); }
                BindOutcome::AlreadyBound(first) => prop_assert!(bound.contains_key(&first.blob_id())),
            }
        }
        drop(store);
        let store = open(&fs.crash(CrashLoss::All), PackMode::ReadWrite).unwrap();
        prop_assert_eq!(store.blob_ids().len(), bound.len());
        for (m, data) in bound.values() {
            prop_assert_eq!(store.manifest(m.blob_id()).unwrap(), Some(m.clone()));
            prop_assert_eq!(&read_all(&store, m).unwrap(), data);
        }
    }
}

// --- Crash tests (SimFs) ---

/// What a scripted session got from the store before the fault stopped it.
#[derive(Debug, Default)]
struct Session {
    bound: Vec<BlobManifest>,
}

const SCRIPT: [(&[u8], u32, u128); 3] = [
    (b"the first blob, in chunks of seven", 7, 1),
    (b"", 4, 2),
    (b"a second blob", 5, 3),
];

/// Runs the script until the first error; records each `Bound` answer.
fn run_script(store: &PackStore<SimFsFile>, session: &mut Session) {
    for (data, chunk, upload) in SCRIPT {
        let m = manifest_of(data, chunk, upload);
        if stage_all(store, &m, data).is_err() {
            return;
        }
        match store.bind(&m) {
            Ok(BindOutcome::Bound) => session.bound.push(m),
            Ok(BindOutcome::AlreadyBound(_)) => panic!("every blob of the script differs"),
            Err(_) => return,
        }
    }
}

/// After a crash: the pack opens, each blob of the script is wholly readable or absent, every `Bound` one is there.
fn check_recovered(fs: &SimFs, session: &Session, what: &str) {
    let store = open(fs, PackMode::ReadWrite).unwrap_or_else(|e| panic!("{what}: reopen: {e}"));
    for (data, chunk, upload) in SCRIPT {
        let m = manifest_of(data, chunk, upload);
        match store.manifest(m.blob_id()).unwrap() {
            Some(found) => {
                assert_eq!(found, m, "{what}");
                assert_eq!(read_all(&store, &m).unwrap(), data, "{what}");
            }
            None => assert!(!session.bound.contains(&m), "{what}: a Bound blob is gone"),
        }
    }
    // The recovered pack takes writes again.
    let m = manifest_of(b"after the crash", 4, 99);
    stage_all(&store, &m, b"after the crash").unwrap();
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
}

fn crash_at_every_call(seeds: u64) {
    let total = {
        let fs = sim();
        let start = fs.mutations();
        run_script(
            &open(&fs, PackMode::ReadWrite).unwrap(),
            &mut Session::default(),
        );
        fs.mutations() - start
    };
    assert!(total > 10, "the script makes {total} mutating calls");
    for k in 0..=total {
        for fault in [SimFault::FailAt, SimFault::ApplyThenFail] {
            let fs = sim();
            let store = open(&fs, PackMode::ReadWrite).unwrap();
            fs.set_fault(Some(fault(fs.mutations() + k)));
            let mut session = Session::default();
            run_script(&store, &mut session);
            drop(store);
            let losses = [CrashLoss::All, CrashLoss::Nothing]
                .into_iter()
                .chain((0..seeds).map(CrashLoss::Seeded));
            for loss in losses {
                let what = format!("{:?} at call {k}, {loss:?}", fault(k));
                check_recovered(&fs.crash(loss), &session, &what);
            }
        }
    }
}

#[test]
fn a_crash_at_any_call_keeps_bound_blobs_and_never_half_of_one() {
    crash_at_every_call(4);
}

#[cfg(feature = "slow")]
#[test]
fn a_crash_at_any_call_keeps_bound_blobs_and_never_half_of_one_many_seeds() {
    crash_at_every_call(64);
}
