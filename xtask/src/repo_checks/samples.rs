//! Samples a page shows that rustdoc cannot run as shown (ROADMAP 10.3.3, 10.1.51).
//!
//! The README's quickstart opens `app.persia` in the working directory, so a user can paste it into a fresh
//! `cargo new` with `persia` as the only dependency. Run as a doctest it would write that file into the crate under
//! test, and a second run would fail (`create_collection_json` refuses a collection that exists); a doctest cannot
//! move to a temporary working directory either (merged doctests share one process). So each such sample has a twin
//! that runs: a Rust block of a page `crates/persia-docs` runs as a doctest, in a temporary directory. This check
//! keeps the shown block equal to its twin, line for line, once the twin is read as the page shows it:
//!
//! - rustdoc's hidden lines (`# ` after the indentation, or a lone `#`) are dropped;
//! - the line `let dir = tempfile::tempdir()?;` is dropped;
//! - `dir.path().join("app.persia")`, which must occur once, is written `"app.persia"`.
//!
//! Nothing else may differ: comments, assertions and calls are the twin's, so the shown sample calls what the twin
//! runs and asserts what it asserts. The twin must be a block rustdoc runs (no `ignore` or `no_run`) on a page a
//! module of `crates/persia-docs` lists (`#[doc = include_str!("...")]`, [`pages`]), and the dependency block shown
//! before it must equal the twin page's first `toml` block, when the shown page has one.

use std::path::Path;

use super::guides::{Fence, doctest, fences, pages, read};
use super::includes::normalize;

/// A page's sample and its twin: the first fence whose first word is `rust` after the line `after` of `page`
/// (from the top when `after` is empty), and the first fence rustdoc reads as Rust after `twin_after` of `twin`.
struct Shown {
    page: &'static str,
    after: &'static str,
    twin: &'static str,
    twin_after: &'static str,
}

/// Every shown sample with a twin. The README's is the getting-started guide's first sample, so the two cannot
/// drift apart (the guide says it is the README quickstart in a temporary directory).
const SHOWN: &[Shown] = &[Shown {
    page: "README.md",
    after: "## Quickstart",
    twin: "website/src/content/docs/guide/getting-started.md",
    twin_after: "## Open a database, index, and search",
}];

/// The modules of `crates/persia-docs` whose item docs are pages run as doctests.
const DOC_MODULES: &[&str] = &["crates/persia-docs/src/guides.rs"];

/// The fences of `text` after its line `after` (all of them when `after` is empty).
fn fences_after<'a>(text: &'a str, after: &str, page: &str) -> Vec<Fence<'a>> {
    let start = if after.is_empty() {
        0
    } else {
        text.lines()
            .position(|line| line.trim_end() == after)
            .unwrap_or_else(|| panic!("{page}: no line {after:?}"))
    };
    fences(text).into_iter().filter(|f| f.at > start).collect()
}

/// The first word of a fence's info string (`rust title="..."` is `rust`, `rust,ignore` is `rust`).
fn first_word(info: &str) -> &str {
    info.split(|c: char| c.is_whitespace() || c == ',')
        .next()
        .unwrap_or_default()
}

/// The twin's lines as the shown page writes them (the module docs' three rules), or why they cannot be.
fn as_shown(twin: &Fence<'_>) -> Result<Vec<String>, String> {
    let mut joins = 0;
    let mut lines = Vec::new();
    for (_, line) in &twin.lines {
        let trimmed = line.trim_start();
        if trimmed == "#"
            || trimmed.starts_with("# ")
            || trimmed == "let dir = tempfile::tempdir()?;"
        {
            continue;
        }
        joins += line.matches(r#"dir.path().join("app.persia")"#).count();
        lines.push(line.replace(r#"dir.path().join("app.persia")"#, r#""app.persia""#));
    }
    if joins == 1 {
        Ok(lines)
    } else {
        Err(format!(
            "it names `dir.path().join(\"app.persia\")` {joins} times, not once"
        ))
    }
}

/// The first line where `shown` and `want` differ, as a message, or `None` when they are equal.
fn difference(shown: &Fence<'_>, want: &[String]) -> Option<String> {
    let have: Vec<&str> = shown.lines.iter().map(|(_, l)| *l).collect();
    if have == want {
        return None;
    }
    let at = have
        .iter()
        .zip(want)
        .take_while(|(a, b)| **a == b.as_str())
        .count();
    let line = shown
        .lines
        .get(at)
        .map_or(shown.at + have.len() + 1, |(n, _)| *n);
    Some(format!(
        "line {line}: shows {:?}, the twin has {:?}",
        have.get(at).copied().unwrap_or("(end of block)"),
        want.get(at).map_or("(end of block)", String::as_str)
    ))
}

/// The pages `crates/persia-docs` runs, relative to the root.
fn doctested_pages() -> Vec<std::path::PathBuf> {
    let src = Path::new("crates/persia-docs/src");
    DOC_MODULES
        .iter()
        .flat_map(|module| {
            let text = read(module);
            pages(&text)
                .into_iter()
                .filter_map(|page| normalize(&src.join(page)))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn shown_samples_equal_a_doctest() {
    let run = doctested_pages();
    let mut found = Vec::new();
    for s in SHOWN {
        assert!(
            run.iter().any(|p| p == Path::new(s.twin)),
            "{}: its twin {} is no page crates/persia-docs runs ({DOC_MODULES:?})",
            s.page,
            s.twin
        );
        let page = read(s.page);
        let twin_page = read(s.twin);
        let shown_blocks = fences_after(&page, s.after, s.page);
        let twin_blocks = fences_after(&twin_page, s.twin_after, s.twin);
        let shown = shown_blocks
            .iter()
            .find(|f| first_word(f.info) == "rust")
            .unwrap_or_else(|| panic!("{}: no `rust` block after {:?}", s.page, s.after));
        let twin = twin_blocks
            .iter()
            .find(|f| doctest(f.info).rust)
            .unwrap_or_else(|| panic!("{}: no Rust block after {:?}", s.twin, s.twin_after));
        assert!(
            doctest(twin.info).runs,
            "{}:{}: the twin of {} does not run (`{}`)",
            s.twin,
            twin.at,
            s.page,
            twin.info
        );
        match as_shown(twin) {
            Err(why) => found.push(format!(
                "{}:{}: the twin of {}: {why}",
                s.twin, twin.at, s.page
            )),
            Ok(want) => found.extend(difference(shown, &want).map(|d| {
                format!(
                    "{}: the sample at line {} is not {}:{} ({d})",
                    s.page, shown.at, s.twin, twin.at
                )
            })),
        }
        // The dependency block before the sample, when the page shows one.
        let toml = |blocks: &[Fence<'_>]| -> Option<Vec<String>> {
            blocks
                .iter()
                .find(|f| first_word(f.info) == "toml")
                .map(|f| f.lines.iter().map(|(_, l)| (*l).to_owned()).collect())
        };
        let shown_toml = shown_blocks
            .iter()
            .position(|f| std::ptr::eq(f, shown))
            .and_then(|at| toml(&shown_blocks[..at]));
        if let Some(shown_toml) = shown_toml
            && toml(&fences(&twin_page)) != Some(shown_toml)
        {
            found.push(format!(
                "{}: the `toml` block before the sample is not {}'s",
                s.page, s.twin
            ));
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn twins_read_as_shown() {
    let block = |text: &'static str| fences(text).into_iter().next().unwrap();
    let twin = block(
        "```rust\nfn main() {\n#   let dir = tempfile::tempdir().unwrap();\n    let dir = tempfile::tempdir()?;\n    \
         let db = Db::open(dir.path().join(\"app.persia\"))?; // one file\n#\n    db.close()\n}\n```\n",
    );
    assert_eq!(
        as_shown(&twin).unwrap(),
        [
            "fn main() {",
            "    let db = Db::open(\"app.persia\")?; // one file",
            "    db.close()",
            "}"
        ]
    );
    // `##` is rustdoc's escape for a shown `#`, and `#[...]` an attribute: both kept.
    let kept = block("```rust\n##[x]\n#[test]\nlet a = dir.path().join(\"app.persia\");\n```\n");
    assert_eq!(as_shown(&kept).unwrap().len(), 3);
    // No temporary path, or two: refused.
    assert!(
        as_shown(&block(
            "```rust\nlet db = Db::open(\"app.persia\")?;\n```\n"
        ))
        .is_err()
    );
    let two = "```rust\nlet a = dir.path().join(\"app.persia\");\nlet b = dir.path().join(\"app.persia\");\n```\n";
    assert!(as_shown(&block(two)).is_err());
    // A difference names the shown line.
    let shown = block("text\n```rust\nfn main() {\n    db.comit()\n}\n```\n");
    let want = [
        "fn main() {".to_owned(),
        "    db.commit()".to_owned(),
        "}".to_owned(),
    ];
    let found = difference(&shown, &want).unwrap();
    assert!(found.starts_with("line 4: "), "{found}");
    assert_eq!(
        difference(&shown, &want[..1]).map(|d| d.starts_with("line 4: ")),
        Some(true)
    );
    assert_eq!(first_word("rust title=\"src/main.rs\""), "rust");
    assert_eq!(first_word("rust,ignore"), "rust");
}
