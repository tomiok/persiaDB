//! An in-memory [`ContainerFile`] that models what survives a crash, for crash-safety tests of the container
//! (ROADMAP 2.4.3.1).
//!
//! The file keeps two images: what readers see now, and what is durable (as of the last successful
//! [`sync_data`](ContainerFile::sync_data)). A fault fails one mutating call and, like a process death, every later
//! one ([`SimFault`]). [`SimFile::crash`] then produces the file a restart would find: the durable image plus
//! some, all or none of the unsynced writes, at sector granularity ([`CrashLoss`]).

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::file::ContainerFile;
use crate::retry::Jitter;

/// The unit in which a [`CrashLoss::Seeded`] crash keeps or loses unsynced writes: a disk writes a sector whole or
/// not at all.
pub const SECTOR: u64 = 512;

/// Which mutating call (`write_all_at`, `set_len`, `sync_data`, counted from 0) fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimFault {
    /// Call `n` fails without effect.
    FailAt(u64),
    /// Call `n` takes effect, then reports an error (a write or sync whose outcome the caller cannot know).
    ApplyThenFail(u64),
}

/// What a [`SimFile::crash`] keeps of the writes since the last successful sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashLoss {
    /// Everything unsynced is lost.
    All,
    /// Everything unsynced survives, as if the OS flushed it before the crash.
    Nothing,
    /// A seeded subset survives: each sector of each unsynced write, and each length change, independently; the
    /// file may keep its grown length with zeros where data was lost (SPEC §4.4's torn-tail model) or go back to its
    /// synced length. Deterministic for a seed.
    Seeded(u64),
}

/// An unsynced change, in call order.
#[derive(Debug, Clone)]
enum Pending {
    Write { offset: u64, data: Vec<u8> },
    SetLen(u64),
}

#[derive(Debug, Default)]
struct Inner {
    current: Vec<u8>,
    durable: Vec<u8>,
    pending: Vec<Pending>,
    mutations: u64,
    fault: Option<SimFault>,
    crashed: bool,
}

/// An in-memory file with crash semantics. Clones share the file.
///
/// ```
/// use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
/// use persia_storage::container::ContainerFile;
///
/// let file = SimFile::new();
/// file.write_all_at(b"synced", 0)?;
/// file.sync_data()?;
/// file.write_all_at(b"!", 6)?;
/// file.set_fault(Some(SimFault::FailAt(file.mutations())));
/// assert!(file.sync_data().is_err()); // the process dies here
/// assert!(file.write_all_at(b"?", 7).is_err());
/// assert_eq!(file.crash(CrashLoss::All).bytes(), b"synced");
/// assert_eq!(file.crash(CrashLoss::Nothing).bytes(), b"synced!");
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Clone, Default)]
pub struct SimFile {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for SimFile {
    // Sizes, not contents: a container is at least 12 KiB.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("SimFile")
            .field("len", &inner.current.len())
            .field("durable_len", &inner.durable.len())
            .field("unsynced", &inner.pending.len())
            .field("mutations", &inner.mutations)
            .field("fault", &inner.fault)
            .field("crashed", &inner.crashed)
            .finish()
    }
}

impl SimFile {
    /// An empty file.
    pub fn new() -> Self {
        Self::default()
    }

    /// A file holding `bytes`, all durable.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let inner = Inner {
            durable: bytes.clone(),
            current: bytes,
            ..Inner::default()
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves consistent data: every update is a single assignment or push.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Arms (or with `None` disarms) a fault. Mutating calls are counted from the file's creation.
    pub fn set_fault(&self, fault: Option<SimFault>) {
        self.lock().fault = fault;
    }

    /// How many mutating calls the file has seen, failed ones included: the `n` of the next call.
    pub fn mutations(&self) -> u64 {
        self.lock().mutations
    }

    /// Whether a fault has fired.
    pub fn crashed(&self) -> bool {
        self.lock().crashed
    }

    /// What readers see now.
    pub fn bytes(&self) -> Vec<u8> {
        self.lock().current.clone()
    }

    /// The bytes as of the last successful sync.
    pub fn durable_bytes(&self) -> Vec<u8> {
        self.lock().durable.clone()
    }

    /// The file a restart finds after a crash now: a new, independent, fully synced file with no fault armed.
    #[must_use]
    pub fn crash(&self, loss: CrashLoss) -> Self {
        let inner = self.lock();
        let image = match loss {
            CrashLoss::All => inner.durable.clone(),
            CrashLoss::Nothing => inner.current.clone(),
            CrashLoss::Seeded(seed) => seeded_image(&inner, &Jitter::new(seed)),
        };
        Self::from_bytes(image)
    }

    /// Counts a mutating call and applies the armed fault: `Ok(true)` to run it and succeed, `Ok(false)` to run it
    /// and then fail, `Err` to fail without running it.
    fn begin(inner: &mut Inner) -> io::Result<bool> {
        let n = inner.mutations;
        inner.mutations = n.saturating_add(1);
        if inner.crashed {
            return Err(io::Error::other("simulated crash: the process is gone"));
        }
        match inner.fault {
            Some(SimFault::FailAt(k)) if k == n => {
                inner.crashed = true;
                Err(io::Error::other(format!("simulated failure of call {n}")))
            }
            Some(SimFault::ApplyThenFail(k)) if k == n => {
                inner.crashed = true;
                Ok(false)
            }
            _ => Ok(true),
        }
    }

    fn finish(n_ok: bool) -> io::Result<()> {
        if n_ok {
            Ok(())
        } else {
            Err(io::Error::other(
                "simulated failure after the call took effect",
            ))
        }
    }
}

/// `durable` plus a seeded subset of the pending changes, applied in call order.
fn seeded_image(inner: &Inner, rng: &Jitter) -> Vec<u8> {
    let coin = || rng.next_u64() & 1 == 1;
    let mut image = inner.durable.clone();
    for change in &inner.pending {
        match change {
            Pending::SetLen(len) => {
                if coin() {
                    image.resize(to_usize(*len), 0);
                }
            }
            Pending::Write { offset, data } => {
                let mut at = *offset;
                let mut rest = data.as_slice();
                while !rest.is_empty() {
                    // Up to the next sector boundary.
                    let room = SECTOR.saturating_sub(at % SECTOR);
                    let take = rest.len().min(to_usize(room));
                    let (piece, tail) = rest.split_at(take);
                    if coin() {
                        write_into(&mut image, at, piece);
                    }
                    at = at.saturating_add(take as u64);
                    rest = tail;
                }
            }
        }
    }
    // The length may have reached the disk before the data did: lost sectors then read as zeros.
    if inner.current.len() > image.len() && coin() {
        image.resize(inner.current.len(), 0);
    }
    image
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn write_into(image: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let start = to_usize(offset);
    let end = start.saturating_add(data.len());
    if image.len() < end {
        image.resize(end, 0);
    }
    if let Some(dst) = image.get_mut(start..end) {
        dst.copy_from_slice(data);
    }
}

impl ContainerFile for SimFile {
    fn file_len(&self) -> io::Result<u64> {
        Ok(self.lock().current.len() as u64)
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let inner = self.lock();
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let src = start
            .checked_add(buf.len())
            .and_then(|end| inner.current.get(start..end))
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut inner = self.lock();
        let ok = Self::begin(&mut inner)?;
        if offset
            .checked_add(buf.len() as u64)
            .and_then(|end| usize::try_from(end).ok())
            .is_none()
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        write_into(&mut inner.current, offset, buf);
        inner.pending.push(Pending::Write {
            offset,
            data: buf.to_vec(),
        });
        Self::finish(ok)
    }

    fn sync_data(&self) -> io::Result<()> {
        let mut inner = self.lock();
        let ok = Self::begin(&mut inner)?;
        let Inner {
            current, durable, ..
        } = &mut *inner;
        durable.clone_from(current);
        inner.pending.clear();
        Self::finish(ok)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut inner = self.lock();
        let ok = Self::begin(&mut inner)?;
        let new_len = usize::try_from(len).map_err(|_| io::ErrorKind::InvalidInput)?;
        inner.current.resize(new_len, 0);
        inner.pending.push(Pending::SetLen(len));
        Self::finish(ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_see_unsynced_writes() {
        let f = SimFile::new();
        f.write_all_at(b"abc", 2).unwrap();
        assert_eq!(f.bytes(), b"\0\0abc");
        let mut buf = [0; 2];
        f.read_exact_at(&mut buf, 3).unwrap();
        assert_eq!(&buf, b"bc");
        assert_eq!(
            f.read_exact_at(&mut buf, 4).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(f.file_len().unwrap(), 5);
        assert_eq!(f.durable_bytes(), b"");
    }

    #[test]
    fn sync_makes_writes_and_truncation_durable() {
        let f = SimFile::new();
        f.write_all_at(b"abcdef", 0).unwrap();
        f.set_len(4).unwrap();
        f.sync_data().unwrap();
        assert_eq!(f.durable_bytes(), b"abcd");
        assert_eq!(f.crash(CrashLoss::All).bytes(), b"abcd");
        assert_eq!(f.mutations(), 3);
    }

    #[test]
    fn faults_are_sticky() {
        for (fault, applied) in [
            (SimFault::FailAt(1), false),
            (SimFault::ApplyThenFail(1), true),
        ] {
            let f = SimFile::new();
            f.set_fault(Some(fault));
            f.write_all_at(b"a", 0).unwrap();
            assert!(f.write_all_at(b"b", 1).is_err());
            assert!(f.crashed());
            assert_eq!(f.bytes().len(), if applied { 2 } else { 1 });
            assert!(f.sync_data().is_err());
            assert!(f.set_len(0).is_err());
            assert!(
                f.durable_bytes().is_empty(),
                "the failed sync made nothing durable"
            );
            // Reads still work: the image is what a crash would start from.
            assert_eq!(f.file_len().unwrap(), f.bytes().len() as u64);
        }
    }

    #[test]
    fn crash_images_are_independent_and_deterministic() {
        let f = SimFile::new();
        f.write_all_at(&[1; 600], 0).unwrap();
        f.sync_data().unwrap();
        f.write_all_at(&[2; 1500], 300).unwrap();
        f.set_len(1700).unwrap();
        let a = f.crash(CrashLoss::Seeded(7)).bytes();
        assert_eq!(a, f.crash(CrashLoss::Seeded(7)).bytes());
        let after = f.crash(CrashLoss::Nothing);
        after.write_all_at(b"x", 0).unwrap();
        assert_eq!(f.bytes()[0], 1, "a crash image does not share the file");
        assert_eq!(after.mutations(), 1);
    }

    #[test]
    fn seeded_crashes_keep_whole_sectors_of_old_or_new_data() {
        let f = SimFile::new();
        f.write_all_at(&[1; 1024], 0).unwrap();
        f.sync_data().unwrap();
        f.write_all_at(&[2; 1100], 200).unwrap(); // sectors 0 (part), 1, 2 (part)
        let mut seen = std::collections::HashSet::new();
        for seed in 0..64 {
            let img = f.crash(CrashLoss::Seeded(seed)).bytes();
            assert!(img.len() == 1024 || img.len() == 1300, "{}", img.len());
            // Each sector-aligned piece of the write is all old (1, or 0 past the synced end) or all new (2).
            for piece in [200..512, 512..1024, 1024..1300] {
                let Some(bytes) = img.get(piece.clone()) else {
                    continue;
                };
                let new = bytes.iter().all(|&b| b == 2);
                let old = bytes.iter().all(|&b| b == 1 || b == 0);
                assert!(new || old, "seed {seed} {piece:?}");
                seen.insert((piece.start, new));
            }
            assert!(img[..200].iter().all(|&b| b == 1));
        }
        assert_eq!(
            seen.len(),
            6,
            "every piece is both kept and lost by some seed"
        );
    }
}
