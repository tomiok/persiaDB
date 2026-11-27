//! The website guides name only `persia`'s public API (ROADMAP 10.1.41). `crates/persia/public-api.txt` (the
//! committed public-API snapshot, ADR-0063) is the source of truth; every page under `website/src/content/docs/guide/`
//! is read, and the site's other pages (`website/src/content/docs/*.md(x)`: the landing page, features,
//! architecture, ...) by every rule but the sample one, outside the regions marked as engine internals:
//!
//! - **Paths.** Every path of two or more segments that starts with `persia::` (`use persia::{..}` groups
//!   expanded, across lines) names an item the snapshot lists: the path of one of its `pub` lines (an item, a
//!   method, a field, a variant, a constant), or `persia::T::m` for a method of a standard trait the snapshot lists
//!   an `impl` of for `persia::T` ([`TRAIT_METHODS`]: `default`, `from_str`, `clone`, `from`, `to_string`), or a
//!   `persia::T::*` glob of an item it lists.
//! - **Paths by type name.** A path whose first segment is a type `persia` exports (`Db::open`,
//!   `Collection::search`, `Filter::from_json`), not after `::`, is read as `persia::` plus that path and checked the
//!   same way, in prose, inline code and code blocks alike. `serde_json::Value::String` is another crate's path (it
//!   starts after `::`); `Vec::new` names no `persia` type and is not checked.
//! - **No engine names.** No `persia_engine`, `persia_blob`, `persia_format`, `persia_storage` or `persia_analysis`
//!   word anywhere, and none of [`ENGINE_ONLY`] (ADR-0063 item 3's never-public types, and the engine types guides
//!   used to call) as a word of code: an inline code span or a line of a fenced block. Prose may say "segment",
//!   "manifest" or "database" as SPEC §2's domain terms.
//! - **Samples run.** A fenced block the site shows as Rust (Expressive Code's language, the first word of the info
//!   string, is `rust` or `rs`) is on a page `crates/persia-docs/src/guides.rs` runs, and on such a page every
//!   block rustdoc reads as Rust is one rustdoc runs (no `ignore`, `no_run`, `compile_fail`, and no
//!   `rust title="..."`, which rustdoc does not run at all: [`super::guides`]).
//!
//! **Engine internals.** A page that describes the engine on purpose (the architecture, the file format, how
//! ranking works inside) marks each such part, and only it: in Markdown between `<!-- engine-internals -->` and
//! `<!-- /engine-internals -->`, in MDX (which has no HTML comments) between `{/* engine-internals */}` and
//! `{/* /engine-internals */}`, each marker alone on its line. The first non-blank line after the opening marker
//! must say so in prose (it contains "internal"), so a reader is told that the names in it are not API to call.
//! Inside a marked part the engine-crate and engine-type word rules and the paths by type name are skipped; a
//! `persia::` path is still checked against the snapshot, except `persia::internal` and the paths under it (the
//! hidden layer, named there as an internal). The sample rule holds everywhere. A marker of the other form, an
//! unclosed or nested part, or a closing marker without an opening one fails. Status sentences (`%%TOKEN%%`, from
//! `website/src/status.mjs`) are checked by `website/src/status.test.mjs`, not here.
//!
//! **Reading.** A path may start with `::` and have blanks around `::`; inline code is every backtick span (of any
//! run length) and every `<code>...</code>`. Known limits: a renamed import (`use persia as p; p::internal`) and a
//! method named on a value (`page.next_cursor()`) are not resolved, as the check does not type the text; in a
//! sample rustdoc compiling it refuses both where they name nothing public (`persia-docs` run alone), and outside
//! samples review catches them.

// Test-only tooling: every string index here is the position of an ASCII byte (an identifier character, `:`, `{`,
// `}`, `,` or a backtick), so it is on a char boundary.
#![allow(clippy::string_slice)]

use std::collections::{BTreeMap, BTreeSet};

use super::guides::{doctest, fences, pages, read, root};

const GUIDE_DIR: &str = "website/src/content/docs/guide";

/// The site's other pages (the landing page, features, architecture, ...), read by the same rules.
const DOCS_DIR: &str = "website/src/content/docs";

/// The lower crates, as path roots: never API a guide calls (ADR-0063: `persia` is the facade).
const ENGINE_CRATES: &[&str] = &[
    "persia_engine",
    "persia_blob",
    "persia_format",
    "persia_storage",
    "persia_analysis",
];

/// Engine types that are no part of `persia`'s surface, as words of code: ADR-0063 item 3's never-public list, and
/// the engine types and test doubles guides named before the facade (ROADMAP 10.1.34, 10.1.41).
const ENGINE_ONLY: &[&str] = &[
    "Database",
    "DatabaseOptions",
    "Snapshot",
    "Segment",
    "Container",
    "Manifest",
    "SearchPlan",
    "Execution",
    "QueryRequest",
    "QueryResponse",
    "QueryCompiler",
    "QueryLimits",
    "RelaxPlan",
    "CollectionViews",
    "CollectionSnapshot",
    "TypedSortValue",
    "BlobDatabase",
    "BlobStore",
    "PackStore",
    "SimFile",
    "SimFs",
    "FaultyStorage",
    // Engine internals the 10.1.41 rewrite took out of the API parts of the guides.
    "TermScorer",
    "SegmentStatistics",
    "StatisticsProvider",
    "SearchStatistics",
    "AnalyzerRegistry",
    "ReadOnlyDatabase",
    "ReadOnlyBlobDatabase",
    "RemoteBlobStore",
    "MemoryBlobStore",
    "SearchHit",
    "RelaxedHit",
    "RelaxedResults",
    "CommitPolicy",
    "FlushPolicy",
    "CompactionOptions",
    "CancelToken",
    "GcOptions",
    "GcReport",
    "SweepReport",
    "PinSet",
    "WalRecord",
    "WalChunk",
    "DatabaseError",
    "QueryError",
];

/// A standard trait's method a path may name on a `persia` type, with the trait path the snapshot's `impl` line
/// starts with.
const TRAIT_METHODS: &[(&str, &str)] = &[
    ("default", "core::default::Default"),
    ("from_str", "core::str::traits::FromStr"),
    ("clone", "core::clone::Clone"),
    ("from", "core::convert::From"),
    ("to_string", "core::fmt::Display"),
];

/// What the public-API snapshot lists.
struct Api {
    /// The path of every `pub` line: `persia::Db`, `persia::Db::open`, `persia::Hit::document`.
    items: BTreeSet<String>,
    /// The types: `Db` for `pub struct persia::Db`.
    types: BTreeSet<String>,
    /// The traits implemented for each type: `Db` to `core::fmt::Debug`, ...
    impls: BTreeMap<String, Vec<String>>,
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The `persia::...` path at the start of `text`, up to the first character no path holds.
fn leading_path(text: &str) -> &str {
    let end = text
        .bytes()
        .position(|b| !is_ident(b) && b != b':')
        .unwrap_or(text.len());
    text[..end].trim_end_matches(':')
}

impl Api {
    fn parse(snapshot: &str) -> Self {
        let mut api = Api {
            items: BTreeSet::new(),
            types: BTreeSet::new(),
            impls: BTreeMap::new(),
        };
        for line in snapshot.lines() {
            if line.starts_with("# ") {
                continue;
            }
            if let Some(rest) = line.strip_prefix("impl") {
                // `impl[<..>] [!]Trait[<..>] for persia::T[<..>]`
                let Some((head, ty)) = rest.rsplit_once(" for persia::") else {
                    continue;
                };
                // Generics first, then the trait; a negative impl (`!Sync`) implements nothing.
                let trait_ = head.trim_start_matches(|c| c != ' ').trim();
                if trait_.starts_with('!') {
                    continue;
                }
                api.impls
                    .entry(leading_path(ty).to_owned())
                    .or_default()
                    .push(trait_.to_owned());
                continue;
            }
            let decl = line.trim_start_matches("#[non_exhaustive] ");
            let Some(at) = decl.find("persia::") else {
                continue;
            };
            let path = leading_path(&decl[at..]);
            if ["pub struct ", "pub enum ", "pub type "]
                .iter()
                .any(|kind| decl.starts_with(kind))
                && let Some(name) = path.strip_prefix("persia::")
            {
                api.types.insert(name.to_owned());
            }
            api.items.insert(path.to_owned());
        }
        api
    }

    /// Whether `path` (`persia::...`) names a listed item.
    fn has(&self, path: &str) -> bool {
        if let Some(prefix) = path.strip_suffix("::*") {
            let prefix = format!("{prefix}::");
            return self.items.iter().any(|item| item.starts_with(&prefix));
        }
        if self.items.contains(path) {
            return true;
        }
        let Some((ty, method)) = path
            .strip_prefix("persia::")
            .and_then(|rest| rest.split_once("::"))
        else {
            return false;
        };
        TRAIT_METHODS.iter().any(|(name, trait_)| {
            *name == method
                && self
                    .impls
                    .get(ty)
                    .is_some_and(|traits| traits.iter().any(|t| t.starts_with(trait_)))
        })
    }
}

/// The paths of `text` with two or more segments, each with the byte offset it starts at: a path starts at an
/// identifier that follows no identifier character and no `a::` (a leading `::`, as in `::persia::internal`, starts
/// one), `a::{b, c::{d, self}, e as f}` groups expand, and spaces or tabs around `::` (`persia :: internal`) are
/// the same path.
fn paths(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // `i` is an ASCII byte here, so `text[..i]` ends on a char boundary.
        let continues = || {
            text[..i]
                .trim_end_matches([' ', '\t'])
                .strip_suffix("::")
                .is_some_and(|rest| rest.bytes().next_back().is_some_and(is_ident))
        };
        let starts = (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
            && (i == 0 || !is_ident(bytes[i - 1]))
            && !continues();
        if !starts {
            i += 1;
            continue;
        }
        let (expanded, end) = read_path(text, i);
        found.extend(
            expanded
                .into_iter()
                .filter(|p| p.contains("::"))
                .map(|p| (i, p)),
        );
        i = end.max(i + 1);
    }
    found
}

/// The path that starts at `start` (an identifier), expanded, and the offset just past it.
fn read_path(text: &str, start: usize) -> (Vec<String>, usize) {
    let bytes = text.as_bytes();
    let ident_end = |from: usize| {
        (from..bytes.len())
            .find(|&j| !is_ident(bytes[j]))
            .unwrap_or(bytes.len())
    };
    let blank_end = |from: usize| {
        (from..bytes.len())
            .find(|&j| bytes[j] != b' ' && bytes[j] != b'\t')
            .unwrap_or(bytes.len())
    };
    let mut end = ident_end(start);
    let mut prefix = text[start..end].to_owned();
    while text[blank_end(end)..].starts_with("::") {
        let next = blank_end(blank_end(end) + 2);
        match bytes.get(next) {
            Some(&b) if b.is_ascii_alphabetic() || b == b'_' => {
                let to = ident_end(next);
                prefix = format!("{prefix}::{}", &text[next..to]);
                end = to;
            }
            Some(b'*') => return (vec![format!("{prefix}::*")], next + 1),
            Some(b'{') => {
                let mut depth = 0usize;
                let close = (next..bytes.len()).find(|&j| {
                    match bytes[j] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                });
                let Some(close) = close else {
                    return (vec![prefix], next);
                };
                return (group(&prefix, &text[next + 1..close]), close + 1);
            }
            _ => break,
        }
    }
    (vec![prefix], end)
}

/// The paths of the group `inner` (the text between `{` and `}`) under `prefix`.
fn group(prefix: &str, inner: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let (mut depth, mut from) = (0usize, 0);
    for (j, b) in inner.bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&inner[from..j]);
                from = j + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[from..]);
    let mut out = Vec::new();
    for part in parts {
        let part = part.trim();
        let part = part
            .split_once(" as ")
            .map_or(part, |(path, _)| path.trim());
        match part {
            "" => {}
            "self" => out.push(prefix.to_owned()),
            "*" => out.push(format!("{prefix}::*")),
            _ if part.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') => {
                let (sub, _) = read_path(part, 0);
                out.extend(sub.into_iter().map(|s| format!("{prefix}::{s}")));
            }
            _ => {}
        }
    }
    out
}

/// The words of `code`: its runs of identifier characters.
fn words(code: &str) -> impl Iterator<Item = &str> {
    code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

/// The inline code of one line of Markdown: each span between a run of backticks and the next run of the same
/// length (`` ``Database`` `` as well as `` `Database` ``), and the text of each `<code>...</code>`. A span that
/// does not close on its line is not read.
fn inline_code(line: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let run = rest[open..].bytes().take_while(|&b| b == b'`').count();
        let after = &rest[open + run..];
        let fence = &rest[open..open + run];
        // The closing run: the same number of backticks, not part of a longer run.
        let close = after.match_indices(fence).map(|(at, _)| at).find(|&at| {
            !after[at + run..].starts_with('`') && (at == 0 || !after[..at].ends_with('`'))
        });
        let Some(close) = close else { break };
        spans.push(&after[..close]);
        rest = &after[close + run..];
    }
    let mut rest = line;
    while let Some((_, tail)) = rest.split_once("<code>") {
        let Some((code, after)) = tail.split_once("</code>") else {
            break;
        };
        spans.push(code);
        rest = after;
    }
    spans
}

/// The lines (1-based) outside engine-internals parts, or the marker errors.
fn checked_lines(page: &str, text: &str) -> Result<Vec<bool>, Vec<String>> {
    let mdx = std::path::Path::new(page)
        .extension()
        .is_some_and(|e| e == "mdx");
    let (open, close, other_open, other_close) = if mdx {
        (
            "{/* engine-internals */}",
            "{/* /engine-internals */}",
            "<!-- engine-internals -->",
            "<!-- /engine-internals -->",
        )
    } else {
        (
            "<!-- engine-internals -->",
            "<!-- /engine-internals -->",
            "{/* engine-internals */}",
            "{/* /engine-internals */}",
        )
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut checked = vec![true; lines.len() + 1];
    let mut errors = Vec::new();
    let mut inside: Option<usize> = None;
    for (n, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let number = n + 1;
        if trimmed == other_open || trimmed == other_close {
            errors.push(format!(
                "{page}:{number}: `{trimmed}` is the other file type's marker; this page uses `{open}`"
            ));
        } else if trimmed == open {
            if inside.is_some() {
                errors.push(format!(
                    "{page}:{number}: engine-internals parts do not nest"
                ));
            }
            inside = Some(number);
            let says = lines[n + 1..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| l.to_lowercase().contains("internal"));
            if !says {
                errors.push(format!(
                    "{page}:{number}: the first line of an engine-internals part must say it describes internals"
                ));
            }
        } else if trimmed == close {
            if inside.take().is_none() {
                errors.push(format!(
                    "{page}:{number}: `{close}` closes no engine-internals part"
                ));
            }
            checked[number] = false;
        }
        if inside.is_some() {
            checked[number] = false;
        }
    }
    if let Some(at) = inside {
        errors.push(format!(
            "{page}:{at}: an engine-internals part that does not close"
        ));
    }
    if errors.is_empty() {
        Ok(checked)
    } else {
        Err(errors)
    }
}

/// What is wrong with the page `page` (its file name) of text `text`, against `api`; `runs` is whether
/// `guides.rs` runs its samples, `None` for a page outside the guides, to which the sample rule does not apply.
fn page_violations(page: &str, text: &str, api: &Api, runs: Option<bool>) -> Vec<String> {
    let mut found = Vec::new();
    let blocks = fences(text);
    // Samples (checked inside engine-internals parts too), on the guide pages.
    for block in blocks.iter().filter(|_| runs.is_some()) {
        let runs = runs == Some(true);
        let shown = block
            .info
            .split_whitespace()
            .next()
            .map(str::to_lowercase)
            .is_some_and(|lang| lang == "rust" || lang == "rs");
        let reading = doctest(block.info);
        let at = block.at;
        if shown && !runs {
            found.push(format!(
                "{page}:{at}: a Rust sample on a page crates/persia-docs/src/guides.rs does not run"
            ));
        } else if runs && (shown || reading.rust) && !reading.runs {
            found.push(format!(
                "{page}:{at}: `{}`: a sample rustdoc does not run (whole programs, no ignore, no_run, \
                 compile_fail or title=)",
                block.info
            ));
        }
    }
    let checked = match checked_lines(page, text) {
        Ok(checked) => checked,
        Err(errors) => return [found, errors].concat(),
    };
    let is_checked = |line: usize| checked.get(line).copied().unwrap_or(true);
    let line_of = |offset: usize| text[..offset].matches('\n').count() + 1;
    // Paths, by `persia::` or by a `persia` type's name.
    // Inside an engine-internals part only a `persia::` path is checked, and `persia::internal` is the one root it
    // may name (the hidden layer, described as an internal there).
    for (offset, path) in paths(text) {
        let line = line_of(offset);
        let first = path.split("::").next().unwrap_or_default();
        let internal = path == "persia::internal" || path.starts_with("persia::internal::");
        if !is_checked(line) && (first != "persia" || internal) {
            continue;
        }
        let full = if first == "persia" {
            path.clone()
        } else if api.types.contains(first) {
            format!("persia::{path}")
        } else {
            continue;
        };
        if !api.has(&full) {
            found.push(format!(
                "{page}:{line}: `{path}` is not in crates/persia/public-api.txt"
            ));
        }
    }
    // Engine names: crate roots anywhere, engine types as words of code.
    let in_block: BTreeSet<usize> = blocks
        .iter()
        .flat_map(|b| b.lines.iter().map(|(n, _)| *n))
        .collect();
    for (n, line) in text.lines().enumerate() {
        let number = n + 1;
        if !is_checked(number) {
            continue;
        }
        for word in words(line).filter(|w| ENGINE_CRATES.contains(w)) {
            found.push(format!(
                "{page}:{number}: `{word}` is an engine crate, not the API a guide calls"
            ));
        }
        let code: Vec<&str> = if in_block.contains(&number) {
            vec![line]
        } else {
            inline_code(line)
        };
        for word in code
            .into_iter()
            .flat_map(words)
            .filter(|w| ENGINE_ONLY.contains(w))
        {
            found.push(format!(
                "{page}:{number}: `{word}` is an engine type, not in crates/persia/public-api.txt"
            ));
        }
    }
    found
}

/// Every guide page names only the public API, and every Rust sample of the guides runs as a doctest.
#[test]
fn guides_name_only_the_public_api() {
    let api = Api::parse(&read("crates/persia/public-api.txt"));
    assert!(
        api.items.contains("persia::Db::open") && api.types.contains("Collection"),
        "crates/persia/public-api.txt: not read"
    );
    let guides = read("crates/persia-docs/src/guides.rs");
    let running: BTreeSet<&str> = pages(&guides)
        .into_iter()
        .filter_map(|p| p.strip_prefix("../../../"))
        .collect();
    let pages_of = |dir: &str| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root().join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| {
                std::path::Path::new(n)
                    .extension()
                    .is_some_and(|e| e == "md" || e == "mdx")
            })
            .map(|n| format!("{dir}/{n}"))
            .collect();
        names.sort();
        names
    };
    let guide_pages = pages_of(GUIDE_DIR);
    assert!(guide_pages.len() > 20, "{GUIDE_DIR}: {guide_pages:?}");
    let found: Vec<String> = guide_pages
        .iter()
        .chain(&pages_of(DOCS_DIR))
        .flat_map(|path| {
            let text = read(path);
            let name = path
                .strip_prefix(GUIDE_DIR)
                .map_or(path.as_str(), |n| n.trim_start_matches('/'));
            let guide = path.starts_with(GUIDE_DIR);
            page_violations(
                name,
                &text,
                &api,
                guide.then(|| running.contains(path.as_str())),
            )
        })
        .collect();
    assert!(
        found.is_empty(),
        "{} found:\n{}",
        found.len(),
        found.join("\n")
    );
}

const SNAPSHOT: &str = "\
# a comment naming persia::Nothing
#[non_exhaustive] pub struct persia::Hit
pub enum persia::Blob<'r>
pub struct persia::Db
pub type persia::Result<T> = core::result::Result<T, persia::Error>
pub fn persia::Db::open(location: impl core::convert::AsRef<std::path::Path>) -> persia::Result<Self>
pub field persia::Hit::score: f32
pub variant persia::Blob::Stored(persia::BlobRef)
pub const persia::BlobRef::MAX_CHUNK_SIZE: u32
pub fn persia::blob_ref(blob_id: persia::BlobId) -> persia::Result<persia::BlobRef>
impl core::default::Default for persia::Db
impl<'r> !core::marker::Sync for persia::Blob<'r>
";

#[test]
fn the_snapshot_is_read_by_line_kind() {
    let api = Api::parse(SNAPSHOT);
    for item in [
        "persia::Hit",
        "persia::Blob",
        "persia::Db::open",
        "persia::Hit::score",
        "persia::Blob::Stored",
        "persia::BlobRef::MAX_CHUNK_SIZE",
        "persia::blob_ref",
        "persia::Result",
        "persia::Db::default",
        "persia::Blob::*",
    ] {
        assert!(api.has(item), "{item}");
    }
    for item in [
        "persia::Nothing",
        "persia::Db::close",
        "persia::Hit::default",
        "persia::Error::*",
        "persia::Error",
    ] {
        assert!(!api.has(item), "{item}");
    }
    let types: Vec<&str> = api.types.iter().map(String::as_str).collect();
    assert_eq!(types, ["Blob", "Db", "Hit", "Result"]);
    assert_eq!(api.impls["Db"], ["core::default::Default"]);
    assert!(
        !api.impls.contains_key("Blob"),
        "a negative impl implements nothing"
    );
}

#[test]
fn paths_expand_groups_and_start_at_a_root() {
    let text = "use persia::{Db, Filter::{self, Term}, SearchRequest as R,\n    Value::*};\n\
                let db = Db::open(p)?; serde_json::Value::String; c.get_struct::<P>(); x::y; Vec::new(); persia";
    let found = paths(text);
    let found: Vec<&str> = found.iter().map(|(_, p)| p.as_str()).collect();
    assert_eq!(
        found,
        [
            "persia::Db",
            "persia::Filter",
            "persia::Filter::Term",
            "persia::SearchRequest",
            "persia::Value::*",
            "Db::open",
            "serde_json::Value::String",
            "x::y",
            "Vec::new",
        ]
    );
}

#[test]
fn pages_are_checked_against_the_snapshot() {
    let api = Api::parse(SNAPSHOT);
    let check = |page: &str, text: &str, runs: bool| page_violations(page, text, &api, Some(runs));
    // Clean: listed paths, another crate's paths, domain terms in prose, a status token.
    let clean = "Call `Db::open` or `persia::Db::open`; `Vec::new` and `serde_json::Value::String` are not ours.\n\
                 A Database file holds segments and a manifest. %%ENGINE_CALLS%%\n\
                 ```json\n{\"q\": \"x\"}\n```\n";
    assert!(
        check("a.md", clean, false).is_empty(),
        "{:?}",
        check("a.md", clean, false)
    );
    // Each of these is one violation.
    for (text, what) in [
        (
            "Call `persia::Db::close`.",
            "not in crates/persia/public-api.txt",
        ),
        ("Call Db::close.", "not in crates/persia/public-api.txt"),
        ("use persia::{Db, Nothing};", "`persia::Nothing` is not"),
        (
            "use persia::internal::pack_path;",
            "`persia::internal::pack_path` is not",
        ),
        ("The engine's `Database` type.", "engine type"),
        (
            "Scored by `TermScorer`, not by a facade call.",
            "`TermScorer` is an engine type",
        ),
        (
            "```text\nAnalyzerRegistry::register\n```",
            "`AnalyzerRegistry` is an engine type",
        ),
        ("```text\nCollectionViews::search\n```", "engine type"),
        ("persia_engine does it.", "engine crate"),
        // Bypasses: a leading `::`, blanks around `::`, a double-backtick span, an HTML code element.
        (
            "Call `::persia::internal::pack_path`.",
            "`persia::internal::pack_path` is not",
        ),
        (
            "Call `persia :: internal :: pack_path`.",
            "`persia::internal::pack_path` is not",
        ),
        ("The engine's ``Database`` type.", "engine type"),
        ("The engine's <code>Database</code> type.", "engine type"),
        ("```rust\nfn main() {}\n```", "does not run"),
    ] {
        let found = check("a.md", text, false);
        assert_eq!(found.len(), 1, "{text}: {found:?}");
        assert!(found[0].contains(what), "{text}: {found:?}");
    }
    // A page outside the guides: the name rules, not the sample rule.
    let other = page_violations(
        "index.mdx",
        "```rust\nfn main() { persia::Nothing; }\n```",
        &api,
        None,
    );
    assert_eq!(other.len(), 1, "{other:?}");
    assert!(other[0].contains("`persia::Nothing` is not"), "{other:?}");
    // A listed page: its samples run.
    let found = check("a.md", "```rust\nfn main() {}\n```", true);
    assert!(found.is_empty(), "{found:?}");
    for info in ["rust,no_run", "rust title=\"main.rs\"", "ignore", "rs"] {
        let text = format!("```{info}\nfn main() {{}}\n```");
        let found = check("a.md", &text, true);
        assert_eq!(found.len(), 1, "{info}: {found:?}");
        assert!(found[0].contains("rustdoc does not run"), "{found:?}");
    }
}

#[test]
fn engine_internals_parts_are_marked_and_narrow() {
    let api = Api::parse(SNAPSHOT);
    let check = |page: &str, text: &str| page_violations(page, text, &api, Some(false));
    let md = "Intro names `Db::open`.\n<!-- engine-internals -->\nThese are engine internals, not API to call: \
              `Database`, `persia_engine::db::Database::open`.\n<!-- /engine-internals -->\nAfter: `Database`.\n";
    let found = check("a.md", md);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("a.md:5: "), "{found:?}");
    let mdx = md
        .replace("<!-- engine-internals -->", "{/* engine-internals */}")
        .replace("<!-- /engine-internals -->", "{/* /engine-internals */}");
    assert_eq!(check("a.mdx", &mdx).len(), 1);
    // Inside a part, a `persia::` path is still checked, except the hidden `persia::internal` layer.
    let part = "<!-- engine-internals -->\nInternals: `persia::internal::pack_path`, `Db::close`, `persia::Nothing`.\n<!-- /engine-internals -->";
    let found = check("a.md", part);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("`persia::Nothing` is not"), "{found:?}");
    // The other form's marker, a part that does not say so, does not close, nests or closes nothing.
    for (page, text, what) in [
        ("a.mdx", md, "other file type"),
        (
            "a.md",
            "<!-- engine-internals -->\n`Database`\n<!-- /engine-internals -->",
            "must say",
        ),
        (
            "a.md",
            "<!-- engine-internals -->\nInternals: `Database`.",
            "does not close",
        ),
        (
            "a.md",
            "<!-- engine-internals -->\nInternals.\n<!-- engine-internals -->\nInternals.\n<!-- /engine-internals -->",
            "do not nest",
        ),
        ("a.md", "<!-- /engine-internals -->", "closes no"),
    ] {
        let found = check(page, text);
        assert!(found.iter().any(|f| f.contains(what)), "{text}: {found:?}");
    }
    // The sample rule holds inside a part.
    let sample = "<!-- engine-internals -->\nInternals.\n```rust\nfn main() {}\n```\n<!-- /engine-internals -->";
    assert_eq!(check("a.md", sample).len(), 1);
}
