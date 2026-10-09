//! Repository invariants checked as tests (`cargo test -p xtask`): toolchain pinning and
//! `.gitignore`/`.gitattributes` behavior, verified through git itself rather than by parsing the files.

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

    #[test]
    fn source_is_lf_text() {
        assert_eq!(attr("crates/persia/src/lib.rs", "eol"), "lf");
        assert_eq!(attr("crates/persia/src/lib.rs", "text"), "auto");
    }
}
