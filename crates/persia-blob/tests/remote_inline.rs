//! Small blobs in one object (ROADMAP 9.3.3; SPEC §9.4, ADR-0050 item 5) in the remote layout.
//!
//! Invariants:
//! 1. `bind_small` of a blob of 1..=`SMALL_BLOB_MAX` bytes writes one object, the inline manifest at `blobs/<xx>/<id>`,
//!    with one `put_if_absent`, and stages nothing; the empty blob writes its plain manifest the same way.
//! 2. A read is one `get` (the manifest), and every range reads back, on the memory and the local backend.
//! 3. Dedup: the same small blob from two writers at the same chunk size is the same bytes, so both answer `Bound`;
//!    at another chunk size, or bound before as chunks, it is `AlreadyBound` with the bound manifest.
//! 4. A `put_if_absent` error is `BindOutcomeUnknown`; bytes that are not the caller's manifest's, or over
//!    `SMALL_BLOB_MAX`, are `InvalidManifest` and write nothing.
//! 5. Hostile: an inline manifest under another blob's key is `Corrupt`; a chunk index past the one chunk is `Corrupt`.
//!
//! Seeds: proptest's default config; `PROPTEST_RNG_SEED` and `PROPTEST_CASES` override it.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    missing_docs
)]

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::{blob_ref, data_of, keys_of, manifest_of, stage_and_bind};
use persia_blob::remote::RemoteKey;
use persia_blob::{
    BindOutcome, BlobStore, ChunkSize, Error, RemoteBlobStore, SMALL_BLOB_MAX, UploadId,
    blob_id_of, read_range,
};
use persia_storage::faulty::{Fault, FaultyStorage, Trigger};
use persia_storage::{LocalStorage, MemoryStorage, Op, Storage};
use proptest::prelude::*;

fn bind_small<S: Storage>(
    store: &RemoteBlobStore<S>,
    data: &[u8],
    chunk: u32,
    upload: u128,
) -> BindOutcome {
    store
        .bind_small(&manifest_of(data, chunk, upload), data)
        .unwrap()
}

#[test]
fn a_small_blob_is_one_object_written_and_read_in_one_request() {
    for len in [1, 2, 1000, SMALL_BLOB_MAX as usize] {
        let faulty = Arc::new(FaultyStorage::new(MemoryStorage::new(), 1));
        let store = RemoteBlobStore::new(Arc::clone(&faulty));
        let data = data_of(len, 1);
        let m = manifest_of(&data, ChunkSize::MIN, 5);
        assert_eq!(store.bind_small(&m, &data).unwrap(), BindOutcome::Bound);
        assert_eq!(faulty.calls(), 1, "{len}: one put_if_absent");
        let key = RemoteKey::Manifest(m.blob_id()).key().unwrap();
        assert_eq!(keys_of(faulty.inner()), [key.as_str().to_owned()]);
        assert_eq!(store.staged_chunks(m.upload_id()).unwrap(), []);
        let stored = store.manifest(m.blob_id()).unwrap().unwrap();
        assert!(stored.is_inline());
        assert_eq!(stored.upload_id(), UploadId::new(0));
        assert_eq!(stored.chunk_crcs(), m.chunk_crcs());
        let before = faulty.calls();
        assert_eq!(
            read_range(&store, &blob_ref(&m), 0..len as u64).unwrap(),
            data
        );
        assert_eq!(faulty.calls() - before, 1, "{len}: one get");
        let mid = len as u64 / 2;
        assert_eq!(
            read_range(&store, &blob_ref(&m), mid..len as u64).unwrap(),
            &data[mid as usize..]
        );
    }
}

#[test]
fn the_empty_blob_is_its_plain_manifest() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let m = manifest_of(b"", ChunkSize::MIN, 3);
    assert_eq!(store.bind_small(&m, b"").unwrap(), BindOutcome::Bound);
    let stored = store.manifest(blob_id_of(b"")).unwrap().unwrap();
    assert_eq!(stored, m);
    assert!(!stored.is_inline());
    assert_eq!(keys_of(store.storage()).len(), 1);
    assert_eq!(read_range(&store, &blob_ref(&m), 0..0).unwrap(), b"");
}

proptest! {
    #[test]
    fn any_small_blob_round_trips(len in 1_usize..2000, chunk in prop_oneof![Just(ChunkSize::MIN), Just(ChunkSize::DEFAULT.get()), 2000_u32..=ChunkSize::MAX], seed in any::<u8>(), start in any::<prop::sample::Index>()) {
        let store = RemoteBlobStore::new(MemoryStorage::new());
        let data = data_of(len, seed);
        let m = manifest_of(&data, chunk, 1);
        prop_assert_eq!(store.bind_small(&m, &data).unwrap(), BindOutcome::Bound);
        let s = start.index(len) as u64;
        prop_assert_eq!(read_range(&store, &blob_ref(&m), s..len as u64).unwrap(), &data[s as usize..]);
    }
}

#[test]
fn a_small_blob_round_trips_on_the_local_backend() {
    let dir = tempfile::tempdir().unwrap();
    let store = RemoteBlobStore::new(LocalStorage::open(dir.path()).unwrap());
    for len in [0, 1, 4097, SMALL_BLOB_MAX as usize] {
        let data = data_of(len, 2);
        let m = manifest_of(&data, ChunkSize::MIN, 1);
        assert_eq!(store.bind_small(&m, &data).unwrap(), BindOutcome::Bound);
        assert_eq!(
            read_range(&store, &blob_ref(&m), 0..len as u64).unwrap(),
            data
        );
    }
}

#[test]
fn two_writers_of_the_same_small_blob_both_bind() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(100, 3);
    assert_eq!(
        bind_small(&store, &data, ChunkSize::MIN, 1),
        BindOutcome::Bound
    );
    // Another upload id: the inline manifest has none, so the bytes are equal (SPEC §10.1 resolves them).
    assert_eq!(
        bind_small(&store, &data, ChunkSize::MIN, 2),
        BindOutcome::Bound
    );
    // Another chunk size: another manifest, so the first one wins.
    match bind_small(&store, &data, ChunkSize::DEFAULT.get(), 3) {
        BindOutcome::AlreadyBound(bound) => {
            assert!(bound.is_inline());
            assert_eq!(bound.chunk_size().get(), ChunkSize::MIN);
        }
        BindOutcome::Bound => panic!("a duplicate bound"),
    }
}

#[test]
fn a_small_blob_bound_as_chunks_before_is_already_bound() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(50, 4);
    let chunked = stage_and_bind(&store, &data, 16, 1);
    assert_eq!(
        bind_small(&store, &data, ChunkSize::MIN, 2),
        BindOutcome::AlreadyBound(chunked.clone())
    );
    // And the other way round: a chunked upload of a blob bound inline is a duplicate.
    let other = data_of(30, 5);
    assert_eq!(
        bind_small(&store, &other, ChunkSize::MIN, 3),
        BindOutcome::Bound
    );
    let dup = common::staged_manifest(&store, &other, 8, 4);
    assert!(matches!(store.bind(&dup).unwrap(), BindOutcome::AlreadyBound(b) if b.is_inline()));
}

#[test]
fn a_put_if_absent_error_leaves_a_small_bind_unknown() {
    let faulty = Arc::new(FaultyStorage::new(MemoryStorage::new(), 1));
    let store = RemoteBlobStore::new(Arc::clone(&faulty));
    let data = data_of(10, 6);
    let m = manifest_of(&data, ChunkSize::MIN, 1);
    faulty.inject(Trigger::Nth(Op::PutIfAbsent, 1), Fault::ResponseLost);
    assert!(
        store
            .bind_small(&m, &data)
            .unwrap_err()
            .is_bind_outcome_unknown()
    );
    assert_eq!(store.bind_small(&m, &data).unwrap(), BindOutcome::Bound);
}

#[test]
fn bytes_that_are_not_the_small_blob_write_nothing() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let big = data_of(SMALL_BLOB_MAX as usize + 1, 1);
    let data = data_of(10, 2);
    let m = manifest_of(&data, ChunkSize::MIN, 1);
    let mut other = data.clone();
    other[3] ^= 1;
    for (manifest, bytes) in [
        (manifest_of(&big, ChunkSize::MAX, 1), &big[..]),
        (m.clone(), &data[..9]),
        (m.clone(), &other[..]),
        (manifest_of(b"", ChunkSize::MIN, 1), &data[..]),
        // A manifest of several chunks.
        (manifest_of(&data, 4, 1), &data[..]),
    ] {
        assert!(
            matches!(
                store.bind_small(&manifest, bytes),
                Err(Error::InvalidManifest { .. })
            ),
            "{} bytes",
            bytes.len()
        );
    }
    assert_eq!(keys_of(store.storage()), Vec::<String>::new());
}

#[test]
fn an_inline_manifest_under_another_key_is_corrupt() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(10, 7);
    assert_eq!(
        bind_small(&store, &data, ChunkSize::MIN, 1),
        BindOutcome::Bound
    );
    let inline = store.manifest(blob_id_of(&data)).unwrap().unwrap();
    let other = blob_id_of(b"other");
    store
        .storage()
        .put(
            &RemoteKey::Manifest(other).key().unwrap(),
            Bytes::from(inline.encode()),
        )
        .unwrap();
    assert!(store.manifest(other).unwrap_err().is_corruption());
    assert!(store.read_chunk(&inline, 1).unwrap_err().is_corruption());
    assert_eq!(store.read_chunk(&inline, 0).unwrap(), data);
    assert_eq!(store.read_chunks(&inline, 0..1).unwrap(), [data]);
}

#[test]
fn an_unknown_bind_small_retried_with_bind_never_leaves_a_chunkless_manifest() {
    // Not applied (Transient) and applied with its answer lost (ResponseLost).
    for fault in [Fault::Transient, Fault::ResponseLost] {
        let faulty = Arc::new(FaultyStorage::new(MemoryStorage::new(), 1));
        let store = RemoteBlobStore::new(Arc::clone(&faulty));
        let data = data_of(10, 8);
        let m = manifest_of(&data, ChunkSize::MIN, 1);
        faulty.inject(Trigger::Nth(Op::PutIfAbsent, 1), fault);
        assert!(
            store
                .bind_small(&m, &data)
                .unwrap_err()
                .is_bind_outcome_unknown()
        );
        // The wrong retry: its chunk was never staged, so `bind` refuses it and writes nothing.
        assert!(
            matches!(store.bind(&m), Err(Error::InvalidManifest { .. })),
            "{fault:?}"
        );
        match store.manifest(m.blob_id()).unwrap() {
            None => assert_eq!(fault, Fault::Transient),
            Some(bound) => assert!(bound.is_inline(), "{fault:?}: a plain manifest was bound"),
        }
        // The right retry.
        assert_eq!(
            store.bind_small(&m, &data).unwrap(),
            BindOutcome::Bound,
            "{fault:?}"
        );
        assert!(store.manifest(m.blob_id()).unwrap().unwrap().is_inline());
        assert_eq!(read_range(&store, &blob_ref(&m), 0..10).unwrap(), data);
    }
}

#[test]
fn a_one_chunk_small_blob_binds_through_bind_only_when_its_chunk_is_staged() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(10, 9);
    let m = manifest_of(&data, ChunkSize::MIN, 1);
    // Other bytes staged under its upload: refused.
    store
        .stage_chunk(m.upload_id(), 0, &data_of(10, 1))
        .unwrap();
    assert!(matches!(store.bind(&m), Err(Error::InvalidManifest { .. })));
    assert_eq!(store.manifest(m.blob_id()).unwrap(), None);
    // Its own chunk staged: bound as a plain manifest.
    let m = manifest_of(&data, ChunkSize::MIN, 2);
    store.stage_chunk(m.upload_id(), 0, &data).unwrap();
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    assert_eq!(read_range(&store, &blob_ref(&m), 0..10).unwrap(), data);
}
