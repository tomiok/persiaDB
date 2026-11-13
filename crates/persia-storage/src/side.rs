//! Files Persia leaves beside its data: temporary files of writes a crash interrupted, and quarantine copies of a
//! corrupt container tail (ADR-0014). None of them holds acknowledged data, except possibly a quarantine copy, which
//! is for an operator to inspect.

use std::borrow::Cow;
use std::io;
use std::path::PathBuf;

use crate::container::ContainerFile;

/// A file found beside a container or in a local store, from [`LocalStorage::sweep_temporaries`](crate::LocalStorage::sweep_temporaries)
/// or [`Container::side_files`](crate::container::Container::side_files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideFile {
    /// Where it is.
    pub path: PathBuf,
    /// What it is.
    pub kind: SideFileKind,
    /// Whether a live writer is using it.
    pub in_use: InUse,
    /// Whether the call that reported it removed it.
    pub removed: bool,
}

/// What a [`SideFile`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideFileKind {
    /// The temporary file of a write: a local store's `put` (`@tmp.*`, SPEC §10.1.1), or a container's create or
    /// compaction (`<name>.tmp.*`, `<name>.compact.*`, SPEC §4.5.1, §4.6). Left behind, it holds no acknowledged data.
    Temporary,
    /// The copy of a corrupt container tail a recovery made before truncating it (`<name>.quarantine.*`, ADR-0014).
    Quarantine,
}

/// Whether a live writer is using a [`SideFile`]: each one holds the file's lock while it does (on Unix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InUse {
    /// No writer holds it. A temporary file is then stale; a quarantine copy is not in use, but may hold acknowledged
    /// writes (ADR-0014), so `No` alone never means "safe to delete".
    No,
    /// A live writer holds its lock.
    Yes,
    /// Whether a writer uses it cannot be told: files cannot be locked here (Windows, SPEC §4.5.1), or a lock belongs to
    /// the process rather than the handle (a network or FUSE file system, ADR-0019).
    Unknown,
}

/// Takes `file`'s lock if it can: `Some(true)` with the lock held until [`ContainerFile::unlock`], or until
/// `file` and every descriptor duplicated from it (a forked child's) are closed, `Some(false)` if another
/// handle holds it, `None` where files cannot be locked (Windows).
pub(crate) fn try_lock<F: ContainerFile>(file: &F) -> io::Result<Option<bool>> {
    match file.try_lock() {
        Ok(locked) => Ok(Some(locked)),
        Err(err) if err.kind() == io::ErrorKind::Unsupported => Ok(None),
        Err(err) => Err(err),
    }
}

/// Whether a writer uses `file`, found by taking its lock for a moment ([`try_lock`]), and whether this handle took
/// it: then it holds the lock, to release with [`unlock_kept`] if the file stays. A lock taken where locks belong to
/// the process ([`ContainerFile::locks_per_handle`]) proves nothing: [`InUse::Unknown`].
pub(crate) fn in_use<F: ContainerFile>(file: &F) -> io::Result<(InUse, bool)> {
    let locked = try_lock(file)?;
    let per_handle = locked != Some(true) || file.locks_per_handle();
    Ok((InUse::of(locked, per_handle), locked == Some(true)))
}

/// Releases the lock this handle took on a side file it keeps, then returns `result`, the caller's outcome (an unlock
/// failure is added to its error, or becomes the error).
///
/// Closing the file is not enough: `flock` belongs to the open file description, and a child that another thread
/// forked keeps a copy of the descriptor until its `exec` (SPEC §4.5.1). The lock would outlive the listing or sweep,
/// and the next one would find the stale file in use and keep it (ROADMAP 2.3.11). A removed file needs no unlock: its
/// name is gone, so no one can meet the lock. Also called where the lock may belong to the process
/// ([`InUse::Unknown`]): where it does, the close would release it (a lock this process's writer holds included), so
/// the unlock changes nothing; where it belongs to the open file after all, the unlock is needed.
pub(crate) fn unlock_kept<F: ContainerFile, T>(file: &F, result: io::Result<T>) -> io::Result<T> {
    match result {
        Ok(value) => file.unlock().map(|()| value),
        Err(error) => Err(unlock_after(file, error)),
    }
}

/// `error`, after releasing the lock this handle took on a file whose name survives the failure ([`unlock_kept`]):
/// a failed unlock is added to it, never replaces it.
pub(crate) fn unlock_after<F: ContainerFile>(file: &F, error: io::Error) -> io::Error {
    match file.unlock() {
        Ok(()) => error,
        Err(unlock) => then_failed(error, "unlocking the file", unlock),
    }
}

/// `error` with the failure of the step that followed it (`later`, from doing `step`) added. Both stay in the message.
/// The returned `io::Error` has `error`'s kind, but it is a wrapper: its own `raw_os_error()` is `None`. `error` is
/// its [`source`](std::error::Error::source), so the OS error code is read by downcasting that source to `io::Error`
/// and calling `raw_os_error()` on it.
pub(crate) fn then_failed(
    error: io::Error,
    step: impl Into<Cow<'static, str>>,
    later: io::Error,
) -> io::Error {
    io::Error::new(
        error.kind(),
        ThenFailed {
            error,
            step: step.into(),
            later,
        },
    )
}

/// An error, and the failure of a step that followed it ([`then_failed`]).
#[derive(Debug, thiserror::Error)]
#[error("{error}; {step} then failed: {later}")]
struct ThenFailed {
    #[source]
    error: io::Error,
    step: Cow<'static, str>,
    later: io::Error,
}

impl InUse {
    /// The state [`try_lock`] found, given whether locks are per handle on the file's file system.
    pub(crate) fn of(locked: Option<bool>, per_handle: bool) -> Self {
        match locked {
            Some(true) if per_handle => Self::No,
            Some(false) => Self::Yes,
            Some(true) | None => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::InUse;

    #[test]
    fn a_lock_proves_a_file_free_only_where_locks_are_per_handle() {
        assert_eq!(InUse::of(Some(true), true), InUse::No);
        // NFS, FUSE: the lock may be this process's own writer's.
        assert_eq!(InUse::of(Some(true), false), InUse::Unknown);
        assert_eq!(InUse::of(Some(false), true), InUse::Yes);
        assert_eq!(InUse::of(Some(false), false), InUse::Yes);
        // Windows: no lock at all.
        assert_eq!(InUse::of(None, true), InUse::Unknown);
    }
}

#[cfg(test)]
mod unlock_tests {
    use std::io;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::unlock_kept;
    use crate::container::ContainerFile;

    /// A file whose only behaviour is `unlock`: it counts the calls and fails them if `fails`.
    #[derive(Debug)]
    struct Unlocking {
        fails: bool,
        calls: AtomicU32,
    }

    impl ContainerFile for Unlocking {
        fn file_len(&self) -> io::Result<u64> {
            Ok(0)
        }
        fn read_exact_at(&self, _: &mut [u8], _: u64) -> io::Result<()> {
            Ok(())
        }
        fn write_all_at(&self, _: &[u8], _: u64) -> io::Result<()> {
            Ok(())
        }
        fn sync_data(&self) -> io::Result<()> {
            Ok(())
        }
        fn sync_all(&self) -> io::Result<()> {
            Ok(())
        }
        fn set_len(&self, _: u64) -> io::Result<()> {
            Ok(())
        }
        fn try_lock(&self) -> io::Result<bool> {
            Ok(true)
        }
        fn unlock(&self) -> io::Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fails {
                Err(io::Error::other("unlock failed"))
            } else {
                Ok(())
            }
        }
    }

    fn file(fails: bool) -> Unlocking {
        Unlocking {
            fails,
            calls: AtomicU32::new(0),
        }
    }

    fn removal_failed() -> io::Result<u8> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remove failed",
        ))
    }

    /// Every outcome unlocks once; an unlock failure becomes the error, or is added to the caller's, whose kind stays.
    #[test]
    fn the_unlock_runs_once_and_its_failure_is_never_lost() {
        let f = file(false);
        assert_eq!(unlock_kept(&f, Ok(7_u8)).unwrap(), 7);
        assert_eq!(f.calls.load(Ordering::Relaxed), 1);

        let f = file(true);
        let err = unlock_kept(&f, Ok(7_u8)).unwrap_err();
        assert_eq!(err.to_string(), "unlock failed");
        assert_eq!(f.calls.load(Ordering::Relaxed), 1);

        let f = file(false);
        let err = unlock_kept(&f, removal_failed()).unwrap_err();
        assert_eq!(
            (err.kind(), err.to_string()),
            (io::ErrorKind::PermissionDenied, "remove failed".into())
        );
        assert_eq!(f.calls.load(Ordering::Relaxed), 1);

        let f = file(true);
        let err = unlock_kept(&f, removal_failed()).unwrap_err();
        assert_eq!(
            (err.kind(), err.to_string()),
            (
                io::ErrorKind::PermissionDenied,
                "remove failed; unlocking the file then failed: unlock failed".into()
            )
        );
        assert_eq!(f.calls.load(Ordering::Relaxed), 1);
    }

    /// When both fail, the caller's error stays reachable as the source, OS error code included.
    #[test]
    fn a_double_failure_keeps_the_callers_os_error_as_its_source() {
        let removal = io::Error::from_raw_os_error(13);
        let (kind, text) = (removal.kind(), removal.to_string());
        let err = unlock_kept(&file(true), Err::<(), _>(removal)).unwrap_err();
        assert_eq!(err.kind(), kind);
        let source = std::error::Error::source(&err)
            .and_then(|s| s.downcast_ref::<io::Error>())
            .expect("the caller's error is the source");
        assert_eq!(source.raw_os_error(), Some(13));
        assert_eq!(source.to_string(), text);
    }
}
