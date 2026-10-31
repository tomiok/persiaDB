//! The memtable's column values for sorting and facets (ROADMAP 7.5.1, 7.5.3): a column field's keys, and a
//! `keyword` field's values as a dictionary in byte order with each document's ords, as a segment's `Columns` section
//! holds them (SPEC §5.3.5). Each is copied out of the memtable as of a view, under its read lock, once per call.

use std::sync::Arc;

use super::view::field_of;
use super::{Inner, MemField};
use crate::schema::FieldId;
use crate::segment::ReadError;
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
    /// Document `d`'s ords are `ords[starts[d]..starts[d + 1]]`, ascending.
    starts: Arc<[usize]>,
    ords: Arc<[u64]>,
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
        let inverted = &index.inverted;
        // Postings are in docid order: the view's are a prefix.
        let end = inverted.postings.partition_point(|p| p.doc < docs);
        let postings = inverted.postings.get(..end).unwrap_or_default();
        let mut used: Vec<u32> = postings.iter().map(|p| p.term).collect();
        used.sort_unstable();
        used.dedup();
        used.sort_unstable_by(|&a, &b| inverted.terms.key(a).cmp(inverted.terms.key(b)));
        // Term id to ord; term ids are below the arena's length.
        let mut ord_of = vec![0_u64; inverted.terms.len()];
        for (ord, &term) in (0_u64..).zip(&used) {
            if let Some(slot) = ord_of.get_mut(term as usize) {
                *slot = ord;
            }
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
            let first = ords.len();
            ords.extend(
                mine.iter()
                    .map(|p| ord_of.get(p.term as usize).copied().unwrap_or(0)),
            );
            if let Some(list) = ords.get_mut(first..) {
                list.sort_unstable();
            }
            rest = after;
        }
        starts.push(ords.len());
        Ok(Self {
            values,
            starts: starts.into(),
            ords: ords.into(),
        })
    }
}

impl KeywordReader for MemtableKeywords {
    fn ords(&mut self, docid: u32) -> Result<&[u64], ReadError> {
        let d = docid as usize;
        let range = self
            .starts
            .get(d)
            .copied()
            .zip(self.starts.get(d.saturating_add(1)).copied());
        Ok(range
            .and_then(|(from, to)| self.ords.get(from..to))
            .unwrap_or_default())
    }

    fn value(&self, ord: u64) -> Option<&[u8]> {
        self.values.get(usize::try_from(ord).ok()?).map(|v| &**v)
    }

    fn len(&self) -> u64 {
        self.values.len() as u64
    }
}
