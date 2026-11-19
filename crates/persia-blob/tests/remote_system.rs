//! System-level tests of the remote layout (ROADMAP 9.3; SPEC §9.4, ADR-0050) over the real local backend: many
//! threads, real processes killed mid-upload, and a fault-injecting backend under a multi-blob workload.
//!
//! Invariants:
//! 1. Concurrent writers of overlapping blob sets (the same bytes from several threads, in two chunk sizes, chunked
//!    and inline): each blob ends with exactly one manifest object; a writer told `Bound` wrote exactly the stored
//!    manifest; a chunked blob has exactly one `Bound` writer; every writer's outcome manifest yields a `BlobRef` that
//!    reads back the bytes; once the losers discard their uploads, the only staged chunks left are the bound
//!    manifests'. Readers polling meanwhile never see a manifest whose chunks are not all readable, nor wrong bytes.
//! 2. A writer process killed (SIGKILL) at seeded points mid-upload, several times in a row, each restart resuming from a
//!    fresh store instance (`resume`): the resumed run covers at least every chunk the killed process acknowledged,
//!    every chunk object present holds exactly its chunk's bytes (no torn chunk is visible), the blob binds and reads
//!    back, and every key under `blobs/` is one the layout writes (a killed write leaves nothing a listing shows).
//! 3. A multi-writer, multi-blob workload over `FaultyStorage` (seeded `Transient`, `ResponseLost`, `Io`), with the
//!    callers following SPEC §9.4 (bind again after `BindOutcomeUnknown`, keep the chunks; discard only after
//!    `AlreadyBound` to another upload), some giving up after an unknown outcome: no read through the faulty store ever
//!    returns wrong bytes or reports corruption, and afterwards every manifest present (including those an abandoned
//!    unknown bind in fact wrote) reads back its exact bytes through a fault-free store.
//! 4. With `DroppedPut` (a write acknowledged but lost) added, a read is the exact bytes or an error: never other bytes.
//! 5. On real files, a retried chunk write (equal bytes) keeps the chunk's time, as a conflicting one does.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed; replay a failure with
//! `PERSIA_TEST_SEED=<seed> cargo test -p persia-blob --test remote_system`. `--features slow` runs more iterations.

#![allow(
    clippy::too_many_lines,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::print_stdout,
    clippy::print_stderr,
    missing_docs
)]

mod common;

use core::num::{NonZeroU64, NonZeroUsize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use common::{blob_ref, data_of, keys_of, manifest_of};
use persia_blob::remote::{RemoteKey, ResumeToken};
use persia_blob::{
    BindOutcome, BlobId, BlobManifest, BlobStore, ChunkSize, Error, RemoteBlobStore,
    SMALL_BLOB_MAX, UploadId, read_range,
};
use persia_format::checksum::crc32c;
use persia_storage::faulty::{Fault, FaultyStorage, RandomFaults};
use persia_storage::{Key, KeyPrefix, LocalStorage, MemoryStorage, Storage};

fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_0903_0001);
    eprintln!("PERSIA_TEST_SEED={seed}");
    seed
}

/// `SplitMix64`: small, seedable, good enough for test schedules.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, self.below(i as u64 + 1) as usize);
        }
    }
}

fn par(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn local(root: &Path) -> RemoteBlobStore<LocalStorage> {
    RemoteBlobStore::new(LocalStorage::open(root).unwrap())
}

/// The manifest key of every blob bound in `storage`, with its decoded manifest.
fn bound_manifests<S: Storage + ?Sized>(storage: &S) -> BTreeMap<String, BlobManifest> {
    keys_of(storage)
        .into_iter()
        .filter(|k| {
            matches!(
                RemoteKey::parse(&Key::new(k.as_str()).unwrap()),
                Some(RemoteKey::Manifest(_))
            )
        })
        .map(|k| {
            let bytes = storage.get(&Key::new(k.as_str()).unwrap()).unwrap();
            (k, BlobManifest::decode(&bytes).unwrap())
        })
        .collect()
}

/// The upload ids that have chunk objects in `storage`.
fn staged_uploads<S: Storage + ?Sized>(storage: &S) -> BTreeSet<u128> {
    keys_of(storage)
        .into_iter()
        .filter_map(|k| match RemoteKey::parse(&Key::new(k.as_str()).unwrap()) {
            Some(RemoteKey::Chunk { upload_id, .. }) => Some(upload_id.get()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Concurrent writers of overlapping blob sets.
// ---------------------------------------------------------------------------------------------------------------

/// What one writer learned about one blob.
#[derive(Debug, Clone)]
struct Outcome {
    blob: usize,
    upload: u128,
    bound: bool,
    manifest: BlobManifest,
}

/// Writes `data` the way a writer would (SPEC §9.4, §9.6): inline when it fits one chunk of at most
/// `SMALL_BLOB_MAX`, else staged with `stage_chunks` and bound. Discards its upload when another one is bound.
fn write_blob<S: Storage>(
    store: &RemoteBlobStore<S>,
    blob: usize,
    data: &[u8],
    chunk: u32,
    upload: u128,
) -> Outcome {
    let m = manifest_of(data, chunk, upload);
    let small = data.len() as u64 <= u64::from(chunk).min(SMALL_BLOB_MAX);
    let outcome = if small {
        store.bind_small(&m, data).unwrap()
    } else {
        let chunks: Vec<&[u8]> = data.chunks(chunk as usize).collect();
        store.stage_chunks(m.upload_id(), 0, &chunks).unwrap();
        store.bind(&m).unwrap()
    };
    match outcome {
        BindOutcome::Bound => {
            // What `bind_small` wrote is the inline form; read it back to compare with the stored one.
            let written = store.manifest(m.blob_id()).unwrap().unwrap();
            Outcome {
                blob,
                upload,
                bound: true,
                manifest: written,
            }
        }
        BindOutcome::AlreadyBound(bound) => {
            assert_eq!(bound.blob_id(), m.blob_id());
            if bound.upload_id() != m.upload_id() {
                store.discard_upload(m.upload_id()).unwrap();
            }
            Outcome {
                blob,
                upload,
                bound: false,
                manifest: bound,
            }
        }
    }
}

#[test]
fn concurrent_writers_of_overlapping_blobs_on_the_local_backend() {
    let seed = base_seed();
    let dir = tempfile::tempdir().unwrap();
    // Edge sizes around both chunk sizes and the inline threshold; duplicates (equal bytes) across every writer.
    let sizes = [
        0, 1, 100, 16_383, 16_384, 16_385, 65_535, 65_536, 65_537, 100_000,
    ];
    let corpus: Arc<Vec<Vec<u8>>> = Arc::new(
        sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| data_of(n, i as u8 + 1))
            .collect(),
    );
    let writers = if cfg!(feature = "slow") { 16 } else { 6 };
    let readers = 2;
    let barrier = Arc::new(Barrier::new(writers + readers));
    let done = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let root = dir.path().to_owned();

    let reader_handles: Vec<_> = (0..readers)
        .map(|r| {
            let (corpus, barrier, done, reads, root) = (
                Arc::clone(&corpus),
                Arc::clone(&barrier),
                Arc::clone(&done),
                Arc::clone(&reads),
                root.clone(),
            );
            std::thread::spawn(move || {
                let store = local(&root);
                let mut rng = Rng(seed ^ (0xa11ce + r as u64));
                barrier.wait();
                // At least one full pass after the writers finish, so the last manifests are read too.
                let mut last_pass = false;
                loop {
                    let finished = done.load(Ordering::SeqCst);
                    for (i, data) in corpus.iter().enumerate() {
                        let id = persia_blob::blob_id_of(data);
                        // A visible manifest must name chunks that are all readable: chunks are staged first.
                        if let Some(m) = store.manifest(id).unwrap() {
                            let n = m.size();
                            let end = if n == 0 { 0 } else { 1 + rng.below(n) };
                            let start = end / 2;
                            let got = read_range(&store, &blob_ref(&m), start..end)
                                .unwrap_or_else(|e| panic!("blob {i}: {e:?}"));
                            assert_eq!(got, &data[start as usize..end as usize], "blob {i}");
                            reads.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if last_pass {
                        break;
                    }
                    last_pass = finished;
                }
            })
        })
        .collect();

    let writer_handles: Vec<_> = (0..writers)
        .map(|w| {
            let (corpus, barrier, root) = (Arc::clone(&corpus), Arc::clone(&barrier), root.clone());
            std::thread::spawn(move || {
                // Each writer its own store instance (as separate processes would be), half with another chunk size.
                let store = local(&root).with_parallelism(par(1 + w % 4));
                let chunk = if w % 2 == 0 { 16_384 } else { ChunkSize::MIN };
                let mut order: Vec<usize> = (0..corpus.len()).collect();
                Rng(seed ^ w as u64).shuffle(&mut order);
                barrier.wait();
                order
                    .into_iter()
                    .map(|b| {
                        let upload = ((w as u128 + 1) << 64) | (b as u128 + 1);
                        write_blob(&store, b, &corpus[b], chunk, upload)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();

    let outcomes: Vec<Outcome> = writer_handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    done.store(true, Ordering::SeqCst);
    for h in reader_handles {
        h.join().unwrap();
    }

    let storage = LocalStorage::open(&root).unwrap();
    let stored = bound_manifests(&storage);
    assert_eq!(stored.len(), corpus.len(), "one manifest per blob");
    let fresh = local(&root);
    for (b, data) in corpus.iter().enumerate() {
        let id = persia_blob::blob_id_of(data);
        let key = RemoteKey::Manifest(id).key().unwrap().as_str().to_owned();
        let bound = &stored[&key];
        let mine: Vec<&Outcome> = outcomes.iter().filter(|o| o.blob == b).collect();
        assert_eq!(mine.len(), writers);
        let winners = mine.iter().filter(|o| o.bound).count();
        assert!(winners >= 1, "blob {b}: nobody bound it");
        if bound.inline_bytes().is_none() {
            assert_eq!(winners, 1, "blob {b}: a chunked blob has one binder");
        }
        for o in &mine {
            assert_eq!(&o.manifest, bound, "blob {b}, upload {:x}", o.upload);
            let got = read_range(&fresh, &blob_ref(&o.manifest), 0..o.manifest.size()).unwrap();
            assert_eq!(got, *data, "blob {b}");
        }
    }
    // The losers discarded their uploads: what is staged is exactly the bound chunked manifests'.
    let expected: BTreeSet<u128> = stored
        .values()
        .filter(|m| m.inline_bytes().is_none() && m.chunk_count() > 0)
        .map(|m| m.upload_id().get())
        .collect();
    assert_eq!(staged_uploads(&storage), expected);
    eprintln!(
        "concurrent writers: {writers} writers x {} blobs, {} concurrent reads",
        corpus.len(),
        reads.load(Ordering::Relaxed)
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 2. A writer process killed mid-upload and resumed.
// ---------------------------------------------------------------------------------------------------------------

const ROLE_ENV: &str = "PERSIA_BLOB_CHILD_ROLE";
const ROOT_ENV: &str = "PERSIA_BLOB_CHILD_ROOT";
const TAG: &str = "@@persia-blob ";
/// The killed upload: big enough that a SIGKILL lands mid-transfer, small enough to stay fast.
const KILL_LEN: usize = 1 << 20;
const KILL_CHUNK: u32 = 8 << 10;
const KILL_UPLOAD: u128 = 0x00c0_ffee;
const KILL_SEED: u8 = 91;
/// Chunks a child stages per `stage_chunks` call (and reports as acknowledged after it).
const WINDOW: usize = 4;

fn kill_token() -> ResumeToken {
    ResumeToken {
        upload_id: UploadId::new(KILL_UPLOAD),
        chunk_size: ChunkSize::from_encoding(KILL_CHUNK).unwrap(),
    }
}

fn report(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{TAG}{line}").unwrap();
    out.flush().unwrap();
}

/// The child: resumes the upload from whatever the store holds, stages the rest in windows (reporting each
/// acknowledged window), binds, and reports `bound`. In a normal test run it does nothing.
#[test]
fn child_role() {
    if std::env::var(ROLE_ENV).as_deref() != Ok("upload") {
        return;
    }
    let root = std::env::var(ROOT_ENV).unwrap();
    let store = local(Path::new(&root)).with_parallelism(par(4));
    let data = data_of(KILL_LEN, KILL_SEED);
    let resumed = store.resume(kill_token()).unwrap();
    report(&format!("resumed {}", resumed.next_index()));
    let offset = resumed.offset() as usize;
    let (mut chunker, mut crcs) = resumed.into_chunker(&data[offset..]).unwrap();
    loop {
        let mut window = Vec::new();
        while window.len() < WINDOW {
            match chunker.next_chunk().unwrap() {
                Some(c) => window.push(c),
                None => break,
            }
        }
        let Some(first) = window.first().map(persia_blob::Chunk::index) else {
            break;
        };
        let slices: Vec<&[u8]> = window.iter().map(persia_blob::Chunk::bytes).collect();
        store
            .stage_chunks(kill_token().upload_id, first, &slices)
            .unwrap();
        crcs.extend(window.iter().map(persia_blob::Chunk::crc32c));
        report(&format!("acked {}", crcs.len()));
    }
    let blob = chunker.summary().unwrap();
    let m = BlobManifest::new(
        blob.blob_id,
        blob.size,
        blob.chunk_size,
        kill_token().upload_id,
        crcs,
    )
    .unwrap();
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    report("bound");
}

fn spawn_child(root: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, "upload")
        .env(ROOT_ENV, root)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

/// Runs one child; kills it (SIGKILL) `delay` after it reports `kill_at` acknowledged chunks (so the kill lands
/// anywhere in the next window: mid-write, mid-fsync, between chunks), or lets it finish when `kill_at` is `None`.
/// Returns the highest acknowledged count seen and whether it reported `bound`.
fn run_child(root: &Path, kill_at: Option<u32>, delay: Duration) -> (u32, bool) {
    let mut child = spawn_child(root);
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let (mut acked, mut bound) = (0, false);
    for line in stdout.lines() {
        let line = line.unwrap();
        let Some(msg) = line.strip_prefix(TAG) else {
            continue;
        };
        if let Some(n) = msg.strip_prefix("resumed ") {
            acked = n.parse().unwrap();
        } else if let Some(n) = msg.strip_prefix("acked ") {
            acked = n.parse().unwrap();
        } else if msg == "bound" {
            bound = true;
        }
        if kill_at.is_some_and(|k| acked >= k) {
            // The only sleep: a seeded delay before the SIGKILL, never a synchronization.
            std::thread::sleep(delay);
            child.kill().unwrap();
            break;
        }
    }
    let status = child.wait().unwrap();
    if kill_at.is_none() {
        assert!(status.success() && bound, "the last child failed: {status}");
    }
    (acked, bound)
}

fn check_staged_chunks(storage: &LocalStorage, m: &BlobManifest, data: &[u8]) {
    for key in keys_of(storage) {
        let parsed = RemoteKey::parse(&Key::new(key.as_str()).unwrap());
        assert!(parsed.is_some(), "a key the layout never writes: {key}");
        if let Some(RemoteKey::Chunk { upload_id, index }) = parsed {
            assert_eq!(upload_id, m.upload_id());
            let bytes = storage.get(&Key::new(key.as_str()).unwrap()).unwrap();
            let start = index as usize * KILL_CHUNK as usize;
            let end = (start + KILL_CHUNK as usize).min(data.len());
            assert_eq!(&bytes[..], &data[start..end], "chunk {index}");
            assert_eq!(crc32c(&bytes), m.chunk_crcs()[index as usize]);
        }
    }
}

fn kill_and_resume(seed: u64, kills: usize) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let data = data_of(KILL_LEN, KILL_SEED);
    let want = manifest_of(&data, KILL_CHUNK, KILL_UPLOAD);
    let total = want.chunk_count();
    let mut rng = Rng(seed);
    let mut acked = 0;
    for _ in 0..kills {
        // Somewhere after what is already staged, so every child makes progress before it dies.
        let kill_at = acked + 1 + rng.below(u64::from(total - acked).max(1)) as u32;
        let (reported, _) = run_child(
            root,
            Some(kill_at.min(total)),
            Duration::from_micros(rng.below(20_000)),
        );
        assert!(reported >= acked, "a child reported less than the last one");
        // A fresh store instance, as a restarted process: the acknowledged chunks are all there, intact.
        let store = local(root).with_parallelism(par(3));
        let resumed = store.resume(kill_token()).unwrap();
        assert!(
            resumed.next_index() >= reported,
            "seed {seed}: resumed at {} after {reported} acknowledged",
            resumed.next_index()
        );
        assert_eq!(
            resumed.chunk_crcs(),
            &want.chunk_crcs()[..resumed.next_index() as usize]
        );
        check_staged_chunks(store.storage(), &want, &data);
        eprintln!(
            "seed {seed}: killed at {kill_at}, {reported} acknowledged, resumed at {}",
            resumed.next_index()
        );
        acked = resumed.next_index();
        if acked == total {
            break;
        }
    }
    run_child(root, None, Duration::ZERO);
    let store = local(root);
    assert_eq!(store.manifest(want.blob_id()).unwrap(), Some(want.clone()));
    assert_eq!(
        read_range(&store, &blob_ref(&want), 0..want.size()).unwrap(),
        data
    );
    check_staged_chunks(store.storage(), &want, &data);
}

#[cfg(unix)]
#[test]
fn a_writer_process_killed_mid_upload_resumes_from_a_fresh_store() {
    let seed = base_seed();
    let iterations = if cfg!(feature = "slow") { 40 } else { 3 };
    for i in 0..iterations {
        kill_and_resume(seed.wrapping_add(i), 3);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 3–4. A multi-blob workload over FaultyStorage.
// ---------------------------------------------------------------------------------------------------------------

/// How many attempts a caller makes before it gives up on a step: far more than a 1-in-4 fault rate needs.
const ATTEMPTS: usize = 64;

#[derive(Debug, Default)]
struct Tally {
    unknown: u64,
    abandoned: u64,
    reads_ok: u64,
    reads_err: u64,
}

/// A read through the faulty store: the exact bytes, or an error. With `lossy` (acknowledged writes may be lost),
/// corruption and a missing blob are expected errors; without, a read error is never corruption.
fn check_read<S: BlobStore>(store: &S, m: &BlobManifest, data: &[u8], lossy: bool, t: &mut Tally) {
    match read_range(store, &blob_ref(m), 0..m.size()) {
        Ok(got) => {
            assert_eq!(got, data, "wrong bytes for {:?}", m.blob_id());
            t.reads_ok += 1;
        }
        Err(e) => {
            assert!(lossy || !e.is_corruption(), "{e:?}");
            t.reads_err += 1;
        }
    }
}

/// Retries a step whose errors are the backend's (what a caller does on `Transient`/`Io`).
fn retry<T>(mut f: impl FnMut() -> persia_blob::Result<T>) -> T {
    for _ in 0..ATTEMPTS {
        match f() {
            Ok(v) => return v,
            Err(Error::Storage(_)) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    panic!("{ATTEMPTS} attempts failed");
}

/// Writers `0..writers` each write every blob of the corpus, interleaved in a seeded order, over `faulty`. Returns the
/// tally; asserts every invariant on the way and on what `inner` holds at the end.
fn faulty_workload<S: Storage + Clone>(inner: S, seed: u64, lossy: bool) -> Tally {
    let mut rng = Rng(seed);
    let faulty = Arc::new(FaultyStorage::new(inner.clone(), seed));
    let mut fault_kinds = vec![Fault::Transient, Fault::ResponseLost, Fault::Io];
    if lossy {
        fault_kinds.push(Fault::DroppedPut);
    }
    faulty.set_random(Some(RandomFaults {
        one_in: NonZeroU64::new(4).unwrap(),
        faults: fault_kinds,
    }));
    // One request at a time, so the fault schedule is a function of the seed.
    let store = RemoteBlobStore::new(Arc::clone(&faulty)).with_parallelism(par(1));
    let chunk = 512;
    let corpus: Vec<Vec<u8>> = [0, 1, 300, 512, 513, 2048, 5000]
        .iter()
        .enumerate()
        .map(|(i, &n)| data_of(n, (seed as u8) ^ (i as u8 + 1)))
        .collect();
    let writers = 3;
    let mut tasks: Vec<(usize, usize)> = (0..writers)
        .flat_map(|w| (0..corpus.len()).map(move |b| (w, b)))
        .collect();
    rng.shuffle(&mut tasks);
    let mut t = Tally::default();
    let mut known: BTreeMap<BlobId, BlobManifest> = BTreeMap::new();
    for (w, b) in tasks {
        let data = &corpus[b];
        let upload = ((w as u128 + 1) << 32) | (b as u128 + 1);
        let m = manifest_of(data, chunk, upload);
        let small = data.len() as u64 <= u64::from(chunk);
        if !small {
            for (i, c) in data.chunks(chunk as usize).enumerate() {
                retry(|| store.stage_chunk(m.upload_id(), i as u32, c));
            }
        }
        // A caller that sees an unknown outcome binds again; one in three gives up instead (and keeps the chunks).
        let give_up = rng.below(3) == 0;
        let mut outcome = None;
        for _ in 0..ATTEMPTS {
            let r = if small {
                store.bind_small(&m, data)
            } else {
                store.bind(&m)
            };
            match r {
                Ok(o) => {
                    outcome = Some(o);
                    break;
                }
                Err(e) if e.is_bind_outcome_unknown() => {
                    t.unknown += 1;
                    if give_up {
                        t.abandoned += 1;
                        break;
                    }
                }
                // The get after `AlreadyExists` failed: not ours, the caller may simply bind again.
                Err(Error::Storage(_)) => {}
                Err(e) => panic!("{e:?}"),
            }
        }
        match outcome {
            Some(BindOutcome::Bound) => {
                known.entry(m.blob_id()).or_insert_with(|| m.clone());
            }
            Some(BindOutcome::AlreadyBound(bound)) => {
                assert_eq!(bound.blob_id(), m.blob_id());
                assert_eq!(bound.size(), m.size());
                if bound.upload_id() != m.upload_id() {
                    retry(|| store.discard_upload(m.upload_id()));
                }
                known.entry(m.blob_id()).or_insert(bound);
            }
            None if give_up => {}
            None => panic!("seed {seed}: bind never answered"),
        }
        // Read a random blob some writer was told is bound.
        let pick = rng.below(known.len().max(1) as u64) as usize;
        if let Some((_, m)) = known.iter().nth(pick) {
            let data = corpus
                .iter()
                .find(|d| persia_blob::blob_id_of(d) == m.blob_id())
                .unwrap();
            check_read(&store, m, data, lossy, &mut t);
        }
    }
    // Fault-free: every manifest present reads back its exact bytes, abandoned-but-landed binds included.
    let clean = RemoteBlobStore::new(inner);
    for m in bound_manifests(clean.storage()).values() {
        let data = corpus
            .iter()
            .find(|d| persia_blob::blob_id_of(d) == m.blob_id())
            .expect("a manifest of a blob nobody wrote");
        match read_range(&clean, &blob_ref(m), 0..m.size()) {
            Ok(got) => assert_eq!(&got, data, "seed {seed}"),
            Err(e) => assert!(lossy, "seed {seed}: a bound blob lost: {e:?}"),
        }
    }
    if !lossy {
        // Every blob a writer was told is bound is still there.
        for (id, m) in &known {
            assert_eq!(
                clean
                    .manifest(*id)
                    .unwrap()
                    .as_ref()
                    .map(BlobManifest::blob_id),
                Some(m.blob_id()),
                "seed {seed}"
            );
        }
    }
    t
}

#[test]
fn a_faulty_backend_never_yields_wrong_bytes_nor_loses_a_bound_blob() {
    let seed = base_seed();
    let seeds = if cfg!(feature = "slow") { 400 } else { 40 };
    let mut total = Tally::default();
    for i in 0..seeds {
        let s = seed.wrapping_add(i);
        let t = faulty_workload(Arc::new(MemoryStorage::new()), s, false);
        total.unknown += t.unknown;
        total.abandoned += t.abandoned;
        total.reads_ok += t.reads_ok;
        total.reads_err += t.reads_err;
    }
    eprintln!("memory, {seeds} seeds: {total:?}");
    // The scenario is only meaningful if the faults hit the binds.
    assert!(total.unknown > 0 && total.abandoned > 0 && total.reads_err > 0);
}

#[test]
fn a_faulty_local_backend_never_yields_wrong_bytes_nor_loses_a_bound_blob() {
    let seed = base_seed();
    let seeds = if cfg!(feature = "slow") { 40 } else { 3 };
    for i in 0..seeds {
        let dir = tempfile::tempdir().unwrap();
        let s = seed.wrapping_add(i);
        let t = faulty_workload(Arc::new(LocalStorage::open(dir.path()).unwrap()), s, false);
        eprintln!("local, seed {s}: {t:?}");
    }
}

#[test]
fn lost_writes_are_detected_never_read_as_other_bytes() {
    let seed = base_seed();
    let seeds = if cfg!(feature = "slow") { 400 } else { 40 };
    for i in 0..seeds {
        faulty_workload(Arc::new(MemoryStorage::new()), seed.wrapping_add(i), true);
    }
}

/// A sanity check of the harness itself: the clean prefix `blobs/` lists only layout keys after a faulty run.
#[test]
fn a_faulty_run_leaves_only_layout_keys() {
    let inner = Arc::new(MemoryStorage::new());
    faulty_workload(Arc::clone(&inner), base_seed(), false);
    for meta in inner.list(&KeyPrefix::new("blobs/").unwrap()).unwrap() {
        assert!(
            RemoteKey::parse(&meta.key).is_some(),
            "{}",
            meta.key.as_str()
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 5. Object times through the store on real files.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_retried_chunk_write_keeps_the_staged_time_on_the_local_backend() {
    // A sweep ages an upload from its newest chunk (SPEC §9.5): a caller's retry of a chunk write that had in fact
    // landed (`ResponseLost`) neither fails nor makes the upload look newer, since nothing is written again.
    let dir = tempfile::tempdir().unwrap();
    let store = local(dir.path());
    let upload = UploadId::new(5);
    store.stage_chunk(upload, 0, b"chunk").unwrap();
    let key = RemoteKey::Chunk {
        upload_id: upload,
        index: 0,
    }
    .key()
    .unwrap();
    // `blobs/uploads/<id>/<idx>` is the file `blobs@/uploads@/<id>@/<idx>` (SPEC §10.1.1).
    let parts: Vec<&str> = key.as_str().split('/').collect();
    let path = format!("{}@/{}@/{}@/{}", parts[0], parts[1], parts[2], parts[3]);
    let old = std::time::UNIX_EPOCH + Duration::from_secs(1_234_567);
    std::fs::File::options()
        .write(true)
        .open(dir.path().join(path))
        .unwrap()
        .set_modified(old)
        .unwrap();
    store.stage_chunk(upload, 0, b"chunk").unwrap();
    let staged = store.staged_chunks(upload).unwrap();
    assert_eq!(staged.len(), 1);
    assert_eq!((staged[0].size, staged[0].last_modified), (5, Some(old)));
    // Other bytes at the same index are a conflict, and leave the time alone too.
    assert!(matches!(
        store.stage_chunk(upload, 0, b"other"),
        Err(Error::StagedChunkConflict { index: 0, .. })
    ));
    assert_eq!(
        store.staged_chunks(upload).unwrap()[0].last_modified,
        Some(old)
    );
}
