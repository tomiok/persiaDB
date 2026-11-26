//! The internal layer under the public embedded API (ADR-0051): blobs with their pin set, and documents that
//! reference them. **Not a stable API**: M10's `Db` and `Collection` wrap it (SPEC §9.6), and it may change without
//! notice until then.
//!
//! - [`blob`]: `put_blob`, `get_blob`, `get_blob_range` over a `BlobStore` (SPEC §9.2, §9.6; ROADMAP 9.4.2);
//! - [`pins`]: the writer's pin set GC honors (SPEC §9.5);
//! - [`retry`]: the clock and backoff of `put_blob`'s bind retries (SPEC §9.4);
//! - [`documents`]: `put(doc, blob)`, the blob durable before the document is logged (SPEC §9.5; ROADMAP 9.4.1);
//! - [`gc`]: the mark and the sweep of the blob store (SPEC §9.5; ROADMAP 9.5.1, 9.5.2);
//! - [`jsonl`]: the bounded JSON Lines reader of `persia import` (ROADMAP 10.4.2), for the CLI and its fuzz target.

pub mod blob;
pub mod documents;
pub mod jsonl;
pub mod pins;
pub mod retry;

// -- gc (9.5) --
// Garbage collection (SPEC §9.5; ROADMAP 9.5.1, 9.5.2): `BlobDatabase::gc` and its options live in `gc`.
pub mod gc;

// `verify`: dangling references and the pack's checksum scan (SPEC §9.5; ROADMAP 9.5.3, 9.5.6).
pub mod verify;

/// The blob pack path of the container at `container`, `<name>.persia.blobs` (SPEC §9.3): `persia_blob`'s own rule,
/// for tools (`persia stats`) that name the pack without opening it.
///
/// ```
/// use std::path::Path;
/// assert_eq!(
///     persia::internal::pack_path(Path::new("/data/app.persia")),
///     Path::new("/data/app.persia.blobs")
/// );
/// ```
pub fn pack_path(container: &std::path::Path) -> std::path::PathBuf {
    persia_blob::pack_path(container)
}

/// Fuzzing hook (ROADMAP 10.1.1): the filesystem path `location` names, as `Db::open` reads it (SPEC §8.1, §10.2).
///
/// # Errors
/// As `Db::open` for a location: `InvalidArgument` or `Unsupported`.
pub fn location_path(location: &str) -> crate::Result<std::path::PathBuf> {
    crate::location::path_of(std::path::Path::new(location))
}

#[cfg(test)]
mod tests {
    use persia_engine::db::DatabaseOptions;
    use persia_engine::document::Document;
    use persia_engine::schema::SchemaBuilder;
    use persia_engine::wal::Durability;

    use super::documents::{Blob, BlobDatabase};
    use super::pack_path;

    /// The path names the pack file a database writes with its first blob.
    #[test]
    fn pack_path_names_the_pack_a_blob_database_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.persia");
        let mut db = BlobDatabase::create(&path, DatabaseOptions::default()).unwrap();
        db.log_mut()
            .create_collection(SchemaBuilder::new("files").build().unwrap())
            .unwrap();
        db.put(
            "files",
            &Document::new("f1"),
            Blob::from_reader(&b"bytes"[..]),
            Durability::Fsync,
        )
        .unwrap();
        db.commit().unwrap();
        let pack = pack_path(&path);
        assert_eq!(pack, dir.path().join("app.persia.blobs"));
        assert!(pack.is_file(), "{}", pack.display());
    }
}
