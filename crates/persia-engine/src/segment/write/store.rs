//! `DocStore`, `IdMap`, `IdList` and `BlobRefs`: the segment-level sections (SPEC §5.3.6, ROADMAP 5.2.6).

use persia_format::Writer;
use persia_format::block::{BlockWriter, Compression};

use super::super::SegmentError;
use super::super::builder::SegmentBuilder;
use super::terms::fst_error;
use super::{WriteOptions, raw_section};

/// The `DocStore` section: one row per document (SPEC §6.4.2).
pub(super) fn doc_store(
    b: &SegmentBuilder,
    options: WriteOptions,
) -> Result<Vec<u8>, SegmentError> {
    record_array(&b.rows.ends, &b.rows.bytes, options)
}

/// The `IdList` section: each document's id.
pub(super) fn id_list(b: &SegmentBuilder, options: WriteOptions) -> Result<Vec<u8>, SegmentError> {
    let (bytes, ends) = b.ids.records();
    record_array(ends, bytes, options)
}

/// The `IdMap` section: an FST from each id to its docid; `sorted` holds the docids in id order.
pub(super) fn id_map(b: &SegmentBuilder, sorted: &[u32]) -> Result<Vec<u8>, SegmentError> {
    let mut map = fst::MapBuilder::memory();
    for (i, &docid) in sorted.iter().enumerate() {
        map.insert(b.ids.key(docid), u64::from(docid))
            .map_err(|_| fst_error(i))?;
    }
    let bytes = map.into_inner().map_err(|_| fst_error(b.ids.len()))?;
    Ok(raw_section(bytes))
}

/// The `BlobRefs` section, if any document has a blob: each document's `BlobRef` encoding (SPEC §9.1), empty for
/// a document without one.
pub(super) fn blob_refs(
    b: &SegmentBuilder,
    options: WriteOptions,
) -> Result<Option<Vec<u8>>, SegmentError> {
    if b.blobs.is_empty() {
        return Ok(None);
    }
    let mut ends = Vec::with_capacity(b.ids.len());
    let mut blobs = b.blobs.iter().peekable();
    let mut end = 0;
    for doc in 0..b.doc_count() {
        if let Some(&(_, blob_end)) = blobs.next_if(|&&(d, _)| d == doc) {
            end = blob_end;
        }
        ends.push(end);
    }
    record_array(&ends, &b.blob_bytes, options).map(Some)
}

/// A record array (SPEC §5.3.6): the end of each record, relative to the first, then the records; zstd blocks.
fn record_array(
    ends: &[usize],
    bytes: &[u8],
    options: WriteOptions,
) -> Result<Vec<u8>, SegmentError> {
    let mut out = BlockWriter::new(
        options.block_size,
        Compression::Zstd {
            level: options.zstd_level,
        },
    )?;
    let mut w = Writer::new();
    for &end in ends {
        w.write_u64(u64::try_from(end).map_err(|_| SegmentError::TooLarge { what: "records" })?);
    }
    out.write(w.as_slice())?;
    out.write(bytes)?;
    Ok(out.finish()?)
}
