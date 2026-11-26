//! `persia import` (ROADMAP 10.4.2): JSON Lines into a collection, through the embedded API (`persia::Db`), for quick
//! experiments.
//!
//! **Input.** `--input <FILE>`, or the standard input without it. One document per line, in the JSON form of SPEC
//! §6.4.1, as `persia dump documents` writes it, so a dump imports back. A line ends at `\n` (`\r\n` too); lines that
//! are empty or hold only JSON whitespace (spaces, tabs, carriage returns) are skipped (and counted, so line numbers
//! stay those of the input). A line is read in bounded memory: at most 16 MiB, the longest document JSON text (module
//! `persia::internal::jsonl`). An input that starts with a UTF-8 byte order mark (U+FEFF, which JSON does not allow) is
//! refused at line 1 with a message that says so: remove the mark and import again.
//!
//! **The database.** It opens read-write, as any writer does, so it fails (status 1) while another writer has the
//! database open, and recovery runs first (SPEC §8.6). The collection must exist, unless `--schema <FILE>` gives
//! its schema (JSON, SPEC §6.2): then the database is created if no file is at the path, and the collection if the
//! database lacks it; an existing collection must have exactly that schema, and the schema's `name` must be
//! `--collection`. Without `--schema` a missing file is an error and nothing is created. The command has the built-in
//! analyzers only: a schema that names a registered analyzer (SPEC §6.2.2) fails the open (status 1).
//!
//! **Each line** is read with the collection's schema (`Document::from_json`) and put (`Collection::put`, an upsert
//! by id, SPEC §8.3), logged `Buffered`. They are committed at least once, at the end (`Db::close`), which makes them
//! durable: the database's maintenance thread may also commit during a long import, as it does for any write (its
//! commit policy, or a memtable reaching its size, SPEC §8.2). A line with the id of an earlier one replaces it.
//!
//! **A bad line stops the import.** Not UTF-8, longer than 16 MiB, not JSON, not a document of the schema, past a
//! limit of SPEC §6.4.1, or refused by the database when it is put (its blob reference, below, or a document the
//! memtable rejects, SPEC §8.2): the command commits the documents of the lines before it and exits with status 1,
//! naming the line (1-based) and why, unless the put's failure is stored damage: the status follows the embedded
//! API's code, 3 for `ErrorCode::Corrupt` (such as a damaged blob manifest frame the put reached) and 1 for any other
//! code (ROADMAP 10.1.33). Nothing from that line on is imported, so an import resumes from it (`tail -n +<line>`),
//! and running it again is harmless (every put replaces the document of its id). A crash, or a kill, leaves a prefix
//! of the lines imported (the log keeps their order, SPEC §8.2.1).
//!
//! **Blob references.** A line may carry a `blob` reference (as a dump writes it, SPEC §6.4.1). The embedded API
//! never logs a reference its blob store does not hold (SPEC §9.6, "No dangling references"), so a line whose blob
//! is not in this database's blob pack (`<name>.persia.blobs`) stops the import like any bad line, naming the
//! `blob_id`. So does a reference that names a blob the pack holds with another `size` or `chunk_size` than the
//! blob's: the input's error (`ErrorCode::InvalidArgument`, status 1; SPEC §9.6), not damage. A dump carries
//! references, not bytes, so the dump of a database with blobs imports only into that database (or a copy with its
//! pack, SPEC §4.8) until `dump documents` exports blobs (ROADMAP 10.4.6).
//!
//! The text output is `key  value` rows. `--json` writes one object on one line, with these keys and no others:
//! `{"collection": .., "created": .., "lines": .., "blank_lines": .., "documents": ..}`, where `created` says whether
//! `--schema` created the collection, `lines` counts the lines read, `blank_lines` those skipped, and `documents` the
//! documents put (a replaced one counts each time).

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use persia::internal::documents::Error as BlobDatabaseError;
use persia::internal::jsonl::Lines;
use persia::{
    BlobId, Collection, CollectionSchema, Db, Document, Durability, ErrorCode, OpenOptions,
};
use serde_json::{Value, json};

/// What `persia import` was asked to do (its arguments; the doc comments are their help).
#[derive(Debug, clap::Args)]
pub struct Options {
    /// The container file.
    pub path: PathBuf,
    /// The collection the documents go into.
    #[arg(long)]
    pub collection: String,
    /// The JSON Lines file [default: the standard input].
    #[arg(long)]
    pub input: Option<PathBuf>,
    /// The collection's schema (JSON): creates the database if no file is at the path, and the collection if the
    /// database lacks it; an existing collection must have exactly this schema.
    #[arg(long)]
    pub schema: Option<PathBuf>,
    /// Write one JSON object (its keys are listed in the README's command-line section) instead of text.
    #[arg(long)]
    pub json: bool,
}

/// What an import read and put.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    /// Whether `--schema` created the collection.
    created: bool,
    /// The lines read.
    lines: u64,
    /// The lines skipped as blank.
    blank_lines: u64,
    /// The documents put.
    documents: u64,
}

/// Imports the JSON Lines of `options.input` (or the standard input) into the collection and writes what it did to
/// `out`.
pub fn import(out: &mut dyn Write, options: &Options) -> Result<()> {
    let input: Box<dyn BufRead> = match &options.input {
        Some(file) => {
            Box::new(BufReader::new(File::open(file).with_context(|| {
                format!("opening the input {}", file.display())
            })?))
        }
        None => Box::new(std::io::stdin().lock()),
    };
    let counts = run(options, input)?;
    if options.json {
        writeln!(out, "{}", to_json(&options.collection, &counts))?;
    } else {
        write_text(out, &options.collection, &counts)?;
    }
    Ok(())
}

/// Opens the database, imports `input` and closes it, committing what was put.
fn run(options: &Options, input: impl BufRead) -> Result<Counts> {
    let path = &options.path;
    // Read before anything is opened or created: a bad schema file creates no database.
    let schema = match &options.schema {
        Some(file) => Some(read_schema(file, &options.collection)?),
        None => None,
    };
    // Only `--schema` creates a database, and only where nothing is at the path.
    if (schema.is_none() || path.exists()) && !path.is_file() {
        bail!("{} is not a container file", path.display());
    }
    let db = OpenOptions::new()
        .create_if_missing(schema.is_some())
        .open(path)
        .map_err(|err| {
            if err.code() == ErrorCode::Conflict {
                anyhow::anyhow!(
                    "{} is open by a writer; import into it once that is closed",
                    path.display()
                )
            } else {
                anyhow::Error::new(err).context(format!("opening {}", path.display()))
            }
        })?;
    let mut counts = Counts::default();
    let imported = match collection(&db, path, &options.collection, schema, &mut counts) {
        Ok(found) => put_lines(&found, input, &mut counts).map_err(|err| {
            err.context(format!(
                "the import into {} stopped at a bad line (documents put before it, imported: {}; nothing from that \
                 line on)",
                path.display(),
                counts.documents
            ))
        }),
        Err(err) => Err(err),
    };
    // Committed whatever happened: the documents of the lines before a bad one are imported (module docs).
    let closed = db.close();
    match (imported, closed) {
        (Ok(()), Ok(())) => Ok(counts),
        (Ok(()), Err(err)) => Err(anyhow::Error::new(err).context(format!(
            "committing the import into {} (what reached the disk is known after a reopen)",
            path.display()
        ))),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(close)) => Err(anyhow::Error::new(close).context(format!(
            "{err:#}; then committing at the close of {} failed (what reached the disk is known after a reopen)",
            path.display()
        ))),
    }
}

/// The schema in the JSON file `file`, which must name `collection`.
fn read_schema(file: &Path, collection: &str) -> Result<CollectionSchema> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("reading the schema {}", file.display()))?;
    let schema = CollectionSchema::from_json(&text)
        .with_context(|| format!("reading the schema {}", file.display()))?;
    if schema.name() != collection {
        bail!(
            "the schema {} is of the collection {:?}, not {collection:?}",
            file.display(),
            schema.name()
        );
    }
    Ok(schema)
}

/// The collection `name` of `db`, created from `schema` when the database lacks it.
fn collection(
    db: &Db,
    path: &Path,
    name: &str,
    schema: Option<CollectionSchema>,
    counts: &mut Counts,
) -> Result<Collection> {
    let names = db.collection_names()?;
    if names.iter().any(|n| n == name) {
        let found = db.collection(name)?;
        if let Some(wanted) = schema
            && found.schema()? != wanted
        {
            bail!(
                "the collection {name:?} of {} has another schema than the one given",
                path.display()
            );
        }
        return Ok(found);
    }
    let Some(schema) = schema else {
        let names: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
        bail!(
            "{} has no collection {name:?} (it has: {}); give --schema to create it",
            path.display(),
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            }
        );
    };
    let created = db
        .create_collection(schema)
        .with_context(|| format!("creating the collection {name:?}"))?;
    counts.created = true;
    Ok(created)
}

/// Puts the document of every line of `input` into `collection`, stopping at the first bad line.
fn put_lines(collection: &Collection, input: impl BufRead, counts: &mut Counts) -> Result<()> {
    let schema = collection.schema()?;
    let collection = collection.clone().with_durability(Durability::Buffered);
    let mut lines = Lines::new(input);
    while let Some(line) = lines.next_line()? {
        counts.lines = line.number;
        if line.number == 1 && line.text.starts_with(BOM) {
            bail!(
                "line 1: the input starts with a UTF-8 byte order mark (U+FEFF), which JSON does not allow: remove it \
                 and import again"
            );
        }
        if is_blank(line.text) {
            counts.blank_lines += 1;
            continue;
        }
        let at = line.number;
        let doc = Document::from_json(line.text, &schema).with_context(|| {
            format!(
                "line {at}: not a document of the collection {:?}",
                schema.name()
            )
        })?;
        collection
            .put(&doc)
            .map_err(|err| put_failed(at, &doc, err))?;
        counts.documents += 1;
    }
    Ok(())
}

/// The error of line `line`, whose document `doc` was not put.
fn put_failed(line: u64, doc: &Document, err: persia::Error) -> anyhow::Error {
    let context = match blob_not_stored(&err) {
        Some(blob_id) => format!(
            "line {line}: the document {:?} references the blob {blob_id}, which is not in this database's blob \
             store (a dump carries blob references, not their bytes)",
            doc.id()
        ),
        None => format!("line {line}: putting the document {:?}", doc.id()),
    };
    anyhow::Error::new(err).context(context)
}

/// The blob a put found missing from the store (SPEC §9.6): the internal layer's error, under the facade's. The
/// facade gives it the code `NotFound` only, which a dropped collection shares, so its variant is matched here.
fn blob_not_stored(err: &persia::Error) -> Option<BlobId> {
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = next {
        if let Some(BlobDatabaseError::BlobNotStored { blob_id }) = e.downcast_ref() {
            return Some(*blob_id);
        }
        next = e.source();
    }
    None
}

/// The UTF-8 byte order mark, which RFC 8259 forbids at the start of JSON text (module docs: refused, not skipped).
const BOM: char = '\u{feff}';

/// A line of JSON whitespace only (RFC 8259: space, tab, carriage return; a line holds no line feed).
fn is_blank(text: &str) -> bool {
    text.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\r'))
}

fn to_json(collection: &str, counts: &Counts) -> Value {
    json!({
        "collection": collection,
        "created": counts.created,
        "lines": counts.lines,
        "blank_lines": counts.blank_lines,
        "documents": counts.documents,
    })
}

fn write_text(out: &mut dyn Write, collection: &str, counts: &Counts) -> Result<()> {
    let rows = [
        (
            "collection",
            format!(
                "{collection}{}",
                if counts.created { " (created)" } else { "" }
            ),
        ),
        ("lines", counts.lines.to_string()),
        ("blank lines", counts.blank_lines.to_string()),
        ("documents", counts.documents.to_string()),
    ];
    for (key, value) in rows {
        writeln!(out, "{key:<18}{value}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::documents;

    #[test]
    fn the_json_object_has_its_documented_shape() {
        let counts = Counts {
            created: true,
            lines: 7,
            blank_lines: 2,
            documents: 5,
        };
        assert_eq!(
            to_json("notes", &counts).to_string(),
            r#"{"blank_lines":2,"collection":"notes","created":true,"documents":5,"lines":7}"#
        );
    }

    #[test]
    fn the_text_rows_state_what_was_read_and_put() {
        let mut out = Vec::new();
        let counts = Counts {
            created: false,
            lines: 3,
            blank_lines: 1,
            documents: 2,
        };
        write_text(&mut out, "notes", &counts).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "collection        notes\nlines             3\nblank lines       1\ndocuments         2\n"
        );
        let mut out = Vec::new();
        write_text(
            &mut out,
            "notes",
            &Counts {
                created: true,
                ..counts
            },
        )
        .unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .starts_with("collection        notes (created)\n")
        );
    }

    #[test]
    fn only_json_whitespace_is_blank() {
        for blank in ["", " ", "\t", "\r", " \t \r"] {
            assert!(is_blank(blank), "{blank:?}");
        }
        // Other Unicode spaces are not JSON whitespace: such a line is a bad document, not a skipped one.
        for text in ["{}", " x", "\u{a0}", "\u{2028}", "\u{b}", "\u{c}"] {
            assert!(!is_blank(text), "{text:?}");
        }
    }

    /// An error that is not a missing blob names the line and the document, and keeps the facade's error as its
    /// source, so the exit status reads its kind.
    #[test]
    fn a_put_error_names_its_line_and_document() {
        let err = OpenOptions::new().open("s3://bucket/app").unwrap_err();
        assert_eq!(blob_not_stored(&err), None);
        let message = err.message().to_owned();
        let failed = put_failed(12, &Document::new("d1"), err);
        assert_eq!(
            format!("{failed:#}"),
            format!("line 12: putting the document \"d1\": {message}")
        );
        assert_eq!(
            failed
                .downcast_ref::<persia::Error>()
                .map(persia::Error::code),
            Some(ErrorCode::Unsupported)
        );
    }

    /// A blob the target's store lacks is found under the facade's error, and the line's error names it.
    #[test]
    fn a_missing_blob_is_found_under_the_facades_error() {
        let source = tempfile::tempdir().unwrap();
        let db = Db::open(source.path().join("a.persia")).unwrap();
        let files = db
            .create_collection_json(r#"{"name": "files", "fields": {}}"#)
            .unwrap();
        let blob = db
            .put_blob(&b"bytes"[..], &persia::PutBlobOptions::new())
            .unwrap();
        db.close().unwrap();
        drop(files);
        let target = tempfile::tempdir().unwrap();
        let db = Db::open(target.path().join("b.persia")).unwrap();
        let files = db
            .create_collection_json(r#"{"name": "files", "fields": {}}"#)
            .unwrap();
        let mut doc = Document::new("f1");
        doc.set_blob_ref(Some(blob.clone()));
        let err = files.put(&doc).unwrap_err();
        assert_eq!(blob_not_stored(&err), Some(blob.blob_id()));
        let message = format!("{:#}", put_failed(3, &doc, err));
        assert!(
            message.starts_with(&format!(
                "line 3: the document \"f1\" references the blob {}, which is not in this database's blob store",
                blob.blob_id()
            )),
            "{message}"
        );
        db.close().unwrap();
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 128 } else { 16 }))]

        /// The round trip of ROADMAP 10.4.2: after any puts, deletes and commits, `dump documents` of a database,
        /// imported into a new one with the same schema, dumps again as the same lines in the same order (the
        /// import commits them in input order, into one segment), every line put once.
        #[test]
        fn a_dump_imports_back_as_the_same_dump(ops in proptest::collection::vec(documents::tests::op(), 0..24)) {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source.persia");
            let (writer, live) = documents::tests::apply(&source, ops);
            let mut dumped = Vec::new();
            documents::dump(&mut dumped, &source, "c").unwrap();
            drop(writer);

            let schema = dir.path().join("schema.json");
            std::fs::write(&schema, documents::tests::schema().to_json().unwrap()).unwrap();
            let options = Options {
                path: dir.path().join("target.persia"),
                collection: "c".to_owned(),
                input: None,
                schema: Some(schema),
                json: false,
            };
            let counts = run(&options, &dumped[..]).unwrap();
            prop_assert_eq!(counts.documents, u64::try_from(live.len()).unwrap());
            prop_assert_eq!(counts.lines, counts.documents);
            prop_assert!(counts.created);
            let mut again = Vec::new();
            documents::dump(&mut again, &options.path, "c").unwrap();
            prop_assert_eq!(String::from_utf8(again).unwrap(), String::from_utf8(dumped).unwrap());
        }
    }
}
