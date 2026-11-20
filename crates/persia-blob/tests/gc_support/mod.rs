//! Helpers shared by the GC tests: a pin set, and blobs to store.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    missing_docs
)]

use std::collections::BTreeSet;
use std::sync::Mutex;

use persia_blob::gc::{GcObject, PinCheck, PinGate};
use persia_blob::{BindOutcome, BlobId, BlobManifest, BlobStore, ChunkSize, UploadId, blob_id_of};
use persia_format::checksum::crc32c;

/// A pin set as the writer's: one lock over the pinned objects. `on_check` runs under the lock at the start of every
/// `unless_pinned`, before the check: a test pins there what a `put_blob` or a read would pin meanwhile.
#[derive(Default)]
pub struct Pins {
    pinned: Mutex<BTreeSet<GcObject>>,
    #[allow(clippy::type_complexity)]
    on_check: Mutex<Option<Box<dyn FnMut(&mut BTreeSet<GcObject>) + Send>>>,
    /// Every object `unless_pinned` was asked about, in order.
    pub checked: Mutex<Vec<GcObject>>,
}

impl Pins {
    pub fn new(pinned: impl IntoIterator<Item = GcObject>) -> Self {
        let pins = Self::default();
        pins.pinned.lock().unwrap().extend(pinned);
        pins
    }

    pub fn pin(&self, object: GcObject) {
        self.pinned.lock().unwrap().insert(object);
    }

    pub fn unpin(&self, object: GcObject) {
        self.pinned.lock().unwrap().remove(&object);
    }

    /// Runs `f` on the pinned set at each check, under the lock.
    pub fn on_check(&self, f: impl FnMut(&mut BTreeSet<GcObject>) + Send + 'static) {
        *self.on_check.lock().unwrap() = Some(Box::new(f));
    }
}

struct View<'a>(&'a BTreeSet<GcObject>);

impl PinCheck for View<'_> {
    fn is_pinned(&self, object: GcObject) -> bool {
        self.0.contains(&object)
    }

    fn pinned_blobs(&self) -> Vec<BlobId> {
        self.0
            .iter()
            .filter_map(|o| match o {
                GcObject::Blob(id) => Some(*id),
                GcObject::Upload(_) => None,
            })
            .collect()
    }
}

impl PinGate for Pins {
    fn pinned(&self) -> Vec<GcObject> {
        self.pinned.lock().unwrap().iter().copied().collect()
    }

    fn unless_pinned<T>(
        &self,
        object: GcObject,
        delete: impl FnOnce(&dyn PinCheck) -> T,
    ) -> Option<T> {
        let mut pinned = self.pinned.lock().unwrap();
        if let Some(f) = self.on_check.lock().unwrap().as_mut() {
            f(&mut pinned);
        }
        self.checked.lock().unwrap().push(object);
        if pinned.contains(&object) {
            return None;
        }
        Some(delete(&View(&pinned)))
    }
}

/// Deterministic bytes: `len` of them from `seed`.
pub fn data_of(len: usize, seed: u8) -> Vec<u8> {
    let mut x = u64::from(seed).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

/// The manifest of `data` in chunks of `chunk` bytes under `upload`.
pub fn manifest_of(data: &[u8], chunk: u32, upload: u128) -> BlobManifest {
    let crcs = data.chunks(chunk as usize).map(crc32c).collect();
    BlobManifest::new(
        blob_id_of(data),
        data.len() as u64,
        ChunkSize::from_encoding(chunk).unwrap(),
        UploadId::new(upload),
        crcs,
    )
    .unwrap()
}

/// Stages every chunk of `data` under `upload` and returns the (unbound) manifest.
pub fn stage<S: BlobStore + ?Sized>(
    store: &S,
    data: &[u8],
    chunk: u32,
    upload: u128,
) -> BlobManifest {
    let m = manifest_of(data, chunk, upload);
    for (i, c) in data.chunks(chunk as usize).enumerate() {
        store.stage_chunk(m.upload_id(), i as u32, c).unwrap();
    }
    m
}

/// [`stage`], then binds (`Bound` expected).
pub fn put<S: BlobStore + ?Sized>(
    store: &S,
    data: &[u8],
    chunk: u32,
    upload: u128,
) -> BlobManifest {
    let m = stage(store, data, chunk, upload);
    assert_eq!(store.bind(&m).unwrap(), BindOutcome::Bound);
    m
}

/// Every byte of the blob `m` describes, read back and checked (`read_range`).
pub fn read_all<S: BlobStore + ?Sized>(
    store: &S,
    m: &BlobManifest,
) -> persia_blob::Result<Vec<u8>> {
    let r = persia_format::BlobRef::new(
        m.blob_id(),
        m.size(),
        "a/b",
        m.chunk_size().get(),
        std::collections::BTreeMap::new(),
    )
    .unwrap();
    persia_blob::read_range(store, &r, 0..m.size())
}
