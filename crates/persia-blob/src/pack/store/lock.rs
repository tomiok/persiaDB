//! The pack's writer lock (SPEC §9.3; ROADMAP 9.2.7, 9.2.15; ADR-0053): what makes one read-write handle per pack a
//! fact, not a contract. The writer proof is the API gate; this lock is the enforcement.
//!
//! One mechanism on every platform: the locked file `<name>.persia.blobs.lock` beside the pack ([`lock_path`]). On
//! Windows it is what [`Vfs::lock_beside`] takes (a mandatory lock on the pack itself would keep readers out); where
//! that returns `None` (Unix, a `SimFs`), the pack opens or creates the same file itself and takes the container's
//! primitive on it, [`ContainerFile::try_lock`] (`flock` on Unix, a per-handle lock on a `SimFs`; SPEC §4.5.1).
//!
//! - A read-write open takes it before it sweeps, recovers or creates anything, so a second read-write handle fails
//!   with [`Error::PackLocked`] even while no pack file exists yet.
//! - It names no file a creation links or a rewrite renames, so neither has to lock its new file: the pack's path can
//!   name any file while the writer lives, and the lock stays where it is.
//! - It is held until the handle is dropped, however long other owners (an `Arc` clone that outlives the container)
//!   keep the handle alive, and released before the close (a forked copy of the descriptor would keep it otherwise).
//! - A symbolic link at that name is followed (the lock is its target's); one to nothing is an error. The listing
//!   and sweep of the container's side files never open such a link, and keep the pack's temporary files.
//! - The file is created if missing and never removed by Persia: removing it would let a second writer lock a new file
//!   of that name while the first holds the old one. It holds no data, so its creation is not synced: a crash that
//!   loses it loses no lock (locks do not survive the process).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use persia_storage::vfs::{EntryKind, Vfs};

use super::{ContainerFile, Error, Result, io, io_err};

/// How often the open retries a lock file found and then gone before it could be opened (someone removed it).
const OPEN_ATTEMPTS: u32 = 8;

/// A locked file whose lock is released when dropped: the lock file beside the pack.
#[derive(Debug)]
pub(super) struct Held<F: ContainerFile>(F);

impl<F: ContainerFile> Drop for Held<F> {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            // `Drop` cannot return it; closing the descriptor next releases the lock anyway while no duplicate remains.
            tracing::warn!(%error, "releasing the blob pack's writer lock failed");
        }
    }
}

/// `<pack>.lock`: the writer lock file beside the pack at `pack`.
pub(super) fn lock_path(pack: &Path) -> PathBuf {
    let mut name = OsString::from(pack.as_os_str());
    name.push(".lock");
    PathBuf::from(name)
}

/// Takes the writer lock beside the pack at `path` without waiting, creating the lock file if missing.
///
/// # Errors
/// [`Error::PackLocked`] if another handle holds it, or [`Error::PackIo`].
pub(super) fn lock_beside<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    path: &Path,
) -> Result<Held<F>> {
    match vfs.lock_beside(path) {
        Ok(Some(file)) => return Ok(Held(file)),
        Ok(None) => {}
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Err(Error::PackLocked),
        Err(e) => return Err(io_err("lock")(e)),
    }
    let file = open_or_create(vfs, &lock_path(path))?;
    match file.try_lock() {
        Ok(true) => Ok(Held(file)),
        Ok(false) => Err(Error::PackLocked),
        Err(e) => Err(io_err("lock")(e)),
    }
}

/// The lock file at `path`, opened read-write, created if missing.
fn open_or_create<F: ContainerFile>(vfs: &dyn Vfs<File = F>, path: &Path) -> Result<F> {
    for _ in 0..OPEN_ATTEMPTS {
        match vfs.create_new(path) {
            Ok(file) => return Ok(file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err("create the lock file")(e)),
        }
        match vfs.open(path, true) {
            Ok(file) => return Ok(file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // The name exists (the create said so) but opens to nothing: a symbolic link to nothing, which the
                // create will never replace. Anything else was removed between the two calls: create it again.
                if matches!(vfs.entry_kind(path), Ok(EntryKind::Other)) {
                    return Err(Error::PackIo {
                        op: "open the lock file",
                        source: io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("{} is a symbolic link to nothing", path.display()),
                        ),
                    });
                }
            }
            Err(e) => return Err(io_err("open the lock file")(e)),
        }
    }
    Err(Error::PackIo {
        op: "open the lock file",
        source: io::Error::other("the lock file kept disappearing"),
    })
}
