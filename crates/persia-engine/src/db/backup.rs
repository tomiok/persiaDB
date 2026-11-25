//! A copy of a database's committed state, and the writing of a backup (SPEC §4.8; ROADMAP 10.1.8; ADR-0067).
//!
//! `persia` reaches the file system only through the engine (and `persia-blob`), so the storage primitives are
//! offered here: [`Database::committed_image`] and [`ReadOnlyDatabase::committed_image`] take the container's
//! [`CommittedImage`], and [`write_backup`] writes images to new files durably, all or nothing.

use std::fs::File;
use std::path::Path;

pub use persia_storage::container::CommittedImage;
use persia_storage::container::{self, ContainerFile};
use persia_storage::vfs::{RealFs, Vfs};

use super::{Database, DatabaseError, ReadOnlyDatabase};

impl<F: ContainerFile> Database<F> {
    /// The container's last commit as a [`CommittedImage`] (SPEC §4.8): the state an open of a copy finds, without
    /// what was logged after it. Take it right after a [`Database::commit`] to copy every write that returned, and
    /// hold [`Database::hold_compaction`] until the copy ends.
    ///
    /// ```
    /// use persia_engine::db::{Database, DatabaseOptions, write_backup};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let mut db = Database::create(dir.path().join("app.persia"), DatabaseOptions::default())?;
    /// db.commit()?;
    /// let hold = db.hold_compaction();
    /// let image = db.committed_image()?;
    /// let dest = dir.path().join("backup.persia");
    /// write_backup(&[(dest.as_path(), &image)])?;
    /// drop(hold);
    /// assert_eq!(Database::open(&dest, DatabaseOptions::default())?.seq(), db.seq());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`DatabaseError::Container`] as [`container::Container::committed_image`].
    pub fn committed_image(&self) -> Result<CommittedImage<F>, DatabaseError> {
        Ok(self.container().committed_image()?)
    }
}

impl<F: ContainerFile> ReadOnlyDatabase<F> {
    /// The last commit of the container this reader's state was read from (its open's, or the last refresh's), as a
    /// [`CommittedImage`] (SPEC §4.8). The WAL records the reader replayed after that commit are not in it.
    ///
    /// ```
    /// use persia_analysis::AnalyzerRegistry;
    /// use persia_engine::db::{Database, DatabaseOptions, ReadOnlyDatabase};
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// Database::create(&path, DatabaseOptions::default())?.commit()?;
    /// let reader = ReadOnlyDatabase::open(&path, &AnalyzerRegistry::builtin())?;
    /// assert_eq!(reader.committed_image()?.len(), reader.container().superblock().log_end);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`DatabaseError::Container`] as [`container::Container::committed_image`].
    pub fn committed_image(&self) -> Result<CommittedImage<F>, DatabaseError> {
        Ok(self.container().committed_image()?)
    }
}

/// [`write_backup_in`] on the operating system's file system.
///
/// # Errors
/// As [`write_backup_in`].
pub fn write_backup(files: &[(&Path, &CommittedImage<File>)]) -> Result<(), DatabaseError> {
    write_backup_in(&RealFs, files)
}

/// Writes each image to its path on `vfs` as a new file, durably and all or nothing, linked in the order given
/// ([`container::write_backup_in`]): the blob pack before the container, so a container never survives a crash
/// without the pack it references.
///
/// # Errors
/// [`DatabaseError::Container`]: `AlreadyExists` if a path exists, `Io` if the file system fails.
pub fn write_backup_in<V: Vfs + ?Sized, F: ContainerFile>(
    vfs: &V,
    files: &[(&Path, &CommittedImage<F>)],
) -> Result<(), DatabaseError> {
    Ok(container::write_backup_in(vfs, files)?)
}
