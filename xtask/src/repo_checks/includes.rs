//! The files the published crates are compiled with (ROADMAP 10.1.42, 10.1.53).
//!
//! - **Which crates are published.** Every member of the root workspace (`[workspace] members`, `dir/*` expanded)
//!   whose manifest does not write `publish = false` (or `publish = []`) itself ([`published_crates`]). Today the
//!   workspace's `publish = false` holds every crate back; a crate that inherits it (`publish.workspace = true`) is
//!   one meant to be published when it is flipped, and one that is never published says so in its own manifest:
//!   `persia-docs` (it reads the website), `persia-testutil` (a dev-dependency only) and `xtask` (repo tooling).
//!   `fuzz/` is its own workspace, not a member.
//! - **Their includes stay in their directory.** No `include_str!`, `include_bytes!` or `include!` (with any of
//!   `(`, `[`, `{`) and no `#[path = "..."]` module attribute in a source file under a published crate's directory
//!   names a path outside that directory: `cargo package` ships the crate's directory only, so its first publish
//!   (or a test of the published crate) would fail on one. A path is resolved against the directory of the file
//!   that names it, as rustc does, and normalized lexically (`.` dropped, `..` pops a component); an argument that
//!   is not a plain string literal (`concat!(env!("OUT_DIR"), ..)`, a raw string) cannot be checked and is refused
//!   too. Checks that read files elsewhere in the repository live in `crates/persia-docs`, never published.
//! - **Copies of fuzz seeds stay equal to the corpus.** A crate's tests that check committed fuzz seeds read copies
//!   under its `tests/fixtures/fuzz_seeds/<target>/<name>`; each must be byte for byte the file
//!   `fuzz/corpus/<target>/<name>`, which stays the source (cargo-fuzz reads that directory). A seed changed or
//!   removed in the corpus fails here until its copy follows.

use std::path::{Component, Path, PathBuf};

use super::guides::{read, root};

/// The macros whose argument is a path the compiler reads, relative to the file that names it.
const INCLUDE_MACROS: &[&str] = &["include_str!", "include_bytes!", "include!"];

/// `path` without `.` and with each `..` applied to the component before it; `None` when a `..` climbs above the
/// start of `path` or `path` is absolute (both are outside any directory of the repository the check names).
pub(super) fn normalize(path: &Path) -> Option<PathBuf> {
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
/// `crate_dir` (relative to the root), as `file:line: ...` messages: the include macros, with any of their three
/// delimiters, and `#[path = "..."]` attributes, which make rustc read a module's file from that path. A path is
/// resolved against the directory of `file` (rustc's rule for a `#[path]` outside an inline module, and for every
/// include). An include whose argument is not a plain string literal, or a macro name not followed by a delimiter,
/// is reported as unchecked.
fn includes_outside(file: &Path, text: &str, crate_dir: &Path) -> Vec<String> {
    let dir = file.parent().unwrap_or(Path::new(""));
    let mut found = Vec::new();
    for name in INCLUDE_MACROS.iter().chain(&["#[path"]) {
        for (at, _) in text.match_indices(name) {
            let (head, tail) = text.split_at(at);
            // `my_include!(` is another macro.
            let before = head.chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let line = head.matches('\n').count() + 1;
            let where_ = format!("{}:{line}", file.display());
            let rest = tail.strip_prefix(name).unwrap_or_default().trim_start();
            // A macro call takes any of the three delimiters (`include_str!["x"]` reads a file as `(` does); the
            // attribute is `#[path = "..."]`.
            let opened = if *name == "#[path" {
                rest.strip_prefix('=')
            } else {
                rest.strip_prefix('(')
                    .or_else(|| rest.strip_prefix('['))
                    .or_else(|| rest.strip_prefix('{'))
            };
            let literal = opened
                .map(str::trim_start)
                .and_then(|args| args.strip_prefix('"'))
                .and_then(|args| args.split_once('"'))
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
                    "{where_}: `{name}` names \"{path}\", a file outside {}, which `cargo package` does not ship",
                    crate_dir.display()
                ));
            }
        }
    }
    found
}

/// Every file under `dir` (relative to the root) that `keep` accepts, relative to the root, sorted.
fn files_under(dir: &Path, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(root().join(&next)).unwrap() {
            let entry = entry.unwrap();
            let rel = next.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                pending.push(rel);
            } else if keep(&rel) {
                files.push(rel);
            }
        }
    }
    files.sort();
    files
}

/// Every `.rs` file under `dir` (relative to the root), relative to the root.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    files_under(dir, |f| f.extension().is_some_and(|e| e == "rs"))
}

/// Whether a member whose manifest's `[package]` is `package` is meant to be published: unless it writes
/// `publish = false` or `publish = []` itself, as a crate that is never published does. An inherited
/// `publish.workspace = true` is the workspace's `false` today, flipped for every such crate at once.
fn meant_to_publish(package: &toml::Table) -> bool {
    match package.get("publish") {
        Some(toml::Value::Boolean(publish)) => *publish,
        Some(toml::Value::Array(registries)) => !registries.is_empty(),
        _ => true,
    }
}

/// The directories (relative to the root) of every member of the root workspace, from its `members` (a `dir/*`
/// pattern is every subdirectory of `dir` with a `Cargo.toml`; any other glob is refused, as this does not expand
/// it), sorted.
fn members() -> Vec<PathBuf> {
    let manifest: toml::Table = read("Cargo.toml").parse().unwrap();
    let members = manifest["workspace"]["members"].as_array().unwrap();
    let mut dirs = Vec::new();
    for pattern in members.iter().map(|m| m.as_str().unwrap()) {
        if let Some(parent) = pattern.strip_suffix("/*") {
            for entry in std::fs::read_dir(root().join(parent)).unwrap() {
                let dir = Path::new(parent).join(entry.unwrap().file_name());
                if root().join(&dir).join("Cargo.toml").exists() {
                    dirs.push(dir);
                }
            }
        } else {
            assert!(
                !pattern.contains(['*', '?', '[']),
                "member pattern {pattern:?}: expand it here"
            );
            dirs.push(PathBuf::from(pattern));
        }
    }
    dirs.sort();
    dirs
}

/// The members [`meant_to_publish`], read from their manifests.
fn published_crates() -> Vec<PathBuf> {
    members()
        .into_iter()
        .filter(|dir| {
            let manifest: toml::Table = read(dir.join("Cargo.toml").to_str().unwrap())
                .parse()
                .unwrap();
            meant_to_publish(manifest["package"].as_table().unwrap())
        })
        .collect()
}

/// No published crate reads a file outside its directory at compile time (ROADMAP 10.1.42, extended to every
/// published crate by 10.1.53).
#[test]
fn published_crates_include_nothing_outside_their_directory() {
    let crates = published_crates();
    // The rule reads the manifests; these say what it must pick today.
    for (dir, published) in [
        ("crates/persia", true),
        ("crates/persia-engine", true),
        ("crates/persia-blob", true),
        ("crates/persia-cli", true),
        ("crates/persia-docs", false),
        ("crates/persia-testutil", false),
        ("xtask", false),
    ] {
        assert_eq!(
            crates.contains(&PathBuf::from(dir)),
            published,
            "{dir} among {crates:?}"
        );
    }
    let mut found = Vec::new();
    for crate_dir in &crates {
        let files = rust_files(crate_dir);
        assert!(
            files.iter().any(|f| f.starts_with(crate_dir.join("src"))),
            "no source under {} among {files:?}",
            crate_dir.display()
        );
        for file in &files {
            let text = read(file.to_str().unwrap());
            found.extend(includes_outside(file, &text, crate_dir));
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn only_a_manifests_own_false_keeps_a_crate_unpublished() {
    for (package, published) in [
        ("publish.workspace = true", true),
        ("", true),
        ("publish = true", true),
        ("publish = [\"registry\"]", true),
        ("publish = false", false),
        ("publish = []", false),
    ] {
        let table: toml::Table = format!("name = \"x\"\n{package}").parse().unwrap();
        assert_eq!(meant_to_publish(&table), published, "{package}");
    }
}

/// Each copy of a fuzz seed under a member's `tests/fixtures/fuzz_seeds/` is the corpus file it copies (ROADMAP
/// 10.1.53).
#[test]
fn fuzz_seed_copies_equal_the_corpus() {
    let mut copies = 0;
    let mut differ = Vec::new();
    for member in members() {
        let seeds = member.join("tests/fixtures/fuzz_seeds");
        if !root().join(&seeds).is_dir() {
            continue;
        }
        for copy in files_under(&seeds, |_| true) {
            let source = Path::new("fuzz/corpus").join(copy.strip_prefix(&seeds).unwrap());
            copies += 1;
            match std::fs::read(root().join(&source)) {
                Ok(bytes) if bytes == std::fs::read(root().join(&copy)).unwrap() => {}
                Ok(_) => differ.push(format!(
                    "{} differs from {}",
                    copy.display(),
                    source.display()
                )),
                Err(e) => differ.push(format!(
                    "{} copies {}, which is not there: {e}",
                    copy.display(),
                    source.display()
                )),
            }
        }
    }
    // The seed tests read 27 copies (the engine's 13, persia-format's 14): the check is not vacuous.
    assert!(copies >= 27, "{copies} copies found");
    assert!(differ.is_empty(), "{differ:#?}");
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
        // A module file, as the tests' shared support modules name theirs.
        (
            "crates/persia/tests/blob.rs",
            "#[path = \"support/store.rs\"]\nmod store;",
        ),
        ("crates/persia/src/a.rs", r#"include_str!["b.md"]"#),
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
        // The other delimiters, and a module file outside the crate.
        ("crates/persia/src/a.rs", r#"include_str!["../../../x"]"#),
        ("crates/persia/src/a.rs", r#"include_bytes!{"../../../x"}"#),
        (
            "crates/persia/src/a.rs",
            "#[path = \"../../../x.rs\"] mod m;",
        ),
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
        // No delimiter, and a `#[path]` without a literal.
        "include_str! <x>",
        "#[path(x)] mod m;",
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
