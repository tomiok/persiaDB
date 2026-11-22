//! Reader lease takes, rewrites and drops interleaved with the writer's scan, step by step (SPEC §9.3, §9.5; ROADMAP
//! 9.2.16; ADR-0054 items 4 to 8, 12).
//!
//! Each actor (a reader that takes a lease, rewrites it twice and drops it; a writer that scans) runs on its own
//! thread over a simulated file system whose every operation on `<pack>.leases/` is a step: a deterministic
//! scheduler lets exactly one actor run between two steps, picked by a policy. The policies are every schedule with
//! at most two switches between a reader and the writer (every window: create to lock, lock to the name check, the
//! drop's removal to its unlock, a rewrite half done, each against every step of a scan), and seeded random schedules over two readers and a writer
//! that scans twice. Variants: the record locked itself (Unix), a lock sidecar (Windows' `lock_beside`, simulated),
//! and writes torn into pieces in a random order (a rewrite is not atomic against a concurrent read, ADR-0054 item
//! 6).
//!
//! Invariants:
//! 1. The writer never removes a reader lease, or its sidecar, that a take returned as held, before its drop.
//! 2. A scan that lists after a take returned, and ends before its drop, finds the lease live. The record it read is
//!    the last one completely written before its read, or, if a rewrite was in progress then, the old one, the new
//!    one or unreadable: never an older one, never another one.
//! 3. When every actor is done, the leases directory is empty: no take attempt, stale removal or drop leaves a file.
//! 4. A scan makes at most one take attempt fail, so with at most two scans every take is held.
//!
//! Deterministic: schedules come from the policy and a fixed seed; the seed and schedule are in each thread's name
//! and every failure message, with the whole trace. Lease ids are drawn at random by the take (no seam), so with two
//! readers the order the writer judges them in, and so a random schedule's steps, may differ between runs. Not run
//! under Miri (threads and thousands of schedules).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use persia_blob::Error;
use persia_blob::pack::lease::{
    LEASE_LEN, LeaseRecord, LeaseScan, LiveLease, ReaderLease, leases_dir, scan_leases,
};
use persia_storage::container::{Container, ContainerFile};
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use persia_storage::vfs::{DirEntry, EntryKind, Vfs};

const DB: &str = "/db/app.persia";
const PACK: &str = "/db/app.persia.blobs";
const UUID: [u8; 16] = [7; 16];
/// The seqs a reader's record names in turn once it holds its lease.
const SEQS: [u64; 2] = [5, 6];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- the scheduler ---

/// What an actor did, in the order it happened (one actor runs at a time, so this order is the real one).
#[derive(Debug, Clone)]
enum Event {
    /// A take returned a held lease at this path.
    Held(PathBuf),
    /// A take failed (only `LeaseContended` is allowed, and only with more than two scans).
    TakeFailed(String),
    /// A write of `bytes` at offset 0 of a lease file began / ended.
    WriteBegin(PathBuf, Vec<u8>),
    WriteEnd(PathBuf),
    /// The holder's drop began.
    DropBegin(PathBuf),
    /// A file under the leases directory was removed by this actor.
    Removed(PathBuf),
    /// The writer listed the leases directory.
    Listed,
    /// The writer read a lease file's bytes.
    Read(PathBuf),
    /// The writer's scan returned.
    ScanEnd(LeaseScan),
}

#[derive(Default)]
struct State {
    /// The actor allowed to run until its next step.
    turn: Option<usize>,
    waiting: BTreeSet<usize>,
    finished: BTreeSet<usize>,
    events: Vec<(usize, Event)>,
}

#[derive(Default)]
struct Sched {
    state: Mutex<State>,
    cv: Condvar,
}

impl fmt::Debug for Sched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sched").finish_non_exhaustive()
    }
}

impl Sched {
    /// A step: waits until the driver picks `me`.
    fn step(&self, me: usize) {
        let mut s = lock(&self.state);
        s.waiting.insert(me);
        if s.turn == Some(me) {
            s.turn = None;
        }
        self.cv.notify_all();
        while s.turn != Some(me) {
            s = self.cv.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        s.waiting.remove(&me);
    }

    fn event(&self, me: usize, event: Event) {
        lock(&self.state).events.push((me, event));
    }

    fn finish(&self, me: usize) {
        let mut s = lock(&self.state);
        s.finished.insert(me);
        s.waiting.remove(&me);
        if s.turn == Some(me) {
            s.turn = None;
        }
        self.cv.notify_all();
    }

    /// Runs `n` actors to their ends, one step at a time, letting `pick` choose among those waiting. Returns how many
    /// steps each actor was given.
    fn drive(&self, n: usize, mut pick: impl FnMut(&[usize], &[u64]) -> usize) -> Vec<u64> {
        let mut given = vec![0_u64; n];
        loop {
            let mut s = lock(&self.state);
            while !(s.turn.is_none() && s.waiting.len() + s.finished.len() == n) {
                s = self.cv.wait(s).unwrap_or_else(PoisonError::into_inner);
            }
            if s.finished.len() == n {
                return given;
            }
            let waiting: Vec<usize> = s.waiting.iter().copied().collect();
            let chosen = pick(&waiting, &given);
            assert!(waiting.contains(&chosen));
            given[chosen] += 1;
            s.turn = Some(chosen);
            self.cv.notify_all();
        }
    }
}

/// Marks its actor finished when dropped, a panic included, so the driver never waits on a dead actor.
struct Finish<'a>(&'a Sched, usize);

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.0.finish(self.1);
    }
}

// --- the stepped file system ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Variant {
    /// `lock_beside` locks `<path>.lock`, as on Windows.
    sidecars: bool,
    /// A 64-byte write at offset 0 of a lease file is made of pieces, written in a random order, a step each.
    torn: bool,
}

#[derive(Debug, Clone)]
struct Stepped {
    fs: SimFs,
    me: usize,
    sched: Arc<Sched>,
    variant: Variant,
    rng: Arc<Mutex<XorShift>>,
}

#[derive(Debug)]
struct File {
    inner: SimFsFile,
    path: PathBuf,
    owner: Stepped,
}

fn under_leases(path: &Path) -> bool {
    path.starts_with(leases_dir(Path::new(PACK)))
}

impl Stepped {
    fn step(&self, path: &Path) {
        if under_leases(path) {
            self.sched.step(self.me);
        }
    }

    fn event(&self, event: Event) {
        self.sched.event(self.me, event);
    }

    fn wrap(&self, inner: SimFsFile, path: &Path) -> File {
        File {
            inner,
            path: path.to_owned(),
            owner: self.clone(),
        }
    }
}

impl ContainerFile for File {
    fn file_len(&self) -> io::Result<u64> {
        self.owner.step(&self.path);
        self.inner.file_len()
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.owner.step(&self.path);
        let read = self.inner.read_exact_at(buf, offset);
        if under_leases(&self.path) {
            self.owner.event(Event::Read(self.path.clone()));
        }
        read
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.owner.step(&self.path);
        if !under_leases(&self.path) {
            return self.inner.write_all_at(buf, offset);
        }
        self.owner
            .event(Event::WriteBegin(self.path.clone(), buf.to_vec()));
        if self.owner.variant.torn && buf.len() == LEASE_LEN && offset == 0 {
            // Cut into 2 to 8 pieces, written in a random order, a step between two.
            let pieces = {
                let mut rng = lock(&self.owner.rng);
                let cuts = 1 + (rng.next() % 7) as usize;
                let mut at: Vec<usize> =
                    (0..cuts).map(|_| 1 + (rng.next() % 63) as usize).collect();
                at.extend([0, LEASE_LEN]);
                at.sort_unstable();
                at.dedup();
                let mut pieces: Vec<(usize, usize)> = at.windows(2).map(|w| (w[0], w[1])).collect();
                for i in (1..pieces.len()).rev() {
                    pieces.swap(i, (rng.next() % (i as u64 + 1)) as usize);
                }
                pieces
            };
            for (k, (from, to)) in pieces.into_iter().enumerate() {
                if k > 0 {
                    self.owner.sched.step(self.owner.me);
                }
                self.inner.write_all_at(&buf[from..to], from as u64)?;
            }
        } else {
            self.inner.write_all_at(buf, offset)?;
        }
        self.owner.event(Event::WriteEnd(self.path.clone()));
        Ok(())
    }
    fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.owner.step(&self.path);
        self.inner.set_len(len)
    }
    fn try_lock(&self) -> io::Result<bool> {
        self.owner.step(&self.path);
        self.inner.try_lock()
    }
    fn unlock(&self) -> io::Result<()> {
        self.owner.step(&self.path);
        self.inner.unlock()
    }
    fn locks_per_handle(&self) -> bool {
        self.inner.locks_per_handle()
    }
}

impl Vfs for Stepped {
    type File = File;

    fn open(&self, path: &Path, write: bool) -> io::Result<File> {
        self.step(path);
        Ok(self.wrap(self.fs.open(path, write)?, path))
    }
    fn create_new(&self, path: &Path) -> io::Result<File> {
        self.step(path);
        Ok(self.wrap(self.fs.create_new(path)?, path))
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.step(path);
        self.fs.create_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.step(from);
        self.fs.rename(from, to)
    }
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.step(from);
        self.fs.hard_link(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.step(path);
        self.fs.remove_file(path)?;
        if under_leases(path) {
            self.event(Event::Removed(path.to_owned()));
        }
        Ok(())
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.fs.sync_dir(dir)
    }
    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        self.step(path);
        self.fs.entry_kind(path)
    }
    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        self.step(path);
        self.fs.is_dir(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        self.step(dir);
        let listed = self.fs.read_dir(dir)?;
        if under_leases(dir) {
            self.event(Event::Listed);
        }
        Ok(listed)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.fs.canonicalize(path)
    }
    fn same_file(&self, file: &File, path: &Path) -> io::Result<bool> {
        self.step(path);
        self.fs.same_file(&file.inner, path)
    }
    fn lock_beside(&self, path: &Path) -> io::Result<Option<File>> {
        if !self.variant.sidecars {
            return Ok(None);
        }
        let mut name = path.as_os_str().to_owned();
        name.push(".lock");
        let sidecar = PathBuf::from(name);
        // As `RealFs` on Windows: open or create `<path>.lock`, then lock it; two steps.
        self.step(&sidecar);
        let inner = match self.fs.open(&sidecar, true) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.fs.create_new(&sidecar)?,
            Err(e) => return Err(e),
        };
        self.step(&sidecar);
        if inner.try_lock()? {
            Ok(Some(self.wrap(inner, &sidecar)))
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
}

/// A deterministic xorshift generator.
#[derive(Debug)]
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

// --- one run ---

struct Run {
    events: Vec<(usize, Event)>,
    readers: usize,
    scans: usize,
    fs: SimFs,
}

/// Runs `readers` readers (actors `0..readers`) and a writer that scans `scans` times (actor `readers`) under `pick`,
/// then checks the invariants. `what` names the schedule in every failure message.
fn run(
    what: &str,
    variant: Variant,
    readers: usize,
    scans: usize,
    seed: u64,
    pick: impl FnMut(&[usize], &[u64]) -> usize,
) -> Vec<u64> {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    drop(Container::create_in(Arc::new(fs.clone()), DB).unwrap());
    let sched = Arc::new(Sched::default());
    let vfs_of = |me: usize| Stepped {
        fs: fs.clone(),
        me,
        sched: Arc::clone(&sched),
        variant,
        rng: Arc::new(Mutex::new(XorShift::new(seed ^ ((me as u64) << 32)))),
    };
    let given = std::thread::scope(|s| {
        for me in 0..readers {
            let stepped = vfs_of(me);
            let fs = fs.clone();
            let sched = Arc::clone(&sched);
            std::thread::Builder::new()
                .name(format!("reader {me} of {what}"))
                .spawn_scoped(s, move || {
                    let _finish = Finish(&sched, me);
                    sched.step(me);
                    let vfs: Arc<dyn Vfs<File = File>> = Arc::new(stepped);
                    reader(&vfs, &fs, &sched, me);
                })
                .unwrap();
        }
        let stepped = vfs_of(readers);
        let sched_w = Arc::clone(&sched);
        std::thread::Builder::new()
            .name(format!("writer of {what}"))
            .spawn_scoped(s, move || {
                let me = readers;
                let _finish = Finish(&sched_w, me);
                sched_w.step(me);
                for _ in 0..scans {
                    let scan = scan_leases(&stepped, Path::new(PACK), true, UUID);
                    sched_w.event(me, Event::ScanEnd(scan));
                }
            })
            .unwrap();
        sched.drive(readers + 1, pick)
    });
    let events = std::mem::take(&mut lock(&sched.state).events);
    check(
        what,
        &Run {
            events,
            readers,
            scans,
            fs,
        },
    );
    given
}

fn reader(vfs: &Arc<dyn Vfs<File = File>>, fs: &SimFs, sched: &Sched, me: usize) {
    let mut lease = match ReaderLease::take_in(vfs, Path::new(PACK), Path::new(DB)) {
        Ok(lease) => lease,
        Err(e @ Error::LeaseContended { .. }) => {
            sched.event(me, Event::TakeFailed(e.to_string()));
            return;
        }
        Err(e) => panic!("the take failed: {e:?}"),
    };
    assert!(lease.is_held());
    let path = lease.path().unwrap().to_owned();
    sched.event(me, Event::Held(path.clone()));
    for seq in SEQS {
        lease.set_open(UUID, seq);
        assert_eq!(
            lease.record(),
            Some(LeaseRecord::open(
                lease.record().unwrap().lease_id(),
                std::process::id(),
                UUID,
                seq
            )),
            "set_open failed"
        );
    }
    // Still its name, and its record, right before the drop (read directly: no step).
    assert_eq!(
        fs.read(&path).map(|b| LeaseRecord::decode(&b).unwrap()),
        lease.record(),
        "the held lease's file was replaced or removed"
    );
    sched.event(me, Event::DropBegin(path));
    drop(lease);
}

// --- the invariants ---

/// What a lease file held at event index `at`, as the writer must read it.
enum AtRead {
    /// The last record completely written, no write in progress.
    Whole(Vec<u8>),
    /// A rewrite from the first record to the second was in progress.
    Torn(Vec<u8>, Vec<u8>),
    /// No record was completely written yet.
    Nothing,
}

fn record_at(events: &[(usize, Event)], path: &Path, at: usize) -> AtRead {
    let mut last: Option<Vec<u8>> = None;
    let mut in_progress: Option<Vec<u8>> = None;
    for (_, event) in &events[..at] {
        match event {
            Event::WriteBegin(p, bytes) if p == path => in_progress = Some(bytes.clone()),
            Event::WriteEnd(p) if p == path => last = in_progress.take(),
            _ => {}
        }
    }
    match (last, in_progress) {
        (Some(last), None) => AtRead::Whole(last),
        (Some(old), Some(new)) => AtRead::Torn(old, new),
        (None, _) => AtRead::Nothing,
    }
}

fn check(what: &str, run: &Run) {
    let events = &run.events;
    let writer = run.readers;
    let held: Vec<(usize, usize, PathBuf, Option<usize>)> = events
        .iter()
        .enumerate()
        .filter_map(|(at, (actor, e))| match e {
            Event::Held(path) => {
                let drop_at = events.iter().position(|(a, e)| {
                    *a == *actor && matches!(e, Event::DropBegin(p) if p == path)
                });
                Some((*actor, at, path.clone(), drop_at))
            }
            _ => None,
        })
        .collect();
    let trace = || {
        events
            .iter()
            .enumerate()
            .map(|(i, (a, e))| format!("{i:3} actor {a}: {e:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // 1. A held lease, or its sidecar, is never removed by the writer before its drop.
    for (reader, _, path, drop_at) in &held {
        let sidecar = PathBuf::from(format!("{}.lock", path.display()));
        let until = drop_at.unwrap_or(events.len());
        for (at, (actor, e)) in events[..until].iter().enumerate() {
            if let Event::Removed(p) = e {
                assert!(
                    !(*actor == writer && (p == path || *p == sidecar)),
                    "{what}: the writer removed reader {reader}'s held lease at event {at}\n{}",
                    trace()
                );
            }
        }
    }

    // 2. Every live lease of every scan holds the record of the writer's read: a lease the writer finds locked always
    //    holds a whole record (written before the lock), and only a rewrite in progress may read as unreadable. A scan
    //    that lists after a take returned and ends before its drop finds that lease live.
    let mut listed_at = None;
    for (at, (actor, e)) in events.iter().enumerate() {
        if *actor != writer {
            continue;
        }
        match e {
            Event::Listed => listed_at = Some(at),
            Event::ScanEnd(scan) => {
                let listed = listed_at.take();
                for found in &scan.live {
                    let path = &found.path;
                    let read_at = (listed.unwrap_or(0)..at)
                        .rev()
                        .find(|&i| {
                            events[i].0 == writer
                                && matches!(&events[i].1, Event::Read(p) if p == path)
                        })
                        .unwrap_or_else(|| {
                            panic!("{what}: live without a read: {found:?}\n{}", trace())
                        });
                    let decoded = |b: &[u8]| LiveLease::Record(LeaseRecord::decode(b).unwrap());
                    match record_at(events, path, read_at) {
                        AtRead::Whole(bytes) => assert_eq!(
                            found.lease,
                            decoded(&bytes),
                            "{what}: the record of {}, read at event {read_at}\n{}",
                            path.display(),
                            trace()
                        ),
                        AtRead::Torn(old, new) => assert!(
                            matches!(found.lease, LiveLease::Unreadable(_))
                                || found.lease == decoded(&old)
                                || found.lease == decoded(&new),
                            "{what}: a torn read of {} gave {:?}\n{}",
                            path.display(),
                            found.lease,
                            trace()
                        ),
                        AtRead::Nothing => panic!(
                            "{what}: the writer found {} locked before a whole record was written\n{}",
                            path.display(),
                            trace()
                        ),
                    }
                }
                for (reader, held_at, path, drop_at) in &held {
                    let covered =
                        listed.is_some_and(|l| *held_at < l) && drop_at.is_none_or(|d| d > at);
                    assert!(
                        !covered || scan.live.iter().any(|s| s.path == *path),
                        "{what}: a scan missed reader {reader}'s held lease {}: {scan:?}\n{}",
                        path.display(),
                        trace()
                    );
                }
            }
            _ => {}
        }
    }

    // 3. Nothing is left behind.
    let left = run.fs.read_dir(&leases_dir(Path::new(PACK))).map_or_else(
        |e| {
            assert_eq!(e.kind(), io::ErrorKind::NotFound, "{what}");
            Vec::new()
        },
        |entries| entries.into_iter().map(|e| e.name).collect(),
    );
    assert!(left.is_empty(), "{what}: left {left:?}\n{}", trace());

    // 4. With at most two scans, every take is held.
    if run.scans <= 2 {
        let failed: Vec<&str> = events
            .iter()
            .filter_map(|(_, e)| match e {
                Event::TakeFailed(why) => Some(why.as_str()),
                _ => None,
            })
            .collect();
        assert!(failed.is_empty(), "{what}: {failed:?}\n{}", trace());
        assert_eq!(held.len(), run.readers, "{what}");
    }
}

// --- the schedules ---

const VARIANTS: [Variant; 4] = [
    Variant {
        sidecars: false,
        torn: false,
    },
    Variant {
        sidecars: true,
        torn: false,
    },
    Variant {
        sidecars: false,
        torn: true,
    },
    Variant {
        sidecars: true,
        torn: true,
    },
];

/// Every schedule with at most two switches: the reader (actor 0) for `a` steps, the writer for `b`, the reader to its
/// end, the writer to its end; for every `a` up to the reader's length and every `b` up to the writer's. Returns how
/// many schedules ran.
fn every_two_switch_schedule(variant: Variant, seed: u64) -> u64 {
    let mut schedules = 0;
    for a in 0_u64..500 {
        let mut reader_ended_early = false;
        for b in 0_u64.. {
            let what = format!("{variant:?}, reader {a} steps, writer {b} steps, seed {seed}");
            let given = run(&what, variant, 1, 1, seed, |waiting, given| {
                let (reader, writer) = (waiting.contains(&0), waiting.contains(&1));
                if given[0] < a && reader {
                    0
                } else if given[1] < b && writer {
                    1
                } else {
                    usize::from(!reader)
                }
            });
            schedules += 1;
            reader_ended_early = given[0] < a;
            // The writer ended within its `b` steps, or the reader within its `a`: later `b` add nothing.
            if given[1] < b || reader_ended_early {
                break;
            }
            assert!(b < 500, "{what}: the writer never ends");
        }
        if reader_ended_early {
            return schedules;
        }
    }
    panic!("{variant:?}, seed {seed}: the reader never ends")
}

#[test]
fn every_schedule_with_two_switches_keeps_the_held_lease() {
    // Covers the scan at every step of a take, a rewrite and a drop (the create-to-lock window, the lock-to-name
    // check, the drop's removal to its unlock, a rewrite half written), and a take, rewrite or drop at every step of a
    // scan of a lease held before it listed.
    for variant in VARIANTS {
        // Torn writes cut at random places: more seeds for those.
        let seeds: &[u64] = match (variant.torn, cfg!(feature = "slow")) {
            (false, _) => &[1],
            (true, false) => &[1, 2],
            (true, true) => &[1, 2, 3, 4, 5, 6, 7, 8],
        };
        for &seed in seeds {
            let schedules = every_two_switch_schedule(variant, seed);
            assert!(schedules > 50, "{variant:?}: only {schedules} schedules");
        }
    }
}

#[test]
fn random_schedules_of_two_readers_and_two_scans_keep_every_held_lease() {
    let cases: u64 = if cfg!(feature = "slow") { 4000 } else { 160 };
    for case in 0..cases {
        let seed = 0x5EED_0000 + case;
        let variant = VARIANTS[(case % 4) as usize];
        let mut rng = XorShift::new(seed);
        // How often the scheduler switches actors: rarely, half the time, or almost always.
        let switch = [8_u64, 50, 92][(rng.next() % 3) as usize];
        let mut current = 0;
        let what = format!("random schedule, seed {seed}, {variant:?}, switch {switch}%");
        run(&what, variant, 2, 2, seed, |waiting, _| {
            if !waiting.contains(&current) || rng.next() % 100 < switch {
                current = waiting[(rng.next() % waiting.len() as u64) as usize];
            }
            current
        });
    }
}

#[test]
fn three_scans_may_exhaust_a_take_but_never_leave_a_file() {
    // With more scans than attempts a take may fail (`LeaseContended`); invariants 1 to 3 still hold.
    let cases: u64 = if cfg!(feature = "slow") { 2000 } else { 80 };
    for case in 0..cases {
        let seed = 0xC0DE_0000 + case;
        let variant = VARIANTS[(case % 4) as usize];
        let mut rng = XorShift::new(seed);
        let what = format!("three scans, seed {seed}, {variant:?}");
        run(&what, variant, 1, 3, seed, |waiting, _| {
            waiting[(rng.next() % waiting.len() as u64) as usize]
        });
    }
}

/// ADR-0054 item 6: a rewrite that fails is logged and the handle goes on. Failed without effect, the file keeps the
/// older seq (GC is held back); applied then failed, the file names the new seq the handle already holds. Either
/// way GC keeps more, never less, and the handle's own view stays the last record it knows was written.
#[test]
fn a_failed_rewrite_keeps_more_never_less() {
    use persia_storage::container::sim::SimFault;
    for (fault, on_disk) in [
        (SimFault::FailAt as fn(u64) -> SimFault, 5),
        (SimFault::ApplyThenFail, 6),
    ] {
        let fs = SimFs::new();
        fs.create_dir(Path::new("/db")).unwrap();
        drop(Container::create_in(Arc::new(fs.clone()), DB).unwrap());
        let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
        let mut lease = ReaderLease::take_in(&vfs, Path::new(PACK), Path::new(DB)).unwrap();
        lease.set_open(UUID, 5);
        let id = lease.record().unwrap().lease_id();
        let path = lease.path().unwrap().to_owned();
        fs.set_fault(Some(fault(fs.mutations())));
        lease.set_open(UUID, 6);
        assert_eq!(
            lease.record().unwrap().manifest_seq(),
            Some(5),
            "{on_disk}: the handle's view"
        );
        let disk = LeaseRecord::decode(&fs.read(&path).unwrap()).unwrap();
        assert_eq!(
            disk,
            LeaseRecord::open(id, std::process::id(), UUID, on_disk)
        );
        fs.set_fault(None);
        let report = scan_leases(&*vfs, Path::new(PACK), true, UUID).judge(6);
        assert_eq!((report.live, report.stale_removed), (1, 0), "{report:?}");
        assert_eq!(report.held_back, on_disk != 6, "{report:?}");
    }
}
