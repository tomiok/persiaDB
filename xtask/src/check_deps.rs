//! `cargo xtask check-deps`: enforce the dependency rules in CLAUDE.md ("Architecture", "Dependencies").
//!
//! Internal crates: every workspace member must appear in [`ALLOWED`] and may only depend on the crates
//! listed for it. Normal and build dependencies use that list; dev-dependencies may also use
//! `persia-testutil`, which is never a normal or build dependency.
//!
//! External crates: every direct dependency must be declared in the root `[workspace.dependencies]`
//! (the allowed list) and inherited with `.workspace = true`, so versions live in one place. Tiers:
//! [`BINARY_ONLY`] crates only in [`BINARIES`] (or as dev-dependencies), [`DEV_ONLY`] crates only as
//! dev-dependencies, [`TOOLING_ONLY`] crates only in `xtask`. Build scripts are not allowed.
//! Licenses, advisories and banned crates in the full graph are cargo-deny's job (`deny.toml`).

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const TESTUTIL: &str = "persia-testutil";
const XTASK: &str = "xtask";

/// Direct internal dependencies each crate may have. Layers may not be skipped:
/// e.g. `persia` reaches persia-format only through persia-engine / persia-blob.
const ALLOWED: &[(&str, &[&str])] = &[
    ("persia-format", &[]),
    ("persia-storage", &["persia-format"]),
    ("persia-analysis", &[]),
    (
        "persia-engine",
        &["persia-format", "persia-storage", "persia-analysis"],
    ),
    ("persia-blob", &["persia-format", "persia-storage"]),
    ("persia", &["persia-engine", "persia-blob"]),
    ("persia-proto", &[]),
    ("persia-server", &["persia", "persia-proto"]),
    ("persia-cli", &["persia"]),
    (TESTUTIL, &[]),
    (XTASK, &[]),
];

const BINARIES: &[&str] = &["persia-server", "persia-cli", XTASK];
const BINARY_ONLY: &[&str] = &["clap", "anyhow", "rustls", "tracing-subscriber"];
const DEV_ONLY: &[&str] = &[
    "proptest",
    "criterion",
    "insta",
    "tempfile",
    "testcontainers",
    "hdrhistogram",
];
const TOOLING_ONLY: &[&str] = &["toml"];

const DEP_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// The subset of `cargo metadata --format-version 1 --no-deps` this check reads.
#[derive(Debug, Deserialize)]
pub(crate) struct Metadata {
    packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
struct Package {
    name: String,
    #[serde(default)]
    dependencies: Vec<Dependency>,
    #[serde(default)]
    targets: Vec<Target>,
    #[serde(default)]
    manifest_path: String,
}

#[derive(Debug, Deserialize)]
struct Dependency {
    /// The real package name, even when the dependency is renamed.
    name: String,
    /// `None` for normal dependencies, else `"dev"` or `"build"`.
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Target {
    kind: Vec<String>,
    src_path: String,
}

fn allowed_internal(krate: &str) -> Option<&'static [&'static str]> {
    ALLOWED
        .iter()
        .find(|(name, _)| *name == krate)
        .map(|(_, deps)| *deps)
}

fn external_violation(
    krate: &str,
    target: &str,
    kind: &str,
    external: &BTreeSet<String>,
) -> Option<&'static str> {
    if !external.contains(target) {
        Some("not in [workspace.dependencies] (allowed list)")
    } else if DEV_ONLY.contains(&target) && kind != "dev" {
        Some("dev-only crate used outside [dev-dependencies]")
    } else if BINARY_ONLY.contains(&target) && kind != "dev" && !BINARIES.contains(&krate) {
        Some("binary-only crate used by a library")
    } else if TOOLING_ONLY.contains(&target) && krate != XTASK {
        Some("tooling-only crate used outside xtask")
    } else {
        None
    }
}

/// Internal-direction and external-allowlist violations, one message per offending edge.
pub(crate) fn violations(metadata: &Metadata, external: &BTreeSet<String>) -> Vec<String> {
    let members: BTreeSet<&str> = metadata.packages.iter().map(|p| p.name.as_str()).collect();
    let mut errors: Vec<String> = members
        .iter()
        .filter(|name| allowed_internal(name).is_none())
        .map(|name| format!("{name}: workspace member is not listed in xtask check-deps ALLOWED"))
        .collect();

    let mut packages: Vec<&Package> = metadata.packages.iter().collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    for pkg in packages {
        let Some(allowed) = allowed_internal(&pkg.name) else {
            continue; // already reported above
        };
        for dep in &pkg.dependencies {
            let target = dep.name.as_str();
            let kind = dep.kind.as_deref().unwrap_or("normal");
            if !members.contains(target) {
                let problem = if target.starts_with("persia") {
                    // Internal crates outside the workspace (e.g. a future sdk/rust) must be added deliberately.
                    Some("internal crate outside the workspace")
                } else {
                    external_violation(&pkg.name, target, kind, external)
                };
                if let Some(problem) = problem {
                    errors.push(format!("{} -> {target} ({kind}): {problem}", pkg.name));
                }
                continue;
            }
            let ok = allowed.contains(&target)
                || (kind == "dev" && target == TESTUTIL && pkg.name != TESTUTIL);
            if !ok {
                errors.push(format!("{} -> {target} ({kind}): not allowed", pkg.name));
            }
        }
    }
    errors
}

/// Build scripts are not allowed (CLAUDE.md "Dependencies"): generated code is committed and checked in CI.
pub(crate) fn build_scripts(metadata: &Metadata) -> Vec<String> {
    let mut packages: Vec<&Package> = metadata.packages.iter().collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
        .iter()
        .flat_map(|pkg| {
            pkg.targets
                .iter()
                .filter(|t| t.kind.iter().any(|k| k == "custom-build"))
                .map(move |t| {
                    format!(
                        "{}: has a build script ({}); commit generated code instead",
                        pkg.name, t.src_path
                    )
                })
        })
        .collect()
}

/// All dependency tables of a member manifest, including target-specific ones.
fn dependency_tables(manifest: &toml::Table) -> Vec<(String, &toml::Table)> {
    let mut tables: Vec<(String, &toml::Table)> = DEP_TABLES
        .iter()
        .filter_map(|t| {
            manifest
                .get(*t)
                .and_then(toml::Value::as_table)
                .map(|deps| ((*t).to_owned(), deps))
        })
        .collect();
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for (cfg, section) in targets {
            for t in DEP_TABLES {
                if let Some(deps) = section.get(t).and_then(toml::Value::as_table) {
                    tables.push((format!("target.'{cfg}'.{t}"), deps));
                }
            }
        }
    }
    tables
}

/// Dependencies that do not use `.workspace = true` (versions and features must live in the root).
pub(crate) fn not_inherited(krate: &str, manifest: &toml::Table) -> Vec<String> {
    dependency_tables(manifest)
        .into_iter()
        .flat_map(|(table, deps)| {
            deps.iter()
                .filter(|(_, spec)| {
                    spec.get("workspace").and_then(toml::Value::as_bool) != Some(true)
                })
                .map(move |(dep, _)| {
                    format!("{krate} -> {dep} ([{table}]): must be `{dep}.workspace = true`")
                })
        })
        .collect()
}

/// Names declared in the root `[workspace.dependencies]`.
pub(crate) fn workspace_dependencies(root_manifest: &toml::Table) -> Result<BTreeSet<String>> {
    let deps = root_manifest
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(toml::Value::as_table)
        .context("root Cargo.toml has no [workspace.dependencies]")?;
    Ok(deps.keys().cloned().collect())
}

fn read_toml(path: &Path) -> Result<toml::Table> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.parse()
        .with_context(|| format!("parsing {}", path.display()))
}

/// Runs the check on the workspace at `root`; prints violations and returns whether it passed.
pub(crate) fn run(root: &Path) -> Result<bool> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: Metadata =
        serde_json::from_slice(&output.stdout).context("parsing cargo metadata")?;

    let external = workspace_dependencies(&read_toml(&root.join("Cargo.toml"))?)?;
    let mut errors = violations(&metadata, &external);
    errors.extend(build_scripts(&metadata));
    for pkg in &metadata.packages {
        errors.extend(not_inherited(
            &pkg.name,
            &read_toml(Path::new(&pkg.manifest_path))?,
        ));
    }

    if errors.is_empty() {
        println!("dependency rules OK ({} crates)", ALLOWED.len());
        return Ok(true);
    }
    eprintln!("Dependency rule violations (see CLAUDE.md \"Architecture\" and \"Dependencies\"):");
    for e in &errors {
        eprintln!("  {e}");
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace with every ALLOWED crate plus `extra`; `edges` are (from, to, kind).
    fn metadata(edges: &[(&str, &str, Option<&str>)], extra: &[&str]) -> Metadata {
        let names = ALLOWED.iter().map(|(n, _)| *n).chain(extra.iter().copied());
        Metadata {
            packages: names
                .map(|name| Package {
                    name: name.to_owned(),
                    dependencies: edges
                        .iter()
                        .filter(|(from, _, _)| *from == name)
                        .map(|(_, to, kind)| Dependency {
                            name: (*to).to_owned(),
                            kind: kind.map(str::to_owned),
                        })
                        .collect(),
                    targets: Vec::new(),
                    manifest_path: String::new(),
                })
                .collect(),
        }
    }

    fn ext(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn every_allowed_edge_passes() {
        let edges: Vec<_> = ALLOWED
            .iter()
            .flat_map(|(from, tos)| tos.iter().map(move |to| (*from, *to, None)))
            .collect();
        assert_eq!(
            violations(&metadata(&edges, &[]), &ext(&[])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn upward_edge_rejected() {
        let md = metadata(&[("persia-format", "persia-engine", None)], &[]);
        assert_eq!(
            violations(&md, &ext(&[])),
            ["persia-format -> persia-engine (normal): not allowed"]
        );
    }

    #[test]
    fn layer_skip_rejected() {
        let md = metadata(&[("persia", "persia-format", None)], &[]);
        assert_eq!(
            violations(&md, &ext(&[])),
            ["persia -> persia-format (normal): not allowed"]
        );
    }

    #[test]
    fn engine_must_not_depend_on_blob() {
        assert_ne!(
            violations(
                &metadata(&[("persia-engine", "persia-blob", None)], &[]),
                &ext(&[])
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn build_and_dev_edges_follow_direction_rules() {
        let md = metadata(&[("persia-storage", "persia-engine", Some("build"))], &[]);
        assert_eq!(
            violations(&md, &ext(&[])),
            ["persia-storage -> persia-engine (build): not allowed"]
        );
        let md = metadata(&[("persia-format", "persia-engine", Some("dev"))], &[]);
        assert_ne!(violations(&md, &ext(&[])), Vec::<String>::new());
    }

    #[test]
    fn testutil_is_dev_only() {
        assert_eq!(
            violations(
                &metadata(&[("persia-engine", TESTUTIL, Some("dev"))], &[]),
                &ext(&[])
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            violations(
                &metadata(&[("persia-engine", TESTUTIL, None)], &[]),
                &ext(&[])
            ),
            [format!("persia-engine -> {TESTUTIL} (normal): not allowed")]
        );
    }

    #[test]
    fn self_dev_dependency_rejected() {
        assert_ne!(
            violations(
                &metadata(&[("persia-engine", "persia-engine", Some("dev"))], &[]),
                &ext(&[])
            ),
            Vec::<String>::new()
        );
        assert_ne!(
            violations(
                &metadata(&[(TESTUTIL, TESTUTIL, Some("dev"))], &[]),
                &ext(&[])
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn unlisted_member_reported_once() {
        let md = metadata(&[("persia-new", "persia-server", None)], &["persia-new"]);
        assert_eq!(
            violations(&md, &ext(&[])),
            ["persia-new: workspace member is not listed in xtask check-deps ALLOWED"]
        );
    }

    #[test]
    fn internal_crate_outside_workspace_rejected() {
        let md = metadata(&[("persia-server", "persia-client", None)], &[]);
        assert_eq!(
            violations(&md, &ext(&[])),
            ["persia-server -> persia-client (normal): internal crate outside the workspace"]
        );
    }

    #[test]
    fn external_dependencies_must_be_on_the_allowed_list() {
        assert_eq!(
            violations(
                &metadata(&[("persia-format", "zstd", None)], &[]),
                &ext(&["zstd"])
            ),
            Vec::<String>::new()
        );
        for kind in [None, Some("dev"), Some("build")] {
            let md = metadata(&[("persia-format", "left-pad", kind)], &[]);
            let kind = kind.unwrap_or("normal");
            assert_eq!(
                violations(&md, &ext(&["zstd"])),
                [format!(
                    "persia-format -> left-pad ({kind}): not in [workspace.dependencies] (allowed list)"
                )]
            );
        }
    }

    #[test]
    fn binary_only_crates_stay_out_of_libraries() {
        let md = metadata(&[("persia-engine", "anyhow", None)], &[]);
        assert_eq!(
            violations(&md, &ext(&["anyhow"])),
            ["persia-engine -> anyhow (normal): binary-only crate used by a library"]
        );
        let md = metadata(
            &[
                ("persia-cli", "clap", None),
                ("xtask", "anyhow", None),
                ("persia-engine", "anyhow", Some("dev")),
            ],
            &[],
        );
        assert_eq!(
            violations(&md, &ext(&["clap", "anyhow"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn dev_only_crates_stay_in_dev_dependencies() {
        for (krate, kind) in [
            ("persia-format", None),
            ("persia-format", Some("build")),
            ("persia-server", None),
        ] {
            assert_ne!(
                violations(
                    &metadata(&[(krate, "proptest", kind)], &[]),
                    &ext(&["proptest"])
                ),
                Vec::<String>::new()
            );
        }
        assert_eq!(
            violations(
                &metadata(&[("persia-format", "proptest", Some("dev"))], &[]),
                &ext(&["proptest"])
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn tooling_only_crates_stay_in_xtask() {
        assert_eq!(
            violations(&metadata(&[("xtask", "toml", None)], &[]), &ext(&["toml"])),
            Vec::<String>::new()
        );
        assert_eq!(
            violations(
                &metadata(&[("persia-cli", "toml", None)], &[]),
                &ext(&["toml"])
            ),
            ["persia-cli -> toml (normal): tooling-only crate used outside xtask"]
        );
    }

    #[test]
    fn tiers_are_on_the_real_allowed_list() {
        let root: toml::Table = include_str!("../../Cargo.toml").parse().unwrap();
        let external = workspace_dependencies(&root).unwrap();
        for name in BINARY_ONLY.iter().chain(DEV_ONLY).chain(TOOLING_ONLY) {
            assert!(
                external.contains(*name),
                "{name} missing from [workspace.dependencies]"
            );
        }
    }

    #[test]
    fn build_scripts_rejected() {
        let mut md = metadata(&[], &[]);
        md.packages[0].targets = vec![
            Target {
                kind: vec!["lib".into()],
                src_path: "/f/src/lib.rs".into(),
            },
            Target {
                kind: vec!["custom-build".into()],
                src_path: "/f/build.rs".into(),
            },
        ];
        assert_eq!(
            build_scripts(&md),
            ["persia-format: has a build script (/f/build.rs); commit generated code instead"]
        );
    }

    #[test]
    fn inherited_dependencies_pass() {
        let manifest: toml::Table = r#"
            [dependencies]
            zstd.workspace = true
            serde = { workspace = true, features = ["rc"] }
            [target.'cfg(unix)'.dependencies]
            memmap2.workspace = true
        "#
        .parse()
        .unwrap();
        assert_eq!(
            not_inherited("persia-format", &manifest),
            Vec::<String>::new()
        );
    }

    #[test]
    fn direct_versions_rejected_in_every_table() {
        let manifest: toml::Table = r#"
            [dependencies]
            zstd = "0.13"
            [dev-dependencies]
            proptest = { version = "1" }
            [build-dependencies]
            cc = { git = "https://example.com/cc" }
            [target.'cfg(unix)'.dependencies]
            libc = "0.2"
        "#
        .parse()
        .unwrap();
        assert_eq!(
            not_inherited("persia-format", &manifest),
            [
                "persia-format -> zstd ([dependencies]): must be `zstd.workspace = true`",
                "persia-format -> proptest ([dev-dependencies]): must be `proptest.workspace = true`",
                "persia-format -> cc ([build-dependencies]): must be `cc.workspace = true`",
                "persia-format -> libc ([target.'cfg(unix)'.dependencies]): must be `libc.workspace = true`",
            ]
        );
    }
}
