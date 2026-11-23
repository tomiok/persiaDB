//! The facet counts of relaxed searches, counted once per query (SPEC §7.5, §7.6; ROADMAP 8.1.11, ADR-0061).
//!
//! Every page of one relaxed query on one snapshot counts the same facets (ROADMAP 8.1.9): those of the steps a page
//! holding every hit builds. A page computes them by searching every such step, those before its cursor's and past
//! its own included, which a deep cursor page would otherwise pay again only to count. A page from the top that counts
//! every one of them in full keeps them here, and later pages of the same query read them instead. A cursor page does
//! not keep its counts: its cursor may come from a page the budget truncated, whose last step stops short of the steps
//! a page from the top builds, so it may count fewer.
//!
//! The cache belongs to a collection's snapshot ([`super::super::CollectionSnapshot`]), so it lives while that
//! snapshot is the database's and goes with it at the next publish: a cursor never pages another snapshot (SPEC §7.4).
//! A key names everything the counts follow from beside the snapshot: the memtable view's version, the query and its
//! compilation, the filters marked soft, the relaxation settings, the step limits and the facets asked for. The cache
//! holds at most [`MAX_ENTRIES`] counts of at most [`MAX_BYTES`] in all (keys included, as estimated), and evicts the
//! least recently used first, by a counter of uses: no clock. A page reads it by a [`KeyRef`], borrowing the query and
//! its compilation, and copies them into a [`Key`] only to keep new counts, sized before the cache is locked.

use crate::query::{Compiled, Filter, Query, QueryLimits};
use crate::relax::RelaxSettings;
use crate::search::{FacetCounts, FacetRequest};

/// The most counts a snapshot's cache holds.
pub(crate) const MAX_ENTRIES: usize = 32;

/// The most bytes (as [`Kept::new`] estimates them) a snapshot's cache holds; counts that alone need more are not kept.
pub(crate) const MAX_BYTES: usize = 1 << 20;

/// What a relaxed query's facet counts follow from, beside the snapshot that holds the cache, as a page borrows it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyRef<'a> {
    /// The memtable view's version: another view holds other documents.
    pub(crate) version: u64,
    pub(crate) query: &'a Query,
    /// The compilation the levels read (clauses, synonyms, filters), `None` for a programmatic query.
    pub(crate) compiled: Option<&'a Compiled>,
    /// The filters the filter step may drop (ROADMAP 8.2.13).
    pub(crate) soft: &'a [Filter],
    /// The levels, `min_results`, `fill` and `filters` (`None`: L0 alone).
    pub(crate) settings: Option<RelaxSettings>,
    /// The limits the steps are held to (SPEC §7.1.3): other limits cut or skip other steps.
    pub(crate) limits: QueryLimits,
    pub(crate) facets: &'a [FacetRequest],
}

impl KeyRef<'_> {
    /// The key this names, owned: for counts to keep.
    pub(crate) fn to_key(self) -> Key {
        Key {
            version: self.version,
            query: self.query.clone(),
            compiled: self.compiled.cloned(),
            soft: self.soft.to_vec(),
            settings: self.settings,
            limits: self.limits,
            facets: self.facets.to_vec(),
        }
    }
}

/// [`KeyRef`], owned: what the cache keeps counts by.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Key {
    version: u64,
    query: Query,
    compiled: Option<Compiled>,
    soft: Vec<Filter>,
    settings: Option<RelaxSettings>,
    limits: QueryLimits,
    facets: Vec<FacetRequest>,
}

impl Key {
    /// Whether `key` names this key, compared field by field without copying it.
    fn matches(&self, key: &KeyRef<'_>) -> bool {
        self.version == key.version
            && self.settings == key.settings
            && self.limits == key.limits
            && self.facets == key.facets
            && self.soft == key.soft
            && self.compiled.as_ref() == key.compiled
            && self.query == *key.query
    }

    /// About how many bytes the key holds: its debug form's length, a bound of its strings and a count of its nodes.
    fn bytes(&self) -> usize {
        format!("{self:?}").len()
    }
}

/// About how many bytes `counts` hold: each value's bytes and 16 more for its count and length.
fn counts_bytes(counts: &[FacetCounts]) -> usize {
    counts
        .iter()
        .flat_map(|c| &c.values)
        .map(|v| v.value.len().saturating_add(16))
        .fold(0usize, usize::saturating_add)
}

/// Counts to keep for a key, sized ([`Self::new`]) before the cache is locked.
#[derive(Debug)]
pub(crate) struct Kept {
    key: Key,
    counts: Vec<FacetCounts>,
    bytes: usize,
}

impl Kept {
    /// `counts` for `key`, with their estimated size: the key's and the counts'.
    pub(crate) fn new(key: Key, counts: Vec<FacetCounts>) -> Self {
        let bytes = key.bytes().saturating_add(counts_bytes(&counts));
        Self { key, counts, bytes }
    }
}

#[derive(Debug)]
struct Entry {
    kept: Kept,
    /// The use counter when it was last read or kept.
    used: u64,
}

/// A snapshot's facet counts by query ([`Key`]), bounded by entries and bytes.
#[derive(Debug, Default)]
pub(crate) struct FacetCache {
    entries: Vec<Entry>,
    /// Counts every read and insert: the order of use, for eviction.
    uses: u64,
    bytes: usize,
}

impl FacetCache {
    /// The counts kept for `key`, now the most recently used.
    pub(crate) fn get(&mut self, key: &KeyRef<'_>) -> Option<Vec<FacetCounts>> {
        self.uses = self.uses.wrapping_add(1);
        let uses = self.uses;
        let entry = self.entries.iter_mut().find(|e| e.kept.key.matches(key))?;
        entry.used = uses;
        Some(entry.kept.counts.clone())
    }

    /// Keeps `kept`, evicting the least recently used counts until it fits; not when it alone is over [`MAX_BYTES`].
    /// Counts already kept for its key are replaced.
    pub(crate) fn insert(&mut self, kept: Kept) {
        if kept.bytes > MAX_BYTES {
            return;
        }
        if let Some(at) = self.entries.iter().position(|e| e.kept.key == kept.key) {
            let old = self.entries.swap_remove(at);
            self.bytes = self.bytes.saturating_sub(old.kept.bytes);
        }
        while self.entries.len() >= MAX_ENTRIES || self.bytes.saturating_add(kept.bytes) > MAX_BYTES
        {
            let Some(oldest) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.used)
                .map(|(i, _)| i)
            else {
                break;
            };
            let old = self.entries.swap_remove(oldest);
            self.bytes = self.bytes.saturating_sub(old.kept.bytes);
        }
        self.uses = self.uses.wrapping_add(1);
        self.bytes = self.bytes.saturating_add(kept.bytes);
        self.entries.push(Entry {
            kept,
            used: self.uses,
        });
    }

    /// How many counts it holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)] // Tests: inputs are fixed.

    use super::*;
    use crate::schema::FieldId;
    use crate::search::FacetValue;

    static ALL: Query = Query::MatchAll;

    fn key(version: u64) -> KeyRef<'static> {
        KeyRef {
            version,
            query: &ALL,
            compiled: None,
            soft: &[],
            settings: None,
            limits: QueryLimits::default(),
            facets: &[],
        }
    }

    fn kept(version: u64, counts: Vec<FacetCounts>) -> Kept {
        Kept::new(key(version).to_key(), counts)
    }

    fn counts(bytes: usize) -> Vec<FacetCounts> {
        vec![FacetCounts {
            field: FieldId::new(1),
            values: vec![FacetValue {
                value: vec![b'x'; bytes],
                count: 1,
            }],
        }]
    }

    #[test]
    fn a_key_reads_back_its_counts_and_another_key_none() {
        let mut cache = FacetCache::default();
        cache.insert(kept(1, counts(3)));
        assert_eq!(cache.get(&key(1)), Some(counts(3)));
        assert_eq!(cache.get(&key(2)), None);
        // Kept again, replaced.
        cache.insert(kept(1, counts(4)));
        assert_eq!(cache.get(&key(1)), Some(counts(4)));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn the_least_recently_used_goes_first_past_the_entries() {
        let mut cache = FacetCache::default();
        for v in 0..u64::try_from(MAX_ENTRIES).unwrap() {
            cache.insert(kept(v, counts(1)));
        }
        // Read 0: 1 is now the least recently used.
        assert!(cache.get(&key(0)).is_some());
        cache.insert(kept(1000, counts(1)));
        assert_eq!(cache.len(), MAX_ENTRIES);
        assert!(cache.get(&key(1)).is_none());
        assert!(cache.get(&key(0)).is_some());
        assert!(cache.get(&key(1000)).is_some());
    }

    #[test]
    fn bytes_bound_the_cache_and_counts_over_it_alone_are_not_kept() {
        let mut cache = FacetCache::default();
        cache.insert(kept(1, counts(MAX_BYTES)));
        assert_eq!(cache.len(), 0);
        let half = MAX_BYTES / 2;
        cache.insert(kept(1, counts(half)));
        cache.insert(kept(2, counts(half)));
        // Both do not fit: the first went.
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&key(2)).is_some());
        assert!(cache.get(&key(1)).is_none());
    }

    #[test]
    fn a_key_matches_the_ref_it_was_made_from_and_no_other() {
        let red = Query::Term(crate::query::TermQuery {
            field: FieldId::new(0),
            term: "red".into(),
        });
        let hat = Query::Term(crate::query::TermQuery {
            field: FieldId::new(0),
            term: "hat".into(),
        });
        let of = |query| KeyRef { query, ..key(1) };
        let kept = of(&red).to_key();
        assert!(kept.matches(&of(&red)));
        assert!(!kept.matches(&of(&hat)));
        assert!(!kept.matches(&KeyRef {
            version: 2,
            ..of(&red)
        }));
        assert!(!kept.matches(&KeyRef {
            limits: QueryLimits::UNLIMITED,
            ..of(&red)
        }));
    }
}
