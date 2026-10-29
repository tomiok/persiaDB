//! `persia inspect segment` and `persia dump segment` (ROADMAP 5.4.2, 5.4.3; ADR-0020): one segment file (SPEC §5),
//! read with the collection schema it was written with (a JSON definition, SPEC §6.2), since a segment does not hold
//! its schema, only its hash.
//!
//! The output is for people: `key  value` lines and aligned tables, written as it is produced. Terms are quoted and
//! ids and names escaped as Rust `Debug` escapes them, so each stays on one line and none is ambiguous.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use persia_engine::schema::{CollectionSchema, Field, FieldType};
use persia_engine::segment::{ReadError, SEGMENT_FIELD, SectionInfo, Segment, SegmentCorruption};
use persia_format::ByteSource;

/// Opens the segment file `path` with the schema in `schema_path`; also returns the file's size.
pub fn open(path: &Path, schema_path: &Path) -> Result<(Segment, CollectionSchema, usize)> {
    let json = std::fs::read_to_string(schema_path)
        .with_context(|| format!("reading the schema {}", schema_path.display()))?;
    let schema = CollectionSchema::from_json(&json)
        .with_context(|| format!("parsing the schema {}", schema_path.display()))?;
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let size = bytes.len();
    let segment = match Segment::open(ByteSource::new(bytes.into()), &schema) {
        Ok(segment) => segment,
        // The schema is an argument here, not stored data: a mismatch is a usage failure (exit 1), not a corrupt
        // segment (exit 3, ROADMAP 6.6.7). No `ReadError` stays in the chain.
        Err(ReadError::Corrupt {
            reason:
                SegmentCorruption::SchemaHash { .. } | SegmentCorruption::SchemaFieldCount { .. },
            ..
        }) => bail!(
            "the schema {} is not the one the segment {} was written with",
            schema_path.display(),
            path.display()
        ),
        Err(e) => {
            return Err(e).with_context(|| format!("opening the segment {}", path.display()));
        }
    };
    Ok((segment, schema, size))
}

/// Writes the footer, the section table and per-field statistics to `out`; with `verify`, then runs
/// [`Segment::verify`], whose error comes after the report (so an operator still sees what the segment holds).
pub fn inspect(out: &mut dyn Write, path: &Path, schema_path: &Path, verify: bool) -> Result<()> {
    let (seg, schema, size) = open(path, schema_path)?;
    let report = write_report(out, &seg, &schema, size);
    if !verify {
        return report;
    }
    // Even when the report could not be written (a closed pipe), a corrupt segment must fail the command.
    seg.verify()
        .with_context(|| format!("verifying the segment {}", path.display()))?;
    report?;
    writeln!(out, "\nverify            ok")?;
    Ok(())
}

/// The footer, the section table and the per-field statistics.
fn write_report(
    out: &mut dyn Write,
    seg: &Segment,
    schema: &CollectionSchema,
    size: usize,
) -> Result<()> {
    let (min_id, max_id) = seg.id_range();
    let rows = [
        ("segment_id", format!("{:032x}", seg.segment_id())),
        ("format", format!("1.{}", seg.format_minor())),
        ("rocompat_flags", format!("{:#x}", seg.rocompat_flags())),
        ("doc_count", seg.doc_count().to_string()),
        ("merge_generation", seg.merge_generation().to_string()),
        (
            "created_at",
            format!("{} (µs since the Unix epoch)", seg.created_at()),
        ),
        (
            "schema",
            format!(
                "{} ({} of {} fields)",
                escaped(schema.name()),
                seg.schema_field_count(),
                schema.fields().len()
            ),
        ),
        (
            "id_range",
            format!("{} .. {}", escaped_bytes(min_id), escaped_bytes(max_id)),
        ),
        ("size", format!("{size} bytes")),
    ];
    for (key, value) in rows {
        writeln!(out, "{key:<18}{value}")?;
    }

    let sections = seg.sections().context("reading the section table")?;
    writeln!(out, "\nsections ({})", sections.len())?;
    writeln!(
        out,
        "  {:<10} {:<16} {:>10} {:>10} {:>12} {:>7} {:>8}",
        "kind", "field", "offset", "len", "stream_len", "blocks", "block"
    )?;
    for s in &sections {
        write_section(out, s, schema)?;
    }

    let count = usize::try_from(seg.schema_field_count()).unwrap_or(usize::MAX);
    let fields = schema.fields().get(..count).unwrap_or_default();
    writeln!(out, "\nfields ({})", fields.len())?;
    writeln!(
        out,
        "  {:<16} {:<9} {:>9} {:>15} {:>9} {:>9} {:>10}",
        "field", "type", "docs", "sum_field_len", "avg_len", "terms", "bytes"
    )?;
    for f in fields {
        write_field(out, seg, f, &sections)?;
    }
    Ok(())
}

fn write_section(out: &mut dyn Write, s: &SectionInfo, schema: &CollectionSchema) -> Result<()> {
    let field = field_label(s.field, schema);
    let (stream_len, blocks, block) = match s.stream {
        Some(st) => (
            st.len.to_string(),
            st.block_count.to_string(),
            st.block_size.to_string(),
        ),
        None => ("raw".to_owned(), "-".to_owned(), "-".to_owned()),
    };
    writeln!(
        out,
        "  {:<10} {:<16} {:>10} {:>10} {:>12} {:>7} {:>8}",
        format!("{:?}", s.kind),
        field,
        s.offset,
        s.len,
        stream_len,
        blocks,
        block
    )?;
    Ok(())
}

/// One field's line: documents with it, field lengths and terms for `text`/`keyword`, documents with a value for
/// a column field, and the bytes of its sections.
fn write_field(
    out: &mut dyn Write,
    seg: &Segment,
    f: &Field,
    sections: &[SectionInfo],
) -> Result<()> {
    let id = f.id();
    let bytes: u64 = sections
        .iter()
        .filter(|s| s.field == id.get())
        .map(|s| s.len)
        .sum();
    let ty = f.field_type();
    let (docs, sum, avg, terms) = match ty {
        FieldType::Text | FieldType::Keyword => {
            let stats = seg.field_stats(id).context("missing field statistics")?;
            let terms = seg.terms(id)?.len();
            let avg = if stats.docs_with_field == 0 {
                "-".to_owned()
            } else {
                // Display only: the rounding of the u64 → f64 conversions does not matter.
                #[allow(clippy::cast_precision_loss)]
                let avg = stats.sum_field_len as f64 / f64::from(stats.docs_with_field);
                format!("{avg:.2}")
            };
            (
                stats.docs_with_field.to_string(),
                stats.sum_field_len.to_string(),
                avg,
                terms.to_string(),
            )
        }
        FieldType::I64 | FieldType::F64 | FieldType::Datetime => {
            let col = seg.numeric_column(id)?;
            let docs = (0..seg.doc_count()).filter(|&d| col.has_value(d)).count();
            (docs.to_string(), "-".into(), "-".into(), "-".into())
        }
        FieldType::Bool => {
            let col = seg.bool_column(id)?;
            let docs = (0..seg.doc_count())
                .filter(|&d| col.get(d).is_some())
                .count();
            (docs.to_string(), "-".into(), "-".into(), "-".into())
        }
        FieldType::Stored => ("-".into(), "-".into(), "-".into(), "-".into()),
    };
    writeln!(
        out,
        "  {:<16} {:<9} {:>9} {:>15} {:>9} {:>9} {:>10}",
        escaped(f.name()),
        ty.name(),
        docs,
        sum,
        avg,
        terms,
        bytes
    )?;
    Ok(())
}

/// Writes to `out` the terms of `field_name` with their statistics, or with `term` that term's postings: per document its docid,
/// id, frequency and positions (when the field records them), grouped by postings block.
pub fn dump(
    out: &mut dyn Write,
    path: &Path,
    schema_path: &Path,
    field_name: &str,
    term: Option<&str>,
) -> Result<()> {
    let (seg, schema, _) = open(path, schema_path)?;
    let Some(field) = schema.field(field_name) else {
        bail!("the schema has no field {}", escaped(field_name));
    };
    if !matches!(field.field_type(), FieldType::Text | FieldType::Keyword) {
        bail!(
            "field {} is a {} field, which has no terms",
            escaped(field_name),
            field.field_type()
        );
    }
    let mut terms = seg.terms(field.id())?;
    let Some(term) = term else {
        writeln!(
            out,
            "{:<24} {:>9} {:>9} {:>7}",
            "term", "df", "ttf", "max_tf"
        )?;
        let mut all = terms.range(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded);
        while let Some(next) = all.next() {
            let (t, info) = next?;
            writeln!(
                out,
                "{:<24} {:>9} {:>9} {:>7}",
                quoted(t),
                info.df(),
                info.ttf(),
                info.max_tf()
            )?;
        }
        return Ok(());
    };
    let Some(info) = terms.get(term.as_bytes())? else {
        bail!(
            "field {} has no term {} in this segment (terms are matched as indexed, after analysis)",
            escaped(field_name),
            quoted(term.as_bytes())
        );
    };
    let rows = [
        ("term", quoted(term.as_bytes())),
        ("ordinal", info.ordinal().to_string()),
        ("df", info.df().to_string()),
        ("ttf", info.ttf().to_string()),
        ("max_tf", info.max_tf().to_string()),
        ("min_norm", info.min_norm().to_string()),
    ];
    for (key, value) in rows {
        writeln!(out, "{key:<10}{value}")?;
    }
    let mut postings = terms.postings(&info)?;
    let mut block_last = None;
    while let Some(docid) = postings.next()? {
        if let Some(b) = postings.block_max()
            && block_last.is_none_or(|last| docid > last)
        {
            writeln!(
                out,
                "\nblock  last_docid {}  max_tf {}  min_norm {}",
                b.last_docid, b.max_tf, b.min_norm
            )?;
            writeln!(
                out,
                "  {:>10}  {:<24} {:>5}  positions",
                "docid", "id", "tf"
            )?;
            block_last = Some(b.last_docid);
        }
        let tf = postings.freq()?;
        let positions = postings
            .positions()?
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let id = seg.id_of(docid)?;
        writeln!(
            out,
            "  {docid:>10}  {:<24} {tf:>5}  {positions}",
            escaped(&id)
        )?;
    }
    Ok(())
}

/// A term in double quotes, escaped (so an empty term or one with spaces reads as one).
fn quoted(term: &[u8]) -> String {
    match std::str::from_utf8(term) {
        Ok(s) => format!("{s:?}"),
        Err(_) => format!("b\"{}\"", term.escape_ascii()),
    }
}

/// `field`'s name, or `-` for the segment-level sections.
fn field_label(field: u32, schema: &CollectionSchema) -> String {
    if field == SEGMENT_FIELD {
        return "-".to_owned();
    }
    schema
        .fields()
        .iter()
        .find(|f| f.id().get() == field)
        .map_or_else(|| format!("#{field}"), |f| escaped(f.name()))
}

/// `s` as Rust escapes it in `Debug` (control and format characters, backslashes and quotes), so it stays one line
/// and one table cell, and two strings never print the same.
fn escaped(s: &str) -> String {
    s.escape_debug().to_string()
}

/// As [`escaped`], for bytes that should be UTF-8 (terms and ids, SPEC §5.3.1); others bytes as `\x..`.
fn escaped_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => escaped(s),
        Err(_) => bytes.escape_ascii().to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// A writer whose reader is gone.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// `inspect --verify` verifies even when the report cannot be written: a corrupt segment is a verify error, a
    /// sound one gives back the write error (which the binary treats as success).
    #[test]
    fn verify_runs_when_the_report_cannot_be_written() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/segment/v1.0");
        let schema = dir.join("basic.schema.json");
        let sound = inspect(&mut ClosedPipe, &dir.join("basic.pseg"), &schema, true).unwrap_err();
        let io = sound
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind);
        assert_eq!(io, Some(std::io::ErrorKind::BrokenPipe), "{sound:#}");

        let mut bytes = std::fs::read(dir.join("basic.pseg")).unwrap();
        bytes[10568 + 20] ^= 0x01; // a payload byte of title's Positions block
        let tmp = tempfile::tempdir().unwrap();
        let damaged = tmp.path().join("damaged.pseg");
        std::fs::write(&damaged, &bytes).unwrap();
        let err = inspect(&mut ClosedPipe, &damaged, &schema, true).unwrap_err();
        assert!(
            format!("{err:#}").contains("verifying the segment"),
            "{err:#}"
        );
    }

    #[test]
    fn escaping_is_one_line_and_unambiguous() {
        assert_ne!(escaped("a\\nb"), escaped("a\nb"));
        assert!(!escaped("a\nb\u{202e}").contains(['\n', '\u{202e}']));
        assert_eq!(escaped("doc-1"), "doc-1");
        assert_eq!(quoted(b""), "\"\"");
        assert_eq!(quoted(b"\xff"), "b\"\\xff\"");
    }
}
