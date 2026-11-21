//! A resumed upload across a crash (SPEC §9.4, §9.5, §10.1, §10.1.1; ROADMAP 9.3.4, 9.3.6; ADR-0053): `put_blob` over
//! the remote layout on the local backend over a simulated file system (`LocalStorage::open_in(SimFs, ..)`), with a
//! fault at every mutating call it makes, then a crash; and the resume itself cut the same way.
//!
//! Invariants:
//! 1. A fault at any call of the staging (failed, or applied with the process gone) fails the `put_blob` with
//!    `UploadInterrupted`. After a crash that keeps everything, nothing or a seeded part of what was not synced (each
//!    point is crashed all three ways), every object left under `blobs/` is a chunk holding its chunk's bytes or the
//!    blob's manifest, all of one upload, the token's. A new process over the crashed file system, given only the token
//!    and the whole blob, binds the `BlobRef` an uninterrupted `put_blob` binds. It binds under the token's upload
//!    unless every chunk was staged already, when it moves to a new one (ROADMAP 9.3.6). The bound upload holds
//!    exactly the blob's chunks, the blob reads back, and the completion only adds objects: it deletes none.
//! 2. A fault in the bind is an unknown outcome, never `UploadInterrupted`. After the crash the blob is bound and reads
//!    back, or a new `put_blob` binds it. A `put_blob` that returned is durable: a crash right after it, losing
//!    whatever was not synced, keeps its blob.
//! 3. Faults in the staging come before faults in the bind, and some crash leaves chunks the resume keeps.
//! 4. The same holds for a resume cut by a crash at any of its calls (the restage copy of ROADMAP 9.3.6 among them),
//!    from every distinct listing a first crash leaves: a third process completes the blob from what the resume
//!    returned. Some such crash interrupts the resume after it moved to a new upload, and some before.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed and picks the seeded losses and the data; every
//! failure names the seed, the call, the fault and the loss.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::print_stderr,
    clippy::manual_assert_eq
)] // Tests: inputs are fixed or seeded; `assert!(a == b)` keeps blob bytes out of a failure message.

use core::num::NonZeroUsize;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::pins::Pins;
use persia::internal::retry::ManualRetryClock;
use persia_blob::remote::{RemoteBlobStore, RemoteKey};
use persia_blob::{BlobRef, BlobStore, ChunkSize, Error, ResumeToken, UploadId};
use persia_engine::db::SegmentIds;
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::vfs::sim::SimFs;
use persia_storage::{KeyPrefix, LocalStorage, MemoryStorage, Storage};

const ROOT: &str = "/store";
const CHUNK: u32 = ChunkSize::MIN;
const CHUNK_LEN: usize = CHUNK as usize;
/// Three full chunks and a short one, in one window.
const LEN: usize = 3 * CHUNK_LEN + 100;
const CHUNKS: u32 = 4;

type Local = RemoteBlobStore<LocalStorage<SimFs>>;
type Put = Result<(BlobRef, Pins), Error>;

/// The base seed: `PERSIA_TEST_SEED`, or a fixed default; printed so a failure can be replayed.
fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_c4a5);
    eprintln!("blob_resume_crash: seed {seed} (replay with PERSIA_TEST_SEED={seed})");
    seed
}

/// `SplitMix64`.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// One way to crash: everything unsynced lost, nothing, or a seeded subset.
    fn one_loss(&mut self) -> CrashLoss {
        match self.next_u64() % 4 {
            0 => CrashLoss::All,
            1 => CrashLoss::Nothing,
            _ => CrashLoss::Seeded(self.next_u64()),
        }
    }

    /// Every crash point loses everything unsynced, nothing, and `seeded` seeded subsets.
    fn losses(&mut self, seeded: usize) -> Vec<CrashLoss> {
        let mut out = vec![CrashLoss::All, CrashLoss::Nothing];
        out.extend((0..seeded).map(|_| CrashLoss::Seeded(self.next_u64())));
        out
    }
}

fn data(seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    (0..LEN).map(|_| rng.next_u64() as u8).collect()
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(CHUNK).unwrap(),
        // One attempt: a bind on a crashed file system answers nothing more.
        bind_budget: Duration::ZERO,
        ..PutBlobOptions::default()
    }
}

fn resume(token: ResumeToken) -> PutBlobOptions {
    PutBlobOptions {
        resume: Some(token),
        ..options()
    }
}

/// A process's handle on the store over `fs`, one request at a time, so the calls come in one order.
fn process(fs: &SimFs, ids: u64) -> Blobs<Local> {
    let storage = LocalStorage::open_in(fs.clone(), ROOT).unwrap();
    let store = RemoteBlobStore::new(storage).with_parallelism(NonZeroUsize::MIN);
    Blobs::with_upload_ids(Arc::new(store), SegmentIds::seeded(ids))
        .with_retry_clock(Arc::new(ManualRetryClock::new()))
}

/// The reference an uninterrupted `put_blob` returns, on a store of its own.
fn uninterrupted(data: &[u8]) -> BlobRef {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
    let storage = MemoryStorage::with_clock(clock as Arc<dyn Clock>);
    Blobs::new(Arc::new(RemoteBlobStore::new(storage)))
        .put_blob(data, &options())
        .unwrap()
        .0
}

/// Every key under `blobs/` (`list` skips a crashed write's temporary files, SPEC §10.1.1).
fn keys(blobs: &Blobs<Local>) -> BTreeSet<String> {
    blobs
        .store()
        .storage()
        .list(&KeyPrefix::new("blobs/").unwrap())
        .unwrap()
        .into_iter()
        .map(|meta| meta.key.as_str().to_owned())
        .collect()
}

fn chunk_key(upload_id: UploadId, index: u32) -> String {
    RemoteKey::Chunk { upload_id, index }
        .key()
        .unwrap()
        .as_str()
        .to_owned()
}

/// The keys of the blob `r` bound to `upload`: its manifest and every chunk.
fn bound_keys(r: &BlobRef, upload: UploadId) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = (0..CHUNKS).map(|i| chunk_key(upload, i)).collect();
    out.insert(
        RemoteKey::Manifest(r.blob_id())
            .key()
            .unwrap()
            .as_str()
            .to_owned(),
    );
    out
}

/// Checks every object under `blobs/` is `reference`'s manifest or a chunk holding the bytes of its chunk of `data`
/// (a crash never leaves a torn or foreign one), and returns the uploads the chunks belong to.
fn check_objects(
    blobs: &Blobs<Local>,
    data: &[u8],
    reference: &BlobRef,
    what: &str,
) -> BTreeSet<UploadId> {
    let storage = blobs.store().storage();
    let mut uploads = BTreeSet::new();
    for meta in storage.list(&KeyPrefix::new("blobs/").unwrap()).unwrap() {
        match RemoteKey::parse(&meta.key) {
            Some(RemoteKey::Manifest(id)) => {
                assert_eq!(id, reference.blob_id(), "{what}: a foreign manifest");
            }
            Some(RemoteKey::Chunk { upload_id, index }) => {
                assert!(index < CHUNKS, "{what}: chunk {index} past the blob");
                let start = index as usize * CHUNK_LEN;
                let end = (start + CHUNK_LEN).min(data.len());
                assert!(
                    storage.get(&meta.key).unwrap() == data[start..end],
                    "{what}: chunk {index} of {upload_id:?} holds other bytes"
                );
                uploads.insert(upload_id);
            }
            None => panic!("{what}: an object Persia did not write: {}", meta.key),
        }
    }
    uploads
}

/// The chunk indices listed under `upload`, sorted.
fn listed(blobs: &Blobs<Local>, upload: UploadId) -> Vec<u32> {
    let mut out: Vec<u32> = blobs
        .store()
        .staged_chunks(upload)
        .unwrap()
        .iter()
        .map(|c| c.index)
        .collect();
    out.sort_unstable();
    out
}

/// Checks the blob `r` is bound in `blobs`' store to an upload holding exactly its chunks, reads back, and returns
/// that upload.
fn check_bound(blobs: &Blobs<Local>, r: &BlobRef, data: &[u8], what: &str) -> UploadId {
    let m = blobs
        .store()
        .manifest(r.blob_id())
        .unwrap()
        .unwrap_or_else(|| panic!("{what}: not bound"));
    assert_eq!(m.chunk_count(), CHUNKS, "{what}");
    assert_eq!(
        listed(blobs, m.upload_id()),
        (0..CHUNKS).collect::<Vec<_>>(),
        "{what}"
    );
    let read = blobs
        .get_blob_range(r, 0..r.size())
        .unwrap_or_else(|e| panic!("{what}: read back: {e}"));
    assert!(read == data, "{what}: read back other bytes");
    m.upload_id()
}

/// Where a fault fell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The call returned: the blob is acknowledged.
    Acked,
    /// `UploadInterrupted`.
    Staging,
    /// A bind of unknown outcome.
    Bind,
}

/// Runs `op` in a process (upload ids from `ids`) over a copy of `fs`, with each fault at each of its mutating calls
/// in turn until neither fires, and hands `on_crash` each run whose fault fired, with the call's index and the file
/// system it left; then
/// the first run whose fault never fired, crashed after `op` returned, whose blob is acknowledged.
fn every_fault(
    fs: &SimFs,
    ids: u64,
    op: &dyn Fn(&Blobs<Local>) -> Put,
    on_crash: &mut dyn FnMut(u64, String, &SimFs, &Put),
) {
    let mut acked = false;
    for k in 0_u64.. {
        let mut fired = false;
        for fault in [
            SimFault::FailAt as fn(u64) -> SimFault,
            SimFault::ApplyThenFail,
        ] {
            // A copy: everything on `fs` is durable already.
            let run = fs.crash(CrashLoss::Nothing);
            let blobs = process(&run, ids);
            let at = run.mutations() + k;
            run.set_fault(Some(fault(at)));
            let result = op(&blobs);
            if !run.crashed() {
                // Past the last call: the fault never fired. The crash comes once `op` returned.
                assert!(result.is_ok(), "call {k}: {result:?}");
                if !acked {
                    acked = true;
                    on_crash(k, format!("after the last call ({k})"), &run, &result);
                }
                continue;
            }
            fired = true;
            on_crash(k, format!("call {k}, {:?}", fault(k)), &run, &result);
        }
        if !fired {
            break;
        }
    }
}

/// Checks what a new process `after` finds once the process that returned `result` crashed (invariants 1 and 2),
/// then completes the blob there and checks the completion only added the bound blob's objects.
fn recover(
    after: &Blobs<Local>,
    result: &Put,
    data: &[u8],
    reference: &BlobRef,
    what: &str,
) -> Phase {
    check_objects(after, data, reference, what);
    let before = keys(after);
    let (phase, bound) = match result {
        Ok((r, _)) => {
            // Acknowledged: durable, whatever the crash lost.
            assert_eq!(*r, *reference, "{what}");
            (Phase::Acked, check_bound(after, r, data, what))
        }
        Err(Error::UploadInterrupted { token, source }) => {
            assert!(matches!(**source, Error::Storage(_)), "{what}: {source:?}");
            assert_eq!(token.chunk_size.get(), CHUNK, "{what}");
            let staged = listed(after, token.upload_id);
            let (r, pins) = after
                .put_blob(data, &resume(*token))
                .unwrap_or_else(|e| panic!("{what}: resume: {e:?}"));
            assert_eq!(r, *reference, "{what}");
            let bound = check_bound(after, &r, data, what);
            // ROADMAP 9.3.6: a resume that writes no chunk the listing did not show moves to a young upload.
            let all_staged = staged == (0..CHUNKS).collect::<Vec<_>>();
            assert_eq!(
                bound != token.upload_id,
                all_staged,
                "{what}: bound under {bound:?}, token {token:?}, listed {staged:?}"
            );
            drop(pins);
            assert!(after.pin_set().pinned().is_empty(), "{what}: pins left");
            (Phase::Staging, bound)
        }
        Err(e) => {
            assert!(e.is_bind_outcome_unknown(), "{what}: {e:?}");
            if after
                .store()
                .manifest(reference.blob_id())
                .unwrap()
                .is_none()
            {
                let (r, _pins) = after
                    .put_blob(data, &options())
                    .unwrap_or_else(|e| panic!("{what}: a new put_blob: {e:?}"));
                assert_eq!(r, *reference, "{what}");
            }
            (Phase::Bind, check_bound(after, reference, data, what))
        }
    };
    let mut expected = before;
    expected.extend(bound_keys(reference, bound));
    assert_eq!(
        keys(after),
        expected,
        "{what}: the completion deleted, or wrote other than the bound blob"
    );
    check_objects(after, data, reference, what);
    phase
}

/// Where the faults fell, over every call of a `put_blob` (or of a resume). `staging` and `bind` are the distinct
/// calls (a sweep, and the call's index in it) whose fault fell in that phase, however many faults and losses each
/// got.
#[derive(Default)]
struct Tally {
    staging: BTreeSet<(usize, u64)>,
    bind: BTreeSet<(usize, u64)>,
    acked: usize,
    /// Crashes after which the resume found chunks already staged.
    kept: usize,
    /// Second crashes whose interrupted resume names a new upload, not the token it resumed.
    moved: usize,
    /// Second crashes whose interrupted resume names the token it resumed.
    stayed: usize,
    /// Distinct listings a first crash left that a second sweep started from.
    listings: usize,
}

/// Counts, not the call sets: a failure message stays short.
impl std::fmt::Debug for Tally {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tally")
            .field("staging_calls", &self.staging.len())
            .field("bind_calls", &self.bind.len())
            .field("acked", &self.acked)
            .field("kept", &self.kept)
            .field("moved", &self.moved)
            .field("stayed", &self.stayed)
            .field("listings", &self.listings)
            .finish()
    }
}

impl Tally {
    /// Counts `phase`, checking the staging faults of one sweep come before its bind faults (invariant 3).
    fn count(&mut self, phase: Phase, call: (usize, u64), bind_started: &mut bool, what: &str) {
        match phase {
            Phase::Acked => self.acked += 1,
            Phase::Staging => {
                assert!(!*bind_started, "{what}: a staging fault after a bind fault");
                self.staging.insert(call);
            }
            Phase::Bind => {
                *bind_started = true;
                self.bind.insert(call);
            }
        }
    }
}

fn crash_anywhere(seed: u64, seeded_losses: usize) -> Tally {
    let mut rng = Rng(seed);
    let data = data(seed);
    let reference = uninterrupted(&data);
    let mut tally = Tally::default();
    let mut bind_started = false;
    let put = |blobs: &Blobs<Local>| blobs.put_blob(&data[..], &options());
    every_fault(&SimFs::new(), seed, &put, &mut |k, at, fs, result| {
        for loss in rng.losses(seeded_losses) {
            let what = format!("seed {seed}, {at}, {loss:?}");
            // A new process: other upload ids than the crashed one's.
            let after = process(&fs.crash(loss), !seed);
            let uploads = check_objects(&after, &data, &reference, &what);
            assert!(
                uploads.len() <= 1,
                "{what}: one process, one upload: {uploads:?}"
            );
            if let Err(Error::UploadInterrupted { token, .. }) = result {
                assert!(
                    uploads.iter().all(|u| *u == token.upload_id),
                    "{what}: {uploads:?}, token {token:?}"
                );
                if !listed(&after, token.upload_id).is_empty() {
                    tally.kept += 1;
                }
            }
            let phase = recover(&after, result, &data, &reference, &what);
            tally.count(phase, (0, k), &mut bind_started, &what);
        }
    });
    tally
}

/// Invariant 4: from each distinct listing a first crash in the staging leaves, a resume cut at every call, then a third
/// process. With `every_loss`, the first crash also loses a seeded subset (which may list other chunks) and each
/// point of the resume is crashed every way, not one seeded way.
fn crash_anywhere_twice(seed: u64, every_loss: bool) -> Tally {
    let mut rng = Rng(seed ^ 0x7769_6365);
    let data = data(seed);
    let reference = uninterrupted(&data);
    let mut tally = Tally::default();
    let mut listings: BTreeSet<Vec<u32>> = BTreeSet::new();
    let put = |blobs: &Blobs<Local>| blobs.put_blob(&data[..], &options());
    every_fault(&SimFs::new(), seed, &put, &mut |_, at, fs, first| {
        let Err(Error::UploadInterrupted { token, .. }) = first else {
            return;
        };
        let token = *token;
        for loss in rng.losses(usize::from(every_loss)) {
            let crashed = fs.crash(loss);
            let listing = listed(&process(&crashed, !seed), token.upload_id);
            if !listings.insert(listing.clone()) {
                continue;
            }
            let sweep = tally.listings;
            tally.listings += 1;
            let first_what = format!("seed {seed}, first {at}, {loss:?}, listed {listing:?}");
            let mut bind_started = false;
            let resumed = |blobs: &Blobs<Local>| blobs.put_blob(&data[..], &resume(token));
            every_fault(&crashed, !seed, &resumed, &mut |k, at, fs, second| {
                let losses = if every_loss {
                    rng.losses(2)
                } else {
                    vec![rng.one_loss()]
                };
                for loss in losses {
                    let what = format!("{first_what}; resume {at}, {loss:?}");
                    let after = process(&fs.crash(loss), seed.rotate_left(17));
                    if let Err(Error::UploadInterrupted { token: again, .. }) = second {
                        if *again == token {
                            tally.stayed += 1;
                        } else {
                            tally.moved += 1;
                        }
                    }
                    let phase = recover(&after, second, &data, &reference, &what);
                    tally.count(phase, (sweep, k), &mut bind_started, &what);
                }
            });
        }
    });
    tally
}

fn check_tally(tally: &Tally) {
    eprintln!("blob_resume_crash: {tally:?}");
    // Each chunk write is several calls (SPEC §10.1.1: a temporary file, synced, then linked), and so is the bind.
    assert!(tally.staging.len() >= 4 * CHUNKS as usize, "{tally:?}");
    assert!(tally.bind.len() >= 2, "{tally:?}");
    assert!(
        tally.acked > 0,
        "no crash after an acknowledged put_blob: {tally:?}"
    );
    assert!(
        tally.kept > 0,
        "no crash left chunks to resume from: {tally:?}"
    );
}

fn check_twice_tally(tally: &Tally) {
    eprintln!("blob_resume_crash (twice): {tally:?}");
    // From no chunk staged up to every chunk staged (the 9.3.6 restage).
    assert!(tally.listings > CHUNKS as usize, "{tally:?}");
    assert!(tally.bind.len() >= 2, "{tally:?}");
    assert!(
        tally.acked > 0,
        "no crash after an acknowledged put_blob: {tally:?}"
    );
    assert!(
        tally.moved > 0,
        "no resume was cut after it moved: {tally:?}"
    );
    assert!(
        tally.stayed > 0,
        "no resume was cut before it moved: {tally:?}"
    );
}

#[test]
fn an_upload_cut_by_a_crash_at_any_call_resumes_from_its_token() {
    check_tally(&crash_anywhere(base_seed(), 1));
}

#[test]
fn a_resume_cut_by_a_crash_at_any_call_resumes_again() {
    check_twice_tally(&crash_anywhere_twice(base_seed(), false));
}

#[cfg(feature = "slow")]
#[test]
fn an_upload_cut_by_a_crash_at_any_call_resumes_from_its_token_many_losses() {
    let seed = base_seed();
    for i in 0..8 {
        check_tally(&crash_anywhere(seed.wrapping_add(i), 6));
    }
}

#[cfg(feature = "slow")]
#[test]
fn a_resume_cut_by_a_crash_at_any_call_resumes_again_every_way() {
    let seed = base_seed();
    for i in 0..4 {
        check_twice_tally(&crash_anywhere_twice(seed.wrapping_add(i), true));
    }
}
