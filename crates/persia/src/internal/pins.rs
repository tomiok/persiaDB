//! The writer's pin set (SPEC §9.5, ADR-0034 item 7, ADR-0051): the upload ids and blob ids that GC must not sweep
//! although nothing durable references them yet.
//!
//! `put_blob` pins its upload id before its first chunk write and the blob id it returns before it binds, and
//! hands both back as one [`Pins`] guard. The guard releases them when it is dropped: `put(doc, blob)` keeps it
//! until the document that references the blob is durable (ADR-0051), a caller of `put_blob` alone until it drops
//! it. One id may be pinned by several guards at once (two uploads of the same bytes): it stays pinned until the last
//! one is dropped.
//!
//! GC's sweep (ROADMAP 9.5.2) deletes an object only through [`PinSet::unless_pinned`], which checks the pin set
//! and runs the delete as one step under its lock, so a pin taken after the mark still protects what it names.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use persia_blob::{BlobId, UploadId};

/// What a pin protects from GC (SPEC §9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pinned {
    /// The chunks staged under an upload.
    Upload(UploadId),
    /// A bound blob's manifest. Whoever pins one also pins the upload its manifest names (`put_blob` and
    /// `pin_contained` do), after reading the manifest; a sweep must therefore not delete an upload's chunks while a
    /// pinned blob's manifest names it, which it checks under the lock (see [`PinSet::unless_pinned`]).
    Blob(BlobId),
}

/// The pin set of one writer (see the module docs). Shared by `Arc` between `put_blob`, the [`Pins`] guards it
/// returns, and GC.
///
/// ```
/// use std::sync::Arc;
/// use persia::internal::pins::{PinSet, Pinned};
/// use persia_blob::UploadId;
///
/// let set = Arc::new(PinSet::new());
/// let upload = Pinned::Upload(UploadId::new(7));
/// let pins = set.pin([upload]);
/// assert!(set.is_pinned(upload));
/// // The sweep's delete does not run while the pin holds.
/// assert_eq!(set.unless_pinned(upload, || "deleted"), None);
/// drop(pins);
/// assert_eq!(set.unless_pinned(upload, || "deleted"), Some("deleted"));
/// ```
#[derive(Debug, Default)]
pub struct PinSet {
    /// How many live [`Pins`] name each id.
    pinned: Mutex<BTreeMap<Pinned, usize>>,
}

impl PinSet {
    /// An empty pin set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pins every id of `ids`; the returned guard releases them when dropped.
    pub fn pin(self: &Arc<Self>, ids: impl IntoIterator<Item = Pinned>) -> Pins {
        let mut pins = Pins::empty(self);
        for id in ids {
            pins.add(id);
        }
        pins
    }

    /// Whether `id` is pinned now.
    pub fn is_pinned(&self, id: Pinned) -> bool {
        self.lock().contains_key(&id)
    }

    /// Every id pinned now, in order: what GC's mark adds to the ids it found referenced (SPEC §9.5). A pin taken
    /// after this returns is still honored by [`PinSet::unless_pinned`].
    pub fn pinned(&self) -> Vec<Pinned> {
        self.lock().keys().copied().collect()
    }

    /// Runs `delete` unless `id` is pinned, holding the pin set's lock throughout (SPEC §9.5: checking an object
    /// against the pin set and deleting it is one step), and returns its result; `None` when `id` is pinned and
    /// nothing ran. No pin can be taken while `delete` runs, so `delete` must not call `put_blob` or take a pin.
    ///
    /// A `Blob` pin is taken before its manifest is read and its `Upload` pin after, so between the two the upload's
    /// chunks are protected only by the blob pin: the sweep (ROADMAP 9.5.2), deleting an upload's chunks, must also
    /// find no pinned blob whose manifest names that upload, checked inside `delete` (which may read manifests).
    pub fn unless_pinned<T>(&self, id: Pinned, delete: impl FnOnce() -> T) -> Option<T> {
        let pinned = self.lock();
        if pinned.contains_key(&id) {
            return None;
        }
        let out = delete();
        drop(pinned);
        Some(out)
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<Pinned, usize>> {
        // Every change is one insert, increment, decrement or remove: a panic elsewhere leaves the map consistent.
        self.pinned.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn acquire(&self, id: Pinned) {
        let mut pinned = self.lock();
        let count = pinned.entry(id).or_insert(0);
        *count = count.saturating_add(1);
    }

    fn release(&self, id: Pinned) {
        let mut pinned = self.lock();
        if let Some(count) = pinned.get_mut(&id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                pinned.remove(&id);
            }
        }
    }
}

/// A guard over pins of a [`PinSet`]: each id it holds stays pinned until the guard is dropped (SPEC §9.5).
#[derive(Debug)]
#[must_use = "dropping the guard releases its pins at once"]
pub struct Pins {
    set: Arc<PinSet>,
    ids: Vec<Pinned>,
}

impl Pins {
    /// A guard over no pin of `set`.
    pub fn empty(set: &Arc<PinSet>) -> Self {
        Self {
            set: Arc::clone(set),
            ids: Vec::new(),
        }
    }

    /// Pins `id` too, until this guard is dropped. Takes effect before it returns.
    pub fn add(&mut self, id: Pinned) {
        self.set.acquire(id);
        self.ids.push(id);
    }

    /// The ids this guard holds, in the order they were pinned.
    pub fn ids(&self) -> &[Pinned] {
        &self.ids
    }
}

impl Drop for Pins {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            self.set.release(id);
        }
    }
}
