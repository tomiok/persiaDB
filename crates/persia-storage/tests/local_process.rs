//! Real-file, multi-process tests of the temporary files of a local store's `put`s and their sweep (ROADMAP 2.3.8;
//! SPEC §10.1.1, ADR-0012, ADR-0018).
//!
//! Invariants, across processes:
//! 1. Writer processes `put` and `put_if_absent` while sweeper processes sweep in a loop: no write fails, and every
//!    acknowledged object reads back with its exact bytes.
//! 2. Writer processes killed with SIGKILL at seeded points, some certainly mid-`put` with the temporary file written and
//!    locked: a sweep then removes every temporary file left (exactly the ones present), none survives it, and every
//!    acknowledged object survives. Every object present, acknowledged or not, holds its exact bytes.
//! 3. A temporary file a live process holds the lock of is reported `InUse::Yes` and kept by every sweep, forced or
//!    not, and is removed once that process is gone.
//! 4. A `put` whose new temporary file another process locks before it does (a listing, or a sweep that removes it)
//!    takes another name, succeeds, and leaves no temporary file behind.
//!
//! Child processes are this test binary re-executed with [`ROLE_ENV`] set (the pattern of `container_process.rs`):
//! [`child_role`] then acts as the child named by the variable, and in a normal run it does nothing. Children report
//! on stdout with [`TAG`]-prefixed, newline-terminated lines, so the parent synchronizes on them, not on sleeps. The
//! only sleep is the seeded delay before a SIGKILL.
//!
//! Seeds: the scenarios derive their randomness from [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) and print it,
//! so a failure is replayed with `PERSIA_TEST_SEED=<seed> cargo test -p persia-storage --test local_process`.

#![cfg(unix)]
// Test-only code: unwraps, indexing and casts are the point.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::panic
)]

use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use bytes::Bytes;
use persia_storage::container::ContainerFile;
use persia_storage::vfs::{DirEntry, EntryKind, RealFs, Vfs};
use persia_storage::{
    ErrorKind, InUse, Key, KeyPrefix, LocalStorage, PutOutcome, SideFileKind, Storage,
};

/// Selects the child role of a re-executed test binary.
const ROLE_ENV: &str = "PERSIA_LOCAL_CHILD_ROLE";
/// The store root a child works on.
const ROOT_ENV: &str = "PERSIA_LOCAL_CHILD_ROOT";
/// The seed of a child's randomness.
const SEED_ENV: &str = "PERSIA_LOCAL_CHILD_SEED";
/// The key prefix of a writer child's objects.
const PREFIX_ENV: &str = "PERSIA_LOCAL_CHILD_PREFIX";
/// How many operations a writer child runs before it reports `done`.
const OPS_ENV: &str = "PERSIA_LOCAL_CHILD_OPS";
/// If set to `n`, a writer child stalls in its `n`-th (from 0) rename or link of a temporary file, lock held.
const STALL_ENV: &str = "PERSIA_LOCAL_CHILD_STALL_AFTER";
/// If set, a writer child pauses after creating its first temporary file ([`StallFs`]).
const PAUSE_CREATE_ENV: &str = "PERSIA_LOCAL_CHILD_PAUSE_CREATE";
/// If set to `put` or `put_if_absent`, every operation of a writer child is a write of that kind to a new key.
const OP_ENV: &str = "PERSIA_LOCAL_CHILD_OP";
/// What a sweeper child does: `sweep`, `force` (sweep with `force`) or `list` (`temporaries` only).
const MODE_ENV: &str = "PERSIA_LOCAL_CHILD_SWEEP_MODE";
/// Prefix of the lines children report; anything else on their stdout is libtest's.
const TAG: &str = "@@persia ";
/// A child stops on its own after this long, so a broken parent never leaves it running.
const CHILD_DEADLINE: Duration = Duration::from_secs(60);

fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_0238_0018);
    eprintln!("PERSIA_TEST_SEED={seed}");
    seed
}

/// `SplitMix64`: small, seedable, and good enough for test schedules.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform-ish in `lo..hi`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo)
    }
}

/// The bytes of the object at `key`: a pure function of it, so the parent can check any object a child wrote,
/// acknowledged or not. Keys starting with `L` sometimes get a payload of megabytes, whose write and fsync a SIGKILL
/// is likely to interrupt; the others stay small. Some payloads are empty.
fn payload(key: &str) -> Vec<u8> {
    let (mut rng, len) = payload_rng(key);
    let mut out = vec![0; len];
    for chunk in out.chunks_mut(8) {
        let word = rng.next_u64().to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    out
}

/// The length of [`payload`]`(key)`, without building it.
fn payload_len(key: &str) -> usize {
    payload_rng(key).1
}

/// The generator of [`payload`]`(key)` after it drew the length, and the length.
fn payload_rng(key: &str) -> (Rng, usize) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let mut rng = Rng(h);
    let large = key.starts_with('L');
    let len = match rng.next_u64() % 16 {
        0 if large => rng.range(1 << 20, 4 << 20),
        1..=3 if large => rng.range(16 << 10, 256 << 10),
        4 => 0,
        _ => rng.range(1, 4096),
    } as usize;
    (rng, len)
}

fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

/// Re-executes this test binary as the child `role` on the store at `root`.
fn spawn_child(role: &str, root: &Path, seed: u64, env: &[(&str, String)]) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .env(ROOT_ENV, root)
        .env(SEED_ENV, seed.to_string())
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
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

/// Forwards every tagged line of `child`'s stdout to the returned channel from a thread, so the child never blocks
/// on a full pipe (which would make most kills land in a `write` to stdout).
fn drain(child: &mut Child) -> (mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        while let Some(msg) = next_tagged(&mut out) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    (rx, reader)
}

/// Writes one tagged line to stdout and flushes it, bypassing libtest's capture.
fn report(msg: &str) {
    let mut out = io::stdout().lock();
    out.write_all(format!("{TAG}{msg}\n").as_bytes()).unwrap();
    out.flush().unwrap();
}

/// Reports `fail <why>` and exits with status 1, so the parent sees the reason, not just a status.
fn fail(why: &str) -> ! {
    report(&format!("fail {why}"));
    std::process::exit(1)
}

/// Blocks until stdin reaches EOF: the parent closes it, or the process is killed.
fn wait_for_stdin_eof() {
    let mut sink = Vec::new();
    io::stdin().read_to_end(&mut sink).unwrap();
}

/// Every `@tmp.*` file under `root` (at any depth), listed without opening or locking any.
fn temp_files(root: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                dirs.push(e.path());
            } else if e.file_name().to_string_lossy().starts_with("@tmp.") {
                out.insert(e.path());
            }
        }
    }
    out
}

/// Checks every object of `store`: each has the size of its [`payload`] and, if its key starts with `bytes_of`,
/// exactly its bytes (no partial or foreign object, acknowledged or not); and each key of `acked` is present.
/// Returns how many objects there are.
fn check_objects_under<V: Vfs>(
    store: &LocalStorage<V>,
    acked: &BTreeSet<String>,
    bytes_of: &str,
    ctx: &str,
) -> usize {
    let listed = store.list(&KeyPrefix::new("").unwrap()).unwrap();
    let present: BTreeSet<String> = listed.iter().map(|m| m.key.as_str().to_owned()).collect();
    for k in acked {
        assert!(
            present.contains(k),
            "{ctx}: acknowledged object {k} is missing"
        );
    }
    for m in &listed {
        let k = m.key.as_str();
        assert_eq!(m.size, payload_len(k) as u64, "{ctx}: listed size of {k}");
        if !k.starts_with(bytes_of) {
            continue;
        }
        let want = payload(k);
        let got = store
            .get(&m.key)
            .unwrap_or_else(|e| panic!("{ctx}: {k} unreadable: {e}"));
        assert!(
            *got == *want,
            "{ctx}: {k} holds {} bytes that are not its payload of {}",
            got.len(),
            want.len()
        );
    }
    listed.len()
}

/// [`check_objects_under`] reading every object's bytes.
fn check_objects<V: Vfs>(store: &LocalStorage<V>, acked: &BTreeSet<String>, ctx: &str) -> usize {
    check_objects_under(store, acked, "", ctx)
}

// ---------------------------------------------------------------------------------------------------------------
// Child roles
// ---------------------------------------------------------------------------------------------------------------

/// The entry point of every child process; a no-op in a normal test run.
#[test]
fn child_role() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let root = PathBuf::from(std::env::var(ROOT_ENV).unwrap());
    let seed: u64 = std::env::var(SEED_ENV).unwrap().parse().unwrap();
    match role.as_str() {
        "writer" => child_writer(&root, seed),
        "sweeper" => child_sweeper(&root),
        "hold-temp" => child_hold_temp(&root, seed),
        other => panic!("unknown child role {other}"),
    }
}

/// The real file system, except for two test points:
/// - With `stall_after` set to `n`, the `n`-th rename or hard link of a temporary file (the step of `put` and
///   `put_if_absent` after the file is written, fsynced and locked) reports `stalled <temp path>` and waits to be
///   killed: the kill then certainly leaves that temporary file behind.
/// - With `pause_create`, the first temporary file created reports `created <temp path>` and waits for a line on
///   stdin before `put` gets its handle, so another process can lock (or lock and remove) it before `put` does.
#[derive(Debug)]
struct StallFs {
    stall_after: Option<u64>,
    publishes: AtomicU64,
    pause_create: AtomicBool,
}

fn is_temp(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("@tmp."))
}

impl StallFs {
    fn before_publish(&self, from: &Path) {
        if !is_temp(from) {
            return;
        }
        let n = self.publishes.fetch_add(1, Ordering::Relaxed);
        if self.stall_after.is_some_and(|after| n >= after) {
            report(&format!("stalled {}", from.display()));
            wait_for_stdin_eof();
            fail("the stalled writer was not killed");
        }
    }
}

impl Vfs for StallFs {
    type File = std::fs::File;

    fn open(&self, path: &Path, write: bool) -> io::Result<Self::File> {
        RealFs.open(path, write)
    }
    fn create_new(&self, path: &Path) -> io::Result<Self::File> {
        let file = RealFs.create_new(path)?;
        if is_temp(path) && self.pause_create.swap(false, Ordering::Relaxed) {
            report(&format!("created {}", path.display()));
            let mut go = String::new();
            io::stdin().read_line(&mut go).unwrap();
        }
        Ok(file)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        RealFs.create_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.before_publish(from);
        RealFs.rename(from, to)
    }
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.before_publish(from);
        RealFs.hard_link(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        RealFs.remove_file(path)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        RealFs.sync_dir(dir)
    }
    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        RealFs.entry_kind(path)
    }
    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        RealFs.is_dir(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        RealFs.read_dir(dir)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        RealFs.canonicalize(path)
    }
    fn same_file(&self, file: &Self::File, path: &Path) -> io::Result<bool> {
        RealFs.same_file(file, path)
    }
    fn lock_beside(&self, path: &Path) -> io::Result<Option<Self::File>> {
        RealFs.lock_beside(path)
    }
}

/// Runs [`OPS_ENV`] seeded writes under [`PREFIX_ENV`] (or until killed, or [`CHILD_DEADLINE`]): `put`s and
/// `put_if_absent`s of new keys `<prefix>/<i % 5>/<i>`, each holding its [`payload`], and now and then a
/// `put_if_absent` of other bytes to its last new key, which must find it ([`OP_ENV`] forces one kind of write).
/// With [`STALL_ENV`] or [`PAUSE_CREATE_ENV`], stalls or pauses as [`StallFs`] says.
///
/// Reports `opened`, `ack <key>` once a write of a new key returned, `exists <key>` once a `put_if_absent` found
/// the object, and `done <acks>` after the last operation; `fail <why>` on any error.
fn child_writer(root: &Path, seed: u64) {
    let start = Instant::now();
    let mut rng = Rng(seed);
    let prefix = std::env::var(PREFIX_ENV).unwrap();
    let ops: u64 = std::env::var(OPS_ENV).unwrap().parse().unwrap();
    let op_kind = std::env::var(OP_ENV).ok();
    let vfs = StallFs {
        stall_after: std::env::var(STALL_ENV).ok().map(|n| n.parse().unwrap()),
        publishes: AtomicU64::new(0),
        pause_create: AtomicBool::new(std::env::var(PAUSE_CREATE_ENV).is_ok()),
    };
    let store = LocalStorage::open_in(vfs, root).unwrap_or_else(|e| fail(&format!("open: {e}")));
    report("opened");
    let (mut acks, mut last) = (0, None::<String>);
    for i in 0..ops {
        if start.elapsed() > CHILD_DEADLINE {
            break;
        }
        let op = match op_kind.as_deref() {
            Some("put") => 0,
            Some("put_if_absent") => 4,
            _ => rng.next_u64() % 8,
        };
        if op == 7
            && let Some(k) = &last
        {
            let other = Bytes::from(format!("not the payload of {k}"));
            match store.put_if_absent(&key(k), other) {
                Ok(PutOutcome::AlreadyExists) => report(&format!("exists {k}")),
                other => fail(&format!("put_if_absent over {k}: {other:?}")),
            }
            continue;
        }
        let k = format!("{prefix}/{}/{i}", i % 5);
        let data = Bytes::from(payload(&k));
        if op < 4 {
            store
                .put(&key(&k), data)
                .unwrap_or_else(|e| fail(&format!("put {k}: {e}")));
        } else {
            match store.put_if_absent(&key(&k), data) {
                Ok(PutOutcome::Created) => {}
                other => fail(&format!("put_if_absent {k}: {other:?}")),
            }
        }
        report(&format!("ack {k}"));
        acks += 1;
        last = Some(k);
    }
    report(&format!("done {acks}"));
}

/// Sweeps the store in a loop until stdin closes: with `force` if [`MODE_ENV`] is `force`, or only lists its
/// temporary files (`temporaries`, which takes each one's lock for an instant) if it is `list`. Reports `ready` after
/// the first pass, `swept <passes> <removed> <in_use>` at EOF (`removed` and `in_use` summed over all passes), and
/// `fail <why>` if a pass fails or reports something wrong.
fn child_sweeper(root: &Path) {
    let start = Instant::now();
    let mode = std::env::var(MODE_ENV).unwrap();
    let store = LocalStorage::open(root).unwrap_or_else(|e| fail(&format!("open: {e}")));
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            wait_for_stdin_eof();
            stop.store(true, Ordering::Relaxed);
        }
    });
    let list = mode == "list";
    let (mut sweeps, mut removed, mut in_use) = (0_u64, 0_u64, 0_u64);
    while !stop.load(Ordering::Relaxed) && start.elapsed() < CHILD_DEADLINE {
        let found = match mode.as_str() {
            "list" => store.temporaries(),
            "force" => store.sweep_temporaries(true),
            _ => store.sweep_temporaries(false),
        }
        .unwrap_or_else(|e| fail(&format!("{mode} pass {sweeps}: {e}")));
        for f in &found {
            let ok = f.kind == SideFileKind::Temporary
                && match f.in_use {
                    InUse::No => f.removed != list,
                    InUse::Yes => !f.removed,
                    InUse::Unknown => false,
                };
            if !ok {
                fail(&format!("{mode} pass {sweeps} reported {f:?}"));
            }
            removed += u64::from(f.removed);
            in_use += u64::from(f.in_use == InUse::Yes);
        }
        if sweeps == 0 {
            report("ready");
        }
        sweeps += 1;
    }
    report(&format!("swept {sweeps} {removed} {in_use}"));
}

/// Creates a temporary file in `<root>/a@/` the way `put` does (a new `@tmp.<16 hex>.<n>` name, locked right after
/// creation, the lock checked to be on the file that has the name, then written and fsynced), reports
/// `held <path>`, and keeps the lock until stdin closes; then exits, leaving the file behind as a crashed `put`
/// would.
fn child_hold_temp(root: &Path, seed: u64) {
    let dir = root.join("a@");
    let path = dir.join(format!(
        "@tmp.{:016x}.{}",
        Rng(seed).next_u64(),
        seed % 1000
    ));
    let file = RealFs
        .create_new(&path)
        .unwrap_or_else(|e| fail(&format!("create {}: {e}", path.display())));
    if !ContainerFile::try_lock(&file).unwrap() || !RealFs.same_file(&file, &path).unwrap() {
        fail("a sweep took the file before it was locked");
    }
    file.write_all_at(b"a put in progress", 0).unwrap();
    file.sync_all().unwrap();
    report(&format!("held {}", path.display()));
    wait_for_stdin_eof();
    drop(file);
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Writer and sweeper processes race (ADR-0018)
// ---------------------------------------------------------------------------------------------------------------

#[cfg(not(feature = "slow"))]
const RACE_WRITERS: u64 = 3;
#[cfg(feature = "slow")]
const RACE_WRITERS: u64 = 6;
#[cfg(not(feature = "slow"))]
const RACE_OPS: u64 = 120;
#[cfg(feature = "slow")]
const RACE_OPS: u64 = 2_000;

/// Writer processes `put` and `put_if_absent` while two sweeper processes (one forced) sweep and a third lists the
/// temporary files, each in a loop: no write fails, every acknowledged object reads back with its exact bytes, and
/// no temporary file is left. Without the lock, or without the check after taking it that the name is still the
/// writer's, a sweep removes the file of a write in progress and that write fails; a `put` that gives up a name
/// because a listing held its lock for an instant, without removing the file, leaves it behind.
#[test]
fn sweeper_processes_never_fail_or_lose_a_put_of_writer_processes() {
    let seed = base_seed();
    let mut rng = Rng(seed ^ 0x0238_0001);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = LocalStorage::open(&root).unwrap();

    let mut sweepers: Vec<_> = ["sweep", "force", "list"]
        .into_iter()
        .map(|mode| {
            let mut child = spawn_child("sweeper", &root, 0, &[(MODE_ENV, mode.to_owned())]);
            let (rx, reader) = drain(&mut child);
            assert_eq!(rx.recv().ok().as_deref(), Some("ready"), "seed {seed}");
            (child, rx, reader)
        })
        .collect();

    let started = Instant::now();
    let writers: Vec<_> = (0..RACE_WRITERS)
        .map(|w| {
            let env = [
                (PREFIX_ENV, format!("w{w}")),
                (OPS_ENV, RACE_OPS.to_string()),
            ];
            let mut child = spawn_child("writer", &root, rng.next_u64(), &env);
            let (rx, reader) = drain(&mut child);
            (child, rx, reader)
        })
        .collect();
    let mut acked = BTreeSet::new();
    let mut exists = 0;
    for (w, (mut child, rx, reader)) in writers.into_iter().enumerate() {
        let status = child.wait().unwrap();
        reader.join().unwrap();
        let lines: Vec<String> = rx.try_iter().collect();
        assert!(
            status.success() && lines.last().is_some_and(|l| l.starts_with("done ")),
            "seed {seed}: writer {w} ended with {status}: {:?}",
            lines.last()
        );
        for l in &lines {
            match l.split_once(' ') {
                Some(("ack", k)) => assert!(acked.insert(k.to_owned()), "{k} twice"),
                Some(("exists", _)) => exists += 1,
                _ => {}
            }
        }
    }
    let writing = started.elapsed();

    let mut sweep_stats = Vec::new();
    for (mut child, rx, reader) in sweepers.drain(..) {
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        reader.join().unwrap();
        let last = rx.try_iter().last();
        assert!(
            status.success() && last.as_deref().is_some_and(|l| l.starts_with("swept ")),
            "seed {seed}: sweeper ended with {status}: {last:?}"
        );
        sweep_stats.push(last.unwrap());
    }

    assert_eq!(
        acked.len() as u64 + exists,
        RACE_WRITERS * RACE_OPS,
        "seed {seed}"
    );
    let objects = check_objects(&store, &acked, &format!("seed {seed}"));
    assert_eq!(
        objects,
        acked.len(),
        "seed {seed}: an object nobody acknowledged"
    );
    assert_eq!(store.temporaries().unwrap(), [], "seed {seed}");
    assert_eq!(temp_files(&root), BTreeSet::new(), "seed {seed}");
    eprintln!(
        "writer/sweeper processes: {RACE_WRITERS} writers, {} new objects, {exists} put_if_absent collisions in \
         {writing:?}; sweepers (sweeps removed in_use): {sweep_stats:?}",
        acked.len()
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 2. kill -9 loop, then sweep (ADR-0012, ADR-0018)
// ---------------------------------------------------------------------------------------------------------------

#[cfg(not(feature = "slow"))]
const KILLS: u64 = 9;
#[cfg(feature = "slow")]
const KILLS: u64 = 240;

/// How [`kill_writer`] picks the moment of the kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kill {
    /// After a seeded number of acknowledgements and a seeded delay: anywhere in a write, or between two.
    Timed,
    /// Once the writer stalls in a rename or link, its temporary file written, fsynced and locked.
    MidPut,
}

/// Runs one writer child on `root` with keys under `prefix` and SIGKILLs it at a seeded point; returns the keys it
/// acknowledged and, for [`Kill::MidPut`], the temporary file it stalled on.
fn kill_writer(
    root: &Path,
    prefix: &str,
    rng: &mut Rng,
    kill: Kill,
    ctx: &str,
) -> (Vec<String>, Option<PathBuf>) {
    let child_seed = rng.next_u64();
    let kill_after = rng.range(0, 30) as usize;
    let delay = Duration::from_micros(rng.range(0, 4000));
    let mut env = vec![
        (PREFIX_ENV, prefix.to_owned()),
        (OPS_ENV, u64::MAX.to_string()),
    ];
    if kill == Kill::MidPut {
        env.push((STALL_ENV, rng.range(0, 30).to_string()));
    }
    let mut child = spawn_child("writer", root, child_seed, &env);
    let (rx, reader) = drain(&mut child);
    assert_eq!(rx.recv().ok().as_deref(), Some("opened"), "{ctx}");
    let mut acks = Vec::new();
    let mut stalled = None;
    let take = |msg: &str, acks: &mut Vec<String>| {
        if let Some(k) = msg.strip_prefix("ack ") {
            acks.push(k.to_owned());
        } else {
            assert!(msg.starts_with("exists "), "{ctx}: {msg}");
        }
    };
    loop {
        let Ok(msg) = rx.recv() else {
            panic!("{ctx}: writer exited before the kill");
        };
        if let Some(p) = msg.strip_prefix("stalled ") {
            assert_eq!(kill, Kill::MidPut, "{ctx}: {msg}");
            stalled = Some(PathBuf::from(p));
            break;
        }
        take(&msg, &mut acks);
        if kill == Kill::Timed && acks.len() >= kill_after {
            std::thread::sleep(delay);
            break;
        }
    }
    // Held until the child is reaped: `wait` closes stdin, and a killed child may still run long enough to see EOF
    // (macOS), where a stalled writer reports that it was not killed.
    let stdin = child.stdin.take();
    child.kill().unwrap(); // SIGKILL
    let status = child.wait().unwrap();
    drop(stdin);
    reader.join().unwrap();
    for msg in rx.try_iter() {
        if msg.starts_with("stalled ") {
            continue; // A timed kill that landed right after the stall point cannot happen: no stall configured.
        }
        take(&msg, &mut acks);
    }
    assert!(
        !status.success(),
        "{ctx}: writer exited by itself: {status}"
    );
    (acks, stalled)
}

/// Writer children are killed with SIGKILL at seeded points, a third of them certainly mid-`put` with a written, fsynced and
/// locked temporary file, and some keys carry megabyte payloads, so timed kills land mid-write and mid-fsync too.
/// After each kill, a sweep in this process removes exactly the temporary files present, all stale (no writer is
/// alive); afterwards none is left, every acknowledged object reads back, and every object present holds its exact
/// bytes.
#[test]
fn a_sweep_after_kill_9_removes_every_leftover_temporary_and_keeps_every_acknowledged_object() {
    let seed = base_seed();
    let mut rng = Rng(seed ^ 0x0238_0002);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = LocalStorage::open(&root).unwrap();
    let mut acked = BTreeSet::new();
    let (mut left, mut timed_left, mut mid_put) = (0, 0, 0);
    for iter in 0..KILLS {
        let ctx = format!("seed {seed}, kill {iter}");
        let kill = if iter % 3 == 2 {
            Kill::MidPut
        } else {
            Kill::Timed
        };
        let (acks, stalled) = kill_writer(&root, &format!("L{iter}"), &mut rng, kill, &ctx);
        acked.extend(acks);

        let present = temp_files(&root);
        if let Some(p) = &stalled {
            mid_put += 1;
            assert!(
                present.contains(p),
                "{ctx}: the stalled put's temporary {} is gone: {present:?}",
                p.display()
            );
        } else {
            timed_left += present.len();
        }
        left += present.len();

        let swept = store
            .sweep_temporaries(false)
            .unwrap_or_else(|e| panic!("{ctx}: sweep failed: {e}"));
        let swept_paths: BTreeSet<PathBuf> = swept.iter().map(|f| f.path.clone()).collect();
        assert_eq!(swept_paths, present, "{ctx}: the sweep found other files");
        assert_eq!(swept.len(), present.len(), "{ctx}");
        for f in &swept {
            assert_eq!(
                (f.kind, f.in_use, f.removed),
                (SideFileKind::Temporary, InUse::No, true),
                "{ctx}: {f:?}"
            );
        }
        assert_eq!(store.temporaries().unwrap(), [], "{ctx}");
        assert_eq!(temp_files(&root), BTreeSet::new(), "{ctx}");
        // Bytes of this writer's objects (earlier ones were read after their own kill); sizes of all.
        check_objects_under(&store, &acked, &format!("L{iter}/"), &ctx);
    }
    // The store still takes writes after all of it.
    store
        .put(&key("after/x"), Bytes::from(payload("after/x")))
        .unwrap();
    acked.insert("after/x".to_owned());
    let objects = check_objects(&store, &acked, &format!("seed {seed}"));
    assert!(
        left >= mid_put,
        "seed {seed}: {left} temporaries for {mid_put} stalled puts"
    );
    eprintln!(
        "kill-9 then sweep: {KILLS} kills ({mid_put} mid-put), {} acknowledged, {objects} objects, {left} \
         temporaries swept ({timed_left} from timed kills)",
        acked.len()
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 3. A temporary held by a live process is kept (ADR-0018)
// ---------------------------------------------------------------------------------------------------------------

/// How a holder child ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Kill,
    Exit,
}

/// A child that locked a temporary file the way `put` does ([`child_hold_temp`]) keeps it through every sweep,
/// forced or not, beside a stale one that goes; once the child is gone (killed, or exited leaving the file), the
/// next sweep removes it.
#[test]
fn a_temporary_a_live_process_holds_is_kept_until_the_process_is_gone() {
    for (n, end) in [End::Kill, End::Exit].into_iter().enumerate() {
        let ctx = format!("{end:?}");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let store = LocalStorage::open(&root).unwrap();
        store.put(&key("a/x"), Bytes::from(payload("a/x"))).unwrap();
        let stale = root.join("a@/@tmp.0123456789abcdef.0");
        std::fs::write(&stale, b"a crashed put").unwrap();

        let mut child = spawn_child("hold-temp", &root, 0x0238_0003 + n as u64, &[]);
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let held = next_tagged(&mut out).unwrap_or_else(|| "<eof>".into());
        let held = PathBuf::from(
            held.strip_prefix("held ")
                .unwrap_or_else(|| panic!("{ctx}: {held}")),
        );

        let listed = store.temporaries().unwrap();
        let report_of_list: Vec<_> = listed
            .iter()
            .map(|f| (f.path.clone(), f.in_use, f.removed))
            .collect();
        let mut want = vec![
            (held.clone(), InUse::Yes, false),
            (stale.clone(), InUse::No, false),
        ];
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(report_of_list, want, "{ctx}: temporaries");
        assert!(stale.exists(), "{ctx}: temporaries removed a file");

        let swept = store.sweep_temporaries(false).unwrap();
        let report_of_sweep: Vec<_> = swept
            .iter()
            .map(|f| (f.path.clone(), f.in_use, f.removed))
            .collect();
        let mut want = vec![
            (held.clone(), InUse::Yes, false),
            (stale.clone(), InUse::No, true),
        ];
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(report_of_sweep, want, "{ctx}: sweep");
        let forced = store.sweep_temporaries(true).unwrap();
        assert_eq!(
            forced
                .iter()
                .map(|f| (f.path.clone(), f.in_use, f.removed))
                .collect::<Vec<_>>(),
            [(held.clone(), InUse::Yes, false)],
            "{ctx}: forced sweep"
        );
        assert!(held.exists() && !stale.exists(), "{ctx}");
        // Writes go on beside the held file.
        store.put(&key("a/y"), Bytes::from(payload("a/y"))).unwrap();

        // For a kill, stdin stays open until the child is reaped, so it never sees EOF first.
        let stdin = child.stdin.take();
        let open_stdin = match end {
            End::Kill => {
                child.kill().unwrap();
                stdin
            }
            End::Exit => {
                drop(stdin);
                None
            }
        };
        let status = child.wait().unwrap();
        drop(open_stdin);
        assert_eq!(status.success(), end == End::Exit, "{ctx}: {status}");
        assert!(held.exists(), "{ctx}: the holder removed its file");

        // The first lock this process takes on the file: no copy of it can linger in another process.
        let swept = store.sweep_temporaries(false).unwrap();
        assert_eq!(
            swept
                .iter()
                .map(|f| (f.path.clone(), f.in_use, f.removed))
                .collect::<Vec<_>>(),
            [(held.clone(), InUse::No, true)],
            "{ctx}: sweep after the holder is gone"
        );
        assert!(!held.exists(), "{ctx}");
        assert_eq!(store.temporaries().unwrap(), [], "{ctx}");
        let acked: BTreeSet<String> = ["a/x", "a/y"].map(str::to_owned).into();
        assert_eq!(check_objects(&store, &acked, &ctx), 2, "{ctx}");
    }
}

/// A real `put` and `put_if_absent` in a writer process, stalled after the temporary file is written and locked
/// ([`StallFs`]): the file is reported in use and kept by every sweep, the object does not exist yet, and once the
/// writer is killed with SIGKILL the next sweep removes it, with the object still absent.
#[test]
fn a_stalled_put_of_a_live_process_keeps_its_temporary_until_killed() {
    let seed = base_seed();
    let mut rng = Rng(seed ^ 0x0238_0004);
    for (n, stall_after) in [0_u64, 1, 2, 3].into_iter().enumerate() {
        let ctx = format!("seed {seed}, stall after {stall_after}");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let store = LocalStorage::open(&root).unwrap();
        let env = [
            (PREFIX_ENV, format!("p{n}")),
            (OPS_ENV, u64::MAX.to_string()),
            (STALL_ENV, stall_after.to_string()),
        ];
        let mut child = spawn_child("writer", &root, rng.next_u64(), &env);
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut acked = BTreeSet::new();
        let held = loop {
            let msg = next_tagged(&mut out).unwrap_or_else(|| panic!("{ctx}: writer died"));
            if let Some(p) = msg.strip_prefix("stalled ") {
                break PathBuf::from(p);
            }
            if let Some(k) = msg.strip_prefix("ack ") {
                acked.insert(k.to_owned());
            }
        };

        for force in [false, true] {
            let swept = store.sweep_temporaries(force).unwrap();
            assert_eq!(
                swept
                    .iter()
                    .map(|f| (f.path.clone(), f.in_use, f.removed))
                    .collect::<Vec<_>>(),
                [(held.clone(), InUse::Yes, false)],
                "{ctx}: sweep, force {force}"
            );
        }
        assert!(held.exists(), "{ctx}");
        assert_eq!(
            check_objects(&store, &acked, &ctx),
            acked.len(),
            "{ctx}: the stalled write is visible"
        );

        let stdin = child.stdin.take();
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success(), "{ctx}");
        drop(stdin);
        let swept = store.sweep_temporaries(false).unwrap();
        assert_eq!(
            swept
                .iter()
                .map(|f| (f.path.clone(), f.in_use, f.removed))
                .collect::<Vec<_>>(),
            [(held.clone(), InUse::No, true)],
            "{ctx}: sweep after the kill"
        );
        assert_eq!(store.temporaries().unwrap(), [], "{ctx}");
        assert_eq!(check_objects(&store, &acked, &ctx), acked.len(), "{ctx}");
        // The key the stalled put was writing (if a new one) never appeared.
        for i in 0..64_u64 {
            let k = format!("p{n}/{}/{i}", i % 5);
            if !acked.contains(&k) {
                assert!(
                    matches!(store.get(&key(&k)), Err(e) if e.kind() == ErrorKind::NotFound),
                    "{ctx}: {k}"
                );
            }
        }
    }
}

/// What this process does to a writer process's temporary file between its creation and the writer's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interfere {
    /// Locks it and keeps the lock while the writer goes on, as a listing (`temporaries`) or a sweep holds it for an
    /// instant: the writer's lock fails on a file that still has the name.
    Hold,
    /// Locks it, removes it and unlocks, as a sweep does: the writer's lock succeeds on a file without the name.
    Remove,
    /// Locks it and removes it, keeping the lock while the writer goes on: the writer's lock fails on a file without
    /// the name.
    RemoveAndHold,
}

/// A writer process pauses right after creating its temporary file ([`StallFs`]), and this process locks the file
/// first, as a listing or a sweep in another process can: whatever it does ([`Interfere`]), the writer's `put` or
/// `put_if_absent` takes another name and succeeds, the object holds its bytes, and once the lock here is dropped
/// no temporary file is left (a file the writer gave up while a listing held it would be).
#[test]
fn a_put_whose_temporary_another_process_locked_first_takes_another_name() {
    for interfere in [Interfere::Hold, Interfere::Remove, Interfere::RemoveAndHold] {
        for op in ["put", "put_if_absent"] {
            let ctx = format!("{interfere:?}, {op}");
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("store");
            let store = LocalStorage::open(&root).unwrap();
            let env = [
                (PREFIX_ENV, "q".to_owned()),
                (OPS_ENV, "1".to_owned()),
                (OP_ENV, op.to_owned()),
                (PAUSE_CREATE_ENV, String::new()),
            ];
            let mut child = spawn_child("writer", &root, 0, &env);
            let mut out = BufReader::new(child.stdout.take().unwrap());
            assert_eq!(next_tagged(&mut out).as_deref(), Some("opened"), "{ctx}");
            let created = next_tagged(&mut out).unwrap_or_else(|| "<eof>".into());
            let temp = PathBuf::from(
                created
                    .strip_prefix("created ")
                    .unwrap_or_else(|| panic!("{ctx}: {created}")),
            );

            let file = std::fs::File::open(&temp).unwrap();
            assert!(ContainerFile::try_lock(&file).unwrap(), "{ctx}");
            if interfere != Interfere::Hold {
                std::fs::remove_file(&temp).unwrap();
            }
            let held = (interfere != Interfere::Remove).then_some(file);
            let stdin = child.stdin.as_mut().unwrap();
            stdin.write_all(b"go\n").unwrap();
            stdin.flush().unwrap();
            let mut lines = Vec::new();
            while let Some(l) = next_tagged(&mut out) {
                lines.push(l);
            }
            let status = child.wait().unwrap();
            assert!(
                status.success(),
                "{ctx}: writer ended with {status}: {lines:?}"
            );
            assert_eq!(lines, ["ack q/0/0", "done 1"], "{ctx}");
            drop(held);

            let acked: BTreeSet<String> = ["q/0/0".to_owned()].into();
            assert_eq!(check_objects(&store, &acked, &ctx), 1, "{ctx}");
            assert_eq!(
                temp_files(&root),
                BTreeSet::new(),
                "{ctx}: the writer left the temporary file it gave up"
            );
            assert_eq!(store.temporaries().unwrap(), [], "{ctx}");
        }
    }
}
