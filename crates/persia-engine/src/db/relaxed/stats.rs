//! Relaxation metrics (SPEC §7.6, §12.4; ROADMAP 8.3.3): how often the cascade relaxes, at which level its hits were
//! found, and how often a query still has none.
//!
//! Each collection of a database counts its relaxed searches in atomic counters its snapshots share
//! ([`RelaxationCounters`]), so a search on any thread counts without a lock. They are read as a [`RelaxationStats`]
//! snapshot: per collection ([`super::super::CollectionSnapshot::relaxation_stats`]) or for the whole database
//! ([`super::super::Database::relaxation_stats`]). They count from the open: nothing is stored.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::relax::RelaxLevel;

/// Every level, in cascade order: the index of a level's count in [`RelaxationStats`].
const LEVELS: [RelaxLevel; 7] = [
    RelaxLevel::Exact,
    RelaxLevel::Synonyms,
    RelaxLevel::DropTerms,
    RelaxLevel::Prefix,
    RelaxLevel::Fuzzy,
    RelaxLevel::Fallback,
    RelaxLevel::Filters,
];

const fn index(level: RelaxLevel) -> usize {
    match level {
        RelaxLevel::Exact => 0,
        RelaxLevel::Synonyms => 1,
        RelaxLevel::DropTerms => 2,
        RelaxLevel::Prefix => 3,
        RelaxLevel::Fuzzy => 4,
        RelaxLevel::Fallback => 5,
        RelaxLevel::Filters => 6,
    }
}

/// What relaxed searches did, counted since the database was opened (SPEC §7.6, §12.4; ROADMAP 8.3.3): enough for a
/// histogram of the levels hits were found at, the share of queries relaxed, and the share still empty after
/// relaxation.
///
/// A *query* is a relaxed search (`CollectionViews::search_relaxed`) that returned a response and was not given a
/// cursor: a cursor's page continues a query counted on its first page, so it counts nowhere. A page by `offset` runs
/// the cascade from the top as its first page does, and counts as a query. A request whose settings run L0 alone
/// (`relax: false`) runs the plain search and counts as a query too, at level `exact` (ROADMAP 10.1.21): `empty` when L0
/// found no hit. The engine's own plain search, run without a request, counts nowhere.
///
/// Each count is its own atomic counter, added to and read one at a time: a snapshot read while a search is in
/// flight may hold some of that query's counts and not the others (in any order). So the per-level counts add up to
/// `queries − empty` only when no search is in flight. An exporter that subtracts counts (`queries − empty`, or the
/// levels' sum from it) should use saturating subtraction.
///
/// ```
/// use persia_engine::db::{Database, DatabaseOptions, Execution, SearchRequest};
/// use persia_engine::document::{Document, Value};
/// use persia_engine::query::{Query, TermQuery};
/// use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
/// use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
/// use persia_engine::wal::Durability;
/// use persia_storage::container::sim::SimFile;
///
/// let schema = SchemaBuilder::new("c").field("body", FieldKind::Text(TextOptions::default())).build()?;
/// let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
/// let mut db = Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default())?;
/// db.create_collection(schema)?;
/// db.put("c", &Document::new("a").with(body, Value::String("red shoes".into())), Durability::Fsync)?;
///
/// let snapshot = db.snapshot();
/// let c = snapshot.collection("c").ok_or("no collection")?;
/// let memtable = c.memtable();
/// let views = c.views(&memtable)?;
/// let relax = RelaxPlan::new(c.schema().relaxation(), &RelaxOptions::default())?;
/// let term = |t: &str| Query::Term(TermQuery { field: body, term: t.into() });
/// let request = SearchRequest::default();
/// views.search_relaxed(&term("red"), &request, &relax, &Execution::default())?;
/// // `blue` is in no document: L3 finds nothing, L5 ranks the collection.
/// views.search_relaxed(&term("blue"), &request, &relax, &Execution::default())?;
///
/// let stats = db.relaxation_stats();
/// assert_eq!((stats.queries, stats.relaxed, stats.empty), (2, 1, 0));
/// assert_eq!(stats.hits_at(RelaxLevel::Exact), 1);
/// assert_eq!(stats.hits_at(RelaxLevel::Fallback), 1);
/// assert_eq!(stats.relaxed_ratio(), 0.5);
/// assert_eq!(stats, c.relaxation_stats());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelaxationStats {
    /// Queries searched.
    pub queries: u64,
    /// Queries whose cascade built a step after L0 that it did not skip for the limits: the response's `level` is not
    /// `exact` (SPEC §7.6).
    pub relaxed: u64,
    /// Queries no step of whose cascade found a hit, L0 included: the zero-result queries, after whatever relaxation
    /// ran. A query relaxation is not enabled for counts here too when L0 finds nothing.
    pub empty: u64,
    /// Per level in cascade order, the queries whose *hit step* (SPEC §7.6: the last step that found a hit) is of that
    /// level ([`Self::hits_at`]). An empty query has no hit step and is not counted here, so these add up to
    /// `queries − empty` when no search is in flight (above).
    hit_steps: [u64; 7],
}

impl RelaxationStats {
    /// The queries whose hit step is of `level` (SPEC §7.6): `exact` for those L0 found hits for and no later step
    /// added one.
    pub fn hits_at(&self, level: RelaxLevel) -> u64 {
        self.hit_steps.get(index(level)).copied().unwrap_or(0)
    }

    /// Every level in cascade order with [`Self::hits_at`] it: the level histogram.
    pub fn levels(&self) -> impl ExactSizeIterator<Item = (RelaxLevel, u64)> + '_ {
        LEVELS.iter().map(|&level| (level, self.hits_at(level)))
    }

    /// The share of queries relaxed, in `[0, 1]`; 0 before any query.
    pub fn relaxed_ratio(&self) -> f64 {
        ratio(self.relaxed, self.queries)
    }

    /// The share of queries still empty after relaxation (the zero-result rate), in `[0, 1]`; 0 before any query.
    pub fn empty_ratio(&self) -> f64 {
        ratio(self.empty, self.queries)
    }

    /// `self` and `other` counted together (saturating): a database's collections, or what a dropped one counted.
    #[must_use]
    pub fn plus(&self, other: &Self) -> Self {
        let mut hit_steps = self.hit_steps;
        for (sum, &n) in hit_steps.iter_mut().zip(&other.hit_steps) {
            *sum = sum.saturating_add(n);
        }
        Self {
            queries: self.queries.saturating_add(other.queries),
            relaxed: self.relaxed.saturating_add(other.relaxed),
            empty: self.empty.saturating_add(other.empty),
            hit_steps,
        }
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    // Int to float on purpose: counts below 2^53 convert exactly, and a share needs no more precision past it.
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    let (part, whole) = (part as f64, whole as f64);
    part / whole
}

/// The atomic counters behind a collection's [`RelaxationStats`], shared by its snapshots.
#[derive(Debug, Default)]
pub(crate) struct RelaxationCounters {
    queries: AtomicU64,
    relaxed: AtomicU64,
    empty: AtomicU64,
    hit_steps: [AtomicU64; 7],
}

impl RelaxationCounters {
    /// Counts a query whose cascade `relaxed` (or not) and whose hit step is of `hit` (`None`: no step found a hit).
    pub(crate) fn record(&self, relaxed: bool, hit: Option<RelaxLevel>) {
        // Each counter is its own: a reader may see one query counted in `queries` and not yet in the others.
        // `fetch_add` wraps at `u64::MAX`, which is unreachable: a count (not a size or an offset) that a query per
        // nanosecond takes 584 years to pass. One `fetch_add` costs a search less than a compare-and-swap loop that
        // threads counting together retry.
        let add = |c: &AtomicU64| {
            c.fetch_add(1, Ordering::Relaxed);
        };
        add(&self.queries);
        if relaxed {
            add(&self.relaxed);
        }
        match hit {
            Some(level) => {
                if let Some(c) = self.hit_steps.get(index(level)) {
                    add(c);
                }
            }
            None => add(&self.empty),
        }
    }

    /// What the counters hold now.
    pub(crate) fn load(&self) -> RelaxationStats {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        RelaxationStats {
            queries: load(&self.queries),
            relaxed: load(&self.relaxed),
            empty: load(&self.empty),
            hit_steps: self.hit_steps.each_ref().map(load),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_counts_once_in_queries_and_once_in_empty_or_its_hit_level() {
        let counters = RelaxationCounters::default();
        counters.record(false, Some(RelaxLevel::Exact));
        counters.record(true, Some(RelaxLevel::Fuzzy));
        counters.record(true, Some(RelaxLevel::Fuzzy));
        counters.record(true, None);
        counters.record(false, None);
        let stats = counters.load();
        assert_eq!((stats.queries, stats.relaxed, stats.empty), (5, 3, 2));
        let levels: Vec<(RelaxLevel, u64)> = stats.levels().collect();
        assert_eq!(
            levels,
            [
                (RelaxLevel::Exact, 1),
                (RelaxLevel::Synonyms, 0),
                (RelaxLevel::DropTerms, 0),
                (RelaxLevel::Prefix, 0),
                (RelaxLevel::Fuzzy, 2),
                (RelaxLevel::Fallback, 0),
                (RelaxLevel::Filters, 0),
            ]
        );
        let found: u64 = stats.levels().map(|(_, n)| n).sum();
        assert_eq!(found, stats.queries - stats.empty);
        assert_eq!(stats.relaxed_ratio(), 0.6);
        assert_eq!(stats.empty_ratio(), 0.4);
    }

    #[test]
    fn every_level_has_its_own_count() {
        let counters = RelaxationCounters::default();
        for (i, &level) in LEVELS.iter().enumerate() {
            for _ in 0..=i {
                counters.record(true, Some(level));
            }
        }
        let stats = counters.load();
        for (i, &level) in LEVELS.iter().enumerate() {
            assert_eq!(stats.hits_at(level), i as u64 + 1, "{level:?}");
        }
    }

    #[test]
    fn no_query_is_a_ratio_of_zero_and_sums_saturate() {
        let none = RelaxationStats::default();
        assert_eq!((none.relaxed_ratio(), none.empty_ratio()), (0.0, 0.0));
        let counters = RelaxationCounters::default();
        counters.record(true, Some(RelaxLevel::Prefix));
        let one = counters.load();
        let full = RelaxationStats {
            queries: u64::MAX,
            relaxed: u64::MAX,
            empty: u64::MAX,
            hit_steps: [u64::MAX; 7],
        };
        assert_eq!(full.plus(&one), full);
        let two = one.plus(&one);
        assert_eq!(
            (two.queries, two.relaxed, two.hits_at(RelaxLevel::Prefix)),
            (2, 2, 2)
        );
    }
}
