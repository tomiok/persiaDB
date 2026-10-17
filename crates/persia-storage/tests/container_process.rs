//! Real-file, multi-process tests of the single-file container (SPEC §4.5, §4.5.1, §4.7, §8.4).
//!
//! Child processes are this test binary re-executed with [`ROLE_ENV`] set: [`child_role`] then acts as the child
//! named by the variable, and in a normal run it does nothing. Children report progress on stdout with prefixed,
//! newline-terminated lines (libtest's own output is ignored), so the parent synchronizes on readiness lines, not
//! on sleeps. The only sleep is the seeded delay before a SIGKILL, which is the point of that scenario.
//!
//! Seeds: the kill-9 scenario derives its randomness from [`base_seed`] (`PERSIA_TEST_SEED`, default fixed) and prints
//! it (the lock and concurrent-create scenarios are deterministic), so a failure is replayed with
//! `PERSIA_TEST_SEED=<seed> cargo test -p persia-storage --test container_process`.

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
use persia_format::superblock::FIRST_FRAME_OFFSET;
use persia_storage::container::{Commit, Container, Error, FrameRef, Mode};

const MAX: usize = 1 << 20;
const NONE: Compression = Compression::None;

/// Selects the child role of a re-executed test binary.
const ROLE_ENV: &str = "PERSIA_CONTAINER_CHILD_ROLE";
/// The container path a child works on.
const PATH_ENV: &str = "PERSIA_CONTAINER_CHILD_PATH";
/// The seed of a child's randomness.
const SEED_ENV: &str = "PERSIA_CONTAINER_CHILD_SEED";
/// If set, the writer child stalls half-way through its first large frame after this many acknowledgements.
const STALL_ENV: &str = "PERSIA_CONTAINER_CHILD_STALL_AFTER";
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
    spawn_child_with(role, path, seed, &[])
}

/// [`spawn_child`] with extra environment variables.
fn spawn_child_with(role: &str, path: &Path, seed: u64, env: &[(&str, String)]) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["child_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .env(PATH_ENV, path)
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
        "mapped-reader" => child_mapped_reader(&path, seed, false),
        "mapped-reader-reopen" => child_mapped_reader(&path, seed, true),
        "torn-writer-kill" => child_torn_writer(&path, false),
        "torn-writer-exit" => child_torn_writer(&path, true),
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

/// Payloads above this many bytes are appended in pieces by [`append_payload`].
const STREAM_ABOVE: usize = 4 << 10;
/// The piece [`append_payload`] writes at once: one `pwrite` each.
const STREAM_CHUNK: usize = 64;

/// Appends the [`payload`] of the next frame of `kind`. A large one goes through [`Container::append_stream`] in
/// [`STREAM_CHUNK`]-byte writes: [`Container::append`] writes a frame with one `pwrite`, which a SIGKILL almost never
/// interrupts, so without this a killed writer would hardly ever leave a torn tail (ROADMAP 2.4.20).
///
/// With `stall`, a large frame stops half-way instead: this reports `stalled <frame_offset>` and waits to be killed,
/// so the kill tears it for certain.
fn append_payload(c: &mut Container, kind: FrameKind, stall: bool) -> FrameRef {
    let bytes = payload(kind, c.next_frame_seq());
    if bytes.len() <= STREAM_ABOVE {
        return c.append(kind, &bytes, NONE).unwrap();
    }
    let offset = c.end();
    let mut w = c.append_stream(kind, bytes.len() as u64).unwrap();
    for (i, chunk) in bytes.chunks(STREAM_CHUNK).enumerate() {
        if stall && i == bytes.len() / STREAM_CHUNK / 2 {
            report(&format!("stalled {offset}"));
            let mut sink = Vec::new();
            std::io::stdin().read_to_end(&mut sink).unwrap();
            panic!("the stalled writer was not killed");
        }
        w.write(chunk).unwrap();
    }
    w.finish().unwrap()
}

/// Opens `path` read-write and appends and commits until killed (or [`CHILD_DEADLINE`]). Large frames are streamed
/// in pieces ([`append_payload`]), so kills tear them. With [`STALL_ENV`] set to `n`, the first large frame after
/// `n` acknowledgements stalls half-way.
///
/// Reports `opened <seq>` after recovery, `synced <offset> <len> <frame_seq>` for each WAL frame once `sync`
/// returned, and `commit <seq> <manifest_offset> <manifest_len> <frame_seq>` once `commit` returned.
fn child_writer(path: &Path, seed: u64) {
    let start = std::time::Instant::now();
    let mut rng = Rng(seed);
    let stall_after: Option<usize> = std::env::var(STALL_ENV).ok().map(|n| n.parse().unwrap());
    let mut acks = 0;
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
            let stall = stall_after.is_some_and(|n| acks >= n);
            batch.push(append_payload(&mut c, FrameKind::WalChunk, stall));
        }
        c.sync().unwrap();
        for f in &batch {
            report(&format!("synced {} {} {}", f.offset, f.len, f.frame_seq));
            acks += 1;
        }
        // Some batches stay uncommitted: synced WAL frames after `log_end` must survive too (SPEC §8.2).
        if !rng.next_u64().is_multiple_of(4) {
            let stall = stall_after.is_some_and(|n| acks >= n);
            let m = append_payload(&mut c, FrameKind::Manifest, stall);
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
            acks += 1;
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

/// Opens `path` read-only with mapped reads and re-reads every committed frame of it until stdin closes (SPEC §4.7,
/// ADR-0017). With `reopen`, it also opens further mapped handles at seeded passes and keeps re-reading the last
/// [`MAPPED_VIEWS`] of them, so it holds mappings of files that writers in other processes later extend, recover,
/// truncate the tail of, or compact away.
///
/// Reports `ready <seq> <log_end> <frames>` for its first handle, `ok <passes> <opens> <reads>` after a full pass
/// that started after each line on stdin, `done <passes> <opens> <reads>` at EOF, and `fail <why>` before exiting
/// with status 1 on the first wrong byte or failed read.
fn child_mapped_reader(path: &Path, seed: u64, reopen: bool) {
    let mut rng = Rng(seed);
    let first = MappedView::open(path).unwrap_or_else(|e| fail(&format!("first open: {e}")));
    report(&format!(
        "ready {} {} {}",
        first.container.superblock().seq,
        first.log_end,
        first.seen.len()
    ));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            if line.is_err() || tx.send(()).is_err() {
                break;
            }
        }
    });
    let mut views = vec![first];
    let (mut passes, mut opens, mut reads) = (0_u64, 1_u64, 0_u64);
    let pass = |views: &[MappedView], passes: &mut u64, reads: &mut u64| {
        for (i, v) in views.iter().enumerate() {
            *reads += v
                .verify()
                .unwrap_or_else(|e| fail(&format!("pass {passes}, view {i}: {e}")));
        }
        *passes += 1;
    };
    loop {
        pass(&views, &mut passes, &mut reads);
        if reopen && rng.next_u64().is_multiple_of(8) {
            let v = MappedView::open(path).unwrap_or_else(|e| fail(&format!("open {opens}: {e}")));
            views.push(v);
            opens += 1;
            if views.len() > MAPPED_VIEWS {
                views.remove(0);
            }
        }
        match rx.try_recv() {
            Ok(()) => {
                pass(&views, &mut passes, &mut reads);
                report(&format!("ok {passes} {opens} {reads}"));
            }
            // Re-reading while the writer works is the point of this reader, so it does not wait for the parent; it
            // only yields, since tests do not sleep on the wall clock (CLAUDE.md).
            Err(std::sync::mpsc::TryRecvError::Empty) => std::thread::yield_now(),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }
    report(&format!("done {passes} {opens} {reads}"));
}

/// How many mapped handles a reopening reader re-reads at once.
const MAPPED_VIEWS: usize = 4;

/// Reports `fail <why>` and exits with status 1, so the parent sees the reason, not just a status.
fn fail(why: &str) -> ! {
    report(&format!("fail {why}"));
    std::process::exit(1)
}

/// The original `frame_seq` that [`payload`] embedded in `bytes`, if `bytes` is exactly the payload it builds for
/// a frame of `kind` and that seq. Compaction renumbers frames but keeps payloads, so this checks a frame whatever
/// file it was copied into.
fn payload_origin(kind: FrameKind, bytes: &[u8]) -> Result<u64, String> {
    let Some((&flag, rest)) = bytes.split_first() else {
        return Err("empty payload".into());
    };
    if flag != u8::from(kind == FrameKind::Manifest) {
        return Err(format!("{kind:?} frame with kind flag {flag}"));
    }
    let seq = u64::from_le_bytes(
        rest.get(..8)
            .ok_or_else(|| format!("payload of {} bytes", bytes.len()))?
            .try_into()
            .unwrap(),
    );
    if bytes != payload(kind, seq) {
        return Err(format!(
            "payload of {} bytes is not that of frame_seq {seq}",
            bytes.len()
        ));
    }
    Ok(seq)
}

/// A read-only handle with mapped reads, and every committed frame it saw when opened with the original
/// `frame_seq` its payload carries.
struct MappedView {
    container: Container,
    log_end: u64,
    seen: Vec<(FrameRef, u64)>,
}

impl MappedView {
    /// Opens `path` read-only, maps it, and reads every frame of `12288..log_end`, checking each one comes from the
    /// mapping (two reads share their bytes) and carries its [`payload`].
    fn open(path: &Path) -> Result<Self, String> {
        let mut container =
            Container::open(path, Mode::ReadOnly).map_err(|e| format!("open: {e}"))?;
        container
            .enable_mapped_reads()
            .map_err(|e| format!("map: {e}"))?;
        let log_end = container.superblock().log_end;
        let mut seen = Vec::new();
        let mut at = FIRST_FRAME_OFFSET;
        while at < log_end {
            let a = container
                .read_frame_source(at, MAX)
                .map_err(|e| format!("frame at {at}: {e}"))?;
            let b = container.read_frame_source(at, MAX).unwrap();
            if a.payload.as_ptr() != b.payload.as_ptr() {
                return Err(format!(
                    "frame at {at} below log_end {log_end} is not mapped"
                ));
            }
            let origin = payload_origin(a.frame.kind, &a.payload)
                .map_err(|e| format!("frame at {at}: {e}"))?;
            seen.push((a.frame, origin));
            at = a.frame.end();
        }
        if at != log_end {
            return Err(format!("frames end at {at}, past log_end {log_end}"));
        }
        let view = Self {
            container,
            log_end,
            seen,
        };
        view.verify()?;
        Ok(view)
    }

    /// Re-reads every frame seen at open; returns how many.
    fn verify(&self) -> Result<u64, String> {
        let range = self.container.mapped_range();
        if range != Some(FIRST_FRAME_OFFSET..self.log_end) {
            return Err(format!("mapped range is now {range:?}"));
        }
        for (f, origin) in &self.seen {
            let got = self
                .container
                .read_frame_source(f.offset, MAX)
                .map_err(|e| format!("{f:?}: {e}"))?;
            if got.frame != *f {
                return Err(format!("{f:?} now reads as {:?}", got.frame));
            }
            let now = payload_origin(f.kind, &got.payload).map_err(|e| format!("{f:?}: {e}"))?;
            if now != *origin {
                return Err(format!(
                    "{f:?} carried frame_seq {origin}, now carries {now}"
                ));
            }
        }
        Ok(self.seen.len() as u64)
    }
}

/// Opens `path` read-write, appends and syncs two WAL frames without committing them, then starts a Segment frame
/// and writes half its payload: a writer that dies mid-append (SPEC §4.4). Reports
/// `torn <wal1_offset> <wal1_len> <wal2_offset> <wal2_len> <torn_offset> <end>`, where `end` is the file length
/// the half frame leaves, then waits to be killed or, with `exit`, exits at once without finishing or syncing.
fn child_torn_writer(path: &Path, exit: bool) {
    let mut c = Container::open(path, Mode::ReadWrite).unwrap();
    let mut wal = Vec::new();
    for _ in 0..2 {
        let seq = c.next_frame_seq();
        wal.push(
            c.append(
                FrameKind::WalChunk,
                &payload(FrameKind::WalChunk, seq),
                NONE,
            )
            .unwrap(),
        );
    }
    c.sync().unwrap();
    let torn_offset = c.end();
    let mut w = c.append_stream(FrameKind::Segment, 200_000).unwrap();
    w.write(&vec![0xab; 100_000]).unwrap();
    let end = std::fs::metadata(path).unwrap().len();
    report(&format!(
        "torn {} {} {} {} {torn_offset} {end}",
        wal[0].offset, wal[0].len, wal[1].offset, wal[1].len
    ));
    if exit {
        std::process::exit(0);
    }
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
    panic!("the torn writer was not killed");
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
    // Every other kill tears a stalled frame (ROADMAP 2.4.20), and timed kills tear some streamed frames too.
    assert!(
        torn as u64 >= KILL_FILES * KILLS_PER_FILE / 2,
        "seed {seed}: only {torn} torn tails"
    );
}

/// How [`run_and_kill_writer`] picks the moment of the kill.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kill {
    /// After a seeded number of acknowledgements and a seeded delay: anywhere in the loop.
    Timed,
    /// Once the writer stalls half-way through a large frame, so the kill certainly tears it (ROADMAP 2.4.20).
    MidFrame,
}

/// Runs one writer child on `path` and SIGKILLs it at a seeded point; returns its acknowledgements and, for
/// [`Kill::MidFrame`], the offset of the frame it tore.
#[cfg(unix)]
fn run_and_kill_writer(
    path: &Path,
    rng: &mut Rng,
    kill: Kill,
    ctx: &str,
) -> (Vec<String>, Option<u64>) {
    let child_seed = rng.next_u64();
    // Kill after a seeded number of acknowledgements, then a seeded delay of up to 3 ms, so kills land at
    // every step of the loop: mid-append (streamed frames take many writes), mid-sync, between the Manifest and
    // its slot, mid-slot-write.
    let kill_after = rng.range(1, 120) as usize;
    let delay = std::time::Duration::from_micros(rng.range(0, 3000));

    let env = match kill {
        Kill::Timed => vec![],
        Kill::MidFrame => vec![(STALL_ENV, kill_after.to_string())],
    };
    let mut child = spawn_child_with("writer", path, child_seed, &env);
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
    let mut torn = None;
    loop {
        let Ok(msg) = rx.recv() else {
            panic!("{ctx}: writer exited before the kill");
        };
        if let Some(offset) = msg.strip_prefix("stalled ") {
            assert_eq!(kill, Kill::MidFrame, "{ctx}: {msg}");
            torn = Some(offset.parse().unwrap());
            break;
        }
        iter_acks.push(msg);
        if kill == Kill::Timed && iter_acks.len() >= kill_after {
            std::thread::sleep(delay);
            break;
        }
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    reader.join().unwrap();
    iter_acks.extend(rx.try_iter());
    assert!(
        !status.success(),
        "{ctx}: writer exited by itself: {status}"
    );
    (iter_acks, torn)
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
        // Every other writer is killed half-way through a large frame, so every file sees torn tails.
        let kill = if iter % 2 == 0 {
            Kill::Timed
        } else {
            Kill::MidFrame
        };
        let (iter_acks, stalled) = run_and_kill_writer(&path, rng, kill, &ctx);
        for msg in &iter_acks {
            acks.record(msg);
        }

        // Recovery.
        let len_before = std::fs::metadata(&path).unwrap().len();
        let mut c = Container::open(&path, Mode::ReadWrite)
            .unwrap_or_else(|e| panic!("{ctx}: recovery failed: {e}"));
        match &c.recovery().scan_end {
            ScanEnd::Clean { offset } => {
                assert_eq!(stalled, None, "{ctx}: the frame stalled at is not torn");
                assert_eq!(*offset, len_before, "{ctx}");
                assert_eq!(c.recovery().truncated_from, None, "{ctx}");
            }
            ScanEnd::TornTail { offset, .. } => {
                torn += 1;
                assert!(
                    stalled.is_none_or(|s| s == *offset),
                    "{ctx}: stalled at {stalled:?}"
                );
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

// ---------------------------------------------------------------------------------------------------------------
// 4. Mapped readers across processes (SPEC §4.6, §4.7, §8.6; ADR-0017)
// ---------------------------------------------------------------------------------------------------------------

/// A mapped-reader child ([`child_mapped_reader`]) and its stdout.
struct MappedReader {
    child: Child,
    out: BufReader<ChildStdout>,
    /// `seq`, `log_end` and frame count of its first handle.
    ready: (u64, u64, usize),
}

impl MappedReader {
    fn spawn(role: &str, path: &Path, seed: u64) -> Self {
        let mut child = spawn_child(role, path, seed);
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let ready = next_tagged(&mut out).unwrap_or_else(|| "<eof>".into());
        let nums: Vec<&str> = ready.split(' ').collect();
        assert_eq!(nums[0], "ready", "mapped reader: {ready}");
        let ready = (
            nums[1].parse().unwrap(),
            nums[2].parse().unwrap(),
            nums[3].parse().unwrap(),
        );
        Self { child, out, ready }
    }

    /// Asks for a full pass over every mapped frame and returns its `ok` line.
    fn check(&mut self, ctx: &str) -> String {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(b"check\n").unwrap();
        stdin.flush().unwrap();
        let reply = next_tagged(&mut self.out);
        match reply {
            Some(r) if r.starts_with("ok ") => r,
            other => {
                let status = self.child.wait().unwrap();
                panic!(
                    "{ctx}: mapped reader replied {other:?}, {}",
                    describe(status)
                )
            }
        }
    }

    /// Closes its stdin and returns its `done` line once it exited cleanly.
    fn finish(mut self, ctx: &str) -> String {
        drop(self.child.stdin.take());
        let last = next_tagged(&mut self.out);
        let status = self.child.wait().unwrap();
        assert!(
            status.success() && last.as_deref().is_some_and(|l| l.starts_with("done ")),
            "{ctx}: mapped reader ended with {last:?}, {}",
            describe(status)
        );
        last.unwrap()
    }
}

/// An exit status, naming the signal that ended the process if one did: a SIGBUS (7) on a mapped page is the
/// failure these tests look for.
fn describe(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal} (SIGBUS is 7)");
        }
    }
    format!("exited with {status}")
}

/// Appends 0-2 Segment frames, 1-3 WAL frames and a Manifest, all carrying their [`payload`], and commits them
/// with `wal_start` at the first WAL frame, so the earlier frames fall below it; returns the Segment frames.
fn commit_round(c: &mut Container, rng: &mut Rng) -> Vec<FrameRef> {
    let append = |c: &mut Container, kind| {
        let seq = c.next_frame_seq();
        c.append(kind, &payload(kind, seq), NONE).unwrap()
    };
    let segments: Vec<_> = (0..rng.range(0, 3))
        .map(|_| append(c, FrameKind::Segment))
        .collect();
    let wal = append(c, FrameKind::WalChunk);
    for _ in 1..rng.range(1, 4) {
        append(c, FrameKind::WalChunk);
    }
    c.sync().unwrap();
    let manifest = append(c, FrameKind::Manifest);
    c.commit(Commit {
        manifest,
        wal_start: wal.offset,
        fence_token: 0,
    })
    .unwrap();
    segments
}

/// The Manifest payload a parent's compaction writes: a [`payload`] whose seq no writer reaches.
fn compaction_manifest(n: u64) -> Vec<u8> {
    payload(FrameKind::Manifest, (1 << 40) + n)
}

/// A reader process holds one mapped read-only handle while this process appends and commits many batches and
/// then compacts in place: every frame it mapped keeps its bytes (the rename leaves it the old file, SPEC §4.6), and
/// a fresh reader process maps the compacted file.
#[test]
fn a_mapped_reader_process_keeps_its_bytes_across_commits_and_compaction() {
    const SEED: u64 = 0x0517_0001;
    eprintln!("seed {SEED}");
    let mut rng = Rng(SEED);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut w = Container::create(&path).unwrap();
    let mut segments = Vec::new();
    for _ in 0..10 {
        segments.extend(commit_round(&mut w, &mut rng));
    }

    let mut old = MappedReader::spawn("mapped-reader", &path, 0);
    assert_eq!(old.ready.0, 10);
    assert_eq!(old.ready.1, w.superblock().log_end);
    for round in 0..40 {
        segments.extend(commit_round(&mut w, &mut rng));
        if round % 10 == 9 {
            old.check(&format!("after round {round}"));
        }
    }

    // Every Segment lies below the last `wal_start`; keep those with an even `frame_seq`.
    let live: Vec<_> = segments
        .iter()
        .filter(|s| s.frame_seq.is_multiple_of(2))
        .map(|s| s.offset..s.end())
        .collect();
    assert!(!live.is_empty() && live.len() < segments.len());
    let len_before = std::fs::metadata(&path).unwrap().len();
    let relocation = w.compact(&live, |_| compaction_manifest(0), NONE).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() < len_before);
    old.check("after compaction");
    let mut new_segments = Vec::new();
    for _ in 0..5 {
        new_segments.extend(commit_round(&mut w, &mut rng));
    }
    old.check("after commits on the compacted file");
    let old_ready = old.ready;
    let done = old.finish("old reader");

    let mut fresh = MappedReader::spawn("mapped-reader", &path, 0);
    assert_eq!(fresh.ready.0, w.superblock().seq);
    assert_eq!(fresh.ready.1, w.superblock().log_end);
    fresh.check("fresh reader");
    fresh.finish("fresh reader");

    // The compacted file holds exactly the live Segments, with their payloads, and the later ones.
    let r = Container::open(&path, Mode::ReadOnly).unwrap();
    for s in &segments {
        let moved = relocation.get(s.offset);
        assert_eq!(moved.is_some(), s.frame_seq.is_multiple_of(2), "{s:?}");
        if let Some(m) = moved {
            let p = r.read_frame(m.offset, MAX).unwrap();
            assert_eq!(payload_origin(m.kind, &p.payload), Ok(s.frame_seq));
        }
    }
    let (mut at, mut found) = (FIRST_FRAME_OFFSET, 0);
    while at < r.superblock().log_end {
        let f = r.read_frame(at, MAX).unwrap();
        found += usize::from(f.frame.kind == FrameKind::Segment);
        at = f.frame.end();
    }
    assert_eq!(found, live.len() + new_segments.len());
    eprintln!(
        "mapped reader across compaction: {} frames mapped, {} segments ({} live), {done}",
        old_ready.2,
        segments.len(),
        live.len()
    );
}

/// Runs a torn writer child (`role`) on `path`: kills it unless it exits by itself; returns its report.
fn run_torn_writer(role: &str, path: &Path) -> (Vec<u64>, std::process::ExitStatus) {
    let mut child = spawn_child(role, path, 0);
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let torn = next_tagged(&mut out).unwrap_or_else(|| "<eof>".into());
    let mut words = torn.split(' ');
    assert_eq!(words.next(), Some("torn"), "torn writer: {torn}");
    let nums = words.map(|w| w.parse().unwrap()).collect();
    if role == "torn-writer-kill" {
        child.kill().unwrap();
    }
    (nums, child.wait().unwrap())
}

/// A writer process dies mid-append, leaving half a frame after two synced but uncommitted WAL frames. A mapped
/// reader process opened before keeps reading its frames; a read-write open in this process truncates the torn
/// tail, which lies past every mapping, on Unix. On Windows a live mapping keeps the file from shrinking, so that
/// open may fail with `Error::Io` and leave the file unchanged (ADR-0017); it succeeds once the reader is gone.
fn a_torn_tail_left_beside_a_mapped_reader_process(role: &str) {
    let mut rng = Rng(0x0517_0002);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut w = Container::create(&path).unwrap();
    for _ in 0..5 {
        commit_round(&mut w, &mut rng);
    }
    let sb = *w.superblock();
    drop(w);
    let mut reader = MappedReader::spawn("mapped-reader", &path, 0);
    assert_eq!((reader.ready.0, reader.ready.1), (sb.seq, sb.log_end));

    let (nums, status) = run_torn_writer(role, &path);
    let [wal1_off, wal1_len, wal2_off, wal2_len, torn_offset, end] = nums[..] else {
        panic!("{role}: {nums:?}");
    };
    assert_eq!(
        status.success(),
        role == "torn-writer-exit",
        "{role}: {status}"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), end, "{role}");
    assert_eq!(torn_offset, wal2_off + wal2_len, "{role}");
    reader.check(&format!("{role}: after the torn append"));

    let opened = Container::open(&path, Mode::ReadWrite);
    let c = if cfg!(windows) && matches!(opened, Err(Error::Io { .. })) {
        assert_eq!(std::fs::metadata(&path).unwrap().len(), end, "{role}");
        reader.check(&format!("{role}: after the refused truncation"));
        reader.finish(role);
        Container::open(&path, Mode::ReadWrite).unwrap()
    } else {
        let c = opened.unwrap_or_else(|e| panic!("{role}: read-write open failed: {e}"));
        reader.check(&format!("{role}: after the truncation"));
        reader.finish(role);
        c
    };
    assert!(
        matches!(c.recovery().scan_end, ScanEnd::TornTail { offset, .. } if offset == torn_offset),
        "{role}: {:?}",
        c.recovery()
    );
    assert_eq!(c.recovery().truncated_from, Some(end), "{role}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        torn_offset,
        "{role}"
    );
    assert_eq!(*c.superblock(), sb, "{role}");
    // The synced WAL frames survive the truncation (SPEC §8.2).
    let tail: Vec<_> = c
        .uncommitted_frames()
        .iter()
        .map(|f| (f.offset, f.len))
        .collect();
    assert_eq!(tail, [(wal1_off, wal1_len), (wal2_off, wal2_len)], "{role}");
    for f in c.uncommitted_frames() {
        let p = c.read_frame(f.offset, MAX).unwrap();
        assert_eq!(
            payload_origin(f.kind, &p.payload),
            Ok(f.frame_seq),
            "{role}"
        );
    }
}

#[test]
fn a_killed_writer_leaves_a_torn_tail_beside_a_mapped_reader_process() {
    a_torn_tail_left_beside_a_mapped_reader_process("torn-writer-kill");
}

#[test]
fn an_exited_writer_leaves_a_torn_tail_beside_a_mapped_reader_process() {
    a_torn_tail_left_beside_a_mapped_reader_process("torn-writer-exit");
}

#[cfg(all(unix, not(feature = "slow")))]
const MAPPED_KILLS: u64 = 10;
#[cfg(all(unix, feature = "slow"))]
const MAPPED_KILLS: u64 = 200;

/// Writer children are killed with SIGKILL at seeded points ([`run_and_kill_writer`]) and recovered read-write here, and
/// every third recovery compacts in place (a quarter of the writers die mid-way through a streamed frame, so
/// recoveries truncate torn tails too), while one reader process keeps re-reading every committed frame of
/// the mapped handles it opens along the way: no mismatched payload, no failed read, no SIGBUS.
#[cfg(unix)]
#[test]
fn kill_9_writers_never_disturb_a_mapped_reader_process() {
    let seed = base_seed();
    let mut rng = Rng(seed ^ 0x0517_0003);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut w = Container::create(&path).unwrap();
    commit_round(&mut w, &mut rng);
    drop(w);
    let mut reader = MappedReader::spawn("mapped-reader-reopen", &path, rng.next_u64());
    let (mut torn, mut compactions, mut last_ok) = (0, 0, String::new());
    for iter in 0..MAPPED_KILLS {
        let ctx = format!("seed {seed}, kill {iter}");
        // Killed writers tear streamed frames at random points; these always die half-way through one.
        if rng.next_u64().is_multiple_of(4) {
            run_torn_writer("torn-writer-kill", &path);
        } else {
            run_and_kill_writer(&path, &mut rng, Kill::Timed, &ctx);
        }
        let mut c = Container::open(&path, Mode::ReadWrite)
            .unwrap_or_else(|e| panic!("{ctx}: recovery failed: {e}"));
        torn += usize::from(matches!(c.recovery().scan_end, ScanEnd::TornTail { .. }));
        if iter % 3 == 2 {
            c.compact(&[], |_| compaction_manifest(iter), NONE)
                .unwrap_or_else(|e| panic!("{ctx}: compaction failed: {e}"));
            compactions += 1;
        }
        drop(c);
        last_ok = reader.check(&ctx);
    }
    let done = reader.finish(&format!("seed {seed}"));
    eprintln!(
        "kill-9 beside a mapped reader: {MAPPED_KILLS} kills, {torn} torn tails, {compactions} compactions, \
         last {last_ok}, {done} (passes opens reads)"
    );
}
