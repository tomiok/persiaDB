//! The memtable's column values for sorting and facets (ROADMAP 7.5.1, 7.5.3): a column field's keys, and a
//! `keyword` field's values as a dictionary in byte order with each document's ords, as a segment's `Columns` section
//! holds them (SPEC §5.3.5). Each is copied out of the memtable as of a view, under its read lock, once per call.

use std::sync::Arc;

use super::view::field_of;
use super::{Inner, MemField};
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
    /// The keys of the documents below `docs` in column field `field`.
    pub(super) fn new(inner: &Inner, field: FieldId, docs: u32) -> Result<Self, ReadError> {
        let (field_type, memfield) = field_of(inner, field)?;
        let MemField::Column { keys } = memfield else {
            return Err(ReadError::FieldType {
                field,
                field_type,
                what: "column",
            });
        };
        let end = keys.partition_point(|&(docid, _)| docid < docs);
        Ok(Self {
            keys: keys.get(..end).unwrap_or_default().into(),
            at: 0,
        })
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
}

impl MemtableKeywords {
    /// The values of the documents below `docs` in `keyword` field `field`, deleted ones included (as a segment's
    /// column holds them).
    pub(super) fn new(inner: &Inner, field: FieldId, docs: u32) -> Result<Self, ReadError> {
        let (field_type, memfield) = field_of(inner, field)?;
        let MemField::Keyword { index } = memfield else {
            return Err(ReadError::FieldType {
                field,
                field_type,
                what: "keyword column",
            });
        };
        from_inverted(&index.inverted, field, docs)
    }
}

/// The keyword column of `inverted` for documents `0..docs`.
fn from_inverted(
    inverted: &Inverted,
    field: FieldId,
    docs: u32,
) -> Result<MemtableKeywords, ReadError> {
    // Postings are in docid order: the view's are a prefix.
    let end = inverted.postings.partition_point(|p| p.doc < docs);
    let postings = inverted.postings.get(..end).unwrap_or_default();
    let mut used: Vec<u32> = postings.iter().map(|p| p.term).collect();
    used.sort_unstable();
    used.dedup();
    // A term id outside the arena has no bytes. Reject it before ordering, so it cannot sort as the empty key.
    for &term in &used {
        let in_range = usize::try_from(term).is_ok_and(|i| i < inverted.terms.len());
        if !in_range {
            return Err(missing_term(field, term));
        }
    }
    used.sort_unstable_by(|&a, &b| inverted.terms.key(a).cmp(inverted.terms.key(b)));
    // `None` is not ordinal 0: a term the dictionary does not hold must not sort or facet as the first value.
    // Ordinal `i` is `used[i]`, which is byte order.
    let mut ord_of = vec![None; inverted.terms.len()];
    for (ord, &term) in (0_u64..).zip(&used) {
        let Some(slot) = ord_of.get_mut(term as usize) else {
            return Err(missing_term(field, term));
        };
        *slot = Some(ord);
    }
    let values: Arc<[Box<[u8]>]> = used.iter().map(|&t| inverted.terms.key(t).into()).collect();
    let doc_count = docs as usize;
    let mut starts = Vec::with_capacity(doc_count.saturating_add(1));
    let mut ords = Vec::with_capacity(postings.len());
    let mut rest = postings;
    for d in 0..docs {
        starts.push(ords.len());
        let n = rest.partition_point(|p| p.doc == d);
        let (mine, after) = rest.split_at(n);
        let mut mapped = Vec::with_capacity(mine.len());
        for posting in mine {
            mapped.push(ord_at(&ord_of, posting.term, field)?);
        }
        mapped.sort_unstable();
        ords.extend(mapped);
        rest = after;
    }
    starts.push(ords.len());
    Ok(MemtableKeywords {
        values,
        starts: starts.into(),
        ords: ords.into(),
        docs,
        field,
    })
}

/// The ordinal of term id `term`, or [`missing_term`] when `ord_of` has no entry for it (including a missing slot,
/// which is not ordinal 0).
fn ord_at(ord_of: &[Option<u64>], term: u32, field: FieldId) -> Result<u64, ReadError> {
    let index = usize::try_from(term).unwrap_or(usize::MAX);
    ord_of
        .get(index)
        .copied()
        .flatten()
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
        let d = docid as usize;
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
        self.values.len() as u64
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::builder::inverted::Posting;

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
        // A slot that was never filled is not the first dictionary term.
        let missing = ord_at(&[None], 0, field).unwrap_err();
        assert_eq!(section_what(&missing), "a keyword term with no ordinal");
        let past = ord_at(&[], 0, field).unwrap_err();
        assert_eq!(section_what(&past), "a keyword term with no ordinal");
        // Ordinal 0 is a real ordinal when the slot says so.
        assert_eq!(ord_at(&[Some(0)], 0, field).unwrap(), 0);

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
