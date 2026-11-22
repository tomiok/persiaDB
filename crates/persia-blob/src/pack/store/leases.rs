//! The writer's scan of the pack's reader leases (SPEC §9.5, ROADMAP 9.2.16, ADR-0054 item 8): GC runs it after the
//! mark and before the sweep, holding the pack's writer lock.

use persia_storage::container::ContainerFile;

use super::{Error, PackMode, PackStore, Result};
use crate::pack::lease::{LeaseScan, scan_leases};

impl<F: ContainerFile + 'static> PackStore<F> {
    /// Scans `<pack>.leases/` ([`scan_leases`]): stale reader leases are removed, live ones read. Whether locks can be
    /// judged is asked of the pack file this handle holds, before any reader lease is opened. `None` when there is
    /// no pack file yet (nothing to sweep).
    ///
    /// # Errors
    /// [`Error::PackReadOnly`] on a read-only handle: only the writer removes stale reader leases.
    pub fn reader_leases(&self) -> Result<Option<LeaseScan>> {
        if self.mode != PackMode::ReadWrite {
            return Err(Error::PackReadOnly);
        }
        let judge_locks = match self.read().pack.as_ref() {
            Some(pack) => pack.file.locks_per_handle(),
            None => return Ok(None),
        };
        Ok(Some(scan_leases(
            &*self.vfs,
            &self.path,
            judge_locks,
            self.db_uuid,
        )))
    }
}
