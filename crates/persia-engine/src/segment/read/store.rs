//! Record arrays (SPEC §5.3.6): `DocStore` rows, `IdList` ids and `BlobRefs`, read through the segment's block cache
//! (ROADMAP 5.3.6, 5.3.11).

use super::error::{ReadError, SegmentCorruption};
use super::stream::Stream;

/// Checks the shape of a record array of `count` records (SPEC §5.3.6): the stream holds the ends and the last end
/// is the stream's end. The ends in between are checked as records are read.
pub(super) fn check_records(stream: &Stream<'_>, count: u32) -> Result<(), ReadError> {
    let header = u64::from(count).saturating_mul(8);
    let data_len = stream.len().checked_sub(header).ok_or_else(|| {
        stream.corrupt(
            0,
            SegmentCorruption::Invalid {
                what: "a record array shorter than its ends",
            },
        )
    })?;
    let last_at = header.saturating_sub(8);
    if stream.u64_at(last_at)? != data_len {
        return Err(stream.corrupt(
            last_at,
            SegmentCorruption::Invalid {
                what: "a last record end other than the data length",
            },
        ));
    }
    Ok(())
}

/// Record `docid` of a record array of `count` records. One longer than `max` bytes (the longest valid record of the
/// section) is corruption, reported before anything is allocated.
pub(super) fn record(
    stream: &Stream<'_>,
    count: u32,
    docid: u32,
    max: u64,
) -> Result<Vec<u8>, ReadError> {
    let header = u64::from(count).saturating_mul(8);
    let end_at = u64::from(docid).saturating_mul(8);
    let end = stream.u64_at(end_at)?;
    let start = match end_at.checked_sub(8) {
        Some(at) => stream.u64_at(at)?,
        None => 0,
    };
    let data_len = stream.len().saturating_sub(header);
    if start > end || end > data_len {
        return Err(stream.corrupt(
            end_at,
            SegmentCorruption::Invalid {
                what: "record ends that decrease or pass the data",
            },
        ));
    }
    let len = end.saturating_sub(start);
    if len > max {
        return Err(stream.corrupt(
            end_at,
            SegmentCorruption::Invalid {
                what: "a record longer than any valid one",
            },
        ));
    }
    let len_usize = usize::try_from(len).unwrap_or(usize::MAX);
    let mut out = Vec::new();
    out.try_reserve_exact(len_usize)
        .map_err(|_| stream.error(persia_format::Error::OutOfMemory { bytes: Some(len) }))?;
    out.resize(len_usize, 0);
    stream.read_into(header.saturating_add(start), &mut out)?;
    Ok(out)
}
