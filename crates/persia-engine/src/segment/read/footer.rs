//! The locator, footer and section table of a segment, and every layout rule SPEC §5.1 lists (ROADMAP 5.3.1).

use core::ops::Range;

use persia_format::Reader;
use persia_format::checksum::crc32c;

use super::super::kind::{SEGMENT_FIELD, SectionKind};
use super::super::write::{FOOTER_MAGIC, FORMAT_MAJOR, MAX_FOOTER_LEN};
use super::error::{ReadError, SegmentCorruption, UnsupportedSegment};
use crate::schema::{CollectionSchema, FieldType, SchemaHash};

/// Bytes of the locator: `footer_len u32` and the magic.
const LOCATOR_LEN: u64 = 8;
/// Smallest footer: the 88-byte fixed part, two 1-byte ids with their lengths, the CRC (SPEC §5.1).
const MIN_FOOTER_LEN: u32 = 98;
/// Sections and the footer start at multiples of this (SPEC §5.1).
const ALIGN: u64 = 8;
/// Bytes of a field-statistics entry and of a section-table entry.
const STATS_LEN: usize = 16;
const ENTRY_LEN: usize = 24;
/// Longest `min_id`/`max_id` (SPEC §5.1, the document id limit of §6.4.1).
const MAX_ID_LEN: usize = 512;
/// Largest `doc_count` (SPEC §5.4).
const MAX_DOC_COUNT: u32 = 1 << 31;

/// Statistics of one `text` or `keyword` field (SPEC §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldStats {
    /// Documents with at least one token (`text`) or value (`keyword`) in the field.
    pub docs_with_field: u32,
    /// Sum of the field lengths (SPEC §5.3.4) over the segment's documents.
    pub sum_field_len: u64,
}

/// A section table entry; `range` is relative to the segment start.
#[derive(Debug, Clone)]
pub(super) struct Section {
    pub(super) kind: SectionKind,
    pub(super) field: u32,
    pub(super) range: Range<u64>,
}

/// A parsed, validated footer.
#[derive(Debug, Clone)]
pub(super) struct Footer {
    pub(super) segment_id: u128,
    pub(super) rocompat_flags: u64,
    pub(super) format_minor: u16,
    pub(super) schema_field_count: u32,
    pub(super) doc_count: u32,
    pub(super) merge_generation: u32,
    pub(super) created_at: i64,
    /// `(field id, stats)`, ascending.
    pub(super) stats: Vec<(u32, FieldStats)>,
    /// In table order: ascending `(field, kind)`.
    pub(super) sections: Vec<Section>,
    pub(super) min_id: Vec<u8>,
    pub(super) max_id: Vec<u8>,
}

/// Parses the footer of `segment` (the whole segment, starting at absolute offset `base`) and checks every rule of
/// SPEC §5.1 against `schema`, padding included.
pub(super) fn parse(
    segment: &[u8],
    base: u64,
    schema: &CollectionSchema,
) -> Result<Footer, ReadError> {
    let (start, body) = locate(segment, base)?;
    let mut r = Reader::with_base_offset(body, base.saturating_add(start));
    let major_at = r.offset();
    let major = r.read_u16().map_err(fail)?;
    if major != FORMAT_MAJOR {
        return Err(ReadError::Unsupported {
            offset: major_at,
            reason: UnsupportedSegment::FormatMajor { major },
        });
    }
    let format_minor = r.read_u16().map_err(fail)?;
    r.expect_zeros(4).map_err(fail)?;
    let flags_at = r.offset();
    let incompat = r.read_u64().map_err(fail)?;
    if incompat != 0 {
        return Err(ReadError::Unsupported {
            offset: flags_at,
            reason: UnsupportedSegment::IncompatFlags { flags: incompat },
        });
    }
    let rocompat_flags = r.read_u64().map_err(fail)?;
    let segment_id = r.read_u128().map_err(fail)?;
    let schema_field_count = read_schema(&mut r, schema)?;
    let doc_count_at = r.offset();
    let doc_count = r.read_u32().map_err(fail)?;
    if !(1..=MAX_DOC_COUNT).contains(&doc_count) {
        return Err(ReadError::Corrupt {
            offset: doc_count_at,
            reason: SegmentCorruption::DocCount { count: doc_count },
        });
    }
    let merge_generation = r.read_u32().map_err(fail)?;
    let created_at = r.read_i64().map_err(fail)?;
    let stats_count = r.read_u32().map_err(fail)?;
    let section_count = r.read_u32().map_err(fail)?;
    let field_count = usize::try_from(schema_field_count).unwrap_or(usize::MAX);
    let stats = read_stats(&mut r, schema, field_count, stats_count, doc_count)?;
    let layout = Layout {
        segment,
        base,
        footer_start: start,
        schema,
        field_count: schema_field_count,
    };
    let sections = layout.read_sections(&mut r, section_count)?;
    let (min_id, max_id) = read_ids(&mut r)?;
    Ok(Footer {
        segment_id,
        rocompat_flags,
        format_minor,
        schema_field_count,
        doc_count,
        merge_generation,
        created_at,
        stats,
        sections,
        min_id,
        max_id,
    })
}

/// A [`Reader`] error inside the footer.
fn fail(e: persia_format::Error) -> ReadError {
    match e {
        persia_format::Error::Corrupt { offset, reason } => ReadError::Corrupt {
            offset,
            reason: reason.into(),
        },
        other => ReadError::Format(other),
    }
}

/// Checks the locator and the footer CRC; returns where the footer starts (relative to the segment) and its
/// bytes before the CRC.
fn locate(segment: &[u8], base: u64) -> Result<(u64, &[u8]), ReadError> {
    let len = u64::try_from(segment.len()).unwrap_or(u64::MAX);
    let corrupt = |pos: u64, reason| ReadError::Corrupt {
        offset: base.saturating_add(pos),
        reason,
    };
    let (Some(locator_at), Some(locator)) =
        (len.checked_sub(LOCATOR_LEN), segment.last_chunk::<8>())
    else {
        return Err(corrupt(0, SegmentCorruption::BadMagic));
    };
    let [l0, l1, l2, l3, m0, m1, m2, m3] = *locator;
    if [m0, m1, m2, m3] != FOOTER_MAGIC {
        return Err(corrupt(
            locator_at.saturating_add(4),
            SegmentCorruption::BadMagic,
        ));
    }
    let footer_len = u32::from_le_bytes([l0, l1, l2, l3]);
    let bad_len = || corrupt(locator_at, SegmentCorruption::FooterLen { len: footer_len });
    let start = Some(footer_len)
        .filter(|n| (MIN_FOOTER_LEN..=MAX_FOOTER_LEN).contains(n))
        .and_then(|n| locator_at.checked_sub(u64::from(n)))
        .filter(|s| s.is_multiple_of(ALIGN))
        .ok_or_else(bad_len)?;
    let (body, stored) = usize::try_from(start)
        .ok()
        .zip(usize::try_from(locator_at).ok())
        .and_then(|(s, e)| segment.get(s..e))
        .and_then(<[u8]>::split_last_chunk::<4>)
        .ok_or_else(bad_len)?;
    let stored = u32::from_le_bytes(*stored);
    let computed = crc32c(body);
    if stored != computed {
        return Err(corrupt(
            locator_at.saturating_sub(4),
            SegmentCorruption::FooterChecksum { stored, computed },
        ));
    }
    Ok((start, body))
}

/// The schema hash, its encoding version and field count, checked against `schema` (SPEC §6.2.5).
fn read_schema(r: &mut Reader<'_>, schema: &CollectionSchema) -> Result<u32, ReadError> {
    let hash_at = r.offset();
    let schema_hash = r.read_u128().map_err(fail)?;
    let version_at = r.offset();
    let version = r.read_u8().map_err(fail)?;
    if version != SchemaHash::ENCODING_VERSION {
        return Err(ReadError::Unsupported {
            offset: version_at,
            reason: UnsupportedSegment::SchemaEncoding { version },
        });
    }
    r.expect_zeros(3).map_err(fail)?;
    let count_at = r.offset();
    let schema_field_count = r.read_u32().map_err(fail)?;
    let field_count = usize::try_from(schema_field_count).unwrap_or(usize::MAX);
    let expected = schema.hash_at(field_count).ok_or(ReadError::Corrupt {
        offset: count_at,
        reason: SegmentCorruption::SchemaFieldCount {
            recorded: schema_field_count,
            current: schema.fields().len(),
        },
    })?;
    if expected.get() != schema_hash {
        return Err(ReadError::Corrupt {
            offset: hash_at,
            reason: SegmentCorruption::SchemaHash {
                recorded: schema_hash,
                expected: expected.get(),
            },
        });
    }
    Ok(schema_field_count)
}

/// The field statistics: exactly the `text` and `keyword` fields below the field count, ascending.
fn read_stats(
    r: &mut Reader<'_>,
    schema: &CollectionSchema,
    field_count: usize,
    stats_count: u32,
    doc_count: u32,
) -> Result<Vec<(u32, FieldStats)>, ReadError> {
    let fields = schema.fields().get(..field_count).unwrap_or_default();
    let mut expected = fields
        .iter()
        .filter(|f| matches!(f.field_type(), FieldType::Text | FieldType::Keyword))
        .map(|f| f.id().get());
    if usize::try_from(stats_count).ok() != Some(expected.clone().count()) {
        return Err(ReadError::Corrupt {
            offset: r.offset(),
            reason: SegmentCorruption::FieldStats { field: u32::MAX },
        });
    }
    let mut stats = Vec::with_capacity(r.remaining() / STATS_LEN);
    for _ in 0..stats_count {
        let entry_at = r.offset();
        let field = r.read_u32().map_err(fail)?;
        let docs_with_field = r.read_u32().map_err(fail)?;
        let sum_field_len = r.read_u64().map_err(fail)?;
        if expected.next() != Some(field) || docs_with_field > doc_count {
            return Err(ReadError::Corrupt {
                offset: entry_at,
                reason: SegmentCorruption::FieldStats { field },
            });
        }
        stats.push((
            field,
            FieldStats {
                docs_with_field,
                sum_field_len,
            },
        ));
    }
    Ok(stats)
}

/// `min_id` and `max_id`, the footer's last fields.
fn read_ids(r: &mut Reader<'_>) -> Result<(Vec<u8>, Vec<u8>), ReadError> {
    let ids_at = r.offset();
    let bad = ReadError::Corrupt {
        offset: ids_at,
        reason: SegmentCorruption::IdRange,
    };
    let mut id = || -> Result<Vec<u8>, ReadError> {
        let len = usize::from(r.read_u16().map_err(fail)?);
        if !(1..=MAX_ID_LEN).contains(&len) {
            return Err(bad.clone());
        }
        Ok(r.read_bytes(len).map_err(fail)?.to_vec())
    };
    let min_id = id()?;
    let max_id = id()?;
    if min_id > max_id {
        return Err(bad);
    }
    if !r.is_empty() {
        return Err(ReadError::Corrupt {
            offset: r.offset(),
            reason: SegmentCorruption::TrailingBytes,
        });
    }
    Ok((min_id, max_id))
}

/// What the section table is checked against.
struct Layout<'a> {
    segment: &'a [u8],
    base: u64,
    footer_start: u64,
    schema: &'a CollectionSchema,
    field_count: u32,
}

impl Layout<'_> {
    /// The section table (SPEC §5.1): known kinds under fields of the right type, strictly ascending, 8-aligned,
    /// packed with zero padding, ending before the footer.
    fn read_sections(&self, r: &mut Reader<'_>, count: u32) -> Result<Vec<Section>, ReadError> {
        let mut sections: Vec<Section> = Vec::with_capacity(r.remaining() / ENTRY_LEN);
        let mut end = 0_u64;
        for _ in 0..count {
            let entry_at = r.offset();
            let code = r.read_u16().map_err(fail)?;
            r.expect_zeros(2).map_err(fail)?;
            let field = r.read_u32().map_err(fail)?;
            let offset = r.read_u64().map_err(fail)?;
            let len = r.read_u64().map_err(fail)?;
            let entry_err = |reason| ReadError::Corrupt {
                offset: entry_at,
                reason,
            };
            let kind = SectionKind::from_code(code)
                .ok_or_else(|| entry_err(SegmentCorruption::UnknownSectionKind { kind: code }))?;
            if !kind_fits(kind, field, self.schema, self.field_count) {
                return Err(entry_err(SegmentCorruption::SectionField {
                    kind: code,
                    field,
                }));
            }
            if sections
                .last()
                .is_some_and(|prev| (prev.field, prev.kind) >= (field, kind))
            {
                return Err(entry_err(SegmentCorruption::SectionOrder));
            }
            let bounds = SegmentCorruption::SectionBounds { offset, len };
            let section_end = offset
                .checked_add(len)
                .filter(|&e| e <= self.footer_start)
                .ok_or_else(|| entry_err(bounds))?;
            if !offset.is_multiple_of(ALIGN) || offset < end || offset.wrapping_sub(end) >= ALIGN {
                return Err(entry_err(bounds));
            }
            check_padding(self.segment, end..offset, self.base)?;
            sections.push(Section {
                kind,
                field,
                range: offset..section_end,
            });
            end = section_end;
        }
        if self.footer_start.wrapping_sub(end) >= ALIGN {
            return Err(ReadError::Corrupt {
                offset: self.base.saturating_add(end),
                reason: SegmentCorruption::SectionBounds {
                    offset: end,
                    len: self.footer_start.wrapping_sub(end),
                },
            });
        }
        check_padding(self.segment, end..self.footer_start, self.base)?;
        Ok(sections)
    }
}

/// Whether a section of `kind` may sit under `field` (SPEC §5.3 "Framing").
fn kind_fits(kind: SectionKind, field: u32, schema: &CollectionSchema, field_count: u32) -> bool {
    use SectionKind as K;
    if kind.is_segment_level() || field == SEGMENT_FIELD {
        return kind.is_segment_level() && field == SEGMENT_FIELD;
    }
    if field >= field_count {
        return false;
    }
    let Some(f) = usize::try_from(field)
        .ok()
        .and_then(|i| schema.fields().get(i))
    else {
        return false;
    };
    match f.field_type() {
        FieldType::Text => matches!(
            kind,
            K::TermDict | K::TermInfo | K::Postings | K::Freqs | K::Skip | K::Positions | K::Norms
        ),
        FieldType::Keyword => matches!(
            kind,
            K::TermDict | K::TermInfo | K::Postings | K::Freqs | K::Skip | K::Columns
        ),
        FieldType::I64 | FieldType::F64 | FieldType::Bool | FieldType::Datetime => {
            kind == K::Columns
        }
        FieldType::Stored => false,
    }
}

/// Zero padding between sections and before the footer (SPEC §4.1, §5.1).
fn check_padding(segment: &[u8], range: Range<u64>, base: u64) -> Result<(), ReadError> {
    let bytes = usize::try_from(range.start)
        .ok()
        .zip(usize::try_from(range.end).ok())
        .and_then(|(s, e)| segment.get(s..e))
        .unwrap_or_default();
    match bytes.iter().position(|&b| b != 0) {
        None => Ok(()),
        Some(i) => Err(ReadError::Corrupt {
            offset: base
                .saturating_add(range.start)
                .saturating_add(u64::try_from(i).unwrap_or(0)),
            reason: SegmentCorruption::Format(persia_format::Corruption::NonZeroReserved {
                value: bytes.get(i).copied().unwrap_or(0),
            }),
        }),
    }
}
