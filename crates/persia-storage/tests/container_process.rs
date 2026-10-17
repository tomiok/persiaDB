//! Real-file, multi-process tests of the single-file container (SPEC §4.5, §4.5.1, §4.7, §8.4).
//!
//! Child processes are this test binary re-executed with [`ROLE_ENV`] set: [`child_role`] then acts as the child
//! named by the variable, and in a normal run it does nothing. Children report progress on stdout with prefixed,
//! newline-terminated lines (libtest's own output is ignored), so the parent synchronizes on readiness lines, not
//! on sleeps. The only sleep is the seeded delay before a SIGKILL, which is the point of that scenario.
//!
//! Seeds: every scenario derives its randomness from [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) and prints
//! it, so a failure is replayed with `PERSIA_TEST_SEED=<seed> cargo test -p persia-storage --test
//! container_process`.

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

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Barrier};

use persia_format::block::Compression;
use persia_format::frame::{FrameKind, ScanEnd};
use persia_storage::container::{Commit, Container, Error, Mode};

#[cfg(unix)]
const MAX: usize = 1 << 20;
const NONE: Compression = Compression::None;

/// Selects the child role of a re-executed test binary.
const ROLE_ENV: &str = "PERSIA_CONTAINER_CHILD_ROLE";
/// The container path a child works on.
const PATH_ENV: &str = "PERSIA_CONTAINER_CHILD_PATH";
/// The seed of a child's randomness.
const SEED_ENV: &str = "PERSIA_CONTAINER_CHILD_SEED";
/// Prefix of the lines children report; anything else on their stdout is libtest's.
const TAG: &str = "@@persia ";
/// A writer child stops on its own after this long, so a broken parent never leaves it running.
const CHILD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(unix)]
fn base_seed() -> u64 {
    let seed = std::env::var("PERSIA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_c0de_2405);
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

/// The payload of the frame with `frame_seq`: a pure function of it, so the parent can check any frame a child
/// wrote. Sizes are skewed small with an occasional large frame, which a SIGKILL may tear mid-write.
fn payload(kind: FrameKind, frame_seq: u64) -> Vec<u8> {
    let mut rng = Rng(frame_seq ^ 0xa5a5_0000_0000_0000);
    let len = match rng.next_u64() % 16 {
        0 => rng.range(64 << 10, 256 << 10),
        1..=3 => rng.range(4 << 10, 32 << 10),
        4 => 0,
        _ => rng.range(1, 2048),
    } as usize;
    let mut out = Vec::with_capacity(len + 9);
    out.push(u8::from(kind == FrameKind::Manifest));
    out.extend_from_slice(&frame_seq.to_le_bytes());
    let mut x = rng.next_u64();
    out.extend((0..len).map(|i| {
        if i % 8 == 0 {
            x = rng.next_u64();
        }
        (x >> ((i % 8) * 8)) as u8
    }));
    out
}

/// Re-executes this test binary as the child `role`.
fn spawn_child(role: &str, path: &Path, seed: u64) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .env(PATH_ENV, path)
        .env(SEED_ENV, seed.to_string())
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

/// Writes one tagged line to stdout and flushes it, bypassing libtest's capture.
fn report(msg: &str) {
    let mut out = std::io::stdout().lock();
    out.write_all(format!("{TAG}{msg}\n").as_bytes()).unwrap();
    out.flush().unwrap();
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
    let path = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    let seed: u64 = std::env::var(SEED_ENV).unwrap().parse().unwrap();
    match role.as_str() {
        "hold-lock" => child_hold_lock(&path),
        "writer" => child_writer(&path, seed),
        "create" => child_create(&path),
        other => panic!("unknown child role {other}"),
    }
}

/// Opens `path` read-write, reports `locked`, and holds the lock until stdin closes.
fn child_hold_lock(path: &Path) {
    let c = Container::open(path, Mode::ReadWrite).unwrap();
    report("locked");
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
    drop(c);
}

/// Opens `path` read-write and appends and commits until killed (or [`CHILD_DEADLINE`]).
///
/// Reports `opened <seq>` after recovery, `synced <offset> <len> <frame_seq>` for each WAL frame once `sync`
/// returned, and `commit <seq> <manifest_offset> <manifest_len> <frame_seq>` once `commit` returned.
fn child_writer(path: &Path, seed: u64) {
    let start = std::time::Instant::now();
    let mut rng = Rng(seed);
    // A process another test forks keeps copies of the parent's file descriptions until it execs, so the lock of
    // the parent's last check can outlive it for a moment. Retry `Locked` briefly.
    let mut attempts = 0;
    let mut c = loop {
        match Container::open(path, Mode::ReadWrite) {
            Err(Error::Locked) if attempts < 100_000 => {
                attempts += 1;
                std::thread::yield_now();
            }
            other => break other.unwrap(),
        }
    };
    report(&format!("opened {}", c.superblock().seq));
    while start.elapsed() < CHILD_DEADLINE {
        let mut batch = Vec::new();
        for _ in 0..rng.range(1, 5) {
            let seq = c.next_frame_seq();
            batch.push(
                c.append(
                    FrameKind::WalChunk,
                    &payload(FrameKind::WalChunk, seq),
                    NONE,
                )
                .unwrap(),
            );
        }
        c.sync().unwrap();
        for f in &batch {
            report(&format!("synced {} {} {}", f.offset, f.len, f.frame_seq));
        }
        // Some batches stay uncommitted: synced WAL frames after `log_end` must survive too (SPEC §8.2).
        if !rng.next_u64().is_multiple_of(4) {
            let seq = c.next_frame_seq();
            let m = c
                .append(
                    FrameKind::Manifest,
                    &payload(FrameKind::Manifest, seq),
                    NONE,
                )
                .unwrap();
            c.commit(Commit {
                manifest: m,
                wal_start: batch[0].offset,
                fence_token: 0,
            })
            .unwrap();
            report(&format!(
                "commit {} {} {} {}",
                c.superblock().seq,
                m.offset,
                m.len,
                m.frame_seq
            ));
        }
    }
}

/// Waits for a line on stdin, then calls [`Container::create`] and reports `created`, `exists` or `error <e>`.
fn child_create(path: &Path) {
    report("ready");
    let mut go = String::new();
    std::io::stdin().read_line(&mut go).unwrap();
    match Container::create(path) {
        Ok(c) => {
            report("created");
            drop(c);
        }
        Err(Error::AlreadyExists { .. }) => report("exists"),
        Err(e) => report(&format!("error {e}")),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Writer lock across processes (SPEC §8.4)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_writer_lock_excludes_other_processes_until_the_holder_exits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(Container::create(&path).unwrap());

    let mut child = spawn_child("hold-lock", &path, 0);
    let mut out = BufReader::new(child.stdout.take().unwrap());
    assert_eq!(next_tagged(&mut out).as_deref(), Some("locked"));

    assert!(
        matches!(Container::open(&path, Mode::ReadWrite), Err(Error::Locked)),
        "a second writer must get Locked while another process holds the lock"
    );
    let ro = Container::open(&path, Mode::ReadOnly).unwrap();
    assert_eq!(ro.superblock().seq, 0);
    drop(ro);
    // `create` never replaces the locked file either.
    assert!(matches!(
        Container::create(&path),
        Err(Error::AlreadyExists { .. })
    ));

    drop(child.stdin.take()); // EOF: the child drops its handle and exits.
    let status = child.wait().unwrap();
    assert!(status.success(), "lock holder failed: {status}");

    let mut c = Container::open(&path, Mode::ReadWrite).unwrap();
    let w = c.append(FrameKind::WalChunk, b"after", NONE).unwrap();
    c.sync().unwrap();
    let m = c.append(FrameKind::Manifest, b"m", NONE).unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: w.offset,
        fence_token: 0,
    })
    .unwrap();
    assert_eq!(c.superblock().seq, 1);
}

#[cfg(unix)]
#[test]
fn a_killed_holder_releases_the_writer_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(Container::create(&path).unwrap());

    let mut child = spawn_child("hold-lock", &path, 0);
    let mut out = BufReader::new(child.stdout.take().unwrap());
    assert_eq!(next_tagged(&mut out).as_deref(), Some("locked"));
    assert!(matches!(
        Container::open(&path, Mode::ReadWrite),
        Err(Error::Locked)
    ));
    child.kill().unwrap(); // SIGKILL on Unix
    child.wait().unwrap();
    // The kernel drops the lock with the process's last descriptor.
    Container::open(&path, Mode::ReadWrite).unwrap();
}

// ---------------------------------------------------------------------------------------------------------------
// 2. kill -9 loop (SPEC §4.5, §4.5.1, §8.6)
// ---------------------------------------------------------------------------------------------------------------

#[cfg(all(unix, not(feature = "slow")))]
const KILL_FILES: u64 = 1;
#[cfg(all(unix, feature = "slow"))]
const KILL_FILES: u64 = 25;

/// Kills per file: the checks after each kill read every acknowledged frame of the file so far.
#[cfg(unix)]
const KILLS_PER_FILE: u64 = 12;

/// A frame a writer child reported as durable.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct Acked {
    kind: FrameKind,
    offset: u64,
    len: u64,
    frame_seq: u64,
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct Acks {
    frames: Vec<Acked>,
    /// The highest commit seq acknowledged, with its Manifest.
    commit: Option<(u64, Acked)>,
    lines: usize,
}

#[cfg(unix)]
impl Acks {
    fn record(&mut self, msg: &str) {
        let mut words = msg.split(' ');
        let verb = words.next().unwrap();
        let nums: Vec<u64> = words.map(|w| w.parse().unwrap()).collect();
        match verb {
            "opened" => return,
            "synced" => self.frames.push(Acked {
                kind: FrameKind::WalChunk,
                offset: nums[0],
                len: nums[1],
                frame_seq: nums[2],
            }),
            "commit" => {
                let m = Acked {
                    kind: FrameKind::Manifest,
                    offset: nums[1],
                    len: nums[2],
                    frame_seq: nums[3],
                };
                self.frames.push(m);
                self.commit = Some((nums[0], m));
            }
            other => panic!("unknown child report {other}"),
        }
        self.lines += 1;
    }
}

/// A writer child appends and commits in a loop on a real file and is killed with SIGKILL at a seeded point;
/// the parent recovers the file read-write and checks every acknowledgement survived.
///
/// SIGKILL kills the process, not the machine: data the child wrote but never synced stays in the page cache and
/// a reopen sees it. So this tests process-crash recovery (torn appends of a dying process, a commit cut between
/// its steps, repeated recoveries of one file, the lock released by the kernel), not power loss, which
/// `container_crash.rs` simulates on a `SimFile`.
#[cfg(unix)]
#[test]
fn kill_9_loop_never_loses_an_acknowledged_commit_or_sync() {
    let seed = base_seed();
    let mut rng = Rng(seed);
    let (mut acks, mut torn, mut bytes) = (0, 0, 0);
    for file in 0..KILL_FILES {
        let (a, t, b) = kill_loop_on_one_file(&mut rng, &format!("seed {seed}, file {file}"));
        acks += a;
        torn += t;
        bytes += b;
    }
    eprintln!(
        "kill-9 loop: {} kills on {KILL_FILES} files, {acks} acknowledgements, {torn} torn tails, {bytes} B written",
        KILL_FILES * KILLS_PER_FILE
    );
}

/// Runs one writer child on `path` and SIGKILLs it at a seeded point; returns its acknowledgements.
#[cfg(unix)]
fn run_and_kill_writer(path: &Path, rng: &mut Rng, ctx: &str) -> Vec<String> {
    let child_seed = rng.next_u64();
    // Kill after a seeded number of acknowledgements, then a seeded delay of up to 3 ms, so kills land at
    // every step of the loop: mid-append, mid-sync, between the Manifest and its slot, mid-slot-write.
    let kill_after = rng.range(1, 120) as usize;
    let delay = std::time::Duration::from_micros(rng.range(0, 3000));

    let mut child = spawn_child("writer", path, child_seed);
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let opened = next_tagged(&mut out).unwrap_or_else(|| panic!("{ctx}: writer died"));
    assert!(opened.starts_with("opened "), "{ctx}: {opened}");
    // A reader thread drains stdout so the child never blocks on a full pipe, which would make most kills
    // land in a `write` to stdout.
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        while let Some(msg) = next_tagged(&mut out) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    let mut iter_acks = Vec::new();
    while iter_acks.len() < kill_after {
        let Ok(msg) = rx.recv() else {
            panic!("{ctx}: writer exited before the kill");
        };
        iter_acks.push(msg);
    }
    std::thread::sleep(delay);
    child.kill().unwrap();
    let status = child.wait().unwrap();
    reader.join().unwrap();
    iter_acks.extend(rx.try_iter());
    assert!(
        !status.success(),
        "{ctx}: writer exited by itself: {status}"
    );
    iter_acks
}

/// [`KILLS_PER_FILE`] kills of writers on one new file; returns the acknowledgements, the torn tails recovered and
/// the final file size. Every acknowledgement so far is checked after every recovery: a later recovery must not
/// destroy an earlier one's frames.
#[cfg(unix)]
fn kill_loop_on_one_file(rng: &mut Rng, ctx: &str) -> (usize, usize, u64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(Container::create(&path).unwrap());

    let mut acks = Acks::default();
    let mut torn = 0;
    for iter in 0..KILLS_PER_FILE {
        let ctx = format!("{ctx}, kill {iter}");
        let iter_acks = run_and_kill_writer(&path, rng, &ctx);
        for msg in &iter_acks {
            acks.record(msg);
        }

        // Recovery.
        let len_before = std::fs::metadata(&path).unwrap().len();
        let mut c = Container::open(&path, Mode::ReadWrite)
            .unwrap_or_else(|e| panic!("{ctx}: recovery failed: {e}"));
        match &c.recovery().scan_end {
            ScanEnd::Clean { offset } => {
                assert_eq!(*offset, len_before, "{ctx}");
                assert_eq!(c.recovery().truncated_from, None, "{ctx}");
            }
            ScanEnd::TornTail { offset, .. } => {
                torn += 1;
                assert_eq!(c.recovery().truncated_from, Some(len_before), "{ctx}");
                assert_eq!(std::fs::metadata(&path).unwrap().len(), *offset, "{ctx}");
            }
            other => panic!("{ctx}: a killed process left {other:?}"),
        }
        assert_eq!(c.end(), std::fs::metadata(&path).unwrap().len(), "{ctx}");
        let sb = *c.superblock();
        if let Some((seq, m)) = acks.commit {
            assert!(sb.seq >= seq, "{ctx}: seq {} < acknowledged {seq}", sb.seq);
            if sb.seq == seq {
                assert_eq!(
                    (sb.manifest_off, sb.manifest_len),
                    (m.offset, m.len),
                    "{ctx}"
                );
            }
        }
        for a in &acks.frames {
            assert!(a.frame_seq < c.next_frame_seq(), "{ctx}: {a:?}");
            let f = c
                .read_frame(a.offset, MAX)
                .unwrap_or_else(|e| panic!("{ctx}: acknowledged {a:?} unreadable: {e}"));
            assert_eq!(
                (f.frame.kind, f.frame.len, f.frame.frame_seq),
                (a.kind, a.len, a.frame_seq),
                "{ctx}: {a:?}"
            );
            assert_eq!(f.payload, payload(a.kind, a.frame_seq), "{ctx}: {a:?}");
        }

        // A further commit works, and is itself acknowledged for the next iterations.
        let seq = c.next_frame_seq();
        let w = c
            .append(
                FrameKind::WalChunk,
                &payload(FrameKind::WalChunk, seq),
                NONE,
            )
            .unwrap();
        c.sync().unwrap();
        let seq = c.next_frame_seq();
        let m = c
            .append(
                FrameKind::Manifest,
                &payload(FrameKind::Manifest, seq),
                NONE,
            )
            .unwrap();
        c.commit(Commit {
            manifest: m,
            wal_start: w.offset,
            fence_token: 0,
        })
        .unwrap_or_else(|e| panic!("{ctx}: commit after recovery failed: {e}"));
        assert_eq!(c.superblock().seq, sb.seq + 1, "{ctx}");
        acks.record(&format!("synced {} {} {}", w.offset, w.len, w.frame_seq));
        acks.record(&format!(
            "commit {} {} {} {}",
            c.superblock().seq,
            m.offset,
            m.len,
            m.frame_seq
        ));
        drop(c);
    }
    (acks.lines, torn, std::fs::metadata(&path).unwrap().len())
}

// ---------------------------------------------------------------------------------------------------------------
// 3. Concurrent create (SPEC §4.7)
// ---------------------------------------------------------------------------------------------------------------

/// The container must be the only entry left in `dir`, and a valid, empty, unlocked container.
fn assert_single_valid_container(dir: &Path, path: &Path) {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        // Windows keeps the writer lock beside the container (SPEC §4.5.1).
        .filter(|n| !(cfg!(windows) && Path::new(n).extension().is_some_and(|e| e == "lock")))
        .collect();
    assert_eq!(
        entries,
        vec![path.file_name().unwrap().to_owned()],
        "temporary files left behind"
    );
    // Other tests of this binary spawn children: between fork and exec a child holds copies of every open file
    // description, so a `flock` the winner already dropped can outlive it for a moment. Retry `Locked` briefly.
    let mut attempts = 0;
    let c = loop {
        match Container::open(path, Mode::ReadWrite) {
            Err(Error::Locked) if attempts < 10_000 => {
                attempts += 1;
                std::thread::yield_now();
            }
            other => break other.unwrap(),
        }
    };
    assert_eq!(c.superblock().seq, 0);
    assert!(matches!(c.recovery().scan_end, ScanEnd::Clean { .. }));
    assert_eq!(c.frames(), &[]);
}

#[test]
fn concurrent_creates_in_threads_yield_one_container() {
    const THREADS: usize = 16;
    for round in 0..8 {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("app.persia"));
        let barrier = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let (path, barrier) = (Arc::clone(&path), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    // Dropped at once: the winner's lock must not outlive its handle.
                    Container::create(&*path).map(drop)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let created = results.iter().filter(|r| r.is_ok()).count();
        for r in &results {
            assert!(
                matches!(r, Ok(()) | Err(Error::AlreadyExists { .. })),
                "round {round}: {r:?}"
            );
        }
        assert_eq!(created, 1, "round {round}: {results:?}");
        assert_single_valid_container(dir.path(), &path);
    }
}

#[test]
fn concurrent_creates_in_processes_yield_one_container() {
    const PROCESSES: usize = 6;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut children: Vec<_> = (0..PROCESSES)
        .map(|_| {
            let mut child = spawn_child("create", &path, 0);
            let out = BufReader::new(child.stdout.take().unwrap());
            (child, out)
        })
        .collect();
    for (_, out) in &mut children {
        assert_eq!(next_tagged(out).as_deref(), Some("ready"));
    }
    // Release them as close together as pipes allow.
    for (child, _) in &mut children {
        child.stdin.as_mut().unwrap().write_all(b"go\n").unwrap();
    }
    let mut outcomes = Vec::new();
    for (mut child, mut out) in children {
        outcomes.push(next_tagged(&mut out).unwrap_or_else(|| "<eof>".into()));
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(
        outcomes.iter().filter(|o| *o == "created").count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes.iter().filter(|o| *o == "exists").count(),
        PROCESSES - 1,
        "{outcomes:?}"
    );
    assert_single_valid_container(dir.path(), &path);
}
