//! `cargo xtask miri-filters`: `just miri`'s test filters still select the tests they were written for
//! (ROADMAP 0.6.9).
//!
//! Each `cargo miri test` line of the `miri` recipe in the justfile is parsed (with its `{{ var }}`s expanded), and
//! the same targets are listed natively with `cargo test <targets> -- --list`: Miri builds the same test binaries,
//! so the names are the same unless code is gated on `cfg(miri)`. A line with filters or skips must therefore target
//! packages without `cfg(miri)` in their sources (a test under `cfg_attr(miri, ignore)` would count as selected
//! here and not run there). libtest matches filters and `--skip`s as substrings of the full test name. The check
//! fails when
//! - a line selects no test at all,
//! - a filter selects no test (a renamed module would drop it from Miri without an error), or
//! - a `--skip` does not match exactly one of the tests the line selects: none means it is stale, more than one means
//!   it also skips a test it does not name, maybe one added later that Miri should run.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

const RECIPE: &str = "miri";

/// One `cargo miri test` line of the recipe.
#[derive(Debug, PartialEq, Eq)]
pub struct MiriRun {
    /// The line as written, for messages.
    pub line: String,
    /// The arguments between `test` and `--`: packages and targets.
    pub targets: Vec<String>,
    /// Positional arguments after `--`: a test runs if its name contains any of them (all if there are none).
    pub filters: Vec<String>,
    /// `--skip` arguments after `--`.
    pub skips: Vec<String>,
}

pub fn run(root: &Path) -> Result<bool> {
    let justfile = std::fs::read_to_string(root.join("justfile")).context("reading justfile")?;
    let runs = parse_recipe(&justfile)?;
    let mut ok = true;
    for run in &runs {
        let tests = list_tests(root, &run.targets)?;
        let mut problems = check(run, &tests);
        if !run.filters.is_empty() || !run.skips.is_empty() {
            for package in packages(&run.targets) {
                if let Some(file) = miri_cfg_in(&root.join("crates").join(package))? {
                    problems.push(format!(
                        "{} uses cfg(miri): the native test list may not be what Miri runs, so the filters \
                         and skips of this line cannot be checked",
                        file.display()
                    ));
                }
            }
        }
        for p in &problems {
            eprintln!("just miri: {p}\n  in: {}", run.line);
        }
        ok &= problems.is_empty();
        println!(
            "just miri: {} of {} tests left after filters and skips: {}",
            selected(run, &tests)
                .iter()
                .filter(|t| !run.skips.iter().any(|s| t.contains(s.as_str())))
                .count(),
            tests.len(),
            run.line
        );
    }
    Ok(ok)
}

/// The tests of `targets`, by full name, as `cargo test <targets> -- --list` prints them.
fn list_tests(root: &Path, targets: &[String]) -> Result<Vec<String>> {
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(root)
        .arg("test")
        .arg("--quiet")
        .args(targets)
        .args(["--", "--list", "--format", "terse"])
        .output()
        .context("running cargo test --list")?;
    if !out.status.success() {
        bail!(
            "cargo test {} -- --list failed:\n{}",
            targets.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(parse_list(&String::from_utf8_lossy(&out.stdout)))
}

/// The packages `-p <name>` / `--package <name>` in `targets`.
fn packages(targets: &[String]) -> Vec<&str> {
    targets
        .windows(2)
        .filter_map(|w| match w {
            [flag, name] if flag == "-p" || flag == "--package" => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

/// The first `.rs` file under `dir` whose code depends on `cfg(miri)`, if any.
fn miri_cfg_in(dir: &Path) -> Result<Option<std::path::PathBuf>> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            if let Some(found) = miri_cfg_in(&path)? {
                return Ok(Some(found));
            }
        } else if path.extension().is_some_and(|e| e == "rs")
            && uses_miri_cfg(&std::fs::read_to_string(&path)?)
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Whether Rust source `text` has a `cfg(miri)` predicate (`cfg(miri)`, `cfg_attr(miri, …)`, `not(miri)`, `any(…,
/// miri)`), ignoring whitespace.
pub fn uses_miri_cfg(text: &str) -> bool {
    let code: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    ["(miri)", "(miri,", ",miri)", ",miri,"]
        .iter()
        .any(|p| code.contains(p))
}

/// Test names from `--list --format terse` output (`<name>: test` per test; benches and other lines are not tests).
pub fn parse_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .map(str::to_owned)
        .collect()
}

/// The tests `run` would execute, before its skips.
fn selected<'a>(run: &MiriRun, tests: &'a [String]) -> Vec<&'a String> {
    tests
        .iter()
        .filter(|t| run.filters.is_empty() || run.filters.iter().any(|f| t.contains(f.as_str())))
        .collect()
}

/// What is wrong with `run` given the tests its targets hold; empty if nothing.
pub fn check(run: &MiriRun, tests: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    let selected = selected(run, tests);
    if selected.is_empty() {
        problems.push(format!("selects none of the {} tests", tests.len()));
    }
    for f in &run.filters {
        if !tests.iter().any(|t| t.contains(f.as_str())) {
            problems.push(format!("filter `{f}` selects no test"));
        }
    }
    for s in &run.skips {
        let hits: Vec<&&String> = selected.iter().filter(|t| t.contains(s.as_str())).collect();
        match hits.as_slice() {
            [_] => {}
            [] => problems.push(format!("`--skip {s}` matches no selected test")),
            many => problems.push(format!(
                "`--skip {s}` matches {} tests, name one: {}",
                many.len(),
                many.iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
    problems
}

/// The `cargo … miri test` lines of the `miri` recipe, with `{{ var }}`s expanded from the justfile's
/// `name := "value"` assignments. A variable assigned another way (a backtick command) must not appear after
/// `test`, where it would change what is selected.
pub fn parse_recipe(justfile: &str) -> Result<Vec<MiriRun>> {
    let vars: Vec<(&str, &str)> = justfile
        .lines()
        .filter_map(|l| l.split_once(":="))
        .filter_map(|(name, value)| {
            let value = value.trim();
            let quoted = value.strip_prefix('"')?.strip_suffix('"')?;
            // One string literal only: `"a" + "b"` is not a value this parser can expand.
            if quoted.contains('"') {
                return None;
            }
            Some((name.trim(), quoted))
        })
        .collect();
    let header = format!("{RECIPE}:");
    let body = justfile
        .lines()
        .skip_while(|l| l.trim_end() != header)
        .skip(1)
        // A recipe body may hold blank lines; it ends at the first non-blank line that is not indented.
        .take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t']));
    let mut runs = Vec::new();
    for raw in body {
        let line = raw.trim();
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(test_at) = words.windows(2).position(|w| w == ["miri", "test"]) else {
            continue;
        };
        let after = words.get(test_at + 2..).unwrap_or_default().join(" ");
        let expanded = expand(&after, &vars).with_context(|| format!("in: {line}"))?;
        let words: Vec<&str> = expanded.split_whitespace().collect();
        let (targets, args) =
            words
                .iter()
                .position(|w| *w == "--")
                .map_or((words.as_slice(), &[][..]), |at| {
                    (
                        words.get(..at).unwrap_or_default(),
                        words.get(at + 1..).unwrap_or_default(),
                    )
                });
        let mut run = MiriRun {
            line: line.to_owned(),
            targets: targets.iter().map(|w| (*w).to_owned()).collect(),
            filters: Vec::new(),
            skips: Vec::new(),
        };
        let mut args = args.iter().copied();
        while let Some(a) = args.next() {
            match a {
                "--skip" => {
                    let s = args
                        .next()
                        .with_context(|| format!("`--skip` without a name in: {line}"))?;
                    run.skips.push(s.to_owned());
                }
                flag if flag.starts_with('-') => {
                    bail!("unexpected test argument `{flag}` in: {line}")
                }
                filter => run.filters.push(filter.to_owned()),
            }
        }
        runs.push(run);
    }
    if runs.is_empty() {
        bail!("justfile: no `cargo miri test` lines in recipe `{RECIPE}`");
    }
    Ok(runs)
}

/// `text` with each `{{ name }}` replaced by its value.
fn expand(text: &str, vars: &[(&str, &str)]) -> Result<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some((before, after)) = rest.split_once("{{") {
        let (name, tail) = after.split_once("}}").context("unclosed `{{`")?;
        let name = name.trim();
        let value = vars
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
            .with_context(|| format!("`{{{{ {name} }}}}` is not a quoted justfile variable"))?;
        out.push_str(before);
        out.push_str(value);
        rest = tail;
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JUSTFILE: &str = r#"nightly := `cat .github/nightly-toolchain`

other:
    cargo +{{ nightly }} miri test -p not-this

# comment
miri:
    cargo +{{ nightly }} miri test -p a --lib

    @cargo +{{ nightly }} miri test -p b --lib -- mod_a:: mod_b {{ skips }}
    echo not a test line
    cargo +{{ nightly }} miri test -p c --doc --

skips := "--skip slow_one --skip other_slow"
after:
    cargo +{{ nightly }} miri test -p not-this-either
"#;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| (*s).to_owned()).collect()
    }

    fn run(filters: &[&str], skips: &[&str]) -> MiriRun {
        MiriRun {
            line: String::new(),
            targets: Vec::new(),
            filters: names(filters),
            skips: names(skips),
        }
    }

    #[test]
    fn parses_only_the_miri_recipe_and_expands_variables() {
        let runs = parse_recipe(JUSTFILE).unwrap();
        assert_eq!(runs.len(), 3, "the blank line does not end the recipe");
        assert_eq!(runs[0].targets, names(&["-p", "a", "--lib"]));
        assert!(runs[0].filters.is_empty() && runs[0].skips.is_empty());
        assert_eq!(runs[1].targets, names(&["-p", "b", "--lib"]));
        assert_eq!(runs[1].filters, names(&["mod_a::", "mod_b"]));
        assert_eq!(runs[1].skips, names(&["slow_one", "other_slow"]));
        // A trailing `--` separates nothing: it is neither a target nor a filter.
        assert_eq!(runs[2].targets, names(&["-p", "c", "--doc"]));
        assert!(runs[2].filters.is_empty() && runs[2].skips.is_empty());
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        assert!(parse_recipe("miri:\n    echo nothing\n").is_err());
        assert!(parse_recipe("miri:\n    cargo miri test -p a -- {{ missing }}\n").is_err());
        assert!(parse_recipe("miri:\n    cargo miri test -p a -- --exact x\n").is_err());
        assert!(parse_recipe("miri:\n    cargo miri test -p a -- --skip\n").is_err());
        assert!(parse_recipe("miri:\n    cargo miri test -p a -- {{ x\n").is_err());
        let concatenated = "v := \"a\" + \"b\"\nmiri:\n    cargo miri test -p a -- {{ v }}\n";
        assert!(
            parse_recipe(concatenated).is_err(),
            "only a single string literal expands"
        );
    }

    #[test]
    fn parses_list_output() {
        let out = "a::b: test\nc: test\nsome_bench: bench\n\n2 tests, 1 benchmark\n";
        assert_eq!(parse_list(out), names(&["a::b", "c"]));
    }

    #[test]
    fn accepts_filters_and_skips_that_match_as_named() {
        let tests = names(&["m::one", "m::two", "n::three", "other"]);
        assert_eq!(
            check(&run(&["m::", "n::"], &["two", "three"]), &tests),
            Vec::<String>::new()
        );
        assert_eq!(check(&run(&[], &["other"]), &tests), Vec::<String>::new());
    }

    #[test]
    fn a_filter_that_selects_nothing_fails() {
        let tests = names(&["m::one", "n::two"]);
        let problems = check(&run(&["m::", "renamed::"], &[]), &tests);
        assert_eq!(problems, ["filter `renamed::` selects no test"]);
        assert_eq!(
            check(&run(&["gone"], &[]), &tests).len(),
            2,
            "no test selected at all"
        );
    }

    #[test]
    fn a_skip_must_match_exactly_one_selected_test() {
        let tests = names(&["m::flip_a", "m::flip_b", "n::kept"]);
        let stale = check(&run(&["m::"], &["renamed"]), &tests);
        assert_eq!(stale, ["`--skip renamed` matches no selected test"]);
        // A test outside the filters is not run anyway, so skipping it is stale too.
        assert_eq!(check(&run(&["m::"], &["kept"]), &tests).len(), 1);
        let wide = check(&run(&["m::"], &["flip"]), &tests);
        assert_eq!(
            wide,
            ["`--skip flip` matches 2 tests, name one: m::flip_a, m::flip_b"]
        );
    }

    #[test]
    fn finds_miri_cfgs_and_packages() {
        for gated in [
            "#[cfg(miri)]",
            "#[cfg_attr(miri, ignore = \"slow\")]",
            "#[cfg(not( miri ))]",
            "#[cfg(any(test, miri))]",
            "cfg!(miri)",
        ] {
            assert!(uses_miri_cfg(gated), "{gated}");
        }
        assert!(!uses_miri_cfg(
            "// just miri skips this (slow)\nfn miri_like() {}"
        ));
        let targets = names(&["-p", "a", "--lib", "--package", "b", "--test", "p"]);
        assert_eq!(packages(&targets), ["a", "b"]);
    }

    /// The recipe as committed: if a line stopped parsing, its filters would go unchecked.
    #[test]
    fn the_repository_recipe_parses() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let runs = parse_recipe(&std::fs::read_to_string(root.join("justfile")).unwrap()).unwrap();
        let targets: Vec<String> = runs.iter().map(|r| r.targets.join(" ")).collect();
        assert_eq!(
            targets,
            [
                "-p persia-format --lib",
                "-p persia-format --doc",
                "-p persia-engine --lib",
                "-p persia-engine --test manifest --test wal --test document_encoding",
            ]
        );
        assert!(runs[2].filters.contains(&"document::".to_owned()) && runs[2].skips.len() > 1);
        assert!(runs[3].filters.is_empty() && runs[3].skips.len() > 1);
    }
}
