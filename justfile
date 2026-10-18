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
    cargo fmt --manifest-path fuzz/Cargo.toml --all

# fmt check + clippy with warnings as errors (workspace and fuzz/)
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo fmt --manifest-path fuzz/Cargo.toml --all -- --check
    cargo clippy --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings

# Unit + property tests (fast suite)
test:
    cargo test --workspace

# Extended property tests (what nightly CI runs)
test-slow:
    PROPTEST_CASES=10000 cargo test --workspace --features slow --release

# Dependency direction/allowlist, script tests, PROGRESS.md freshness
repo-checks:
    cargo xtask check-deps
    cargo xtask progress --check
    cargo xtask fixtures --check

# Undefined-behavior check of persia-format under Miri (needs the dated nightly with miri)
miri:
    cargo +{{ nightly }} miri test -p persia-format --lib
    cargo +{{ nightly }} miri test -p persia-format --doc

# Licenses, advisories, banned engines (workspace and fuzz/Cargo.lock)
deny:
    cargo deny check
    cargo deny --manifest-path fuzz/Cargo.toml check

# Everything CI runs, in CI order
check: lint test repo-checks deny

# Regenerate the golden container fixtures of the current format version (tests/fixtures/format/v<major>.<minor>/)
fixtures:
    cargo xtask fixtures

# Regenerate PROGRESS.md and record today's snapshot (after ticking a ROADMAP leaf)
progress:
    cargo xtask progress --record

# Fuzz one target for N seconds (dated nightly). New inputs go to the git-ignored fuzz/corpus-local/;
# committed seeds in fuzz/corpus/ are only read (minimize with `cargo fuzz cmin` before adding any)
fuzz target seconds="60":
    @test -d fuzz || { echo "no fuzz/ crate yet (ROADMAP 0.2.3.2)"; exit 1; }
    mkdir -p fuzz/corpus-local/{{ target }} fuzz/corpus/{{ target }}
    cd fuzz && cargo +{{ nightly }} fuzz run {{ target }} corpus-local/{{ target }} corpus/{{ target }} -- -max_total_time={{ seconds }}

# Criterion benchmarks (optionally one package)
bench package="persia-engine":
    cargo bench -p {{ package }}

# Line coverage report (what nightly CI uploads); needs cargo-llvm-cov and the llvm-tools component
coverage:
    cargo llvm-cov --workspace --html

# Cloud-emulator integration tests: MinIO, fake-gcs-server, Azurite (ROADMAP 12.2.4)
integration:
    @echo "not implemented yet: emulator integration tests land with M12 (ROADMAP 12.2.4)"; exit 1

# Start the cloud-storage emulators in the background (docker-compose.dev.yml; needs Docker)
dev-up:
    docker compose -f docker-compose.dev.yml up -d

# Stop the emulators and drop their data
dev-down:
    docker compose -f docker-compose.dev.yml down
