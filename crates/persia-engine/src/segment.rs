//! Segments: the immutable unit of the index (SPEC §5).
//!
//! A [`SegmentBuilder`] accumulates documents in memory (ROADMAP 5.1); a [`SegmentWriter`] turns it into the bytes
//! of one segment: sections, then a footer (SPEC §5.1, §5.3, ROADMAP 5.2). The output is deterministic: the same
//! documents, in the same order, with the same [`SegmentMeta`] and [`WriteOptions`], give the same bytes.
//!
//! ```
//! use persia_analysis::AnalyzerRegistry;
//! use persia_engine::document::{Document, Value};
//! use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder};
//! use persia_engine::segment::{SegmentBuilder, SegmentMeta, SegmentWriter};
//!
//! let schema = SchemaBuilder::new("products")
//!     .field("brand", FieldKind::Keyword(KeywordOptions::default()))
//!     .build()?;
//! let brand = schema.field("brand").map(|f| f.id()).ok_or("no field")?;
//! let mut builder = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin())?;
//! builder.add(&Document::new("p1").with(brand, Value::String("acme".into())))?;
//!
//! let meta = SegmentMeta { segment_id: 7, created_at: 0, merge_generation: 0 };
//! let bytes = SegmentWriter::new(&builder, meta).finish()?;
//! assert_eq!(&bytes[bytes.len() - 4..], b"PSEG");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod arena;
mod builder;
mod kind;
mod norms;
mod write;

pub use builder::{MAX_DOCS, POSITION_GAP, SegmentBuilder};
pub use kind::{SEGMENT_FIELD, SectionKind};
pub use write::{
    FOOTER_MAGIC, FORMAT_MAJOR, FORMAT_MINOR, MAX_FOOTER_LEN, SegmentMeta, SegmentWriter,
    WriteOptions,
};

use crate::document::DocumentError;

/// Why a segment could not be built or written.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SegmentError {
    /// A `text` field names an analyzer version the registry does not have.
    #[error("field {field}: unknown analyzer {analyzer}")]
    UnknownAnalyzer {
        /// The field.
        field: String,
        /// The analyzer, as `name@version`.
        analyzer: String,
    },
    /// The document breaks a rule of the schema (SPEC §6.4.1).
    #[error(transparent)]
    Document(#[from] DocumentError),
    /// The segment already holds a document with this id.
    #[error("duplicate document id {id:?}")]
    DuplicateId {
        /// The id.
        id: String,
    },
    /// The segment holds as many documents as it may (SPEC §5.4).
    #[error("segment is full ({max_docs} documents)")]
    Full {
        /// The limit.
        max_docs: u32,
    },
    /// A segment needs at least one document.
    #[error("segment has no documents")]
    Empty,
    /// An internal count or offset would overflow: the builder holds more than a segment can address.
    #[error("too many {what} for one segment")]
    TooLarge {
        /// What overflowed.
        what: &'static str,
    },
    /// An invariant of the builder did not hold while writing: a bug, reported rather than written as a corrupt
    /// segment.
    #[error("internal error: {what}")]
    Internal {
        /// What did not hold.
        what: &'static str,
    },
    /// A section encoder rejected its input: invalid [`WriteOptions`], or a bug.
    #[error(transparent)]
    Encode(#[from] persia_format::EncodeError),
}
