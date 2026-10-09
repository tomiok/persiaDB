# Developer entry points (ROADMAP 0.3.1). `just` lists recipes; `just check` mirrors CI.
# Toolchain comes from rust-toolchain.toml.

# Dated nightly for Miri and fuzzing (single source of truth)
nightly := `cat .github/nightly-toolchain`

# Show available recipes
default:
    @just --list

# Format all code
fmt:
    cargo fmt --all

# fmt check + clippy with warnings as errors
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Unit + property tests (fast suite)
test:
    cargo test --workspace

# Extended property tests (what nightly CI runs)
test-slow:
    PROPTEST_CASES=10000 cargo test --workspace --features slow --release

# Dependency direction/allowlist, script tests, PROGRESS.md freshness
repo-checks:
    cargo xtask check-deps
    python3 -m unittest discover -s scripts
    cargo xtask progress --check

# Undefined-behavior check of persia-format under Miri (needs the dated nightly with miri)
miri:
    cargo +{{ nightly }} miri test -p persia-format --lib
    cargo +{{ nightly }} miri test -p persia-format --doc

# Licenses, advisories, banned engines
deny:
    cargo deny check

# Everything CI runs, in CI order
check: lint test repo-checks deny

# Regenerate PROGRESS.md and record today's snapshot (after ticking a ROADMAP leaf)
progress:
    cargo xtask progress --record

# Fuzz one target for N seconds (dated nightly). New inputs go to the git-ignored fuzz/corpus-local/;
# committed seeds in fuzz/corpus/ are only read (minimize with `cargo fuzz cmin` before adding any)
fuzz target seconds="60":
    @test -d fuzz || { echo "no fuzz/ crate yet (ROADMAP 0.2.3.2)"; exit 1; }
    mkdir -p fuzz/corpus-local/{{ target }}
    cd fuzz && cargo +{{ nightly }} fuzz run {{ target }} corpus-local/{{ target }} corpus/{{ target }} -- -max_total_time={{ seconds }}

# Criterion benchmarks (optionally one package)
bench package="persia-engine":
    cargo bench -p {{ package }}

# Cloud-emulator integration tests: MinIO, fake-gcs-server, Azurite (ROADMAP 12.2.4)
integration:
    @echo "not implemented yet: emulator integration tests land with M12 (ROADMAP 12.2.4)"; exit 1
