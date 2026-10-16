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
mod stream;

use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use persia_format::block::Compression;
use persia_format::frame::{
    FRAME_HEADER_LEN, Frame, FrameHeader, FrameKind, ScanEnd, StreamScanner, encode_frame,
};
use persia_format::header::{Access, HEADER_LEN, Header, KNOWN_ROCOMPAT_FLAGS};
use persia_format::superblock::{
    ActiveSuperblock, FIRST_FRAME_OFFSET, SLOTS_LEN, Slot, Superblock,
};
use persia_format::{Corruption, Reader, Writer};

pub use error::{Error, ReadOnlyReason, Result};
pub use file::ContainerFile;
pub use stream::FrameWriter;

use crate::local::fs::{parent_or_cwd, sync_dir};
use crate::local::temp_nonce;

/// Distinguishes the temporary files of one process's [`Container::create`] calls.
static CREATE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How often [`Container::open`] reopens a path that a compaction renamed over while it was taking the lock.
const OPEN_ATTEMPTS: u32 = 8;

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

    fn of_header(header: &FrameHeader) -> Self {
        Self {
            offset: header.offset(),
            len: header.total_len(),
            kind: header.kind(),
            frame_seq: header.frame_seq(),
        }
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
    /// Where [`Container::create`] or [`Container::open`] found the file; compaction renames over it.
    path: Option<PathBuf>,
    /// On Windows, the locked `<name>.lock` file that holds the writer lock (SPEC §4.5.1); `None` elsewhere.
    sidecar_lock: Option<File>,
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
    /// [`Error::AlreadyExists`] if `path` exists, or [`Error::Io`]. An `Io` error from removing the temporary name
    /// or syncing the directory comes after the link: the container then exists at `path`, unlocked, and a retry
    /// gets `AlreadyExists`.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let dir = parent_or_cwd(path);
        let name = path
            .file_name()
            .ok_or_else(|| Error::InvalidInput(format!("{} has no file name", path.display())))?;
        let sidecar_lock = match lock_sidecar(path) {
            Err(Error::Locked) if path.exists() => {
                return Err(Error::AlreadyExists {
                    path: path.to_owned(),
                });
            }
            other => other?,
        };
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
        let result = Self::create_on(file, new_db_uuid()).and_then(|container| {
            match std::fs::hard_link(&temp, path) {
                Ok(()) => Ok(Container {
                    path: Some(std::fs::canonicalize(path).map_err(Error::io("canonicalize"))?),
                    sidecar_lock,
                    ..container
                }),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    Err(Error::AlreadyExists {
                        path: path.to_owned(),
                    })
                }
                Err(err) => Err(Error::io("link")(err)),
            }
        });
        // The temporary name goes whatever happened. One directory sync then covers the link and the removal, and
        // runs even if the removal failed, so a container reported or not is durable either way.
        let removed = std::fs::remove_file(&temp).map_err(Error::io("remove temporary file"));
        let synced = sync_dir(dir).map_err(Error::io("sync directory"));
        let container = result?;
        removed?;
        synced?;
        Ok(container)
    }

    /// Opens the container at `path` and recovers it (SPEC §4.4, §8.6). [`Mode::ReadWrite`] takes the writer lock.
    ///
    /// # Errors
    /// [`Error::Locked`] if another handle holds the writer lock, and the errors of [`Container::open_on`].
    pub fn open(path: impl AsRef<Path>, mode: Mode) -> Result<Self> {
        // Canonical, so compaction renames over the real file, not over a symbolic link to it.
        let path = std::fs::canonicalize(path.as_ref()).map_err(Error::io("open"))?;
        let sidecar_lock = match mode {
            Mode::ReadWrite => lock_sidecar(&path)?,
            Mode::ReadOnly => None,
        };
        for _ in 0..OPEN_ATTEMPTS {
            let file = OpenOptions::new()
                .read(true)
                .write(mode == Mode::ReadWrite)
                .open(&path)
                .map_err(Error::io("open"))?;
            if mode == Mode::ReadWrite {
                take_lock(&file)?;
                // A compaction may have renamed a new file over the path between our open and our lock; the lock
                // we got is then on the old, unlinked file, which its writer has just released. Open again.
                if !same_file(&file, &path)? {
                    continue;
                }
            }
            let mut container = Self::recover(file, mode)?;
            container.path = Some(path);
            container.sidecar_lock = sidecar_lock;
            return Ok(container);
        }
        // The path changed under every attempt: a writer is busy compacting.
        Err(Error::Locked)
    }

    /// Compacts the container in place (SPEC §4.6): [`Container::compact_into`] a temporary file
    /// `<name>.compact.<nonce>.<n>` next to it, then renames that over the container and syncs the directory. This
    /// handle then works on the new file; frame offsets the caller held are replaced by the returned
    /// [`Relocation`]. Handles that opened the old file keep reading it.
    ///
    /// The path names a complete old or new container at every instant. On an error before the rename, the
    /// temporary file is removed and this handle is as [`Container::compact_into`] left it. The rename goes to the
    /// canonical path found at open (a symbolic link to the container keeps pointing at it; other hard links to the
    /// file are not supported: they keep the old contents), and is refused if that path no longer names this
    /// file. If the directory sync fails after the rename, [`Error::CompactedNotDurable`] carries the
    /// [`Relocation`] and this handle moves to the new file, poisoned. On Windows an open mapping may block the
    /// rename (ROADMAP 2.4.5.3) and the path identity is not checked (2.4.16).
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if the container was not opened from a path or the path names another file,
    /// [`Error::CompactedNotDurable`], and the errors of [`Container::compact_into`].
    pub fn compact(
        &mut self,
        live: &[Range<u64>],
        manifest: impl FnOnce(&Relocation) -> Vec<u8>,
        compression: Compression,
    ) -> Result<Relocation> {
        let path = self.path.clone().ok_or_else(|| {
            Error::InvalidInput("the container was not opened from a path".into())
        })?;
        self.writable()?;
        let dir = parent_or_cwd(&path).to_owned();
        let mut temp_name = path
            .file_name()
            .ok_or_else(|| Error::InvalidInput(format!("{} has no file name", path.display())))?
            .to_owned();
        let n = CREATE_COUNTER.fetch_add(1, Ordering::Relaxed);
        temp_name.push(format!(".compact.{:016x}.{n}", temp_nonce()));
        let temp = dir.join(temp_name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(Error::io("create compaction file"))?;
        let (mut new, relocation) = match self.compact_into(file, live, manifest, compression) {
            Ok(done) => done,
            Err(err) => return Err(discard(&temp, err)),
        };
        // Never rename over a path that no longer names this file (replaced from outside): that file is not ours.
        let renamed = match same_file(&self.file, &path) {
            Ok(true) => std::fs::rename(&temp, &path).map_err(Error::io("rename")),
            Ok(false) => Err(Error::InvalidInput(format!(
                "{} no longer names this container",
                path.display()
            ))),
            Err(err) => Err(err),
        };
        if let Err(err) = renamed {
            drop(new);
            return Err(discard(&temp, err));
        }
        new.path = Some(path);
        new.sidecar_lock = self.sidecar_lock.take();
        let synced = sync_dir(&dir);
        new.poisoned = synced.is_err();
        *self = new;
        match synced {
            Ok(()) => Ok(relocation),
            Err(source) => Err(Error::CompactedNotDurable { relocation, source }),
        }
    }
}

/// Removes an unused compaction file after `err`; a failed removal is added to the error rather than lost.
fn discard(temp: &Path, err: Error) -> Error {
    match std::fs::remove_file(temp) {
        Ok(()) => err,
        Err(cleanup) => Error::Io {
            op: "compact",
            source: io::Error::other(format!(
                "{err}; removing {} also failed: {cleanup}",
                temp.display()
            )),
        },
    }
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
    /// Writes a new container into `file`, which must be empty, and opens it read-write, taking the writer lock
    /// ([`ContainerFile::try_lock`]).
    ///
    /// Writes the header and both slots in the new-container state of SPEC §4.3, then syncs.
    ///
    /// # Errors
    /// [`Error::Locked`], [`Error::InvalidInput`] if `file` is not empty, or [`Error::Io`].
    pub fn create_on(file: F, db_uuid: [u8; 16]) -> Result<Self> {
        take_lock(&file)?;
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
            path: None,
            sidecar_lock: None,
        })
    }

    /// Opens the container in `file` and recovers it (SPEC §4.4, §8.6). [`Mode::ReadWrite`] takes the writer lock
    /// ([`ContainerFile::try_lock`]); [`Mode::ReadOnly`] takes none.
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
    /// for a read-write open whose scan from `log_end` ends in corruption (ROADMAP 2.4.10), [`Error::Locked`],
    /// [`Error::ReadOnly`] for a read-write open of a container with unknown `rocompat_flags`, or [`Error::Io`].
    pub fn open_on(file: F, mode: Mode) -> Result<Self> {
        if mode == Mode::ReadWrite {
            take_lock(&file)?;
        }
        Self::recover(file, mode)
    }

    /// [`Container::open_on`] after the lock, if any, was taken.
    fn recover(file: F, mode: Mode) -> Result<Self> {
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
            path: None,
            sidecar_lock: None,
        };
        container.recover_tail(file_len, &mut frames)?;
        container.frames = frames;
        Ok(container)
    }

    /// Scans from `log_end` to `file_len` (SPEC §4.4) and appends the valid frames to `frames`.
    fn recover_tail(&mut self, file_len: u64, frames: &mut Vec<FrameRef>) -> Result<()> {
        let log_end = self.active.superblock.log_end;
        let scanner = StreamScanner::new(log_end, file_len).expect_frame_seq(self.next_frame_seq);
        let (scanner, end) = scan(&self.file, scanner, frames)?;
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
        if self.mode == Mode::ReadWrite {
            // What the scan read may still be only in the page cache of a writer that died before syncing (frames,
            // a truncation, even the slot it chose). Make it durable before this handle builds on it: a commit
            // that covers those frames must never reach the disk before them (SPEC §4.5 step 2).
            self.file.sync_data().map_err(Error::io("sync"))?;
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
    /// not committed: appended through this handle (durable once synced), or found by recovery (WAL frames synced
    /// after the last commit, SPEC §8.2, or the frames of a commit that did not complete; a read-write open makes
    /// them durable).
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
    /// waiting (frames found by a read-write open's recovery are already durable). Synced frames survive a crash
    /// even before they are committed: recovery finds them after `log_end`.
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
        let known = self
            .frames
            .binary_search_by_key(&commit.manifest.offset, |f| f.offset)
            .is_ok_and(|i| self.frames.get(i) == Some(&commit.manifest));
        if commit.manifest.kind != FrameKind::Manifest || !known {
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
    /// decompressed under the bomb limit `max_payload` (SPEC §4.4). Works on a poisoned handle too, where it may
    /// return frames that never became durable.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `offset` is outside the frames, [`Error::Format`] if the frame there is invalid
    /// or its payload exceeds `max_payload`, or [`Error::Io`].
    pub fn read_frame(&self, offset: u64, max_payload: usize) -> Result<FramePayload> {
        let bytes = self.frame_bytes(offset)?;
        let frame = Frame::decode(&mut Reader::with_base_offset(&bytes, offset))?;
        let payload = frame.payload(max_payload)?.into_owned();
        Ok(FramePayload {
            frame: FrameRef::of(&frame),
            payload,
        })
    }

    /// The bytes of the frame at `offset`, sized from its header and bounded by [`Container::end`].
    fn frame_bytes(&self, offset: u64) -> Result<Vec<u8>> {
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
        read_at(&self.file, offset, header.total_len())
    }

    /// Sorts the live frames below `wal_start` named by the writer and checks that they can be frames there: inside
    /// `12288..wal_start`, not overlapping. Exact duplicates are merged.
    fn live_below_wal(&self, live: &[Range<u64>]) -> Result<Vec<Range<u64>>> {
        let wal_start = self.active.superblock.wal_start;
        let mut sorted = live.to_vec();
        sorted.sort_by_key(|r| (r.start, r.end));
        sorted.dedup();
        let mut prev_end = FIRST_FRAME_OFFSET;
        for r in &sorted {
            if r.start < prev_end || r.end <= r.start || r.end > wal_start {
                return Err(Error::InvalidInput(format!(
                    "live frame {}..{} is not a separate frame in {FIRST_FRAME_OFFSET}..{wal_start}",
                    r.start, r.end
                )));
            }
            prev_end = r.end;
        }
        Ok(sorted)
    }

    /// How much of the container is live, given the live frames below `wal_start` that the writer's manifest
    /// references (SPEC §4.6). Every frame from `wal_start` on counts as live. Ranges are checked for bounds and
    /// overlap only, not read: a wrong range skews the result (compaction does check them).
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if a range is outside `12288..wal_start` or overlaps another.
    pub fn space_usage(&self, live: &[Range<u64>]) -> Result<SpaceUsage> {
        let below: u64 = self
            .live_below_wal(live)?
            .iter()
            .map(|r| r.end.saturating_sub(r.start))
            .fold(0, u64::saturating_add);
        let from_wal = self.end.saturating_sub(self.active.superblock.wal_start);
        Ok(SpaceUsage {
            total: self.end,
            live: FIRST_FRAME_OFFSET
                .saturating_add(below)
                .saturating_add(from_wal),
        })
    }

    /// Writes the compacted container into `target`, an empty file, and returns it open read-write together with
    /// where every frame moved (SPEC §4.6). `live` names the live frames below `wal_start` (byte ranges, as the
    /// manifest records them); every frame from `wal_start` on is copied too. `manifest` builds the new Manifest
    /// payload from the [`Relocation`]; it is stored with `compression`.
    ///
    /// Unsynced appends are synced first; a failure there poisons this handle like any failed sync. Otherwise this
    /// container's file is only read, so on error it is unchanged and still writable, and `target` is garbage.
    /// [`Container::compact`] wraps this with the temporary file and the rename.
    ///
    /// `live` must come from the writer's own manifest: a range is accepted if the bytes there decode as one valid
    /// frame of exactly that length. The [`Relocation`] passed to `manifest` covers the frames below `log_end` (a
    /// manifest never references uncommitted frames); the returned one also covers the tail.
    ///
    /// # Errors
    /// [`Error::ReadOnly`], [`Error::Poisoned`], [`Error::Locked`] (on `target`), [`Error::InvalidInput`] for bad
    /// `live` ranges (also a range that is not exactly one frame) or a non-empty `target`, [`Error::Format`] if a
    /// frame to copy is corrupt, [`Error::Encode`], or [`Error::Io`].
    pub fn compact_into<G: ContainerFile>(
        &mut self,
        target: G,
        live: &[Range<u64>],
        manifest: impl FnOnce(&Relocation) -> Vec<u8>,
        compression: Compression,
    ) -> Result<(Container<G>, Relocation)> {
        self.writable()?;
        self.sync()?;
        take_lock(&target)?;
        let target_len = target.file_len().map_err(Error::io("stat"))?;
        if target_len != 0 {
            return Err(Error::InvalidInput(format!(
                "compaction writes into an empty file, not one of {target_len} bytes"
            )));
        }
        let old = self.active.superblock;
        let live = self.live_below_wal(live)?;
        let mut out = Rewriter {
            target: &target,
            end: FIRST_FRAME_OFFSET,
            next_frame_seq: self.next_frame_seq,
            moved: Vec::new(),
        };
        for r in &live {
            out.copy(self, r.start, Some(r.end.saturating_sub(r.start)))?;
        }
        let (committed, tail): (Vec<FrameRef>, Vec<FrameRef>) =
            self.frames.iter().partition(|f| f.offset < old.log_end);
        let wal_first = out.moved.len();
        for f in &committed {
            out.copy(self, f.offset, Some(f.len))?;
        }
        let relocation = Relocation {
            moved: out.moved.clone(),
        };
        let manifest = out.append(FrameKind::Manifest, &manifest(&relocation), compression)?;
        let wal_start = out
            .moved
            .get(wal_first)
            .filter(|_| !committed.is_empty())
            .map_or(manifest.end(), |(_, new)| new.offset);
        let superblock = Superblock {
            seq: old
                .seq
                .checked_add(1)
                .ok_or_else(|| Error::InvalidInput("commit seq exhausted".into()))?,
            manifest_off: manifest.offset,
            manifest_len: manifest.len,
            log_end: manifest.end(),
            wal_start,
            fence_token: old.fence_token,
            next_frame_seq: out.next_frame_seq,
        };
        for f in &tail {
            out.copy(self, f.offset, Some(f.len))?;
        }
        let header = self.header.for_write();
        let mut w = Writer::new();
        header.encode(&mut w)?;
        superblock.encode(&mut w)?;
        superblock.encode(&mut w)?;
        target
            .write_all_at(w.as_slice(), 0)
            .map_err(Error::io("write header"))?;
        target.sync_data().map_err(Error::io("sync"))?;

        let (end, next_frame_seq) = (out.end, out.next_frame_seq);
        let mut frames: Vec<FrameRef> = out
            .moved
            .get(wal_first..)
            .unwrap_or_default()
            .iter()
            .map(|(_, new)| *new)
            .filter(|f| f.offset >= wal_start)
            .collect();
        if manifest.offset >= wal_start {
            frames.push(manifest);
        }
        frames.sort_by_key(|f| f.offset);
        let relocation = Relocation { moved: out.moved };
        let container = Container {
            file: target,
            header,
            mode: Mode::ReadWrite,
            active: ActiveSuperblock {
                slot: Slot::A,
                superblock,
            },
            end,
            next_frame_seq,
            frames,
            dirty: false,
            poisoned: false,
            recovery: Recovery {
                scan_end: ScanEnd::Clean { offset: end },
                truncated_from: None,
            },
            path: None,
            sidecar_lock: None,
        };
        Ok((container, relocation))
    }
}

/// Reads `len` bytes at `offset`, which the caller has bounded by the file length.
fn read_at<F: ContainerFile>(file: &F, offset: u64, len: u64) -> Result<Vec<u8>> {
    let oom = || Error::OutOfMemory { bytes: len };
    let len = usize::try_from(len).map_err(|_| oom())?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(|_| oom())?;
    buf.resize(len, 0);
    file.read_exact_at(&mut buf, offset)
        .map_err(Error::io("read"))?;
    Ok(buf)
}

/// Scans the range of `scanner` in `file` with bounded memory (ROADMAP 2.4.12), appending the valid frames to
/// `frames`; returns the stopped scanner and why it stopped.
fn scan<F: ContainerFile>(
    file: &F,
    mut scanner: StreamScanner,
    frames: &mut Vec<FrameRef>,
) -> Result<(StreamScanner, ScanEnd)> {
    let mut read = |offset, buf: &mut [u8]| file.read_exact_at(buf, offset);
    while let Some(h) = scanner.next_frame(&mut read).map_err(Error::io("read"))? {
        frames.push(FrameRef::of_header(&h));
    }
    let end = scanner.end().cloned().unwrap_or(ScanEnd::Clean {
        offset: scanner.offset(),
    });
    Ok((scanner, end))
}

/// Checks that the superblock's manifest is one valid Manifest frame of exactly `manifest_len` bytes (SPEC §4.3).
fn check_manifest<F: ContainerFile>(file: &F, sb: &Superblock) -> Result<()> {
    // `check_bounds` put the frame inside the file, so its end fits.
    let range_end = sb.manifest_off.saturating_add(sb.manifest_len);
    let mut scanner = StreamScanner::new(sb.manifest_off, range_end);
    let mut read = |offset, buf: &mut [u8]| file.read_exact_at(buf, offset);
    let Some(frame) = scanner.next_frame(&mut read).map_err(Error::io("read"))? else {
        let error = scanner.end().and_then(ScanEnd::error).cloned();
        return Err(Error::Format(error.unwrap_or(
            persia_format::Error::Corrupt {
                offset: sb.manifest_off,
                reason: Corruption::NotAManifest { kind: 0, len: 0 },
            },
        )));
    };
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
    let mut frames = Vec::new();
    let scanner = StreamScanner::new(sb.wal_start, sb.log_end).contiguous();
    match scan(file, scanner, &mut frames)?.1 {
        ScanEnd::Clean { .. } => {}
        // A torn tail below `log_end` is still corruption: these frames were committed.
        ScanEnd::TornTail { error, .. }
        | ScanEnd::Corrupt { error, .. }
        | ScanEnd::Unsupported { error, .. } => return Err(Error::Format(error)),
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

/// Takes the exclusive writer lock on `file` without waiting (SPEC §8.4).
fn take_lock<F: ContainerFile>(file: &F) -> Result<()> {
    match file.try_lock() {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::Locked),
        Err(err) => Err(Error::io("lock")(err)),
    }
}

impl Container<sim::SimFile> {
    /// The simulated file, for tests to inspect, fault or crash it. A real [`File`] is not exposed: writing it
    /// directly would bypass poisoning and the `frame_seq` run.
    pub fn file(&self) -> &sim::SimFile {
        &self.file
    }
}

/// How much of a container is live (SPEC §4.6), from [`Container::space_usage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceUsage {
    /// The end of the valid frames.
    pub total: u64,
    /// The prefix, the live frames below `wal_start`, and everything from `wal_start` on.
    pub live: u64,
}

/// The dead-space ratio at which compaction is due by default (SPEC §4.6).
pub const DEFAULT_DEAD_SPACE_PERCENT: u8 = 40;

impl SpaceUsage {
    /// Bytes compaction would reclaim.
    pub fn dead(&self) -> u64 {
        self.total.saturating_sub(self.live)
    }

    /// Whether dead bytes reach `percent` of the total; never when nothing is dead.
    ///
    /// ```
    /// use persia_storage::container::SpaceUsage;
    ///
    /// let usage = SpaceUsage { total: 1000, live: 600 };
    /// assert!(usage.is_due(40));
    /// assert!(!usage.is_due(41));
    /// ```
    pub fn is_due(&self, percent: u8) -> bool {
        self.dead() > 0
            && u128::from(self.dead()).saturating_mul(100)
                >= u128::from(self.total).saturating_mul(u128::from(percent))
    }
}

/// Where compaction moved each frame: old frame to new frame, in old-offset order (SPEC §4.6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Relocation {
    moved: Vec<(FrameRef, FrameRef)>,
}

impl Relocation {
    /// The new frame of the old frame at `old_offset`, if it was copied.
    pub fn get(&self, old_offset: u64) -> Option<FrameRef> {
        let i = self
            .moved
            .binary_search_by_key(&old_offset, |(old, _)| old.offset)
            .ok()?;
        self.moved.get(i).map(|(_, new)| *new)
    }

    /// Every `(old, new)` pair, in old-offset order.
    pub fn iter(&self) -> impl Iterator<Item = (FrameRef, FrameRef)> + '_ {
        self.moved.iter().copied()
    }

    /// How many frames were copied.
    pub fn len(&self) -> usize {
        self.moved.len()
    }

    /// Whether no frame was copied.
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty()
    }
}

/// Appends renumbered copies of frames to a compaction target.
struct Rewriter<'a, G> {
    target: &'a G,
    end: u64,
    next_frame_seq: u64,
    moved: Vec<(FrameRef, FrameRef)>,
}

impl<G: ContainerFile> Rewriter<'_, G> {
    fn take_seq(&mut self) -> Result<u64> {
        let seq = self.next_frame_seq;
        self.next_frame_seq = seq
            .checked_add(1)
            .ok_or_else(|| Error::InvalidInput("frame_seq exhausted (ADR-0013)".into()))?;
        Ok(seq)
    }

    fn write(&mut self, w: &Writer, kind: FrameKind, frame_seq: u64) -> Result<FrameRef> {
        let len = w.len() as u64;
        let offset = self.end;
        self.end = offset
            .checked_add(len)
            .ok_or_else(|| Error::InvalidInput("container offset exhausted".into()))?;
        self.target
            .write_all_at(w.as_slice(), offset)
            .map_err(Error::io("write compacted frame"))?;
        Ok(FrameRef {
            offset,
            len,
            kind,
            frame_seq,
        })
    }

    /// Copies the frame at `offset` of `from`, checking it is `len` bytes long.
    fn copy<F: ContainerFile>(
        &mut self,
        from: &Container<F>,
        offset: u64,
        len: Option<u64>,
    ) -> Result<()> {
        let bytes = from.frame_bytes(offset)?;
        let frame = Frame::decode(&mut Reader::with_base_offset(&bytes, offset))?;
        if len.is_some_and(|len| len != frame.total_len()) {
            return Err(Error::InvalidInput(format!(
                "live range at {offset} is not one frame: the frame there is {} bytes",
                frame.total_len()
            )));
        }
        let seq = self.take_seq()?;
        let mut w = Writer::with_base_offset(self.end);
        frame.reframe(&mut w, seq);
        let new = self.write(&w, frame.kind(), seq)?;
        self.moved.push((FrameRef::of(&frame), new));
        Ok(())
    }

    fn append(
        &mut self,
        kind: FrameKind,
        payload: &[u8],
        compression: Compression,
    ) -> Result<FrameRef> {
        let seq = self.take_seq()?;
        let mut w = Writer::with_base_offset(self.end);
        encode_frame(&mut w, kind, seq, payload, compression)?;
        self.write(&w, kind, seq)
    }
}

/// Takes the writer lock of the container at `path` on Windows: an exclusive lock on `<name>.lock` next to it, created
/// if missing and never removed (removing it would race with a writer taking it). `LockFileEx` locks are mandatory, so
/// locking the container itself would keep every reader out (SPEC §4.5.1, ROADMAP 2.4.16).
#[cfg(windows)]
fn lock_sidecar(path: &Path) -> Result<Option<File>> {
    let mut name = path
        .file_name()
        .ok_or_else(|| Error::InvalidInput(format!("{} has no file name", path.display())))?
        .to_owned();
    name.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_file_name(name))
        .map_err(Error::io("open lock file"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Locked),
        Err(std::fs::TryLockError::Error(err)) => Err(Error::io("lock")(err)),
    }
}

/// Unix locks the container file itself (`flock`, advisory), through [`ContainerFile::try_lock`].
#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)] // same signature as the Windows version
fn lock_sidecar(_path: &Path) -> Result<Option<File>> {
    Ok(None)
}

/// Whether `path` still names the file open as `file` (same device and inode).
#[cfg(unix)]
fn same_file(file: &File, path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let open = file.metadata().map_err(Error::io("stat"))?;
    match std::fs::metadata(path) {
        Ok(named) => Ok((open.dev(), open.ino()) == (named.dev(), named.ino())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(Error::io("stat")(err)),
    }
}

/// The standard library exposes no stable file identity on Windows; the check is part of ROADMAP 2.4.16.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // same signature as the Unix version
fn same_file(_file: &File, _path: &Path) -> Result<bool> {
    Ok(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn same_file_sees_a_rename_over_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, b"old").unwrap();
        let old = File::open(&path).unwrap();
        assert!(same_file(&old, &path).unwrap());
        let other = dir.path().join("b");
        std::fs::write(&other, b"new").unwrap();
        std::fs::rename(&other, &path).unwrap();
        assert!(
            !same_file(&old, &path).unwrap(),
            "the path names another inode now"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(!same_file(&old, &path).unwrap(), "the path names nothing");
    }
}
