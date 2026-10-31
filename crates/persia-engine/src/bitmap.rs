//! Roaring bitmaps built from docids in increasing order, in bounded memory: a scan of a segment that claims many
//! documents must not buffer one entry per match (CLAUDE.md "Safety & errors").

use roaring::RoaringBitmap;

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
}
