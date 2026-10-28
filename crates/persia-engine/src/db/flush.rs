//! Memtable → segment bytes (SPEC §8.2 step 3, ROADMAP 6.3.1).

use persia_analysis::AnalyzerRegistry;

use crate::document::Document;
use crate::memtable::Memtable;
use crate::segment::{SegmentBuilder, SegmentError, SegmentMeta, SegmentWriter, WriteOptions};

/// A segment built from a memtable.
#[derive(Debug)]
pub(super) struct Flushed {
    pub(super) bytes: Vec<u8>,
    pub(super) doc_count: u32,
}

/// The segment of `memtable`'s live documents, in docid order (so in the order they were written); `None` when it
/// has none (every document was replaced or deleted, or it saw only deletes).
///
/// Each document is indexed again by the segment builder, with the same schema and analyzers as the memtable, so
/// it gives the terms, positions and norms the memtable had.
pub(super) fn segment(
    memtable: &Memtable,
    registry: &AnalyzerRegistry,
    meta: SegmentMeta,
    options: WriteOptions,
) -> Result<Option<Flushed>, SegmentError> {
    let schema = memtable.schema();
    let mut builder = SegmentBuilder::new(schema, registry)?;
    memtable.for_each_live(|bytes| {
        // The memtable encoded and validated it with this schema.
        let doc = Document::decode(bytes, schema).map_err(|_| SegmentError::Internal {
            what: "a memtable document does not decode",
        })?;
        builder.add(&doc).map(drop)
    })?;
    let doc_count = builder.doc_count();
    if doc_count == 0 {
        return Ok(None);
    }
    let bytes = SegmentWriter::new(&builder, meta)
        .with_options(options)
        .finish()?;
    Ok(Some(Flushed { bytes, doc_count }))
}
