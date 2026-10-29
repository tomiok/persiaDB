//! `persia inspect segment` and `persia dump segment` (ROADMAP 5.4.2, 5.4.3), end to end on the golden segment
//! `tests/fixtures/segment/v1.0/basic.pseg` and its schema: what they print, and the errors for a corrupt segment, a
//! wrong schema, and a field or term the segment does not have.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/segment/v1.0")
        .join(name)
}

fn persia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_persia"))
        .args(args)
        .output()
        .unwrap()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// The value of the `key  value` line `key`.
fn value<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find_map(|l| l.strip_prefix(key).filter(|v| v.starts_with(' ')))
        .unwrap_or_else(|| panic!("no {key} line in\n{text}"))
        .trim()
}

/// The whitespace-separated cells of the table row that starts with `first` (after its indent).
fn row<'a>(text: &'a str, first: &str) -> Vec<&'a str> {
    text.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|cells| cells.first() == Some(&first))
        .unwrap_or_else(|| panic!("no {first} row in\n{text}"))
}

#[test]
fn inspect_prints_footer_sections_and_fields() {
    let (seg, schema) = (fixture("basic.pseg"), fixture("basic.schema.json"));
    let out = stdout(&persia(&[
        "inspect",
        "segment",
        s(&seg),
        "--schema",
        s(&schema),
        "--verify",
    ]));
    assert_eq!(
        value(&out, "segment_id"),
        "0123456789abcdeffedcba9876543210"
    );
    assert_eq!(value(&out, "format"), "1.0");
    assert_eq!(value(&out, "doc_count"), "300");
    assert_eq!(value(&out, "merge_generation"), "2");
    assert_eq!(value(&out, "schema"), "things (8 of 8 fields)");
    assert_eq!(value(&out, "id_range"), "doc-000 .. doc-299");
    let size = std::fs::metadata(&seg).unwrap().len();
    assert_eq!(value(&out, "size"), format!("{size} bytes"));
    assert_eq!(value(&out, "verify"), "ok");

    // Every section, with segment-level ones under `-`, raw ones without a stream; offsets sum up to the footer.
    assert_eq!(value(&out, "sections"), "(27)");
    assert_eq!(
        row(&out, "IdMap")[1..],
        ["-", "17456", "465", "raw", "-", "-"]
    );
    let doc_store = row(&out, "DocStore");
    assert_eq!(doc_store[1], "-");
    assert!(doc_store[4].parse::<u64>().unwrap() > doc_store[3].parse::<u64>().unwrap());
    // `tag` (keyword): 225 documents, 450 values, 5 terms.
    assert_eq!(
        row(&out, "tag")[..7],
        ["tag", "keyword", "225", "450", "2.00", "5", "1903"]
    );
    assert_eq!(row(&out, "raw")[..3], ["raw", "stored", "-"]);
    assert_eq!(row(&out, "qty")[..3], ["qty", "i64", "300"]);
}

#[test]
fn inspect_verify_finds_damage_that_open_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let schema = fixture("basic.schema.json");
    let mut bytes = std::fs::read(fixture("basic.pseg")).unwrap();
    // A payload byte of title's Positions block (offset 10568, after its 16-byte block header): open checks the
    // block tables only.
    bytes[10568 + 20] ^= 0x01;
    let seg = dir.path().join("damaged.pseg");
    std::fs::write(&seg, &bytes).unwrap();
    let out = stdout(&persia(&[
        "inspect",
        "segment",
        s(&seg),
        "--schema",
        s(&schema),
    ]));
    assert_eq!(value(&out, "doc_count"), "300");
    let failed = persia(&[
        "inspect",
        "segment",
        s(&seg),
        "--schema",
        s(&schema),
        "--verify",
    ]);
    assert_eq!(failed.status.code(), Some(3), "corruption (ROADMAP 6.6.7)");
    let err = stderr(&failed);
    assert!(err.contains("verifying the segment"), "{err}");
    assert!(err.contains("Positions"), "{err}");
    // The report still comes first, then the error.
    let report = String::from_utf8(failed.stdout).unwrap();
    assert_eq!(value(&report, "doc_count"), "300");
    assert!(!report.contains("\nverify "), "{report}");
}

#[test]
fn a_segment_needs_its_schema() {
    let dir = tempfile::tempdir().unwrap();
    let seg = fixture("basic.pseg");
    let json = std::fs::read_to_string(fixture("basic.schema.json")).unwrap();
    let other = dir.path().join("other.json");
    std::fs::write(&other, json.replace("\"things\"", "\"others\"")).unwrap();
    let err = stderr(&persia(&[
        "inspect",
        "segment",
        s(&seg),
        "--schema",
        s(&other),
    ]));
    assert!(err.contains("opening the segment"), "{err}");
    let err = stderr(&persia(&[
        "inspect",
        "segment",
        s(&seg),
        "--schema",
        s(&dir.path().join("missing.json")),
    ]));
    assert!(err.contains("reading the schema"), "{err}");
}

#[test]
fn dump_lists_terms_and_postings() {
    let (seg, schema) = (fixture("basic.pseg"), fixture("basic.schema.json"));
    let dump = |extra: &[&str]| {
        let mut args = vec!["dump", "segment", s(&seg), "--schema", s(&schema)];
        args.extend_from_slice(extra);
        persia(&args)
    };
    let terms = stdout(&dump(&["--field", "tag"]));
    assert_eq!(row(&terms, "\"red\""), ["\"red\"", "225", "270", "2"]);
    assert_eq!(row(&terms, "\"\""), ["\"\"", "45", "45", "1"]);
    assert_eq!(terms.lines().count(), 6, "{terms}");

    // `red`: 225 documents in two postings blocks (128 + 97); doc ids from the IdList.
    let red = stdout(&dump(&["--field", "tag", "--term", "red"]));
    assert_eq!(value(&red, "df"), "225");
    assert_eq!(value(&red, "ttf"), "270");
    assert_eq!(red.matches("\nblock ").count(), 2, "{red}");
    let docs: Vec<Vec<&str>> = red
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|c| c.len() >= 3 && c[0].parse::<u32>().is_ok())
        .collect();
    assert_eq!(docs.len(), 225);
    assert_eq!(
        docs.iter()
            .map(|c| c[2].parse::<u32>().unwrap())
            .sum::<u32>(),
        270
    );
    assert!(docs.iter().all(|c| c[1].starts_with("doc-")));

    // A positioned field prints positions.
    let the = stdout(&dump(&["--field", "title", "--term", "the"]));
    assert!(the.lines().any(|l| l.trim_end().ends_with(" 0,3")), "{the}");

    for (args, want) in [
        (&["--field", "nope"][..], "no field"),
        (&["--field", "qty"][..], "has no terms"),
        (
            &["--field", "body", "--term", "running"][..],
            "no term \"running\"",
        ),
    ] {
        let err = stderr(&dump(args));
        assert!(err.contains(want), "{args:?}: {err}");
    }
}
