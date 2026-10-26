//! Term → postings accumulation for one `text` or `keyword` field (ROADMAP 5.1.1).
//!
//! Postings are appended to one flat log in docid order, a `(term, doc, tf, positions)` entry per term a document
//! has, never to a per-term list: a term costs only its [`Arena`] entry (its bytes, an end offset, a hash and a
//! table slot) until the writer groups the log by term with one counting sort (`by_term`). Field lengths (the norms
//! input) are kept per document that has any (ROADMAP 5.1.1.2).

use super::super::SegmentError;
use super::super::arena::Arena;

/// One term of one document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Posting {
    pub(crate) term: u32,
    pub(crate) doc: u32,
    /// Occurrences of the term in the document's field, at least 1.
    pub(crate) tf: u32,
    /// Where the document's positions of the term start in [`Inverted::positions`] (0 without positions).
    pub(crate) pos_start: u32,
}

/// The lengths of an [`Inverted`]'s accumulators at one point.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mark {
    terms: usize,
    postings: usize,
    positions: usize,
    lengths: usize,
    sum_len: u64,
}

/// The inverted index of one field, as documents arrive.
#[derive(Debug)]
pub(crate) struct Inverted {
    pub(crate) terms: Arena,
    /// In docid order, and within a document in term id order.
    pub(crate) postings: Vec<Posting>,
    /// Positions of each posting, `tf` of them from its `pos_start`, ascending; `None` without positions.
    pub(crate) positions: Option<Vec<u32>>,
    /// `(docid, field length)` of each document with a length above 0, in docid order (SPEC §5.3.4).
    pub(crate) lengths: Vec<(u32, u32)>,
    /// Sum of the field lengths (SPEC §5.1 `sum_field_len`).
    pub(crate) sum_len: u64,
}

impl Inverted {
    pub(crate) fn new(positions: bool) -> Self {
        Self {
            terms: Arena::default(),
            postings: Vec::new(),
            positions: positions.then(Vec::new),
            lengths: Vec::new(),
            sum_len: 0,
        }
    }

    /// The term id of `term`, interning it.
    pub(crate) fn intern(&mut self, term: &[u8]) -> Result<(u32, bool), SegmentError> {
        self.terms
            .intern(term)
            .map_err(|_| SegmentError::TooLarge { what: "terms" })
    }

    /// Records document `doc`, whose field has `len` tokens (or values) and these `(term id, position)`
    /// occurrences, in any order. `doc` must be above every docid added before.
    pub(crate) fn add_doc(
        &mut self,
        doc: u32,
        occurrences: &mut [(u32, u32)],
        len: u32,
    ) -> Result<(), SegmentError> {
        if len == 0 {
            return Ok(());
        }
        occurrences.sort_unstable();
        let too_many = || SegmentError::TooLarge { what: "positions" };
        for group in occurrences.chunk_by(|a, b| a.0 == b.0) {
            let Some(&(term, _)) = group.first() else {
                continue;
            };
            let tf = u32::try_from(group.len()).map_err(|_| too_many())?;
            let pos_start = match &mut self.positions {
                Some(positions) => {
                    let start = u32::try_from(positions.len()).map_err(|_| too_many())?;
                    positions.extend(group.iter().map(|&(_, p)| p));
                    // The next start must fit too, so every slice ends within u32.
                    u32::try_from(positions.len()).map_err(|_| too_many())?;
                    start
                }
                None => 0,
            };
            self.postings.push(Posting {
                term,
                doc,
                tf,
                pos_start,
            });
        }
        self.lengths.push((doc, len));
        self.sum_len = self.sum_len.saturating_add(u64::from(len));
        Ok(())
    }

    /// Where the accumulators end, for [`Self::rollback`].
    pub(crate) fn mark(&self) -> Mark {
        Mark {
            terms: self.terms.len(),
            postings: self.postings.len(),
            positions: self.positions.as_ref().map_or(0, Vec::len),
            lengths: self.lengths.len(),
            sum_len: self.sum_len,
        }
    }

    /// Undoes everything added since `mark`, terms included.
    pub(crate) fn rollback(&mut self, mark: &Mark) {
        self.terms.truncate(mark.terms);
        self.postings.truncate(mark.postings);
        if let Some(p) = &mut self.positions {
            p.truncate(mark.positions);
        }
        self.lengths.truncate(mark.lengths);
        self.sum_len = mark.sum_len;
    }

    /// Documents with a field length above 0 (SPEC §5.1 `docs_with_field`).
    pub(crate) fn docs_with_field(&self) -> u32 {
        u32::try_from(self.lengths.len()).unwrap_or(u32::MAX)
    }

    /// The positions of `posting`.
    pub(crate) fn positions_of(&self, posting: &Posting) -> &[u32] {
        let Some(positions) = &self.positions else {
            return &[];
        };
        let start = posting.pos_start as usize;
        let end = start.saturating_add(posting.tf as usize);
        positions.get(start..end).unwrap_or_default()
    }

    /// The postings grouped by term, terms in ascending byte order.
    pub(crate) fn by_term(&self) -> Grouped {
        let order = self.terms.sorted_ids();
        let mut rank = vec![0_usize; order.len()];
        for (r, &id) in order.iter().enumerate() {
            if let Some(slot) = rank.get_mut(id as usize) {
                *slot = r;
            }
        }
        // Counting sort by rank; stable, so each term keeps docid order.
        let mut starts = vec![0_usize; order.len().saturating_add(1)];
        for p in &self.postings {
            if let Some(c) = rank
                .get(p.term as usize)
                .and_then(|&r| starts.get_mut(r.saturating_add(1)))
            {
                *c = c.saturating_add(1);
            }
        }
        for i in 1..starts.len() {
            let prev = starts.get(i.wrapping_sub(1)).copied().unwrap_or(0);
            if let Some(s) = starts.get_mut(i) {
                *s = s.saturating_add(prev);
            }
        }
        let mut next = starts.clone();
        let mut sorted = vec![
            Posting {
                term: 0,
                doc: 0,
                tf: 0,
                pos_start: 0
            };
            self.postings.len()
        ];
        for p in &self.postings {
            let Some(n) = rank.get(p.term as usize).and_then(|&r| next.get_mut(r)) else {
                continue;
            };
            if let Some(slot) = sorted.get_mut(*n) {
                *slot = *p;
            }
            *n = n.saturating_add(1);
        }
        Grouped {
            order,
            postings: sorted,
            starts,
        }
    }

    pub(crate) fn size_in_bytes(&self) -> usize {
        self.terms
            .size_in_bytes()
            .saturating_add(
                self.postings
                    .capacity()
                    .saturating_mul(size_of::<Posting>()),
            )
            .saturating_add(
                self.positions
                    .as_ref()
                    .map_or(0, |p| p.capacity().saturating_mul(size_of::<u32>())),
            )
            .saturating_add(
                self.lengths
                    .capacity()
                    .saturating_mul(size_of::<(u32, u32)>()),
            )
    }
}

/// Postings grouped by term: `order[r]` is the term id of rank `r` (ascending bytes), whose postings are
/// `postings[starts[r]..starts[r + 1]]`, in docid order.
#[derive(Debug)]
pub(crate) struct Grouped {
    pub(crate) order: Vec<u32>,
    pub(crate) postings: Vec<Posting>,
    pub(crate) starts: Vec<usize>,
}

impl Grouped {
    /// The postings of the term of rank `rank`.
    pub(crate) fn postings_of(&self, rank: usize) -> &[Posting] {
        let start = self.starts.get(rank).copied().unwrap_or(0);
        let end = self
            .starts
            .get(rank.saturating_add(1))
            .copied()
            .unwrap_or(start);
        self.postings.get(start..end).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(inv: &mut Inverted, doc: u32, terms: &[&str]) {
        let mut occ: Vec<(u32, u32)> = Vec::new();
        for (pos, t) in (0_u32..).zip(terms) {
            occ.push((inv.intern(t.as_bytes()).unwrap().0, pos));
        }
        inv.add_doc(doc, &mut occ, u32::try_from(terms.len()).unwrap())
            .unwrap();
    }

    #[test]
    fn groups_by_term_in_byte_order() {
        let mut inv = Inverted::new(true);
        add(&mut inv, 0, &["b", "a", "b"]);
        add(&mut inv, 1, &[]);
        add(&mut inv, 2, &["a", "c"]);
        assert_eq!(inv.lengths, [(0, 3), (2, 2)]);
        assert_eq!((inv.sum_len, inv.docs_with_field()), (5, 2));

        let g = inv.by_term();
        let (order, sorted, starts) = (&g.order, &g.postings, &g.starts);
        let terms: Vec<&[u8]> = order.iter().map(|&id| inv.terms.key(id)).collect();
        assert_eq!(terms, [b"a", b"b", b"c"]);
        assert_eq!(*starts, [0, 2, 3, 4]);
        assert_eq!(g.postings_of(1).len(), 1);
        let docs: Vec<(u32, u32)> = sorted.iter().map(|p| (p.doc, p.tf)).collect();
        assert_eq!(docs, [(0, 1), (2, 1), (0, 2), (2, 1)]);
        let positions: Vec<&[u32]> = sorted.iter().map(|p| inv.positions_of(p)).collect();
        assert_eq!(positions, [&[1][..], &[0], &[0, 2], &[1]]);
    }

    #[test]
    fn without_positions() {
        let mut inv = Inverted::new(false);
        add(&mut inv, 0, &["x", "x"]);
        let sorted = inv.by_term().postings;
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0].tf, 2);
        assert_eq!(inv.positions_of(&sorted[0]), [0_u32; 0]);
        assert!(inv.size_in_bytes() > 0);
    }
}
