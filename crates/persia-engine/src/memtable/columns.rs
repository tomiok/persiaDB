//! The memtable's column values for sorting and facets (ROADMAP 7.5.1, 7.5.3): a column field's keys, and a
//! `keyword` field's values as a dictionary in byte order with each document's ords, as a segment's `Columns` section
//! holds them (SPEC §5.3.5). Each is copied out of the memtable as of a view once per view (or once for the views of the
//! same documents, [`super::cache`]), in chunks under its read
//! lock ([`super::view`], ROADMAP 7.6.6), and built here without it.

use std::sync::Arc;

use persia_format::widen::{u64_from_usize, usize_from_u32};

use crate::schema::FieldId;
use crate::segment::builder::inverted::Inverted;
use crate::segment::{ReadError, SectionKind, SegmentCorruption};
use crate::view::{KeyReader, KeywordReader};

/// A column field's keys as of a view, from [`crate::view::SearchView::keys`].
#[derive(Debug, Clone)]
pub struct MemtableKeys {
    /// `(docid, key)` in docid order, one per document with a value.
    keys: Arc<[(u32, u64)]>,
    /// Where the last lookup landed: docids read in ascending order find theirs by stepping from it.
    at: usize,
}

impl MemtableKeys {
    /// The keys copied out of a column field, `(docid, key)` in docid order.
    pub(super) fn new(keys: Vec<(u32, u64)>) -> Self {
        Self {
            keys: keys.into(),
            at: 0,
        }
    }

    /// The keys, `(docid, key)` in docid order.
    pub(super) fn entries(&self) -> &[(u32, u64)] {
        &self.keys
    }

    /// Heap bytes of the copy.
    pub(crate) fn size_in_bytes(&self) -> usize {
        self.keys.len().saturating_mul(size_of::<(u32, u64)>())
    }

    /// Whether `other` reads the same copy (a clone of this one).
    #[cfg(test)]
    pub(crate) fn same_copy(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.keys, &other.keys)
    }
}

impl KeyReader for MemtableKeys {
    fn key(&mut self, docid: u32) -> Result<Option<u64>, ReadError> {
        let keys = &*self.keys;
        // A sort reads ascending docids: the next one is at or just after the last.
        let from = match keys.get(self.at) {
            Some(&(d, _)) if d <= docid => self.at,
            _ => 0,
        };
        let rest = keys.get(from..).unwrap_or_default();
        let i = from.saturating_add(rest.partition_point(|&(d, _)| d < docid));
        self.at = i;
        Ok(keys.get(i).filter(|&&(d, _)| d == docid).map(|&(_, k)| k))
    }
}

/// A `keyword` field's values as of a view, from [`crate::view::SearchView::keywords`].
#[derive(Debug, Clone)]
pub struct MemtableKeywords {
    /// The distinct values the documents hold, in byte order: ord `i` is `values[i]`.
    values: Arc<[Box<[u8]>]>,
    /// Document `d`'s ords are `ords[starts[d]..starts[d + 1]]`, ascending. `starts` has `docs + 1` entries.
    starts: Arc<[usize]>,
    ords: Arc<[u64]>,
    /// Documents `0..docs` are in the view. A docid at or above this is [`ReadError::DocOutOfRange`].
    docs: u32,
    /// The field, for a corruption error.
    field: FieldId,
    /// Heap bytes of the copy, counted once when it is built: the copy is never changed after, and the
    /// memtable reads [`Self::size_in_bytes`] on every write while it is cached.
    bytes: usize,
}

/// Postings `[from, from + n)` of a `keyword` field's `inverted` that are in a view of `docs` documents, as
/// `(docid, term id)`: postings are in docid order, so the view's are a prefix, which later writes do not change.
pub(super) fn posting_chunk(
    inverted: &Inverted,
    docs: u32,
    from: usize,
    n: usize,
) -> Vec<(u32, u32)> {
    inverted
        .postings
        .get(from..)
        .unwrap_or_default()
        .iter()
        .take(n)
        .take_while(|p| p.doc < docs)
        .map(|p| (p.doc, p.term))
        .collect()
}

/// The bytes of term ids `ids` of `inverted`. A term id outside the arena has no bytes, and is
/// [`missing_term`], so it cannot sort as the empty key. A term with a document in a view keeps its id
/// (a failed `put` truncates only the ids it interned), so ids read in several chunks name the same terms.
pub(super) fn term_bytes(
    inverted: &Inverted,
    field: FieldId,
    ids: &[u32],
) -> Result<Vec<Box<[u8]>>, ReadError> {
    ids.iter()
        .map(|&term| {
            let in_range = usize::try_from(term).is_ok_and(|i| i < inverted.terms.len());
            if !in_range {
                return Err(missing_term(field, term));
            }
            Ok(inverted.terms.key(term).into())
        })
        .collect()
}

/// The distinct term ids of `postings`, ascending: the ids whose bytes [`term_bytes`] copies.
pub(super) fn used_terms(postings: &[(u32, u32)]) -> Vec<u32> {
    let mut used: Vec<u32> = postings.iter().map(|&(_, term)| term).collect();
    used.sort_unstable();
    used.dedup();
    used
}

/// The keyword column of documents `0..docs` of `field`: their `postings` (`(docid, term id)`, in docid order),
/// the [`used_terms`] of them and each one's bytes, in that order.
pub(super) fn keywords(
    field: FieldId,
    docs: u32,
    postings: &[(u32, u32)],
    used: &[u32],
    bytes: Vec<Box<[u8]>>,
) -> Result<MemtableKeywords, ReadError> {
    if bytes.len() != used.len() {
        return Err(missing_term(
            field,
            used.get(bytes.len()).copied().unwrap_or(0),
        ));
    }
    // Ordinal `o` is the `o`-th used term in byte order; `ords[i]` is the ordinal of `used[i]`.
    let mut by_bytes: Vec<usize> = (0..used.len()).collect();
    by_bytes.sort_unstable_by(|&a, &b| bytes.get(a).cmp(&bytes.get(b)));
    let mut ords = vec![0_u64; used.len()];
    for (ord, &i) in (0_u64..).zip(&by_bytes) {
        if let Some(slot) = ords.get_mut(i) {
            *slot = ord;
        }
    }
    let mut bytes: Vec<Option<Box<[u8]>>> = bytes.into_iter().map(Some).collect();
    let values: Arc<[Box<[u8]>]> = by_bytes
        .iter()
        .map(|&i| bytes.get_mut(i).and_then(Option::take).unwrap_or_default())
        .collect();
    let doc_count = usize_from_u32(docs);
    let mut starts = Vec::with_capacity(doc_count.saturating_add(1));
    let mut doc_ords = Vec::with_capacity(postings.len());
    let mut rest = postings;
    for d in 0..docs {
        starts.push(doc_ords.len());
        let n = rest.partition_point(|&(doc, _)| doc == d);
        let (mine, after) = rest.split_at(n);
        let mut mapped = Vec::with_capacity(mine.len());
        for &(_, term) in mine {
            mapped.push(ord_at(used, &ords, term, field)?);
        }
        mapped.sort_unstable();
        doc_ords.extend(mapped);
        rest = after;
    }
    starts.push(doc_ords.len());
    let mut column = MemtableKeywords {
        values,
        starts: starts.into(),
        ords: doc_ords.into(),
        docs,
        field,
        bytes: 0,
    };
    column.bytes = column.count_bytes();
    Ok(column)
}

/// The ordinal of term id `term`, `ords[i]` for `used[i] == term`, or [`missing_term`] when `used` does not hold it:
/// a term with no ordinal is not ordinal 0.
fn ord_at(used: &[u32], ords: &[u64], term: u32, field: FieldId) -> Result<u64, ReadError> {
    used.binary_search(&term)
        .ok()
        .and_then(|i| ords.get(i).copied())
        .ok_or_else(|| missing_term(field, term))
}

/// A posting's term id is outside the arena, or was never given an ordinal.
fn missing_term(field: FieldId, term: u32) -> ReadError {
    ReadError::Section {
        kind: SectionKind::Columns,
        field: field.get(),
        position: u64::from(term),
        reason: SegmentCorruption::Invalid {
            what: "a keyword term with no ordinal",
        },
    }
}

impl KeywordReader for MemtableKeywords {
    fn ords(&mut self, docid: u32) -> Result<&[u64], ReadError> {
        // An out-of-range docid is not a document without values: an empty slice would sort and facet as missing.
        if docid >= self.docs {
            return Err(ReadError::DocOutOfRange {
                docid,
                doc_count: self.docs,
            });
        }
        let d = usize_from_u32(docid);
        let Some((from, to)) = self
            .starts
            .get(d)
            .copied()
            .zip(self.starts.get(d.saturating_add(1)).copied())
        else {
            return Err(self.broken(docid));
        };
        self.ords.get(from..to).ok_or_else(|| self.broken(docid))
    }

    fn value(&self, ord: u64) -> Option<&[u8]> {
        self.values.get(usize::try_from(ord).ok()?).map(|v| &**v)
    }

    fn len(&self) -> u64 {
        u64_from_usize(self.values.len())
    }
}

impl MemtableKeywords {
    /// `starts` or `ords` do not describe `docid`, which is below `docs`. The column was built here, so this is an
    /// invariant break and still not an absent value.
    fn broken(&self, docid: u32) -> ReadError {
        ReadError::Section {
            kind: SectionKind::Columns,
            field: self.field.get(),
            position: u64::from(docid),
            reason: SegmentCorruption::Invalid {
                what: "keyword ords out of range for the document",
            },
        }
    }

    /// Documents `0..docs` are in the column.
    pub(crate) fn docs(&self) -> u32 {
        self.docs
    }

    /// Heap bytes of the copy: the values, their boxes, and the document ords. Counted when it was built.
    pub(crate) fn size_in_bytes(&self) -> usize {
        self.bytes
    }

    /// Walks the copy for [`Self::size_in_bytes`]: O(distinct values).
    pub(super) fn count_bytes(&self) -> usize {
        let values = self
            .values
            .iter()
            .fold(0_usize, |sum, v| sum.saturating_add(v.len()));
        values
            .saturating_add(self.values.len().saturating_mul(size_of::<Box<[u8]>>()))
            .saturating_add(self.starts.len().saturating_mul(size_of::<usize>()))
            .saturating_add(self.ords.len().saturating_mul(size_of::<u64>()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::builder::inverted::Posting;

    /// The keyword column of `inverted` for documents `0..docs`, copied in one piece.
    fn from_inverted(
        inverted: &Inverted,
        field: FieldId,
        docs: u32,
    ) -> Result<MemtableKeywords, ReadError> {
        let postings = posting_chunk(inverted, docs, 0, usize::MAX);
        let used = used_terms(&postings);
        let bytes = term_bytes(inverted, field, &used)?;
        keywords(field, docs, &postings, &used, bytes)
    }

    fn section_what(err: &ReadError) -> &'static str {
        match err {
            ReadError::Section {
                reason: SegmentCorruption::Invalid { what },
                ..
            } => what,
            other => panic!("expected a section error, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_term_ordinal_is_not_ordinal_zero() {
        let field = FieldId::new(3);
        // A term `used` does not hold is not the first dictionary term.
        let missing = ord_at(&[1], &[0], 0, field).unwrap_err();
        assert_eq!(section_what(&missing), "a keyword term with no ordinal");
        let past = ord_at(&[], &[], 0, field).unwrap_err();
        assert_eq!(section_what(&past), "a keyword term with no ordinal");
        // Ordinal 0 is a real ordinal when `used` holds the term.
        assert_eq!(ord_at(&[0], &[0], 0, field).unwrap(), 0);
        // Bytes missing for a used term: no ordinal either.
        let short = keywords(field, 1, &[(0, 0)], &[0], Vec::new()).unwrap_err();
        assert_eq!(section_what(&short), "a keyword term with no ordinal");

        let mut inverted = Inverted::new(false);
        inverted.postings.push(Posting {
            term: 0,
            doc: 0,
            tf: 1,
            pos_start: 0,
        });
        let err = from_inverted(&inverted, field, 1).unwrap_err();
        assert_eq!(section_what(&err), "a keyword term with no ordinal");

        let (id, _) = inverted.intern(b"zeta").unwrap();
        inverted.postings.clear();
        inverted.postings.push(Posting {
            term: id,
            doc: 0,
            tf: 1,
            pos_start: 0,
        });
        let mut words = from_inverted(&inverted, field, 2).unwrap();
        assert_eq!(words.value(0), Some(&b"zeta"[..]));
        assert_eq!(words.ords(0).unwrap(), &[0]);
        // Document 1 has no value: empty, not an error.
        assert_eq!(words.ords(1).unwrap(), &[] as &[u64]);
        assert!(matches!(
            words.ords(2),
            Err(ReadError::DocOutOfRange {
                docid: 2,
                doc_count: 2
            })
        ));
    }
}
