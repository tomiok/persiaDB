//! Opening packs in tests (ROADMAP 9.2.7, ADR-0053): a read-write open takes a real [`WriterProof`], minted by a real
//! read-write [`Container`] on a simulated file, never a test-only constructor.
#![allow(dead_code, clippy::unwrap_used, missing_docs)] // Each test binary uses part of it.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use persia_blob::{PackMode, PackStore, WriterProof};
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Container, ContainerFile};
use persia_storage::vfs::Vfs;

/// The writer proof of a read-write container with `db_uuid`. The container lives for the whole test binary (one per
/// `db_uuid`, kept in a static), so it outlives every pack opened with its proof, as the proof's contract asks.
pub fn writer_proof(db_uuid: [u8; 16]) -> WriterProof<'static> {
    static WRITERS: Mutex<Vec<&'static Container<SimFile>>> = Mutex::new(Vec::new());
    let mut writers = WRITERS.lock().unwrap_or_else(PoisonError::into_inner);
    let container = if let Some(c) = writers.iter().find(|c| c.header().db_uuid == db_uuid) {
        *c
    } else {
        let c: &'static Container<SimFile> = Box::leak(Box::new(
            Container::create_on(SimFile::new(), db_uuid).unwrap(),
        ));
        writers.push(c);
        c
    };
    container.writer_proof().unwrap()
}

/// The pack at `path` on `vfs` in `mode`: read-write with [`writer_proof`] of `db_uuid`, read-only with `db_uuid`.
pub fn open_pack<F: ContainerFile + 'static>(
    vfs: Arc<dyn Vfs<File = F>>,
    path: impl AsRef<Path>,
    mode: PackMode,
    db_uuid: [u8; 16],
) -> persia_blob::Result<PackStore<F>> {
    match mode {
        PackMode::ReadWrite => PackStore::open_writer_in(vfs, path, &writer_proof(db_uuid)),
        PackMode::ReadOnly => PackStore::open_in(vfs, path, db_uuid),
    }
}
