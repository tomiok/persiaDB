//! No async runtime in the embedded stack (ROADMAP 0.6.11, ADR-0065).
//!
//! The crates a program embeds ([`EMBEDDED`]) are synchronous: threads are ours (`std::thread`, joined on close).
//! None of them may have an async runtime ([`ASYNC_RUNTIMES`]) anywhere in its resolved dependency graph, as a
//! normal, build or dev-dependency, on any platform. `persia-server` may: it uses the embedded crates through their
//! synchronous API. The direct-dependency rules ([`super::violations`]) cannot see this: a runtime arrives through
//! a library that is itself allowed.
//!
//! The graph is the one `cargo metadata --all-features` resolves (the `resolve` object: one node per package, with the
//! edges cargo would build, every feature of every workspace member on, every target platform included). With the
//! default features only, an optional dependency behind a feature nobody enables by default would escape the walk. From an embedded crate the
//! walk follows every kind of edge; from any other package only normal and build edges, since a dependency's
//! dev-dependencies are not built for its dependents.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Deserialize;

/// The crates of the embedded stack: what an application links to open a database in-process, and the CLI.
pub(crate) const EMBEDDED: &[&str] = &[
    "persia",
    "persia-engine",
    "persia-blob",
    "persia-storage",
    "persia-format",
    "persia-analysis",
    "persia-cli",
];

/// Async runtimes, and the executors and reactors they are built from: a crate here schedules futures or drives
/// I/O readiness for them. `tokio` is one crate; `async-std` is `async-global-executor` over `async-executor`,
/// `async-io` and `blocking`; `smol` is the same parts. `futures-executor` is the executor of the `futures` crate.
/// The others are runtimes a dependency could bring instead. Crates that only define or combine futures
/// (`futures-core`, `futures-util`, `async-trait`, `pin-project`) are not runtimes and are not listed.
pub(crate) const ASYNC_RUNTIMES: &[&str] = &[
    "tokio",
    "async-std",
    "smol",
    "async-executor",
    "async-global-executor",
    "async-io",
    "blocking",
    "futures-executor",
    "glommio",
    "monoio",
    "tokio-uring",
    "actix-rt",
    "embassy-executor",
];

/// The subset of `cargo metadata --format-version 1` (with dependencies) this check reads.
#[derive(Debug, Deserialize)]
pub(crate) struct Graph {
    packages: Vec<GraphPackage>,
    resolve: Resolve,
}

#[derive(Debug, Deserialize)]
struct GraphPackage {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Debug, Deserialize)]
struct Node {
    id: String,
    #[serde(default)]
    deps: Vec<NodeDep>,
}

#[derive(Debug, Deserialize)]
struct NodeDep {
    /// The package id of the dependency.
    pkg: String,
    #[serde(default)]
    dep_kinds: Vec<DepKind>,
}

#[derive(Debug, Deserialize)]
struct DepKind {
    /// `None` for a normal dependency, else `"dev"` or `"build"`.
    kind: Option<String>,
}

impl NodeDep {
    /// Whether the edge is built for a dependent of its package: a normal or a build dependency. Cargo lists no
    /// kind at all for some edges of older formats: that is a normal dependency.
    fn built_for_dependents(&self) -> bool {
        self.dep_kinds.is_empty()
            || self
                .dep_kinds
                .iter()
                .any(|k| k.kind.as_deref() != Some("dev"))
    }

    /// `normal`, `build` or `dev`: the strongest kind the edge has.
    fn kind(&self) -> &'static str {
        let has = |kind: Option<&str>| self.dep_kinds.iter().any(|k| k.kind.as_deref() == kind);
        if self.dep_kinds.is_empty() || has(None) {
            "normal"
        } else if has(Some("build")) {
            "build"
        } else {
            "dev"
        }
    }
}

/// The error of an embedded crate the resolved graph lacks: renamed or removed, it would escape the rule silently.
const MISSING: &str = "not in the resolved dependency graph, so the no-async rule cannot check it (ADR-0065): \
                       update EMBEDDED in xtask/src/check_deps/no_async.rs";

/// One message per embedded crate and runtime it reaches, with the shortest path to it, and one per embedded crate
/// the graph lacks.
pub(crate) fn violations<'g>(graph: &'g Graph) -> Vec<String> {
    let names: BTreeMap<&str, &str> = graph
        .packages
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    let nodes: BTreeMap<&str, &Node> = graph
        .resolve
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n))
        .collect();
    let name_of = |id: &'g str| -> &'g str { names.get(id).copied().unwrap_or(id) };

    let mut errors = Vec::new();
    for root in EMBEDDED {
        // A workspace has one package per member name; a registry crate of the same name would be another id.
        let Some(root_id) = graph
            .packages
            .iter()
            .find(|p| p.name == *root && nodes.contains_key(p.id.as_str()))
            .map(|p| p.id.as_str())
        else {
            errors.push(format!("{root}: {MISSING}"));
            continue;
        };
        // Breadth-first, so the path recorded for each package is a shortest one.
        let mut via: BTreeMap<&str, (&str, &'static str)> = BTreeMap::new();
        let mut seen: BTreeSet<&str> = BTreeSet::from([root_id]);
        let mut queue = VecDeque::from([root_id]);
        while let Some(id) = queue.pop_front() {
            let Some(node) = nodes.get(id) else { continue };
            for dep in &node.deps {
                if id != root_id && !dep.built_for_dependents() {
                    continue;
                }
                if seen.insert(dep.pkg.as_str()) {
                    via.insert(dep.pkg.as_str(), (id, dep.kind()));
                    queue.push_back(dep.pkg.as_str());
                }
            }
        }
        let mut found: Vec<&str> = seen
            .iter()
            .copied()
            .filter(|id| ASYNC_RUNTIMES.contains(&name_of(id)))
            .collect();
        found.sort_by_key(|id| name_of(id));
        for runtime in found {
            let mut path = vec![name_of(runtime).to_owned()];
            let mut at = runtime;
            let mut first_kind = "normal";
            while let Some((parent, kind)) = via.get(at) {
                path.push(name_of(parent).to_owned());
                first_kind = kind;
                at = parent;
            }
            path.reverse();
            errors.push(format!(
                "{} ({first_kind}): async runtime in the embedded stack (ADR-0065)",
                path.join(" -> ")
            ));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`super::violations`] of a fake graph, without the errors of the embedded crates it does not list.
    fn violations(graph: &Graph) -> Vec<String> {
        super::violations(graph)
            .into_iter()
            .filter(|e| !e.ends_with(MISSING))
            .collect()
    }

    /// A resolved graph with one package per name in `edges` (from, to, kind) and in `extra`.
    fn graph(edges: &[(&str, &str, Option<&str>)], extra: &[&str]) -> Graph {
        let names: BTreeSet<&str> = edges
            .iter()
            .flat_map(|(from, to, _)| [*from, *to])
            .chain(extra.iter().copied())
            .collect();
        let id = |name: &str| format!("registry+{name}@1.0.0");
        Graph {
            packages: names
                .iter()
                .map(|name| GraphPackage {
                    id: id(name),
                    name: (*name).to_owned(),
                })
                .collect(),
            resolve: Resolve {
                nodes: names
                    .iter()
                    .map(|name| Node {
                        id: id(name),
                        deps: edges
                            .iter()
                            .filter(|(from, _, _)| from == name)
                            .map(|(_, to, kind)| NodeDep {
                                pkg: id(to),
                                dep_kinds: vec![DepKind {
                                    kind: kind.map(str::to_owned),
                                }],
                            })
                            .collect(),
                    })
                    .collect(),
            },
        }
    }

    const NONE: [&str; 0] = [];

    #[test]
    fn a_direct_runtime_is_rejected() {
        let g = graph(&[("persia-engine", "tokio", None)], &[]);
        assert_eq!(
            violations(&g),
            ["persia-engine -> tokio (normal): async runtime in the embedded stack (ADR-0065)"]
        );
    }

    #[test]
    fn a_transitive_runtime_is_rejected_with_its_path() {
        let g = graph(
            &[
                ("persia-storage", "object_store", None),
                ("object_store", "reqwest", None),
                ("reqwest", "tokio", None),
            ],
            &[],
        );
        assert_eq!(
            violations(&g),
            [
                "persia-storage -> object_store -> reqwest -> tokio (normal): async runtime in the embedded stack (ADR-0065)"
            ]
        );
    }

    #[test]
    fn a_runtime_reached_through_another_embedded_crate_is_reported_for_each() {
        let g = graph(
            &[
                ("persia", "persia-engine", None),
                ("persia-engine", "persia-storage", None),
                ("persia-storage", "smol", None),
            ],
            &[],
        );
        assert_eq!(
            violations(&g),
            [
                "persia -> persia-engine -> persia-storage -> smol (normal): async runtime in the embedded stack (ADR-0065)",
                "persia-engine -> persia-storage -> smol (normal): async runtime in the embedded stack (ADR-0065)",
                "persia-storage -> smol (normal): async runtime in the embedded stack (ADR-0065)",
            ]
        );
    }

    #[test]
    fn a_dev_only_runtime_is_rejected() {
        let g = graph(&[("persia", "tokio", Some("dev"))], &[]);
        assert_eq!(
            violations(&g),
            ["persia -> tokio (dev): async runtime in the embedded stack (ADR-0065)"]
        );
        // Also behind a dev-dependency that is itself harmless.
        let g = graph(
            &[
                ("persia-cli", "assert_cmd", Some("dev")),
                ("assert_cmd", "async-std", None),
            ],
            &[],
        );
        assert_eq!(
            violations(&g),
            [
                "persia-cli -> assert_cmd -> async-std (dev): async runtime in the embedded stack (ADR-0065)"
            ]
        );
    }

    #[test]
    fn a_build_dependency_runtime_is_rejected() {
        let g = graph(
            &[
                ("persia-format", "zstd-sys", None),
                ("zstd-sys", "fetcher", Some("build")),
                ("fetcher", "futures-executor", None),
            ],
            &[],
        );
        assert_eq!(
            violations(&g),
            [
                "persia-format -> zstd-sys -> fetcher -> futures-executor (normal): async runtime in the embedded stack (ADR-0065)"
            ]
        );
        let g = graph(&[("persia-analysis", "blocking", Some("build"))], &[]);
        assert_eq!(
            violations(&g),
            ["persia-analysis -> blocking (build): async runtime in the embedded stack (ADR-0065)"]
        );
    }

    #[test]
    fn the_server_may_depend_on_a_runtime() {
        let g = graph(
            &[
                ("persia-server", "tokio", None),
                ("persia-server", "axum", None),
                ("axum", "tokio", None),
                ("persia-server", "persia", None),
                ("persia", "persia-engine", None),
                ("persia-proto", "tonic", None),
                ("tonic", "tokio", None),
                ("xtask", "tokio", None),
            ],
            &[],
        );
        assert_eq!(violations(&g), NONE);
    }

    #[test]
    fn dev_dependencies_of_a_dependency_are_not_built_and_not_followed() {
        // `persia-testutil` is not an embedded crate: its own dev-dependencies are built only for its own tests.
        let g = graph(
            &[
                ("persia", "persia-testutil", Some("dev")),
                ("persia-testutil", "tokio", Some("dev")),
                ("persia-engine", "rayon", None),
                ("rayon", "smol", Some("dev")),
            ],
            &[],
        );
        assert_eq!(violations(&g), NONE);
        // But its normal dependencies are.
        let g = graph(
            &[
                ("persia", "persia-testutil", Some("dev")),
                ("persia-testutil", "tokio", None),
            ],
            &[],
        );
        assert_eq!(
            violations(&g),
            [
                "persia -> persia-testutil -> tokio (dev): async runtime in the embedded stack (ADR-0065)"
            ]
        );
    }

    #[test]
    fn crates_that_only_define_futures_are_not_runtimes() {
        let g = graph(
            &[
                ("persia-storage", "futures-core", None),
                ("persia-storage", "futures-util", None),
                ("persia-storage", "async-trait", None),
                ("persia-engine", "rayon", None),
                ("persia", "tokio-not", None),
            ],
            &[],
        );
        assert_eq!(violations(&g), NONE);
    }

    #[test]
    fn every_listed_runtime_is_rejected_and_cycles_end() {
        for runtime in ASYNC_RUNTIMES {
            let g = graph(
                &[
                    ("persia-blob", "a", None),
                    ("a", "b", None),
                    ("b", "a", None), // a dev-dependency cycle, as cargo allows between a crate and its tests
                    ("b", runtime, None),
                ],
                &[],
            );
            assert_eq!(
                violations(&g),
                [format!(
                    "persia-blob -> a -> b -> {runtime} (normal): async runtime in the embedded stack (ADR-0065)"
                )]
            );
        }
    }

    #[test]
    fn an_edge_with_several_kinds_is_named_by_the_strongest() {
        let both = |kinds: &[Option<&str>]| NodeDep {
            pkg: "x".to_owned(),
            dep_kinds: kinds
                .iter()
                .map(|k| DepKind {
                    kind: k.map(str::to_owned),
                })
                .collect(),
        };
        assert_eq!(both(&[Some("dev"), None]).kind(), "normal");
        assert_eq!(both(&[Some("dev"), Some("build")]).kind(), "build");
        assert_eq!(both(&[Some("dev")]).kind(), "dev");
        assert_eq!(both(&[]).kind(), "normal");
        assert!(both(&[Some("dev"), Some("build")]).built_for_dependents());
        assert!(both(&[]).built_for_dependents());
        assert!(!both(&[Some("dev")]).built_for_dependents());
    }

    #[test]
    fn the_real_metadata_shape_parses() {
        let json = r#"{
            "packages": [
                {"id": "path+file:///w/crates/persia#0.0.0", "name": "persia", "dependencies": []},
                {"id": "registry+https://github.com/rust-lang/crates.io-index#tokio@1.53.2", "name": "tokio"}
            ],
            "resolve": {"nodes": [
                {"id": "path+file:///w/crates/persia#0.0.0", "deps": [
                    {"name": "tokio", "pkg": "registry+https://github.com/rust-lang/crates.io-index#tokio@1.53.2",
                     "dep_kinds": [{"kind": "dev", "target": null}]}
                ], "features": []},
                {"id": "registry+https://github.com/rust-lang/crates.io-index#tokio@1.53.2", "deps": []}
            ], "root": null},
            "workspace_members": []
        }"#;
        let graph: Graph = serde_json::from_str(json).unwrap();
        assert_eq!(
            violations(&graph),
            ["persia -> tokio (dev): async runtime in the embedded stack (ADR-0065)"]
        );
    }

    #[test]
    fn an_embedded_crate_missing_from_the_graph_is_an_error() {
        let g = graph(&[("persia-engine", "rayon", None)], &[]);
        let missing: Vec<String> = super::violations(&g)
            .into_iter()
            .filter(|e| e.ends_with(MISSING))
            .collect();
        let named: Vec<&str> = missing.iter().filter_map(|e| e.split(':').next()).collect();
        let expected: Vec<&str> = EMBEDDED
            .iter()
            .copied()
            .filter(|c| *c != "persia-engine")
            .collect();
        assert_eq!(named, expected);
    }

    /// A throwaway workspace on disk: an embedded crate (`persia-engine`) whose `Cargo.toml` dependency section is
    /// `deps`, and a local crate named `tokio` it can depend on by path. Removed on drop.
    struct Workspace(std::path::PathBuf);

    impl Workspace {
        fn new(name: &str, features: &str, deps: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "persia-no-async-{name}-{}-{nanos}",
                std::process::id()
            ));
            let write = |path: &str, text: &str| {
                let path = root.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            };
            write(
                "Cargo.toml",
                "[workspace]\nmembers = [\"engine\", \"tokio\"]\nresolver = \"2\"\n",
            );
            write(
                "engine/Cargo.toml",
                &format!(
                    "[package]\nname = \"persia-engine\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
                     [features]\n{features}\n\n[dependencies]\n{deps}\n"
                ),
            );
            write("engine/src/lib.rs", "");
            write(
                "tokio/Cargo.toml",
                "[package]\nname = \"tokio\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
            );
            write("tokio/src/lib.rs", "");
            Self(root)
        }

        /// The workspace's violations, without the embedded crates it does not have.
        fn violations(&self) -> Vec<String> {
            let json =
                super::super::cargo_metadata(&self.0, &["--all-features", "--offline"]).unwrap();
            let graph: Graph = serde_json::from_slice(&json).unwrap();
            violations(&graph)
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            // A leftover directory in the temp dir is harmless; the test's assertions are what matter.
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                eprintln!("leaving {}: {e}", self.0.display());
            }
        }
    }

    #[test]
    fn a_runtime_behind_a_feature_nobody_enables_by_default_is_caught() {
        let ws = Workspace::new(
            "feature",
            "net = [\"dep:tokio\"]",
            "tokio = { path = \"../tokio\", optional = true }",
        );
        assert_eq!(
            ws.violations(),
            ["persia-engine -> tokio (normal): async runtime in the embedded stack (ADR-0065)"]
        );
    }

    #[test]
    fn a_renamed_runtime_is_caught_by_its_package_name() {
        let ws = Workspace::new(
            "renamed",
            "",
            "rt = { package = \"tokio\", path = \"../tokio\" }",
        );
        assert_eq!(
            ws.violations(),
            ["persia-engine -> tokio (normal): async runtime in the embedded stack (ADR-0065)"]
        );
    }
}
