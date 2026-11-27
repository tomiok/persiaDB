//! The website guides (ROADMAP 10.1.34, 10.1.43). The include paths of the published crates are `includes.rs`.
//!
//! **Guide samples are doctests of the public API.** `crates/persia-docs/src/guides.rs` runs the guides' Rust
//! samples as doctests (its module docs say with which features, and what the compiler cannot refuse there); this
//! check keeps it wired, keeps every page it lists present with a Rust sample, and refuses names a guide has
//! reached for outside the documented API. Pages are the `#[doc = include_str!("...")]` lines of `guides.rs`
//! outside comments ([`pages`]). A block is a sample when rustdoc collects it as a doctest ([`doctest`]): a port of
//! rustdoc 1.99's info-string tokenizer and its Rust rule, unit-tested against what that rustdoc did with each
//! string, so `no_run`, `ignore`, `should_panic`, `compile_fail`, `edition20xx` or `{.class}` blocks are samples
//! too, and an Expressive Code `rust title="..."` is not (rustdoc's tokenizer stops at the `=` and runs nothing).
//! A toolchain bump re-checks that table: put each string of `fences_are_rust_as_rustdoc_reads_them` over a block
//! and compare `rustdoc --test --test-args --list`. Indented code blocks (which rustdoc also runs) are not read
//! as samples; the guides use fences only.

// Test-only tooling: every string index here comes from `char_indices`, a `match_indices` hit or a count of ASCII
// characters, so it is on a char boundary.
#![allow(clippy::string_slice)]

use std::path::{Path, PathBuf};

pub(super) fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

pub(super) fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

// --- Guide samples (ROADMAP 10.1.34, 10.1.43) ---

/// The pages `guides.rs` runs: the path of each item doc `#[doc = include_str!("<path>")]`, one per line, outside
/// comments (a commented-out or `///`-quoted include names no page).
pub(super) fn pages(guides_rs: &str) -> Vec<&str> {
    let mut depth = 0usize;
    let mut found = Vec::new();
    for line in guides_rs.lines() {
        let trimmed = line.trim();
        let in_comment = depth > 0;
        // Block comments nest in Rust; a line that opens or closes one holds no include of its own here.
        depth += trimmed.matches("/*").count();
        depth = depth.saturating_sub(trimmed.matches("*/").count());
        if in_comment || trimmed.contains("/*") {
            continue;
        }
        if let Some(path) = trimmed
            .strip_prefix("#[doc = include_str!(\"")
            .and_then(|rest| rest.strip_suffix("\")]"))
        {
            found.push(path);
        }
    }
    found
}

/// A token of a fenced block's info string, as rustdoc's tokenizer (`TagIterator` in
/// `src/librustdoc/html/markdown.rs`) yields it.
#[derive(Debug, PartialEq, Eq)]
enum Tag<'a> {
    /// A word, or a quoted string outside an attribute block: `rust`, `no_run`, `json`.
    Word(&'a str),
    /// `key=value` inside `{...}`.
    KeyValue,
    /// `.class` inside `{...}`.
    Class,
    /// A `(...)` comment that is no part of a word.
    Comment,
}

fn is_leading(c: char) -> bool {
    c == '_' || c == '-' || c == ':' || c.is_ascii_alphanumeric()
}

fn is_bareword(c: char) -> bool {
    is_leading(c) || ".!#$%&*+/;<>?@^|~".contains(c)
}

fn is_separator(c: char) -> bool {
    c == ' ' || c == ',' || c == '\t'
}

/// rustdoc's tokens of `info`, or `None` where its tokenizer reports an error ("unexpected character `=`" for an
/// Expressive Code `title="..."` outside braces, an unclosed string, block or comment): rustdoc then runs no
/// doctest of the block. A port of rustdoc 1.99's tokenizer: words of bareword characters split by ` `, `,` or tab,
/// a quoted word, `(...)` comments skipped, and `{...}` attribute blocks of `.class` and `key=value` (either side
/// quoted); a word that runs into `{` is dropped, as rustdoc drops it.
fn info_tags(info: &str) -> Option<Vec<Tag<'_>>> {
    let mut t = Tokenizer {
        info,
        chars: info.char_indices().peekable(),
    };
    let mut in_block = false;
    let mut tags = Vec::new();
    loop {
        while t.chars.next_if(|&(_, c)| is_separator(c)).is_some() {}
        let Some(&(start, _)) = t.chars.peek() else {
            return (!in_block).then_some(tags);
        };
        if in_block {
            match t.block_tag()? {
                Step::Tag(tag) => tags.push(tag),
                Step::Block => in_block = false,
            }
        } else {
            match t.word_tag(start)? {
                Step::Tag(tag) => tags.push(tag),
                Step::Block => in_block = true,
            }
        }
    }
}

/// What one step of [`info_tags`] read.
enum Step<'a> {
    /// A tag.
    Tag(Tag<'a>),
    /// A `{` or a `}`: into or out of an attribute block.
    Block,
}

/// The state of [`info_tags`]: the info string and where in it the tokenizer is.
struct Tokenizer<'a> {
    info: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
}

impl<'a> Tokenizer<'a> {
    /// A quoted string from just past its opening `"` at `open`: its text, its closing quote consumed.
    fn quoted(&mut self, open: usize) -> Option<&'a str> {
        let (close, _) = self.chars.by_ref().find(|&(_, c)| c == '"')?;
        Some(&self.info[open + 1..close])
    }

    /// The word of the consumed character at `start` and the bareword characters after it, or `None` at the end.
    fn word(&mut self, start: usize) -> Option<&'a str> {
        loop {
            match self.chars.peek() {
                Some(&(pos, c)) if !is_bareword(c) => return Some(&self.info[start..pos]),
                Some(_) => {
                    self.chars.next();
                }
                None => return None,
            }
        }
    }

    /// After a class or a key=value: a `}`, a separator, a `(` or the end.
    fn ends_cleanly(&mut self) -> bool {
        self.chars
            .peek()
            .is_none_or(|&(_, c)| c == '}' || c == '(' || is_separator(c))
    }

    /// The next tag inside `{...}`: [`Step::Block`] at its `}`, `None` on an error.
    fn block_tag(&mut self) -> Option<Step<'a>> {
        let (pos, c) = self.chars.next()?;
        if c == '}' {
            return Some(Step::Block);
        }
        if c == '.' {
            let class = self.word(pos).unwrap_or(&self.info[pos..]);
            return (class.len() > 1 && self.ends_cleanly()).then_some(Step::Tag(Tag::Class));
        }
        if c != '"' && !is_leading(c) {
            return None;
        }
        let key = if c == '"' {
            self.quoted(pos)?
        } else {
            self.word(pos)?
        };
        if key.is_empty() || self.chars.next()?.1 != '=' {
            return None;
        }
        let value = match self.chars.next()? {
            (open, '"') => self.quoted(open)?,
            (at, c) if is_bareword(c) => self.word(at)?,
            _ => return None,
        };
        (!value.is_empty() && self.ends_cleanly()).then_some(Step::Tag(Tag::KeyValue))
    }

    /// The word at `start`, outside `{...}`: [`Step::Block`] when it runs into a `{` (which opens a block and drops
    /// the word), `None` on an error.
    fn word_tag(&mut self, start: usize) -> Option<Step<'a>> {
        loop {
            match self.chars.next() {
                None => return Some(Step::Tag(Tag::Word(&self.info[start..]))),
                Some((pos, '"')) => {
                    if pos != start {
                        return None;
                    }
                    let text = self.quoted(pos)?;
                    let next_ok = self
                        .chars
                        .peek()
                        .is_none_or(|&(_, c)| c == '{' || c == '(' || is_separator(c));
                    return next_ok.then_some(Step::Tag(Tag::Word(text)));
                }
                Some((_, '{')) => return Some(Step::Block),
                Some((pos, c)) if is_separator(c) => {
                    return Some(Step::Tag(Tag::Word(&self.info[start..pos])));
                }
                Some((pos, '(')) => {
                    self.chars.by_ref().find(|&(_, c)| c == ')')?;
                    // `(comment)` alone is no word; the loop of `info_tags` goes on.
                    let word = if pos == start {
                        Tag::Comment
                    } else {
                        Tag::Word(&self.info[start..pos])
                    };
                    return Some(Step::Tag(word));
                }
                Some((pos, c))
                    if (pos == start && is_leading(c)) || (pos != start && is_bareword(c)) => {}
                Some(_) => return None,
            }
        }
    }
}

/// How rustdoc treats a fenced block, from its info string (`LangString::parse` in rustdoc 1.99, stable: no
/// `custom` classes, no error-code check, no per-target ignores).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) struct Doctest {
    /// rustdoc collects it as a doctest: compiled unless `ignore`, run unless `no_run` or `compile_fail`.
    pub(super) rust: bool,
    /// Collected and run: `rust` and none of `ignore`, `ignore-*`, `no_run`, `compile_fail`.
    pub(super) runs: bool,
}

/// rustdoc's reading of a fence's info string: Rust when it has no word (empty, or only `{...}` attributes), or
/// when a `rust` word, or a rustdoc attribute word seen before any other word (`no_run`, `ignore`, `ignore-*`,
/// `should_panic`, `compile_fail`, `test_harness`, `standalone_crate`), is among its words; never when the
/// tokenizer fails. `edition*` words and `rust2021` and the like are neutral (an edition sets the doctest's edition
/// and makes nothing Rust: `edition2021,json` is not collected), `E0425` is another word on stable.
pub(super) fn doctest(info: &str) -> Doctest {
    let Some(tags) = info_tags(info) else {
        return Doctest {
            rust: false,
            runs: false,
        };
    };
    let (mut seen_rust, mut seen_other, mut runs) = (false, false, true);
    for tag in tags {
        let Tag::Word(word) = tag else { continue };
        match word {
            "should_panic" => seen_rust = !seen_other,
            "no_run" | "ignore" => {
                seen_rust = !seen_other;
                runs = false;
            }
            "rust" => seen_rust = true,
            "test_harness" | "standalone_crate" => seen_rust = !seen_other || seen_rust,
            "compile_fail" => {
                seen_rust = !seen_other || seen_rust;
                runs = false;
            }
            // Per-target ignores are unstable: rustdoc reads the word and does nothing with it. Not counted as
            // running, in case a toolchain turns them on.
            w if w.starts_with("ignore-") => runs = false,
            // Sets the edition only: rustdoc 1.99 collects no `edition2021,json` block.
            w if w.starts_with("edition") => {}
            w if w
                .strip_prefix("rust")
                .is_some_and(|e| ["2015", "2018", "2021", "2024"].contains(&e)) => {}
            _ => seen_other = true,
        }
    }
    let rust = !seen_other || seen_rust;
    Doctest {
        rust,
        runs: rust && runs,
    }
}

/// A fenced block of a Markdown page.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Fence<'a> {
    /// Its info string, trimmed.
    pub(super) info: &'a str,
    /// The line number of its opening fence.
    pub(super) at: usize,
    /// Its lines, with their numbers.
    pub(super) lines: Vec<(usize, &'a str)>,
}

/// The fenced blocks of a Markdown page. A fence is three or more backticks or tildes, at any indentation (list
/// items indent theirs); it closes with at least as many of the same character and nothing else on the line, and a
/// backtick fence's info string holds no backtick, as the Markdown spec has it.
pub(super) fn fences(text: &str) -> Vec<Fence<'_>> {
    let mut blocks = Vec::new();
    // The open fence's character and length, and its block so far.
    let mut open: Option<(char, usize, Fence<'_>)> = None;
    for (n, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        // The fence characters are ASCII, so their count is a byte offset.
        let run = |c: char| trimmed.chars().take_while(|&x| x == c).count();
        if let Some((c, len, block)) = &mut open {
            let closing = run(*c);
            if closing >= *len
                && trimmed
                    .get(closing..)
                    .is_some_and(|rest| rest.trim().is_empty())
            {
                blocks.extend(open.take().map(|(_, _, block)| block));
            } else {
                block.lines.push((n + 1, line));
            }
            continue;
        }
        for c in ['`', '~'] {
            let len = run(c);
            let info = trimmed.get(len..).unwrap_or_default().trim();
            if len >= 3 && !(c == '`' && info.contains('`')) {
                let block = Fence {
                    info,
                    at: n + 1,
                    lines: Vec::new(),
                };
                open = Some((c, len, block));
                break;
            }
        }
    }
    blocks
}

/// The website guides `crates/persia-docs/src/guides.rs` runs as doctests are wired (`mod guides` under
/// `#[cfg(doctest)]` in that crate's `lib.rs`, every listed page present, each with a Rust sample), and their samples
/// do not name what a guide has reached for outside the documented API. `cargo test --workspace` links the doctests
/// to a `persia` with `internal` on (feature unification, see `guides.rs`), and every `#[doc(hidden)]` item
/// (`Filter::from_json`, which takes the engine's `QueryLimits`) compiles in a sample: rustdoc alone does not catch
/// them. A block is a sample when rustdoc reads it as Rust ([`doctest`]), whatever its first word.
#[test]
fn guide_samples_are_doctests_of_the_public_api() {
    let lib = read("crates/persia-docs/src/lib.rs");
    assert!(
        lib.contains("#[cfg(doctest)]\nmod guides;"),
        "crates/persia-docs/src/lib.rs: `#[cfg(doctest)] mod guides;` is missing, so no guide sample runs"
    );
    let guides = read("crates/persia-docs/src/guides.rs");
    let pages = pages(&guides);
    assert!(
        !pages.is_empty(),
        "crates/persia-docs/src/guides.rs: no page"
    );
    for page in pages {
        let text = std::fs::read_to_string(root().join("crates/persia-docs/src").join(page))
            .unwrap_or_else(|e| panic!("guides.rs lists {page}, which cannot be read: {e}"));
        let mut samples = 0;
        for block in fences(&text) {
            if !doctest(block.info).rust {
                continue;
            }
            samples += 1;
            for (n, line) in block.lines {
                for refused in [
                    "persia_engine",
                    "persia::internal",
                    "QueryLimits",
                    "Filter::from_json",
                ] {
                    assert!(
                        !line.contains(refused),
                        "{page}:{n}: a guide sample names `{refused}`, which is not the documented API"
                    );
                }
            }
        }
        assert!(
            samples > 0,
            "{page}: no Rust sample, so guides.rs checks nothing of it"
        );
    }
}

/// Each info string with what rustdoc 1.99 did with a block under it (`rustdoc --test --test-args --list` over one
/// block per string, 2026-11-27): collected as a doctest or not.
#[test]
fn fences_are_rust_as_rustdoc_reads_them() {
    let rust = [
        "",
        "rust",
        "no_run",
        "ignore",
        "rust,ignore",
        "ignore-windows",
        "ignore-",
        "should_panic",
        "compile_fail",
        "compile_fail,E0425",
        "edition2021",
        "edition2024",
        "edition2021,rust",
        "test_harness,json",
        "compile_fail,json",
        "standalone_crate,json",
        "test_harness",
        "standalone_crate",
        " rust ",
        "rust,",
        "rust\tno_run",
        "rust2021",
        "no_run,json",
        "rust,json",
        "json,rust",
        "\"rust\"",
        "no_run,\"json\"",
        "rust (a comment)",
        "{.rust}",
        "{class=\"rust\"}",
        "{title=\"a b\" .c}",
        "{\"k\"=v}",
        "rust {title=\"x\"}",
        "no_run {k=v}",
        "rust{.x}",
        // The word before `{` is dropped.
        "json{.x}",
    ];
    let not_rust = [
        "json",
        "text",
        "toml",
        "sh",
        "Rust",
        "rs",
        "wrap",
        "custom",
        "json,no_run",
        "json (x) no_run",
        "json {.x} no_run",
        "E0425,compile_fail",
        "edition2021,json",
        "edition2024,json",
        "\"json\"",
        "\"json\",no_run",
        "{k=\"v\"}x",
        "rust;no_run",
        // Tokenizer errors: Expressive Code's `key=value` outside braces, unclosed parts, a stray character.
        "rust title=\"a b\"",
        "title=\"src/main.rs\"",
        "rust title=\"src/main.rs\"",
        "rust,no_run title=\"x y\"",
        "title=\"x y\" rust",
        "no_run title=\"x\"",
        "rust ins={1-3}",
        "mark=3",
        "frame=\"none\"",
        ".rust",
        "rust {.x",
        "rust (unclosed",
        "{=x}",
    ];
    for info in rust {
        assert!(doctest(info).rust, "{info:?} is Rust to rustdoc");
    }
    for info in not_rust {
        assert!(!doctest(info).rust, "{info:?} is not Rust to rustdoc");
    }
    for (info, runs) in [
        ("", true),
        ("rust", true),
        ("should_panic", true),
        ("edition2024", true),
        ("no_run", false),
        ("rust,ignore", false),
        ("ignore-windows", false),
        ("compile_fail", false),
        ("json", false),
    ] {
        assert_eq!(doctest(info).runs, runs, "{info:?}");
    }
}

#[test]
fn fences_open_and_close_as_commonmark() {
    let page = "text\n```rust\nlet a = 1;\n```\n\n  ~~~~ json\n  ```\n  ~~~\n  ~~~~~\n````no_run\n```\n````\n```\nx\n```  \n";
    let blocks: Vec<(&str, usize, Vec<&str>)> = fences(page)
        .into_iter()
        .map(|b| (b.info, b.at, b.lines.into_iter().map(|(_, l)| l).collect()))
        .collect();
    assert_eq!(
        blocks,
        [
            ("rust", 2, vec!["let a = 1;"]),
            ("json", 6, vec!["  ```", "  ~~~"]),
            ("no_run", 10, vec!["```"]),
            ("", 13, vec!["x"]),
        ]
    );
}

#[test]
fn pages_are_item_docs_outside_comments() {
    let guides = r#"
/// `a.md`, also `#[doc = include_str!("quoted.md")]` in a doc comment.
#[doc = include_str!("../../../website/a.md")]
pub struct A;
// #[doc = include_str!("../../../website/commented.md")]
let x = include_str!("../../../website/not-a-doc.md");
/* #[doc = include_str!("../../../website/block.md")]
   #[doc = include_str!("../../../website/block-2.md")]
   /* nested */
   #[doc = include_str!("../../../website/block-3.md")]
*/
    #[doc = include_str!("../../../website/b.md")]
pub struct B;
"#;
    assert_eq!(
        pages(guides),
        ["../../../website/a.md", "../../../website/b.md"]
    );
}
