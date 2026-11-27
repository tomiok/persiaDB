//! The website guides and the files `persia` is compiled with (ROADMAP 10.1.34, 10.1.42).
//!
//! - **Includes stay in `persia`.** No `include_str!`, `include_bytes!` or `include!` in a source file under
//!   `crates/persia/` names a path outside that directory: `cargo package` ships the crate's directory only, so the
//!   first publish (or a test of the published crate) would fail on one. A path is resolved against the directory of
//!   the file that names it, as rustc does, and normalized lexically (`.` dropped, `..` pops a component); an argument
//!   that is not a plain string literal (`concat!(env!("OUT_DIR"), ..)`, a raw string) cannot be checked and is
//!   refused too. Checks that read files elsewhere in the repository live in `crates/persia-docs`, never published.
//! - **Guide samples are doctests of the public API.** `crates/persia-docs/src/guides.rs` runs the guides' Rust
//!   samples as doctests (its module docs say with which features, and what the compiler cannot refuse there); this
//!   check keeps it wired, keeps every page it lists present with a Rust sample, and refuses names a guide has
//!   reached for outside the documented API.

use std::path::{Component, Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

// --- Includes (ROADMAP 10.1.42) ---

/// The macros whose argument is a path the compiler reads, relative to the file that names it.
const INCLUDE_MACROS: &[&str] = &["include_str!", "include_bytes!", "include!"];

/// `path` without `.` and with each `..` applied to the component before it; `None` when a `..` climbs above the
/// start of `path` or `path` is absolute (both are outside any directory of the repository the check names).
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(part) => out.push(part),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// The includes of `text`, the source of `file` (relative to the repository root), whose path is not under
/// `crate_dir` (relative to the root), as `file:line: ...` messages. An include whose argument is not a plain string
/// literal is reported as unchecked.
fn includes_outside(file: &Path, text: &str, crate_dir: &Path) -> Vec<String> {
    let dir = file.parent().unwrap_or(Path::new(""));
    let mut found = Vec::new();
    for name in INCLUDE_MACROS {
        for (at, _) in text.match_indices(name) {
            let (head, tail) = text.split_at(at);
            // `my_include!(` is another macro.
            let before = head.chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let Some(args) = tail
                .strip_prefix(name)
                .and_then(|rest| rest.trim_start().strip_prefix('('))
            else {
                continue;
            };
            let line = head.matches('\n').count() + 1;
            let where_ = format!("{}:{line}", file.display());
            let literal = args
                .trim_start()
                .strip_prefix('"')
                .and_then(|rest| rest.split_once('"'))
                .map(|(path, _)| path)
                .filter(|path| !path.contains('\\'));
            let Some(path) = literal else {
                found.push(format!(
                    "{where_}: `{name}` with an argument that is not a plain string literal, which this check cannot \
                     resolve"
                ));
                continue;
            };
            let inside = normalize(&dir.join(path)).is_some_and(|p| p.starts_with(crate_dir));
            if !inside {
                found.push(format!(
                    "{where_}: `{name}(\"{path}\")` reads a file outside {}, which `cargo package` does not ship",
                    crate_dir.display()
                ));
            }
        }
    }
    found
}

/// Every `.rs` file under `dir` (relative to the root), relative to the root.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(root().join(&next)).unwrap() {
            let entry = entry.unwrap();
            let rel = next.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                pending.push(rel);
            } else if rel.extension().is_some_and(|e| e == "rs") {
                files.push(rel);
            }
        }
    }
    files.sort();
    files
}

/// `persia` reads no file outside `crates/persia/` at compile time (ROADMAP 10.1.42).
#[test]
fn persia_includes_nothing_outside_its_directory() {
    let crate_dir = Path::new("crates/persia");
    let files = rust_files(crate_dir);
    assert!(
        files.iter().any(|f| f.ends_with("src/lib.rs")),
        "no crates/persia/src/lib.rs among {files:?}"
    );
    let found: Vec<String> = files
        .iter()
        .flat_map(|file| {
            let text = read(file.to_str().unwrap());
            includes_outside(file, &text, crate_dir)
        })
        .collect();
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn includes_resolve_against_their_file() {
    let crate_dir = Path::new("crates/persia");
    let check = |file: &str, text: &str| includes_outside(Path::new(file), text, crate_dir);
    // Inside: the same directory, a sibling, out and back in; and another macro.
    for (file, text) in [
        ("crates/persia/src/a.rs", r#"include_str!("b.md")"#),
        (
            "crates/persia/src/a.rs",
            r#"include_bytes!("../tests/data/x.bin")"#,
        ),
        (
            "crates/persia/src/a.rs",
            r#"include_str!("./../../persia/README.md")"#,
        ),
        ("crates/persia/tests/t.rs", "include!( \"common/mod.rs\" )"),
        ("crates/persia/src/a.rs", r#"my_include!("../../../x")"#),
    ] {
        let found = check(file, text);
        assert!(found.is_empty(), "{text}: {found:?}");
    }
    // Outside: the repository, a sibling crate whose name starts with this one's, past the root, absolute.
    for (file, text) in [
        (
            "crates/persia/src/a.rs",
            r#"include_str!("../../../website/x.md")"#,
        ),
        (
            "crates/persia/src/error.rs",
            r#"let doc = include_str!("../../../docs/sdk/c-abi.md");"#,
        ),
        (
            "crates/persia/src/a.rs",
            r#"include_bytes!("../../persia-engine/x")"#,
        ),
        (
            "crates/persia/src/a.rs",
            r#"include!("../../../../../etc/passwd")"#,
        ),
        ("crates/persia/src/a.rs", r#"include_str!("/etc/hosts")"#),
    ] {
        let found = check(file, text);
        assert_eq!(found.len(), 1, "{text}: {found:?}");
        assert!(found[0].starts_with(&format!("{file}:1: ")), "{found:?}");
    }
    // Not a literal: refused, as it cannot be resolved.
    for text in [
        r#"include!(concat!(env!("OUT_DIR"), "/x.rs"))"#,
        r##"include_str!(r#"x.md"#)"##,
        r#"include_str!("a\\b.md")"#,
    ] {
        assert_eq!(check("crates/persia/src/a.rs", text).len(), 1, "{text}");
    }
    // The line number.
    let found = check(
        "crates/persia/src/a.rs",
        "\n\n  include_str!(\"../../../x\")",
    );
    assert!(
        found[0].starts_with("crates/persia/src/a.rs:3: "),
        "{found:?}"
    );
}

// --- Guide samples (ROADMAP 10.1.34) ---

/// The website guides `crates/persia-docs/src/guides.rs` runs as doctests are wired (`mod guides` under
/// `#[cfg(doctest)]` in that crate's `lib.rs`, every listed page present, each with a Rust sample), and their samples
/// do not name what a guide has reached for outside the documented API. `cargo test --workspace` links the doctests
/// to a `persia` with `internal` on (feature unification, see `guides.rs`), and every `#[doc(hidden)]` item
/// (`Filter::from_json`, which takes the engine's `QueryLimits`) compiles in a sample: rustdoc alone does not catch
/// them.
#[test]
fn guide_samples_are_doctests_of_the_public_api() {
    let lib = read("crates/persia-docs/src/lib.rs");
    assert!(
        lib.contains("#[cfg(doctest)]\nmod guides;"),
        "crates/persia-docs/src/lib.rs: `#[cfg(doctest)] mod guides;` is missing, so no guide sample runs"
    );
    let guides = read("crates/persia-docs/src/guides.rs");
    let pages: Vec<&str> = guides
        .split("include_str!(\"")
        .skip(1)
        .filter_map(|rest| rest.split_once("\")").map(|(path, _)| path))
        .collect();
    assert!(
        !pages.is_empty(),
        "crates/persia-docs/src/guides.rs: no page"
    );
    for page in pages {
        let text = std::fs::read_to_string(root().join("crates/persia-docs/src").join(page))
            .unwrap_or_else(|e| panic!("guides.rs lists {page}, which cannot be read: {e}"));
        // rustdoc runs a fence with no tag or a `rust` one (`rust,no_run`), not `json`, `text` and the like.
        let mut samples = 0;
        let mut in_fence: Option<bool> = None;
        for (n, line) in text.lines().enumerate() {
            match (line.trim_start().strip_prefix("```"), in_fence) {
                (Some(_), Some(_)) => in_fence = None,
                (Some(tag), None) => {
                    let tag = tag.trim();
                    let rust = tag.is_empty()
                        || tag.split(|c: char| c == ',' || c.is_whitespace()).next()
                            == Some("rust");
                    samples += usize::from(rust);
                    in_fence = Some(rust);
                }
                (None, Some(true)) => {
                    for refused in [
                        "persia_engine",
                        "persia::internal",
                        "QueryLimits",
                        "Filter::from_json",
                    ] {
                        assert!(
                            !line.contains(refused),
                            "{page}:{}: a guide sample names `{refused}`, which is not the documented API",
                            n + 1
                        );
                    }
                }
                (None, _) => {}
            }
        }
        assert!(
            samples > 0,
            "{page}: no Rust sample, so guides.rs checks nothing of it"
        );
    }
}
