//! The pack's committed image (SPEC §4.8, §9.3; ROADMAP 10.1.8; ADR-0067): a copy opens as a pack holding exactly the
//! blobs bound when the image was taken, whatever is staged, bound or rewritten afterwards.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)] // Tests: inputs are fixed.

mod gc_support;
mod pack_support;

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gc_support::{Pins, data_of, put, read_all, stage};
use persia_blob::gc::{Mark, Sweep, SweepOptions};
use persia_blob::{BlobStore, PackMode, PackStore};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const PACK: &str = "/db/app.persia.blobs";
const COPY: &str = "/db/copy.persia.blobs";
const DB: [u8; 16] = [9; 16];

fn sim() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    fs
}

fn open(fs: &SimFs, path: &str, mode: PackMode) -> PackStore<SimFsFile> {
    pack_support::open_pack(Arc::new(fs.clone()), path, mode, DB).unwrap()
}

fn write_copy(fs: &SimFs, store: &PackStore<SimFsFile>) {
    let image = store.committed_image().unwrap().unwrap();
    let target = fs.create_new(Path::new(COPY)).unwrap();
    image.write_to(&target).unwrap();
    assert_eq!(image.len(), store.superblock().unwrap().log_end);
}

#[test]
fn no_pack_has_no_image() {
    let fs = sim();
    assert!(
        open(&fs, PACK, PackMode::ReadWrite)
            .committed_image()
            .unwrap()
            .is_none()
    );
    assert!(
        open(&fs, PACK, PackMode::ReadOnly)
            .committed_image()
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_copy_holds_the_blobs_bound_when_the_image_was_taken() {
    let fs = sim();
    let store = open(&fs, PACK, PackMode::ReadWrite);
    let kept = put(&store, &data_of(3000, 1), 1024, 1);
    // Staged, not bound: after `log_end`, never in a copy.
    let staged = stage(&store, &data_of(500, 2), 1024, 2);
    let image = store.committed_image().unwrap().unwrap();
    // Bound after the image: not in the copy either.
    let later = put(&store, &data_of(700, 3), 1024, 3);
    let target = fs.create_new(Path::new(COPY)).unwrap();
    image.write_to(&target).unwrap();
    drop(target);

    let copy = open(&fs, COPY, PackMode::ReadOnly);
    assert_eq!(copy.blob_ids(), vec![kept.blob_id()]);
    assert_eq!(read_all(&copy, &kept).unwrap(), data_of(3000, 1));
    assert!(copy.manifest(later.blob_id()).unwrap().is_none());
    assert!(copy.manifest(staged.blob_id()).unwrap().is_none());
    assert_eq!(copy.superblock().unwrap().log_end, image.len());
    // A read-write open finds nothing after the commit to roll forward or truncate.
    drop(copy);
    let reopened = open(&fs, COPY, PackMode::ReadWrite);
    assert_eq!(reopened.recovery().truncated_from, None);
    assert_eq!(reopened.recovery().rolled_forward, 0);
}

#[test]
fn a_copy_taken_before_a_rewrite_keeps_what_the_rewrite_dropped() {
    let fs = sim();
    let store = open(&fs, PACK, PackMode::ReadWrite);
    let dropped = put(&store, &data_of(4000, 4), 1024, 4);
    let kept = put(&store, &data_of(100, 5), 1024, 5);
    let image = store.committed_image().unwrap().unwrap();
    // A GC rewrite renames a new pack without `dropped` over the path: the image reads the old file.
    let mark = Mark::new([kept.blob_id()], []);
    let options = SweepOptions {
        grace: std::time::Duration::from_hours(24),
        now: SystemTime::now().max(UNIX_EPOCH),
        rewrite_ratio: 0.0,
    };
    let report = store.sweep(&mark, &Pins::new([]), &options).unwrap();
    assert_eq!(report.blobs_deleted, 1);
    assert!(store.manifest(dropped.blob_id()).unwrap().is_none());
    let target = fs.create_new(Path::new(COPY)).unwrap();
    image.write_to(&target).unwrap();
    drop(target);
    let copy = open(&fs, COPY, PackMode::ReadOnly);
    assert_eq!(read_all(&copy, &dropped).unwrap(), data_of(4000, 4));
    assert_eq!(read_all(&copy, &kept).unwrap(), data_of(100, 5));
}

#[test]
fn a_read_only_handle_copies_the_state_it_opened() {
    let fs = sim();
    let writer = open(&fs, PACK, PackMode::ReadWrite);
    let first = put(&writer, &data_of(900, 6), 1024, 6);
    let reader = open(&fs, PACK, PackMode::ReadOnly);
    let second = put(&writer, &data_of(900, 7), 1024, 7);
    write_copy(&fs, &reader);
    let copy = open(&fs, COPY, PackMode::ReadOnly);
    assert_eq!(copy.blob_ids(), vec![first.blob_id()]);
    assert!(copy.manifest(second.blob_id()).unwrap().is_none());
}
