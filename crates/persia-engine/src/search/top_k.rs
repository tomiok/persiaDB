//! The top-k collector (ROADMAP 7.4.3): a bounded heap whose order is total and deterministic.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// A matching document and its score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// The document in its view.
    pub docid: u32,
    /// Its score (SPEC §7.2).
    pub score: f64,
}

impl Hit {
    /// The rank order: a higher score first, then a lower docid; total, so the top `k` of a set of hits is always
    /// the same, whatever order they come in. `-0.0` ties with `0.0`.
    pub fn rank(&self, other: &Self) -> Ordering {
        (other.score + 0.0)
            .total_cmp(&(self.score + 0.0))
            .then(self.docid.cmp(&other.docid))
    }
}

/// [`Hit`] ordered so the heap's greatest element is the worst hit kept, the one a better hit replaces.
#[derive(Debug, Clone, Copy)]
struct Worst(Hit);

impl PartialEq for Worst {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Worst {}

impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.rank(&other.0)
    }
}

/// The best `k` hits pushed so far, by [`Hit::rank`]: O(log k) per hit, `k` hits of memory.
///
/// ```
/// use persia_engine::search::TopK;
///
/// let mut top = TopK::new(2);
/// for (docid, score) in [(0, 1.0), (1, 3.0), (2, 1.0), (3, 2.0)] {
///     top.push(docid, score);
/// }
/// let hits: Vec<_> = top.into_hits().iter().map(|h| (h.docid, h.score)).collect();
/// assert_eq!(hits, [(1, 3.0), (3, 2.0)]);
/// ```
#[derive(Debug, Clone)]
pub struct TopK {
    k: usize,
    heap: BinaryHeap<Worst>,
}

impl TopK {
    /// A collector of at most `k` hits; it keeps none for `k = 0`.
    pub fn new(k: usize) -> Self {
        Self {
            k,
            // A huge `k` (a deep page) should not reserve memory for hits that never come.
            heap: BinaryHeap::with_capacity(k.min(1024)),
        }
    }

    /// Offers a hit; it is kept if fewer than `k` are, or if it ranks above the worst kept one, which it replaces.
    /// A score of `-0.0` counts as `0.0`, so equal scores tie on docid.
    pub fn push(&mut self, docid: u32, score: f64) {
        if self.k == 0 {
            return;
        }
        let hit = Worst(Hit {
            docid,
            score: score + 0.0,
        });
        if self.heap.len() < self.k {
            self.heap.push(hit);
        } else if let Some(mut worst) = self.heap.peek_mut()
            && hit < *worst
        {
            *worst = hit;
        }
    }

    /// Hits kept so far.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether no hit is kept.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// The kept hits, best first.
    pub fn into_hits(self) -> Vec<Hit> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|w| w.0)
            .collect()
    }
}
