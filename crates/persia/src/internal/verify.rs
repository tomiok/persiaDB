//! The blob checks of `persia verify` (SPEC §9.5; ROADMAP 9.5.3, 9.5.6; ADR-0053), over the local layout (the pack;
//! the remote layout is ROADMAP 13.2.6). The CLI reaches them through this crate.
//!
//! [`ReadOnlyBlobDatabase::verify_blobs`] runs, in order, stopping at the first failure:
//! 1. **References** (always): every blob reference of a live segment document and of every WAL put from `wal_start`
//!    on ([`ReadOnlyDatabase::blob_references`](persia_engine::db::ReadOnlyDatabase::blob_references)) must name a
//!    blob the pack has bound, whose manifest matches it (`check_ref`). One that does not is a dangling reference,
//!    [`VerifyError::Dangling`], naming the collection, the document id and the `blob_id`: never a warning.
//! 2. **Checksum scan** (by default; [`Scan::Quick`] skips it): every committed frame of the pack, its checksum and
//!    each chunk's CRC32C, orphans included ([`PackStore::scan_checksums`]).
//! 3. **Re-hash** ([`Scan::Deep`] only): every referenced blob read whole, its SHA-256 against its `blob_id`.
//!
//! Every failure here is an integrity failure ([`VerifyError::is_corruption`]), except an I/O error.

use std::collections::BTreeMap;

use persia_blob::pack::{ContainerFile, PackScan};
use persia_blob::{
    BlobId, BlobManifest, BlobStore, ContentHasher, PackStore, read_manifest_range_into,
};
use persia_engine::db::{BlobRefSite, DatabaseError};

use super::blob::WINDOW_BYTES;
use super::documents::ReadOnlyBlobDatabase;

/// How much of the pack [`ReadOnlyBlobDatabase::verify_blobs`] reads beyond the references (ADR-0053).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scan {
    /// `--quick`: the references only, no frame read.
    Quick,
    /// The default: the references, then the checksum scan of every committed frame.
    #[default]
    Checksums,
    /// `--deep`: also re-hash every referenced blob with SHA-256 against its `blob_id`.
    Deep,
}

/// What [`ReadOnlyBlobDatabase::verify_blobs`] found, for a database that passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct BlobReport {
    /// Blob references checked: of live segment documents and of WAL puts.
    pub references: u64,
    /// Distinct blobs they name.
    pub blobs: usize,
    /// Whether the pack file exists.
    pub pack: bool,
    /// Blobs the pack has bound, referenced or not.
    pub bound: usize,
    /// The checksum scan, unless [`Scan::Quick`] skipped it (or there is no pack).
    pub scan: Option<PackScan>,
    /// Blobs re-hashed, with [`Scan::Deep`].
    pub rehashed: Option<usize>,
}

/// Why a reference is dangling (SPEC §9.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// The database has no pack file at all (a container copied without it, SPEC §4.8).
    NoPack,
    /// The pack has bound no blob of that id.
    NotBound,
    /// A blob of that id is bound, but its manifest does not match the reference (`check_ref`): what failed.
    Mismatch(String),
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPack => f.write_str("the database has no blob pack"),
            Self::NotBound => f.write_str("the blob pack has not bound it"),
            Self::Mismatch(what) => write!(f, "its bound manifest does not match: {what}"),
        }
    }
}

/// Where a reference was found, for people.
fn describe(site: &BlobRefSite) -> String {
    match site {
        BlobRefSite::Segment { segment } => format!("segment {segment}"),
        BlobRefSite::Wal { chunk, offset } => {
            format!("the WAL record at offset {offset} of the chunk at {chunk}")
        }
        other => format!("{other:?}"),
    }
}

/// Errors of [`ReadOnlyBlobDatabase::verify_blobs`].
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// A document references a blob the pack does not hold (SPEC §9.5, ROADMAP 9.5.6).
    #[error(
        "collection {collection:?}, document {id:?} references blob {blob_id}, which is missing: {missing} (in {})",
        describe(.site)
    )]
    Dangling {
        /// The document's collection.
        collection: String,
        /// The document's id.
        id: String,
        /// The blob it references.
        blob_id: BlobId,
        /// Where the reference was found.
        site: BlobRefSite,
        /// Why it is dangling.
        missing: Missing,
    },
    /// A referenced blob reads back whole, every chunk's CRC32C right, but its bytes hash to another id
    /// ([`Scan::Deep`]).
    #[error(
        "collection {collection:?}, document {id:?} references blob {blob_id}, whose bytes hash to {actual}"
    )]
    HashMismatch {
        /// The collection of the first document found referencing it.
        collection: String,
        /// That document's id.
        id: String,
        /// The blob.
        blob_id: BlobId,
        /// What its bytes hash to.
        actual: BlobId,
    },
    /// A referenced blob cannot be looked up or read: a damaged manifest or chunk, or an I/O error.
    #[error(
        "collection {collection:?}, document {id:?} references blob {blob_id}, which cannot be read"
    )]
    Unreadable {
        /// The collection of the first document found referencing it.
        collection: String,
        /// That document's id.
        id: String,
        /// The blob.
        blob_id: BlobId,
        /// Why.
        #[source]
        source: persia_blob::Error,
    },
    /// The documents' references cannot be listed (a segment or WAL chunk that cannot be read).
    #[error("listing the documents' blob references")]
    Database(#[source] Box<DatabaseError>),
    /// The checksum scan found a damaged frame, or failed to read the pack.
    #[error("scanning the blob pack")]
    Pack(#[source] persia_blob::Error),
}

impl VerifyError {
    /// Whether this is an integrity failure (a dangling reference, damaged or mismatched bytes) rather than an I/O
    /// error: what `persia verify` exits with its corruption status for.
    pub fn is_corruption(&self) -> bool {
        match self {
            Self::Dangling { .. } | Self::HashMismatch { .. } => true,
            Self::Unreadable { source, .. } | Self::Pack(source) => source.is_corruption(),
            Self::Database(e) => e.is_corruption(),
        }
    }
}

impl From<DatabaseError> for VerifyError {
    fn from(e: DatabaseError) -> Self {
        Self::Database(Box::new(e))
    }
}

/// A referenced blob, with its manifest and the first document found referencing it.
struct Referenced {
    manifest: BlobManifest,
    collection: String,
    id: String,
}

impl<F: ContainerFile + Sync + 'static> ReadOnlyBlobDatabase<F> {
    /// Checks the documents' blob references against the pack, then, as `scan` asks, the pack's checksums and the
    /// referenced blobs' ids (see the module docs). Run it after
    /// [`ReadOnlyDatabase::verify`](persia_engine::db::ReadOnlyDatabase::verify), which checks the documents
    /// themselves. Memory: one manifest per referenced blob, and one window of a blob while it is re-hashed.
    ///
    /// ```
    /// use persia::internal::documents::{Blob, BlobDatabase, ReadOnlyBlobDatabase};
    /// use persia::internal::verify::{Scan, VerifyError};
    /// use persia_engine::db::DatabaseOptions;
    /// use persia_engine::document::Document;
    /// use persia_engine::schema::SchemaBuilder;
    /// use persia_engine::wal::Durability;
    /// use persia_blob::{BlobId, BlobRef};
    ///
    /// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// let mut db = BlobDatabase::create(&path, DatabaseOptions::default())?;
    /// db.log_mut().create_collection(SchemaBuilder::new("files").build()?)?;
    /// db.put("files", &Document::new("a"), Blob::from_reader(&b"bytes"[..]), Durability::Fsync)?;
    /// let reader = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default())?;
    /// let report = reader.verify_blobs(Scan::Deep)?;
    /// assert_eq!((report.references, report.blobs, report.rehashed), (1, 1, Some(1)));
    ///
    /// // A reference set below the public API, with no blob behind it.
    /// let mut doc = Document::new("b");
    /// doc.set_blob_ref(Some(BlobRef::new(BlobId::from_bytes([7; 32]), 3, "text/plain", 1 << 16, Default::default())?));
    /// db.log_mut().put("files", &doc, Durability::Fsync)?;
    /// let reader = ReadOnlyBlobDatabase::open(&path, &DatabaseOptions::default())?;
    /// assert!(matches!(reader.verify_blobs(Scan::Quick), Err(VerifyError::Dangling { id, .. }) if id == "b"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// The first failure found (see [`VerifyError`]).
    #[allow(clippy::result_large_err)] // Once per run, and the error names the document: not worth a box.
    pub fn verify_blobs(&self, scan: Scan) -> Result<BlobReport, VerifyError> {
        let store = self.blobs().store();
        let mut report = BlobReport {
            pack: store.exists(),
            bound: store.blob_ids().len(),
            ..BlobReport::default()
        };
        // In id order, so a deep run re-hashes (and reports) in the same order every time.
        let mut referenced: BTreeMap<BlobId, Referenced> = BTreeMap::new();
        self.database().blob_references(|r| {
            report.references = report.references.saturating_add(1);
            let blob_id = r.blob_ref.blob_id();
            let dangling = |missing| VerifyError::Dangling {
                collection: r.collection.clone(),
                id: r.id.clone(),
                blob_id,
                site: r.site,
                missing,
            };
            if !report.pack {
                return Err(dangling(Missing::NoPack));
            }
            let manifest = match referenced.get(&blob_id) {
                Some(found) => found.manifest.clone(),
                None => match store.manifest(blob_id) {
                    Ok(Some(manifest)) => manifest,
                    Ok(None) => return Err(dangling(Missing::NotBound)),
                    Err(source) => {
                        return Err(VerifyError::Unreadable {
                            collection: r.collection,
                            id: r.id,
                            blob_id,
                            source,
                        });
                    }
                },
            };
            if let Err(e) = manifest.check_ref(&r.blob_ref) {
                return Err(dangling(Missing::Mismatch(e.to_string())));
            }
            referenced.entry(blob_id).or_insert(Referenced {
                manifest,
                collection: r.collection,
                id: r.id,
            });
            Ok(())
        })?;
        report.blobs = referenced.len();
        if scan != Scan::Quick {
            report.scan = store.scan_checksums().map_err(VerifyError::Pack)?;
        }
        if scan == Scan::Deep {
            for (&blob_id, found) in &referenced {
                rehash(store, blob_id, found)?;
            }
            report.rehashed = Some(referenced.len());
        }
        Ok(report)
    }
}

/// Reads the blob `found` names whole, a window at a time, and checks that its bytes hash to `blob_id`.
#[allow(clippy::result_large_err)] // As `verify_blobs`.
fn rehash<F: ContainerFile + 'static>(
    store: &PackStore<F>,
    blob_id: BlobId,
    found: &Referenced,
) -> Result<(), VerifyError> {
    let manifest = &found.manifest;
    let unreadable = |source| VerifyError::Unreadable {
        collection: found.collection.clone(),
        id: found.id.clone(),
        blob_id,
        source,
    };
    // Whole chunks per window: a range is read by the chunks it touches.
    let chunk = u64::from(manifest.chunk_size().get());
    let window = chunk.saturating_mul(WINDOW_BYTES.checked_div(chunk).unwrap_or(1).max(1));
    let mut hasher = ContentHasher::new();
    let (mut scratch, mut out) = (Vec::new(), Vec::new());
    let mut start = 0;
    while start < manifest.size() {
        let end = start.saturating_add(window).min(manifest.size());
        out.clear();
        read_manifest_range_into(store, manifest, start..end, &mut scratch, &mut out)
            .map_err(unreadable)?;
        hasher.update(&out);
        start = end;
    }
    let actual = hasher.finish();
    if actual != blob_id {
        return Err(VerifyError::HashMismatch {
            collection: found.collection.clone(),
            id: found.id.clone(),
            blob_id,
            actual,
        });
    }
    Ok(())
}
