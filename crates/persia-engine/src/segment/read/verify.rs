//! [`Segment::verify`] (ROADMAP 5.4.1): reads a whole segment and checks the rules that no single lookup can, because
//! they span a section, a field or the segment (SPEC §5.1, §5.3).

use core::ops::Bound;

use persia_format::bitpack::BLOCK_LEN;

use super::super::kind::{SEGMENT_FIELD, SectionKind};
use super::super::norms;
use super::columns::{BoolColumn, KeywordColumn, Norms, NumericColumn};
use super::terms::{SKIP_ENTRY_LEN, TERM_INFO_LEN, TermIndex, TermInfo};
use super::{ReadError, Segment, SegmentCorruption, raw_payload};
use crate::document::{MAX_DATETIME, MIN_DATETIME, Value};
use crate::schema::{Field, FieldType};

/// A column field's doc values, decoded with the writers' rules checked too.
enum Column {
    Numeric(NumericColumn),
    Bool(BoolColumn),
    Keyword(KeywordColumn),
}

/// What a term's posting list holds, to compare with its `TermInfo`.
struct ListStats {
    df: u32,
    ttf: u64,
    max_tf: u32,
    min_norm: u8,
}

impl Segment {
    /// Reads the whole segment and checks every rule of SPEC §5.1 and §5.3, including those lookups leave out
    /// because they span more than what a lookup reads:
    ///
    /// - the CRC of every block and raw section (an open segment's bytes may have changed since, e.g. when mapped);
    /// - per term: `df`, `ttf`, `max_tf` and `min_norm` equal to what its list holds, each `Skip` entry's bounds
    ///   equal to its block's, and the `skip_off` chain: each term's entries start where the previous term's end,
    ///   and the last ones end the section, which exists only when some term has `df ≥ 128`;
    /// - the minimal bit width of every bit-packed group and FOR block (and the FOR blocks' true minimum);
    /// - per field: the norms and the field statistics against the term frequencies, a keyword column against
    ///   the term dictionary and postings, and stored values against the columns;
    /// - per document: the `IdList`, `IdMap`, `BlobRefs` and `DocStore` agree, a row holds only stored fields of
    ///   the segment's schema, and `min_id`/`max_id` are the smallest and largest id;
    /// - sections that hold nothing (SPEC §5.3 "Presence");
    /// - column values a document can hold: no NaN, infinite or `-0.0` `f64`, no `datetime` out of range.
    ///
    /// It does not analyze stored `text` values again to compare them with the postings.
    ///
    /// It decodes every block once or more, and holds 4 bytes per document for one `text`/`keyword` field at a time,
    /// plus the decoded columns of every column field (8 bytes per document each, and a keyword column's values and
    /// ords). The document count is bounded by the input (open checked that the record arrays hold 8 bytes per
    /// document), so memory is linear in the segment size times the number of column fields.
    ///
    /// ```
    /// # use persia_analysis::AnalyzerRegistry;
    /// # use persia_engine::document::{Document, Value};
    /// # use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
    /// # use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
    /// # use persia_format::ByteSource;
    /// # let schema = SchemaBuilder::new("notes").field("body", FieldKind::Text(TextOptions::default())).build()?;
    /// # let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
    /// # let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
    /// # builder.add(&Document::new("n1").with(body, Value::String("Hello, world".into())))?;
    /// # let meta = SegmentMeta { segment_id: 7, created_at: 0, merge_generation: 0 };
    /// let bytes = SegmentWriter::new(&builder, meta).finish()?;
    /// let segment = Segment::open(ByteSource::new(bytes.into()), &schema)?;
    /// segment.verify()?; // what a writer produces always passes
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// A corruption ([`ReadError::is_corruption`]) for the first rule found broken; a [`ReadError::Block`] that is
    /// not one when memory runs out.
    pub fn verify(&self) -> Result<(), ReadError> {
        self.verify_checksums()?;
        let count = usize::try_from(self.footer.schema_field_count).unwrap_or(usize::MAX);
        let fields = self.schema.fields().get(..count).unwrap_or_default();
        let mut columns = Vec::new();
        for f in fields {
            if let Some(column) = self.verified_column(f)? {
                columns.push((f, column));
            }
        }
        self.verify_documents(&columns)?;
        for f in fields {
            let keyword = columns.iter().find_map(|(c, col)| match col {
                Column::Keyword(k) if c.id() == f.id() => Some(k),
                _ => None,
            });
            if matches!(f.field_type(), FieldType::Text | FieldType::Keyword) {
                self.verify_terms(f, keyword)?;
            }
        }
        Ok(())
    }

    /// Validates every block table again in full and checks it against the one open kept for lookups (they differ
    /// if the bytes changed since, ROADMAP 5.3.10), decodes every block (each checks its CRC), and recomputes every
    /// raw section's CRC.
    fn verify_checksums(&self) -> Result<(), ReadError> {
        let mut buf = Vec::new();
        for section in &self.footer.sections {
            if section.kind.is_block_framed() {
                // The whole table again (its CRC and every block end), not what open kept: the bytes may have
                // changed since (a mapped file), and lookups reopen the table without checking it (ROADMAP 5.3.10).
                let stream = self.open_stream(section)?;
                if Some(stream.validated()) != section.table {
                    return Err(invalid(
                        section.kind,
                        section.field,
                        0,
                        "a block table other than the one validated when the segment was opened",
                    ));
                }
                for i in 0..stream.block_count() {
                    stream.read_block(i, &mut buf)?;
                }
            } else {
                raw_payload(self.source.as_slice(), section)?;
            }
        }
        Ok(())
    }

    /// The column of `field`, if it is a column field; a section must hold at least one value (SPEC §5.3).
    fn verified_column(&self, field: &Field) -> Result<Option<Column>, ReadError> {
        let id = field.id().get();
        let stream = self.stream(SectionKind::Columns, id)?;
        let present = stream.is_some();
        let n = self.doc_count();
        let (column, any) = match field.field_type() {
            ty @ (FieldType::I64 | FieldType::F64 | FieldType::Datetime) => {
                let c = NumericColumn::read(stream, ty, n, true)?;
                let any = (0..n).any(|d| c.has_value(d));
                if (0..n).any(|d| c.get(d).is_some_and(|v| !in_range(&v))) {
                    return Err(invalid(
                        SectionKind::Columns,
                        id,
                        // The section has no single position for a document's key (FOR blocks are bit-packed).
                        0,
                        "a column value no document can hold",
                    ));
                }
                (Column::Numeric(c), any)
            }
            FieldType::Bool => {
                let c = BoolColumn::read(stream, n)?;
                let any = (0..n).any(|d| c.get(d).is_some());
                (Column::Bool(c), any)
            }
            FieldType::Keyword => {
                let c = KeywordColumn::read(stream, n, true)?;
                let any = (0..n).any(|d| !c.ords(d).is_empty());
                (Column::Keyword(c), any)
            }
            FieldType::Text | FieldType::Stored => return Ok(None),
        };
        if present && !any {
            return Err(self.presence_error(SectionKind::Columns, id, true));
        }
        Ok(Some(column))
    }

    /// The `DocStore`, `IdList`, `IdMap` and `BlobRefs` agree document by document, stored values equal the
    /// columns, and the footer's id range is the ids'.
    fn verify_documents(&self, columns: &[(&Field, Column)]) -> Result<(), ReadError> {
        let mut range: Option<(String, String)> = None;
        let mut any_blob = false;
        for d in 0..self.doc_count() {
            let doc = self.document(d)?;
            let id = self.id_of(d)?;
            let position = u64::from(d).saturating_mul(8);
            if doc.id() != id {
                return Err(invalid(
                    SectionKind::IdList,
                    SEGMENT_FIELD,
                    position,
                    "an id other than its DocStore row's",
                ));
            }
            if self.docid_of(&id)? != Some(d) {
                return Err(invalid(
                    SectionKind::IdMap,
                    SEGMENT_FIELD,
                    0,
                    "an id mapped to a docid other than its IdList position",
                ));
            }
            let blob = self.blob_ref(d)?;
            if blob.as_ref() != doc.blob_ref() {
                return Err(invalid(
                    SectionKind::BlobRefs,
                    SEGMENT_FIELD,
                    position,
                    "a blob reference other than its DocStore row's",
                ));
            }
            any_blob |= blob.is_some();
            // A field added after the segment was written has no value in it (SPEC §6.2.5).
            let unstored = doc.fields().any(|(f, _)| {
                f.get() >= self.footer.schema_field_count
                    || self
                        .schema
                        .field_by_id(f)
                        .is_none_or(|f| !f.kind().stored())
            });
            if unstored {
                return Err(invalid(
                    SectionKind::DocStore,
                    SEGMENT_FIELD,
                    position,
                    "a row with a field that is not stored",
                ));
            }
            for (field, column) in columns {
                if field.kind().stored() && !stored_matches(doc.get(field.id()), column, d) {
                    return Err(invalid(
                        SectionKind::Columns,
                        field.id().get(),
                        0,
                        "a column value other than the document's stored one",
                    ));
                }
            }
            range = Some(match range {
                None => (id.clone(), id),
                Some((lo, hi)) => (lo.min(id.clone()), hi.max(id)),
            });
        }
        if !any_blob && self.section(SectionKind::BlobRefs, SEGMENT_FIELD).is_some() {
            return Err(self.presence_error(SectionKind::BlobRefs, SEGMENT_FIELD, true));
        }
        let (min, max) = self.id_range();
        if range.is_none_or(|(lo, hi)| lo.as_bytes() != min || hi.as_bytes() != max) {
            return Err(ReadError::Corrupt {
                offset: self.source.base_offset(),
                reason: SegmentCorruption::Invalid {
                    what: "a min_id or max_id other than the smallest or largest document id",
                },
            });
        }
        Ok(())
    }

    /// Every term of a `text` or `keyword` field against its list, then the field against its terms.
    fn verify_terms(
        &self,
        field: &Field,
        keyword: Option<&KeywordColumn>,
    ) -> Result<(), ReadError> {
        let id = field.id();
        let fid = id.get();
        let index = self.terms(id)?;
        let norms = match field.field_type() {
            FieldType::Text => Some(self.norms(id)?),
            _ => None,
        };
        let stats = self.field_stats(id).unwrap_or(super::FieldStats {
            docs_with_field: 0,
            sum_field_len: 0,
        });
        let stats_error = || ReadError::Corrupt {
            offset: self.source.base_offset(),
            reason: SegmentCorruption::FieldStats { field: fid },
        };
        if index.is_empty() {
            // Open checked that the field has no sections exactly when `docs_with_field` is 0.
            return if stats.sum_field_len == 0 {
                Ok(())
            } else {
                Err(stats_error())
            };
        }
        let n = usize::try_from(self.doc_count()).unwrap_or(usize::MAX);
        let mut lens: Vec<u32> = Vec::new();
        lens.try_reserve_exact(n).map_err(|_| ReadError::Block {
            kind: SectionKind::Postings,
            field: fid,
            source: persia_format::Error::OutOfMemory {
                bytes: u64::try_from(n).ok().and_then(|n| n.checked_mul(4)),
            },
        })?;
        lens.resize(n, 0);

        let Totals {
            skip_end,
            sum_len,
            postings,
        } = walk_terms(&index, norms.as_ref(), keyword, &mut lens)?;
        if let Some(skip) = self.stream(SectionKind::Skip, fid)? {
            if skip_end == 0 {
                return Err(self.presence_error(SectionKind::Skip, fid, true));
            }
            if skip.len() != skip_end {
                return Err(invalid(
                    SectionKind::Skip,
                    fid,
                    skip_end,
                    "bytes after the last term's Skip entries",
                ));
            }
        }
        if let Some(col) = keyword {
            let ords = (0..self.doc_count()).fold(0_u64, |s, d| {
                s.saturating_add(u64::try_from(col.ords(d).len()).unwrap_or(u64::MAX))
            });
            if col.len() != index.len() || ords != postings {
                return Err(invalid(
                    SectionKind::Columns,
                    fid,
                    0,
                    "keyword ords other than the field's postings",
                ));
            }
        }
        let docs_with_field = lens.iter().filter(|&&l| l > 0).count();
        if u64::try_from(docs_with_field).ok() != Some(u64::from(stats.docs_with_field))
            || sum_len != stats.sum_field_len
        {
            return Err(stats_error());
        }
        if let Some(norms) = &norms {
            let docids = 0..self.doc_count();
            if let Some((d, _)) = docids
                .zip(&lens)
                .find(|&(d, &len)| norms.get(d) != norms::encode(len))
            {
                return Err(invalid(
                    SectionKind::Norms,
                    fid,
                    u64::from(d),
                    "a norm other than its document's field length",
                ));
            }
        }
        Ok(())
    }
}

/// What the terms of a field add up to.
struct Totals {
    /// Where the last Skip entries end.
    skip_end: u64,
    /// Sum of the frequencies: the sum of the field lengths.
    sum_len: u64,
    /// Sum of the dfs.
    postings: u64,
}

/// Checks every term of `index` against its list and the `skip_off` chain, adding the frequencies to `lens`.
fn walk_terms(
    index: &TermIndex<'_>,
    norms: Option<&Norms>,
    keyword: Option<&KeywordColumn>,
    lens: &mut [u32],
) -> Result<Totals, ReadError> {
    let fid = index.field();
    let mut t = Totals {
        skip_end: 0,
        sum_len: 0,
        postings: 0,
    };
    let mut terms = index.range(Bound::Unbounded, Bound::Unbounded);
    while let Some(next) = terms.next() {
        let (term, info) = next?;
        let ordinal = info.ordinal();
        if let Some(col) = keyword
            && col.value(ordinal) != Some(term)
        {
            return Err(invalid(
                SectionKind::Columns,
                fid,
                0,
                "a keyword dictionary other than the field's terms",
            ));
        }
        if let Some(off) = info.skip {
            if off != t.skip_end {
                return Err(invalid(
                    SectionKind::Skip,
                    fid,
                    off,
                    "a skip_off other than where the previous term's entries end",
                ));
            }
            t.skip_end = entries_end(off, info.df());
        }
        let list = verify_list(index, &info, norms, keyword, lens)?;
        if (list.df, list.ttf, list.max_tf, list.min_norm)
            != (info.df(), info.ttf(), info.max_tf(), info.min_norm())
        {
            return Err(invalid(
                SectionKind::TermInfo,
                fid,
                ordinal.saturating_mul(TERM_INFO_LEN),
                "a df, ttf, max_tf or min_norm other than its list's",
            ));
        }
        t.sum_len = t.sum_len.saturating_add(list.ttf);
        t.postings = t.postings.saturating_add(u64::from(list.df));
    }
    Ok(t)
}

/// Walks one posting list with every check on, adding each frequency to its document's field length.
fn verify_list(
    index: &TermIndex<'_>,
    info: &TermInfo,
    norms: Option<&Norms>,
    keyword: Option<&KeywordColumn>,
    lens: &mut [u32],
) -> Result<ListStats, ReadError> {
    let fid = index.field();
    let mut p = index.postings(info)?.strict();
    let block_len = u32::try_from(BLOCK_LEN).unwrap_or(128);
    let mut list = ListStats {
        df: 0,
        ttf: 0,
        max_tf: 0,
        min_norm: u8::MAX,
    };
    let (mut block_max_tf, mut block_min_norm) = (0, u8::MAX);
    let mut bounds = None;
    while let Some(d) = p.next()? {
        let tf = p.freq()?;
        if info.positions.is_some() {
            p.positions()?;
        }
        let norm = norms.map_or(0, |n| n.get(d));
        if list.df.is_multiple_of(block_len) {
            bounds = p.block_max();
            (block_max_tf, block_min_norm) = (0, u8::MAX);
        }
        block_max_tf = block_max_tf.max(tf);
        block_min_norm = block_min_norm.min(norm);
        list.df = list.df.saturating_add(1);
        list.ttf = list.ttf.saturating_add(u64::from(tf));
        list.max_tf = list.max_tf.max(tf);
        list.min_norm = list.min_norm.min(norm);
        if let Some(len) = usize::try_from(d).ok().and_then(|d| lens.get_mut(d)) {
            *len = len.saturating_add(tf);
        }
        if let Some(col) = keyword
            && col.ords(d).binary_search(&info.ordinal()).is_err()
        {
            return Err(invalid(
                SectionKind::Columns,
                fid,
                0,
                "keyword ords other than the field's postings",
            ));
        }
        let block_done = list.df.is_multiple_of(block_len) || list.df == info.df();
        if let (true, Some(off), Some(b)) = (block_done, info.skip, bounds)
            && (b.max_tf, b.min_norm) != (block_max_tf, block_min_norm)
        {
            let k = list
                .df
                .saturating_sub(1)
                .checked_div(block_len)
                .unwrap_or(0);
            let at = off.saturating_add(u64::from(k).saturating_mul(SKIP_ENTRY_LEN));
            return Err(invalid(
                SectionKind::Skip,
                fid,
                at,
                "skip entry bounds other than its block's",
            ));
        }
    }
    Ok(list)
}

/// Where the Skip entries of a term with `df ≥ 128` starting at `skip_off` end (one per postings block).
fn entries_end(skip_off: u64, df: u32) -> u64 {
    let blocks = u64::from(df).div_ceil(u64::try_from(BLOCK_LEN).unwrap_or(128));
    // The reader checked that the entries fit in the section, so this cannot overflow.
    skip_off.saturating_add(blocks.saturating_mul(SKIP_ENTRY_LEN))
}

/// Whether a stored value is what `column` holds for `docid` (a keyword column holds the distinct values, sorted).
fn stored_matches(value: Option<&Value>, column: &Column, docid: u32) -> bool {
    match column {
        Column::Numeric(c) => value == c.get(docid).as_ref(),
        Column::Bool(c) => value == c.get(docid).map(Value::Bool).as_ref(),
        Column::Keyword(c) => {
            let mut values: Vec<&[u8]> = match value {
                None => Vec::new(),
                Some(Value::String(s)) => vec![s.as_bytes()],
                Some(Value::Strings(list)) => list.iter().map(String::as_bytes).collect(),
                Some(_) => return false,
            };
            values.sort_unstable();
            values.dedup();
            values.into_iter().eq(c.values(docid))
        }
    }
}

fn invalid(kind: SectionKind, field: u32, position: u64, what: &'static str) -> ReadError {
    ReadError::Section {
        kind,
        field,
        position,
        reason: SegmentCorruption::Invalid { what },
    }
}

/// Whether a numeric column value is one a document can hold (SPEC §5.2.1, §6.4.1): a finite `f64` other than `-0.0`,
/// a `datetime` in its range; any `i64`.
fn in_range(value: &Value) -> bool {
    match *value {
        Value::F64(v) => v.is_finite() && !(v == 0.0 && v.is_sign_negative()),
        Value::Datetime(t) => (MIN_DATETIME..=MAX_DATETIME).contains(&t),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use persia_analysis::AnalyzerRegistry;
    use persia_format::ByteSource;

    use super::*;
    use crate::document::Document;
    use crate::schema::SchemaBuilder;
    use crate::segment::{SegmentBuilder, SegmentMeta, SegmentWriter};

    /// `verify` checks every block table again in full and compares it with what open kept for the lookups
    /// (ROADMAP 5.3.10): a kept table that no longer matches the bytes (as when a mapped file changed) is reported.
    #[test]
    fn a_kept_block_table_other_than_the_bytes_is_corruption() {
        let schema = SchemaBuilder::new("c").build().unwrap();
        let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
        for i in 0..50 {
            builder.add(&Document::new(format!("doc-{i}"))).unwrap();
        }
        let meta = SegmentMeta {
            segment_id: 1,
            created_at: 0,
            merge_generation: 0,
        };
        let bytes = SegmentWriter::new(&builder, meta).finish().unwrap();
        let mut seg = Segment::open(ByteSource::new(bytes.into()), &schema).unwrap();
        seg.verify().unwrap();
        // DocStore and IdList: block-framed, different lengths.
        let framed: Vec<usize> = (0..seg.footer.sections.len())
            .filter(|&i| seg.footer.sections[i].kind.is_block_framed())
            .collect();
        let (a, b) = (framed[0], framed[1]);
        let (ta, tb) = (seg.footer.sections[a].table, seg.footer.sections[b].table);
        assert_ne!(ta, tb);
        seg.footer.sections[a].table = tb;
        let err = seg.verify().unwrap_err();
        assert!(err.is_corruption(), "{err:?}");
        assert!(
            matches!(&err, ReadError::Section { reason: SegmentCorruption::Invalid { what }, .. }
                if what.contains("other than the one validated")),
            "{err:?}"
        );
    }
}
