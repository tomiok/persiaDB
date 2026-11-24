//! The engine's one `unsafe` call (ADR-0017, amended; listed in `UNSAFE_ALLOWED`): the mapping of a read-only
//! database's container, the default of read handles on local disks (SPEC §4.7, owner decision n). No safe public
//! function of `persia-storage` maps a file. The engine takes on the mapping contract here, for the containers its
//! read-only open by path opens itself, and states what it cannot rule out in the documentation of
//! [`super::MappedReads::IfLocal`] and SPEC §4.7.
#![allow(unsafe_code)]

use std::fs::File;

use persia_storage::container::{Container, Result};

/// Maps `container`'s committed frames if it is read-only, opened by Persia by path, and on a local disk
/// ([`Container::map_read_only_if_local`]); returns whether it mapped.
///
/// Called only by [`super::ReadOnlyDatabase::open_with_options`] (and the refreshes of a database it opened) on a
/// container it opened itself with [`Container::open`] in [`persia_storage::container::Mode::ReadOnly`], or reopened
/// from one.
pub(super) fn map_read_only_if_local(container: &mut Container<File>) -> Result<bool> {
    // SAFETY: `map_read_only_if_local`'s contract asks that nothing but Persia writes, truncates or hole-punches the
    // file below the mapped `log_end` while the mapping lives, and that no superblock slot is damaged meanwhile.
    // What Persia can rule out, the call rules out itself: it maps only a read-only container whose `File` Persia
    // opened by path and never hands out, on an allow-listed local disk where the writer lock keeps a second Persia
    // writer out, and Persia's writers never change bytes below a committed `log_end` (its own `SAFETY` comment).
    // The rest (another program, or a handle of the file this process opens on its own, writing or truncating the
    // container; a slot damaged later) cannot be checked by any code. The owner accepted it as the residual of the
    // default of read-only handles (decision n, ADR-0017 amended 2026-11-23): `MappedReads::IfLocal` documents it as a
    // condition of deploying a reader with that default, and `MappedReads::Never` is the way out where it cannot be
    // excluded.
    unsafe { container.map_read_only_if_local() }
}
