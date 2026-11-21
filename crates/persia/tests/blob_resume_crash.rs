//! A resumed upload across a crash (SPEC §9.4, §9.5, §10.1, §10.1.1; ROADMAP 9.3.4; ADR-0053): `put_blob` over the
//! remote layout on the local backend over a simulated file system (`LocalStorage::open_in(SimFs, ..)`), with a fault
//! at every mutating call it makes, then a crash.
//!
//! Invariants:
//! 1. A fault at any call of the staging (failed, or applied with the process gone) fails the `put_blob` with
//!    `UploadInterrupted`. After a crash that keeps everything, nothing or a seeded part of what was not synced, every
//!    chunk object left holds its chunk's bytes, and a new process over the crashed file system, given only the token
//!    and the whole blob, binds the `BlobRef` an uninterrupted `put_blob` binds. The bound upload holds exactly the
//!    blob's chunks, and the blob reads back.
//! 2. A fault in the bind is an unknown outcome, never `UploadInterrupted`. After the crash the blob is bound and reads
//!    back, or a new `put_blob` binds it. A `put_blob` that returned is durable: the crash keeps its blob.
//! 3. Faults in the staging come before faults in the bind, and some crash leaves chunks the resume keeps.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed and picks each crash point's loss; every
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
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::retry::ManualRetryClock;
use persia_blob::remote::{RemoteBlobStore, RemoteKey};
use persia_blob::{BlobRef, BlobStore, ChunkSize, Error};
use persia_engine::db::SegmentIds;
use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::vfs::sim::SimFs;
use persia_storage::{LocalStorage, MemoryStorage, Storage};

const ROOT: &str = "/store";
const CHUNK: u32 = ChunkSize::MIN;
const CHUNK_LEN: usize = CHUNK as usize;
/// Three full chunks and a short one, in one window.
const LEN: usize = 3 * CHUNK_LEN + 100;
const CHUNKS: u32 = 4;

type Local = RemoteBlobStore<LocalStorage<SimFs>>;

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

    fn loss(&mut self) -> CrashLoss {
        match self.next_u64() % 4 {
            0 => CrashLoss::All,
            1 => CrashLoss::Nothing,
            _ => CrashLoss::Seeded(self.next_u64()),
        }
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

/// Checks the blob `r` is bound in `blobs`' store to an upload holding exactly its chunks, and reads back.
fn check_bound(blobs: &Blobs<Local>, r: &BlobRef, data: &[u8], what: &str) {
    let store = blobs.store();
    let m = store
        .manifest(r.blob_id())
        .unwrap()
        .unwrap_or_else(|| panic!("{what}: not bound"));
    assert_eq!(m.chunk_count(), CHUNKS, "{what}");
    let mut staged: Vec<u32> = store
        .staged_chunks(m.upload_id())
        .unwrap()
        .iter()
        .map(|c| c.index)
        .collect();
    staged.sort_unstable();
    assert_eq!(staged, (0..CHUNKS).collect::<Vec<_>>(), "{what}");
    let read = blobs
        .get_blob_range(r, 0..r.size())
        .unwrap_or_else(|e| panic!("{what}: read back: {e}"));
    assert!(read == data, "{what}: read back other bytes");
}

/// Every chunk object of `upload` holds the bytes of its chunk of `data`: a crash never leaves a torn one.
fn check_chunk_objects(
    blobs: &Blobs<Local>,
    upload: persia_blob::UploadId,
    data: &[u8],
    what: &str,
) -> usize {
    let store = blobs.store();
    let listed = store.staged_chunks(upload).unwrap();
    for chunk in &listed {
        let key = RemoteKey::Chunk {
            upload_id: upload,
            index: chunk.index,
        }
        .key()
        .unwrap();
        let start = chunk.index as usize * CHUNK_LEN;
        let end = (start + CHUNK_LEN).min(data.len());
        assert!(
            store.storage().get(&key).unwrap() == data[start..end],
            "{what}: chunk {} holds other bytes",
            chunk.index
        );
    }
    listed.len()
}

/// Where the faults fell, over every call of a `put_blob`.
#[derive(Debug, Default)]
struct Tally {
    staging: usize,
    bind: usize,
    /// Crashes after which the resume found chunks already staged.
    kept: usize,
}

fn crash_anywhere(seed: u64, losses_per_point: usize) -> Tally {
    let mut rng = Rng(seed);
    let data = data(seed);
    let reference = uninterrupted(&data);
    let mut tally = Tally::default();
    let mut bind_started = false;
    for k in 0_u64.. {
        let mut fired = false;
        for fault in [
            SimFault::FailAt as fn(u64) -> SimFault,
            SimFault::ApplyThenFail,
        ] {
            let fs = SimFs::new();
            let blobs = process(&fs, seed);
            let at = fs.mutations() + k;
            fs.set_fault(Some(fault(at)));
            let result = blobs.put_blob(&data[..], &options());
            if !fs.crashed() {
                // Past the last call: the fault never fired.
                assert!(result.is_ok(), "seed {seed}, call {k}: {result:?}");
                continue;
            }
            fired = true;
            for _ in 0..losses_per_point {
                let loss = rng.loss();
                let what = format!("seed {seed}, call {k}, {:?}, {loss:?}", fault(k));
                let crashed = fs.crash(loss);
                // A new process: other upload ids than the crashed one's.
                let after = process(&crashed, !seed);
                match &result {
                    Ok((r, _)) => {
                        // Acknowledged: durable, whatever the crash lost.
                        assert_eq!(*r, reference, "{what}");
                        check_bound(&after, r, &data, &what);
                    }
                    Err(Error::UploadInterrupted { token, source }) => {
                        assert!(!bind_started, "{what}: a staging fault after a bind fault");
                        assert!(matches!(**source, Error::Storage(_)), "{what}: {source:?}");
                        assert_eq!(token.chunk_size.get(), CHUNK, "{what}");
                        tally.staging += 1;
                        if check_chunk_objects(&after, token.upload_id, &data, &what) > 0 {
                            tally.kept += 1;
                        }
                        let resumed = PutBlobOptions {
                            resume: Some(*token),
                            ..options()
                        };
                        let (r, _pins) = after
                            .put_blob(&data[..], &resumed)
                            .unwrap_or_else(|e| panic!("{what}: resume: {e:?}"));
                        assert_eq!(r, reference, "{what}");
                        check_bound(&after, &r, &data, &what);
                    }
                    Err(e) => {
                        assert!(e.is_bind_outcome_unknown(), "{what}: {e:?}");
                        bind_started = true;
                        tally.bind += 1;
                        if after
                            .store()
                            .manifest(reference.blob_id())
                            .unwrap()
                            .is_none()
                        {
                            let (r, _pins) = after
                                .put_blob(&data[..], &options())
                                .unwrap_or_else(|e| panic!("{what}: a new put_blob: {e:?}"));
                            assert_eq!(r, reference, "{what}");
                        }
                        check_bound(&after, &reference, &data, &what);
                    }
                }
            }
        }
        if !fired {
            break;
        }
    }
    tally
}

fn check_tally(tally: &Tally) {
    eprintln!("blob_resume_crash: {tally:?}");
    // Each chunk write is several calls (SPEC §10.1.1: a temporary file, synced, then linked), and so is the bind.
    assert!(tally.staging >= 4 * CHUNKS as usize, "{tally:?}");
    assert!(tally.bind >= 2, "{tally:?}");
    assert!(
        tally.kept > 0,
        "no crash left chunks to resume from: {tally:?}"
    );
}

#[test]
fn an_upload_cut_by_a_crash_at_any_call_resumes_from_its_token() {
    check_tally(&crash_anywhere(base_seed(), 1));
}

#[cfg(feature = "slow")]
#[test]
fn an_upload_cut_by_a_crash_at_any_call_resumes_from_its_token_many_losses() {
    let seed = base_seed();
    for i in 0..8 {
        check_tally(&crash_anywhere(seed.wrapping_add(i), 6));
    }
}
