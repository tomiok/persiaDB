//! The facade's blob options set every option with names from `persia` alone (ROADMAP 10.1.15): this file imports
//! nothing from `persia-blob`, `persia-engine` or `persia-format`, as a program that depends only on `persia` would.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::time::Duration;

use persia::{Blob, Db, Document, ErrorCode, PutBlobOptions, ResumeToken};

fn db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    (dir, db)
}

#[test]
fn a_chunk_size_set_through_the_facade_is_the_blobs() {
    let (_dir, db) = db();
    let bytes: Vec<u8> = (0..300_000_u32).map(|i| (i % 251) as u8).collect();
    for chunk in [64 << 10, 128 << 10, 1 << 20] {
        let options = PutBlobOptions::new()
            .chunk_size(chunk)
            .mime("application/x-test")
            .user_meta("k", "v")
            .expected_size(300_000)
            .bind_budget(Duration::from_secs(1));
        let blob = db.put_blob(&bytes[..], &options).unwrap();
        // The same bytes stored again keep the chunk size they were first stored with.
        assert_eq!(blob.chunk_size(), 64 << 10, "{chunk}");
        assert_eq!(blob.mime(), "application/x-test");
        assert_eq!(db.get_blob_range(&blob, 0..300_000).unwrap(), bytes);
    }
    let other: Vec<u8> = bytes.iter().map(|b| b ^ 1).collect();
    let blob = db
        .put_blob(&other[..], &PutBlobOptions::new().chunk_size(128 << 10))
        .unwrap();
    assert_eq!(blob.chunk_size(), 128 << 10);
    let blob = db
        .put_blob(
            &bytes[..10],
            &PutBlobOptions::new().expected_id(blob.blob_id()),
        )
        .unwrap_err();
    assert_eq!(blob.code(), ErrorCode::InvalidArgument);
}

#[test]
fn chunk_sizes_outside_the_writers_range_are_invalid_arguments() {
    let (_dir, db) = db();
    for chunk in [0, 1, (64 << 10) - 1, (64 << 20) + 1, u32::MAX] {
        let err = db
            .put_blob(&b"x"[..], &PutBlobOptions::new().chunk_size(chunk))
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{chunk}");
    }
}

/// A resume set through the facade reaches the store: the local pack cannot resume (SPEC §9.4), and says so before
/// it reads a byte, whatever the token. The checks that come first still apply: a token whose chunk size no encoding
/// holds is a caller's error.
#[test]
fn a_resume_set_through_the_facade_reaches_the_store() {
    let (_dir, db) = db();
    let token = ResumeToken::new(u128::MAX, 64 << 10);
    assert_eq!(
        (token.upload_id(), token.chunk_size()),
        (u128::MAX, 64 << 10)
    );
    let options = PutBlobOptions::new().resume(token).expected_size(2);
    let err = db.put_blob(&b"hi"[..], &options).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Unsupported, "{err}");
    assert_eq!(err.resume_token(), None);
    let files = db
        .create_collection_json(r#"{"name": "files", "fields": {}}"#)
        .unwrap();
    let with_blob = Blob::from_reader(&b"hi"[..]).with_options(options);
    let err = files
        .put_with_blob(&Document::new("a"), with_blob)
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Unsupported, "{err}");
    assert!(files.get("a").unwrap().is_none(), "nothing is logged");
    let unencodable = PutBlobOptions::new()
        .resume(ResumeToken::new(1, 0))
        .expected_size(2);
    assert_eq!(
        db.put_blob(&b"hi"[..], &unencodable).unwrap_err().code(),
        ErrorCode::InvalidArgument
    );
}
