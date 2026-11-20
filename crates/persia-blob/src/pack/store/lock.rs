//! The pack's writer lock (SPEC §9.3; ROADMAP 9.2.7; ADR-0053): what makes one read-write handle per pack a fact, not
//! a contract. The writer proof is the API gate; this lock is the enforcement.
//!
//! The primitive is the container's (SPEC §4.5.1): [`ContainerFile::try_lock`] on the pack file itself (`flock` on
//! Unix, a per-handle lock on a `SimFs`), or on Windows, where a file lock is mandatory and would keep readers out,
//! the locked `<name>.persia.blobs.lock` file beside the pack ([`Vfs::lock_beside`]).
//!
//! - A read-write open takes the lock beside the pack, or the lock on an existing pack file, before it sweeps or
//!   recovers anything; a pack opened just as a rewrite renamed a new file over it is reopened (the lock must be on
//!   the file the path names).
//! - The creation of the pack and its rewrite lock their new file before writing it, so the path never names an
//!   unlocked pack while the writer lives; the old file's lock is released only when the handle drops it, after the
//!   rename (ADR-0052). The lock beside the pack names no file the rewrite replaces.
//! - A creation or rewrite that fails after its new file may name the path keeps that file locked for the handle's life
//!   (the handle is poisoned and writes nothing more), so the path never names a pack a second writer could lock.
//! - The lock is held until the handle (and its file) is dropped, however long other owners keep the handle alive.
//! - Before the pack file exists (Unix) there is nothing to lock: two read-write handles opened then can both exist,
//!   and the one whose creation loses the race to link its file fails at its first write, poisoned.

use std::path::Path;

use persia_storage::vfs::Vfs;

use super::{ContainerFile, Error, Result, io, io_err};

/// How often a read-write open retries a pack a rewrite renamed over the path while it was taking the lock.
const OPEN_ATTEMPTS: u32 = 8;

/// A locked file whose lock is released when dropped: the lock file beside the pack (Windows), or a new pack file a
/// rewrite may have renamed over the path when its rename reported an error (kept locked for the handle's life).
#[derive(Debug)]
pub(super) struct Held<F: ContainerFile>(pub(super) F);

impl<F: ContainerFile> Drop for Held<F> {
    fn drop(&mut self) {
        release(&self.0);
    }
}

/// Releases `file`'s lock, logging a failure: [`Drop`] cannot return it, and closing the descriptor next releases the
/// lock anyway while no duplicate remains.
pub(super) fn release<F: ContainerFile>(file: &F) {
    if let Err(error) = file.unlock() {
        tracing::warn!(%error, "releasing the blob pack's writer lock failed");
    }
}

/// The lock beside the pack at `path`, where the platform uses one (`None`: the pack file itself is locked).
///
/// # Errors
/// [`Error::PackLocked`] if another handle holds it, or [`Error::PackIo`].
pub(super) fn lock_beside<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    path: &Path,
) -> Result<Option<Held<F>>> {
    match vfs.lock_beside(path) {
        Ok(file) => Ok(file.map(Held)),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(Error::PackLocked),
        Err(e) => Err(io_err("lock")(e)),
    }
}

/// Takes the writer lock on `file` without waiting.
///
/// # Errors
/// [`Error::PackLocked`] if another handle holds it, or [`Error::PackIo`].
pub(super) fn lock<F: ContainerFile>(file: &F) -> Result<()> {
    match file.try_lock() {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::PackLocked),
        Err(e) => Err(io_err("lock")(e)),
    }
}

/// Opens the pack at `path` read-write and locks it; `None` when there is no pack file. The lock is taken on the file
/// the path still names after locking: one a rewrite renamed away meanwhile is unlocked and the path opened again.
///
/// # Errors
/// [`Error::PackLocked`] if another handle holds the lock (or the path kept changing under it), or [`Error::PackIo`].
pub(super) fn open_locked<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    path: &Path,
) -> Result<Option<F>> {
    for _ in 0..OPEN_ATTEMPTS {
        let file = match vfs.open(path, true) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("open")(e)),
        };
        lock(&file)?;
        match vfs.same_file(&file, path) {
            Ok(true) => return Ok(Some(file)),
            Ok(false) => release(&file),
            Err(e) => {
                release(&file);
                return Err(io_err("stat")(e));
            }
        }
    }
    // The path changed under every attempt: a writer is busy rewriting it (as the container's open, SPEC §4.5.1).
    Err(Error::PackLocked)
}

/// `error`, after releasing `file`'s lock if `locked`: a failed open must unlock, not only close, since a forked
/// copy of the descriptor keeps the lock after the close (SPEC §4.5.1).
pub(super) fn unlock_after<F: ContainerFile>(file: &F, locked: bool, error: Error) -> Error {
    if locked {
        release(file);
    }
    error
}

/// Unlocks `file` on drop while [`locked`](Self::locked). A failed create or rewrite must unlock, not only close
/// (SPEC §4.5.1): a forked copy of the descriptor keeps the lock after the close. Clear `locked` to keep the lock
/// (the caller takes the file).
pub(super) struct UnlockOnDrop<'a, F: ContainerFile> {
    pub(super) file: &'a F,
    pub(super) locked: bool,
}

impl<F: ContainerFile> Drop for UnlockOnDrop<'_, F> {
    fn drop(&mut self) {
        if self.locked {
            release(self.file);
        }
    }
}
