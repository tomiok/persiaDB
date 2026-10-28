//! What readers see (SPEC §4.5 step 4, §8.4, ROADMAP 6.3.3): the latest commit's segments, and each collection's
//! memtable.
//!
//! A commit builds a new [`Snapshot`] and swaps it in atomically once its superblock slot is durable; readers load
//! the current one without locking ([`DatabaseReader::snapshot`]) and keep it as long as they like. A memtable a
//! commit flushes stays in the snapshots that reference it, so every snapshot shows each document once: in the
//! memtable before the commit, in its segment after.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::manifest::Manifest;
use crate::memtable::{MemtableReader, MemtableView};
use crate::schema::CollectionSchema;
use crate::segment::Segment;

/// The state one commit published, plus each collection's memtable (SPEC §8.4).
#[derive(Debug)]
pub struct Snapshot {
    pub(super) manifest: Arc<Manifest>,
    pub(super) collections: BTreeMap<String, CollectionSnapshot>,
}

impl Snapshot {
    /// The commit that published it; 0 before the first.
    pub fn seq(&self) -> u64 {
        self.manifest.seq()
    }

    /// The committed manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The collection named `name`.
    pub fn collection(&self, name: &str) -> Option<&CollectionSnapshot> {
        self.collections.get(name)
    }

    /// Collections in ascending byte order of their names.
    pub fn collections(&self) -> impl ExactSizeIterator<Item = (&str, &CollectionSnapshot)> {
        self.collections.iter().map(|(n, c)| (n.as_str(), c))
    }
}

/// One collection in a [`Snapshot`].
#[derive(Debug, Clone)]
pub struct CollectionSnapshot {
    pub(super) schema: Arc<CollectionSchema>,
    pub(super) segments: Vec<Arc<Segment>>,
    pub(super) memtable: MemtableReader,
}

impl CollectionSnapshot {
    /// The committed schema.
    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// The live segments, oldest first, as the manifest lists them.
    pub fn segments(&self) -> &[Arc<Segment>] {
        &self.segments
    }

    /// A view of the memtable's writes so far. Its versions of ids hide the segments' (SPEC §8.3).
    pub fn memtable(&self) -> MemtableView {
        self.memtable.view()
    }
}

/// A handle that loads the latest [`Snapshot`], from any thread. Cheap to clone.
#[derive(Debug, Clone)]
pub struct DatabaseReader {
    pub(super) current: Arc<ArcSwap<Snapshot>>,
}

impl DatabaseReader {
    /// The latest published snapshot. Never blocks the writer, nor waits for it.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current.load_full()
    }

    /// The `seq` of the latest published snapshot.
    pub fn seq(&self) -> u64 {
        self.current.load().seq()
    }
}
