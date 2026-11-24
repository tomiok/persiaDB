//! Real-process tests of the public `Db` (ROADMAP 10.1.9, 10.2.3, 10.2.6; SPEC §8.1, §8.2, §8.2.1, §8.4; ADR-0063,
//! ADR-0064): hard kills between a write and the close, kills inside a maintenance round, a second process beside the
//! writer, a clean shutdown in a process of its own, and creates racing across processes.
//!
//! Children are this test binary re-executed with [`ROLE_ENV`] set: [`child_role`] then plays the role the variable
//! names, and in a normal run it does nothing (as in `persia-engine`'s `database_process.rs`). A child reports on
//! stdout with tagged, newline-terminated lines (libtest's own output is skipped) and takes commands on stdin; the
//! parent synchronizes on those lines, never on a sleep. A child is hard-killed with [`Child::kill`]: SIGKILL on Unix,
//! `TerminateProcess` on Windows. Nothing here is `cfg(unix)`: only the thread census of a child is Linux-only
//! (`/proc/self/task`), and elsewhere that one check reports `-`.
//!
//! Scenarios:
//! 1. [`kill_loop_keeps_every_acknowledged_write`]: a writer child runs a seeded mix of puts of fresh ids, upserts,
//!    deletes, deletes of deleted ids, puts that bring a deleted id back and `put_batch`es, at `Fsync`, `Async` and
//!    `Buffered`, with commits, and reports each operation before it starts it and once it returned. A write is
//!    acknowledged when an `Fsync` or `Async` call returned (SIGKILL keeps what is in the file, SPEC §8.2.1), or a
//!    later commit did. The parent kills it after a seeded number of returns, and the next child goes on on the same
//!    file. After every kill:
//!    - a read-only open, `verify` and the blob checks of `persia verify` pass, before any read-write open;
//!    - `Db::open` succeeds and shows the same documents as the read-only open;
//!    - every document is whole (its body is the one its version token names) and was put by the workload: no phantom;
//!    - the documents are the state after a *prefix* of the logged operations that covers every acknowledged one (the
//!      WAL is one ordered log, SPEC §8.2.1, and a crash may keep a prefix of a batch): nothing acknowledged is lost,
//!      no delete is undone, and nothing unacknowledged is half-applied or applied out of order;
//!    - after `maintain_now` and `close`, no temporary file is left beside the database.
//! 2. [`kills_inside_maintenance_rounds`]: the same with the maintenance thread held (only requested rounds run),
//!    `idle_flush` 0 so every round flushes and commits, a memtable of a few KiB, merges on the maintenance thread or
//!    on a worker, and puts with blobs. The child asks for a round (`maintain_now`) every few operations and the
//!    parent kills it the moment it reports the request: mid-flush, mid-merge, or mid-merge-commit.
//! 3. [`a_put_then_a_hard_kill_before_close`]: one fixed sequence, killed while the child waits between its writes and
//!    its close.
//! 4. [`a_second_process_beside_the_writer`] and [`a_second_process_is_refused_as_a_writer`]: a read-write open while
//!    another process holds the writer is `Conflict` (ADR-0063's code for the container's `Locked`), a read-only open
//!    beside it succeeds and sees new writes after `refresh`; the lock goes with a killed holder.
//! 5. [`a_child_that_closes_exits_cleanly`] and [`a_child_that_drops_makes_the_final_commit`]: `close` returns `Ok`,
//!    no `persia-` thread is left, the process exits 0; a `Drop` without `close` makes the final commit too (owner
//!    decision d), so `Buffered` writes that were never synced are in a segment after the process exited.
//! 6. [`racing_creates_across_processes`]: processes racing `Db::open` on one missing path (missing parents too) end
//!    with one database, created once: while the first holds it every other open is `Conflict`; when each closes at
//!    once, every open succeeds or is `Conflict`, and all saw the same database.
//!
//! Seeds: [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) is printed, with each file's seed; replay a failure with
//! `PERSIA_TEST_SEED=<seed> cargo test -p persia --test db_process`. A seed fixes the schedule (operations,
//! durabilities, options, kill points); the instant a kill lands after its trigger depends on timing. On a failure the
//! work directory is kept and printed, with `after-kill/`, the files as the last kill left them. The `slow` feature
//! runs more files and kills (nightly).
//!
//! The children run this test binary from its path: rebuilding it while the test runs makes later children run other
//! code than the parent.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::too_many_lines
)] // Tests: inputs are generated or fixed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use persia::internal::documents::ReadOnlyBlobDatabase;
use persia::internal::verify::Scan;
use persia::{Blob, Db, Document, Durability, ErrorCode, OpenOptions};
use persia_engine::db::{DatabaseOptions, Snapshot};
use persia_engine::view::SearchView as _;

/// Selects the child role of a re-executed test binary.
const ROLE_ENV: &str = "PERSIA_DB_PROCESS_ROLE";
/// The database path a child works on.
const PATH_ENV: &str = "PERSIA_DB_PROCESS_PATH";
/// The child's configuration, `key=value` pairs joined by `;`.
const CONFIG_ENV: &str = "PERSIA_DB_PROCESS_CONFIG";
/// Prefix of the lines children report; anything else on their stdout is libtest's.
const TAG: &str = "@@persia ";

const COLLECTION: &str = "docs";
const SCHEMA: &str =
    r#"{"name": "docs", "fields": {"body": {"type": "text"}, "v": {"type": "keyword"}}}"#;

/// The most operations a writer child runs before it reports `idle`: far above every kill point.
const MAX_OPS: u64 = 5_000;

#[cfg(not(feature = "slow"))]
const CRASH_FILES: u64 = 3;
#[cfg(feature = "slow")]
const CRASH_FILES: u64 = 16;
#[cfg(not(feature = "slow"))]
const CRASH_KILLS: u64 = 8;
#[cfg(feature = "slow")]
const CRASH_KILLS: u64 = 30;
#[cfg(not(feature = "slow"))]
const MAINTENANCE_FILES: u64 = 2;
#[cfg(feature = "slow")]
const MAINTENANCE_FILES: u64 = 10;
#[cfg(not(feature = "slow"))]
const MAINTENANCE_KILLS: u64 = 6;
#[cfg(feature = "slow")]
const MAINTENANCE_KILLS: u64 = 30;
#[cfg(not(feature = "slow"))]
const RACE_ROUNDS: u64 = 4;
#[cfg(feature = "slow")]
const RACE_ROUNDS: u64 = 24;

fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_1019_0206);
    eprintln!("PERSIA_TEST_SEED={seed}");
    seed
}

/// `SplitMix64`: small, seedable, and good enough for test schedules.
#[derive(Debug)]
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

    /// A number in `lo..=hi`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    fn pick_in<'a>(&mut self, set: &'a BTreeSet<String>) -> &'a String {
        set.iter()
            .nth(self.below(set.len() as u64) as usize)
            .unwrap()
    }
}

/// FNV-1a: a stable hash of a string or bytes, for derived data and header fingerprints.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Documents and blobs, derived from a version token
// ---------------------------------------------------------------------------------------------------------------

const WORDS: &[&str] = &[
    "persia",
    "search",
    "segment",
    "flush",
    "merge",
    "commit",
    "replay",
    "durable",
    "kill",
    "reopen",
    "árbol",
    "ação",
    "niño",
    "日本語",
    "検索",
    "🦀",
    "lorem",
    "ipsum",
    "zipf",
    "tail",
];

/// The body of the version `token`: its words follow from the token, so a body that does not is torn.
fn body_of(token: &str) -> String {
    let mut rng = Rng(fnv(token.as_bytes()));
    let words = rng.range(2, 160);
    let mut body = String::from(token);
    for _ in 0..words {
        body.push(' ');
        body.push_str(rng.pick(WORDS));
    }
    body
}

fn doc_json(id: &str, token: &str) -> String {
    serde_json::json!({"id": id, "body": body_of(token), "v": token}).to_string()
}

/// The blob sizes a blob put picks from: empty, one byte, small, and around the 64 KiB staging chunk.
const BLOB_SIZES: &[u64] = &[0, 1, 300, 4096, 65_535, 65_536, 65_537, 140_000];

fn blob_len_of(token: &str) -> u64 {
    BLOB_SIZES[(fnv(token.as_bytes()) % BLOB_SIZES.len() as u64) as usize]
}

fn blob_bytes(token: &str, len: u64) -> Vec<u8> {
    let seed = fnv(token.as_bytes()) as u8;
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A live document as the tests see it: its version token, and its blob's size if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Val {
    token: String,
    blob: Option<u64>,
}

/// The live documents, by id.
type State = BTreeMap<String, Val>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Effect {
    Put(String, Val),
    Delete(String),
}

impl Effect {
    fn id(&self) -> &str {
        match self {
            Self::Put(id, _) | Self::Delete(id) => id,
        }
    }
}

fn apply(state: &mut State, effect: &Effect) {
    match effect {
        Effect::Put(id, val) => {
            state.insert(id.clone(), val.clone());
        }
        Effect::Delete(id) => {
            state.remove(id);
        }
    }
}

fn durability(code: char) -> Durability {
    match code {
        'B' => Durability::Buffered,
        'A' => Durability::Async,
        'F' => Durability::Fsync,
        other => panic!("unknown durability {other}"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Reports, configuration and child processes
// ---------------------------------------------------------------------------------------------------------------

/// Writes one tagged line to stdout and flushes it, bypassing libtest's capture.
fn report(msg: &str) {
    let mut out = std::io::stdout().lock();
    out.write_all(format!("{TAG}{msg}\n").as_bytes()).unwrap();
    out.flush().unwrap();
}

/// A child's configuration: `key=value` pairs.
#[derive(Debug, Default, Clone)]
struct Config(BTreeMap<String, String>);

impl Config {
    fn with(mut self, key: &str, value: &dyn std::fmt::Display) -> Self {
        self.0.insert(key.to_owned(), value.to_string());
        self
    }

    fn get(&self, key: &str) -> &str {
        self.0
            .get(key)
            .unwrap_or_else(|| panic!("no {key} in {self:?}"))
    }

    fn num(&self, key: &str) -> u64 {
        self.get(key).parse().unwrap()
    }

    fn flag(&self, key: &str) -> bool {
        self.get(key) == "1"
    }

    fn encode(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(";")
    }

    fn decode(s: &str) -> Self {
        Self(
            s.split(';')
                .filter(|kv| !kv.is_empty())
                .map(|kv| {
                    let (k, v) = kv.split_once('=').unwrap();
                    (k.to_owned(), v.to_owned())
                })
                .collect(),
        )
    }
}

/// A running child. Dropped alive (a failing parent), it is killed and reaped, so no test leaves a process behind.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    out: BufReader<ChildStdout>,
    ctx: String,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // Cleanup of a child a failing test left running: its outcome no longer matters.
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }
}

impl Proc {
    fn spawn(role: &str, path: &Path, config: &Config, ctx: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
            .env(ROLE_ENV, role)
            .env(PATH_ENV, path)
            .env(CONFIG_ENV, config.encode())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Self {
            stdin: child.stdin.take(),
            child,
            out,
            ctx: format!("{ctx} [{role} child]"),
        }
    }

    /// The child's next tagged line, without the tag; `None` at EOF.
    fn next(&mut self) -> Option<String> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.out.read_line(&mut line).unwrap() == 0 {
                return None;
            }
            // libtest prints `test child_role ... ` without a newline before the test runs, so the tag may follow it.
            // An unterminated line was cut by the child's death: what it reported did not happen.
            if let Some(rest) = line.strip_suffix('\n')
                && let Some((_, msg)) = rest.split_once(TAG)
            {
                return Some(msg.trim_end_matches('\r').to_owned());
            }
        }
    }

    /// The next tagged line, which must come.
    fn expect(&mut self) -> String {
        if let Some(msg) = self.next() {
            return msg;
        }
        let status = self.child.wait().unwrap();
        panic!("{}: exited before its next report: {status}", self.ctx)
    }

    /// The next tagged line, which must start with `prefix`; the rest of it.
    fn expect_prefix(&mut self, prefix: &str) -> String {
        let msg = self.expect();
        msg.strip_prefix(prefix)
            .unwrap_or_else(|| panic!("{}: expected {prefix:?}, got {msg:?}", self.ctx))
            .trim_start()
            .to_owned()
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Hard-kills the child (SIGKILL, `TerminateProcess`); the lines it had reported meanwhile, and its status.
    fn kill(mut self) -> (Vec<String>, ExitStatus) {
        // Held until the child is reaped: a killed child may run long enough to see EOF and stop by itself.
        let stdin = self.stdin.take();
        self.child.kill().unwrap();
        let status = self.child.wait().unwrap();
        drop(stdin);
        let mut rest = Vec::new();
        while let Some(msg) = self.next() {
            rest.push(msg);
        }
        (rest, status)
    }

    /// Closes the child's stdin and waits for it to exit by itself; the lines it reported, and its status.
    fn finish(mut self) -> (Vec<String>, ExitStatus) {
        drop(self.stdin.take());
        let mut rest = Vec::new();
        while let Some(msg) = self.next() {
            rest.push(msg);
        }
        (rest, self.child.wait().unwrap())
    }
}

/// A temporary work directory, kept (and printed) when the test fails.
struct Workdir(Option<tempfile::TempDir>);

impl Workdir {
    fn new() -> Self {
        Self(Some(tempfile::tempdir().unwrap()))
    }

    fn path(&self) -> &Path {
        self.0.as_ref().unwrap().path()
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(dir) = self.0.take()
        {
            eprintln!("kept the work directory {}", dir.keep().display());
        }
    }
}

/// The child's stdin reached EOF: the parent holds it until it reaped the child, so EOF means the parent is gone.
fn stdin_eof() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("stdin-watch".to_owned())
        .spawn(move || {
            let mut sink = Vec::new();
            let ended = std::io::stdin().read_to_end(&mut sink);
            // The writer may have returned already: then nobody listens.
            tx.send(()).unwrap_or_default();
            ended.unwrap();
        })
        .unwrap();
    rx
}

/// The commands the parent sends on stdin, one per line, until EOF.
fn commands() -> impl Iterator<Item = String> {
    std::io::stdin().lines().map(Result::unwrap)
}

/// The threads Persia started in this process that are still alive (its maintenance thread and the merge workers),
/// as `<count> <name:state,...>`.
///
/// A joined thread can still be listed for a moment: glibc's `pthread_join` returns when the kernel clears the
/// thread's tid, early in its exit path, before the task leaves `/proc` (seen as `persia-maintena:R` right after a
/// `close` on a loaded machine). So a task that released its memory map (no `VmSize` line) or is a zombie counts as
/// gone, and a census that still finds one is taken again after a yield, a bounded number of times: a thread that was
/// left behind is parked or working, and stays listed.
#[cfg(target_os = "linux")]
fn persia_threads() -> String {
    let census = || -> Vec<String> {
        let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
            return vec!["unreadable".to_owned()];
        };
        tasks
            .filter_map(|task| {
                let task = task.ok()?.path();
                let name = std::fs::read_to_string(task.join("comm")).ok()?;
                let status = std::fs::read_to_string(task.join("status")).ok()?;
                let state = status
                    .lines()
                    .find_map(|l| l.strip_prefix("State:"))?
                    .trim()
                    .chars()
                    .next()?;
                let exiting = !status.contains("VmSize:") || matches!(state, 'Z' | 'X');
                (name.starts_with("persia-") && !exiting)
                    .then(|| format!("{}:{state}", name.trim_end()))
            })
            .collect()
    };
    let mut running = census();
    for _ in 0..10_000 {
        if running.is_empty() {
            break;
        }
        std::thread::yield_now();
        running = census();
    }
    format!("{} {}", running.len(), running.join(","))
}

/// No census off Linux: the parent then checks only that the process exits.
#[cfg(not(target_os = "linux"))]
fn persia_threads() -> String {
    "-".to_owned()
}

/// A fingerprint of the container's header page, which holds its `db_uuid`: equal ones mean the same database.
fn header_of(path: &Path) -> String {
    let mut header = vec![0; 4096];
    std::fs::File::open(path)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    format!("{:016x}", fnv(&header))
}

// ---------------------------------------------------------------------------------------------------------------
// Child role
// ---------------------------------------------------------------------------------------------------------------

/// The entry point of every child process; a no-op in a normal test run.
#[test]
fn child_role() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let path = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    let config = Config::decode(&std::env::var(CONFIG_ENV).unwrap());
    match role.as_str() {
        "writer" => child_writer(&path, &config),
        "put-and-wait" => child_put_and_wait(&path),
        "holder" => child_holder(&path),
        "probe" => child_probe(&path, &config),
        "shutdown" => child_shutdown(&path, &config),
        "racer" => child_racer(&path, &config),
        other => panic!("unknown child role {other}"),
    }
}

/// The ids a writer child may pick for upserts, deletes and comebacks: the parent's model, one `live <id>` or
/// `deleted <id>` per line.
fn read_known(path: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut live, mut deleted) = (BTreeSet::new(), BTreeSet::new());
    for line in std::fs::read_to_string(path).unwrap().lines() {
        match line.split_once(' ') {
            Some(("live", id)) => live.insert(id.to_owned()),
            Some(("deleted", id)) => deleted.insert(id.to_owned()),
            _ => panic!("bad known line {line:?}"),
        };
    }
    (live, deleted)
}

fn writer_options(config: &Config) -> OpenOptions {
    let options = OpenOptions::new()
        .memtable_bytes(usize::try_from(config.num("memtable")).unwrap())
        .merge_threads(usize::try_from(config.num("merge")).unwrap());
    if config.flag("hold") {
        // Only the rounds the child asks for run, and each one flushes and commits what is unflushed.
        options.hold_maintenance(true).idle_flush(Duration::ZERO)
    } else {
        options
    }
}

/// A writer: the seeded workload of scenarios 1 and 2, each operation reported before it starts (`op <n> ...`) and
/// once it returned (`ret <n>`), until killed (or `idle` after [`MAX_OPS`]).
fn child_writer(path: &Path, config: &Config) {
    let gone = stdin_eof();
    let db = writer_options(config).open(path).unwrap();
    report(&format!("opened {}", db.committed_seq().unwrap()));
    let docs = db.collection(COLLECTION).unwrap();
    let schema = docs.schema().unwrap();
    let doc = |id: &str, token: &str| Document::from_json(&doc_json(id, token), &schema).unwrap();
    let (mut live, mut deleted) = read_known(Path::new(config.get("known")));
    let mut rng = Rng(config.num("seed"));
    let prefix = config.get("prefix").to_owned();
    let durabilities: Vec<char> = config.get("durs").chars().collect();
    let (hold, blobs) = (config.flag("hold"), config.flag("blobs"));
    let mut fresh = 0u64;
    let mut fresh_id = || {
        fresh += 1;
        format!("{prefix}-{fresh}")
    };
    for n in 0..MAX_OPS {
        if gone.try_recv() != Err(mpsc::TryRecvError::Empty) {
            return; // the parent is gone
        }
        let token = format!("{prefix}o{n}");
        let dur = *rng.pick(&durabilities);
        let c = docs.clone().with_durability(durability(dur));
        let r = rng.below(100);
        if hold && r < 14 {
            report(&format!("op {n} maint"));
            db.maintain_now().unwrap();
            report(&format!("ret {n} {}", db.committed_seq().unwrap()));
        } else if r < 20 {
            report(&format!("op {n} commit"));
            db.commit().unwrap();
            report(&format!("ret {n}"));
        } else if r < 32 {
            let mut ids = BTreeSet::new();
            for _ in 0..rng.range(2, 6) {
                ids.insert(if !live.is_empty() && rng.chance(40) {
                    rng.pick_in(&live).clone()
                } else {
                    fresh_id()
                });
            }
            let batch: Vec<(String, String)> = ids
                .into_iter()
                .enumerate()
                .map(|(j, id)| (id, format!("{token}b{j}")))
                .collect();
            let listed: Vec<String> = batch.iter().map(|(id, t)| format!("{id}={t}")).collect();
            report(&format!("op {n} batch {dur} {}", listed.join(",")));
            let docs: Vec<Document> = batch.iter().map(|(id, t)| doc(id, t)).collect();
            c.put_batch(&docs).unwrap();
            for (id, _) in batch {
                deleted.remove(&id);
                live.insert(id);
            }
            report(&format!("ret {n}"));
        } else if r < 42 && !live.is_empty() {
            let id = rng.pick_in(&live).clone();
            report(&format!("op {n} del {dur} {id}"));
            c.delete(&id).unwrap();
            live.remove(&id);
            deleted.insert(id);
            report(&format!("ret {n}"));
        } else if r < 45 {
            // A delete of an id that is deleted already, or was never written: it changes nothing.
            let id = if deleted.is_empty() {
                format!("{prefix}-never-{n}")
            } else {
                rng.pick_in(&deleted).clone()
            };
            report(&format!("op {n} del {dur} {id}"));
            c.delete(&id).unwrap();
            report(&format!("ret {n}"));
        } else {
            // A put: of a deleted id (it comes back), of a live one (an upsert), or of a fresh one; with a blob or not.
            let id = if r < 50 && !deleted.is_empty() {
                rng.pick_in(&deleted).clone()
            } else if r < 65 && !live.is_empty() {
                rng.pick_in(&live).clone()
            } else {
                fresh_id()
            };
            if blobs && rng.chance(30) {
                let len = blob_len_of(&token);
                report(&format!("op {n} put {dur} {id} {token} {len}"));
                let bytes = blob_bytes(&token, len);
                c.put_with_blob(&doc(&id, &token), Blob::from_reader(&bytes[..]))
                    .unwrap();
            } else {
                report(&format!("op {n} put {dur} {id} {token} -"));
                c.put(&doc(&id, &token)).unwrap();
            }
            deleted.remove(&id);
            live.insert(id);
            report(&format!("ret {n}"));
        }
    }
    report("idle");
    gone.recv().unwrap_or_default();
}

/// Scenario 3: a fixed sequence of writes, then a wait for the kill, the database still open.
fn child_put_and_wait(path: &Path) {
    let db = Db::open(path).unwrap();
    let docs = db.create_collection_json(SCHEMA).unwrap();
    let at = |d| docs.clone().with_durability(d);
    for i in 0..5 {
        at(Durability::Fsync)
            .put_json(&doc_json(&format!("f{i}"), &format!("f{i}")))
            .unwrap();
    }
    db.commit().unwrap(); // f0..f4 in a segment: the deletes below are tombstones over it
    at(Durability::Fsync).delete("f0").unwrap();
    for i in 0..5 {
        at(Durability::Async)
            .put_json(&doc_json(&format!("a{i}"), &format!("a{i}")))
            .unwrap();
    }
    at(Durability::Async).delete("f1").unwrap();
    for i in 0..5 {
        at(Durability::Buffered)
            .put_json(&doc_json(&format!("b{i}"), &format!("b{i}")))
            .unwrap();
    }
    report("put");
    // Until the kill: the parent holds stdin until it reaped this process.
    for _ in commands() {}
    panic!("not killed");
}

/// Scenario 4: holds the writer and writes what the parent asks for.
fn child_holder(path: &Path) {
    let db = Db::open(path).unwrap();
    let docs = db.create_collection_json(SCHEMA).unwrap();
    for i in 0..10 {
        docs.put_json(&doc_json(&format!("d{i}"), &format!("d{i}")))
            .unwrap();
    }
    db.commit().unwrap();
    report("ready");
    for command in commands() {
        let (verb, id) = command.split_once(' ').unwrap_or((&command, ""));
        match verb {
            "fsync" | "async" | "buffered" => {
                let d = match verb {
                    "fsync" => Durability::Fsync,
                    "async" => Durability::Async,
                    _ => Durability::Buffered,
                };
                docs.clone()
                    .with_durability(d)
                    .put_json(&doc_json(id, id))
                    .unwrap();
            }
            "delete" => docs.delete(id).unwrap(),
            "commit" => db.commit().unwrap(),
            other => panic!("unknown command {other}"),
        }
        report("ok");
    }
    panic!("not killed");
}

/// Scenario 4, the other way round: a second process beside the parent's writer. It reports the codes of two
/// read-write opens, then which of the configured ids a read-only open shows, again after each `refresh` asked.
fn child_probe(path: &Path, config: &Config) {
    let ids: Vec<&str> = config.get("ids").split(',').collect();
    for options in [
        OpenOptions::new(),
        OpenOptions::new().create_if_missing(false),
    ] {
        let err = options.open(path).unwrap_err();
        assert!(err.message().contains("app.persia"), "{err}");
        report(&format!("rw {}", err.code().as_str()));
    }
    let reader = OpenOptions::new().read_only(true).open(path).unwrap();
    let visible = |reader: &Db| {
        let docs = reader.collection(COLLECTION).unwrap();
        let seen: Vec<&str> = ids
            .iter()
            .copied()
            .filter(|id| docs.get(id).unwrap().is_some())
            .collect();
        format!("ro {}", seen.join(","))
    };
    report(&visible(&reader));
    for command in commands() {
        assert_eq!(command, "refresh");
        reader.refresh().unwrap();
        report(&visible(&reader));
    }
    reader.close().unwrap();
    report("closed");
}

/// Scenario 5: open, `Buffered` writes that nothing synced, then `close` or a drop; the Persia threads left after.
fn child_shutdown(path: &Path, config: &Config) {
    let db = Db::open(path).unwrap();
    let docs = db
        .create_collection_json(SCHEMA)
        .unwrap()
        .with_durability(Durability::Buffered);
    report(&format!("created {}", db.committed_seq().unwrap()));
    for i in 0..config.num("docs") {
        docs.put_json(&doc_json(&format!("s{i}"), &format!("s{i}")))
            .unwrap();
    }
    match config.get("end") {
        "close" => {
            db.close().unwrap();
            report("closed");
        }
        "drop" => {
            drop(db);
            report("dropped");
        }
        other => panic!("unknown end {other}"),
    }
    report(&format!("threads {}", persia_threads()));
}

/// Scenario 6: one of the racing opens. `ready`, then waits for `go`, opens, reports `ok <header>` or
/// `err <code>`; an open that succeeded writes its own document and closes, at once or on `close`.
fn child_racer(path: &Path, config: &Config) {
    let name = config.get("name");
    report("ready");
    let mut commands = commands();
    assert_eq!(commands.next().as_deref(), Some("go"));
    let db = match Db::open(path) {
        Ok(db) => db,
        Err(err) => {
            report(&format!("err {}", err.code().as_str()));
            return;
        }
    };
    let docs = match db.collection(COLLECTION) {
        Ok(docs) => docs,
        Err(err) if err.code() == ErrorCode::NotFound => db.create_collection_json(SCHEMA).unwrap(),
        Err(err) => panic!("{name}: {err}"),
    };
    docs.put_json(&doc_json(name, name)).unwrap();
    report(&format!("ok {}", header_of(path)));
    if config.flag("hold") {
        assert_eq!(commands.next().as_deref(), Some("close"));
    }
    db.close().unwrap();
    report("closed");
}

// ---------------------------------------------------------------------------------------------------------------
// The parent's model of a writer child
// ---------------------------------------------------------------------------------------------------------------

/// One operation a writer child started.
#[derive(Debug)]
struct Op {
    /// What it does to the documents, in log order (a batch puts its documents in order).
    effects: Vec<Effect>,
    /// Whether its return makes it, and every operation before it, survive a process kill: a write at `Async` or
    /// `Fsync`, a commit, a requested maintenance round (which flushes and commits with `idle_flush` 0).
    durable_on_return: bool,
    maint: bool,
}

/// What one writer child reported.
#[derive(Debug, Default)]
struct WriterRun {
    ops: Vec<Op>,
    returned: usize,
    /// The committed seq after each requested round that returned.
    round_seqs: Vec<u64>,
    /// The seq the child's open found.
    opened: Option<u64>,
}

impl WriterRun {
    fn record(&mut self, msg: &str, ctx: &str) {
        let words: Vec<&str> = msg.split(' ').collect();
        match words[0] {
            "opened" => self.opened = Some(words[1].parse().unwrap()),
            "op" => {
                let n: usize = words[1].parse().unwrap();
                assert_eq!(n, self.ops.len(), "{ctx}: operations out of order");
                assert_eq!(self.returned, n, "{ctx}: two operations in flight");
                self.ops.push(parse_op(&words[2..]));
            }
            "ret" => {
                let n: usize = words[1].parse().unwrap();
                assert_eq!(n, self.returned, "{ctx}: returns out of order");
                assert_eq!(n + 1, self.ops.len(), "{ctx}: return of no operation");
                if let Some(seq) = words.get(2) {
                    self.round_seqs.push(seq.parse().unwrap());
                }
                self.returned += 1;
            }
            "idle" => panic!("{ctx}: the child ran out of operations before its kill point"),
            other => panic!("{ctx}: unknown report {other}"),
        }
    }

    /// Every effect of every operation started, in log order.
    fn effects(&self) -> Vec<&Effect> {
        self.ops.iter().flat_map(|op| &op.effects).collect()
    }

    /// How many leading effects are acknowledged: everything up to the last durable return.
    fn acknowledged(&self) -> usize {
        let (mut n, mut acked) = (0, 0);
        for op in &self.ops[..self.returned] {
            n += op.effects.len();
            if op.durable_on_return {
                acked = n;
            }
        }
        acked
    }
}

fn parse_op(words: &[&str]) -> Op {
    let write = |effects, dur: &str| Op {
        effects,
        durable_on_return: dur != "B",
        maint: false,
    };
    match words {
        ["maint"] => Op {
            effects: Vec::new(),
            durable_on_return: true,
            maint: true,
        },
        ["commit"] => Op {
            effects: Vec::new(),
            durable_on_return: true,
            maint: false,
        },
        ["del", dur, id] => write(vec![Effect::Delete((*id).to_owned())], dur),
        ["put", dur, id, token, blob] => {
            let blob = (*blob != "-").then(|| blob.parse().unwrap());
            let val = Val {
                token: (*token).to_owned(),
                blob,
            };
            write(vec![Effect::Put((*id).to_owned(), val)], dur)
        }
        ["batch", dur, list] => {
            let effects = list
                .split(',')
                .map(|pair| {
                    let (id, token) = pair.split_once('=').unwrap();
                    let val = Val {
                        token: token.to_owned(),
                        blob: None,
                    };
                    Effect::Put(id.to_owned(), val)
                })
                .collect();
            write(effects, dur)
        }
        other => panic!("bad operation {other:?}"),
    }
}

/// When the parent kills a writer.
#[derive(Debug, Clone, Copy)]
enum KillAt {
    /// Right after the `n`th return.
    Returns(usize),
    /// Right after the child asked for its `n`th maintenance round: inside the round.
    Round(usize),
}

/// Runs a writer child and kills it as `kill` says; what it reported.
fn run_and_kill(path: &Path, config: &Config, kill: KillAt, ctx: &str) -> WriterRun {
    let mut child = Proc::spawn("writer", path, config, ctx);
    let mut run = WriterRun::default();
    loop {
        let msg = child.expect();
        run.record(&msg, ctx);
        let rounds = run.ops.iter().filter(|op| op.maint).count();
        match kill {
            KillAt::Returns(n) if run.returned >= n => break,
            KillAt::Round(n) if rounds >= n => break,
            _ => {}
        }
    }
    let (rest, status) = child.kill();
    for msg in rest {
        run.record(&msg, ctx);
    }
    assert!(
        !status.success(),
        "{ctx}: the writer exited by itself: {status}"
    );
    run
}

// ---------------------------------------------------------------------------------------------------------------
// Checks after a kill
// ---------------------------------------------------------------------------------------------------------------

/// Every live document id of `collection` in `snapshot`: the memtable's and the segments', as a lookup shows them.
fn ids(snapshot: &Snapshot, collection: &str) -> BTreeSet<String> {
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

/// The checks of `persia verify` on the file as it is, read-only (nothing is replayed into it, nothing truncated):
/// the engine's (manifest, segments, checksums of every committed frame) and the blob references'. Returns every
/// live id the read-only open shows.
fn verify_read_only(path: &Path, ctx: &str) -> BTreeSet<String> {
    let ro = ReadOnlyBlobDatabase::open(path, &DatabaseOptions::default())
        .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"));
    ro.database()
        .verify()
        .unwrap_or_else(|e| panic!("{ctx}: verify: {e}"));
    ro.verify_blobs(Scan::Deep)
        .unwrap_or_else(|e| panic!("{ctx}: verify blobs: {e}"));
    ids(&ro.database().snapshot(), COLLECTION)
}

/// The live documents among `ids` as `db` shows them, each checked whole: its body is the one its version token
/// names, and its blob holds the bytes the token names.
fn read_state(db: &Db, ids: &BTreeSet<String>, ctx: &str) -> State {
    let docs = db.collection(COLLECTION).unwrap();
    let mut state = State::new();
    for id in ids {
        let Some(doc) = docs.get(id).unwrap() else {
            continue;
        };
        let json: serde_json::Value =
            serde_json::from_str(&docs.get_json(id).unwrap().unwrap()).unwrap();
        let token = json["v"].as_str().unwrap().to_owned();
        assert_eq!(json["id"], id.as_str(), "{ctx}: {id}");
        assert_eq!(
            json["body"].as_str(),
            Some(body_of(&token).as_str()),
            "{ctx}: {id} is torn: its body is not the one of its version {token}"
        );
        let blob = doc.blob_ref().map(|r| {
            let mut bytes = Vec::new();
            db.get_blob(r)
                .unwrap_or_else(|e| panic!("{ctx}: {id}'s blob: {e}"))
                .read_to_end(&mut bytes)
                .unwrap_or_else(|e| panic!("{ctx}: reading {id}'s blob: {e}"));
            assert!(
                bytes == blob_bytes(&token, r.size()),
                "{ctx}: {id}'s blob is not the one of its version {token}"
            );
            r.size()
        });
        state.insert(id.clone(), Val { token, blob });
    }
    state
}

/// The least `p` from `acked` up such that `base` with the first `p` effects applied is `got`.
fn matching_prefix(base: &State, effects: &[&Effect], acked: usize, got: &State) -> Option<usize> {
    let mut state = base.clone();
    for e in &effects[..acked] {
        apply(&mut state, e);
    }
    if state == *got {
        return Some(acked);
    }
    for (i, e) in effects[acked..].iter().enumerate() {
        apply(&mut state, e);
        if state == *got {
            return Some(acked + i + 1);
        }
    }
    None
}

/// The first few ids where `a` and `b` differ, for failure messages.
fn diff(a: &State, b: &State) -> String {
    let ids: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let differ: Vec<String> = ids
        .into_iter()
        .filter(|id| a.get(*id) != b.get(*id))
        .map(|id| format!("{id}: {:?} vs {:?}", a.get(id), b.get(id)))
        .collect();
    format!(
        "{} ids differ: {}",
        differ.len(),
        differ
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ")
    )
}

/// Every file and directory under `dir`, relative, sorted.
fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            out.push(
                path.strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}

/// No temporary file of a create, a compaction or the blob pack is left under `dir` (SPEC §4.5.1, §9.2).
fn assert_no_temporary_files(dir: &Path, ctx: &str) {
    let files = tree(dir);
    let temporary: Vec<&String> = files
        .iter()
        .filter(|name| {
            name.contains(".tmp") || name.contains(".compact.") || name.contains("@tmp.")
        })
        .collect();
    assert!(
        temporary.is_empty(),
        "{ctx}: temporary files left: {temporary:?} (all: {files:?})"
    );
}

/// Copies the database's files as a kill left them, for the post-mortem of a failure.
fn snapshot_files(db_dir: &Path, into: &Path) {
    drop(std::fs::remove_dir_all(into)); // the previous kill's copy, if any
    std::fs::create_dir_all(into).unwrap();
    for entry in std::fs::read_dir(db_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), into.join(entry.file_name())).unwrap();
        }
    }
}

/// What the checks after one kill found.
#[derive(Debug, Default)]
struct Recovered {
    state: State,
    /// Effects the child started but had not acknowledged, and how many of them survived.
    unacked: usize,
    unacked_kept: usize,
    reopen: Duration,
}

/// The checks after a kill (module docs, scenario 1): `base` is the state before the child ran.
fn check_after_kill(
    path: &Path,
    base: &State,
    run: &WriterRun,
    universe: &BTreeSet<String>,
    ctx: &str,
) -> Recovered {
    // Before any read-write open, which may commit, truncate or quarantine.
    let seen = verify_read_only(path, ctx);
    let phantoms: Vec<&String> = seen.difference(universe).collect();
    assert!(
        phantoms.is_empty(),
        "{ctx}: documents nobody put: {phantoms:?}"
    );
    let reader = OpenOptions::new()
        .read_only(true)
        .open(path)
        .unwrap_or_else(|e| panic!("{ctx}: read-only Db::open after the kill: {e}"));
    let read_only = read_state(&reader, universe, ctx);
    reader.close().unwrap();

    let started = Instant::now();
    let db = Db::open(path)
        .unwrap_or_else(|e| panic!("{ctx}: Db::open after the kill: {e} ({:?})", e.code()));
    let reopen = started.elapsed();
    let state = read_state(&db, universe, ctx);
    assert!(
        read_only == state,
        "{ctx}: the read-only and the read-write opens differ: {}",
        diff(&read_only, &state)
    );

    let effects = run.effects();
    let acked = run.acknowledged();
    let Some(kept) = matching_prefix(base, &effects, acked, &state) else {
        let mut all = base.clone();
        for e in &effects {
            apply(&mut all, e);
        }
        let mut acknowledged = base.clone();
        for e in &effects[..acked] {
            apply(&mut acknowledged, e);
        }
        panic!(
            "{ctx}: the reopened documents are no prefix of the log that keeps the {acked} acknowledged effects \
             (of {} started; {} operations returned). Against the acknowledged prefix: {}. Against every started \
             effect: {}. Effects from the acknowledged one on: {:?}",
            effects.len(),
            run.returned,
            diff(&acknowledged, &state),
            diff(&all, &state),
            &effects[acked..]
        );
    };
    // A round of maintenance on the recovered database, then the close: nothing temporary is left behind.
    db.maintain_now()
        .unwrap_or_else(|e| panic!("{ctx}: maintenance after the reopen: {e}"));
    db.close()
        .unwrap_or_else(|e| panic!("{ctx}: close after the reopen: {e}"));
    assert_no_temporary_files(path.parent().unwrap(), ctx);
    Recovered {
        state,
        unacked: effects.len() - acked,
        unacked_kept: kept - acked,
        reopen,
    }
}

fn write_known(path: &Path, state: &State, universe: &BTreeSet<String>) {
    let mut out = String::new();
    for id in universe {
        let kind = if state.contains_key(id) {
            "live"
        } else {
            "deleted"
        };
        out.extend([kind, " ", id.as_str(), "\n"]);
    }
    std::fs::write(path, out).unwrap();
}

// ---------------------------------------------------------------------------------------------------------------
// Scenarios 1 and 2: kill loops
// ---------------------------------------------------------------------------------------------------------------

/// What a kill loop varies.
#[derive(Debug)]
struct Scenario {
    name: &'static str,
    files: u64,
    kills: u64,
    /// The maintenance thread runs only the rounds the child asks for (with `idle_flush` 0).
    hold: bool,
    blobs: bool,
    memtables: &'static [usize],
    merge_threads: &'static [u64],
    /// Durability mixes: each child picks one, then each operation a durability from it.
    durabilities: &'static [&'static str],
    kill: fn(&mut Rng) -> KillAt,
}

/// The totals a kill loop prints.
#[derive(Debug, Default)]
struct Totals {
    kills: u64,
    ops: usize,
    acked_effects: usize,
    unacked: usize,
    unacked_kept: usize,
    rounds: usize,
    merge_rounds: usize,
    reopens: Vec<Duration>,
}

fn kill_loop(sc: &Scenario) {
    let base = base_seed();
    let started = Instant::now();
    let mut totals = Totals::default();
    for file in 0..sc.files {
        let seed = base ^ fnv(sc.name.as_bytes()).wrapping_add(file.wrapping_mul(0x9e37_79b9));
        let ctx = format!(
            "{} file {file} (file seed {seed}, PERSIA_TEST_SEED={base})",
            sc.name
        );
        kill_loop_on_one_file(sc, seed, &ctx, &mut totals);
    }
    totals.reopens.sort();
    let pct = |p: usize| totals.reopens[(totals.reopens.len() - 1) * p / 100];
    eprintln!(
        "{}: {} files x {} kills in {:.1?}: {} operations started, {} acknowledged effects, {} unacknowledged of \
         which {} survived; {} requested rounds, {} of them committed more than one seq (a merge); reopen after a \
         kill p50 {:.1?} p99 {:.1?} max {:.1?}",
        sc.name,
        sc.files,
        sc.kills,
        started.elapsed(),
        totals.ops,
        totals.acked_effects,
        totals.unacked,
        totals.unacked_kept,
        totals.rounds,
        totals.merge_rounds,
        pct(50),
        pct(99),
        pct(100),
    );
}

fn kill_loop_on_one_file(sc: &Scenario, seed: u64, ctx: &str, totals: &mut Totals) {
    let work = Workdir::new();
    let db_dir = work.path().join("db");
    let path = db_dir.join("app.persia");
    let known = work.path().join("known.txt");
    let db = Db::open(&path).unwrap();
    db.create_collection_json(SCHEMA).unwrap();
    db.close().unwrap();

    let mut rng = Rng(seed);
    let mut state = State::new();
    let mut universe = BTreeSet::new();
    for kill in 0..sc.kills {
        write_known(&known, &state, &universe);
        let config = Config::default()
            .with("seed", &rng.next_u64())
            .with("prefix", &format!("k{kill}"))
            .with("memtable", rng.pick(sc.memtables))
            .with("merge", rng.pick(sc.merge_threads))
            .with("hold", &u8::from(sc.hold))
            .with("blobs", &u8::from(sc.blobs))
            .with("durs", rng.pick(sc.durabilities))
            .with("known", &known.display());
        let at = (sc.kill)(&mut rng);
        let ctx = format!("{ctx}, kill {kill} ({at:?}, child {})", config.encode());
        let run = run_and_kill(&path, &config, at, &ctx);
        snapshot_files(&db_dir, &work.path().join("after-kill"));
        universe.extend(run.effects().iter().map(|e| e.id().to_owned()));
        let recovered = check_after_kill(&path, &state, &run, &universe, &ctx);

        totals.kills += 1;
        totals.ops += run.ops.len();
        totals.acked_effects += run.acknowledged();
        totals.unacked += recovered.unacked;
        totals.unacked_kept += recovered.unacked_kept;
        totals.rounds += run.round_seqs.len();
        let mut last = run.opened.unwrap();
        for &seq in &run.round_seqs {
            if seq > last + 1 {
                totals.merge_rounds += 1;
            }
            last = seq;
        }
        totals.reopens.push(recovered.reopen);
        state = recovered.state;
    }
    // The last recovery is durable: a clean reopen shows it, and the file verifies.
    let final_ids = verify_read_only(&path, ctx);
    assert!(final_ids.is_subset(&universe), "{ctx}: final");
    let db = Db::open(&path).unwrap();
    let last = read_state(&db, &universe, ctx);
    assert!(
        last == state,
        "{ctx}: the final reopen differs: {}",
        diff(&state, &last)
    );
    db.close().unwrap();
}

#[test]
fn kill_loop_keeps_every_acknowledged_write() {
    kill_loop(&Scenario {
        name: "kill-loop",
        files: CRASH_FILES,
        kills: CRASH_KILLS,
        hold: false,
        blobs: false,
        // The default threshold (a backlog the next open replays) or a few KiB (the writes flush, then merge).
        memtables: &[64 << 20, 4096],
        merge_threads: &[0, 1],
        // Each write acknowledged at its return; or `Buffered` ones acknowledged by a later commit or synced write.
        durabilities: &["F", "A", "BBF", "BA", "BBBF"],
        kill: |rng| KillAt::Returns(rng.range(1, 60) as usize),
    });
}

#[test]
fn kills_inside_maintenance_rounds() {
    kill_loop(&Scenario {
        name: "maintenance-kills",
        files: MAINTENANCE_FILES,
        kills: MAINTENANCE_KILLS,
        hold: true,
        blobs: true,
        memtables: &[2048, 8192],
        // 0: the merge runs inside the round; 1: on a worker, committed by a later round.
        merge_threads: &[0, 1],
        durabilities: &["BA", "BBF", "A"],
        kill: |rng| KillAt::Round(rng.range(2, 10) as usize),
    });
}

// ---------------------------------------------------------------------------------------------------------------
// Scenario 3: a hard kill between the writes and the close
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_put_then_a_hard_kill_before_close() {
    let work = Workdir::new();
    let path = work.path().join("db").join("app.persia");
    let ctx = "put then kill";
    let mut child = Proc::spawn("put-and-wait", &path, &Config::default(), ctx);
    assert_eq!(child.expect(), "put");
    let (rest, status) = child.kill();
    assert!(rest.is_empty() && !status.success(), "{rest:?} {status}");

    let ids: BTreeSet<String> = ["f", "a", "b"]
        .iter()
        .flat_map(|p| (0..5).map(move |i| format!("{p}{i}")))
        .collect();
    let seen = verify_read_only(&path, ctx);
    assert!(seen.is_subset(&ids), "{seen:?}");
    let db = Db::open(&path).unwrap();
    let state = read_state(&db, &ids, ctx);
    db.close().unwrap();
    let live: BTreeSet<&str> = state.keys().map(String::as_str).collect();
    // Fsync and Async writes survive a killed process, deletes included; the Buffered ones that survive (if any
    // reached the file) are a prefix of them.
    for id in ["f2", "f3", "f4", "a0", "a1", "a2", "a3", "a4"] {
        assert!(live.contains(id), "{id} was acknowledged: {live:?}");
    }
    assert!(
        !live.contains("f0") && !live.contains("f1"),
        "a delete was undone: {live:?}"
    );
    let buffered = (0..5)
        .take_while(|i| live.contains(format!("b{i}").as_str()))
        .count();
    assert_eq!(
        live.len(),
        8 + buffered,
        "the Buffered writes kept are not a prefix: {live:?}"
    );
    for (id, val) in &state {
        assert_eq!(&val.token, id);
    }
    // The recovery is durable.
    let db = Db::open(&path).unwrap();
    assert_eq!(read_state(&db, &ids, ctx), state);
    db.close().unwrap();
    assert_no_temporary_files(path.parent().unwrap(), ctx);
}

// ---------------------------------------------------------------------------------------------------------------
// Scenario 4: a second process
// ---------------------------------------------------------------------------------------------------------------

fn visible(db: &Db, ids: &[&str]) -> Vec<String> {
    let docs = db.collection(COLLECTION).unwrap();
    ids.iter()
        .filter(|id| docs.get(id).unwrap().is_some())
        .map(|id| (*id).to_owned())
        .collect()
}

#[test]
fn a_second_process_beside_the_writer() {
    let work = Workdir::new();
    let path = work.path().join("app.persia");
    let ctx = "beside the writer";
    let mut holder = Proc::spawn("holder", &path, &Config::default(), ctx);
    assert_eq!(holder.expect(), "ready");

    // Every read-write open in this process is refused at once while the child holds the writer.
    for options in [
        OpenOptions::new(),
        OpenOptions::new().create_if_missing(false),
    ] {
        let err = options.open(&path).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Conflict, "{err}");
        assert!(err.message().contains("app.persia"), "{err}");
    }
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    let ids = ["d0", "d9", "x1", "x2", "x3"];
    assert_eq!(visible(&reader, &ids), ["d0", "d9"]);

    let mut step = |command: &str| {
        holder.send(command);
        assert_eq!(holder.expect(), "ok", "{command}");
    };
    step("fsync x1");
    assert_eq!(
        visible(&reader, &ids),
        ["d0", "d9"],
        "one state until the refresh"
    );
    reader.refresh().unwrap();
    assert_eq!(
        visible(&reader, &ids),
        ["d0", "d9", "x1"],
        "an Fsync write, logged"
    );
    step("async x2");
    step("delete d0");
    reader.refresh().unwrap();
    assert_eq!(
        visible(&reader, &ids),
        ["d9", "x1", "x2"],
        "Async writes are in the file (SPEC §8.4)"
    );
    step("buffered x3");
    step("commit");
    reader.refresh().unwrap();
    assert_eq!(
        visible(&reader, &ids),
        ["d9", "x1", "x2", "x3"],
        "a Buffered write once committed"
    );

    // The holder is killed: the kernel releases its lock, and the writer is free at once.
    let (rest, status) = holder.kill();
    assert!(rest.is_empty() && !status.success(), "{rest:?} {status}");
    let db = Db::open(&path).unwrap();
    assert_eq!(visible(&db, &ids), ["d9", "x1", "x2", "x3"]);
    db.collection(COLLECTION)
        .unwrap()
        .put_json(&doc_json("x4", "x4"))
        .unwrap();
    db.close().unwrap();
    reader.refresh().unwrap();
    assert_eq!(
        visible(&reader, &["x3", "x4"]),
        ["x3", "x4"],
        "the reader outlived its writer"
    );
    reader.close().unwrap();
}

#[test]
fn a_second_process_is_refused_as_a_writer() {
    let work = Workdir::new();
    let path = work.path().join("app.persia");
    let ctx = "second writer";
    let db = Db::open(&path).unwrap();
    let docs = db.create_collection_json(SCHEMA).unwrap();
    docs.put_json(&doc_json("p0", "p0")).unwrap();
    db.commit().unwrap();

    let config = Config::default().with("ids", &"p0,p1,p2");
    let mut probe = Proc::spawn("probe", &path, &config, ctx);
    // Locked, as ADR-0063 maps it: `Conflict`, whether the open would create or must find the file.
    assert_eq!(probe.expect_prefix("rw"), "PERSIA_CONFLICT");
    assert_eq!(probe.expect_prefix("rw"), "PERSIA_CONFLICT");
    assert_eq!(probe.expect_prefix("ro"), "p0");
    docs.put_json(&doc_json("p1", "p1")).unwrap(); // Fsync: in the file once it returns
    docs.clone()
        .with_durability(Durability::Buffered)
        .put_json(&doc_json("p2", "p2"))
        .unwrap();
    db.commit().unwrap();
    probe.send("refresh");
    assert_eq!(probe.expect_prefix("ro"), "p0,p1,p2");
    db.close().unwrap();
    let (rest, status) = probe.finish();
    assert!(status.success(), "{ctx}: {status}");
    assert_eq!(rest, ["closed"]);
}

// ---------------------------------------------------------------------------------------------------------------
// Scenario 5: clean shutdown in a process of its own
// ---------------------------------------------------------------------------------------------------------------

/// Runs a shutdown child that ends with `end`; checks it exited cleanly with no Persia thread, and that its
/// `Buffered` writes are in a segment (a commit after the one that created the collection).
fn shutdown_child(end: &str) {
    let work = Workdir::new();
    let path = work.path().join("app.persia");
    let ctx = format!("shutdown by {end}");
    let docs = 50;
    let config = Config::default().with("end", &end).with("docs", &docs);
    let mut child = Proc::spawn("shutdown", &path, &config, &ctx);
    let created: u64 = child.expect_prefix("created").parse().unwrap();
    let (rest, status) = child.finish();
    assert!(
        status.success(),
        "{ctx}: the process did not exit cleanly: {status}"
    );
    let ended = if end == "close" { "closed" } else { "dropped" };
    assert_eq!(rest[0], ended, "{ctx}: {rest:?}");
    let threads = rest[1].strip_prefix("threads ").unwrap();
    if cfg!(target_os = "linux") {
        assert!(
            threads.starts_with("0 "),
            "{ctx}: Persia threads still running after the {end}: {threads}"
        );
    }

    // The writes never reached the file before the end (`Buffered`, no commit, no idle flush in time): only the final
    // commit put them there. A read-only open does not commit, so what it shows is the file.
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    assert!(
        reader.committed_seq().unwrap() > created,
        "{ctx}: no final commit"
    );
    let all: BTreeSet<String> = (0..docs).map(|i| format!("s{i}")).collect();
    let state = read_state(&reader, &all, &ctx);
    assert_eq!(state.len(), all.len(), "{ctx}: {:?}", state.keys());
    reader.close().unwrap();
    verify_read_only(&path, &ctx);
    assert_no_temporary_files(work.path(), &ctx);
}

#[test]
fn a_child_that_closes_exits_cleanly() {
    shutdown_child("close");
}

#[test]
fn a_child_that_drops_makes_the_final_commit() {
    shutdown_child("drop");
}

// ---------------------------------------------------------------------------------------------------------------
// Scenario 6: creates racing across processes
// ---------------------------------------------------------------------------------------------------------------

/// `racers` processes open one missing path at once; each holds its database until all reported (`hold`) or closes
/// it at once.
fn race(round: u64, racers: usize, hold: bool) {
    let work = Workdir::new();
    let path = work.path().join("deep").join("er").join("app.persia");
    let ctx = format!("race round {round}, {racers} racers, hold {hold}");
    let mut children: Vec<Proc> = (0..racers)
        .map(|i| {
            let config = Config::default()
                .with("name", &format!("r{i}"))
                .with("hold", &u8::from(hold));
            Proc::spawn("racer", &path, &config, &ctx)
        })
        .collect();
    for child in &mut children {
        assert_eq!(child.expect(), "ready");
    }
    for child in &mut children {
        child.send("go");
    }
    let outcomes: Vec<String> = children.iter_mut().map(Proc::expect).collect();
    let mut headers = BTreeSet::new();
    let mut opened = Vec::new();
    for (i, outcome) in outcomes.iter().enumerate() {
        match outcome.split_once(' ') {
            Some(("ok", header)) => {
                headers.insert(header.to_owned());
                opened.push(i);
            }
            // SPEC §8.1 "Racing opens": never NotFound, AlreadyExists, Corrupt or Transient.
            Some(("err", code)) => assert_eq!(code, "PERSIA_CONFLICT", "{ctx}: racer {i}"),
            _ => panic!("{ctx}: racer {i}: {outcome}"),
        }
    }
    assert!(!opened.is_empty(), "{ctx}: no open succeeded: {outcomes:?}");
    if hold {
        assert_eq!(opened.len(), 1, "{ctx}: two writers at once: {outcomes:?}");
    }
    assert_eq!(
        headers.len(),
        1,
        "{ctx}: the opens saw different databases: {outcomes:?}"
    );
    for (i, mut child) in children.into_iter().enumerate() {
        if hold && opened.contains(&i) {
            child.send("close");
        }
        let (rest, status) = child.finish();
        assert!(status.success(), "{ctx}: racer {i}: {status}");
        let closed: &[&str] = if opened.contains(&i) {
            &["closed"]
        } else {
            &[]
        };
        assert_eq!(rest, closed, "{ctx}: racer {i}");
    }
    // One database, created once and never replaced, holding every winner's document.
    assert_eq!(
        headers.iter().next().unwrap(),
        &header_of(&path),
        "{ctx}: replaced"
    );
    let db = Db::open(&path).unwrap();
    let names: BTreeSet<String> = opened.iter().map(|i| format!("r{i}")).collect();
    let state = read_state(&db, &(0..racers).map(|i| format!("r{i}")).collect(), &ctx);
    assert_eq!(
        state.keys().cloned().collect::<BTreeSet<_>>(),
        names,
        "{ctx}"
    );
    db.close().unwrap();
    assert_no_temporary_files(work.path(), &ctx);
}

#[test]
fn racing_creates_across_processes() {
    for round in 0..RACE_ROUNDS {
        race(round, 4, true);
        race(round, 4, false);
    }
}
