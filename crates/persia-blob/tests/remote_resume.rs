//! Parallel transfers and resumable uploads of the remote layout (ROADMAP 9.3.2; SPEC §9.4, ADR-0050 items 3–4).
//!
//! Invariants:
//! 1. Parallel and sequential agree: any blob at any chunk size, staged with `stage_chunks` and read with
//!    `read_chunks` at any parallelism, leaves the same objects and reads the same bytes as one request at a time.
//! 2. Bounded: no more than `parallelism` requests are in flight at once.
//! 3. A failing chunk write or read reports the lowest failing index's error, the chunks before it are staged, and a
//!    `Transient` mid-transfer succeeds when the transfer is repeated.
//! 4. Resume after a cut at every chunk boundary (also with a short last chunk, gaps and a lying listing staged):
//!    `resume` keeps exactly the run of full chunks from 0, the resumed chunker continues it, and the bound blob is
//!    the original, with the same id.
//! 5. A conflicting staged chunk restarts under a new upload id: `restage` copies what was staged, the rest is staged
//!    under the new id, and the blob binds and reads back; a chunk changed before the restage is `Corrupt`.
//!
//! Seeds: proptest's default config; `PROPTEST_RNG_SEED` and `PROPTEST_CASES` override it.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    missing_docs
)]

mod common;

use core::num::NonZeroUsize;
use std::sync::Arc;

use bytes::Bytes;
use common::{KeyFaults, blob_ref, data_of, keys_of, manifest_of};
use persia_blob::remote::{RemoteKey, ResumeToken};
use persia_blob::{
    BindOutcome, BlobManifest, BlobStore, ChunkSize, Error, RemoteBlobStore, UploadId, read_range,
};
use persia_storage::{LocalStorage, MemoryStorage, Op, Storage};
use proptest::prelude::*;

fn par(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn chunk_key(upload: u128, index: u32) -> String {
    RemoteKey::Chunk {
        upload_id: UploadId::new(upload),
        index,
    }
    .key()
    .unwrap()
    .as_str()
    .to_owned()
}

/// Stages `data` with one `stage_chunks` call and binds it.
fn put_parallel<S: Storage>(
    store: &RemoteBlobStore<S>,
    data: &[u8],
    chunk: u32,
    upload: u128,
) -> BlobManifest {
    let m = manifest_of(data, chunk, upload);
    let chunks: Vec<&[u8]> = data.chunks(chunk as usize).collect();
    store.stage_chunks(m.upload_id(), 0, &chunks).unwrap();
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    m
}

fn objects<S: Storage>(storage: &S) -> Vec<(String, Bytes)> {
    keys_of(storage)
        .into_iter()
        .map(|k| {
            let v = storage
                .get(&persia_storage::Key::new(k.as_str()).unwrap())
                .unwrap();
            (k, v)
        })
        .collect()
}

proptest! {
    #[test]
    fn parallel_and_sequential_agree(
        len in 0_usize..800,
        chunk in 1_u32..=50,
        parallelism in 2_usize..=9,
        seed in any::<u8>(),
        window in any::<prop::sample::Index>(),
    ) {
        let data = data_of(len, seed);
        let seq = RemoteBlobStore::new(MemoryStorage::new()).with_parallelism(par(1));
        let parl = RemoteBlobStore::new(MemoryStorage::new()).with_parallelism(par(parallelism));
        let m = put_parallel(&seq, &data, chunk, 9);
        put_parallel(&parl, &data, chunk, 9);
        prop_assert_eq!(objects(seq.storage()), objects(parl.storage()));
        let n = m.chunk_count();
        prop_assert_eq!(parl.read_chunks(&m, 0..n).unwrap(), seq.read_chunks(&m, 0..n).unwrap());
        // Any sub-range of chunks, through `read_range`.
        let r = blob_ref(&m);
        let end = window.index(len + 1) as u64;
        let start = end / 2;
        prop_assert_eq!(read_range(&parl, &r, start..end).unwrap(), &data[start as usize..end as usize]);
    }
}

#[test]
fn the_default_parallelism_is_eight_and_configurable() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    assert_eq!(store.parallelism().get(), 8);
    assert_eq!(store.with_parallelism(par(3)).parallelism().get(), 3);
}

#[test]
fn no_more_than_parallelism_requests_are_in_flight() {
    for p in [1, 2, 4] {
        let store =
            RemoteBlobStore::new(KeyFaults::new(MemoryStorage::new())).with_parallelism(par(p));
        let data = data_of(400, 1);
        let m = put_parallel(&store, &data, 4, 1);
        store.read_chunks(&m, 0..m.chunk_count()).unwrap();
        assert!(
            store.storage().max_in_flight() <= p,
            "{p}: {}",
            store.storage().max_in_flight()
        );
    }
}

#[test]
fn a_failing_chunk_write_reports_the_lowest_and_keeps_those_before() {
    for p in [1, 3, 8] {
        let store =
            RemoteBlobStore::new(KeyFaults::new(MemoryStorage::new())).with_parallelism(par(p));
        let data = data_of(40, 2);
        let chunks: Vec<&[u8]> = data.chunks(4).collect();
        store
            .storage()
            .fail(Op::PutIfAbsent, &chunk_key(5, 3), 1, false);
        store
            .storage()
            .fail(Op::PutIfAbsent, &chunk_key(5, 7), 1, true);
        match store.stage_chunks(UploadId::new(5), 0, &chunks) {
            Err(Error::Storage(e)) => assert!(!e.is_retryable(), "{p}: chunk 3's error, not 7's"),
            other => panic!("{p}: {other:?}"),
        }
        let staged: Vec<u32> = store
            .staged_chunks(UploadId::new(5))
            .unwrap()
            .iter()
            .map(|c| c.index)
            .collect();
        assert_eq!(&staged[..3], [0, 1, 2], "{p}");
        assert!(!staged.contains(&3), "{p}");
        store.storage().faults.lock().unwrap().clear();
        // Repeating the transfer (same bytes) completes it.
        store.stage_chunks(UploadId::new(5), 0, &chunks).unwrap();
        let m = manifest_of(&data, 4, 5);
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        assert_eq!(read_range(&store, &blob_ref(&m), 0..40).unwrap(), data);
    }
}

#[test]
fn a_transient_read_mid_transfer_fails_it_and_a_retry_reads() {
    let store = RemoteBlobStore::new(KeyFaults::new(MemoryStorage::new())).with_parallelism(par(4));
    let data = data_of(50, 3);
    let m = put_parallel(&store, &data, 5, 6);
    store.storage().fail(Op::Get, &chunk_key(6, 6), 1, true);
    match read_range(&store, &blob_ref(&m), 0..50) {
        Err(Error::Storage(e)) => assert!(e.is_retryable()),
        other => panic!("{other:?}"),
    }
    assert_eq!(read_range(&store, &blob_ref(&m), 0..50).unwrap(), data);
}

/// The reference driver of a resumable upload (ADR-0050 item 4): resume, continue the cut, and on a conflict restage
/// under `fresh` and go on there. Returns the unbound manifest.
fn put_resumable<S: Storage>(
    store: &RemoteBlobStore<S>,
    data: &[u8],
    token: ResumeToken,
    fresh: UploadId,
) -> BlobManifest {
    let resumed = store.resume(token).unwrap();
    let offset = resumed.offset() as usize;
    let (mut chunker, mut crcs) = resumed.into_chunker(&data[offset..]).unwrap();
    let mut upload = token.upload_id;
    while let Some(chunk) = chunker.next_chunk().unwrap() {
        match store.stage_chunk(upload, chunk.index(), chunk.bytes()) {
            Ok(()) => {}
            Err(Error::StagedChunkConflict { .. }) if upload == token.upload_id => {
                store.restage(token, &crcs, fresh).unwrap();
                upload = fresh;
                store
                    .stage_chunk(upload, chunk.index(), chunk.bytes())
                    .unwrap();
            }
            Err(e) => panic!("{e:?}"),
        }
        crcs.push(chunk.crc32c());
    }
    let blob = chunker.summary().unwrap();
    BlobManifest::new(blob.blob_id, blob.size, blob.chunk_size, upload, crcs).unwrap()
}

fn token(upload: u128, chunk: u32) -> ResumeToken {
    ResumeToken {
        upload_id: UploadId::new(upload),
        chunk_size: ChunkSize::from_encoding(chunk).unwrap(),
    }
}

/// Stages chunks `which` of `data` under `upload`.
fn stage_some<S: Storage>(
    store: &RemoteBlobStore<S>,
    data: &[u8],
    chunk: u32,
    upload: u128,
    which: &[usize],
) {
    let chunks: Vec<&[u8]> = data.chunks(chunk as usize).collect();
    for &i in which {
        store
            .stage_chunk(UploadId::new(upload), i as u32, chunks[i])
            .unwrap();
    }
}

fn check_resume<S: Storage>(
    store: &RemoteBlobStore<S>,
    data: &[u8],
    chunk: u32,
    staged: &[usize],
    run: u32,
) {
    let upload = 77;
    stage_some(store, data, chunk, upload, staged);
    let resumed = store.resume(token(upload, chunk)).unwrap();
    assert_eq!(resumed.next_index(), run, "staged {staged:?}");
    assert_eq!(resumed.offset(), u64::from(run) * u64::from(chunk));
    let want = manifest_of(data, chunk, upload);
    assert_eq!(resumed.chunk_crcs(), &want.chunk_crcs()[..run as usize]);
    let m = put_resumable(store, data, token(upload, chunk), UploadId::new(78));
    assert_eq!(m, want, "staged {staged:?}");
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    assert_eq!(read_range(store, &blob_ref(&m), 0..m.size()).unwrap(), data);
    store.discard_upload(UploadId::new(upload)).unwrap();
    store
        .storage()
        .delete(&RemoteKey::Manifest(m.blob_id()).key().unwrap())
        .unwrap();
}

#[test]
fn resume_after_a_cut_at_every_chunk_boundary() {
    for (len, chunk) in [(0, 4), (3, 4), (12, 4), (14, 4), (41, 5)] {
        let data = data_of(len, 4);
        let n = len.div_ceil(chunk as usize);
        let full = len / chunk as usize;
        for p in [1, 3] {
            let store = RemoteBlobStore::new(MemoryStorage::new()).with_parallelism(par(p));
            for cut in 0..=n {
                let staged: Vec<usize> = (0..cut).collect();
                // A staged short last chunk is not part of the run, but the same bytes again succeed.
                check_resume(&store, &data, chunk, &staged, cut.min(full) as u32);
            }
        }
    }
}

#[test]
fn resume_keeps_only_the_contiguous_run_from_zero() {
    let data = data_of(40, 5);
    let store = RemoteBlobStore::new(MemoryStorage::new()).with_parallelism(par(2));
    check_resume(&store, &data, 4, &[0, 1, 3, 4], 2);
    check_resume(&store, &data, 4, &[1, 2], 0);
    check_resume(&store, &data, 4, &[0, 2, 9], 1);
    // A foreign key under the upload's prefix is not a chunk.
    let key = format!("blobs/uploads/{}/junk", UploadId::new(77));
    store
        .storage()
        .put(
            &persia_storage::Key::new(key).unwrap(),
            Bytes::from_static(b"zzzz"),
        )
        .unwrap();
    check_resume(&store, &data, 4, &[0, 1], 2);
}

#[test]
fn a_listing_that_claims_full_chunks_does_not_fool_the_resume() {
    // Every listed object claims the full length; the short last chunk read back ends the run.
    let mut storage = KeyFaults::new(MemoryStorage::new());
    storage.list_size = Some(4);
    let store = RemoteBlobStore::new(storage);
    let data = data_of(10, 6);
    stage_some(&store, &data, 4, 1, &[0, 1, 2]);
    assert_eq!(store.resume(token(1, 4)).unwrap().next_index(), 2);
}

#[test]
fn a_resume_list_error_is_reported() {
    let store = RemoteBlobStore::new(KeyFaults::new(MemoryStorage::new()));
    let prefix = format!("blobs/uploads/{}/", UploadId::new(1));
    store.storage().fail(Op::List, &prefix, 1, true);
    assert!(matches!(store.resume(token(1, 4)), Err(Error::Storage(e)) if e.is_retryable()));
    assert_eq!(store.resume(token(1, 4)).unwrap().next_index(), 0);
}

#[test]
fn a_conflicting_staged_chunk_restarts_under_a_new_upload_id() {
    // The upload staged chunks 0..2 and a short last chunk of other bytes (another attempt's tail).
    for p in [1, 4] {
        let store = RemoteBlobStore::new(MemoryStorage::new()).with_parallelism(par(p));
        let data = data_of(14, 7);
        stage_some(&store, &data, 4, 1, &[0, 1]);
        store.stage_chunk(UploadId::new(1), 2, b"zz").unwrap();
        let m = put_resumable(&store, &data, token(1, 4), UploadId::new(2));
        assert_eq!(m.upload_id(), UploadId::new(2));
        assert_eq!(m, manifest_of(&data, 4, 2));
        assert_eq!(store.staged_chunks(UploadId::new(2)).unwrap().len(), 4);
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        assert_eq!(read_range(&store, &blob_ref(&m), 0..14).unwrap(), data);
        // The old upload is untouched (left to the caller or GC).
        let key = persia_storage::Key::new(chunk_key(1, 2)).unwrap();
        assert_eq!(&store.storage().get(&key).unwrap()[..], b"zz");
    }
}

#[test]
fn a_conflict_after_new_chunks_restages_them_too() {
    // Chunks 0 and 1 stored, 2 and 3 staged in this session, then chunk 4 conflicts.
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(18, 8);
    stage_some(&store, &data, 4, 1, &[0, 1]);
    store.stage_chunk(UploadId::new(1), 4, b"x").unwrap();
    let m = put_resumable(&store, &data, token(1, 4), UploadId::new(2));
    assert_eq!(m, manifest_of(&data, 4, 2));
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    assert_eq!(read_range(&store, &blob_ref(&m), 0..18).unwrap(), data);
}

#[test]
fn restage_rejects_a_chunk_that_changed_or_vanished() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(12, 9);
    stage_some(&store, &data, 4, 1, &[0, 1, 2]);
    let resumed = store.resume(token(1, 4)).unwrap();
    let key = persia_storage::Key::new(chunk_key(1, 1)).unwrap();
    for change in [Some(&b"XXXX"[..]), Some(b"abc"), None] {
        match change {
            Some(b) => store
                .storage()
                .put(&key, Bytes::copy_from_slice(b))
                .unwrap(),
            None => store.storage().delete(&key).unwrap(),
        }
        assert!(
            store
                .restage(resumed.token(), resumed.chunk_crcs(), UploadId::new(2))
                .unwrap_err()
                .is_corruption(),
            "{change:?}"
        );
    }
}

#[test]
fn restage_onto_an_upload_with_other_bytes_conflicts() {
    let store = RemoteBlobStore::new(MemoryStorage::new());
    let data = data_of(8, 1);
    stage_some(&store, &data, 4, 1, &[0, 1]);
    store.stage_chunk(UploadId::new(2), 1, b"nope").unwrap();
    let resumed = store.resume(token(1, 4)).unwrap();
    assert!(matches!(
        store.restage(resumed.token(), resumed.chunk_crcs(), UploadId::new(2)),
        Err(Error::StagedChunkConflict { index: 1, .. })
    ));
}

#[test]
fn resume_on_the_local_backend() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        RemoteBlobStore::new(LocalStorage::open(dir.path()).unwrap()).with_parallelism(par(3));
    let data = data_of(30, 2);
    for cut in [0, 2, 4, 6, 8] {
        check_resume(
            &store,
            &data,
            4,
            &(0..cut).collect::<Vec<_>>(),
            cut.min(7) as u32,
        );
    }
}

#[test]
fn resume_with_arc_dyn_storage() {
    // The default type parameter: the engine holds backends as `Arc<dyn Storage>`.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let store: RemoteBlobStore = RemoteBlobStore::new(storage);
    let data = data_of(9, 3);
    check_resume(&store, &data, 4, &[0], 1);
}

#[test]
fn stage_chunks_stages_each_from_its_first_index() {
    for p in [1, 3] {
        let store = RemoteBlobStore::new(MemoryStorage::new()).with_parallelism(par(p));
        let data = data_of(18, 4);
        let chunks: Vec<&[u8]> = data.chunks(4).collect();
        store
            .stage_chunks(UploadId::new(3), 2, &chunks[2..])
            .unwrap();
        store
            .stage_chunks(UploadId::new(3), 0, &chunks[..2])
            .unwrap();
        let m = manifest_of(&data, 4, 3);
        assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
        assert_eq!(read_range(&store, &blob_ref(&m), 0..18).unwrap(), data);
        assert!(matches!(
            store.stage_chunks(UploadId::new(3), u32::MAX, &chunks[..2]),
            Err(Error::InvalidManifest { .. })
        ));
    }
}
