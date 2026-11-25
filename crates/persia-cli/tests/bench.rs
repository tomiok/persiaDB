//! `persia bench`, end to end (ROADMAP 10.4.3): its `--json` shape, that a seed fixes the corpus and the queries
//! (timings are never asserted), that its scratch directory is removed and no existing file is touched, and that its
//! help and its output both say the numbers are local guidance only.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value as Json;

/// What every report and the help say (the crate's `bench::NOTICE`).
const NOT_A_BENCHMARK: &str = "local guidance only, not a published benchmark";

fn persia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_persia"))
        .args(args)
        .output()
        .unwrap()
}

fn path_arg(p: &Path) -> &str {
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

/// A small run in `dir`, as JSON: one line, nothing else.
fn bench_json(dir: &Path, docs: &str, queries: &str, seed: &str) -> Json {
    let out = persia(&[
        "bench",
        "--dir",
        path_arg(dir),
        "--docs",
        docs,
        "--queries",
        queries,
        "--seed",
        seed,
        "--json",
    ]);
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "{text}");
    serde_json::from_str(&text).unwrap()
}

fn keys(value: &Json) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn set<'a>(names: &[&'a str]) -> BTreeSet<&'a str> {
    names.iter().copied().collect()
}

fn entries(dir: &Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

/// What a run did, without what it measured: the work a seed fixes. The hits are part of it only while the
/// relaxation budget, which reads the wall clock, truncated no query of the kind (`truncated` is 0) in `both` runs.
fn work(report: &Json, both: &[&Json]) -> Json {
    let calm = |kind: usize| {
        both.iter()
            .all(|r| r["query_kinds"][kind]["truncated"].as_u64() == Some(0))
    };
    let kinds: Vec<Json> = report["query_kinds"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let hits = if calm(i) {
                k["hits"].clone()
            } else {
                Json::Null
            };
            serde_json::json!([k["kind"], k["count"], hits])
        })
        .collect();
    serde_json::json!([
        report["seed"],
        report["docs"],
        report["queries"],
        report["corpus"],
        report["query_mix"],
        kinds
    ])
}

#[test]
fn bench_json_has_its_documented_shape() {
    let dir = tempfile::tempdir().unwrap();
    let report = bench_json(dir.path(), "300", "52", "7");
    assert_eq!(
        keys(&report),
        set(&[
            "notice",
            "build",
            "seed",
            "docs",
            "queries",
            "corpus",
            "indexing",
            "query_mix",
            "query_kinds"
        ])
    );
    assert_eq!(
        keys(&report["corpus"]),
        set(&["fingerprint", "container_bytes"])
    );
    assert_eq!(
        keys(&report["indexing"]),
        set(&["nanos", "docs_per_second", "commit_nanos"])
    );
    assert_eq!(keys(&report["query_mix"]), set(&["fingerprint"]));
    let notice = report["notice"].as_str().unwrap();
    assert!(notice.starts_with(NOT_A_BENCHMARK), "{notice}");
    assert!(notice.contains("docs/benchmarks.md"), "{notice}");
    assert!(matches!(
        report["build"].as_str(),
        Some("debug" | "release")
    ));
    assert_eq!(report["seed"], 7);
    assert_eq!(report["docs"], 300);
    assert_eq!(report["queries"], 52);
    for print in [
        &report["corpus"]["fingerprint"],
        &report["query_mix"]["fingerprint"],
    ] {
        let print = print.as_str().unwrap();
        assert!(
            print.len() == 16 && print.bytes().all(|b| b.is_ascii_hexdigit()),
            "{print}"
        );
    }
    assert!(report["corpus"]["container_bytes"].as_u64().unwrap() > 12_288);
    let indexing = &report["indexing"];
    assert!(indexing["nanos"].is_u64(), "{indexing}");
    assert!(indexing["commit_nanos"].is_u64(), "{indexing}");
    assert!(
        indexing["docs_per_second"].is_u64() || indexing["docs_per_second"].is_null(),
        "{indexing}"
    );

    // The five kinds in rotation order; 52 queries are 10 of each and one more of the first two.
    let kinds = report["query_kinds"].as_array().unwrap();
    let found: Vec<(&str, u64)> = kinds
        .iter()
        .map(|k| (k["kind"].as_str().unwrap(), k["count"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        found,
        [
            ("term", 11),
            ("terms", 11),
            ("phrase", 10),
            ("filter", 10),
            ("typo", 10)
        ]
    );
    for kind in kinds {
        assert_eq!(
            keys(kind),
            set(&["kind", "count", "hits", "truncated", "latency_nanos"])
        );
        let latency = &kind["latency_nanos"];
        assert_eq!(keys(latency), set(&["p50", "p90", "p99", "max"]));
        let at = |key: &str| latency[key].as_u64().unwrap();
        assert!(
            at("p50") <= at("p90") && at("p90") <= at("p99") && at("p99") <= at("max"),
            "{kind}"
        );
        // Every query is drawn from the corpus, so each kind finds documents, at most a page of 20 per query; a
        // page the relaxation budget truncated (on a loaded machine) may hold fewer.
        let hits = kind["hits"].as_u64().unwrap();
        let count = kind["count"].as_u64().unwrap();
        let truncated = kind["truncated"].as_u64().unwrap();
        assert!(truncated <= count, "{kind}");
        assert!(hits <= count * 20, "{kind}");
        if truncated == 0 {
            assert!(hits >= count, "{kind}");
        }
    }
    assert_eq!(entries(dir.path()), 0, "the scratch directory is removed");
}

#[test]
fn the_same_seed_gives_the_same_corpus_and_queries() {
    let dir = tempfile::tempdir().unwrap();
    let first = bench_json(dir.path(), "200", "40", "42");
    let again = bench_json(dir.path(), "200", "40", "42");
    let both = [&first, &again];
    assert_eq!(work(&first, &both), work(&again, &both));
    let other = bench_json(dir.path(), "200", "40", "43");
    assert_ne!(
        first["corpus"]["fingerprint"],
        other["corpus"]["fingerprint"]
    );
    assert_ne!(
        first["query_mix"]["fingerprint"],
        other["query_mix"]["fingerprint"]
    );
    // A seed means the same work on every machine: this one is pinned, as in the generator's unit tests.
    let pinned = bench_json(dir.path(), "100", "100", "42");
    assert_eq!(pinned["corpus"]["fingerprint"], "ec2ceb787982b863");
    assert_eq!(pinned["query_mix"]["fingerprint"], "ce5ad2d8c903b087");
    assert_eq!(entries(dir.path()), 0);
}

#[test]
fn bench_text_says_first_that_its_numbers_are_local_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let out = persia(&[
        "bench",
        "--dir",
        path_arg(dir.path()),
        "--docs",
        "120",
        "--queries",
        "10",
    ]);
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with(&format!("notice            {NOT_A_BENCHMARK}")),
        "{text}"
    );
    assert!(lines[0].contains("docs/benchmarks.md"), "{text}");
    let rows: Vec<&str> = lines
        .iter()
        .take_while(|l| !l.is_empty())
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(
        rows,
        [
            "notice", "build", "seed", "corpus", "indexing", "commit", "queries"
        ]
    );
    // The default seed, and the sizes asked for.
    assert_eq!(lines[2], "seed              42");
    assert!(
        lines[3].starts_with("corpus            120 documents, fingerprint "),
        "{text}"
    );
    assert!(
        lines[4].contains(" docs/s (") || lines[4].contains("n/a ("),
        "{text}"
    );
    assert!(
        lines[6].starts_with("queries           10, fingerprint "),
        "{text}"
    );
    let table: Vec<Vec<&str>> = lines[8..]
        .iter()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(
        table[0],
        [
            "kind",
            "count",
            "hits",
            "truncated",
            "p50",
            "p90",
            "p99",
            "max"
        ]
    );
    let kinds: Vec<(&str, &str)> = table[1..].iter().map(|row| (row[0], row[1])).collect();
    assert_eq!(
        kinds,
        [
            ("term", "2"),
            ("terms", "2"),
            ("phrase", "2"),
            ("filter", "2"),
            ("typo", "2")
        ]
    );
    assert_eq!(entries(dir.path()), 0);
}

#[test]
fn bench_help_says_its_numbers_are_local_guidance_and_names_its_defaults() {
    let help = stdout(&persia(&["bench", "--help"]));
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("THE NUMBERS ARE LOCAL GUIDANCE ONLY, NOT A PUBLISHED BENCHMARK"),
        "{help}"
    );
    assert!(flat.contains("docs/benchmarks.md"), "{help}");
    for flag in [
        "--dir <DIR>",
        "--docs <DOCS>",
        "--queries <QUERIES>",
        "--seed <SEED>",
        "--json",
    ] {
        assert!(help.contains(flag), "{flag} in\n{help}");
    }
    for default in [
        "[default: the system temporary directory]",
        "[default: 20000]",
        "[default: 1000]",
        "[default: 42]",
    ] {
        assert!(flat.contains(default), "{default} in\n{help}");
    }
    assert!(
        flat.contains("Exits with status 1 on any failure"),
        "{help}"
    );
    let root = stdout(&persia(&["--help"]));
    assert!(
        root.lines().any(|l| l.trim_start().starts_with("bench ")),
        "{root}"
    );
}

/// The run works in a directory it creates: a database of the name it uses, and any other file there, is neither
/// opened nor changed nor removed.
#[test]
fn bench_never_touches_what_its_directory_already_holds() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("bench.persia");
    std::fs::write(&existing, b"a user's database").unwrap();
    let taken = dir.path().join("persia-bench-1-0");
    std::fs::create_dir(&taken).unwrap();
    std::fs::write(taken.join("bench.persia"), b"another one").unwrap();
    bench_json(dir.path(), "50", "5", "1");
    assert_eq!(std::fs::read(&existing).unwrap(), b"a user's database");
    assert_eq!(
        std::fs::read(taken.join("bench.persia")).unwrap(),
        b"another one"
    );
    assert_eq!(entries(dir.path()), 2);
}

#[test]
fn bench_fails_with_status_1_on_a_dir_that_is_not_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let file = dir.path().join("file");
    std::fs::write(&file, b"not a directory").unwrap();
    for base in [&missing, &file] {
        let out = persia(&["bench", "--dir", path_arg(base), "--docs", "10"]);
        assert_eq!(out.status.code(), Some(1), "{}", base.display());
        assert_eq!(out.stdout, b"");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("is not a directory"), "{err}");
    }
    assert!(!missing.exists());
    assert_eq!(std::fs::read(&file).unwrap(), b"not a directory");
    assert_eq!(entries(dir.path()), 1);
}

#[test]
fn bench_usage_errors_exit_with_2() {
    for args in [
        &["bench", "--docs", "0"][..],
        &["bench", "--docs", "many"],
        &["bench", "--queries", "-1"],
        &["bench", "--seed", "x"],
        &["bench", "--docs", "4294967296"],
        // `--queries` is capped, so a run cannot ask for gigabytes of latencies.
        &["bench", "--queries", "10000001"],
        &["bench", "--queries", "4294967295"],
        &["bench", "extra"],
    ] {
        let out = persia(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(out.stdout, b"", "{args:?}");
    }
}

/// No query at all is a run that only indexes: every kind is reported, without a latency.
#[test]
fn bench_without_queries_reports_the_indexing_alone() {
    let dir = tempfile::tempdir().unwrap();
    let report = bench_json(dir.path(), "40", "0", "3");
    for kind in report["query_kinds"].as_array().unwrap() {
        assert_eq!(kind["count"], 0);
        assert_eq!(kind["hits"], 0);
        assert_eq!(kind["truncated"], 0);
        assert!(kind["latency_nanos"].is_null(), "{kind}");
    }
    let text = stdout(&persia(&[
        "bench",
        "--dir",
        path_arg(dir.path()),
        "--docs",
        "40",
        "--queries",
        "0",
    ]));
    let last: Vec<&str> = text.lines().last().unwrap().split_whitespace().collect();
    assert_eq!(last, ["typo", "0", "0", "0", "-", "-", "-", "-"]);
}
