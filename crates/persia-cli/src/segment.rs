//! `persia inspect segment` and `persia dump segment` (ROADMAP 5.4.2, 5.4.3; ADR-0020): one segment file (SPEC §5),
//! read with the collection schema it was written with (a JSON definition, SPEC §6.2), since a segment does not hold
//! its schema, only its hash.
//!
//! The output is for people: `key  value` lines and aligned tables. Terms and ids are printed with their control
//! characters escaped, so each stays on one line.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use persia_engine::schema::{CollectionSchema, Field, FieldType};
use persia_engine::segment::{SEGMENT_FIELD, SectionInfo, Segment};
use persia_format::ByteSource;

/// Opens the segment file `path` with the schema in `schema_path`.
pub fn open(path: &Path, schema_path: &Path) -> Result<(Segment, CollectionSchema)> {
    let json = std::fs::read_to_string(schema_path)
        .with_context(|| format!("reading the schema {}", schema_path.display()))?;
    let schema = CollectionSchema::from_json(&json)
        .with_context(|| format!("parsing the schema {}", schema_path.display()))?;
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let segment = Segment::open(ByteSource::new(bytes.into()), &schema)
        .with_context(|| format!("opening the segment {}", path.display()))?;
    Ok((segment, schema))
}

/// The footer, the section table and per-field statistics; with `verify`, then [`Segment::verify`].
pub fn inspect(path: &Path, schema_path: &Path, verify: bool) -> Result<String> {
    let (seg, schema) = open(path, schema_path)?;
    let mut out = String::new();
    let (min_id, max_id) = seg.id_range();
    let size = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
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
        write_section(&mut out, s, &schema)?;
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
        write_field(&mut out, &seg, f, &sections)?;
    }
    if verify {
        seg.verify()
            .with_context(|| format!("verifying the segment {}", path.display()))?;
        writeln!(out, "\nverify            ok")?;
    }
    Ok(out)
}

fn write_section(out: &mut String, s: &SectionInfo, schema: &CollectionSchema) -> Result<()> {
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
fn write_field(out: &mut String, seg: &Segment, f: &Field, sections: &[SectionInfo]) -> Result<()> {
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

/// The terms of `field_name` with their statistics, or with `term` that term's postings: per document its docid,
/// id, frequency and positions (when the field records them), grouped by postings block.
pub fn dump(
    path: &Path,
    schema_path: &Path,
    field_name: &str,
    term: Option<&str>,
) -> Result<String> {
    let (seg, schema) = open(path, schema_path)?;
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
    let mut out = String::new();
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
        return Ok(out);
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
    Ok(out)
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

/// `s` with its control characters escaped, so it stays one line (and one table cell).
fn escaped(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// As [`escaped`], for bytes that should be UTF-8 (terms and ids, SPEC §5.3.1); others bytes as `\x..`.
fn escaped_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => escaped(s),
        Err(_) => bytes.escape_ascii().to_string(),
    }
}
