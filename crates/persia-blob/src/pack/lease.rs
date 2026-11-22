//! Reader leases on the local blob pack (SPEC §9.3, §9.5; ROADMAP 9.2.16; ADR-0054).
//!
//! A read-only handle holds one reader lease, `<pack>.leases/<lease_id>.lease`, from before it opens its documents
//! until it is dropped, under the container's lock primitive: the file itself ([`ContainerFile::try_lock`]), or where
//! a file lock is mandatory (Windows) the sidecar `<lease_id>.lease.lock` ([`Vfs::lock_beside`]), so the record stays
//! readable. The file holds one 64-byte record ([`LeaseRecord`]): the seq of the documents the handle holds. GC on
//! the pack ([`scan_leases`]) judges each reader lease by its lock, never by its record: one it is granted is stale
//! and removed; one held elsewhere is live, and holds the sweep back unless it is open at the writer's committed seq
//! ([`LeaseScan::judge`]).
//!
//! Nothing about a reader lease is synced (ADR-0054 item 7): it only matters while its process lives, and a machine
//! crash ends every process.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_format::checksum::crc32c;
use persia_storage::container::{Container, ContainerFile, Mode};
use persia_storage::vfs::{EntryKind, FileType, RealFs, Vfs};

use super::pack_path;
use crate::error::{Error, Result};

/// The first bytes of a reader lease record.
pub const LEASE_MAGIC: [u8; 4] = *b"PRLS";
/// The record version this build writes and reads.
pub const LEASE_VERSION: u32 = 1;
/// A version 1 record's length.
pub const LEASE_LEN: usize = 64;
/// What the leases directory's name adds to its pack's: `app.persia.blobs.leases/`.
pub const LEASES_SUFFIX: &str = ".leases";
/// A reader lease file's extension.
pub const LEASE_EXTENSION: &str = ".lease";
/// The most bytes the writer reads of a reader lease: a hostile file costs one bounded read.
pub const LEASE_READ_MAX: u64 = 4096;
/// How many times a take starts again under a new id before it fails ([`Error::LeaseContended`]).
pub const TAKE_ATTEMPTS: u32 = 3;

const ENCODING: &str = "reader lease";

/// The leases directory of the pack at `pack`: `<pack>.leases`.
///
/// ```
/// use std::path::Path;
/// use persia_blob::pack::lease::leases_dir;
/// assert_eq!(leases_dir(Path::new("/db/app.persia.blobs")), Path::new("/db/app.persia.blobs.leases"));
/// ```
pub fn leases_dir(pack: &Path) -> PathBuf {
    let mut name = pack.as_os_str().to_owned();
    name.push(LEASES_SUFFIX);
    PathBuf::from(name)
}

/// The file name of the reader lease `lease_id`: 32 lowercase hex digits and [`LEASE_EXTENSION`].
pub fn lease_file_name(lease_id: u128) -> String {
    format!("{lease_id:032x}{LEASE_EXTENSION}")
}

/// The lease id a reader lease file named `name` holds; `None` for any other name.
pub fn parse_lease_name(name: &OsStr) -> Option<u128> {
    let hex = name
        .as_encoded_bytes()
        .strip_suffix(LEASE_EXTENSION.as_bytes())?;
    parse_hex_id(hex)
}

fn parse_hex_id(hex: &[u8]) -> Option<u128> {
    if hex.len() != 32
        || !hex
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return None;
    }
    u128::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
}

/// The lease id of a Windows lock sidecar named `name` (`<id>.lease.lock`); `None` for any other name.
fn parse_sidecar_name(name: &OsStr) -> Option<u128> {
    let hex = name.as_encoded_bytes().strip_suffix(b".lease.lock")?;
    parse_hex_id(hex)
}

/// One reader lease record (SPEC §9.3, ADR-0054 item 3). **Opening** (no `db_uuid`) while the handle opens its
/// documents; **open** with the container's `db_uuid` and the committed seq of the documents it holds. Built only
/// through [`LeaseRecord::opening`] and [`LeaseRecord::open`], so every value has one encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseRecord {
    pid: u32,
    lease_id: u128,
    db_uuid: [u8; 16],
    manifest_seq: u64,
}

impl LeaseRecord {
    /// The record of a handle still opening its documents.
    pub const fn opening(lease_id: u128, pid: u32) -> Self {
        Self {
            pid,
            lease_id,
            db_uuid: [0; 16],
            manifest_seq: 0,
        }
    }

    /// The record of a handle that holds the documents of `manifest_seq` of the database `db_uuid`. A zero `db_uuid`
    /// is the opening state's: such a database's handles stay opening, which holds GC back (it deletes less).
    pub const fn open(lease_id: u128, pid: u32, db_uuid: [u8; 16], manifest_seq: u64) -> Self {
        if is_zero(&db_uuid) {
            return Self::opening(lease_id, pid);
        }
        Self {
            pid,
            lease_id,
            db_uuid,
            manifest_seq,
        }
    }

    /// The holder's process id, for operators only.
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The id in the file's name.
    pub const fn lease_id(&self) -> u128 {
        self.lease_id
    }

    /// Whether the handle is still opening its documents.
    pub const fn is_opening(&self) -> bool {
        is_zero(&self.db_uuid)
    }

    /// The database of an open record; `None` while opening.
    pub const fn db_uuid(&self) -> Option<[u8; 16]> {
        if self.is_opening() {
            None
        } else {
            Some(self.db_uuid)
        }
    }

    /// The committed seq of the documents an open record's handle holds; `None` while opening.
    pub const fn manifest_seq(&self) -> Option<u64> {
        if self.is_opening() {
            None
        } else {
            Some(self.manifest_seq)
        }
    }

    /// The record's 64 bytes (SPEC §9.3).
    pub fn encode(&self) -> [u8; LEASE_LEN] {
        let mut out = [0_u8; LEASE_LEN];
        out[0..4].copy_from_slice(&LEASE_MAGIC);
        out[4..8].copy_from_slice(&LEASE_VERSION.to_le_bytes());
        // 8..12: flags, none assigned.
        out[12..16].copy_from_slice(&self.pid.to_le_bytes());
        out[16..32].copy_from_slice(&self.db_uuid);
        out[32..48].copy_from_slice(&self.lease_id.to_le_bytes());
        out[48..56].copy_from_slice(&self.manifest_seq.to_le_bytes());
        // 56..60: reserved zero.
        let crc = crc32c(&out[..60]);
        out[60..64].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decodes a record, checking in the order of SPEC §9.3: at least 8 bytes, the magic, `version` (0 is corruption,
    /// above 1 unsupported), exactly 64 bytes, the CRC32C, `flags`, `reserved`, and that an opening record has seq 0.
    /// The name and the pack's `db_uuid` are checked by the caller, who knows them.
    ///
    /// # Errors
    /// [`Error::Corrupt`], [`Error::UnsupportedVersion`] or [`Error::UnsupportedFlags`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (Some(magic), Some(version)) = (bytes.get(0..4), bytes.get(4..8)) else {
            return Err(corrupt("shorter than its magic and version"));
        };
        if magic != LEASE_MAGIC {
            return Err(corrupt("wrong magic"));
        }
        let version = u32::from_le_bytes(word(version));
        if version == 0 {
            return Err(corrupt("version 0"));
        }
        if version > LEASE_VERSION {
            return Err(Error::UnsupportedVersion {
                encoding: ENCODING,
                version,
            });
        }
        let Ok(bytes) = <&[u8; LEASE_LEN]>::try_from(bytes) else {
            return Err(corrupt("not 64 bytes long"));
        };
        let (body, trailer) = bytes.split_at(60);
        if crc32c(body) != u32::from_le_bytes(word(trailer)) {
            return Err(corrupt("checksum mismatch"));
        }
        let flags = u32::from_le_bytes(word(&bytes[8..12]));
        if flags != 0 {
            return Err(Error::UnsupportedFlags {
                encoding: ENCODING,
                flags,
            });
        }
        if bytes[56..60] != [0; 4] {
            return Err(corrupt("reserved bytes are not zero"));
        }
        let mut db_uuid = [0_u8; 16];
        db_uuid.copy_from_slice(&bytes[16..32]);
        let mut id = [0_u8; 16];
        id.copy_from_slice(&bytes[32..48]);
        let mut seq = [0_u8; 8];
        seq.copy_from_slice(&bytes[48..56]);
        let record = Self {
            pid: u32::from_le_bytes(word(&bytes[12..16])),
            lease_id: u128::from_le_bytes(id),
            db_uuid,
            manifest_seq: u64::from_le_bytes(seq),
        };
        if record.is_opening() && record.manifest_seq != 0 {
            return Err(corrupt("an opening record with a seq"));
        }
        Ok(record)
    }
}

const fn is_zero(uuid: &[u8; 16]) -> bool {
    u128::from_le_bytes(*uuid) == 0
}

/// Four bytes as an array; the caller passes exactly four.
fn word(bytes: &[u8]) -> [u8; 4] {
    let mut out = [0_u8; 4];
    if let Some(b) = bytes.get(..4) {
        out.copy_from_slice(b);
    }
    out
}

fn corrupt(what: &'static str) -> Error {
    Error::Corrupt {
        encoding: ENCODING,
        what,
    }
}

fn io_err(op: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::PackIo { op, source }
}

/// The committed seq of the container at `path` and its canonical path, opened read-only again (as `persia verify`
/// does: a compaction renames a new file over the path, SPEC §4.6).
///
/// # Errors
/// [`Error::Container`] when the container cannot be opened (its classification kept: a file that is not a
/// container, or a damaged one, is corruption).
pub fn committed_seq_in<F: ContainerFile + 'static>(
    vfs: Arc<dyn Vfs<File = F>>,
    container: &Path,
) -> Result<(u64, PathBuf)> {
    let container = Container::open_in(vfs, container, Mode::ReadOnly).map_err(Error::Container)?;
    let canonical = container.path().unwrap_or(Path::new("")).to_path_buf();
    Ok((container.superblock().seq, canonical))
}

// -- the reader's side --

/// A read-only handle's reader lease (SPEC §9.3, ADR-0054 item 4): taken before the handle opens its documents, the
/// record rewritten in place as they change ([`ReaderLease::set_open`]), its names removed and its lock released when
/// dropped. When the directory refuses writes (ADR-0054 item 11), it holds nothing ([`ReaderLease::is_held`]) and a
/// warning was logged.
pub struct ReaderLease<F: ContainerFile = std::fs::File> {
    dir: PathBuf,
    taken_seq: u64,
    held: Option<Held<F>>,
}

struct Held<F: ContainerFile> {
    vfs: Arc<dyn Vfs<File = F>>,
    path: PathBuf,
    file: F,
    /// The lock sidecar where the platform uses one (Windows); `None`: `file` itself is locked.
    sidecar: Option<(PathBuf, F)>,
    record: LeaseRecord,
}

impl<F: ContainerFile> fmt::Debug for ReaderLease<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReaderLease")
            .field("dir", &self.dir)
            .field("taken_seq", &self.taken_seq)
            .field("record", &self.held.as_ref().map(|h| h.record))
            .finish()
    }
}

/// How one take attempt ended.
enum Attempt<F: ContainerFile> {
    Held(Held<F>),
    /// Start again under a new id: the lock was not granted, its sidecar could not be opened (a stale removal's delete
    /// pending, Windows), or the name was removed before the lock was taken.
    Again,
}

impl ReaderLease<std::fs::File> {
    /// The reader lease beside the container at `container` on the operating system's file system: in
    /// `<canonical container>.blobs.leases/`, with the container's committed seq read after the lock. The container is
    /// opened twice: once first (its canonical path, and nothing is created beside a file that is not a container),
    /// once after the lock.
    ///
    /// # Errors
    /// As [`ReaderLease::take_in`].
    pub fn take_beside(container: impl AsRef<Path>) -> Result<Self> {
        let vfs: Arc<dyn Vfs<File = std::fs::File>> = Arc::new(RealFs);
        let (_, canonical) = committed_seq_in(Arc::clone(&vfs), container.as_ref())?;
        Self::take_checked(&vfs, &pack_path(&canonical), &canonical)
    }
}

impl<F: ContainerFile + 'static> ReaderLease<F> {
    /// Takes a reader lease in `<pack>.leases/` on `vfs` for the container at `container`, on the same `vfs` (ADR-0054
    /// item 4): opens the container (nothing is created beside a file that is not one), creates the directory if
    /// missing, creates `<id>.lease` holding the opening record, locks it, checks the path still names it, then reads
    /// the container's committed seq again (its **taken seq**). Starts again under a new id when the lock is not
    /// granted, its sidecar or its name is delete pending (Windows), or the name is gone, at most [`TAKE_ATTEMPTS`]
    /// times. When creating the directory or the file fails with permission denied or a read-only file system, the
    /// lease holds nothing and a warning names the path.
    ///
    /// # Errors
    /// [`Error::Container`] when the container cannot be opened; [`Error::LeaseContended`] when every attempt was
    /// refused; [`Error::PackIo`] for another I/O error, `<pack>.leases` that is not a directory included.
    pub fn take_in(vfs: &Arc<dyn Vfs<File = F>>, pack: &Path, container: &Path) -> Result<Self> {
        committed_seq_in(Arc::clone(vfs), container)?;
        Self::take_checked(vfs, pack, container)
    }

    /// [`ReaderLease::take_in`] once the container was found to be one.
    fn take_checked(vfs: &Arc<dyn Vfs<File = F>>, pack: &Path, container: &Path) -> Result<Self> {
        let dir = leases_dir(pack);
        match vfs.create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // A regular file there (not Persia's) fails every read-only open, clearly, and holds GC back.
                if !vfs
                    .is_dir(&dir)
                    .map_err(io_err("stat the reader leases directory"))?
                {
                    return Err(Error::PackIo {
                        op: "use the reader leases directory, which is not a directory",
                        source: io::Error::new(
                            io::ErrorKind::NotADirectory,
                            dir.display().to_string(),
                        ),
                    });
                }
            }
            Err(e) if refuses_writes(&e) => return Ok(Self::unheld(dir, &e)),
            Err(e) => return Err(io_err("create the reader leases directory")(e)),
        }
        let mut held = None;
        for _ in 0..TAKE_ATTEMPTS {
            let path = dir.join(lease_file_name(super::store::random_u128()));
            match attempt(vfs, &path) {
                Ok(Attempt::Held(h)) => {
                    held = Some(h);
                    break;
                }
                Ok(Attempt::Again) => {}
                // ADR-0054 item 11: only a creation the directory refuses opens without a lease.
                Err(AttemptError::Create(e)) if refuses_writes(&e) => {
                    return Ok(Self::unheld(dir, &e));
                }
                Err(AttemptError::Create(e) | AttemptError::Other(e)) => {
                    return Err(io_err("take a reader lease")(e));
                }
            }
        }
        let Some(held) = held else {
            return Err(Error::LeaseContended {
                attempts: TAKE_ATTEMPTS,
            });
        };
        // Read after the lock: every GC that ran before this lease existed had a mark seq at most this one.
        let (taken_seq, _) = committed_seq_in(Arc::clone(&held.vfs), container)?;
        Ok(Self {
            dir,
            taken_seq,
            held: Some(held),
        })
    }

    fn unheld(dir: PathBuf, error: &io::Error) -> Self {
        tracing::warn!(
            path = %dir.display(),
            %error,
            "cannot create a reader lease here: GC may sweep blobs this reader still references",
        );
        Self {
            dir,
            taken_seq: 0,
            held: None,
        }
    }
}

impl<F: ContainerFile> ReaderLease<F> {
    /// The leases directory, `<pack>.leases`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The container's committed seq read once the lease was held: documents older than it may show a blob a GC
    /// swept before the lease existed (`OpenedBeforeLease`). 0 when nothing is held or no container path was given.
    pub const fn taken_seq(&self) -> u64 {
        self.taken_seq
    }

    /// Whether a reader lease is held: `false` when the directory refused writes.
    pub const fn is_held(&self) -> bool {
        self.held.is_some()
    }

    /// The reader lease file, if held.
    pub fn path(&self) -> Option<&Path> {
        self.held.as_ref().map(|h| h.path.as_path())
    }

    /// The record as last written, if held.
    pub fn record(&self) -> Option<LeaseRecord> {
        self.held.as_ref().map(|h| h.record)
    }

    /// Rewrites the record in place to say the handle holds the documents of `manifest_seq` of `db_uuid`. Call it
    /// only once the handle holds no older documents. A failed write is logged, not returned: the record left (the
    /// older one, or a torn one the writer cannot decode) makes GC keep more, never less (ADR-0054 item 6).
    pub fn set_open(&mut self, db_uuid: [u8; 16], manifest_seq: u64) {
        let Some(held) = self.held.as_mut() else {
            return;
        };
        let record = LeaseRecord::open(
            held.record.lease_id(),
            held.record.pid(),
            db_uuid,
            manifest_seq,
        );
        match held.file.write_all_at(&record.encode(), 0) {
            Ok(()) => held.record = record,
            Err(error) => tracing::warn!(
                path = %held.path.display(),
                %error,
                "rewriting the reader lease failed: GC is held back until it is written",
            ),
        }
    }
}

impl<F: ContainerFile> Drop for ReaderLease<F> {
    fn drop(&mut self) {
        let Some(held) = self.held.take() else {
            return;
        };
        // Names first, while the lock is held, then the lock: explicitly, since a forked child's copy of the
        // descriptor would keep it after the close (SPEC §4.5.1).
        remove_logged(&*held.vfs, &held.path);
        if let Some((sidecar, _)) = &held.sidecar {
            remove_logged(&*held.vfs, sidecar);
        }
        let locked = held.sidecar.as_ref().map_or(&held.file, |(_, lock)| lock);
        if let Err(error) = locked.unlock() {
            tracing::warn!(path = %held.path.display(), %error, "releasing a reader lease's lock failed");
        }
    }
}

/// Removes `path`; whether it is gone (removed now, or already). A failure is logged: the writer removes an unlocked
/// file left behind.
fn remove_logged<F: ContainerFile>(vfs: &dyn Vfs<File = F>, path: &Path) -> bool {
    match vfs.remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "removing a reader lease file failed; the writer removes it once it is unlocked",
            );
            false
        }
    }
}

/// Whether `error` says the directory refuses writes (ADR-0054 item 11).
fn refuses_writes(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
    )
}

/// Why a take attempt failed.
enum AttemptError {
    /// Creating the lease file failed: the directory may refuse writes (ADR-0054 item 11).
    Create(io::Error),
    /// Anything after the file was created: never taken for a directory that refuses writes.
    Other(io::Error),
}

/// Whether `error` from a lock or a name check means "not granted, start again": held elsewhere, or (Windows) a
/// delete-pending name after the writer removed a stale lease of the same name (ADR-0054 item 12).
fn not_granted(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::PermissionDenied
    )
}

/// One take attempt at `path`. Only the creation's error is [`AttemptError::Create`]; a refused lock, a sidecar or a
/// name that is delete pending, and a name gone before the lock are [`Attempt::Again`]. A failed attempt removes what
/// it created before it releases its lock.
fn attempt<F: ContainerFile>(
    vfs: &Arc<dyn Vfs<File = F>>,
    path: &Path,
) -> std::result::Result<Attempt<F>, AttemptError> {
    let file = vfs.create_new(path).map_err(AttemptError::Create)?;
    let failed = |e: io::Error| {
        remove_logged(&**vfs, path);
        AttemptError::Other(e)
    };
    let record = LeaseRecord::opening(parse_id(path), std::process::id());
    // Written before the lock: a reader lease the writer finds locked always holds a whole record.
    file.write_all_at(&record.encode(), 0).map_err(failed)?;
    let sidecar = match vfs.lock_beside(path) {
        Ok(Some(lock)) => Some((sidecar_path(path), lock)),
        Ok(None) => match file.try_lock() {
            Ok(true) => None,
            // Refused (the writer may be judging it): this file is not the lease, so it goes.
            Ok(false) => {
                remove_logged(&**vfs, path);
                return Ok(Attempt::Again);
            }
            Err(e) => return Err(failed(e)),
        },
        Err(e) if not_granted(&e) => {
            remove_logged(&**vfs, path);
            return Ok(Attempt::Again);
        }
        Err(e) => return Err(failed(e)),
    };
    // The writer may have judged the new file stale before the lock and removed it, or (Windows) removed the sidecar
    // as an orphan before it was locked: then this lock protects nothing.
    let named = match &sidecar {
        Some((name, lock)) => match vfs.same_file(lock, name) {
            Ok(true) => vfs.same_file(&file, path),
            other => other,
        },
        None => vfs.same_file(&file, path),
    };
    let undo = |sidecar: &Option<(PathBuf, F)>| {
        remove_logged(&**vfs, path);
        if let Some((name, lock)) = sidecar {
            if vfs.same_file(lock, name).unwrap_or(false) {
                remove_logged(&**vfs, name);
            }
            release(lock);
        } else {
            release(&file);
        }
    };
    match named {
        Ok(true) => Ok(Attempt::Held(Held {
            vfs: Arc::clone(vfs),
            path: path.to_owned(),
            file,
            sidecar,
            record,
        })),
        Ok(false) => {
            undo(&sidecar);
            Ok(Attempt::Again)
        }
        Err(e) if not_granted(&e) => {
            undo(&sidecar);
            Ok(Attempt::Again)
        }
        Err(e) => {
            undo(&sidecar);
            Err(AttemptError::Other(e))
        }
    }
}

fn parse_id(path: &Path) -> u128 {
    path.file_name().and_then(parse_lease_name).unwrap_or(0)
}

fn sidecar_path(lease: &Path) -> PathBuf {
    let mut name = lease.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

fn release<F: ContainerFile>(file: &F) {
    if let Err(error) = file.unlock() {
        tracing::warn!(%error, "releasing a reader lease's lock failed");
    }
}

// -- the writer's side --

/// What the writer found of one live reader lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveLease {
    /// Its record decoded and names this pack's database.
    Record(LeaseRecord),
    /// Live, but its record could not be read or decoded, or does not name its file or this database: it holds GC
    /// back. The reason, for the log.
    Unreadable(String),
}

/// One live reader lease the writer found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedLease {
    /// The reader lease file.
    pub path: PathBuf,
    /// What its record says.
    pub lease: LiveLease,
}

/// What [`scan_leases`] found (ADR-0054 item 8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaseScan {
    /// Every live reader lease, in name order.
    pub live: Vec<ScannedLease>,
    /// Reader leases counted live without being opened, where locks cannot be judged (`locks_per_handle` false).
    pub unjudged: u64,
    /// Stale reader leases removed (and lock sidecars without a lease).
    pub stale_removed: u64,
    /// Whether the directory could not be listed (other than missing): then GC is held back.
    pub unlisted: bool,
}

/// The reader leases as GC reports them (ADR-0054 item 9): counts against the writer's committed seq, and whether the
/// sweep was held back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReaderLeaseReport {
    /// Live reader leases.
    pub live: u64,
    /// Live and open at the committed seq: these let the sweep run.
    pub current: u64,
    /// Live and still opening their documents.
    pub opening: u64,
    /// Live and open at a seq below the committed one.
    pub behind: u64,
    /// Live and open at a seq above the committed one (not this writer's history), each logged.
    pub ahead: u64,
    /// Live, with a record that could not be read or decoded, or counted live without being opened.
    pub unreadable: u64,
    /// Stale reader leases removed.
    pub stale_removed: u64,
    /// The lowest seq of an open live reader lease.
    pub oldest_seq: Option<u64>,
    /// Whether the sweep was held back: any live reader lease not open at the committed seq, or a directory that
    /// could not be listed. GC does not sweep then; every commit and compaction puts readers behind until they
    /// refresh.
    pub held_back: bool,
}

impl LeaseScan {
    /// The report of this scan against the writer's `committed` seq. A reader lease ahead of it, or unreadable, is
    /// logged with its path.
    pub fn judge(&self, committed: u64) -> ReaderLeaseReport {
        let mut r = ReaderLeaseReport {
            live: u64_len(self.live.len()).saturating_add(self.unjudged),
            unreadable: self.unjudged,
            stale_removed: self.stale_removed,
            ..ReaderLeaseReport::default()
        };
        for scanned in &self.live {
            let record = match &scanned.lease {
                LiveLease::Unreadable(why) => {
                    tracing::warn!(
                        path = %scanned.path.display(),
                        why = %why,
                        "a live reader lease is unreadable: GC is held back",
                    );
                    r.unreadable = r.unreadable.saturating_add(1);
                    continue;
                }
                LiveLease::Record(record) => record,
            };
            let Some(seq) = record.manifest_seq() else {
                r.opening = r.opening.saturating_add(1);
                continue;
            };
            r.oldest_seq = Some(r.oldest_seq.map_or(seq, |o| o.min(seq)));
            match seq.cmp(&committed) {
                std::cmp::Ordering::Equal => r.current = r.current.saturating_add(1),
                std::cmp::Ordering::Less => r.behind = r.behind.saturating_add(1),
                std::cmp::Ordering::Greater => {
                    tracing::warn!(
                        path = %scanned.path.display(),
                        lease_seq = seq,
                        committed,
                        "a reader lease is ahead of the committed seq (a restored or copied database?): GC is \
                         held back",
                    );
                    r.ahead = r.ahead.saturating_add(1);
                }
            }
        }
        r.held_back = self.unlisted || r.live > r.current;
        r
    }
}

fn u64_len(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The writer's scan of `<pack>.leases/` (ADR-0054 item 8), for the database `db_uuid`. `judge_locks` is
/// [`ContainerFile::locks_per_handle`] of the pack file the writer holds: when `false`, no reader lease is opened
/// (closing one would drop an in-process reader's lock), each counts live ([`LeaseScan::unjudged`]) and none is
/// removed. Never fails: a per-entry error counts that reader lease live and unreadable.
pub fn scan_leases<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    pack: &Path,
    judge_locks: bool,
    db_uuid: [u8; 16],
) -> LeaseScan {
    let dir = leases_dir(pack);
    let mut scan = LeaseScan::default();
    let entries = match vfs.read_dir(&dir) {
        Ok(entries) => entries,
        // No leases only when nothing is there: a regular file of that name (which some file systems list as missing)
        // is not a directory the writer can judge, so it holds GC back.
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                && matches!(vfs.entry_kind(&dir), Err(e) if e.kind() == io::ErrorKind::NotFound) =>
        {
            return scan;
        }
        Err(error) => {
            tracing::warn!(path = %dir.display(), %error, "cannot list the reader leases: GC is held back");
            scan.unlisted = true;
            return scan;
        }
    };
    let mut leases: Vec<(u128, PathBuf)> = Vec::new();
    let mut sidecars: Vec<(u128, PathBuf)> = Vec::new();
    for entry in entries {
        // Only regular files: a FIFO, a symbolic link or a directory of such a name is not Persia's (an open of a
        // FIFO would block).
        if entry.kind != FileType::File {
            continue;
        }
        if let Some(id) = parse_lease_name(&entry.name) {
            leases.push((id, dir.join(&entry.name)));
        } else if let Some(id) = parse_sidecar_name(&entry.name) {
            sidecars.push((id, dir.join(&entry.name)));
        }
    }
    leases.sort();
    if !judge_locks {
        scan.unjudged = u64_len(leases.len());
        return scan;
    }
    for (id, path) in &leases {
        judge_one(vfs, *id, path, db_uuid, &mut scan);
    }
    // A sidecar whose lease is gone (Windows): removed once its lock is granted.
    for (id, sidecar) in sidecars {
        if leases.iter().any(|(l, _)| *l == id) {
            continue;
        }
        let lease = dir.join(lease_file_name(id));
        match vfs.lock_beside(&lease) {
            Ok(Some(lock)) => {
                // A take in progress creates its lease before its sidecar, and a listing may miss the lease: the
                // sidecar goes only if its lease does not exist now.
                let orphan =
                    matches!(vfs.entry_kind(&lease), Err(e) if e.kind() == io::ErrorKind::NotFound);
                if orphan && remove_logged(vfs, &sidecar) {
                    scan.stale_removed = scan.stale_removed.saturating_add(1);
                }
                release(&lock);
            }
            Ok(None) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => tracing::warn!(
                path = %sidecar.display(),
                %error,
                "cannot lock a reader lease's sidecar; the next GC tries again",
            ),
        }
    }
    scan
}

fn judge_one<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    id: u128,
    path: &Path,
    db_uuid: [u8; 16],
    scan: &mut LeaseScan,
) {
    let unreadable = |scan: &mut LeaseScan, why: String| {
        scan.live.push(ScannedLease {
            path: path.to_owned(),
            lease: LiveLease::Unreadable(why),
        });
    };
    // Checked again right before the open, so a FIFO put there since the listing is not opened (an open of one blocks);
    // only a hostile process swapping the name between this check and the open could make it block.
    match vfs.entry_kind(path) {
        Ok(EntryKind::File { .. }) => {}
        // Removed since the listing: by its holder's drop, or an earlier step.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Ok(_) => return,
        Err(e) => return unreadable(scan, format!("stat: {e}")),
    }
    let file = match vfs.open(path, false) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => return unreadable(scan, format!("open: {e}")),
    };
    let granted = match vfs.lock_beside(path) {
        Ok(Some(lock)) => Some(Some(lock)),
        Ok(None) => match file.try_lock() {
            Ok(true) => Some(None),
            Ok(false) => None,
            Err(e) => return unreadable(scan, format!("lock: {e}")),
        },
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
        Err(e) => return unreadable(scan, format!("lock: {e}")),
    };
    if let Some(sidecar) = granted {
        // Stale: its holder is gone, whatever the record says (it is not read). Names while the lock is held, then the
        // lock, explicitly, also after a failed removal (as the side-file sweep does).
        let mut removed = remove_logged(vfs, path);
        if sidecar.is_some() {
            removed &= remove_logged(vfs, &sidecar_path(path));
        }
        release(sidecar.as_ref().unwrap_or(&file));
        if removed {
            scan.stale_removed = scan.stale_removed.saturating_add(1);
        }
        return;
    }
    let lease = match read_record(&file) {
        Ok(bytes) => match LeaseRecord::decode(&bytes) {
            Ok(record) if record.lease_id() != id => {
                LiveLease::Unreadable("the record names another lease id".to_owned())
            }
            Ok(record) if record.db_uuid().is_some_and(|u| u != db_uuid) => {
                LiveLease::Unreadable("the record names another database".to_owned())
            }
            Ok(record) => LiveLease::Record(record),
            Err(e) => LiveLease::Unreadable(e.to_string()),
        },
        Err(e) => LiveLease::Unreadable(format!("read: {e}")),
    };
    scan.live.push(ScannedLease {
        path: path.to_owned(),
        lease,
    });
}

/// At most [`LEASE_READ_MAX`] bytes of `file`, from 0.
fn read_record<F: ContainerFile>(file: &F) -> io::Result<Vec<u8>> {
    let len = file.file_len()?.min(LEASE_READ_MAX);
    let len = usize::try_from(len).unwrap_or(LEASE_LEN);
    let mut buf = vec![0_u8; len];
    file.read_exact_at(&mut buf, 0)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_names_a_take_makes_are_leases() {
        let id = 0x0123_4567_89ab_cdef_0011_2233_4455_6677_u128;
        let name = lease_file_name(id);
        assert_eq!(name, "0123456789abcdef0011223344556677.lease");
        assert_eq!(parse_lease_name(OsStr::new(&name)), Some(id));
        assert_eq!(
            parse_sidecar_name(OsStr::new(&format!("{name}.lock"))),
            Some(id)
        );
        for other in [
            "0123456789ABCDEF0011223344556677.lease",
            "0123456789abcdef001122334455667.lease",
            "0123456789abcdef00112233445566778.lease",
            "0123456789abcdef0011223344556677",
            "0123456789abcdef0011223344556677.lease.tmp",
            "0123456789abcdef0011223344556677.lease.lock",
            "+123456789abcdef0011223344556677.lease",
        ] {
            assert_eq!(parse_lease_name(OsStr::new(other)), None, "{other}");
        }
    }
}
