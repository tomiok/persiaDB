//! Roaring bitmaps built from docids in increasing order, in bounded memory: a scan of a segment that claims many
//! documents must not buffer one entry per match (CLAUDE.md "Safety & errors").

use roaring::RoaringBitmap;

use crate::segment::{MAX_DOCS, ReadError, SegmentCorruption};

/// Docids `0..doc_count` as roaring runs: one container per 2^16 ids, not one entry per document.
///
/// # Errors
/// [`ReadError::Corrupt`] ([`SegmentCorruption::DocCount`]) when `doc_count` is above [`MAX_DOCS`] (`2^31`,
/// SPEC §5.4). The range is not built. A segment footer is rejected at open above that cap, and a memtable
/// stops at [`MAX_DOCS`]; this is the same check at the allocation.
pub(crate) fn doc_range(doc_count: u32) -> Result<RoaringBitmap, ReadError> {
    if doc_count > MAX_DOCS {
        return Err(ReadError::Corrupt {
            offset: 0,
            reason: SegmentCorruption::DocCount { count: doc_count },
        });
    }
    let mut all = RoaringBitmap::new();
    all.insert_range(0..doc_count);
    Ok(all)
}

/// Docids buffered before they are appended: appending a run is many times faster than inserting each, and a roaring
/// `push` into a bitmap container is slower still.
const CHUNK: usize = 4096;

/// A bitmap of docids added in increasing order.
#[derive(Debug, Default)]
pub(crate) struct Ascending {
    docs: RoaringBitmap,
    chunk: Vec<u32>,
}

impl Ascending {
    /// Adds `docid`, above every docid added before (one that is not is still added, more slowly).
    pub(crate) fn push(&mut self, docid: u32) {
        self.chunk.push(docid);
        if self.chunk.len() >= CHUNK {
            self.flush();
        }
    }

    /// The docids added.
    pub(crate) fn finish(mut self) -> RoaringBitmap {
        self.flush();
        self.docs
    }

    fn flush(&mut self) {
        if self.docs.append(self.chunk.iter().copied()).is_err() {
            // Out of order: what was appended before the first one out of place is inserted again, harmlessly.
            self.docs.extend(self.chunk.iter().copied());
        }
        self.chunk.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_every_docid_in_or_out_of_order() {
        let mut a = Ascending::default();
        let ids: Vec<u32> = (0..10_000).map(|i| i * 3).chain([7, 1, 29_997]).collect();
        for &d in &ids {
            a.push(d);
        }
        assert_eq!(a.finish(), ids.into_iter().collect::<RoaringBitmap>());
        assert!(Ascending::default().finish().is_empty());
    }

    /// A count above [`MAX_DOCS`] is an error. A legal full range is runs, not a slot per document.
    #[test]
    fn doc_range_caps_the_count_and_stores_runs() {
        use crate::segment::{MAX_DOCS, ReadError, SegmentCorruption};

        let over = MAX_DOCS.saturating_add(1);
        match doc_range(over) {
            Err(ReadError::Corrupt {
                reason: SegmentCorruption::DocCount { count },
                ..
            }) => assert_eq!(count, over),
            other => panic!("expected DocCount, got {other:?}"),
        }

        let n = 1_u32 << 20;
        let all = doc_range(n).unwrap();
        assert_eq!(all.len(), u64::from(n));
        assert!(all.serialized_size() < usize::try_from(n / 8).unwrap());
        assert!(doc_range(0).unwrap().is_empty());
    }
}
