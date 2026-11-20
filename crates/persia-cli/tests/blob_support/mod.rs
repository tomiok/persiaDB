//! Shared harness of the process-level blob tests (`blob_crash.rs`, `blob_processes.rs`): re-executed child
//! processes that report on stdout and take commands on stdin, deterministic blob contents, the live documents of a
//! snapshot, and the `persia` binary.
//!
//! A child is the test binary re-executed with [`ROLE_ENV`] set (as `persia-engine`'s `database_process.rs`): it runs
//! the `child_role` test, which dispatches on the role. Children never sleep and never time out: they report each
//! step with [`report`] and wait on stdin (a command line, or EOF once the parent is gone).
#![allow(dead_code)] // Each test binary uses part of the harness.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;

use persia_engine::db::Snapshot;
use persia_engine::view::SearchView;

/// Selects the child role of a re-executed test binary.
pub const ROLE_ENV: &str = "PERSIA_BLOB_CHILD_ROLE";
/// The database path a child works on.
pub const PATH_ENV: &str = "PERSIA_BLOB_CHILD_PATH";
/// The seed of a child's schedule.
pub const SEED_ENV: &str = "PERSIA_BLOB_CHILD_SEED";
/// The prefix of the ids a child writes: unique per child.
pub const PREFIX_ENV: &str = "PERSIA_BLOB_CHILD_PREFIX";
/// How many documents a bounded writer puts.
pub const COUNT_ENV: &str = "PERSIA_BLOB_CHILD_COUNT";
/// Prefix of the lines children report; anything else on their stdout is libtest's.
pub const TAG: &str = "@@persia-blob ";
/// The collection every scenario writes.
pub const COLLECTION: &str = "files";

/// Serializes the tests of one binary. A test that spawns a process forks, and the forked child holds a copy of every
/// descriptor the parent had open until it execs, `flock` included: a container another test had open at that moment
/// would look locked (as `cli.rs` does).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The base seed (`PERSIA_TEST_SEED`, default fixed), printed.
pub fn base_seed(default: u64) -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default);
    eprintln!("PERSIA_TEST_SEED={seed}");
    seed
}

/// A count from the environment variable `var` (long manual runs), else `default`; printed.
pub fn env_count(var: &str, default: u64) -> u64 {
    let n = std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default);
    eprintln!("{var}={n}");
    n
}

/// `SplitMix64`: small, seedable, and good enough for test schedules and contents.
#[derive(Debug, Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform-ish in `lo..hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo)
    }

    /// True with probability about `percent`%.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

/// FNV-1a of `s`: a stable seed from a string.
pub fn hash_str(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for b in s.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The `size` bytes of the blob of content seed `cseed`: a pure function of both, so a parent rebuilds any blob a
/// child reported.
pub fn content(size: u64, cseed: u64) -> Vec<u8> {
    let mut rng = Rng(cseed ^ size.rotate_left(17));
    let size = usize::try_from(size).unwrap();
    let mut out = Vec::with_capacity(size + 8);
    while out.len() < size {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(size);
    out
}

/// 4 MiB: the default chunk size (`PutBlobOptions::default`), so sizes around it cut chunks.
pub const CHUNK: u64 = 4 << 20;

/// A blob size skewed small with the chunk boundaries and a multi-chunk tail: 0 B, 1 B, a few bytes to 200 KiB,
/// `CHUNK - 1`, `CHUNK`, `CHUNK + 1` and about 2.2 chunks. `big_percent` is the share of the sizes at or above a
/// chunk.
pub fn pick_size(rng: &mut Rng, big_percent: u64) -> u64 {
    if rng.chance(big_percent) {
        match rng.range(0, 4) {
            0 => CHUNK - 1,
            1 => CHUNK,
            2 => CHUNK + 1,
            _ => 2 * CHUNK + rng.range(1, 1 << 20),
        }
    } else {
        match rng.range(0, 10) {
            0 => 0,
            1 => 1,
            2..=6 => rng.range(2, 4096),
            _ => rng.range(4096, 200 << 10),
        }
    }
}

/// The size and content seed of the blob a bounded writer puts with document `id`: a pure function of the id.
pub fn blob_of_id(id: &str, big_percent: u64) -> (u64, u64) {
    let mut rng = Rng(hash_str(id));
    let size = pick_size(&mut rng, big_percent);
    (size, rng.next_u64())
}

// ---------------------------------------------------------------------------------------------------------------
// Child side
// ---------------------------------------------------------------------------------------------------------------

/// Writes one tagged line to stdout and flushes it, bypassing libtest's capture.
pub fn report(msg: &str) {
    let mut out = std::io::stdout().lock();
    out.write_all(format!("{TAG}{msg}\n").as_bytes()).unwrap();
    out.flush().unwrap();
}

/// Set once stdin reaches EOF: the parent holds the child's stdin until it reaped it, so EOF means the parent is
/// gone (or is done with the child) and a free-running child stops instead of running on.
pub fn parent_gone() -> Arc<AtomicBool> {
    let gone = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&gone);
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let ended = std::io::stdin().read_to_end(&mut sink);
        flag.store(true, Ordering::Relaxed);
        ended.unwrap();
    });
    gone
}

/// Blocks until stdin reaches EOF (the parent released the child).
pub fn wait_for_eof() {
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

/// The child's environment: its path, seed, prefix and count (0 when unset).
pub struct ChildEnv {
    pub role: String,
    pub path: PathBuf,
    pub seed: u64,
    pub prefix: String,
    pub count: u64,
}

/// The child's environment, or `None` in a normal test run (not a child).
pub fn child_env() -> Option<ChildEnv> {
    let role = std::env::var(ROLE_ENV).ok()?;
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    Some(ChildEnv {
        role,
        path: PathBuf::from(var(PATH_ENV)),
        seed: var(SEED_ENV).parse().unwrap_or(0),
        prefix: var(PREFIX_ENV),
        count: var(COUNT_ENV).parse().unwrap_or(0),
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Parent side
// ---------------------------------------------------------------------------------------------------------------

/// A running child: its tagged reports arrive on a channel (a thread drains its stdout, so it never blocks on a full
/// pipe), and its stdin stays open until [`ChildIo::finish`] or [`ChildIo::kill`].
pub struct ChildIo {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
    reader: Option<std::thread::JoinHandle<()>>,
    pub name: String,
}

impl ChildIo {
    /// Spawns this test binary as a child of `role` on `path`, with `seed`, `prefix` and `count`.
    pub fn spawn(role: &str, path: &Path, seed: u64, prefix: &str, count: u64) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
            .env(ROLE_ENV, role)
            .env(PATH_ENV, path)
            .env(SEED_ENV, seed.to_string())
            .env(PREFIX_ENV, prefix)
            .env(COUNT_ENV, count.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            while let Some(msg) = next_tagged(&mut out) {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            rx,
            reader: Some(reader),
            name: format!("{role} {prefix}"),
        }
    }

    /// The child's next report, or `None` once its stdout closed (it exited or was killed).
    pub fn recv(&self) -> Option<String> {
        self.rx.recv().ok()
    }

    /// The child's next report, which must exist.
    pub fn expect(&self, ctx: &str) -> String {
        self.recv()
            .unwrap_or_else(|| panic!("{ctx}: {} exited without reporting", self.name))
    }

    /// Sends one command line to the child.
    pub fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// SIGKILLs the child and reaps it; returns the reports it made that were not received yet. Its stdin is held
    /// until it is reaped, so it never sees EOF and stops by itself first.
    pub fn kill(mut self) -> Vec<String> {
        self.child.kill().unwrap(); // SIGKILL on Unix
        let status = self.child.wait().unwrap();
        drop(self.stdin.take());
        self.reader.take().unwrap().join().unwrap();
        assert!(
            !status.success(),
            "{} exited by itself: {status}",
            self.name
        );
        self.rx.try_iter().collect()
    }

    /// Closes the child's stdin (EOF: the parent is done with it), waits for it to exit, and asserts it succeeded.
    /// Returns the reports not received yet.
    pub fn finish(mut self) -> Vec<String> {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        self.reader.take().unwrap().join().unwrap();
        assert!(status.success(), "{} failed: {status}", self.name);
        self.rx.try_iter().collect()
    }
}

impl Drop for ChildIo {
    /// A child left running by a failed assertion is killed, so no process outlives the test.
    fn drop(&mut self) {
        if self.reader.is_some() {
            let _ = self.child.kill(); // Cleanup on a failure path: the child may have exited already.
            let _ = self.child.wait();
        }
    }
}

/// Reads the child's stdout up to its next tagged line, returning it without the tag; `None` at EOF.
fn next_tagged(out: &mut BufReader<ChildStdout>) -> Option<String> {
    let mut line = String::new();
    loop {
        line.clear();
        if out.read_line(&mut line).unwrap() == 0 {
            return None;
        }
        // libtest prints `test child_role ... ` without a newline before the test runs, so the tag may follow it.
        // An unterminated line was cut by the child's death: what it reported is not acknowledged.
        if let Some(rest) = line.strip_suffix('\n')
            && let Some(msg) = rest.split_once(TAG).map(|(_, m)| m)
        {
            return Some(msg.to_owned());
        }
    }
}

/// A temp dir kept (its path printed) when the test fails, removed otherwise.
pub struct KeptOnFailure(Option<tempfile::TempDir>);

impl KeptOnFailure {
    pub fn new() -> Self {
        Self(Some(tempfile::tempdir().unwrap()))
    }

    pub fn path(&self) -> &Path {
        self.0.as_ref().unwrap().path()
    }
}

impl Drop for KeptOnFailure {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(dir) = self.0.take()
        {
            eprintln!("kept the failed test's files in {}", dir.keep().display());
        }
    }
}

/// Every live document id of `collection` in `snapshot`: the memtable's and the segments', each confirmed by a
/// lookup (which applies tombstones and replacements).
pub fn live_ids(snapshot: &Snapshot, collection: &str) -> BTreeSet<String> {
    let c = snapshot.collection(collection).unwrap();
    let memtable = c.memtable();
    let mut ids = BTreeSet::new();
    for docid in 0..memtable.doc_count() {
        if memtable.is_live(docid) {
            ids.insert(memtable.document(docid).unwrap().id().to_owned());
        }
    }
    for s in c.segments() {
        for docid in 0..s.doc_count() {
            ids.insert(s.id_of(docid).unwrap());
        }
    }
    ids.into_iter()
        .filter(|id| c.get(id).unwrap().is_some())
        .collect()
}

/// Runs the `persia` binary with `args`.
pub fn persia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_persia"))
        .args(args)
        .output()
        .unwrap()
}

/// `persia verify <args> <path>`, which must succeed: its `key value` rows.
pub fn verify_ok(args: &[&str], path: &Path, ctx: &str) -> Vec<(String, String)> {
    let mut all: Vec<&str> = vec!["verify"];
    all.extend_from_slice(args);
    all.push(path.to_str().unwrap());
    let out = persia(&all);
    assert!(
        out.status.success(),
        "{ctx}: persia {all:?} exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let (k, v) = l.split_once(' ').unwrap_or((l, ""));
            (k.to_owned(), v.trim().to_owned())
        })
        .collect()
}

/// The value of row `key` of a `persia verify` report.
pub fn row<'a>(rows: &'a [(String, String)], key: &str) -> &'a str {
    rows.iter()
        .find(|(k, _)| k == key)
        .map_or_else(|| panic!("no {key} row in {rows:?}"), |(_, v)| v.as_str())
}

/// `persia verify <args> <path>`, which must fail with the corruption status (3) and print nothing on stdout: its
/// stderr.
pub fn verify_corrupt(args: &[&str], path: &Path) -> String {
    let mut all: Vec<&str> = vec!["verify"];
    all.extend_from_slice(args);
    all.push(path.to_str().unwrap());
    let out = persia(&all);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(3), "persia {all:?}: {err}");
    assert_eq!(out.stdout, b"", "persia {all:?}");
    err
}

/// Copies `from` to `to` by reading it once from its start to its end (SPEC §4.8), not with a reflink or
/// `copy_file_range`, which could take the file at one instant and hide the order this copy must follow.
pub fn copy_by_reading(from: &Path, to: &Path) {
    let mut src = std::fs::File::open(from).unwrap();
    let mut dst = std::fs::File::create(to).unwrap();
    let mut buf = vec![0_u8; 256 << 10];
    loop {
        let n = src.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).unwrap();
    }
    dst.sync_all().unwrap();
}
