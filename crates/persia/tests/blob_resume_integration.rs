//! System tests of resumed uploads (SPEC §9.4, §9.5, §10.1; ROADMAP 9.3.4, 9.3.6, 9.5.9; ADR-0053) through
//! `put_blob`, over every `persia-storage` backend with `conditional_put`: the real local backend (files, `mtime`,
//! real processes killed with SIGKILL) and the clocked `MemoryStorage`.
//!
//! No cloud-emulator variant exists: `persia-storage` has no S3, GCS or Azure backend yet (ROADMAP 2.x / 12.2.4), so
//! there is nothing to point at `MinIO`, `fake-gcs` or `Azurite`. Every scenario below is written against
//! `S: Storage`; a cloud backend is added by one more `#[test]` that passes its store to the same function.
//!
//! Invariants:
//! 1. A `put_blob` whose reader fails after N chunks fails with `UploadInterrupted`; the upload's chunk objects
//!    (`blobs/uploads/<id>/`) are exactly chunks `0..64·⌊N/64⌋` (whole staged windows), each holding its chunk's
//!    bytes. Resumed with only the token and the whole blob, it stages only the chunks after that run, under the same
//!    upload, binds the `BlobRef` an uninterrupted `put_blob` binds, and the bound manifest reads back.
//! 2. A child process killed (SIGKILL) mid-upload: it prints its token once the first window is about to be staged,
//!    and the parent uses only that token. Killed after window k was acknowledged, the chunk objects are exactly the
//!    acknowledged run; killed at a seeded point while it runs, they are a superset of the acknowledged run, each with
//!    its chunk's bytes. The resume binds the same `BlobRef`, and the bound upload holds exactly every chunk. Killed
//!    after every chunk was staged (before the bind), the resume restages under a new upload id (ROADMAP 9.3.6).
//! 3. GC with a grace period longer than the upload's age (measured from the backend's `last_modified`) keeps an
//!    interrupted upload, and the resume keeps its run. Aged past the grace period, GC sweeps it, and the resume then
//!    restarts from chunk 0 under the same upload id and binds the same `BlobRef`.
//! 4. ROADMAP 9.3.6 end to end (clocked `MemoryStorage`): an upload with every chunk staged, older than the grace
//!    period, resumed and bound by a document, survives a later GC under its new upload id; the old upload is swept.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed; replay a failure with
//! `PERSIA_TEST_SEED=<seed> cargo test -p persia --test blob_resume_integration`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::manual_assert_eq,
    clippy::assert_is_empty
)] // Tests: inputs are fixed; `assert!(a == b)` keeps megabytes of blob bytes out of a failure message.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::ops::Range;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use persia::internal::blob::{Blobs, PutBlobOptions};
use persia::internal::documents::{Blob, BlobDatabase};
use persia::internal::gc::{DEFAULT_GRACE, GcOptions, GcReport};
use persia_blob::gc::{PinGate, Sweep, SweepOptions, SweepReport};
use persia_blob::remote::{RemoteKey, ResumeToken, ResumedUpload, keys};
use persia_blob::{
    BindOutcome, BlobManifest, BlobRef, BlobStore, ChunkSize, Error, Mark, RemoteBlobStore, Result,
    UploadId,
};
use persia_engine::db::{Database, DatabaseOptions, SegmentIds};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::Durability;
use persia_storage::retry::{Clock, ManualClock};
use persia_storage::{LocalStorage, MemoryStorage, Op, Storage};

const CHUNK: u32 = ChunkSize::MIN;
const CHUNK_LEN: usize = CHUNK as usize;
/// Chunks `put_blob` stages per window at 64 KiB (`WINDOW_BYTES / chunk size`).
const WINDOW: u32 = 64;
/// Three full windows and a short chunk: 193 chunks, about 12 MiB.
const LEN: usize = 3 * WINDOW as usize * CHUNK_LEN + 100;
const CHUNKS: u32 = 3 * WINDOW + 1;

const TAG: &str = "@@persia-resume-it ";
const ROLE_ENV: &str = "PERSIA_RESUME_IT_ROLE";
const ROOT_ENV: &str = "PERSIA_RESUME_IT_ROOT";
const SEED_ENV: &str = "PERSIA_RESUME_IT_SEED";
const BLOCK_ENV: &str = "PERSIA_RESUME_IT_BLOCK_AT";

fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_0934_0001);
    eprintln!("PERSIA_TEST_SEED={seed}");
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

    fn range(&mut self, r: Range<u32>) -> u32 {
        r.start + (self.next_u64() % u64::from(r.end - r.start)) as u32
    }
}

/// The blob's bytes for `seed`: `LEN` of them.
fn data(seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut out = Vec::with_capacity(LEN + 8);
    while out.len() < LEN {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(LEN);
    out
}

fn chunk_of(data: &[u8], index: u32) -> &[u8] {
    let start = index as usize * CHUNK_LEN;
    &data[start..(start + CHUNK_LEN).min(data.len())]
}

fn options() -> PutBlobOptions {
    PutBlobOptions {
        chunk_size: ChunkSize::new(CHUNK).unwrap(),
        ..PutBlobOptions::default()
    }
}

fn resume(token: ResumeToken) -> PutBlobOptions {
    PutBlobOptions {
        resume: Some(token),
        ..options()
    }
}

/// The reference an uninterrupted `put_blob` returns, on a store of its own.
fn uninterrupted(data: &[u8]) -> BlobRef {
    let blobs = Blobs::new(Arc::new(RemoteBlobStore::new(MemoryStorage::new())));
    blobs.put_blob(data, &options()).unwrap().0
}

fn interruption(e: Error) -> (ResumeToken, Error) {
    match e {
        Error::UploadInterrupted { token, source } => (token, *source),
        other => panic!("not an interrupted upload: {other:?}"),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// -- the store under test: the remote layout, recording chunk writes and restages, failing a bind on demand --

/// What the child process reports, and where it stops.
#[derive(Debug)]
struct ChildReport {
    /// The acknowledged chunk count after which the child blocks until it is killed.
    block_at: Option<u32>,
    token_printed: AtomicBool,
}

/// `RemoteBlobStore<S>`, forwarding every call, recording each staged chunk and each restage, and failing the next
/// bind with a backend error (a known outcome: nothing bound) when asked.
struct Probe<S: Storage> {
    inner: RemoteBlobStore<S>,
    fail_next_bind: AtomicBool,
    staged: Mutex<Vec<(UploadId, u32)>>,
    restaged: Mutex<Vec<(UploadId, UploadId, usize)>>,
    child: Option<ChildReport>,
}

impl<S: Storage> Probe<S> {
    fn new(storage: S) -> Self {
        Self {
            inner: RemoteBlobStore::new(storage),
            fail_next_bind: AtomicBool::new(false),
            staged: Mutex::new(Vec::new()),
            restaged: Mutex::new(Vec::new()),
            child: None,
        }
    }

    fn take_staged(&self) -> Vec<(UploadId, u32)> {
        std::mem::take(&mut *lock(&self.staged))
    }

    fn take_restaged(&self) -> Vec<(UploadId, UploadId, usize)> {
        std::mem::take(&mut *lock(&self.restaged))
    }

    /// The chunk indices under `blobs/uploads/<upload>/`, as listed.
    fn chunks(&self, upload: UploadId) -> Vec<u32> {
        let mut v: Vec<u32> = self
            .inner
            .staged_chunks(upload)
            .unwrap()
            .iter()
            .map(|c| c.index)
            .collect();
        v.sort_unstable();
        v
    }

    /// Checks that every chunk object of `upload` holds exactly its chunk of `data`.
    fn check_chunk_bytes(&self, upload: UploadId, data: &[u8]) {
        for index in self.chunks(upload) {
            let key = RemoteKey::Chunk {
                upload_id: upload,
                index,
            }
            .key()
            .unwrap();
            let bytes = self.inner.storage().get(&key).unwrap();
            assert!(
                *bytes == *chunk_of(data, index),
                "chunk {index} of {upload} holds other bytes"
            );
        }
    }

    /// The newest `last_modified` of `upload`'s chunk objects, by the backend's clock (SPEC §10.1).
    fn newest(&self, upload: UploadId) -> SystemTime {
        self.inner
            .storage()
            .list(&keys::upload_prefix(upload).unwrap())
            .unwrap()
            .iter()
            .map(|meta| {
                meta.last_modified
                    .expect("the backend reports last_modified")
            })
            .max()
            .expect("the upload has chunks")
    }

    fn manifest_of(&self, r: &BlobRef) -> BlobManifest {
        self.inner.manifest(r.blob_id()).unwrap().unwrap()
    }
}

fn injected_bind_failure() -> Error {
    Error::Storage(persia_storage::Error::Io {
        op: Op::PutIfAbsent,
        target: "injected".to_owned(),
        source: io::Error::other("injected bind failure"),
    })
}

fn report(line: &str) {
    let mut out = io::stdout().lock();
    writeln!(out, "{TAG}{line}").unwrap();
    out.flush().unwrap();
}

impl<S: Storage> BlobStore for Probe<S> {
    fn stage_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) -> Result<()> {
        self.stage_chunks(upload_id, index, &[bytes])
    }

    fn stage_chunks(&self, upload_id: UploadId, first_index: u32, chunks: &[&[u8]]) -> Result<()> {
        if let Some(child) = &self.child
            && !child.token_printed.swap(true, Ordering::AcqRel)
        {
            // The token is known once the first chunk write is about to be attempted. This hook stands in for a token
            // the caller recorded: a killed process never returns its `UploadInterrupted`, so its parent learns the
            // upload id and chunk size from this line, as an application would from its own record.
            report(&format!("token {} {CHUNK}", upload_id.get()));
        }
        if self.child.is_some() {
            // The child writes one chunk at a time and reports each, so the parent can kill it inside a window.
            for (index, bytes) in (first_index..).zip(chunks) {
                self.inner.stage_chunk(upload_id, index, bytes)?;
                report(&format!("wrote {index}"));
            }
        } else {
            self.inner.stage_chunks(upload_id, first_index, chunks)?;
        }
        let count = u32::try_from(chunks.len()).unwrap();
        lock(&self.staged).extend((first_index..first_index + count).map(|i| (upload_id, i)));
        if let Some(child) = &self.child {
            let acked = first_index + count;
            report(&format!("acked {acked}"));
            if child.block_at == Some(acked) {
                report("blocked");
                // Until killed; a parent that died closes the pipe, and the child exits.
                let mut line = String::new();
                let _ = io::stdin().lock().read_line(&mut line);
                std::process::exit(3);
            }
        }
        Ok(())
    }

    fn bind(&self, manifest: &BlobManifest) -> Result<BindOutcome> {
        if self.fail_next_bind.swap(false, Ordering::AcqRel) {
            return Err(injected_bind_failure());
        }
        self.inner.bind(manifest)
    }

    fn bind_small(&self, manifest: &BlobManifest, bytes: &[u8]) -> Result<BindOutcome> {
        self.inner.bind_small(manifest, bytes)
    }

    fn manifest(&self, blob_id: persia_blob::BlobId) -> Result<Option<BlobManifest>> {
        self.inner.manifest(blob_id)
    }

    fn read_chunk(&self, manifest: &BlobManifest, index: u32) -> Result<Vec<u8>> {
        self.inner.read_chunk(manifest, index)
    }

    fn read_chunks(&self, manifest: &BlobManifest, indices: Range<u32>) -> Result<Vec<Vec<u8>>> {
        self.inner.read_chunks(manifest, indices)
    }

    fn discard_upload(&self, upload_id: UploadId) -> Result<()> {
        self.inner.discard_upload(upload_id)
    }

    fn resumable(&self) -> bool {
        self.inner.resumable()
    }

    fn resume(&self, token: ResumeToken) -> Result<ResumedUpload> {
        BlobStore::resume(&self.inner, token)
    }

    fn restage(&self, from: ResumeToken, chunk_crcs: &[u32], to: UploadId) -> Result<()> {
        BlobStore::restage(&self.inner, from, chunk_crcs, to)?;
        lock(&self.restaged).push((from.upload_id, to, chunk_crcs.len()));
        Ok(())
    }
}

impl<S: Storage> Sweep for Probe<S> {
    fn sweep<P: PinGate>(
        &self,
        mark: &Mark,
        pins: &P,
        options: &SweepOptions,
    ) -> Result<SweepReport> {
        self.inner.sweep(mark, pins, options)
    }
}

fn local(root: &Path) -> LocalStorage {
    LocalStorage::open(root).unwrap()
}

fn clocked() -> (Arc<ManualClock>, MemoryStorage) {
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_hours(1_000)));
    let storage = MemoryStorage::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    (clock, storage)
}

/// A document log with collection `c`, and the blob database over `store`; upload ids from `seed`.
fn database<S: Storage>(dir: &Path, store: Probe<S>, seed: u64) -> BlobDatabase<Probe<S>> {
    let mut log = Database::create(dir.join("app.persia"), DatabaseOptions::default()).unwrap();
    log.create_collection(SchemaBuilder::new("c").build().unwrap())
        .unwrap();
    BlobDatabase::new(
        log,
        Blobs::with_upload_ids(Arc::new(store), SegmentIds::seeded(seed)),
    )
}

/// Reads `r` streamed, by a range across a window boundary, and checks its manifest is the one bound to `upload`.
fn check_bound<S: Storage>(
    db: &BlobDatabase<Probe<S>>,
    r: &BlobRef,
    data: &[u8],
    upload: UploadId,
) {
    assert_eq!(*r, uninterrupted(data));
    let store = db.blobs().store();
    let m = store.manifest_of(r);
    assert_eq!(m.upload_id(), upload);
    assert_eq!(m.chunk_count(), CHUNKS);
    assert_eq!(store.chunks(upload), (0..CHUNKS).collect::<Vec<_>>());
    store.check_chunk_bytes(upload, data);
    let mut streamed = Vec::new();
    db.get_blob(r).unwrap().read_to_end(&mut streamed).unwrap();
    assert!(streamed == data, "streamed other bytes");
    let window = u64::from(WINDOW) * u64::from(CHUNK);
    let range = (window - 5)..(2 * window + 9);
    assert!(
        db.get_blob_range(r, range.clone()).unwrap()
            == data[range.start as usize..range.end as usize],
        "a range read other bytes"
    );
}

/// A reader of `data` that fails once it has yielded `fail_at` bytes.
struct FailAfter<'a> {
    data: &'a [u8],
    pos: usize,
    fail_at: usize,
}

impl Read for FailAfter<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.fail_at {
            return Err(io::Error::other("the reader failed"));
        }
        let end = self.fail_at.min(self.data.len()).min(self.pos + out.len());
        let n = end - self.pos;
        out[..n].copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(n)
    }
}

/// Interrupts a `put_blob` of `data` by failing its reader inside chunk `n`; returns the token.
fn interrupt_reader<S: Storage>(db: &BlobDatabase<Probe<S>>, data: &[u8], n: u32) -> ResumeToken {
    let reader = FailAfter {
        data,
        pos: 0,
        fail_at: n as usize * CHUNK_LEN + 7,
    };
    let (token, source) = interruption(db.blobs().put_blob(reader, &options()).unwrap_err());
    assert!(matches!(source, Error::Read(_)), "{source:?}");
    assert_eq!(token.chunk_size.get(), CHUNK);
    token
}

// -- 1. a reader that fails after N chunks --

fn reader_failure_resumes<S: Storage>(make: &dyn Fn() -> S, seed: u64) {
    let mut rng = Rng(seed);
    // Window boundaries, inside windows, every full chunk staged but the short one, and a seeded point.
    let points = [
        WINDOW,
        WINDOW + 36,
        2 * WINDOW,
        3 * WINDOW,
        rng.range(WINDOW..CHUNKS),
    ];
    for (i, n) in points.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let db = database(dir.path(), Probe::new(make()), seed ^ i as u64);
        let data = data(seed.wrapping_add(i as u64));
        let token = interrupt_reader(&db, &data, n);
        let store = db.blobs().store();
        let run = n / WINDOW * WINDOW;
        assert_eq!(
            store.chunks(token.upload_id),
            (0..run).collect::<Vec<_>>(),
            "failed inside chunk {n}"
        );
        store.check_chunk_bytes(token.upload_id, &data);
        assert!(db.blobs().pin_set().pinned().is_empty());
        store.take_staged();

        let (r, pins) = db.put_blob(&data[..], &resume(token)).unwrap();
        let written: Vec<(UploadId, u32)> = (run..CHUNKS).map(|c| (token.upload_id, c)).collect();
        assert_eq!(
            store.take_staged(),
            written,
            "only the chunks after the run are written"
        );
        assert!(store.take_restaged().is_empty());
        check_bound(&db, &r, &data, token.upload_id);
        drop(pins);
    }
}

#[test]
fn a_reader_failure_after_n_chunks_resumes_on_the_local_backend() {
    let seed = base_seed();
    let root = tempfile::tempdir().unwrap();
    let counter = std::sync::atomic::AtomicUsize::new(0);
    reader_failure_resumes(
        &|| {
            local(
                &root
                    .path()
                    .join(counter.fetch_add(1, Ordering::Relaxed).to_string()),
            )
        },
        seed,
    );
}

#[test]
fn a_reader_failure_after_n_chunks_resumes_on_clocked_memory() {
    reader_failure_resumes(&|| clocked().1, base_seed());
}

// -- 2. a child process killed mid-upload --

/// The child: `put_blob` of the seeded blob into the local backend at `ROOT_ENV`, reporting its token and every
/// acknowledged window, blocking after `BLOCK_ENV` chunks if set. In a normal test run it does nothing.
#[test]
fn child_role() {
    if std::env::var(ROLE_ENV).as_deref() != Ok("upload") {
        return;
    }
    let root = std::env::var(ROOT_ENV).unwrap();
    let seed: u64 = std::env::var(SEED_ENV).unwrap().parse().unwrap();
    let block_at = std::env::var(BLOCK_ENV).ok().map(|n| n.parse().unwrap());
    let mut probe = Probe::new(local(Path::new(&root)));
    probe.child = Some(ChildReport {
        block_at,
        token_printed: AtomicBool::new(false),
    });
    let blobs = Blobs::with_upload_ids(Arc::new(probe), SegmentIds::seeded(seed));
    let data = data(seed);
    let (_, pins) = blobs.put_blob(&data[..], &options()).unwrap();
    drop(pins);
    report("bound");
}

/// Kills the child when dropped, so a failing assertion leaves no process behind.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What the parent learned from a killed child.
struct Killed {
    token: ResumeToken,
    acked: u32,
}

/// Runs a child; SIGKILLs it once it reports `kill_after` acknowledged chunks (blocked there if `block`, else while it
/// writes the next chunk; it stops before the bind either way),
/// or right after it prints its token when `kill_after` is 0.
fn run_child(root: &Path, seed: u64, kill_after: u32, block: bool) -> Killed {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, "upload")
        .env(ROOT_ENV, root)
        .env(SEED_ENV, seed.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    // Free-running, the child still stops before the bind, so it never finishes before the kill.
    let block_at = if block { kill_after } else { CHUNKS };
    cmd.env(BLOCK_ENV, block_at.to_string());
    let mut child = Reaped(cmd.spawn().unwrap());
    let stdout = BufReader::new(child.0.stdout.take().unwrap());
    let (mut token, mut acked) = (None, 0);
    for line in stdout.lines() {
        let line = line.unwrap();
        // The harness may print `test child_role ... ` before the first line on the same line.
        let Some((_, msg)) = line.split_once(TAG) else {
            continue;
        };
        if let Some(rest) = msg.strip_prefix("token ") {
            let (id, chunk) = rest.split_once(' ').unwrap();
            token = Some(ResumeToken {
                upload_id: UploadId::new(id.parse().unwrap()),
                chunk_size: ChunkSize::new(chunk.parse().unwrap()).unwrap(),
            });
            if kill_after == 0 {
                break;
            }
        } else if let Some(n) = msg.strip_prefix("wrote ") {
            // A chunk write `put_if_absent` acknowledged: durable, whatever its window.
            acked = acked.max(n.parse::<u32>().unwrap() + 1);
            if !block && acked >= kill_after {
                break;
            }
        } else if let Some(n) = msg.strip_prefix("acked ") {
            acked = n.parse().unwrap();
        } else if msg == "blocked" {
            assert!(!block || acked == kill_after);
            break;
        } else if msg == "bound" {
            panic!("the child bound before it was killed (kill after {kill_after})");
        }
    }
    child.0.kill().unwrap();
    let status = child.0.wait().unwrap();
    assert!(!status.success(), "{status:?}");
    Killed {
        token: token.expect("the child printed its token"),
        acked,
    }
}

fn resume_killed(root: &Path, seed: u64, killed: &Killed, data: &[u8]) -> (BlobRef, UploadId) {
    let dir = tempfile::tempdir().unwrap();
    // Upload ids of a seed the child did not use, so a restage never lands on the child's id.
    let db = database(dir.path(), Probe::new(local(root)), !seed);
    let (r, pins) = db.put_blob(data, &resume(killed.token)).unwrap();
    let upload = db.blobs().store().manifest_of(&r).upload_id();
    check_bound(&db, &r, data, upload);
    let staged = db.blobs().store().take_staged();
    assert!(staged.iter().all(|&(u, _)| u == upload));
    drop(pins);
    (r, upload)
}

#[test]
fn a_child_killed_between_windows_resumes_from_its_token() {
    let seed = base_seed();
    // After window 1, window 2, and after every chunk (the short last one included) but before the bind.
    for (i, kill_after) in [WINDOW, 2 * WINDOW, CHUNKS].into_iter().enumerate() {
        let root = tempfile::tempdir().unwrap();
        let child_seed = seed.wrapping_add(100 + i as u64);
        let killed = run_child(root.path(), child_seed, kill_after, true);
        assert_eq!(killed.acked, kill_after);
        let data = data(child_seed);
        let probe = Probe::new(local(root.path()));
        assert_eq!(
            probe.chunks(killed.token.upload_id),
            (0..kill_after).collect::<Vec<_>>(),
            "the chunk objects are the acknowledged run"
        );
        probe.check_chunk_bytes(killed.token.upload_id, &data);
        assert!(
            probe
                .inner
                .manifest(uninterrupted(&data).blob_id())
                .unwrap()
                .is_none()
        );

        let (_, upload) = resume_killed(root.path(), child_seed, &killed, &data);
        if kill_after == CHUNKS {
            // ROADMAP 9.3.6: nothing new to write, so the chunks moved to a fresh upload; the old one is GC's.
            assert_ne!(upload, killed.token.upload_id);
            assert_eq!(probe.chunks(killed.token.upload_id).len(), CHUNKS as usize);
        } else {
            assert_eq!(upload, killed.token.upload_id);
        }
    }
}

#[test]
fn a_child_killed_while_it_stages_resumes_from_its_token() {
    let seed = base_seed();
    let mut rng = Rng(seed ^ 0x6b11);
    // Right after the token (before or during the first chunk write), and after seeded chunk writes inside windows 0
    // and 1 (the kill lands during the next chunk write, or between two).
    let points = [0, rng.range(1..WINDOW), rng.range(WINDOW + 1..2 * WINDOW)];
    for (i, kill_after) in points.into_iter().enumerate() {
        let root = tempfile::tempdir().unwrap();
        let child_seed = seed.wrapping_add(200 + i as u64);
        let killed = run_child(root.path(), child_seed, kill_after, false);
        let data = data(child_seed);
        let probe = Probe::new(local(root.path()));
        let present = probe.chunks(killed.token.upload_id);
        eprintln!(
            "kill after {kill_after}: {} acknowledged, {} chunk objects",
            killed.acked,
            present.len()
        );
        assert!(
            (0..killed.acked).all(|c| present.binary_search(&c).is_ok()),
            "an acknowledged chunk is missing"
        );
        assert!(present.iter().all(|&c| c < CHUNKS));
        // A killed write leaves no torn chunk a listing shows.
        probe.check_chunk_bytes(killed.token.upload_id, &data);

        resume_killed(root.path(), child_seed, &killed, &data);
    }
}

// -- 3. GC within, then past, the grace period --

fn gc_at(db: &BlobDatabase<Probe<impl Storage>>, now: SystemTime) -> GcReport {
    db.gc(&GcOptions {
        now: Some(now),
        ..GcOptions::default()
    })
    .unwrap()
}

fn gc_then_resume<S: Storage>(storage: S, seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path(), Probe::new(storage), seed);
    let store = db.blobs().store();
    let hour = Duration::from_hours(1);

    // Within the grace period: the upload survives, and the resume keeps its run.
    let a = data(seed);
    let token_a = interrupt_reader(&db, &a, 2 * WINDOW + 3);
    let token = token_a;
    let report = gc_at(&db, store.newest(token.upload_id) + DEFAULT_GRACE - hour);
    assert_eq!(report.sweep.uploads_deleted, 0, "{report:?}");
    assert_eq!(report.sweep.young, 1, "{report:?}");
    assert_eq!(
        store.chunks(token.upload_id),
        (0..2 * WINDOW).collect::<Vec<_>>()
    );
    store.take_staged();
    let (ra, pins_a) = db.put_blob(&a[..], &resume(token)).unwrap();
    let written: Vec<(UploadId, u32)> =
        (2 * WINDOW..CHUNKS).map(|c| (token.upload_id, c)).collect();
    assert_eq!(store.take_staged(), written);
    check_bound(&db, &ra, &a, token.upload_id);

    // Past the grace period: GC sweeps it, and the resume restarts from chunk 0 under the same upload id.
    let b = data(seed.wrapping_add(1));
    let token = interrupt_reader(&db, &b, WINDOW + 1);
    assert_eq!(
        store.chunks(token.upload_id),
        (0..WINDOW).collect::<Vec<_>>()
    );
    let report = gc_at(&db, store.newest(token.upload_id) + DEFAULT_GRACE + hour);
    // Blob A is pinned by its `put_blob` guard, so only B's upload is garbage.
    assert_eq!(report.sweep.uploads_deleted, 1, "{report:?}");
    assert_eq!(report.sweep.chunks_deleted, u64::from(WINDOW), "{report:?}");
    assert_eq!(report.sweep.blobs_deleted, 0, "{report:?}");
    assert!(store.chunks(token.upload_id).is_empty());
    check_bound(&db, &ra, &a, token_a.upload_id);
    store.take_staged();
    let (rb, pins_b) = db.put_blob(&b[..], &resume(token)).unwrap();
    let written: Vec<(UploadId, u32)> = (0..CHUNKS).map(|c| (token.upload_id, c)).collect();
    assert_eq!(store.take_staged(), written, "restarted from chunk 0");
    assert!(store.take_restaged().is_empty());
    check_bound(&db, &rb, &b, token.upload_id);
    drop((pins_a, pins_b));
}

#[test]
fn gc_keeps_a_young_interrupted_upload_and_sweeps_an_expired_one_on_the_local_backend() {
    let root = tempfile::tempdir().unwrap();
    gc_then_resume(local(root.path()), base_seed());
}

#[test]
fn gc_keeps_a_young_interrupted_upload_and_sweeps_an_expired_one_on_clocked_memory() {
    gc_then_resume(clocked().1, base_seed());
}

// -- 4. ROADMAP 9.3.6 end to end --

#[test]
fn an_expired_fully_staged_upload_resumed_by_a_document_survives_a_later_gc() {
    let seed = base_seed();
    let (clock, storage) = clocked();
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(dir.path(), Probe::new(storage), seed);
    let data = data(seed);
    let grace_and_hour = DEFAULT_GRACE + Duration::from_hours(1);

    // Every chunk staged, the bind failed with a known outcome: an interrupted upload with nothing left to write.
    db.blobs()
        .store()
        .fail_next_bind
        .store(true, Ordering::Release);
    let (token, source) = interruption(db.blobs().put_blob(&data[..], &options()).unwrap_err());
    assert!(matches!(source, Error::Storage(_)), "{source:?}");
    let store = Arc::clone(db.blobs().store());
    assert_eq!(
        store.chunks(token.upload_id),
        (0..CHUNKS).collect::<Vec<_>>()
    );
    // Older than the grace period when it is resumed.
    clock.advance(grace_and_hour);
    store.take_staged();

    let r = db
        .put(
            "c",
            &Document::new("d"),
            Blob::from_reader(&data[..]).with_options(resume(token)),
            Durability::Fsync,
        )
        .unwrap();
    db.release_durable();
    let moved = store.manifest_of(&r).upload_id();
    assert_ne!(moved, token.upload_id, "restaged under a fresh upload id");
    assert_eq!(
        store.take_restaged(),
        [(token.upload_id, moved, CHUNKS as usize - 1)]
    );
    assert_eq!(store.take_staged(), [(moved, CHUNKS - 1)]);

    // A GC now: the old upload is expired, the new one is named by the bound manifest.
    let report = gc_at(&db, clock.now());
    assert_eq!(report.sweep.uploads_deleted, 1, "{report:?}");
    assert_eq!(report.sweep.chunks_deleted, u64::from(CHUNKS), "{report:?}");
    assert!(store.chunks(token.upload_id).is_empty());
    check_bound(&db, &r, &data, moved);

    // And past the grace period again: the document keeps the blob and its chunks.
    clock.advance(grace_and_hour);
    let report = gc_at(&db, clock.now());
    assert_eq!(report.sweep.uploads_deleted, 0, "{report:?}");
    assert_eq!(report.sweep.blobs_deleted, 0, "{report:?}");
    assert!(report.missing.is_empty(), "{report:?}");
    check_bound(&db, &r, &data, moved);
}
