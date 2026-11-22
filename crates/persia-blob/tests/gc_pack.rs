//! The pack rewrite (SPEC §9.3, §9.5, §18.10; ROADMAP 9.5.2, 9.5.4; ADR-0052), on a simulated file system.
//!
//! Invariants:
//! 1. A rewrite keeps exactly the blobs that are marked or pinned (or whose upload is), each wholly readable, and the
//!    chunks this handle staged for marked or pinned uploads, which still bind; everything else is gone, from the
//!    handle and from the file a reopen finds. The new file has generation + 1 and a new `pack_id`.
//! 2. It runs only when the dropped bytes reach the ratio (0 forces it); otherwise nothing changes.
//! 3. A pin taken after the plan that names something dropped defers it: the pack is unchanged and the temporary
//!    file removed.
//! 4. A reader in this process reads on across the rewrite; a reader in another process keeps the old file, and its
//!    `refresh` reports `PackReplaced`.
//! 5. A crash at any call of the rewrite reopens to the old pack or the new one, never a mix, with every kept blob
//!    readable. A failed directory sync poisons the handle, which still reads.
//! 6. So does one of a pack of generation 2 that keeps blobs only a pin on their id or their upload keeps: the
//!    generation and the `pack_id` tell the old pack from the new, and the next sweep converges on the new one.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::struct_excessive_bools,
    clippy::assert_is_empty,
    clippy::type_complexity,
    missing_docs
)] // Tests: inputs are generated or fixed.

mod gc_support;
mod pack_support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gc_support::{Pins, data_of, put, read_all, stage};
use persia_blob::gc::{GcObject, Mark, Rewrite, Sweep, SweepOptions, SweepReport};
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobStore, Error, PackMode, PackStore, read_chunk_checked,
};
use persia_storage::container::ContainerFile;
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

const PACK: &str = "/db/app.persia.blobs";
/// The pack's writer lock file beside it (ROADMAP 9.2.15), created by the first read-write open.
const LOCK: &str = "/db/app.persia.blobs.lock";

fn sim() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    fs
}

fn open(fs: &SimFs, mode: PackMode) -> persia_blob::Result<PackStore<SimFsFile>> {
    pack_support::open_pack(Arc::new(fs.clone()), PACK, mode, [9; 16])
}

fn options(ratio: f64) -> SweepOptions {
    SweepOptions {
        grace: std::time::Duration::from_hours(24),
        now: SystemTime::now().max(UNIX_EPOCH),
        rewrite_ratio: ratio,
    }
}

fn sweep(
    store: &PackStore<SimFsFile>,
    mark: &Mark,
    pins: &Pins,
    ratio: f64,
) -> persia_blob::Result<SweepReport> {
    store.sweep(mark, pins, &options(ratio))
}

/// Only the pack and its lock file are left in its directory (no temporary file).
fn only_the_pack(fs: &SimFs) {
    let names: Vec<PathBuf> = fs.files().into_iter().map(|(p, _)| p).collect();
    assert_eq!(names, [PathBuf::from(PACK), PathBuf::from(LOCK)]);
}

/// One blob of a generated pack.
#[derive(Debug, Clone)]
struct Spec {
    len: usize,
    chunk: u32,
    /// Bound; else staged by this handle (in flight), or staged and discarded (a duplicate's chunks: orphans).
    bound: bool,
    discarded: bool,
    marked: bool,
    pin_blob: bool,
    pin_upload: bool,
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        0..40_usize,
        3..9_u32,
        prop::bool::weighted(0.6),
        any::<bool>(),
        prop::bool::weighted(0.4),
        prop::bool::weighted(0.15),
        prop::bool::weighted(0.2),
    )
        .prop_map(
            |(len, chunk, bound, discarded, marked, pin_blob, pin_upload)| Spec {
                len,
                chunk,
                bound,
                discarded,
                marked,
                pin_blob,
                pin_upload,
            },
        )
}

struct Built {
    blobs: Vec<(Spec, BlobManifest, Vec<u8>)>,
    mark: Mark,
    pins: Pins,
}

fn build(store: &PackStore<SimFsFile>, specs: &[Spec]) -> Built {
    let mut blobs = Vec::new();
    let pins = Pins::default();
    let mut marked = BTreeSet::new();
    for (i, s) in specs.iter().enumerate() {
        let mut data = data_of(s.len, i as u8);
        data.push(i as u8);
        let upload = 100 + i as u128;
        let m = if s.bound {
            put(store, &data, s.chunk, upload)
        } else {
            let m = stage(store, &data, s.chunk, upload);
            if s.discarded {
                store.discard_upload(m.upload_id()).unwrap();
            }
            m
        };
        if s.marked {
            marked.insert(m.blob_id());
        }
        if s.pin_blob {
            pins.pin(GcObject::Blob(m.blob_id()));
        }
        if s.pin_upload {
            pins.pin(GcObject::Upload(m.upload_id()));
        }
        blobs.push((s.clone(), m, data));
    }
    let mut mark = Mark::new(marked, persia_blob::PinGate::pinned(&pins));
    mark.add_manifest_uploads(store).unwrap();
    Built { blobs, mark, pins }
}

fn kept_blob(b: &Built, s: &Spec, m: &BlobManifest) -> bool {
    s.bound && (b.mark.blobs.contains(&m.blob_id()) || b.mark.uploads.contains(&m.upload_id()))
}

fn kept_staged(b: &Built, s: &Spec, m: &BlobManifest) -> bool {
    !s.bound && !s.discarded && b.mark.uploads.contains(&m.upload_id())
}

/// The handle `store` and a reopen of `fs` hold exactly the kept blobs, readable; kept staged chunks bind.
fn check_rewritten(fs: &SimFs, store: &PackStore<SimFsFile>, b: &Built) {
    for (s, m, data) in &b.blobs {
        if s.bound {
            let found = store.manifest(m.blob_id()).unwrap();
            assert_eq!(found.is_some(), kept_blob(b, s, m), "{s:?}");
            if found.is_some() {
                assert_eq!(&read_all(store, m).unwrap(), data, "{s:?}");
            }
        }
    }
    let reopened = open(&fs.crash(CrashLoss::All), PackMode::ReadOnly).unwrap();
    let mut ids: Vec<BlobId> = reopened.blob_ids();
    ids.sort_unstable();
    let mut expected: Vec<BlobId> = b
        .blobs
        .iter()
        .filter(|(s, m, _)| kept_blob(b, s, m))
        .map(|(_, m, _)| m.blob_id())
        .collect();
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(ids, expected);
    for (s, m, data) in &b.blobs {
        if kept_blob(b, s, m) {
            assert_eq!(&read_all(&reopened, m).unwrap(), data, "reopened, {s:?}");
        }
    }
    // Staged chunks of kept uploads still bind on the handle; the others are gone.
    for (s, m, data) in &b.blobs {
        if s.bound || s.discarded {
            continue;
        }
        if kept_staged(b, s, m) {
            assert_eq!(store.bind(m).unwrap(), BindOutcome::Bound, "{s:?}");
            assert_eq!(&read_all(store, m).unwrap(), data);
        } else {
            assert!(
                matches!(store.bind(m), Err(Error::ChunkNotStaged { .. })),
                "{s:?}"
            );
        }
    }
}

fn run(specs: &[Spec], ratio: f64) {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let b = build(&store, specs);
    let before = store.superblock();
    let reader = open(&fs, PackMode::ReadOnly).unwrap();
    let report = sweep(&store, &b.mark, &b.pins, ratio).unwrap();
    match report.rewrite {
        Some(Rewrite::NoPack) => assert!(before.is_none()),
        Some(Rewrite::BelowRatio {
            dead_bytes,
            frame_bytes,
        }) => {
            assert!(ratio > 0.0);
            assert!((dead_bytes as f64) < ratio * frame_bytes as f64 || dead_bytes == 0);
            assert_eq!(store.superblock(), before, "nothing changed");
        }
        Some(Rewrite::Done {
            generation,
            frame_bytes_before,
            frame_bytes_after,
        }) => {
            let (before, after) = (before.unwrap(), store.superblock().unwrap());
            assert_eq!(generation, before.generation + 1);
            assert_eq!(after.generation, generation);
            assert_ne!(after.pack_id, before.pack_id);
            assert!(frame_bytes_after <= frame_bytes_before);
            let dead = frame_bytes_before - frame_bytes_after;
            assert!(ratio == 0.0 || dead as f64 >= ratio * frame_bytes_before as f64);
            let dropped = b
                .blobs
                .iter()
                .filter(|(s, m, _)| s.bound && !kept_blob(&b, s, m))
                .count();
            assert_eq!(report.blobs_deleted, dropped as u64);
            only_the_pack(&fs);
            // A reader in another process keeps the old file, every old blob included, until it reopens.
            for (s, m, data) in &b.blobs {
                if s.bound {
                    assert_eq!(&read_all(&reader, m).unwrap(), data);
                }
            }
            assert!(matches!(reader.refresh(), Err(Error::PackReplaced { .. })));
            check_rewritten(&fs, &store, &b);
        }
        other => panic!("{other:?}"),
    }
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 512 } else { 96 },
        rng_seed: RngSeed::Fixed(0x9_5_2_2),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn a_forced_rewrite_keeps_exactly_the_marked_and_pinned(specs in prop::collection::vec(spec(), 1..10)) {
        run(&specs, 0.0);
    }

    #[test]
    fn a_rewrite_runs_only_past_the_ratio(
        specs in prop::collection::vec(spec(), 1..10),
        ratio in prop::sample::select(vec![0.25, 0.5, 0.75, 1.0, 2.0]),
    ) {
        run(&specs, ratio);
    }
}

#[test]
fn a_database_without_a_pack_has_nothing_to_collect() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let report = sweep(&store, &Mark::default(), &Pins::default(), 0.0).unwrap();
    assert_eq!(report.rewrite, Some(Rewrite::NoPack));
    assert!(!store.exists());
    let ro = open(&fs, PackMode::ReadOnly).unwrap();
    assert!(matches!(
        sweep(&ro, &Mark::default(), &Pins::default(), 0.0),
        Err(Error::PackReadOnly)
    ));
}

#[test]
fn a_pin_taken_after_the_plan_defers_the_rewrite() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let kept = put(&store, b"kept blob", 4, 1);
    let dropped = put(&store, b"dropped blob", 4, 2);
    let mark = Mark::new([kept.blob_id()], []);
    let pins = Pins::default();
    // A `get_blob` of the dropped blob starts while the new file is written: it pins it before the swap.
    let id = dropped.blob_id();
    pins.on_check(move |pinned| {
        pinned.insert(GcObject::Blob(id));
    });
    let before = store.superblock();
    let report = sweep(&store, &mark, &pins, 0.0).unwrap();
    assert_eq!(report.rewrite, Some(Rewrite::Deferred));
    assert_eq!(report.blobs_deleted, 0);
    assert_eq!(store.superblock(), before);
    assert_eq!(read_all(&store, &dropped).unwrap(), b"dropped blob");
    only_the_pack(&fs);
    // A pin on the dropped blob's upload defers it too.
    let pins = Pins::default();
    let upload = dropped.upload_id();
    pins.on_check(move |pinned| {
        pinned.insert(GcObject::Upload(upload));
    });
    assert_eq!(
        sweep(&store, &mark, &pins, 0.0).unwrap().rewrite,
        Some(Rewrite::Deferred)
    );
    // Once released, the next sweep drops it.
    let report = sweep(&store, &mark, &Pins::default(), 0.0).unwrap();
    assert!(matches!(report.rewrite, Some(Rewrite::Done { .. })));
    assert_eq!(report.blobs_deleted, 1);
    assert!(store.manifest(dropped.blob_id()).unwrap().is_none());
    assert_eq!(read_all(&store, &kept).unwrap(), b"kept blob");
}

#[test]
fn pins_taken_after_the_mark_and_marked_uploads_keep_what_they_name() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let pinned_late = put(&store, b"pinned after the mark", 4, 1);
    let marked_upload = stage(&store, b"staged for a marked upload", 4, 2);
    let upload_pinned_late = stage(&store, b"staged, pinned late", 4, 3);
    put(&store, b"dropped", 4, 4);
    // The mark names an upload without a pin (as a marked manifest's would), and nothing pinned yet.
    let mut mark = Mark::default();
    mark.uploads.insert(marked_upload.upload_id());
    let pins = Pins::default();
    pins.pin(GcObject::Blob(pinned_late.blob_id()));
    pins.pin(GcObject::Upload(upload_pinned_late.upload_id()));
    let report = sweep(&store, &mark, &pins, 0.0).unwrap();
    assert!(
        matches!(report.rewrite, Some(Rewrite::Done { .. })),
        "{report:?}"
    );
    assert_eq!(report.blobs_deleted, 1);
    assert_eq!(
        read_all(&store, &pinned_late).unwrap(),
        b"pinned after the mark"
    );
    for m in [&marked_upload, &upload_pinned_late] {
        assert_eq!(store.bind(m).unwrap(), BindOutcome::Bound);
    }
}

#[test]
fn a_reader_in_this_process_reads_on_across_the_rewrite() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    put(&store, b"garbage to drop first", 4, 1);
    let kept = put(&store, b"a blob read chunk by chunk", 4, 2);
    let mark = Mark::new([kept.blob_id()], []);
    // The stream looked the manifest up and read chunk 0 before the rewrite.
    let manifest = store.manifest(kept.blob_id()).unwrap().unwrap();
    assert_eq!(read_chunk_checked(&store, &manifest, 0).unwrap(), b"a bl");
    let report = sweep(&store, &mark, &Pins::default(), 0.0).unwrap();
    assert!(matches!(report.rewrite, Some(Rewrite::Done { .. })));
    let rest: Vec<u8> = (1..manifest.chunk_count())
        .flat_map(|i| read_chunk_checked(&store, &manifest, i).unwrap())
        .collect();
    assert_eq!(rest, b"ob read chunk by chunk");
}

#[test]
fn pinned_staged_chunks_without_a_live_blob_go_after_log_end() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    put(&store, b"an orphan", 4, 1);
    let in_flight = stage(&store, b"staged, not bound yet", 5, 2);
    let pins = Pins::new([GcObject::Upload(in_flight.upload_id())]);
    let mark = Mark::new([], persia_blob::PinGate::pinned(&pins));
    let report = sweep(&store, &mark, &pins, 0.0).unwrap();
    assert!(matches!(report.rewrite, Some(Rewrite::Done { .. })));
    let sb = store.superblock().unwrap();
    assert_eq!((sb.seq, sb.blob_count, sb.log_end), (0, 0, 12288));
    // A crash now drops them, as from any pack: local uploads are not resumable.
    assert!(
        open(&fs.crash(CrashLoss::Nothing), PackMode::ReadWrite)
            .unwrap()
            .blob_ids()
            .is_empty()
    );
    assert_eq!(store.bind(&in_flight).unwrap(), BindOutcome::Bound);
    let reopened = open(&fs.crash(CrashLoss::All), PackMode::ReadWrite).unwrap();
    assert_eq!(
        read_all(&reopened, &in_flight).unwrap(),
        b"staged, not bound yet"
    );
}

#[test]
fn orphan_frames_alone_are_dropped_without_a_pin_check() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let kept = put(&store, b"kept", 2, 1);
    // A duplicate's chunks, discarded: frames nothing names.
    let dup = stage(&store, b"a duplicate's chunks", 4, 2);
    store.discard_upload(dup.upload_id()).unwrap();
    let pins = Pins::default();
    let mark = Mark::new([kept.blob_id()], []);
    let report = sweep(&store, &mark, &pins, 0.5).unwrap();
    assert!(
        matches!(report.rewrite, Some(Rewrite::Done { .. })),
        "{report:?}"
    );
    assert_eq!(report.chunks_deleted, 5);
    assert!(pins.checked.lock().unwrap().is_empty());
    assert_eq!(read_all(&store, &kept).unwrap(), b"kept");
}

/// Damage to a kept chunk stops the rewrite before anything is replaced: a flipped byte (the frame's CRC fails), and
/// other bytes under a recomputed frame CRC (only the chunk's own CRC32C tells), which a rewrite must not copy forward.
#[test]
fn a_damaged_kept_chunk_stops_the_rewrite() {
    use persia_format::block::Compression;
    use persia_format::frame::{FRAME_HEADER_LEN, FrameKind, encode_frame};
    use persia_storage::container::ContainerFile;

    for resealed in [false, true] {
        let fs = sim();
        let store = open(&fs, PackMode::ReadWrite).unwrap();
        let kept = put(&store, b"kept but damaged", 4, 1);
        put(&store, b"dropped", 4, 2);
        // Chunk 1 of upload 1 (" but"): its payload starts with the upload id and the index.
        let bytes = fs.read(Path::new(PACK)).unwrap();
        let mut head = 1_u128.to_le_bytes().to_vec();
        head.extend_from_slice(&1_u32.to_le_bytes());
        head.extend_from_slice(&4_u32.to_le_bytes());
        let payload_at = bytes.windows(head.len()).position(|w| w == head).unwrap();
        let mut payload = bytes[payload_at..payload_at + 36].to_vec();
        payload[32] ^= 0xff;
        let frame_at = payload_at - FRAME_HEADER_LEN;
        let f = fs.open(Path::new(PACK), true).unwrap();
        if resealed {
            let seq = u64::from_le_bytes(bytes[frame_at + 16..frame_at + 24].try_into().unwrap());
            let mut w = persia_format::Writer::new();
            encode_frame(
                &mut w,
                FrameKind::BlobChunk,
                seq,
                &payload,
                Compression::None,
            )
            .unwrap();
            f.write_all_at(w.as_slice(), frame_at as u64).unwrap();
        } else {
            f.write_all_at(&payload[32..33], (payload_at + 32) as u64)
                .unwrap();
        }
        let before = store.superblock();
        let mark = Mark::new([kept.blob_id()], []);
        let err = sweep(&store, &mark, &Pins::default(), 0.0).unwrap_err();
        assert!(err.is_corruption(), "resealed {resealed}: {err}");
        assert_eq!(store.superblock(), before);
        assert!(!store.is_poisoned());
        only_the_pack(&fs);
    }
}

// --- Crash tests ---

const BLOBS: [(&[u8], u32, u128, bool); 4] = [
    (b"the first kept blob", 4, 1, true),
    (b"a dropped one", 3, 2, false),
    (b"a second kept blob", 5, 3, true),
    (b"", 4, 4, false),
];

/// A pack with the blobs of [`BLOBS`], durable.
fn crash_fixture() -> (
    SimFs,
    PackStore<SimFsFile>,
    Mark,
    BTreeMap<BlobId, (Vec<u8>, BlobManifest, bool)>,
) {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let mut blobs = BTreeMap::new();
    let mut marked = Vec::new();
    for (data, chunk, upload, kept) in BLOBS {
        let m = put(&store, data, chunk, upload);
        if kept {
            marked.push(m.blob_id());
        }
        blobs.insert(m.blob_id(), (data.to_vec(), m, kept));
    }
    (fs, store, Mark::new(marked, []), blobs)
}

/// After a crash: the pack reopens to the old one (every blob) or the new one (the kept ones), never a mix.
fn check_crashed(
    fs: &SimFs,
    old_generation: u64,
    blobs: &BTreeMap<BlobId, (Vec<u8>, BlobManifest, bool)>,
    what: &str,
) -> bool {
    let store = open(fs, PackMode::ReadWrite).unwrap_or_else(|e| panic!("{what}: reopen: {e}"));
    let sb = store.superblock().unwrap();
    let mut ids = store.blob_ids();
    ids.sort_unstable();
    let replaced = sb.generation != old_generation;
    let expected: Vec<BlobId> = blobs
        .iter()
        .filter(|(_, (_, _, kept))| *kept || !replaced)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(ids, expected, "{what}: generation {}", sb.generation);
    for (data, m, kept) in blobs.values() {
        if *kept || !replaced {
            assert_eq!(&read_all(&store, m).unwrap(), data, "{what}");
        }
    }
    replaced
}

fn crash_during_rewrite(seeds: u64) {
    let total = {
        let (fs, store, mark, _) = crash_fixture();
        let start = fs.mutations();
        sweep(&store, &mark, &Pins::default(), 0.0).unwrap();
        fs.mutations() - start
    };
    assert!(total >= 8, "the rewrite makes {total} mutating calls");
    let mut outcomes = BTreeSet::new();
    for k in 0..total {
        for fault in [SimFault::FailAt, SimFault::ApplyThenFail] {
            let (fs, store, mark, blobs) = crash_fixture();
            let old = store.superblock().unwrap().generation;
            fs.set_fault(Some(fault(fs.mutations() + k)));
            let result = sweep(&store, &mark, &Pins::default(), 0.0);
            assert!(result.is_err(), "the fault at call {k} fires");
            // The last call is the directory sync, after the rename: the handle stops writing (SPEC §18.10 (b)). So
            // it does after a rename that reported an error but happened: the path no longer names its file.
            let applied_rename = k == total - 2 && fault(0) == SimFault::ApplyThenFail(0);
            assert_eq!(
                store.is_poisoned(),
                k == total - 1 || applied_rename,
                "call {k}"
            );
            // Whatever failed, the handle still reads every kept blob.
            for (data, m, kept) in blobs.values() {
                if *kept {
                    assert_eq!(&read_all(&store, m).unwrap(), data, "call {k}");
                }
            }
            drop(store);
            let losses = [CrashLoss::All, CrashLoss::Nothing]
                .into_iter()
                .chain((0..seeds).map(CrashLoss::Seeded));
            for loss in losses {
                let what = format!("{:?} at call {k}, {loss:?}", fault(k));
                outcomes.insert(check_crashed(&fs.crash(loss), old, &blobs, &what));
            }
        }
    }
    assert_eq!(
        outcomes.len(),
        2,
        "crashes land on both sides of the rename"
    );
    // No crash: the new pack, also after a crash right after the directory sync.
    let (fs, store, mark, blobs) = crash_fixture();
    let old = store.superblock().unwrap().generation;
    sweep(&store, &mark, &Pins::default(), 0.0).unwrap();
    assert!(check_crashed(
        &fs.crash(CrashLoss::All),
        old,
        &blobs,
        "after the sync"
    ));
}

#[test]
fn a_crash_at_any_call_of_the_rewrite_reopens_the_old_or_the_new_pack() {
    crash_during_rewrite(4);
}

#[cfg(feature = "slow")]
#[test]
fn a_crash_at_any_call_of_the_rewrite_reopens_the_old_or_the_new_pack_many_seeds() {
    crash_during_rewrite(64);
}

#[test]
fn a_failed_directory_sync_stops_the_handle_and_the_new_pack_binds_nothing() {
    let (fs, store, mark, blobs) = crash_fixture();
    let total = {
        let (fs, store, mark, _) = crash_fixture();
        let start = fs.mutations();
        sweep(&store, &mark, &Pins::default(), 0.0).unwrap();
        fs.mutations() - start
    };
    // The directory sync fails (and, the process being gone, every later write would).
    fs.set_fault(Some(SimFault::FailAt(fs.mutations() + total - 1)));
    let err = sweep(&store, &mark, &Pins::default(), 0.0).unwrap_err();
    assert!(
        matches!(
            err,
            Error::PackIo {
                op: "sync directory",
                ..
            }
        ),
        "{err}"
    );
    assert!(store.is_poisoned());
    let m = gc_support::manifest_of(b"after", 4, 99);
    assert!(matches!(
        store.stage_chunk(m.upload_id(), 0, b"afte"),
        Err(Error::PackPoisoned)
    ));
    // It moved to the new file: the dropped blob is gone from it, the kept ones read.
    for (data, m, kept) in blobs.values() {
        assert_eq!(store.manifest(m.blob_id()).unwrap().is_some(), *kept);
        if *kept {
            assert_eq!(&read_all(&store, m).unwrap(), data);
        }
    }
    drop(store);
    // A crash may revert the rename: either pack holds every kept blob.
    let old = 1;
    for seed in 0..8 {
        check_crashed(
            &fs.crash(CrashLoss::Seeded(seed)),
            old,
            &blobs,
            "failed sync",
        );
    }
}

// --- Crash tests with pins, on a pack already rewritten once (ROADMAP 9.5.4) ---

/// The blobs of [`pinned_crash_fixture`]: bytes, chunk size, upload id, and what keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    Marked,
    /// Unmarked, its blob id pinned after the mark (a `get_blob` or a `put_blob` still in flight).
    PinnedBlob,
    /// Unmarked, its upload id pinned after the mark.
    PinnedUpload,
    Dropped,
}

const PINNED_BLOBS: [(&[u8], u32, u128, Keep); 6] = [
    (b"a marked blob, kept", 4, 11, Keep::Marked),
    (b"garbage", 3, 12, Keep::Dropped),
    (
        b"only a pin on its blob id keeps this one",
        5,
        13,
        Keep::PinnedBlob,
    ),
    (
        b"only a pin on its upload keeps this one",
        6,
        14,
        Keep::PinnedUpload,
    ),
    (b"", 4, 15, Keep::Dropped),
    (
        b"bound after the first rewrite, dropped",
        4,
        16,
        Keep::Dropped,
    ),
];

struct PinnedFixture {
    fs: SimFs,
    store: PackStore<SimFsFile>,
    mark: Mark,
    pins: Pins,
    blobs: BTreeMap<BlobId, (Vec<u8>, BlobManifest, Keep)>,
}

/// A pack of generation 2 (rewritten once, dropping one blob), then more garbage bound after it, a pinned staged
/// upload and an unpinned one; durable. The mark names the marked blob only; the pins (taken after it) the rest.
fn pinned_crash_fixture() -> PinnedFixture {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let mut blobs = BTreeMap::new();
    for &(data, chunk, upload, keep) in &PINNED_BLOBS[..5] {
        let m = put(&store, data, chunk, upload);
        blobs.insert(m.blob_id(), (data.to_vec(), m, keep));
    }
    let first_garbage = put(&store, b"dropped by the first rewrite", 4, 10);
    let all_but_first: Vec<BlobId> = blobs.keys().copied().collect();
    let first = sweep(&store, &Mark::new(all_but_first, []), &Pins::default(), 0.0).unwrap();
    assert!(
        matches!(first.rewrite, Some(Rewrite::Done { generation: 2, .. })),
        "{first:?}"
    );
    assert!(store.manifest(first_garbage.blob_id()).unwrap().is_none());
    let (data, chunk, upload, keep) = PINNED_BLOBS[5];
    let m = put(&store, data, chunk, upload);
    blobs.insert(m.blob_id(), (data.to_vec(), m, keep));
    // A `put_blob` in flight (its upload pinned) and an abandoned one: chunks after `log_end`.
    let in_flight = stage(&store, b"staged, its upload pinned", 4, 17);
    stage(&store, b"staged, abandoned", 4, 18);
    let mut mark = Mark::new(
        blobs
            .iter()
            .filter(|(_, (_, _, k))| *k == Keep::Marked)
            .map(|(id, _)| *id),
        [],
    );
    mark.add_manifest_uploads(&store).unwrap();
    let pins = Pins::new([GcObject::Upload(in_flight.upload_id())]);
    for (data, m, keep) in blobs.values() {
        match keep {
            Keep::PinnedBlob => pins.pin(GcObject::Blob(m.blob_id())),
            Keep::PinnedUpload => pins.pin(GcObject::Upload(m.upload_id())),
            Keep::Marked | Keep::Dropped => {}
        }
        assert_eq!(&read_all(&store, m).unwrap(), data);
    }
    PinnedFixture {
        fs,
        store,
        mark,
        pins,
        blobs,
    }
}

/// After a crash: the old pack (every blob, the old generation and `pack_id`) or the new one (exactly the marked and
/// pinned blobs, generation + 1, another `pack_id`), each blob byte for byte. Returns the `pack_id` when it is new.
fn check_pinned_crash(
    fs: &SimFs,
    old: persia_blob::pack::layout::PackSuperblock,
    blobs: &BTreeMap<BlobId, (Vec<u8>, BlobManifest, Keep)>,
    what: &str,
) -> Option<u128> {
    let store = open(fs, PackMode::ReadWrite).unwrap_or_else(|e| panic!("{what}: reopen: {e}"));
    let sb = store.superblock().unwrap();
    let replaced = match sb.generation {
        g if g == old.generation => false,
        g if g == old.generation + 1 => true,
        g => panic!("{what}: generation {g} after {}", old.generation),
    };
    assert_eq!(
        sb.pack_id == old.pack_id,
        !replaced,
        "{what}: the pack_id tells the old pack from the new one"
    );
    let mut ids = store.blob_ids();
    ids.sort_unstable();
    let expected: Vec<BlobId> = blobs
        .iter()
        .filter(|(_, (_, _, keep))| !replaced || *keep != Keep::Dropped)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(ids, expected, "{what}: replaced {replaced}");
    assert_eq!(sb.blob_count, expected.len() as u64, "{what}");
    for (data, m, keep) in blobs.values() {
        if !replaced || *keep != Keep::Dropped {
            assert_eq!(&read_all(&store, m).unwrap(), data, "{what}: {keep:?}");
        }
    }
    replaced.then_some(sb.pack_id)
}

/// After [`check_pinned_crash`]: a sweep of the reopened pack with the same mark and pins converges on the new pack.
fn sweep_after_crash(
    fs: &SimFs,
    mark: &Mark,
    pins: &Pins,
    blobs: &BTreeMap<BlobId, (Vec<u8>, BlobManifest, Keep)>,
    what: &str,
) {
    let store = open(fs, PackMode::ReadWrite).unwrap();
    let report = sweep(&store, mark, pins, 0.0)
        .unwrap_or_else(|e| panic!("{what}: the sweep after the crash: {e}"));
    assert!(
        matches!(report.rewrite, Some(Rewrite::Done { .. })),
        "{what}: {report:?}"
    );
    let reopened = open(&fs.crash(CrashLoss::All), PackMode::ReadOnly).unwrap();
    for store in [&store, &reopened] {
        for (data, m, keep) in blobs.values() {
            let found = store.manifest(m.blob_id()).unwrap().is_some();
            assert_eq!(found, *keep != Keep::Dropped, "{what}: {keep:?}");
            if found {
                assert_eq!(&read_all(store, m).unwrap(), data, "{what}: {keep:?}");
            }
        }
    }
}

fn crash_during_pinned_rewrite(seeds: u64) {
    let total = {
        let f = pinned_crash_fixture();
        let start = f.fs.mutations();
        let report = sweep(&f.store, &f.mark, &f.pins, 0.0).unwrap();
        // The three dropped blobs, and the abandoned upload.
        assert_eq!(
            (report.blobs_deleted, report.uploads_deleted),
            (3, 1),
            "{report:?}"
        );
        f.fs.mutations() - start
    };
    let mut sides = BTreeSet::new();
    for k in 0..total {
        for fault in [SimFault::FailAt, SimFault::ApplyThenFail] {
            let f = pinned_crash_fixture();
            let old = f.store.superblock().unwrap();
            f.fs.set_fault(Some(fault(f.fs.mutations() + k)));
            assert!(
                sweep(&f.store, &f.mark, &f.pins, 0.0).is_err(),
                "the fault at call {k} fires"
            );
            // The handle reads every marked and pinned blob, whatever failed.
            for (data, m, keep) in f.blobs.values() {
                if *keep != Keep::Dropped {
                    assert_eq!(&read_all(&f.store, m).unwrap(), data, "call {k}: {keep:?}");
                }
            }
            drop(f.store);
            let mut new_ids = BTreeSet::new();
            let losses = [CrashLoss::All, CrashLoss::Nothing]
                .into_iter()
                .chain((0..seeds).map(CrashLoss::Seeded));
            for loss in losses {
                let what = format!("{:?} at call {k} of {total}, {loss:?}", fault(k));
                let crashed = f.fs.crash(loss);
                let new = check_pinned_crash(&crashed, old, &f.blobs, &what);
                sides.insert(new.is_some());
                new_ids.extend(new);
                sweep_after_crash(&crashed, &f.mark, &f.pins, &f.blobs, &what);
            }
            // Every crash that lands on the new pack finds the one file this rewrite wrote.
            assert!(new_ids.len() <= 1, "call {k}: {new_ids:?}");
        }
    }
    assert_eq!(sides.len(), 2, "crashes land on both sides of the rename");
    // No fault: the new pack, after any crash that follows.
    let f = pinned_crash_fixture();
    let old = f.store.superblock().unwrap();
    sweep(&f.store, &f.mark, &f.pins, 0.0).unwrap();
    let new = f.store.superblock().unwrap().pack_id;
    for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(1)] {
        let crashed = f.fs.crash(loss);
        let found = check_pinned_crash(
            &crashed,
            old,
            &f.blobs,
            &format!("after the sync, {loss:?}"),
        );
        assert_eq!(found, Some(new), "{loss:?}");
    }
}

/// ROADMAP 9.5.4: a crash at any write or sync of a rewrite of a pack of generation 2, keeping blobs that only a pin
/// on their id or their upload keeps, reopens to the complete old pack or the complete new one, told apart by the
/// generation and the `pack_id`; every marked or pinned blob reads back byte for byte, and the next sweep converges.
#[test]
fn a_crash_at_any_call_of_a_rewrite_keeping_pinned_blobs_reopens_a_whole_pack() {
    crash_during_pinned_rewrite(4);
}

#[cfg(feature = "slow")]
#[test]
fn a_crash_at_any_call_of_a_rewrite_keeping_pinned_blobs_reopens_a_whole_pack_many_seeds() {
    crash_during_pinned_rewrite(64);
}

/// A rewrite holds the writer lock throughout (ROADMAP 9.2.7, 9.2.15, ADR-0052, ADR-0053): just before its rename, and
/// after it, the lock file beside the pack is held and a second writer fails; the new file needs no lock of its own.
#[test]
fn a_rewrite_keeps_the_writer_lock_through_the_swap() {
    let fs = sim();
    let store = open(&fs, PackMode::ReadWrite).unwrap();
    let kept = put(&store, b"kept by the mark", 4, 1);
    put(&store, b"dropped by the sweep", 4, 2);
    let pins = Pins::new([]);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let (fs, seen) = (fs.clone(), Arc::clone(&seen));
        // Under the pin set's lock, right before the rename.
        pins.on_check(move |_| {
            let second = matches!(open(&fs, PackMode::ReadWrite), Err(Error::PackLocked));
            let temps: Vec<PathBuf> = fs
                .files()
                .into_iter()
                .map(|(p, _)| p)
                .filter(|p| p.to_string_lossy().contains(".tmp."))
                .collect();
            let lock_held = !fs.open(Path::new(LOCK), true).unwrap().try_lock().unwrap();
            seen.lock().unwrap().push((second, temps.len(), lock_held));
        });
    }
    let mark = Mark::new([kept.blob_id()], persia_blob::PinGate::pinned(&pins));
    let report = sweep(&store, &mark, &pins, 0.0).unwrap();
    assert!(
        matches!(report.rewrite, Some(Rewrite::Done { .. })),
        "{report:?}"
    );
    assert_eq!(*seen.lock().unwrap(), [(true, 1, true)]);
    assert!(matches!(
        open(&fs, PackMode::ReadWrite),
        Err(Error::PackLocked)
    ));
    drop(store);
    let reopened = open(&fs, PackMode::ReadWrite).unwrap();
    assert_eq!(read_all(&reopened, &kept).unwrap(), b"kept by the mark");
}
