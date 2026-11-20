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
    F: ContainerFile + 'static,
{
    for (i, c) in data.chunks(m.chunk_size().get() as usize).enumerate() {
        store.stage_chunk(m.upload_id(), i as u32, c)?;
    }
    Ok(())
}

fn store_blob<F>(store: &PackStore<F>, data: &[u8], chunk: u32, upload: u128) -> BlobManifest
where
    F: ContainerFile + 'static,
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
    assert!(temporaries(fs).is_empty(), "{what}: swept at the open");
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

// --- Concurrent binds (coordinator's contract rule: only durable manifests are ever exposed) ---

/// Two threads bind the same blob at once while the first slot sync fails. `bind` holds the writer lock across
/// append, both syncs and the publish, so no thread can see the entry before its commit is durable: an
/// `AlreadyBound` answer always comes with a `Bound` one, and a failed bind never lends its manifest to the other.
#[test]
fn a_concurrent_bind_never_dedups_onto_a_blob_whose_commit_failed() {
    for round in 0..40_u64 {
        let fs = sim();
        let store = Arc::new(open(&fs, PackMode::ReadWrite).unwrap());
        let data = b"the same bytes, twice";
        let (a, b) = (manifest_of(data, 4, 1), manifest_of(data, 4, 2));
        stage_all(&store, &a, data).unwrap();
        stage_all(&store, &b, data).unwrap();
        // The first bind's calls: index append, sync, slot write, slot sync. The slot sync takes effect, then fails
        // (round % 2 == 0) or fails outright: either way that bind's outcome is unknown and the handle is poisoned.
        let k = fs.mutations() + 3;
        fs.set_fault(Some(if round % 2 == 0 {
            SimFault::ApplyThenFail(k)
        } else {
            SimFault::FailAt(k)
        }));
        let threads: Vec<_> = [a.clone(), b.clone()]
            .into_iter()
            .map(|m| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || store.bind(&m))
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let bound = results
            .iter()
            .filter(|r| matches!(r, Ok(BindOutcome::Bound)))
            .count();
        let deduped = results
            .iter()
            .filter(|r| matches!(r, Ok(BindOutcome::AlreadyBound(_))))
            .count();
        assert_eq!(
            bound, 0,
            "round {round}: the first bind failed, the second ran on a poisoned handle"
        );
        assert_eq!(
            deduped, 0,
            "round {round}: AlreadyBound onto a commit that never synced: {results:?}"
        );
        // Both read as outcome-unknown: the second ran on a handle poisoned by the first's unknown commit of the same
        // id, which only a reopen settles (ADR-0049 item 6). Neither may read as "not bound".
        assert!(
            results
                .iter()
                .all(|r| r.as_ref().is_err_and(Error::is_bind_outcome_unknown)),
            "{results:?}"
        );
        drop(store);
        // Whatever the crash keeps, the reopened pack holds the blob whole or not at all.
        for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(round)] {
            let store = open(&fs.crash(loss), PackMode::ReadWrite).unwrap();
            if let Some(m) = store.manifest(a.blob_id()).unwrap() {
                assert_eq!(read_all(&store, &m).unwrap(), data);
            }
        }
    }
}

/// Review blocker: an upload binds one blob. Binding a second manifest under a bound upload (the empty blob needs no
/// staged chunk, so nothing else stops it) must be refused, or the pack would hold two index frames of one upload
/// and fail every later open.
#[test]
fn a_bound_upload_cannot_bind_another_blob() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let first = store_blob(&store, b"first blob", 4, 7);
    let empty = manifest_of(b"", 4, 7);
    assert!(matches!(
        store.bind(&empty),
        Err(Error::InvalidManifest { .. })
    ));
    assert!(matches!(
        store.bind_small(&empty, b""),
        Err(Error::InvalidManifest { .. })
    ));
    let small = manifest_of(b"tiny", 4, 7);
    assert!(store.bind_small(&small, b"tiny").is_err());
    assert!(!store.is_poisoned());
    drop(store);
    let store = open(&fs.crash(CrashLoss::Nothing), PackMode::ReadWrite).unwrap();
    assert_eq!(store.blob_ids(), [first.blob_id()]);
    assert_eq!(read_all(&store, &first).unwrap(), b"first blob");
    // The empty blob binds under its own upload.
    assert_eq!(
        store.bind(&manifest_of(b"", 4, 8)).unwrap(),
        BindOutcome::Bound
    );
}

/// A fault at each step of the pack's creation (create, header write, sync, link, removal of the temporary name,
/// directory sync), applied or not: no temporary name is left behind once the process survives the failure, and a
/// retry on a fresh handle (or the same one, when nothing was linked) creates or finds a working pack.
#[test]
fn a_failed_creation_leaves_no_temporary_file_and_can_be_retried() {
    for k in 0..6_u64 {
        for apply in [false, true] {
            let fs = sim();
            let store = open(&fs, PackMode::ReadWrite).unwrap();
            let at = fs.mutations() + k;
            fs.set_fault(Some(if apply {
                SimFault::ApplyThenFail(at)
            } else {
                SimFault::FailAt(at)
            }));
            let failed = store.stage_chunk(UploadId::new(1), 0, b"x");
            fs.set_fault(None);
            let what = format!("call {k}, applied {apply}: {failed:?}");
            let crashed = fs.crash(CrashLoss::Nothing);
            // The SimFs fault ends the "process" at the first failing call, so later calls of the cleanup fail too:
            // what a restart sees is checked, the live view is not.
            let names: Vec<_> = crashed.files().into_iter().map(|(p, _)| p).collect();
            assert!(
                names
                    .iter()
                    .all(|p| p == Path::new(PACK) || p.to_string_lossy().contains(".tmp.")),
                "{what}: {names:?}"
            );
            // ROADMAP 9.2.6: the read-write open sweeps what the crash left, durably.
            let left = temporaries(&crashed);
            let reopened = open(&crashed, PackMode::ReadWrite).unwrap();
            assert_eq!(
                reopened.recovery().swept,
                left.len() as u64,
                "{what}: {left:?}"
            );
            assert!(temporaries(&crashed).is_empty(), "{what}");
            assert!(
                temporaries(&crashed.crash(CrashLoss::All)).is_empty(),
                "{what}"
            );
            let m = manifest_of(b"retry", 4, 2);
            stage_all(&reopened, &m, b"retry").unwrap();
            assert_eq!(reopened.bind(&m).unwrap(), BindOutcome::Bound, "{what}");
        }
    }
}

/// The cleanup of a creation the process survives: a link that finds the name taken (a second writer created a pack
/// there, outside the contract) removes its temporary file and stops the handle; a reopen finds the other pack.
#[test]
fn a_failed_link_removes_its_temporary_file_and_poisons() {
    let fs = sim();
    let late = open(&fs, PackMode::ReadWrite).unwrap();
    let early = open(&fs, PackMode::ReadWrite).unwrap();
    early.stage_chunk(UploadId::new(1), 0, b"first").unwrap();
    let err = late.stage_chunk(UploadId::new(2), 0, b"x").unwrap_err();
    assert!(matches!(err, Error::PackIo { .. }), "{err:?}");
    assert!(late.is_poisoned());
    let names: Vec<_> = fs.files().into_iter().map(|(p, _)| p).collect();
    assert_eq!(names, [Path::new(PACK)], "no temporary file left");
    assert!(open(&fs, PackMode::ReadWrite).unwrap().exists());
}

/// ROADMAP 9.4.7: after a failed slot sync the pack refuses every write until it is reopened, and says so through
/// `Error::is_store_poisoned`, which `put_blob` reads to stop retrying at once. A bind of the id whose commit failed
/// stays outcome-unknown; every other write is a plain refusal.
#[test]
fn a_pack_poisoned_by_a_failed_slot_sync_reads_as_a_poisoned_store() {
    for apply in [false, true] {
        let fs = sim();
        let store = open(&fs, PackMode::ReadWrite).unwrap();
        let data = b"a blob whose commit fails";
        let m = manifest_of(data, 8, 1);
        stage_all(&store, &m, data).unwrap();
        // The bind's calls: index append, data sync, slot write, slot sync.
        let k = fs.mutations() + 3;
        fs.set_fault(Some(if apply {
            SimFault::ApplyThenFail(k)
        } else {
            SimFault::FailAt(k)
        }));
        let first = store.bind(&m).unwrap_err();
        assert!(first.is_bind_outcome_unknown(), "{first}");
        assert!(
            !first.is_store_poisoned(),
            "the failure itself is the sync's: {first}"
        );
        fs.set_fault(None);
        let again = store.bind(&m).unwrap_err();
        assert!(
            again.is_bind_outcome_unknown() && again.is_store_poisoned(),
            "{again}"
        );
        let small = manifest_of(b"tiny", 8, 2);
        for refused in [
            store.bind_small(&small, b"tiny").unwrap_err(),
            store.bind(&manifest_of(b"", 8, 3)).unwrap_err(),
            store.stage_chunk(UploadId::new(4), 0, b"x").unwrap_err(),
        ] {
            assert!(refused.is_store_poisoned(), "{refused}");
            assert!(!refused.is_bind_outcome_unknown(), "{refused}");
        }
    }
}

// --- Stale temporary files (ROADMAP 9.2.6) ---

/// The pack's temporary names (`<pack>.tmp.*`) on `fs`, files only.
fn temporaries(fs: &SimFs) -> Vec<std::path::PathBuf> {
    let prefix = format!("{PACK}.tmp.");
    fs.files()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.to_string_lossy().starts_with(&prefix))
        .collect()
}

fn write_file(fs: &SimFs, path: &str) {
    let file = fs.create_new(Path::new(path)).unwrap();
    file.write_all_at(b"left by a crash", 0).unwrap();
    file.sync_data().unwrap();
}

/// A read-write open removes exactly the names a pack creation or rewrite makes (`<pack>.tmp.<16 hex>.<n>`), with or
/// without a pack, and syncs the directory; a read-only open removes nothing, and nothing else is touched.
#[test]
fn a_read_write_open_sweeps_the_stale_temporary_files_of_the_pack() {
    let stale = [
        format!("{PACK}.tmp.0123456789abcdef.0"),
        format!("{PACK}.tmp.fedcba9876543210.17"),
    ];
    let kept = [
        format!("{PACK}.tmp.0123456789ABCDEF.1"),
        format!("{PACK}.tmp.0123456789abcdef"),
        format!("{PACK}.compact.0123456789abcdef.1"),
        "/db/app.persia.tmp.0123456789abcdef.1".to_owned(),
        "/db/other.persia.blobs.tmp.0123456789abcdef.1".to_owned(),
    ];
    for with_pack in [false, true] {
        let fs = sim();
        if with_pack {
            store_blob(&open(&fs, PackMode::ReadWrite).unwrap(), b"kept", 4, 1);
        }
        for name in stale.iter().chain(&kept) {
            write_file(&fs, name);
        }
        // A directory with a temporary file's name is not one.
        fs.create_dir(Path::new(&format!("{PACK}.tmp.aaaaaaaaaaaaaaaa.2")))
            .unwrap();
        fs.sync_dir(Path::new("/db")).unwrap();
        let before = fs.files();

        let reader = open(&fs, PackMode::ReadOnly).unwrap();
        assert_eq!(reader.recovery().swept, 0);
        assert_eq!(fs.files(), before, "a read-only open writes nothing");

        let writer = open(&fs, PackMode::ReadWrite).unwrap();
        assert_eq!(writer.recovery().swept, 2, "with a pack: {with_pack}");
        assert_eq!(writer.exists(), with_pack);
        let names: Vec<String> = fs
            .files()
            .into_iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        for name in &stale {
            assert!(!names.contains(name), "{name} swept");
        }
        for name in &kept {
            assert!(names.contains(name), "{name} kept");
        }
        // The removals are durable: the directory was synced.
        let crashed = fs.crash(CrashLoss::All);
        assert_eq!(temporaries(&crashed).len(), 2, "only the two look-alikes");
        assert!(
            crashed
                .is_dir(Path::new(&format!("{PACK}.tmp.aaaaaaaaaaaaaaaa.2")))
                .unwrap()
        );
        if with_pack {
            let m = manifest_of(b"kept", 4, 1);
            assert_eq!(read_all(&writer, &m).unwrap(), b"kept");
        }
        drop(writer);
        assert_eq!(open(&fs, PackMode::ReadWrite).unwrap().recovery().swept, 0);
    }
}

// --- A file system that counts reads and fails chosen calls (review of ROADMAP 9.2.6, 9.2.9) ---

/// Which `Vfs` calls fail, once set.
#[derive(Debug, Default)]
struct VfsFaults {
    remove: Option<std::path::PathBuf>,
    sync_dir: bool,
    read_dir: bool,
    /// `remove_file` of a pack temporary name deletes it, then reports `NotFound`.
    remove_not_found: bool,
}

/// `SimFs` with failures injected into chosen calls, and a count of `read_exact_at` calls on its files.
#[derive(Debug, Clone)]
struct Wrapped {
    fs: SimFs,
    faults: Arc<std::sync::Mutex<VfsFaults>>,
    reads: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug)]
struct CountedFile {
    file: SimFsFile,
    reads: Arc<std::sync::atomic::AtomicU64>,
}

use persia_storage::container::ContainerFile;
use std::io;

impl ContainerFile for CountedFile {
    fn file_len(&self) -> io::Result<u64> {
        self.file.file_len()
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

impl Wrapped {
    fn new(fs: &SimFs) -> Self {
        Self {
            fs: fs.clone(),
            faults: Arc::default(),
            reads: Arc::default(),
        }
    }
    fn wrap(&self, file: SimFsFile) -> CountedFile {
        CountedFile {
            file,
            reads: Arc::clone(&self.reads),
        }
    }
    fn reads(&self) -> u64 {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn faults(&self) -> std::sync::MutexGuard<'_, VfsFaults> {
        self.faults.lock().unwrap()
    }
    fn open(&self, mode: PackMode) -> persia_blob::Result<PackStore<CountedFile>> {
        PackStore::open_in(Arc::new(self.clone()), PACK, mode, [9; 16])
    }
}

/// Sets one fault, given the path of the file whose removal fails.
type Inject = fn(&mut VfsFaults, &str);

fn injected(what: &str) -> io::Error {
    io::Error::other(format!("injected {what} failure"))
}

impl Vfs for Wrapped {
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
        if self.faults().remove.as_deref() == Some(path) {
            return Err(injected("remove"));
        }
        if self.faults().remove_not_found && path.to_string_lossy().contains(".persia.blobs.tmp.") {
            // Already gone, as when another handle's sweep won the race after the link.
            match self.fs.remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the temporary name was already gone",
            ));
        }
        self.fs.remove_file(path)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        if self.faults().sync_dir {
            return Err(injected("directory sync"));
        }
        self.fs.sync_dir(dir)
    }
    fn entry_kind(&self, path: &Path) -> io::Result<persia_storage::vfs::EntryKind> {
        self.fs.entry_kind(path)
    }
    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        self.fs.is_dir(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<persia_storage::vfs::DirEntry>> {
        if self.faults().read_dir {
            return Err(injected("listing"));
        }
        self.fs.read_dir(dir)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<std::path::PathBuf> {
        self.fs.canonicalize(path)
    }
    fn same_file(&self, file: &CountedFile, path: &Path) -> io::Result<bool> {
        self.fs.same_file(&file.file, path)
    }
    fn lock_beside(&self, path: &Path) -> io::Result<Option<CountedFile>> {
        self.fs.lock_beside(path).map(|f| f.map(|f| self.wrap(f)))
    }
}

/// A creation whose temporary name is already gone after the link (`NotFound`, as when another handle's sweep
/// removed it) still publishes the pack: the name being gone is what the removal wanted (ROADMAP 9.2.6, 9.2.7).
#[test]
fn a_temporary_name_already_gone_after_the_link_still_creates_the_pack() {
    let fs = sim();
    let vfs = Wrapped::new(&fs);
    vfs.faults().remove_not_found = true;
    let store = vfs.open(PackMode::ReadWrite).unwrap();
    let m = store_blob(&store, b"kept", 4, 1);
    assert_eq!(read_all(&store, &m).unwrap(), b"kept");
    assert!(temporaries(&fs).is_empty(), "the temporary name is gone");
    drop(store);
    let store = open(&fs.crash(CrashLoss::All), PackMode::ReadOnly).unwrap();
    assert_eq!(read_all(&store, &m).unwrap(), b"kept");
}

/// ROADMAP 9.2.6, the sweep's failure policy: a failed removal, directory sync or listing is logged and the read-write
/// open goes on; `swept` counts only what was removed, and the next read-write open removes what is left.
#[test]
fn a_failed_sweep_step_does_not_fail_the_open_and_the_next_open_retries() {
    let a = format!("{PACK}.tmp.0123456789abcdef.1");
    let b = format!("{PACK}.tmp.fedcba9876543210.2");
    let cases: [(&str, Inject, u64, usize); 3] = [
        ("removal", |f, a| f.remove = Some(a.into()), 1, 1),
        ("directory sync", |f, _| f.sync_dir = true, 2, 0),
        ("listing", |f, _| f.read_dir = true, 0, 2),
    ];
    for (what, inject, swept, left) in cases {
        for with_pack in [false, true] {
            let fs = sim();
            if with_pack {
                store_blob(&open(&fs, PackMode::ReadWrite).unwrap(), b"kept", 4, 1);
            }
            write_file(&fs, &a);
            write_file(&fs, &b);
            fs.sync_dir(Path::new("/db")).unwrap();
            let vfs = Wrapped::new(&fs);
            inject(&mut vfs.faults(), &a);
            let store = vfs
                .open(PackMode::ReadWrite)
                .unwrap_or_else(|e| panic!("{what}: the open fails: {e}"));
            assert_eq!(store.recovery().swept, swept, "{what}");
            assert_eq!(temporaries(&fs).len(), left, "{what}");
            assert_eq!(store.exists(), with_pack, "{what}");
            drop(store);
            *vfs.faults() = VfsFaults::default();
            let store = vfs.open(PackMode::ReadWrite).unwrap();
            assert_eq!(store.recovery().swept, left as u64, "{what}: retried");
            assert!(temporaries(&fs).is_empty(), "{what}");
        }
    }
}

/// ROADMAP 9.2.9: one read per chunk. Reading a whole blob, or a range across two chunks, by its manifest reads each
/// chunk's frame once, with no separate read of its header.
#[test]
fn a_range_read_makes_one_read_per_chunk() {
    let fs = sim();
    let data: Vec<u8> = (0..100_u32).map(|i| i as u8).collect();
    let m = store_blob(&open(&fs, PackMode::ReadWrite).unwrap(), &data, 16, 1);
    let vfs = Wrapped::new(&fs);
    let store = vfs.open(PackMode::ReadOnly).unwrap();
    for (range, chunks) in [
        (0..100, 7),
        (10..40, 3),
        (15..17, 2),
        (16..32, 1),
        (99..100, 1),
    ] {
        let want = &data[range.start as usize..range.end as usize];
        let before = vfs.reads();
        let got = persia_blob::read_manifest_range(&store, &m, range.clone()).unwrap();
        assert_eq!(got, want);
        assert_eq!(vfs.reads() - before, chunks, "{range:?}");
        // `read_range` reads the manifest's index frame first: one more.
        let before = vfs.reads();
        assert_eq!(
            read_range(&store, &reference(&m), range.clone()).unwrap(),
            want
        );
        assert_eq!(vfs.reads() - before, chunks + 1, "{range:?}");
    }
    let before = vfs.reads();
    store.read_chunk_source(&m, 3).unwrap();
    assert_eq!(vfs.reads() - before, 1);
}

/// ROADMAP 9.2.6: a symbolic link named like a temporary file is not one of the pack's files: the sweep leaves the
/// link, and the file it points to outside the directory is untouched.
#[cfg(unix)]
#[test]
fn a_symlink_named_like_a_temporary_file_is_not_swept() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("precious");
    std::fs::write(&target, b"not the pack's").unwrap();
    let pack = persia_blob::pack_path(&dir.path().join("app.persia"));
    let mut link = pack.as_os_str().to_owned();
    link.push(".tmp.0123456789abcdef.1");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let store = PackStore::open(&pack, PackMode::ReadWrite, [1; 16]).unwrap();
    assert_eq!(store.recovery().swept, 0);
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"not the pack's");
}
