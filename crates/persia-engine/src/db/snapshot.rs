//! What readers see (SPEC §4.5 step 4, §8.4, ROADMAP 6.3.3): the latest commit's segments, and each collection's
//! memtable.
//!
//! A commit builds a new [`Snapshot`] and swaps it in atomically once its superblock slot is durable; readers load
//! the current one without locking ([`DatabaseReader::snapshot`]) and keep it as long as they like. A memtable a
//! commit flushes stays in the snapshots that reference it, so every snapshot shows each document once: in the
//! memtable before the commit, in its segment after.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;

use crate::document::Document;
use crate::manifest::Manifest;
use crate::memtable::{IdState, MemtableReader, MemtableView};
use crate::schema::CollectionSchema;
use crate::segment::{ReadError, Segment, Tombstones};
use crate::stats::{SearchStatistics, SegmentStatistics};
use crate::view::{SearchView, SegmentView};

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
    /// The deletions of each segment, by its index in `segments`, as of the commit (SPEC §8.3).
    pub(super) tombstones: Vec<Option<Arc<Tombstones>>>,
    pub(super) memtable: MemtableReader,
    /// The segments' collection statistics, computed on first use (ROADMAP 7.2.1).
    pub(super) statistics: Arc<OnceLock<SegmentStatistics>>,
}

/// Where the live version of a document id is (SPEC §8.3, ROADMAP 6.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Located {
    /// In the memtable, with this docid.
    Memtable(u32),
    /// In a committed segment.
    Segment {
        /// The segment's index in [`CollectionSnapshot::segments`].
        index: usize,
        /// Its docid there.
        docid: u32,
    },
}

impl CollectionSnapshot {
    /// The committed schema.
    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// The live segments, oldest first, as the manifest lists them. Some of their documents may be deleted:
    /// [`Self::segment_views`] leaves those out.
    pub fn segments(&self) -> &[Arc<Segment>] {
        &self.segments
    }

    /// The deleted documents of each segment of [`Self::segments`], in the same order; `None` for a segment
    /// without deletions.
    pub fn tombstones(&self) -> &[Option<Arc<Tombstones>>] {
        &self.tombstones
    }

    /// Each segment with its committed deletions, oldest first. Before a commit, the memtable's versions and deletes of
    /// ids also hide segment copies the tombstones do not yet delete: a search over these views and the memtable
    /// must apply that too (SPEC §8.3, ROADMAP 7.4.6); [`Self::lookup`] does.
    pub fn segment_views(
        &self,
    ) -> impl DoubleEndedIterator<Item = SegmentView<'_>> + ExactSizeIterator {
        self.segments
            .iter()
            .zip(&self.tombstones)
            .map(|(s, t)| SegmentView::new(s, t.as_deref()))
    }

    /// A view of the memtable's writes so far. Its versions of ids hide the segments' (SPEC §8.3).
    pub fn memtable(&self) -> MemtableView {
        self.memtable.view()
    }

    /// The collection statistics of the segments (SPEC §7.2, ROADMAP 7.2.1): `N` and the field totals, computed
    /// once per snapshot, and `df`, cached as it is asked for.
    pub fn statistics(&self) -> &SegmentStatistics {
        self.statistics
            .get_or_init(|| SegmentStatistics::new(&self.segments))
    }

    /// What a search over the segments and `memtable` (a view of [`Self::memtable`]) scores with: the segments'
    /// [`Self::statistics`] plus the memtable's live documents (SPEC §7.2, §7.3).
    pub fn search_statistics<'a>(&'a self, memtable: &'a MemtableView) -> SearchStatistics<'a> {
        SearchStatistics::new(self.statistics(), memtable)
    }

    /// Where the live version of `id` is, as of `memtable` (a view of [`Self::memtable`]) and this snapshot's
    /// segments (SPEC §8.3, ROADMAP 6.4.1): the memtable's version or delete of the id, if it has one, hides every
    /// segment's; otherwise the newest segment whose `IdMap` holds the id decides: its copy if live, `None` if its
    /// tombstones delete it (older copies are never consulted, so a deleted id stays deleted even if an older
    /// segment still held it live). `None` if no segment holds the id.
    ///
    /// # Errors
    /// A corruption of a segment's `IdMap`.
    pub fn lookup(&self, memtable: &MemtableView, id: &str) -> Result<Option<Located>, ReadError> {
        match memtable.lookup(id) {
            Some(IdState::Live(docid)) => return Ok(Some(Located::Memtable(docid))),
            Some(IdState::Deleted) => return Ok(None),
            None => {}
        }
        Ok(newest(self.segment_views(), id)?
            .map(|(index, docid)| Located::Segment { index, docid }))
    }

    /// The live document with id `id` ([`Self::lookup`] with a view of the memtable taken now). A snapshot loaded
    /// after a write sees it (read-your-writes), and so does one loaded before it, up to the commit that flushes its
    /// memtable: snapshots loaded from then on see it in a segment.
    ///
    /// # Errors
    /// As [`Self::lookup`]; a corruption of the segment's `DocStore`.
    pub fn get(&self, id: &str) -> Result<Option<Document>, ReadError> {
        let memtable = self.memtable();
        match self.lookup(&memtable, id)? {
            None => Ok(None),
            Some(Located::Memtable(docid)) => memtable.document(docid).map(Some),
            Some(Located::Segment { index, docid }) => self
                .segments
                .get(index)
                .map(|s| s.document(docid))
                .transpose(),
        }
    }
}

/// The newest of `views` (oldest first) whose `IdMap` holds `id`, by index, and its docid there, if that copy is live.
fn newest<'s>(
    views: impl DoubleEndedIterator<Item = SegmentView<'s>> + ExactSizeIterator,
    id: &str,
) -> Result<Option<(usize, u32)>, ReadError> {
    for (index, view) in views.enumerate().rev() {
        if let Some(docid) = view.segment().docid_of(id)? {
            return Ok(view.is_live(docid).then_some((index, docid)));
        }
    }
    Ok(None)
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
