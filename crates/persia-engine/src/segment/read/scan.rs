//! Whole-field scans of a segment's columns for filters (SPEC §7.1.2, §7.3, ROADMAP 6.1.5): which documents have a
//! value, and every document's order-preserving key.
//!
//! A scan's work is bounded by the section it reads, not by the `doc_count` a segment claims: a field without a
//! section has no document with a value.

use roaring::RoaringBitmap;

use super::{ReadError, Segment, type_error};
use crate::bitmap::Ascending;
use crate::schema::{FieldId, FieldType};
use crate::segment::SectionKind;

impl Segment {
    /// The documents that have a value in `field` (deleted ones included): for a `text` field, those with at least one
    /// token (a norm byte above 0, SPEC §5.3.4); for a `keyword` field, at least one value.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`], or [`ReadError::FieldType`] for a `stored` field; a corruption of the section
    /// read.
    pub fn present(&self, field: FieldId) -> Result<RoaringBitmap, ReadError> {
        let id = field.get();
        let mut present = Ascending::default();
        match self.field(field)?.field_type() {
            FieldType::Text => {
                if self.section(SectionKind::Norms, id).is_none() {
                    return Ok(RoaringBitmap::new());
                }
                let mut norms = self.norms(field)?.cursor();
                for docid in 0..self.doc_count() {
                    if norms.get(docid)? > 0 {
                        present.push(docid);
                    }
                }
            }
            FieldType::Keyword => {
                if self.section(SectionKind::Columns, id).is_none() {
                    return Ok(RoaringBitmap::new());
                }
                let column = self.keyword_column(field)?;
                let mut ords = column.cursor();
                for docid in 0..self.doc_count() {
                    if !ords.ords(docid)?.is_empty() {
                        present.push(docid);
                    }
                }
            }
            FieldType::I64 | FieldType::F64 | FieldType::Datetime => {
                return Ok(self.numeric_column(field)?.present());
            }
            FieldType::Bool => return Ok(self.bool_column(field)?.present()),
            other @ FieldType::Stored => return Err(type_error(field, other, "values")),
        }
        Ok(present.finish())
    }

    /// The documents whose `bool` field is `true` (deleted ones included): a filter on `bool` reads it instead of a
    /// key per document.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`], or [`ReadError::FieldType`] for a field of another type; a corruption of the
    /// column.
    pub fn trues(&self, field: FieldId) -> Result<RoaringBitmap, ReadError> {
        Ok(self.bool_column(field)?.trues().clone())
    }

    /// Calls `f` with each document that has a value in `field` (deleted ones included) and its key, in docid order:
    /// the order-preserving key of an `i64`, `f64` or `datetime` value (SPEC §5.2.1), 0 or 1 for a `bool`.
    ///
    /// # Errors
    /// [`ReadError::UnknownField`], or [`ReadError::FieldType`] for a field of another type; a corruption of the
    /// column.
    pub fn column_keys(
        &self,
        field: FieldId,
        mut f: impl FnMut(u32, u64),
    ) -> Result<(), ReadError> {
        match self.field(field)?.field_type() {
            FieldType::I64 | FieldType::F64 | FieldType::Datetime => {
                let column = self.numeric_column(field)?;
                let mut keys = column.cursor();
                for docid in &column.present() {
                    if let Some(key) = keys.key(docid)? {
                        f(docid, key);
                    }
                }
            }
            FieldType::Bool => {
                let column = self.bool_column(field)?;
                let trues = column.trues();
                for docid in &column.present() {
                    f(docid, u64::from(trues.contains(docid)));
                }
            }
            other => return Err(type_error(field, other, "column")),
        }
        Ok(())
    }
}
