//! The blob pack's one `unsafe` call (ROADMAP 10.1.23; ADR-0017, amended; listed in `UNSAFE_ALLOWED`): the mapping of
//! a read-only pack on a local disk, the pack's side of the default of read handles (SPEC §4.7, §9.3, owner decision
//! n). No safe public function of `persia-storage` maps a file; this module takes on the mapping contract of
//! [`MapPermit::read_only_if_local`] for the read-only packs [`PackStore::open_mapped_if_local`] opens itself, as the
//! engine's `db/read_only/mapped.rs` does for the container.
#![allow(unsafe_code)]

#[cfg(doc)]
use super::PackStore;
use super::{ByteSource, File, MapFn, MapPermit, Range, io};

/// What maps a read-only pack [`PackStore::open_mapped_if_local`] opened (and the [`PackStore::reopen`] of one):
/// `12288..log_end` of a committed slot of the pack's file, if the file is on a local disk; `Ok(None)` otherwise, and
/// the pack reads with `pread`.
pub(super) fn read_only_if_local() -> MapFn<File> {
    Box::new(map_read_only_if_local)
}

/// Maps `range` of `file`, a read-only pack's, if it is on a local disk.
fn map_read_only_if_local(file: &File, range: Range<u64>) -> io::Result<Option<ByteSource>> {
    // SAFETY: `MapPermit::read_only_if_local`'s contract is `MapPermit::new`'s for a read-only pack handle whose file
    // Persia opened by path and never hands out. What Persia can rule out holds by construction: this function is
    // reached only from the `Mapper` of a `PackStore` that `PackStore::open_mapped_if_local` opened, or that the
    // `reopen` of one opened: a `PackMode::ReadOnly` handle (it never writes, truncates or creates the file, and never
    // remaps over a file it changed), whose `File`s are opened by the store itself, read-only, by path, on the operating
    // system's file system (`RealFs`). They never leave the store but as a duplicate of the read-only handle inside a
    // `CommittedImage` (`PackStore::committed_image`), which only reads from it (`write_to` reads it and writes another
    // file): a duplicate shares the read-only open, so no write or truncation can go through it. It maps only `12288..log_end` of a slot it read on that file (the
    // `Mapper`'s `remap`), bytes Persia's writers never change again: frames are append-only, a writer's recovery
    // truncates only past the committed `log_end`, and a GC rewrite writes a new file and renames it over the path,
    // which this handle keeps reading until it is reopened (SPEC §9.3, ROADMAP 9.2.13). The pack's writer lock keeps a
    // second Persia writer out (ROADMAP 9.2.15), and on the allow-listed local file systems it belongs to the open
    // file, so the per-process-lock exception (NFS) cannot arise. The rest (another program, or a handle of the file
    // this process opens on its own, writing or truncating the pack; a slot damaged later) cannot be checked by any
    // code: it is the residual the owner accepted for the default of read-only handles (decision n, ADR-0017 amended
    // 2026-11-23 and 2026-11-26), stated in SPEC §4.7 and in `MappedReads::IfLocal`'s documentation, with ordinary
    // reads (`OpenOptions::mapped_reads(false)`) as the way out.
    match unsafe { MapPermit::read_only_if_local(file) } {
        Some(permit) => permit.map(file, range).map(Some),
        None => Ok(None),
    }
}
