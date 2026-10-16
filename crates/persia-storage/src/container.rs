//! The single-file container (SPEC §4): a header, two superblock slots and an append-only log of frames.
//!
//! [`Container::create`] writes a new container without ever replacing a file, [`Container::open`] recovers one
//! (SPEC §4.4, §8.6): it picks the active superblock, checks it against the file, validates the committed frames
//! from `wal_start`, and scans the frames after `log_end`, truncating a torn tail. A writer then
//! [`append`](Container::append)s frames, makes them durable with one [`sync`](Container::sync) per group, and
//! publishes them with [`commit`](Container::commit), which writes the inactive slot (SPEC §4.5).
//!
//! A read-write open holds an exclusive lock on the file (SPEC §8.4). Any failed write or sync poisons the handle:
//! what reached the disk is unknown until the container is reopened ([`Error::Poisoned`]).

mod error;
pub mod file;
pub mod sim;

use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use persia_format::block::Compression;
use persia_format::frame::{
    FRAME_HEADER_LEN, Frame, FrameHeader, FrameKind, FrameScanner, ScanEnd, encode_frame,
};
use persia_format::header::{Access, HEADER_LEN, Header, KNOWN_ROCOMPAT_FLAGS};
use persia_format::superblock::{
    ActiveSuperblock, FIRST_FRAME_OFFSET, SLOTS_LEN, Slot, Superblock,
};
use persia_format::{Corruption, Reader, Writer};

pub use error::{Error, ReadOnlyReason, Result};
pub use file::ContainerFile;

use crate::local::fs::{parent_or_cwd, sync_dir};
use crate::local::temp_nonce;

/// Distinguishes the temporary files of one process's [`Container::create`] calls.
static CREATE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How a container is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Takes the writer lock, truncates a torn tail, and allows writes.
    ReadWrite,
    /// Never writes, truncates or locks: the committed state as of the open, plus the valid frames after it.
    ReadOnly,
}

/// Where a frame is, and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRef {
    /// Absolute offset of the frame.
    pub offset: u64,
    /// Total length of the frame (header, padded payload, trailer).
    pub len: u64,
    /// Its kind.
    pub kind: FrameKind,
    /// Its `frame_seq` (SPEC §4.4).
    pub frame_seq: u64,
}

impl FrameRef {
    /// The offset just past the frame.
    pub fn end(&self) -> u64 {
        // Fits: the frame was read from, or written to, a file.
        self.offset.saturating_add(self.len)
    }

    fn of(frame: &Frame<'_>) -> Self {
        Self {
            offset: frame.offset(),
            len: frame.total_len(),
            kind: frame.kind(),
            frame_seq: frame.frame_seq(),
        }
    }
}

/// A frame read back by [`Container::read_frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePayload {
    /// Where the frame is.
    pub frame: FrameRef,
    /// The payload, decompressed if the frame was compressed.
    pub payload: Vec<u8>,
}

/// What a commit publishes (SPEC §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    /// The Manifest frame, appended through this handle or found by its recovery.
    pub manifest: FrameRef,
    /// The first WAL frame still needed: the start of a frame from the current `wal_start` on, or the end of the
    /// log. It never moves back.
    pub wal_start: u64,
    /// The writer's fencing token (SPEC §11.3); 0 for a local database.
    pub fence_token: u64,
}

/// What recovery found when the container was opened (SPEC §4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    /// How the scan from the committed `log_end` ended.
    pub scan_end: ScanEnd,
    /// The file length before a read-write open truncated a torn tail, if it did.
    pub truncated_from: Option<u64>,
}

/// An open container file (SPEC §4).
///
/// ```
/// use persia_format::block::Compression;
/// use persia_format::frame::FrameKind;
/// use persia_storage::container::{Commit, Container, Mode};
///
/// # let dir = tempfile::tempdir()?;
/// let path = dir.path().join("app.persia");
/// let mut c = Container::create(&path)?;
/// let wal = c.append(FrameKind::WalChunk, b"put doc 1", Compression::None)?;
/// c.sync()?; // step 1 of SPEC §4.5
/// let manifest = c.append(FrameKind::Manifest, b"manifest 1", Compression::None)?;
/// c.commit(Commit { manifest, wal_start: wal.offset, fence_token: 0 })?;
/// drop(c);
///
/// let c = Container::open(&path, Mode::ReadOnly)?;
/// assert_eq!(c.superblock().seq, 1);
/// let m = c.read_frame(c.superblock().manifest_off, 1 << 20)?;
/// assert_eq!(m.payload, b"manifest 1");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Container<F = File> {
    file: F,
    header: Header,
    mode: Mode,
    active: ActiveSuperblock,
    /// Where the next frame goes: the end of the valid frames.
    end: u64,
    next_frame_seq: u64,
    /// Every frame from the committed `wal_start` to `end`.
    frames: Vec<FrameRef>,
    /// Appends not yet synced.
    dirty: bool,
    poisoned: bool,
    recovery: Recovery,
}

impl Container<File> {
    /// Creates a new container at `path` and opens it read-write, holding the writer lock.
    ///
    /// Never truncates or replaces a file (SPEC §4.7): the container is written to a temporary file in the same
    /// directory, synced, and hard-linked to `path`, which fails if the name exists; then the temporary name is
    /// removed and the directory synced. A crash leaves no container, or a complete one, at `path` (and possibly a
    /// stale temporary file, `<name>.tmp.<nonce>.<n>`). The file system must support hard links.
    ///
    /// # Errors
    /// [`Error::AlreadyExists`] if `path` exists, or [`Error::Io`].
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let dir = parent_or_cwd(path);
        let name = path
            .file_name()
            .ok_or_else(|| Error::InvalidInput(format!("{} has no file name", path.display())))?;
        let n = CREATE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = name.to_owned();
        temp_name.push(format!(".tmp.{:016x}.{n}", temp_nonce()));
        let temp = dir.join(temp_name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(Error::io("create"))?;
        let result = lock(&file).and_then(|()| {
            let container = Self::create_on(file, new_db_uuid())?;
            match std::fs::hard_link(&temp, path) {
                Ok(()) => Ok(container),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    Err(Error::AlreadyExists {
                        path: path.to_owned(),
                    })
                }
                Err(err) => Err(Error::io("link")(err)),
            }
        });
        // The temporary name goes whatever happened; a failed cleanup only leaves a stale file.
        let removed = std::fs::remove_file(&temp).map_err(Error::io("remove temporary file"));
        let container = result?;
        removed?;
        sync_dir(dir).map_err(Error::io("sync directory"))?;
        Ok(container)
    }

    /// Opens the container at `path` and recovers it (SPEC §4.4, §8.6). [`Mode::ReadWrite`] takes the writer lock.
    ///
    /// # Errors
    /// [`Error::Locked`] if another handle holds the writer lock, and the errors of [`Container::open_on`].
    pub fn open(path: impl AsRef<Path>, mode: Mode) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(mode == Mode::ReadWrite)
            .open(path)
            .map_err(Error::io("open"))?;
        if mode == Mode::ReadWrite {
            lock(&file)?;
        }
        Self::open_on(file, mode)
    }
}

/// Takes the exclusive writer lock on `file` without waiting; it is released when the file is closed.
fn lock(file: &File) -> Result<()> {
    file.try_lock().map_err(|err| match err {
        std::fs::TryLockError::WouldBlock => Error::Locked,
        std::fs::TryLockError::Error(err) => Error::io("lock")(err),
    })
}

/// A random database id: 128 bits from the standard library's per-process hash keys, the time and the pid. Ids
/// tell databases apart (SPEC §4.2); they are not secrets.
fn new_db_uuid() -> [u8; 16] {
    let half = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u32(std::process::id());
        if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            h.write_u128(t.as_nanos());
        }
        h.finish().to_le_bytes()
    };
    let mut id = [0; 16];
    let (lo, hi) = id.split_at_mut(8);
    lo.copy_from_slice(&half(1));
    hi.copy_from_slice(&half(2));
    id
}

impl<F: ContainerFile> Container<F> {
    /// Writes a new container into `file`, which must be empty, and opens it read-write. The caller holds whatever
    /// lock the file needs.
    ///
    /// Writes the header and both slots in the new-container state of SPEC §4.3, then syncs.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `file` is not empty, or [`Error::Io`].
    pub fn create_on(file: F, db_uuid: [u8; 16]) -> Result<Self> {
        let len = file.file_len().map_err(Error::io("stat"))?;
        if len != 0 {
            return Err(Error::InvalidInput(format!(
                "a container is created in an empty file, not one of {len} bytes"
            )));
        }
        let header = Header::new(db_uuid);
        let superblock = Superblock {
            log_end: FIRST_FRAME_OFFSET,
            wal_start: FIRST_FRAME_OFFSET,
            next_frame_seq: 1,
            ..Superblock::default()
        };
        let mut w = Writer::new();
        header.encode(&mut w)?;
        superblock.encode(&mut w)?;
        superblock.encode(&mut w)?;
        file.write_all_at(w.as_slice(), 0)
            .map_err(Error::io("write header"))?;
        file.sync_data().map_err(Error::io("sync"))?;
        Ok(Self {
            file,
            header,
            mode: Mode::ReadWrite,
            active: ActiveSuperblock {
                slot: Slot::A,
                superblock,
            },
            end: FIRST_FRAME_OFFSET,
            next_frame_seq: 1,
            frames: Vec::new(),
            dirty: false,
            poisoned: false,
            recovery: Recovery {
                scan_end: ScanEnd::Clean {
                    offset: FIRST_FRAME_OFFSET,
                },
                truncated_from: None,
            },
        })
    }

    /// Opens the container in `file` and recovers it (SPEC §4.4, §8.6). The caller holds whatever lock the mode
    /// needs.
    ///
    /// 1. Validates the header (SPEC §4.2). Unknown `rocompat_flags` allow only [`Mode::ReadOnly`].
    /// 2. Picks the active superblock slot (SPEC §4.3) and checks its offsets against the file length.
    /// 3. Checks that `manifest_off` holds a Manifest frame of `manifest_len` bytes (after a commit).
    /// 4. Validates the committed frames of `wal_start..log_end`: valid, ending exactly at `log_end`, and one
    ///    `frame_seq` run ending at `next_frame_seq - 1` (ADR-0013).
    /// 5. Scans from `log_end`, expecting `frame_seq = next_frame_seq` there. A torn tail is truncated (read-write:
    ///    physically, then synced; read-only: logically).
    ///
    /// # Errors
    /// [`Error::Format`] for corruption up to `log_end` or an unsupported format or frame, [`Error::CorruptTail`]
    /// for a read-write open whose scan from `log_end` ends in corruption (ROADMAP 2.4.10), [`Error::ReadOnly`] for
    /// a read-write open of a container with unknown `rocompat_flags`, or [`Error::Io`].
    pub fn open_on(file: F, mode: Mode) -> Result<Self> {
        let file_len = file.file_len().map_err(Error::io("stat"))?;
        let prefix = read_at(&file, 0, file_len.min(FIRST_FRAME_OFFSET))?;
        let mut r = Reader::new(&prefix);
        let header = Header::decode(&mut r)?;
        if mode == Mode::ReadWrite && header.access() == Access::ReadOnly {
            return Err(Error::ReadOnly {
                reason: ReadOnlyReason::UnknownRocompatFlags {
                    flags: header.rocompat_flags & !KNOWN_ROCOMPAT_FLAGS,
                },
            });
        }
        debug_assert_eq!(r.position(), HEADER_LEN);
        let active = Superblock::read_active(&mut r)?;
        debug_assert_eq!(r.position(), HEADER_LEN + SLOTS_LEN);
        active.check_bounds(file_len)?;
        let sb = active.superblock;
        if sb.seq != 0 {
            check_manifest(&file, &sb)?;
        }
        let mut frames = committed_frames(&file, &sb)?;
        let mut container = Self {
            file,
            header,
            mode,
            active,
            end: sb.log_end,
            next_frame_seq: sb.next_frame_seq,
            frames: Vec::new(),
            dirty: false,
            poisoned: false,
            recovery: Recovery {
                scan_end: ScanEnd::Clean { offset: sb.log_end },
                truncated_from: None,
            },
        };
        container.recover_tail(file_len, &mut frames)?;
        container.frames = frames;
        Ok(container)
    }

    /// Scans from `log_end` to `file_len` (SPEC §4.4) and appends the valid frames to `frames`.
    fn recover_tail(&mut self, file_len: u64, frames: &mut Vec<FrameRef>) -> Result<()> {
        let log_end = self.active.superblock.log_end;
        let tail = read_at(&self.file, log_end, file_len.saturating_sub(log_end))?;
        let mut scanner = FrameScanner::new(&tail, log_end).expect_frame_seq(self.next_frame_seq);
        frames.extend(scanner.by_ref().map(|f| FrameRef::of(&f)));
        let end = scanner
            .end()
            .cloned()
            .unwrap_or(ScanEnd::Clean { offset: file_len });
        self.next_frame_seq = scanner.next_frame_seq().unwrap_or(self.next_frame_seq);
        self.end = end.offset();
        match &end {
            ScanEnd::Clean { .. } => {}
            ScanEnd::TornTail { offset, .. } => {
                if self.mode == Mode::ReadWrite {
                    // Before anything is appended, so frames that followed `offset` can never reappear behind new
                    // ones (SPEC §4.4). Never below `log_end`, so mapped bytes stay valid (SPEC §4.7).
                    self.file
                        .set_len(*offset)
                        .map_err(Error::io("truncate torn tail"))?;
                    self.file.sync_data().map_err(Error::io("sync"))?;
                    self.recovery.truncated_from = Some(file_len);
                }
            }
            ScanEnd::Corrupt { offset, error } => {
                if self.mode == Mode::ReadWrite {
                    return Err(Error::CorruptTail {
                        offset: *offset,
                        source: error.clone(),
                    });
                }
            }
            ScanEnd::Unsupported { error, .. } => return Err(Error::Format(error.clone())),
        }
        self.recovery.scan_end = end;
        Ok(())
    }

    /// The container header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// How the container was opened.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The committed superblock: the state a crash now would recover.
    pub fn superblock(&self) -> &Superblock {
        &self.active.superblock
    }

    /// Which slot holds the committed superblock; the next commit writes the other (SPEC §4.5).
    pub fn active_slot(&self) -> Slot {
        self.active.slot
    }

    /// The end of the valid frames: where the next one goes.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// The `frame_seq` the next appended frame gets.
    pub fn next_frame_seq(&self) -> u64 {
        self.next_frame_seq
    }

    /// Every frame from the committed `wal_start` to [`Container::end`], in order. Those from `log_end` on are
    /// durable or appended but not committed: WAL frames synced after the last commit (SPEC §8.2), or the frames
    /// of a commit that did not complete.
    pub fn frames(&self) -> &[FrameRef] {
        &self.frames
    }

    /// The frames after the committed `log_end` (see [`Container::frames`]).
    pub fn uncommitted_frames(&self) -> &[FrameRef] {
        let log_end = self.active.superblock.log_end;
        let first = self.frames.partition_point(|f| f.offset < log_end);
        self.frames.get(first..).unwrap_or_default()
    }

    /// What recovery found when the container was opened.
    pub fn recovery(&self) -> &Recovery {
        &self.recovery
    }

    /// Whether appends since the last [`Container::sync`] are waiting for it.
    pub fn has_unsynced(&self) -> bool {
        self.dirty
    }

    /// The underlying file.
    pub fn file(&self) -> &F {
        &self.file
    }

    fn writable(&self) -> Result<()> {
        if self.mode == Mode::ReadOnly {
            return Err(Error::ReadOnly {
                reason: ReadOnlyReason::OpenedReadOnly,
            });
        }
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        Ok(())
    }

    /// Runs a write; a failure poisons the handle, since what reached the file is then unknown.
    fn write_op(&mut self, op: &'static str, f: impl FnOnce(&F) -> io::Result<()>) -> Result<()> {
        f(&self.file).map_err(|err| {
            self.poisoned = true;
            Error::io(op)(err)
        })
    }

    /// Appends one frame at [`Container::end`] with the next `frame_seq`. Not durable until [`Container::sync`];
    /// not visible to a reopen until committed or synced.
    ///
    /// # Errors
    /// [`Error::ReadOnly`], [`Error::Poisoned`], [`Error::Encode`] (bad compression level),
    /// [`Error::InvalidInput`] once `frame_seq` or the file offset would be exhausted, or [`Error::Io`] (which
    /// poisons the handle).
    pub fn append(
        &mut self,
        kind: FrameKind,
        payload: &[u8],
        compression: Compression,
    ) -> Result<FrameRef> {
        self.writable()?;
        let frame_seq = self.next_frame_seq;
        // Fails exactly when `frame_seq = u64::MAX`, which is never written (SPEC §4.4).
        let next_frame_seq = frame_seq
            .checked_add(1)
            .ok_or_else(|| Error::InvalidInput("frame_seq exhausted (ADR-0013)".into()))?;
        let mut w = Writer::with_base_offset(self.end);
        encode_frame(&mut w, kind, frame_seq, payload, compression)?;
        let len = w.len() as u64;
        let end = self
            .end
            .checked_add(len)
            .ok_or_else(|| Error::InvalidInput("container offset exhausted".into()))?;
        let offset = self.end;
        self.write_op("append", |f| f.write_all_at(w.as_slice(), offset))?;
        let frame = FrameRef {
            offset,
            len,
            kind,
            frame_seq,
        };
        self.frames.push(frame);
        self.end = end;
        self.next_frame_seq = next_frame_seq;
        self.dirty = true;
        Ok(frame)
    }

    /// Makes every frame appended so far durable with one `fdatasync` (group commit); nothing to do if none is
    /// waiting. Synced frames survive a crash even before they are committed: recovery finds them after `log_end`.
    ///
    /// # Errors
    /// [`Error::ReadOnly`], [`Error::Poisoned`], or [`Error::Io`] (which poisons the handle).
    pub fn sync(&mut self) -> Result<()> {
        self.writable()?;
        if self.dirty {
            self.write_op("sync", F::sync_data)?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Publishes every frame appended so far (SPEC §4.5): syncs them, then writes the inactive superblock slot with
    /// `seq + 1`, `log_end` = [`Container::end`] and the next `frame_seq`, and syncs again. Callers append the
    /// Segment and `WalChunk` frames and [`sync`](Container::sync) them before appending the Manifest (step 1), so a
    /// crash can tear at most the Manifest.
    ///
    /// On success the new superblock is committed. On an I/O error the handle is poisoned and the commit may or may
    /// not have happened: a reopen recovers the previous commit or this one, never a mix.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `manifest` is not a Manifest frame of this container from the committed
    /// `wal_start` on, if `wal_start` is not a frame start (or the end) at or after the committed `wal_start`, or
    /// once `seq` is exhausted; [`Error::ReadOnly`], [`Error::Poisoned`], or [`Error::Io`].
    pub fn commit(&mut self, commit: Commit) -> Result<()> {
        self.writable()?;
        let old = self.active.superblock;
        if commit.manifest.kind != FrameKind::Manifest || !self.frames.contains(&commit.manifest) {
            return Err(Error::InvalidInput(format!(
                "{:?} is not a Manifest frame of this container after wal_start",
                commit.manifest
            )));
        }
        let wal_start_ok = commit.wal_start == self.end
            || self
                .frames
                .binary_search_by_key(&commit.wal_start, |f| f.offset)
                .is_ok();
        if !wal_start_ok || commit.wal_start < old.wal_start {
            return Err(Error::InvalidInput(format!(
                "wal_start {} is not a frame start in {}..={}",
                commit.wal_start, old.wal_start, self.end
            )));
        }
        let seq = old
            .seq
            .checked_add(1)
            .ok_or_else(|| Error::InvalidInput("commit seq exhausted".into()))?;
        let superblock = Superblock {
            seq,
            manifest_off: commit.manifest.offset,
            manifest_len: commit.manifest.len,
            log_end: self.end,
            wal_start: commit.wal_start,
            fence_token: commit.fence_token,
            next_frame_seq: self.next_frame_seq,
        };
        // Step 2: the frames, Manifest included, are durable before the slot names them.
        self.sync()?;
        // Step 3.
        let slot = self.active.slot.other();
        let mut w = Writer::new();
        superblock.encode(&mut w)?;
        self.write_op("write superblock", |f| {
            f.write_all_at(w.as_slice(), slot.offset())
        })?;
        self.write_op("sync superblock", F::sync_data)?;
        self.active = ActiveSuperblock { slot, superblock };
        self.frames.retain(|f| f.offset >= commit.wal_start);
        Ok(())
    }

    /// Reads and validates the frame at `offset`, which must lie below [`Container::end`], and returns its payload,
    /// decompressed under the bomb limit `max_payload` (SPEC §4.4).
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `offset` is outside the frames, [`Error::Format`] if the frame there is invalid
    /// or its payload exceeds `max_payload`, or [`Error::Io`].
    pub fn read_frame(&self, offset: u64, max_payload: usize) -> Result<FramePayload> {
        let header_len = FRAME_HEADER_LEN as u64;
        if offset < FIRST_FRAME_OFFSET
            || offset.checked_add(header_len).is_none_or(|e| e > self.end)
        {
            return Err(Error::InvalidInput(format!(
                "no frame at {offset}: frames are in {FIRST_FRAME_OFFSET}..{}",
                self.end
            )));
        }
        let head = read_at(&self.file, offset, header_len)?;
        let header = FrameHeader::decode(&mut Reader::with_base_offset(&head, offset))?;
        header.check_fits(self.end)?;
        let bytes = read_at(&self.file, offset, header.total_len())?;
        let frame = Frame::decode(&mut Reader::with_base_offset(&bytes, offset))?;
        let payload = frame.payload(max_payload)?.into_owned();
        Ok(FramePayload {
            frame: FrameRef::of(&frame),
            payload,
        })
    }
}

/// Reads `len` bytes at `offset`, which the caller has bounded by the file length.
fn read_at<F: ContainerFile>(file: &F, offset: u64, len: u64) -> Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| {
        Error::InvalidInput(format!("a read of {len} bytes does not fit in memory"))
    })?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(|_| {
        Error::Format(persia_format::Error::OutOfMemory {
            bytes: Some(len as u64),
        })
    })?;
    buf.resize(len, 0);
    file.read_exact_at(&mut buf, offset)
        .map_err(Error::io("read"))?;
    Ok(buf)
}

/// Checks that the superblock's manifest is one valid Manifest frame of exactly `manifest_len` bytes (SPEC §4.3).
fn check_manifest<F: ContainerFile>(file: &F, sb: &Superblock) -> Result<()> {
    let bytes = read_at(file, sb.manifest_off, sb.manifest_len)?;
    let mut r = Reader::with_base_offset(&bytes, sb.manifest_off);
    let frame = Frame::decode(&mut r)?;
    if frame.kind() != FrameKind::Manifest || frame.total_len() != sb.manifest_len {
        return Err(Error::Format(persia_format::Error::Corrupt {
            offset: sb.manifest_off,
            reason: Corruption::NotAManifest {
                kind: frame.kind().to_u8(),
                len: frame.total_len(),
            },
        }));
    }
    Ok(())
}

/// The committed frames of `wal_start..log_end`, validated (SPEC §8.6; ADR-0013): every one valid, ending exactly
/// at `log_end`, one `frame_seq` run whose last value is `next_frame_seq - 1`. None is ever truncated.
fn committed_frames<F: ContainerFile>(file: &F, sb: &Superblock) -> Result<Vec<FrameRef>> {
    let bytes = read_at(file, sb.wal_start, sb.log_end.saturating_sub(sb.wal_start))?;
    let mut scanner = FrameScanner::new(&bytes, sb.wal_start).contiguous();
    let frames: Vec<FrameRef> = scanner.by_ref().map(|f| FrameRef::of(&f)).collect();
    match scanner.end() {
        Some(ScanEnd::Clean { .. }) | None => {}
        // A torn tail below `log_end` is still corruption: these frames were committed.
        Some(ScanEnd::TornTail { offset, error } | ScanEnd::Corrupt { offset, error }) => {
            return Err(Error::Format(match error {
                persia_format::Error::Corrupt { .. } => error.clone(),
                _ => persia_format::Error::Corrupt {
                    offset: *offset,
                    reason: Corruption::BadFrameMagic,
                },
            }));
        }
        Some(ScanEnd::Unsupported { error, .. }) => return Err(Error::Format(error.clone())),
    }
    if let Some(last) = frames.last() {
        let expected = sb.next_frame_seq.saturating_sub(1);
        if last.frame_seq != expected {
            return Err(Error::Format(persia_format::Error::Corrupt {
                offset: last.offset.saturating_add(16),
                reason: Corruption::FrameSeqMismatch {
                    expected,
                    found: last.frame_seq,
                },
            }));
        }
    }
    Ok(frames)
}
