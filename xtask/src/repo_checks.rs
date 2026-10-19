//! Repository invariants checked as tests (`cargo test -p xtask`): toolchain pinning, fuzz target naming,
//! `.gitignore`/`.gitattributes` behavior (verified through git itself rather than by parsing the files) and
//! digest-pinned docker-compose images.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    fn read_toml(rel: &str) -> toml::Table {
        std::fs::read_to_string(root().join(rel))
            .unwrap()
            .parse()
            .unwrap()
    }

    fn get<'a>(table: &'a toml::Table, path: &[&str]) -> &'a toml::Value {
        let (first, rest) = path.split_first().unwrap();
        rest.iter().fold(&table[*first], |v, k| &v[*k])
    }

    fn git(args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(root())
            .output()
            .unwrap()
    }

    fn ignored(path: &str) -> bool {
        git(&["check-ignore", "--no-index", "-q", path])
            .status
            .success()
    }

    /// `git check-attr -z <name> -- <path>` prints `<path>\0<name>\0<value>\0`.
    fn attr(path: &str, name: &str) -> String {
        let out = git(&["check-attr", "-z", name, "--", path]);
        String::from_utf8(out.stdout)
            .unwrap()
            .split('\0')
            .nth(2)
            .unwrap()
            .to_owned()
    }

    // --- Toolchain (CLAUDE.md "Toolchain") ---

    #[test]
    fn channel_is_an_exact_release() {
        // "1.99" would float across patch releases and break reproducibility.
        let channel = get(&read_toml("rust-toolchain.toml"), &["toolchain", "channel"])
            .as_str()
            .unwrap()
            .to_owned();
        let parts: Vec<&str> = channel.split('.').collect();
        assert!(
            parts.len() == 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
            "{channel}"
        );
    }

    #[test]
    fn dod_gate_components_installed() {
        let components = get(
            &read_toml("rust-toolchain.toml"),
            &["toolchain", "components"],
        )
        .as_array()
        .unwrap()
        .clone();
        for needed in ["rustfmt", "clippy"] {
            assert!(
                components.iter().any(|c| c.as_str() == Some(needed)),
                "missing {needed}"
            );
        }
    }

    #[test]
    fn msrv_equals_pinned_toolchain() {
        let channel = get(&read_toml("rust-toolchain.toml"), &["toolchain", "channel"]).clone();
        let msrv = get(
            &read_toml("Cargo.toml"),
            &["workspace", "package", "rust-version"],
        )
        .clone();
        assert_eq!(
            msrv, channel,
            "update rust-version and rust-toolchain.toml together"
        );
    }

    #[test]
    fn nightly_is_dated() {
        // Nightly-only tools (Miri, cargo-fuzz) must be reproducible too.
        let nightly = std::fs::read_to_string(root().join(".github/nightly-toolchain")).unwrap();
        let date = nightly.trim().strip_prefix("nightly-").unwrap();
        let b = date.as_bytes();
        assert!(
            b.len() == 10
                && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 {
                    *c == b'-'
                } else {
                    c.is_ascii_digit()
                }),
            "{nightly}"
        );
    }

    #[test]
    fn every_member_inherits_msrv() {
        let members = get(&read_toml("Cargo.toml"), &["workspace", "members"])
            .as_array()
            .unwrap()
            .clone();
        let mut manifests = Vec::new();
        for pattern in members.iter().map(|m| m.as_str().unwrap()) {
            match pattern.strip_suffix("/*") {
                Some(dir) => {
                    for entry in std::fs::read_dir(root().join(dir)).unwrap() {
                        let manifest = entry.unwrap().path().join("Cargo.toml");
                        if manifest.exists() {
                            manifests.push(manifest);
                        }
                    }
                }
                None => manifests.push(root().join(pattern).join("Cargo.toml")),
            }
        }
        assert!(manifests.len() > 1);
        for manifest in manifests {
            let table: toml::Table = std::fs::read_to_string(&manifest).unwrap().parse().unwrap();
            assert_eq!(
                table["package"]
                    .get("rust-version")
                    .and_then(|v| v.get("workspace")),
                Some(&toml::Value::Boolean(true)),
                "{}",
                manifest.display()
            );
        }
    }

    // --- Fuzz targets (ROADMAP 1.11.1) ---

    #[test]
    fn fuzz_bins_are_named_after_their_files() {
        // .github/workflows/fuzz.yml builds its matrix from fuzz_targets/*.rs and runs `cargo fuzz run <stem>`.
        let mut bins: Vec<(String, String)> = get(&read_toml("fuzz/Cargo.toml"), &["bin"])
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                let field = |k: &str| b[k].as_str().unwrap().to_owned();
                (field("name"), field("path"))
            })
            .collect();
        bins.sort();
        let mut files: Vec<(String, String)> = std::fs::read_dir(root().join("fuzz/fuzz_targets"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter_map(|f| {
                let stem = f.strip_suffix(".rs")?.to_owned();
                Some((stem, format!("fuzz_targets/{f}")))
            })
            .collect();
        files.sort();
        assert_ne!(files.len(), 0);
        assert_eq!(bins, files);
    }

    // --- .gitignore / .gitattributes (golden fixtures must stay byte-exact, SPEC §15) ---

    #[test]
    fn must_commit_paths_are_not_ignored() {
        for path in [
            "tests/fixtures/v1/basic.persia",
            "tests/fixtures/v1/basic.persia.blobs",
            "crates/persia-engine/tests/fixtures/seg.persia",
            "crates/persia-format/proptest-regressions/frame.txt",
            "crates/persia-format/src/snapshots/header.snap",
            "fuzz/corpus/frame_scanner/seed1",
            "fuzz/Cargo.lock",
            "Cargo.lock",
            ".claude/settings.json",
        ] {
            assert!(!ignored(path), "{path} must be committable");
        }
    }

    #[test]
    fn build_and_scratch_outputs_are_ignored() {
        for path in [
            "target/debug/persia",
            "fuzz/target/x",
            "fuzz/artifacts/frame_scanner/crash-1",
            "fuzz/corpus-local/reader/abc",
            "app.persia",
            "app.persia.blobs",
            "crates/persia-format/src/snapshots/header.snap.new",
            ".claude/settings.local.json",
        ] {
            assert!(ignored(path), "{path} must be ignored");
        }
    }

    #[test]
    fn fixtures_are_binary() {
        // `binary` = -text -diff -merge: no EOL conversion can corrupt golden bytes.
        for path in [
            "tests/fixtures/v1/basic.persia",
            "tests/fixtures/v1/frame.txt",
            "crates/persia-format/tests/fixtures/v1/frame.dat",
            "sdk/go/testdata/a.golden",
            "fuzz/corpus/x/seed",
            "a.bin",
            "seed.zst",
        ] {
            assert_eq!(attr(path, "text"), "unset", "{path}");
            assert_eq!(attr(path, "diff"), "unset", "{path}");
        }
    }

    // --- Safety (CLAUDE.md "Safety & errors") ---

    /// Files that may `allow(unsafe_code)`, each justified by an ADR. Adding one means amending ADR-0010 (or writing
    /// a new ADR) first.
    const UNSAFE_ALLOWED: &[&str] = &[
        "crates/persia-format/src/mmap.rs",              // ADR-0010
        "crates/persia-format/tests/mmap_rename.rs",     // ADR-0010: maps its own temporary files
        "crates/persia-format/tests/source_props.rs",    // ADR-0010: maps its own temporary files
        "crates/persia-storage/src/container/mapped.rs", // ADR-0017
        "crates/persia-storage/src/sys.rs",              // ADR-0021
    ];

    #[test]
    fn unsafe_code_is_relaxed_only_where_an_adr_allows_it() {
        // This file names the lint in its own checks.
        let out = git(&[
            "grep",
            "-n",
            "unsafe_code",
            "--",
            "*.rs",
            ":(exclude)xtask/src/repo_checks.rs",
        ]);
        // `git grep` exits 1 when nothing matches; anything else means the listing cannot be trusted.
        assert!(
            matches!(out.status.code(), Some(0 | 1)),
            "git grep failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let listing = String::from_utf8(out.stdout).unwrap();
        let offenders: Vec<&str> = listing
            .lines()
            .filter(|line| {
                let (file, rest) = line.split_once(':').unwrap();
                let text = rest.split_once(':').unwrap().1.trim();
                // A crate root may forbid the lint, or (ADR-0017) deny it so one module can allow it: neither
                // relaxes it by itself.
                !matches!(text, "#![forbid(unsafe_code)]" | "#![deny(unsafe_code)]")
                    && !UNSAFE_ALLOWED.contains(&file)
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "unsafe_code relaxed outside UNSAFE_ALLOWED: {offenders:#?}"
        );
    }

    // --- Dev environment (ROADMAP 2.5.1) ---

    /// `name[:tag]@sha256:<64 lowercase hex>`: a tag alone can be moved or deleted upstream.
    fn is_pinned(image: &str) -> bool {
        image.split_once("@sha256:").is_some_and(|(name, digest)| {
            !name.is_empty()
                && digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
    }

    #[test]
    fn pin_check_rejects_tags_and_bad_digests() {
        let digest = "0123456789abcdef".repeat(4);
        assert!(is_pinned(&format!("a/b:1@sha256:{digest}")));
        assert!(is_pinned(&format!("a/b@sha256:{digest}")));
        assert!(!is_pinned("a/b:1"));
        assert!(!is_pinned("a/b:latest"));
        assert!(!is_pinned(&format!("@sha256:{digest}")));
        assert!(!is_pinned(&format!("a/b:1@sha256:{}", "0".repeat(63))));
        assert!(!is_pinned(&format!(
            "a/b:1@sha256:{}",
            digest.to_uppercase()
        )));
    }

    #[test]
    fn compose_images_are_pinned_by_digest() {
        let listing = git(&["ls-files", "--cached", "--others", "--exclude-standard"]);
        let files: Vec<String> = String::from_utf8(listing.stdout)
            .unwrap()
            .lines()
            .filter(|f| {
                let path = Path::new(f);
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                (stem.starts_with("docker-compose") || stem.starts_with("compose"))
                    && (ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml"))
            })
            .map(str::to_owned)
            .collect();
        assert!(
            files.iter().any(|f| f == "docker-compose.dev.yml"),
            "{files:?}"
        );
        for file in files {
            let text = std::fs::read_to_string(root().join(&file)).unwrap();
            let images: Vec<&str> = text
                .lines()
                .filter_map(|l| l.trim().strip_prefix("image:"))
                .map(|i| {
                    let value = i.split_once(" #").map_or(i, |(v, _)| v);
                    value.trim().trim_matches(['"', '\''])
                })
                .collect();
            assert_ne!(images.len(), 0, "{file}: no images");
            for image in images {
                assert!(is_pinned(image), "{file}: {image} is not pinned by digest");
            }
        }
    }

    #[test]
    fn source_is_lf_text() {
        assert_eq!(attr("crates/persia/src/lib.rs", "eol"), "lf");
        assert_eq!(attr("crates/persia/src/lib.rs", "text"), "auto");
    }
}
