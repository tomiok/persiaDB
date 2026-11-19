//! [`PackStore`]: the [`BlobStore`] of a local database, over its pack file (SPEC §9.3, ADR-0049).

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use persia_format::frame::{
    FRAME_HEADER_LEN, FrameHeader, FrameKind, MIN_FRAME_LEN, StreamScanner,
};
use persia_format::header::{Access, BLOB_PACK, Header};
use persia_format::source::ByteSource;
use persia_format::superblock::{FIRST_FRAME_OFFSET, Slot};
use persia_format::widen::{u64_from_usize, usize_from_u32};
use persia_format::{BlobId, Reader};
use persia_storage::container::{ContainerFile, MapPermit};
use persia_storage::vfs::{RealFs, Vfs};

use super::layout::{
    self, PackSuperblock, corrupt, decode_chunk_payload, decode_frame, decode_index_payload,
    decode_prefix, encode_chunk, encode_index_frame, from_format,
};
use crate::error::{Error, Result};
use crate::manifest::{BlobManifest, UploadId};
use crate::store::{BindOutcome, BlobStore};

/// How a pack is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackMode {
    /// The container's writer: stages and binds, creates the pack on its first write, and truncates what no commit
    /// covers. Only the process holding the container's writer lock opens the pack this way (ADR-0049 item 8).
    ReadWrite,
    /// Never writes, truncates or creates: the blobs committed as of the open (or the last
    /// [`refresh`](PackStore::refresh)).
    ReadOnly,
}

/// What opening the pack found (SPEC §9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PackRecovery {
    /// The file length before a read-write open truncated it to the committed `log_end`, if it did: the bytes after
    /// it (staged chunks, a torn frame, the index frame of a cut bind) were never acknowledged (ADR-0049 item 9).
    pub truncated_from: Option<u64>,
    /// How many blobs a read-write open committed from after `log_end` (ADR-0049 item 9): the index frames of binds
    /// whose slot write was cut or whose synced slot was damaged since, each with all its chunks.
    pub rolled_forward: u64,
}

/// A bound blob, as the open's scan found it or a bind wrote it.
#[derive(Debug, Clone)]
struct Entry {
    index_off: u64,
    index_len: u64,
    upload_id: UploadId,
    /// The offset of each chunk's `BlobChunk` frame, in chunk order.
    chunks: Box<[u64]>,
}

/// A chunk this handle staged and no bind has claimed yet.
#[derive(Debug, Clone, Copy)]
struct Staged {
    offset: u64,
    len: u32,
    crc: u32,
}

/// The open pack file.
#[derive(Debug)]
struct Pack<F> {
    file: F,
    header: Header,
    slot: Slot,
    sb: PackSuperblock,
    /// Where the next frame goes; `sb.log_end` for a read-only handle.
    end: u64,
    next_frame_seq: u64,
    index: HashMap<BlobId, Entry>,
    /// Which blob each bound upload is.
    uploads: HashMap<UploadId, BlobId>,
    /// Every `BlobChunk` frame below `end`, in offset order: what an index's offsets must name.
    chunk_frames: Vec<u64>,
    /// The mapping of `12288..` a committed `log_end`, with mapped reads on (ROADMAP 9.2.3).
    window: Option<ByteSource>,
}

/// What readers share with the writer, behind the `RwLock`. Only a writer holding [`PackStore`]'s writer mutex
/// changes it, and only to publish what is already written (and, for a bind, synced).
#[derive(Debug)]
struct Inner<F> {
    /// `None` while there is no pack file (SPEC §9.3: created by the first write).
    pack: Option<Pack<F>>,
    staged: HashMap<(UploadId, u32), Staged>,
    /// Set by [`PackStore::enable_mapped_reads`].
    mapper: Option<Mapper<F>>,
}

/// Why a handle stopped writing (SPEC §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Poison {
    /// A write or sync failed where nothing could have been bound.
    NotBound,
    /// The slot write or sync of the bind of this id failed: it may be bound, and only a reopen tells (ADR-0049
    /// item 6). A later `bind` of the id answers [`Error::BindOutcomeUnknown`], never a plain "not bound".
    Unknown(BlobId),
}

/// The writer's own state, behind the writer mutex that serializes every write: held across a bind's appends and
/// syncs, while readers go on under the `RwLock`'s read side.
#[derive(Debug, Default)]
struct WriterState {
    poison: Option<Poison>,
}

/// The local blob store: the pack file `<name>.persia.blobs` (SPEC §9.3, ADR-0049).
///
/// A blob is durable when [`BlobStore::bind`] returns [`BindOutcome::Bound`]: its `BlobIndex` frame is synced, then
/// the superblock slot that covers it (SPEC §18.9, decided per blob). The pack is created by the first
/// [`stage_chunk`](BlobStore::stage_chunk) or `bind`, durably (file, then directory), and a database that never
/// stored a blob has none. Any failed write or sync stops the handle from writing ([`Error::PackPoisoned`]); reopen
/// it, and its recovery decides what survived.
///
/// ```
/// use persia_blob::pack::{PackMode, PackStore};
/// use persia_blob::{BindOutcome, BlobManifest, BlobStore, ChunkSize, UploadId, blob_id_of, read_chunk_checked};
/// use persia_format::checksum::crc32c;
///
/// # if cfg!(miri) { return Ok(()); } // a real pack file: Miri's isolation has no filesystem or real-time clock
/// # let dir = tempfile::tempdir()?;
/// let path = dir.path().join("app.persia.blobs");
/// let store = PackStore::open(&path, PackMode::ReadWrite, [7; 16])?;
/// assert!(!store.exists()); // no blob yet, no file
///
/// let data = b"hello, pack";
/// let m = BlobManifest::new(blob_id_of(data), 11, ChunkSize::from_encoding(8)?, UploadId::new(1),
///     data.chunks(8).map(crc32c).collect())?;
/// store.stage_chunk(m.upload_id(), 0, &data[..8])?;
/// store.stage_chunk(m.upload_id(), 1, &data[8..])?;
/// assert_eq!(store.bind(&m)?, BindOutcome::Bound);
/// drop(store);
///
/// let store = PackStore::open(&path, PackMode::ReadOnly, [7; 16])?;
/// let found = store.manifest(m.blob_id())?.expect("bound");
/// assert_eq!(read_chunk_checked(&store, &found, 1)?, b"ack");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct PackStore<F: ContainerFile = File> {
    vfs: Arc<dyn Vfs<File = F>>,
    path: PathBuf,
    mode: PackMode,
    db_uuid: [u8; 16],
    recovery: PackRecovery,
    writer: Mutex<WriterState>,
    inner: RwLock<Inner<F>>,
}

impl PackStore<File> {
    /// [`PackStore::open_in`] on the operating system's file system.
    ///
    /// # Errors
    /// See [`PackStore::open_in`].
    pub fn open(path: impl AsRef<Path>, mode: PackMode, db_uuid: [u8; 16]) -> Result<Self> {
        Self::open_in(Arc::new(RealFs), path, mode, db_uuid)
    }
}

impl<F: ContainerFile + 'static> PackStore<F> {
    /// Opens the pack at `path` on `vfs` and recovers it (SPEC §9.3, ADR-0049 item 9); a missing file is a pack
    /// with no blobs, which a read-write handle creates at its first write with `db_uuid` (the database's) in its
    /// header.
    ///
    /// Recovery validates the header, picks the active slot and checks it against the file, then rebuilds the
    /// index by scanning the committed frames `12288..log_end`. A read-write open then truncates the file to
    /// `log_end` and syncs it: nothing after `log_end` was ever acknowledged.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a damaged header, slot or committed frame, [`Error::PackUnsupported`] for a newer
    /// format, or [`Error::PackIo`].
    pub fn open_in(
        vfs: Arc<dyn Vfs<File = F>>,
        path: impl AsRef<Path>,
        mode: PackMode,
        db_uuid: [u8; 16],
    ) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let (pack, recovery) = match vfs.open(&path, mode == PackMode::ReadWrite) {
            Ok(file) => {
                let (pack, recovery) = recover(file, mode, db_uuid)?;
                (Some(pack), recovery)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (None, PackRecovery::default()),
            Err(e) => return Err(io_err("open")(e)),
        };
        Ok(Self {
            vfs,
            path,
            mode,
            db_uuid,
            recovery,
            writer: Mutex::new(WriterState::default()),
            inner: RwLock::new(Inner {
                pack,
                staged: HashMap::new(),
                mapper: None,
            }),
        })
    }

    /// Where the pack lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How the pack was opened.
    pub fn mode(&self) -> PackMode {
        self.mode
    }

    /// What the open's recovery found.
    pub fn recovery(&self) -> PackRecovery {
        self.recovery
    }

    /// Whether the pack file exists (for this handle: it was found at open or created since).
    pub fn exists(&self) -> bool {
        self.read().pack.is_some()
    }

    /// The pack's header, if it exists: its `db_uuid` names the database that created it.
    pub fn header(&self) -> Option<Header> {
        self.read().pack.as_ref().map(|p| p.header)
    }

    /// The committed superblock, if the pack exists: the state a crash now would recover.
    pub fn superblock(&self) -> Option<PackSuperblock> {
        self.read().pack.as_ref().map(|p| p.sb)
    }

    /// The ids of the bound blobs, in no particular order.
    pub fn blob_ids(&self) -> Vec<BlobId> {
        self.read()
            .pack
            .as_ref()
            .map(|p| p.index.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Whether a failed write or sync stopped this handle from writing (SPEC §4.5): reopen to recover.
    pub fn is_poisoned(&self) -> bool {
        self.lock_writer().poison.is_some()
    }

    fn lock_writer(&self) -> MutexGuard<'_, WriterState> {
        // The state is one assignment: consistent across a panic.
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner<F>> {
        // Every update leaves the state consistent: a panic cannot interrupt one halfway, as none can panic.
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner<F>> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The writer mutex, for a handle that may write. `blob_id` names the bind asking (if any): after an unknown
    /// commit of that id, the answer is outcome-unknown, never "not bound" (ADR-0049 item 6).
    fn writer(&self, blob_id: Option<BlobId>) -> Result<MutexGuard<'_, WriterState>> {
        if self.mode == PackMode::ReadOnly {
            return Err(Error::PackReadOnly);
        }
        let w = self.lock_writer();
        match w.poison {
            None => Ok(w),
            Some(Poison::Unknown(id)) if Some(id) == blob_id => {
                Err(Error::BindOutcomeUnknown(Box::new(Error::PackPoisoned)))
            }
            Some(_) => Err(Error::PackPoisoned),
        }
    }

    /// Creates the pack if there is none (SPEC §9.3), with the writer mutex held and no `RwLock` guard: the
    /// creation syncs a file and a directory. See [`create_pack`].
    fn ensure_pack(&self, w: &mut WriterState) -> Result<()> {
        if self.read().pack.is_some() {
            return Ok(());
        }
        match create_pack(&*self.vfs, &self.path, self.db_uuid) {
            Ok((file, pack_id)) => {
                self.write().pack = Some(Pack::new(file, self.db_uuid, pack_id));
                Ok(())
            }
            Err(Created::NoPack(e)) => Err(e),
            Err(Created::MaybePack(e)) => {
                w.poison = Some(Poison::NotBound);
                Err(e)
            }
        }
    }
}

/// Why [`create_pack`] failed: before any pack existed at the name, or after (or beside) one.
#[derive(Debug)]
enum Created {
    /// No pack was linked to the name, and the temporary name was removed (or its removal reported).
    NoPack(Error),
    /// The link succeeded and a later step failed, or another pack already held the name: the handle stops writing,
    /// and a reopen finds what the path holds.
    MaybePack(Error),
}

/// Distinguishes the temporary files of one process's pack creations.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<pack>.tmp.<nonce>.<n>`: a fresh temporary name beside the pack.
fn temp_path(path: &Path) -> PathBuf {
    let mut h = RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".tmp.{:016x}.{n}", h.finish()));
    PathBuf::from(name)
}

fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Creates a new, empty pack at `path` durably (SPEC §9.3, §4.5.2) and returns it open read-write: header and slots
/// written to a temporary file and synced, the file hard-linked to `path` (which fails if the name exists), then the
/// temporary name removed and the directory synced whatever happened, as `Container::create_in` does. A failure the
/// process survives leaves a stale temporary file only if that removal fails too (the sweep, ROADMAP 9.2.6, takes it).
fn create_pack<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    path: &Path,
    db_uuid: [u8; 16],
) -> core::result::Result<(F, u128), Created> {
    let pack_id = random_u128();
    let prefix = layout::new_pack_prefix(db_uuid, pack_id).map_err(Created::NoPack)?;
    let temp = temp_path(path);
    let file = vfs
        .create_new(&temp)
        .map_err(|e| Created::NoPack(io_err("create")(e)))?;
    let written = file
        .write_all_at(&prefix, 0)
        .map_err(io_err("write header"))
        .and_then(|()| file.sync_data().map_err(io_err("sync")))
        .map_err(Created::NoPack);
    // A crash before the link leaves only the temporary file (no pack); after it, a complete empty pack.
    let linked = written.and_then(|()| {
        vfs.hard_link(&temp, path).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                // Another writer created a pack there: a second writer, outside the contract (ADR-0049 item 8).
                Created::MaybePack(io_err("link: a pack already exists at the path")(e))
            } else {
                Created::NoPack(io_err("link")(e))
            }
        })
    });
    // One directory sync covers the link and the removal, and runs even if the removal failed.
    let removed = vfs
        .remove_file(&temp)
        .map_err(io_err("remove temporary file"));
    let synced = vfs
        .sync_dir(parent_of(path))
        .map_err(io_err("sync directory"));
    linked?;
    removed.map_err(Created::MaybePack)?;
    synced.map_err(Created::MaybePack)?;
    Ok((file, pack_id))
}

/// 128 bits from the standard library's per-process hash keys, the time and the pid: a pack id (ADR-0049 item 3).
/// Ids tell pack files apart; they are not secrets.
fn random_u128() -> u128 {
    let half = |salt: u64| {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u32(std::process::id());
        if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            h.write_u128(t.as_nanos());
        }
        h.finish()
    };
    (u128::from(half(1)) << 64) | u128::from(half(2))
}

fn io_err(op: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::PackIo { op, source }
}

impl<F: ContainerFile> Pack<F> {
    /// A pack just created: the new-pack state.
    fn new(file: F, db_uuid: [u8; 16], pack_id: u128) -> Self {
        let sb = PackSuperblock::new_pack(pack_id);
        Self {
            file,
            header: Header::new(db_uuid),
            slot: Slot::A,
            sb,
            end: sb.log_end,
            next_frame_seq: sb.next_frame_seq,
            index: HashMap::new(),
            uploads: HashMap::new(),
            chunk_frames: Vec::new(),
            window: None,
        }
    }

    /// Reads `len` bytes at `offset`, from the mapping when it covers them.
    fn bytes_at(&self, offset: u64, len: u64) -> Result<ByteSource> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| corrupt("a frame runs past the end of the file"))?;
        if let Some(w) = &self.window
            && offset >= w.base_offset()
            && end <= w.end_offset()
        {
            return w.slice(offset..end).map_err(from_format);
        }
        let len = usize::try_from(len).map_err(|_| corrupt("a frame too large to read"))?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(len)
            .map_err(|_| corrupt("a frame too large to read"))?;
        buf.resize(len, 0);
        self.file
            .read_exact_at(&mut buf, offset)
            .map_err(io_err("read"))?;
        Ok(ByteSource::with_base_offset(buf.into(), offset))
    }

    /// The whole frame at `offset`, sized from its header and bounded by `limit`.
    fn frame_at(&self, offset: u64, limit: u64) -> Result<ByteSource> {
        let head_len = u64_from_usize(FRAME_HEADER_LEN);
        if offset < FIRST_FRAME_OFFSET || offset.checked_add(head_len).is_none_or(|e| e > limit) {
            return Err(corrupt("a frame offset outside the pack's frames"));
        }
        let head = self.bytes_at(offset, head_len)?;
        let header = FrameHeader::decode(&mut head.reader()).map_err(from_format)?;
        header.check_fits(limit).map_err(from_format)?;
        self.bytes_at(offset, header.total_len())
    }

    /// The manifest of a bound blob, read from its index frame and checked to name `blob_id`.
    fn manifest_of(&self, blob_id: BlobId, entry: &Entry) -> Result<BlobManifest> {
        let bytes = self.bytes_at(entry.index_off, entry.index_len)?;
        let (_, payload) = decode_frame(&bytes, entry.index_off, FrameKind::BlobIndex)?;
        let (manifest, _) = decode_index_payload(payload, entry.index_off)?;
        manifest.check_blob_id(blob_id)?;
        Ok(manifest)
    }

    /// The chunk of the `BlobChunk` frame at `offset`, checked to be chunk `index` of `upload_id` with `crc`.
    fn chunk_at(
        &self,
        offset: u64,
        upload_id: UploadId,
        index: u32,
        crc: u32,
    ) -> Result<ByteSource> {
        let frame = self.frame_at(offset, self.end)?;
        let (_, payload) = decode_frame(&frame, offset, FrameKind::BlobChunk)?;
        let (head, bytes) = decode_chunk_payload(payload)?;
        if head.upload_id != upload_id || head.index != index {
            return Err(corrupt(
                "a chunk frame of another upload or index than its index names",
            ));
        }
        if head.crc != crc {
            return Err(corrupt(
                "a chunk frame's CRC32C differs from its manifest's",
            ));
        }
        let start = offset
            .saturating_add(u64_from_usize(FRAME_HEADER_LEN))
            .saturating_add(u64_from_usize(layout::CHUNK_HEAD_LEN));
        frame
            .slice(start..start.saturating_add(u64_from_usize(bytes.len())))
            .map_err(from_format)
    }

    /// The `frame_seq` of the next frame; never `u64::MAX` (SPEC §4.4).
    fn take_seq(&self) -> Result<u64> {
        match self.next_frame_seq {
            u64::MAX => Err(Error::InvalidManifest {
                what: "frame_seq exhausted (ADR-0013)",
            }),
            seq => Ok(seq),
        }
    }
}

impl<F: ContainerFile + 'static> BlobStore for PackStore<F> {
    fn stage_chunk(&self, upload_id: UploadId, index: u32, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer(None)?;
        // Never overwritten (SPEC §9.2): a chunk already there must be these bytes.
        if let Some(existing) = staged_bytes(&self.read(), upload_id, index)? {
            return if existing.as_slice() == bytes {
                Ok(())
            } else {
                Err(Error::StagedChunkConflict { upload_id, index })
            };
        }
        self.ensure_pack(&mut w)?;
        // Written under the read side: readers go on, and only this writer (holding the mutex) moves `end`.
        let (offset, end, head) = {
            let inner = self.read();
            let pack = inner.pack.as_ref().ok_or(Error::PackPoisoned)?;
            let (frame, head) = encode_chunk(pack.take_seq()?, upload_id, index, bytes)?;
            let offset = pack.end;
            let end = offset
                .checked_add(u64_from_usize(frame.len()))
                .ok_or_else(|| corrupt("the pack's offsets are exhausted"))?;
            if let Err(e) = pack.file.write_all_at(&frame, offset) {
                w.poison = Some(Poison::NotBound);
                return Err(io_err("append")(e));
            }
            (offset, end, head)
        };
        let mut inner = self.write();
        let inner = &mut *inner;
        let pack = inner.pack.as_mut().ok_or(Error::PackPoisoned)?;
        pack.end = end;
        pack.next_frame_seq = pack.next_frame_seq.saturating_add(1);
        pack.chunk_frames.push(offset);
        inner.staged.insert(
            (upload_id, index),
            Staged {
                offset,
                len: head.len,
                crc: head.crc,
            },
        );
        Ok(())
    }

    fn bind(&self, manifest: &BlobManifest) -> Result<BindOutcome> {
        let blob_id = manifest.blob_id();
        let upload_id = manifest.upload_id();
        let mut w = self.writer(Some(blob_id))?;
        let mut chunks = Vec::new();
        {
            let inner = self.read();
            if let Some(pack) = &inner.pack {
                // Entries are published only after their commit synced (the writer mutex is held until then), so
                // the manifest this answers with is durable.
                if let Some(entry) = pack.index.get(&blob_id) {
                    return Ok(BindOutcome::AlreadyBound(pack.manifest_of(blob_id, entry)?));
                }
                // An upload binds one blob: a second index naming it would make the pack fail to open.
                if pack.uploads.contains_key(&upload_id) {
                    return Err(Error::InvalidManifest {
                        what: "an upload already bound to another blob",
                    });
                }
            }
            for index in 0..manifest.chunk_count() {
                let expected = manifest
                    .chunk_range(index)
                    .and_then(|r| r.end.checked_sub(r.start))
                    .zip(manifest.chunk_crcs().get(usize_from_u32(index)));
                match (inner.staged.get(&(upload_id, index)), expected) {
                    (Some(s), Some((len, &crc))) if u64::from(s.len) == len && s.crc == crc => {
                        chunks.push(s.offset);
                    }
                    _ => return Err(Error::ChunkNotStaged { upload_id, index }),
                }
            }
        }
        self.ensure_pack(&mut w)?;
        // Appended, synced, committed and synced under the read side only: readers are not blocked by the syncs,
        // and see the entry only once it is published below.
        // Encoded and checked first: an error here wrote nothing, so the handle stays usable.
        let (frame, log_end) = {
            let inner = self.read();
            let pack = inner.pack.as_ref().ok_or(Error::PackPoisoned)?;
            prepare_index(pack, manifest, &chunks)?
        };
        let committed = {
            let inner = self.read();
            let pack = inner.pack.as_ref().ok_or(Error::PackPoisoned)?;
            commit(pack, &frame, log_end)
        };
        let (sb, slot) = match committed {
            Ok(done) => done,
            Err(e) => {
                w.poison = Some(if e.is_bind_outcome_unknown() {
                    Poison::Unknown(blob_id)
                } else {
                    Poison::NotBound
                });
                return Err(e);
            }
        };
        let mut inner = self.write();
        let inner = &mut *inner;
        let pack = inner.pack.as_mut().ok_or(Error::PackPoisoned)?;
        pack.end = sb.log_end;
        pack.next_frame_seq = sb.next_frame_seq;
        pack.sb = sb;
        pack.slot = slot;
        pack.uploads.insert(upload_id, blob_id);
        pack.index.insert(
            blob_id,
            Entry {
                index_off: sb.index_off,
                index_len: sb.index_len,
                upload_id,
                chunks: chunks.into_boxed_slice(),
            },
        );
        if let Some(mapper) = &mut inner.mapper {
            mapper.remap(pack);
        }
        inner.staged.retain(|(upload, _), _| *upload != upload_id);
        Ok(BindOutcome::Bound)
    }

    fn manifest(&self, blob_id: BlobId) -> Result<Option<BlobManifest>> {
        let inner = self.read();
        let Some(pack) = &inner.pack else {
            return Ok(None);
        };
        match pack.index.get(&blob_id) {
            None => Ok(None),
            Some(entry) => pack.manifest_of(blob_id, entry).map(Some),
        }
    }

    fn read_chunk(&self, manifest: &BlobManifest, index: u32) -> Result<Vec<u8>> {
        self.read_chunk_source(manifest, index)
            .map(|bytes| bytes.as_slice().to_vec())
    }

    /// Does nothing: a duplicate's chunks stay in the pack until the pack rewrite (SPEC §9.2). This handle forgets
    /// them, so they cannot be bound any more.
    fn discard_upload(&self, upload_id: UploadId) -> Result<()> {
        let _writer = self.lock_writer();
        self.write()
            .staged
            .retain(|(upload, _), _| *upload != upload_id);
        Ok(())
    }
}

impl<F: ContainerFile + 'static> PackStore<F> {
    /// [`BlobStore::read_chunk`] without a copy when mapped reads are on: the chunk's bytes as a slice of the
    /// mapping (SPEC §4.7), or of the buffer they were read into. Checked against `manifest` as `read_chunk` is:
    /// the frame's CRC, its upload id and chunk index, and its stored CRC32C equal to the manifest's;
    /// [`read_chunk_checked`](crate::read_chunk_checked) also checks the length and the bytes' CRC32C.
    ///
    /// # Errors
    /// [`Error::Corrupt`] when the chunk is not in the pack or its frame is damaged, or [`Error::PackIo`].
    pub fn read_chunk_source(&self, manifest: &BlobManifest, index: u32) -> Result<ByteSource> {
        let inner = self.read();
        let missing = || corrupt("a chunk the manifest names is not in the pack");
        let pack = inner.pack.as_ref().ok_or_else(missing)?;
        let upload_id = manifest.upload_id();
        let offset = pack
            .index
            .get(&manifest.blob_id())
            .filter(|e| e.upload_id == upload_id)
            .and_then(|e| e.chunks.get(usize_from_u32(index)).copied())
            .or_else(|| inner.staged.get(&(upload_id, index)).map(|s| s.offset))
            .ok_or_else(missing)?;
        let crc = *manifest
            .chunk_crcs()
            .get(usize_from_u32(index))
            .ok_or_else(|| corrupt("a chunk past the blob's last one was asked for"))?;
        pack.chunk_at(offset, upload_id, index, crc)
    }
}

/// The bytes already staged or bound as chunk `index` of `upload_id`, if any.
fn staged_bytes<F: ContainerFile>(
    inner: &Inner<F>,
    upload_id: UploadId,
    index: u32,
) -> Result<Option<Vec<u8>>> {
    let Some(pack) = &inner.pack else {
        return Ok(None);
    };
    let located = match inner.staged.get(&(upload_id, index)) {
        Some(s) => Some(s.offset),
        None => pack
            .uploads
            .get(&upload_id)
            .and_then(|id| pack.index.get(id))
            .and_then(|e| e.chunks.get(usize_from_u32(index)).copied()),
    };
    let Some(offset) = located else {
        return Ok(None);
    };
    let frame = pack.frame_at(offset, pack.end)?;
    let (_, payload) = decode_frame(&frame, offset, FrameKind::BlobChunk)?;
    let (head, bytes) = decode_chunk_payload(payload)?;
    if head.upload_id != upload_id || head.index != index {
        return Err(corrupt(
            "a chunk frame of another upload or index than recorded",
        ));
    }
    Ok(Some(bytes.to_vec()))
}

/// The index frame of `manifest` (its chunks at `chunks`) to append at `pack.end`, and the end it gives the log. Writes
/// nothing.
fn prepare_index<F: ContainerFile>(
    pack: &Pack<F>,
    manifest: &BlobManifest,
    chunks: &[u64],
) -> Result<(Vec<u8>, u64)> {
    let frame = encode_index_frame(pack.take_seq()?, manifest, chunks)?;
    let log_end = pack
        .end
        .checked_add(u64_from_usize(frame.len()))
        .ok_or_else(|| corrupt("the pack's offsets are exhausted"))?;
    Ok((frame, log_end))
}

/// Appends the index frame `frame` (from [`prepare_index`]) at `pack.end` and commits it (SPEC §9.3, §18.9): sync the
/// data, write the inactive slot, sync again. Returns the new superblock and its slot, which the caller publishes;
/// `pack` is not changed. An error before the slot write means the blob is not bound; from the slot write on its
/// outcome is unknown ([`Error::BindOutcomeUnknown`]).
fn commit<F: ContainerFile>(
    pack: &Pack<F>,
    frame: &[u8],
    log_end: u64,
) -> Result<(PackSuperblock, Slot)> {
    let index_off = pack.end;
    let index_len = u64_from_usize(frame.len());
    pack.file
        .write_all_at(frame, index_off)
        .map_err(io_err("append"))?;
    pack.file.sync_data().map_err(io_err("sync"))?;
    let blob_count = pack.sb.blob_count.saturating_add(1);
    let next_frame_seq = pack.next_frame_seq.saturating_add(1);
    write_slot(
        pack,
        index_off,
        index_len,
        blob_count,
        log_end,
        next_frame_seq,
    )
}

/// Commits what a read-write open rolled forward (the frames up to `pack.end` are synced): the inactive slot names
/// the last index frame found and every blob indexed.
fn commit_slot<F: ContainerFile>(pack: &mut Pack<F>) -> Result<PackSuperblock> {
    let (index_off, index_len) = pack
        .index
        .values()
        .map(|e| (e.index_off, e.index_len))
        .max()
        .ok_or_else(|| corrupt("nothing to commit"))?;
    let blob_count = u64_from_usize(pack.index.len());
    let (sb, slot) = write_slot(
        pack,
        index_off,
        index_len,
        blob_count,
        pack.end,
        pack.next_frame_seq,
    )?;
    pack.sb = sb;
    pack.slot = slot;
    Ok(sb)
}

/// Writes and syncs the inactive slot naming the frames up to `log_end`, the last index frame at `index_off`.
fn write_slot<F: ContainerFile>(
    pack: &Pack<F>,
    index_off: u64,
    index_len: u64,
    blob_count: u64,
    log_end: u64,
    next_frame_seq: u64,
) -> Result<(PackSuperblock, Slot)> {
    let old = pack.sb;
    let sb = PackSuperblock {
        seq: old
            .seq
            .checked_add(1)
            .ok_or_else(|| corrupt("the commit seq is exhausted"))?,
        index_off,
        index_len,
        log_end,
        generation: old.generation,
        blob_count,
        next_frame_seq,
        pack_id: old.pack_id,
    };
    let slot = pack.slot.other();
    let unknown = |e: io::Error| Error::BindOutcomeUnknown(Box::new(e));
    pack.file
        .write_all_at(&sb.encode(), slot.offset())
        .map_err(unknown)?;
    pack.file.sync_data().map_err(unknown)?;
    Ok((sb, slot))
}

mod mapped;
mod recover;
mod refresh;

use mapped::Mapper;
use recover::{check_scan, read_prefix, recover, scan_committed};
