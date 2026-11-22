//! The sweep of the local pack (SPEC §9.3, §9.5; ROADMAP 9.5.2): a rewrite into a new file
//! (`PackStore::rewrite`, in `pack::store::rewrite`), since a pack is never changed in place below `log_end`.

use persia_storage::container::ContainerFile;

use super::{Mark, PinGate, Sweep, SweepOptions, SweepReport};
use crate::error::Result;
use crate::pack::PackStore;
use crate::pack::lease::LeaseScan;

impl<F: ContainerFile + 'static> Sweep for PackStore<F> {
    /// Rewrites the pack without what the mark and the pins do not keep, when enough is dropped
    /// ([`SweepOptions::rewrite_ratio`]). There is no grace period locally: only this writer writes the pack, local
    /// uploads are not resumable (ADR-0049), and the pins cover every `put_blob` in flight. A read-only handle in
    /// another process keeps the file it opened only until `refresh` returns
    /// [`Error::PackReplaced`](crate::Error::PackReplaced); nothing reopens or remaps on that error (ROADMAP 9.2.13).
    fn sweep<P: PinGate>(
        &self,
        mark: &Mark,
        pins: &P,
        options: &SweepOptions,
    ) -> Result<SweepReport> {
        self.rewrite(mark, pins, options.rewrite_ratio)
    }

    fn reader_leases(&self) -> Result<Option<LeaseScan>> {
        PackStore::reader_leases(self)
    }
}
