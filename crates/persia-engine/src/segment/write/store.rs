//! `DocStore`, `IdMap`, `IdList` and `BlobRefs`: the segment-level sections (SPEC §5.3.6, ROADMAP 5.2.6), written
//! straight into the segment (ROADMAP 5.1.6).

use persia_format::Writer;
use persia_format::block::Compression;

use super::super::SegmentError;
use super::super::builder::SegmentBuilder;
use super::super::kind::{SEGMENT_FIELD, SectionKind};
use super::terms::fst_error;
use super::{Sections, Sink, WriteOptions};

/// The `DocStore` section: one row per document (SPEC §6.4.2).
pub(super) fn doc_store(
    b: &SegmentBuilder,
    out: &mut Sections,
    options: WriteOptions,
) -> Result<(), SegmentError> {
    record_array(
        out,
        SectionKind::DocStore,
        b.rows.ends.iter().copied(),
        &b.rows.bytes,
        options,
    )
}

/// The `IdList` section: each document's id.
pub(super) fn id_list(
    b: &SegmentBuilder,
    out: &mut Sections,
    options: WriteOptions,
) -> Result<(), SegmentError> {
    let (bytes, ends) = b.ids.records();
    record_array(
        out,
        SectionKind::IdList,
        ends.iter().copied(),
        bytes,
        options,
    )
}

/// The `IdMap` section: an FST from each id to its docid; `sorted` holds the docids in id order.
pub(super) fn id_map(
    b: &SegmentBuilder,
    sorted: &[u32],
    out: &mut Sections,
) -> Result<(), SegmentError> {
    out.fst(SectionKind::IdMap, SEGMENT_FIELD, |map| {
        for (i, &docid) in sorted.iter().enumerate() {
            map.insert(b.ids.key(docid), u64::from(docid))
                .map_err(|_| fst_error(i))?;
        }
        Ok(())
    })
}

/// The `BlobRefs` section, if any document has a blob: each document's `BlobRef` encoding (SPEC §9.1), empty for
/// a document without one.
pub(super) fn blob_refs(
    b: &SegmentBuilder,
    out: &mut Sections,
    options: WriteOptions,
) -> Result<(), SegmentError> {
    if b.blobs.is_empty() {
        return Ok(());
    }
    let mut blobs = b.blobs.iter().peekable();
    let mut end = 0;
    let ends = (0..b.doc_count()).map(|doc| {
        if let Some(&(_, blob_end)) = blobs.next_if(|&&(d, _)| d == doc) {
            end = blob_end;
        }
        end
    });
    record_array(out, SectionKind::BlobRefs, ends, &b.blob_bytes, options)
}

/// A record array (SPEC §5.3.6): the end of each record, relative to the first, then the records; zstd blocks.
fn record_array(
    out: &mut Sections,
    kind: SectionKind,
    ends: impl Iterator<Item = usize>,
    bytes: &[u8],
    options: WriteOptions,
) -> Result<(), SegmentError> {
    let compression = Compression::Zstd {
        level: options.zstd_level,
    };
    out.framed(kind, SEGMENT_FIELD, compression, options, |s| {
        let mut w = Writer::new();
        for end in ends {
            w.write_u64(
                u64::try_from(end).map_err(|_| SegmentError::TooLarge { what: "records" })?,
            );
            if w.len() >= 64 << 10 {
                s.put(w.as_slice())?;
                w.clear();
            }
        }
        s.put(w.as_slice())?;
        s.put(bytes)
    })
}
