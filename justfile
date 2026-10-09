# Developer entry points (ROADMAP 0.3.1). `just` lists recipes; `just check` mirrors CI.
# Toolchain comes from rust-toolchain.toml.

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
    cargo test --workspace --all-features

# Dependency direction/allowlist, script tests, PROGRESS.md freshness
repo-checks:
    python3 scripts/check_deps.py
    python3 -m unittest discover -s scripts
    python3 scripts/progress.py --check

# Licenses, advisories, banned engines
deny:
    cargo deny check

# Everything CI runs, in CI order
check: lint test repo-checks deny

# Regenerate PROGRESS.md and record today's snapshot (after ticking a ROADMAP leaf)
progress:
    python3 scripts/progress.py --record

# Fuzz one target for N seconds (uses fuzz/'s own dated nightly; ROADMAP 0.2.3.2)
fuzz target seconds="60":
    @test -d fuzz || { echo "no fuzz/ crate yet (ROADMAP 0.2.3.2)"; exit 1; }
    cd fuzz && cargo fuzz run {{ target }} -- -max_total_time={{ seconds }}

# Criterion benchmarks (optionally one package)
bench package="persia-engine":
    cargo bench -p {{ package }}

# Cloud-emulator integration tests: MinIO, fake-gcs-server, Azurite. The body lands with ROADMAP 0.3.2.
integration:
    @test -f docker-compose.test.yml || { echo "no docker-compose.test.yml yet (ROADMAP 0.3.2)"; exit 1; }
