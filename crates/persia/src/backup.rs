//! `Db::backup` (SPEC §4.8; ROADMAP 10.1.8; ADR-0067): a consistent copy of both files of a database, taken with the
//! writer open and not blocked for the copy.

#[cfg(feature = "test-hooks")]
use std::fmt;
use std::fs::File;
use std::path::Path;
#[cfg(feature = "test-hooks")]
use std::sync::Arc;

use persia_blob::pack_path;
use persia_engine::db::{CommittedImage, CompactionHold, write_backup};

use crate::db::Inner;
use crate::internal::gc::GcHold;
use crate::{Db, Error, ErrorCode, Result};

/// What keeps the files a backup copies from being replaced until the copy ends: no compaction of the container
/// (SPEC §4.6), no GC rewrite of the pack (SPEC §9.3).
pub(crate) struct Holds {
    _compaction: CompactionHold,
    _gc: GcHold,
}

impl Holds {
    pub(crate) fn new(compaction: CompactionHold, gc: GcHold) -> Self {
        Self {
            _compaction: compaction,
            _gc: gc,
        }
    }
}

/// The images a backup writes, and the holds that live until it is written.
pub(crate) struct Images {
    container: CommittedImage<File>,
    pack: Option<CommittedImage<File>>,
    /// The `seq` of the commit the container's image holds.
    seq: u64,
    /// `None` on a read-only database: its writer is another process's (or none), whose files it does not hold.
    _holds: Option<Holds>,
}

impl Images {
    pub(crate) fn new(
        container: CommittedImage<File>,
        pack: Option<CommittedImage<File>>,
        seq: u64,
        holds: Option<Holds>,
    ) -> Self {
        Self {
            container,
            pack,
            seq,
            _holds: holds,
        }
    }
}

#[cfg(feature = "test-hooks")]
/// Test hook: where a backup is (ADR-0064 item 9's kind of hook), so a test can write, commit or collect garbage
/// while it copies.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackupStep {
    /// The images are taken and the writer mutex released; nothing is copied yet.
    Taken {
        /// The `seq` of the commit the backup holds.
        seq: u64,
    },
}

#[cfg(feature = "test-hooks")]
/// Test hook: called by [`Db::backup`] at each [`BackupStep`], on the caller's thread.
#[doc(hidden)]
#[derive(Clone)]
pub struct BackupHook(pub Arc<dyn Fn(BackupStep) + Send + Sync>);

#[cfg(feature = "test-hooks")]
impl fmt::Debug for BackupHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BackupHook")
    }
}

impl Db {
    /// Writes a copy of the database to `dest`, a path (or a `file://` URL) where nothing is yet (SPEC §4.8): the
    /// container to `dest` and, if the database has stored a blob, its pack to `<dest>.blobs` beside it. The copy
    /// opens as the database was at one commit, with every blob its documents reference.
    ///
    /// On a read-write database the backup first commits, so it holds every write that returned before the call;
    /// it then takes the committed state of the container and then of the pack under the writer mutex (a commit's
    /// time), and copies them with the mutex released: writes, commits and reads go on during the copy, and none
    /// of them is in it. Until the copy ends no compaction rewrites the container and no blob GC rewrites the pack.
    /// On a read-only database it copies the last commit of the state the database shows (its open's or its last
    /// [`refresh`](Db::refresh)'s), without the writes the open replayed after that commit.
    ///
    /// The copy is durable and whole, or absent: each file is written under a temporary name beside its
    /// destination and synced, then linked to its name (which must still be free) with its directory synced, the
    /// pack before the container. A failure removes what it linked, the container first, each removal synced before
    /// the next. A crash may leave `<dest>.blobs` without `dest` (remove it before trying again) and stale temporary
    /// files `<name>.backup.<nonce>.<n>`, never a `dest` without its pack. `dest` is never replaced: back up to a new
    /// name. The file system must support hard links. A temporary name that cannot be removed once the backup is
    /// complete is logged and left (it is not part of the backup), and the call succeeds.
    ///
    /// The bytes are copied as they are: their checksums are not checked on the way (SPEC §4.8). Run `verify` on the
    /// copy to check it. A writer that a failed write poisoned cannot commit, so its backup fails with
    /// [`ErrorCode::Poisoned`]: close and reopen the database first.
    ///
    /// ```
    /// use persia::{Db, ErrorCode};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let db = Db::open(dir.path().join("app.persia"))?;
    /// let notes = db.create_collection_json(r#"{"name": "notes", "fields": {}}"#)?;
    /// notes.put_json(r#"{"id": "n1"}"#)?;
    /// let dest = dir.path().join("backup.persia");
    /// db.backup(&dest)?;
    /// // A later write is not in the backup.
    /// notes.put_json(r#"{"id": "n2"}"#)?;
    /// let copy = Db::open(&dest)?;
    /// assert!(copy.collection("notes")?.get("n1")?.is_some());
    /// assert!(copy.collection("notes")?.get("n2")?.is_none());
    /// // The destination is never replaced.
    /// assert_eq!(db.backup(&dest).unwrap_err().code(), ErrorCode::AlreadyExists);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`ErrorCode::AlreadyExists`] if anything is at `dest` or `<dest>.blobs` (nothing is written);
    /// [`ErrorCode::InvalidArgument`] for an empty or malformed `dest`, [`ErrorCode::Unsupported`] for a URL of
    /// another scheme; [`ErrorCode::Io`] if the file system fails (also a missing directory of `dest`, or one without
    /// hard links); [`ErrorCode::Corrupt`] if a file is shorter than its last commit; as [`Db::commit`] for the commit
    /// of a read-write database, [`ErrorCode::Poisoned`] among them; [`ErrorCode::Closed`]. Do not take an error to
    /// mean `dest` is free: an `Io` error whose message says the backup was written comes from the last directory
    /// sync, after both files are complete at their names (they may not survive a crash), and a removal during the
    /// clean-up of a failed backup may itself fail (logged), leaving `dest` and its pack complete.
    pub fn backup(&self, dest: impl AsRef<Path>) -> Result<()> {
        self.inner.backup(dest.as_ref())
    }
}

impl Inner {
    /// [`Db::backup`], on the state every handle shares (the async API runs it on its pool).
    pub(crate) fn backup(&self, dest: &Path) -> Result<()> {
        self.check_open()?;
        let dest = crate::location::path_of(dest)?;
        let pack_dest = pack_path(&dest);
        for path in [&dest, &pack_dest] {
            // Not following a symbolic link: a dangling one is a name that exists too.
            let taken = match std::fs::symlink_metadata(path) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(source) => {
                    return Err(Error::with_source(
                        ErrorCode::Io,
                        format_args!("looking for {}", path.display()),
                        source,
                    ));
                }
            };
            if taken {
                return Err(Error::new(
                    ErrorCode::AlreadyExists,
                    format!(
                        "cannot back up to {}: {} exists, and a backup never replaces a file",
                        dest.display(),
                        path.display()
                    ),
                ));
            }
        }
        let images = self.committed_images()?;
        #[cfg(feature = "test-hooks")]
        if let Some(hook) = &self.options.backup_hook {
            (hook.0)(BackupStep::Taken { seq: images.seq });
        }
        let mut files: Vec<(&Path, &CommittedImage<File>)> = Vec::with_capacity(2);
        if let Some(pack) = &images.pack {
            files.push((pack_dest.as_path(), pack));
        }
        files.push((dest.as_path(), &images.container));
        write_backup(&files).map_err(|e| {
            Error::wrap(e).context(format_args!(
                "backing up {} (seq {}) to {}",
                self.path().display(),
                images.seq,
                dest.display()
            ))
        })
    }
}
