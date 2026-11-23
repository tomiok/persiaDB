//! How `persia stats` and `persia dump documents` open a database (ROADMAP 10.4.1): read-only and without the writer
//! lock (SPEC §8.4), so a live writer may have it open, and with the built-in analyzers only.
//!
//! A schema may name an analyzer registered by the application (SPEC §6.2.2), which this command cannot load. The open
//! skips the analysis of those `text` fields while it replays the WAL, as `persia verify` does (ROADMAP 6.6.8): the
//! documents are read whole, and neither command searches.

use std::path::Path;

use anyhow::{Context, Result, bail};
use persia_engine::db::{DatabaseOptions, ReadOnlyDatabase};

/// Opens the database in the container file at `path` read-only. It writes nothing and creates nothing: a path that
/// is not a file fails before any open.
pub fn read_only(path: &Path) -> Result<ReadOnlyDatabase> {
    if !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    let options = DatabaseOptions::default();
    ReadOnlyDatabase::open_skipping_analysis(path, &options.registry)
        .with_context(|| format!("opening {}", path.display()))
}
