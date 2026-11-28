//! Samples a page shows that rustdoc cannot run as shown (ROADMAP 10.3.3, 10.1.51).
//!
//! The README's quickstart, which the landing page shows too, opens `app.persia` in the working directory, so a user
//! can paste it into a fresh `cargo new` with `persia` as the only dependency. Run as a doctest it would write that
//! file into the crate under test, and a second run would fail (`create_collection_json` refuses a collection that
//! exists); a doctest cannot move to a temporary working directory either (merged doctests share one process). So
//! each such sample has a twin that runs: a Rust block of a page `crates/persia-docs` runs as a doctest, in a
//! temporary directory. This check keeps the shown block equal to its twin, line for line, once the twin is read as
//! the page shows it:
//!
//! - rustdoc's reading of each line is taken: `##` after the indentation shows `#`, and a hidden line (`# ` after
//!   the indentation, or a lone `#`) is dropped;
//! - a hidden line may only be a comment (`# //...`, a lone `#`) or the one line binding `dir` to a temporary
//!   directory (`let dir = tempfile::tempdir()` then `?;`, `.unwrap();` or `.expect("...");`), which may also be
//!   shown in the twin and is dropped: any other hidden line is code the shown page lacks, so it is refused;
//! - `dir.path().join("app.persia")`, which must occur once, is written `"app.persia"`, and no other `dir` may be
//!   left (the shown page never makes it).
//!
//! Nothing else may differ: comments, assertions and calls are the twin's, so the shown sample calls what the twin
//! runs and asserts what it asserts. The twin must be a block rustdoc runs (no `ignore` or `no_run`) on a page
//! `crates/persia-docs/src/guides.rs` lists (`#[doc = include_str!("...")]`, [`pages`]), and the dependency block
//! (`toml`) the page shows before the sample must equal the twin page's first `toml` block, and must be there when
//! [`Shown::requires_toml`] says so: the sample pastes into a fresh crate only with it.

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
    /// The page must show a `toml` dependency block before the sample (one equal to the twin page's is checked
    /// whenever it is there).
    requires_toml: bool,
}

/// The getting-started guide's first sample, which that page calls the README quickstart in a temporary directory.
const GETTING_STARTED: &str = "website/src/content/docs/guide/getting-started.md";
const GETTING_STARTED_AFTER: &str = "## Open a database, index, and search";

/// Every shown sample with a twin. The README's and the landing page's are one program, so both have the
/// getting-started guide's first sample as their twin, and the three pages cannot drift apart.
const SHOWN: &[Shown] = &[
    Shown {
        page: "README.md",
        after: "## Quickstart",
        twin: GETTING_STARTED,
        twin_after: GETTING_STARTED_AFTER,
        requires_toml: true,
    },
    Shown {
        page: "website/src/content/docs/index.mdx",
        after: "",
        twin: GETTING_STARTED,
        twin_after: GETTING_STARTED_AFTER,
        requires_toml: true,
    },
];

/// The module of `crates/persia-docs` whose item docs are pages run as doctests: `src/guides.rs`, wired by
/// `#[cfg(doctest)] mod guides;` in its `lib.rs`.
const DOC_MODULE: &str = "guides";

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

/// Whether `code` (a line without its indentation) binds `dir` to a temporary directory:
/// `let dir = tempfile::tempdir()` and then `?;`, `.unwrap();` or `.expect("...");` with a plain message.
fn binds_dir(code: &str) -> bool {
    let Some(rest) = code.strip_prefix("let dir = tempfile::tempdir()") else {
        return false;
    };
    rest == "?;"
        || rest == ".unwrap();"
        || rest
            .strip_prefix(".expect(\"")
            .and_then(|message| message.strip_suffix("\");"))
            .is_some_and(|message| !message.contains(['"', '\\']))
}

/// Whether `line` names the identifier `dir` (a word of its own, not part of `directory` or `tmp_dir`).
fn names_dir(line: &str) -> bool {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    line.match_indices("dir").any(|(at, _)| {
        let (before, after) = (line.get(..at), line.get(at + 3..));
        !word(before.and_then(|s| s.chars().next_back()))
            && !word(after.and_then(|s| s.chars().next()))
    })
}

/// The twin's lines as the shown page writes them (the module docs' rules), or why they cannot be.
fn as_shown(twin: &Fence<'_>) -> Result<Vec<String>, String> {
    const JOIN: &str = r#"dir.path().join("app.persia")"#;
    let (mut joins, mut binds) = (0, 0);
    let mut why = Vec::new();
    let mut lines = Vec::new();
    for &(n, line) in &twin.lines {
        let trimmed = line.trim();
        // rustdoc's reading of a line (its `map_line`): `##` after the indentation is a shown `#`, `# ` and a lone
        // `#` start a hidden line.
        let (hidden, line) = if trimmed.starts_with("##") {
            (None, line.replacen("##", "#", 1))
        } else if trimmed == "#" {
            (Some(""), String::new())
        } else {
            (trimmed.strip_prefix("# ").map(str::trim), line.to_owned())
        };
        match hidden {
            Some(code) if code.is_empty() || code.starts_with("//") => {}
            Some(code) if binds_dir(code) => binds += 1,
            Some(code) => why.push(format!(
                "line {n}: the hidden line {code:?} is not on the shown page (only comments and the `dir` line may be hidden)"
            )),
            None if binds_dir(line.trim()) => binds += 1,
            None => {
                joins += line.matches(JOIN).count();
                let shown = line.replace(JOIN, r#""app.persia""#);
                if names_dir(&shown) {
                    why.push(format!(
                        "line {n}: `dir` is left once the path is written \"app.persia\", and the shown page never makes it"
                    ));
                }
                lines.push(shown);
            }
        }
    }
    if joins != 1 {
        why.push(format!("it names `{JOIN}` {joins} times, not once"));
    }
    if binds != 1 {
        why.push(format!(
            "it binds `dir` to `tempfile::tempdir()` {binds} times, not once"
        ));
    }
    if why.is_empty() {
        Ok(lines)
    } else {
        Err(why.join("; "))
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

/// The pages `crates/persia-docs` runs, relative to the root: those [`DOC_MODULE`] lists, which must be wired.
fn doctested_pages() -> Vec<std::path::PathBuf> {
    let src = Path::new("crates/persia-docs/src");
    let lib = read("crates/persia-docs/src/lib.rs");
    assert!(
        lib.contains(&format!("#[cfg(doctest)]\nmod {DOC_MODULE};")),
        "crates/persia-docs/src/lib.rs: `#[cfg(doctest)] mod {DOC_MODULE};` is missing, so its pages do not run"
    );
    let text = read(&format!("crates/persia-docs/src/{DOC_MODULE}.rs"));
    pages(&text)
        .into_iter()
        .filter_map(|page| normalize(&src.join(page)))
        .collect()
}

#[test]
fn shown_samples_equal_a_doctest() {
    let run = doctested_pages();
    let mut found = Vec::new();
    for s in SHOWN {
        assert!(
            run.iter().any(|p| p == Path::new(s.twin)),
            "{}: its twin {} is no page crates/persia-docs runs (src/{DOC_MODULE}.rs)",
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
        // The dependency block before the sample: there when the entry requires it, and the twin page's.
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
        match shown_toml {
            None if s.requires_toml => found.push(format!(
                "{}: no `toml` dependency block before the sample at line {}, so it does not paste into a fresh crate",
                s.page, shown.at
            )),
            Some(shown_toml) if toml(&fences(&twin_page)).as_ref() != Some(&shown_toml) => {
                found.push(format!(
                    "{}: the `toml` block before the sample is not {}'s",
                    s.page, s.twin
                ));
            }
            _ => {}
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn twins_read_as_shown() {
    let block = |text: &'static str| fences(text).into_iter().next().unwrap();
    let twin = block(
        "```rust\nfn main() {\n#   // test setup\n#   let dir = tempfile::tempdir().expect(\"a directory\");\n    \
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
    // The `dir` line may be shown in the twin; `##` is rustdoc's escape for a shown `#`, and `#[...]` an attribute.
    let kept = block(
        "```rust\n    let dir = tempfile::tempdir()?;\n    ##[x]\n#[test]\nlet a = dir.path().join(\"app.persia\");\n```\n",
    );
    assert_eq!(
        as_shown(&kept).unwrap(),
        ["    #[x]", "#[test]", "let a = \"app.persia\";"]
    );
    let refused = |text: &'static str, why: &str| {
        let found = as_shown(&block(text)).unwrap_err();
        assert!(found.contains(why), "{found}");
    };
    // Hidden code the shown page lacks: an import, a binding, a second `dir` line.
    refused(
        "```rust\n# use persia::Something;\nlet dir = tempfile::tempdir()?;\nlet db = Db::open(dir.path().join(\"app.persia\"))?;\n```\n",
        "line 2: the hidden line \"use persia::Something;\"",
    );
    refused(
        "```rust\nlet dir = tempfile::tempdir()?;\n#   let schema = 1;\nlet db = Db::open(dir.path().join(\"app.persia\"))?;\n```\n",
        "line 3: the hidden line \"let schema = 1;\"",
    );
    refused(
        "```rust\n# let dir = tempfile::tempdir()?;\nlet dir = tempfile::tempdir()?;\nlet db = Db::open(dir.path().join(\"app.persia\"))?;\n```\n",
        "binds `dir` to `tempfile::tempdir()` 2 times",
    );
    refused(
        "```rust\n# let dir = tempfile::tempdir().expect(\"a \\\" quote\");\nlet db = Db::open(dir.path().join(\"app.persia\"))?;\n```\n",
        "the hidden line",
    );
    // Another use of `dir` stays on the shown page, which never makes it; `directory` and `tmp_dir` are other words.
    refused(
        "```rust\nlet dir = tempfile::tempdir()?;\nlet db = Db::open(dir.path().join(\"app.persia\"))?;\nprintln!(\"{:?}\", dir.path());\n```\n",
        "line 4: `dir` is left",
    );
    assert!(!names_dir("let directory = tmp_dir; // dirs"));
    assert!(names_dir("drop(dir)"));
    // No temporary path, or two, or no `dir` line: refused.
    refused(
        "```rust\nlet dir = tempfile::tempdir()?;\nlet db = Db::open(\"app.persia\")?;\n```\n",
        "0 times",
    );
    let two = "```rust\nlet dir = tempfile::tempdir()?;\nlet a = dir.path().join(\"app.persia\");\nlet b = dir.path().join(\"app.persia\");\n```\n";
    refused(two, "2 times");
    refused(
        "```rust\nlet db = Db::open(dir.path().join(\"app.persia\"))?;\n```\n",
        "binds `dir` to `tempfile::tempdir()` 0 times",
    );
    assert!(binds_dir("let dir = tempfile::tempdir().unwrap();"));
    assert!(!binds_dir("let dir = tempfile::tempdir()?; let x = 1;"));
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
