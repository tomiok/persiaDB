//! The remote layout (ROADMAP 9.3.1; SPEC §9.4, ADR-0050) over `persia-storage`'s memory and local backends.
//!
//! Invariants:
//! 1. Keys: a staged chunk is at `blobs/uploads/<upload_id:032x>/<chunk_idx:010>`, a blob manifest at
//!    `blobs/<xx>/<id>`; nothing else is written.
//! 2. Round trip: any blob at any chunk size, staged and bound, reads back whole and by every range, on the memory
//!    backend and on the local backend in a tempdir.
//! 3. Never overwritten: staging the same chunk again with the same bytes succeeds, with other bytes is
//!    `StagedChunkConflict` and leaves the first bytes.
//! 4. Binding: `Created` is `Bound`; `AlreadyExists` is `AlreadyBound` with the bound manifest (its chunk size wins);
//!    an error of `put_if_absent` is `BindOutcomeUnknown`, and binding again with the same manifest answers `Bound`
//!    (the backend resolves its own bytes, SPEC §10.1); a backend without `conditional_put` is
//!    `ConditionalPutUnsupported` for writes and still serves reads.
//! 5. Hostile objects are `Corrupt`, never served: a manifest under another blob's key, an undecodable manifest, a
//!    missing chunk, a chunk of the wrong length or CRC, a backend's own corruption report. An unknown flag stays
//!    `UnsupportedFlags`.
//! 6. `discard_upload` deletes its upload's chunks and nothing else (another upload, foreign keys under its prefix).
//!
//! Seeds: proptest's default config; `PROPTEST_RNG_SEED` and `PROPTEST_CASES` override it.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::cast_possible_truncation,
    missing_docs
)]

mod common;

use bytes::Bytes;
use common::{NoConditionalPut, blob_ref, data_of, keys_of, stage_and_bind, staged_manifest};
use persia_blob::remote::RemoteKey;
use persia_blob::{
    BindOutcome, BlobManifest, BlobStore, ChunkSize, Error, RemoteBlobStore, UploadId, blob_id_of,
    read_range,
};
use persia_storage::faulty::{Fault, FaultyStorage, Trigger};
use persia_storage::{Key, LocalStorage, MemoryStorage, Op, Storage};
use proptest::prelude::*;
use std::sync::Arc;

#[test]
fn keys_are_the_spec_layout() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(10, 1);
    let m = stage_and_bind(&store, &data, 4, 0xab);
    let id = m.blob_id().to_string();
    let up = "000000000000000000000000000000ab";
    assert_eq!(
        keys_of(store.storage()),
        [
            format!("blobs/{}/{id}", &id[..2]),
            format!("blobs/uploads/{up}/0000000000"),
            format!("blobs/uploads/{up}/0000000001"),
            format!("blobs/uploads/{up}/0000000002"),
        ]
    );
    let chunk = store
        .storage()
        .get(&Key::new(format!("blobs/uploads/{up}/0000000002")).unwrap())
        .unwrap();
    assert_eq!(&chunk[..], &data[8..], "a chunk object is the raw bytes");
    let manifest = store
        .storage()
        .get(&RemoteKey::Manifest(m.blob_id()).key().unwrap())
        .unwrap();
    assert_eq!(&manifest[..], &m.encode()[..]);
}

fn check_round_trip<S: Storage>(store: &RemoteBlobStore<S>, data: &[u8], chunk: u32, upload: u128) {
    let m = stage_and_bind(store, data, chunk, upload);
    let r = blob_ref(&m);
    assert_eq!(store.manifest(m.blob_id()).unwrap(), Some(m.clone()));
    assert!(store.contains(&r).unwrap());
    assert_eq!(read_range(store, &r, 0..m.size()).unwrap(), data);
    let n = data.len() as u64;
    for (start, end) in [
        (0, 0),
        (0, 1.min(n)),
        (n / 3, n / 2),
        (n.saturating_sub(1), n),
    ] {
        assert_eq!(
            read_range(store, &r, start..end).unwrap(),
            &data[start as usize..end as usize]
        );
    }
}

proptest! {
    #[test]
    fn any_blob_round_trips_in_memory(len in 0_usize..600, chunk in 1_u32..=70, seed in any::<u8>(), upload in any::<u128>()) {
        let store = RemoteBlobStore::new(MemoryStorage::new());
        check_round_trip(&store, &data_of(len, seed), chunk, upload);
    }
}

proptest! {
    // Every object is fsynced with its directory: few cases and at most 25 chunks, each case a fresh tempdir.
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn any_blob_round_trips_on_the_local_backend(len in 0_usize..200, chunk in 8_u32..=70, seed in any::<u8>()) {
        let dir = tempfile::tempdir().unwrap();
        let store = RemoteBlobStore::new(LocalStorage::open(dir.path()).unwrap());
        check_round_trip(&store, &data_of(len, seed), chunk, 7);
    }
}

#[test]
fn default_chunk_sizes_round_trip() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    for (len, chunk) in [
        (0, ChunkSize::MIN),
        (1, ChunkSize::MIN),
        ((64 << 10) * 3 + 5, ChunkSize::MIN),
        ((4 << 20) + 17, ChunkSize::DEFAULT.get()),
    ] {
        check_round_trip(&store, &data_of(len, 3), chunk, len as u128);
    }
}

#[test]
fn a_staged_chunk_is_never_overwritten() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let up = UploadId::new(5);
    store.stage_chunk(up, 0, b"abcd").unwrap();
    store.stage_chunk(up, 0, b"abcd").unwrap();
    assert!(matches!(
        store.stage_chunk(up, 0, b"abce"),
        Err(Error::StagedChunkConflict { upload_id, index: 0 }) if upload_id == up
    ));
    let key = RemoteKey::Chunk {
        upload_id: up,
        index: 0,
    }
    .key()
    .unwrap();
    assert_eq!(&store.storage().get(&key).unwrap()[..], b"abcd");
}

#[test]
fn a_second_upload_of_the_same_bytes_is_already_bound_with_the_first_chunk_size() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(23, 9);
    let first = stage_and_bind(&store, &data, 5, 1);
    let second = staged_manifest(&store, &data, 7, 2);
    match store.bind(&second).unwrap() {
        BindOutcome::AlreadyBound(bound) => {
            assert_eq!(bound, first);
            assert_eq!(bound.chunk_size().get(), 5);
        }
        BindOutcome::Bound => panic!("a duplicate bound"),
    }
    store.discard_upload(second.upload_id()).unwrap();
    assert_eq!(store.staged_chunks(second.upload_id()).unwrap(), []);
    assert_eq!(store.staged_chunks(first.upload_id()).unwrap().len(), 5);
    assert_eq!(read_range(&store, &blob_ref(&first), 0..23).unwrap(), data);
    // Binding the first manifest again is its own bytes: `Bound`.
    assert_eq!(store.bind(&first).unwrap(), BindOutcome::Bound);
}

#[test]
fn a_put_if_absent_error_leaves_the_outcome_unknown_and_a_retry_answers() {
    for fault in [Fault::Transient, Fault::Io, Fault::ResponseLost] {
        let faulty = Arc::new(FaultyStorage::new(MemoryStorage::new(), 1));
        let store = RemoteBlobStore::new(Arc::clone(&faulty));
        let data = data_of(12, 4);
        let m = staged_manifest(&store, &data, 5, 3);
        faulty.inject(Trigger::Nth(Op::PutIfAbsent, 4), fault);
        let err = store.bind(&m).unwrap_err();
        assert!(err.is_bind_outcome_unknown(), "{fault:?}: {err:?}");
        assert_eq!(faulty.pending(), []);
        // ResponseLost applied the write; the others did not. Either way the same manifest binds.
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound, "{fault:?}");
        assert_eq!(read_range(&store, &blob_ref(&m), 0..12).unwrap(), data);
    }
}

#[test]
fn a_staging_error_is_a_storage_error() {
    let faulty = Arc::new(FaultyStorage::new(MemoryStorage::new(), 1));
    let store = RemoteBlobStore::new(Arc::clone(&faulty));
    faulty.inject(Trigger::Nth(Op::PutIfAbsent, 1), Fault::Transient);
    match store.stage_chunk(UploadId::new(1), 0, b"x") {
        Err(Error::Storage(e)) => assert!(e.is_retryable()),
        other => panic!("{other:?}"),
    }
    store.stage_chunk(UploadId::new(1), 0, b"x").unwrap();
}

#[test]
fn a_backend_without_conditional_put_cannot_store_blobs_but_reads() {
    let inner = Arc::new(MemoryStorage::new());
    let writer = RemoteBlobStore::new(Arc::clone(&inner));
    let data = data_of(9, 2);
    let m = stage_and_bind(&writer, &data, 4, 8);
    let store = RemoteBlobStore::new(NoConditionalPut(inner));
    assert!(matches!(
        store.stage_chunk(UploadId::new(9), 0, b"x"),
        Err(Error::ConditionalPutUnsupported)
    ));
    assert!(matches!(
        store.bind(&m),
        Err(Error::ConditionalPutUnsupported)
    ));
    let small =
        BlobManifest::new(blob_id_of(b""), 0, m.chunk_size(), UploadId::new(1), vec![]).unwrap();
    assert!(matches!(
        store.bind_small(&small, b""),
        Err(Error::ConditionalPutUnsupported)
    ));
    assert_eq!(read_range(&store, &blob_ref(&m), 0..9).unwrap(), data);
}

#[test]
fn a_manifest_under_another_blobs_key_is_corrupt() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let real = stage_and_bind(&store, b"real bytes", 4, 1);
    let other_id = blob_id_of(b"other bytes");
    let key = RemoteKey::Manifest(other_id).key().unwrap();
    store
        .storage()
        .put(&key, Bytes::from(real.encode()))
        .unwrap();
    assert!(store.manifest(other_id).unwrap_err().is_corruption());
    let other = staged_manifest(&store, b"other bytes", 4, 2);
    assert!(store.bind(&other).unwrap_err().is_corruption());
}

#[test]
fn an_undecodable_or_newer_manifest_is_reported() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let m = stage_and_bind(&store, b"0123456789", 4, 1);
    let key = RemoteKey::Manifest(m.blob_id()).key().unwrap();
    for (bytes, corrupt) in [
        (b"junk".to_vec(), true),
        (
            {
                let mut b = m.encode();
                b[20] ^= 1;
                b
            },
            true,
        ),
        (
            {
                let mut b = m.encode();
                b[8..12].copy_from_slice(&(1_u32 << 31).to_le_bytes());
                let end = b.len() - 4;
                let crc = persia_format::checksum::crc32c(&b[..end]);
                b[end..].copy_from_slice(&crc.to_le_bytes());
                b
            },
            false,
        ),
    ] {
        store.storage().put(&key, Bytes::from(bytes)).unwrap();
        let err = store.manifest(m.blob_id()).unwrap_err();
        assert_eq!(err.is_corruption(), corrupt, "{err:?}");
        if !corrupt {
            assert!(matches!(err, Error::UnsupportedFlags { .. }), "{err:?}");
        }
        let dup = staged_manifest(&store, b"0123456789", 4, 2);
        let err = store.bind(&dup).unwrap_err();
        assert_eq!(err.is_corruption(), corrupt, "{err:?}");
    }
}

#[test]
fn missing_or_damaged_chunks_are_corrupt() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(10, 5);
    let m = stage_and_bind(&store, &data, 4, 1);
    let r = blob_ref(&m);
    let chunk_key = |index| {
        RemoteKey::Chunk {
            upload_id: m.upload_id(),
            index,
        }
        .key()
        .unwrap()
    };
    // Longer, shorter, other bytes of the right length, missing.
    for bytes in [Some(&b"89x"[..]), Some(b"8"), Some(b"8X"), None] {
        match bytes {
            Some(b) => store
                .storage()
                .put(&chunk_key(2), Bytes::copy_from_slice(b))
                .unwrap(),
            None => store.storage().delete(&chunk_key(2)).unwrap(),
        }
        assert!(
            read_range(&store, &r, 9..10).unwrap_err().is_corruption(),
            "{bytes:?}"
        );
        assert_eq!(read_range(&store, &r, 0..8).unwrap(), &data[..8]);
    }
    assert!(
        persia_blob::read_chunk_checked(&store, &m, 3)
            .unwrap_err()
            .is_corruption()
    );
    assert!(store.read_chunk(&m, 3).unwrap_err().is_corruption());
}

#[test]
fn an_unbound_id_is_none() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    assert_eq!(store.manifest(blob_id_of(b"never")).unwrap(), None);
    let m = staged_manifest(&store, b"never", 4, 1);
    assert!(!store.contains(&blob_ref(&m)).unwrap());
    assert!(matches!(
        read_range(&store, &blob_ref(&m), 0..1),
        Err(Error::BlobNotFound { .. })
    ));
}

#[test]
fn discard_upload_deletes_its_chunks_only() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let keep = staged_manifest(&store, &data_of(9, 1), 4, 1);
    let drop = staged_manifest(&store, &data_of(9, 2), 4, 2);
    let up = drop.upload_id();
    let foreign = [
        format!("blobs/uploads/{up}/notachunk"),
        format!("blobs/uploads/{up}/0000000000/deeper"),
        format!("blobs/uploads/{up}x/0000000000"),
    ];
    for k in &foreign {
        store
            .storage()
            .put(&Key::new(k.as_str()).unwrap(), Bytes::from_static(b"f"))
            .unwrap();
    }
    store.discard_upload(up).unwrap();
    assert_eq!(store.staged_chunks(up).unwrap(), []);
    assert_eq!(store.staged_chunks(keep.upload_id()).unwrap().len(), 3);
    let left = keys_of(store.storage());
    for k in &foreign {
        assert!(left.contains(k), "{k} was deleted");
    }
    // Idempotent.
    store.discard_upload(up).unwrap();
}

#[test]
fn staged_chunks_lists_by_index_with_lengths() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let m = staged_manifest(&store, &data_of(10, 1), 4, 3);
    let listed: Vec<_> = store
        .staged_chunks(m.upload_id())
        .unwrap()
        .into_iter()
        .map(|c| (c.index, c.size))
        .collect();
    assert_eq!(listed, [(0, 4), (1, 4), (2, 2)]);
}

#[test]
fn backend_corruption_stays_corruption() {
    // A backend that reports a short read or a corrupt object on every `get`.
    let store = RemoteBlobStore::new(common::CorruptGets(MemoryStorage::new()));
    assert!(
        store
            .manifest(blob_id_of(b"x"))
            .unwrap_err()
            .is_corruption()
    );
}

#[test]
fn a_listing_outside_the_prefix_never_reaches_another_upload() {
    let store = RemoteBlobStore::new(common::ListsEverything(MemoryStorage::new()));
    let keep = staged_manifest(&store, &data_of(9, 1), 4, 1);
    let drop = staged_manifest(&store, &data_of(5, 2), 4, 2);
    assert_eq!(store.staged_chunks(drop.upload_id()).unwrap().len(), 2);
    store.discard_upload(drop.upload_id()).unwrap();
    assert_eq!(store.staged_chunks(keep.upload_id()).unwrap().len(), 3);
}

#[test]
fn staged_chunks_report_when_they_were_written() {
    use persia_storage::retry::ManualClock;
    use std::time::{Duration, UNIX_EPOCH};

    let clock = Arc::new(ManualClock::new(UNIX_EPOCH));
    let store = RemoteBlobStore::new(MemoryStorage::with_clock(Arc::clone(&clock) as _));
    let up = UploadId::new(4);
    store.stage_chunk(up, 0, b"abcd").unwrap();
    clock.advance(Duration::from_secs(5));
    store.stage_chunk(up, 1, b"ef").unwrap();
    let times: Vec<_> = store
        .staged_chunks(up)
        .unwrap()
        .into_iter()
        .map(|c| c.last_modified)
        .collect();
    assert_eq!(
        times,
        [Some(UNIX_EPOCH), Some(UNIX_EPOCH + Duration::from_secs(5))]
    );
}

#[test]
fn a_bound_manifest_gone_before_its_get_is_written_again() {
    use common::Scripted;
    use persia_storage::PutOutcome::{AlreadyExists, Created};

    // AlreadyExists, the get finds nothing (a sweep deleted it), the next attempt is Created: Bound.
    let store = RemoteBlobStore::new(Scripted::<MemoryStorage>::default());
    let data = data_of(10, 3);
    let m = staged_manifest(&store, &data, 4, 1);
    store
        .storage()
        .puts
        .lock()
        .unwrap()
        .extend([AlreadyExists, Created]);
    store
        .storage()
        .gets_not_found
        .store(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    assert_eq!(store.storage().puts.lock().unwrap().len(), 0);

    // Every attempt finds it gone: the last NotFound is reported, a known outcome (not unknown).
    store
        .storage()
        .puts
        .lock()
        .unwrap()
        .extend([AlreadyExists; 3]);
    store
        .storage()
        .gets_not_found
        .store(3, std::sync::atomic::Ordering::SeqCst);
    match store.bind(&m) {
        Err(Error::Storage(e)) => assert_eq!(e.kind(), persia_storage::ErrorKind::NotFound),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        store.storage().puts.lock().unwrap().len(),
        0,
        "3 attempts, no more"
    );
}

#[test]
fn a_backend_that_claims_conditional_put_but_refuses_it_cannot_store_blobs() {
    let store = RemoteBlobStore::new(common::Scripted {
        unsupported: true,
        ..common::Scripted::<MemoryStorage>::default()
    });
    assert!(matches!(
        store.stage_chunk(UploadId::new(1), 0, b"x"),
        Err(Error::ConditionalPutUnsupported)
    ));
    let m = common::manifest_of(b"", 4, 1);
    assert!(matches!(
        store.bind(&m),
        Err(Error::ConditionalPutUnsupported)
    ));
    assert!(matches!(
        store.bind_small(&m, b""),
        Err(Error::ConditionalPutUnsupported)
    ));
}
