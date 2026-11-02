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

# Miri covers persia-format and the pure parsers of persia-engine (ROADMAP 0.6.6): documents (CBOR, datetimes),
# tombstone sidecars, segment section kinds, norms, the FST check, the manifest codec, WAL chunks and the document
# encoding. Left out: property tests and golden-fixture readers (Miri's isolation has no filesystem: proptest's
# failure persistence calls `getcwd`), and exhaustive sweeps that take minutes interpreted (each over 90 s). Native
# runs cover all of those. CI runs this recipe.

# Undefined-behavior check under Miri: persia-format and the engine's pure parsers (needs the dated nightly)
miri:
    cargo +{{ nightly }} miri test -p persia-format --lib
    cargo +{{ nightly }} miri test -p persia-format --doc
    cargo +{{ nightly }} miri test -p persia-engine --lib -- document:: tombstone:: segment::kind segment::norms segment::read::fst_check {{ miri_engine_lib_skips }}
    cargo +{{ nightly }} miri test -p persia-engine --test manifest --test wal --test document_encoding -- {{ miri_engine_codec_skips }}

# Tests `just miri` leaves out of the engine parsers, by reason (see the recipe). `--skip` matches substrings.
miri_engine_lib_skips := "--skip format_parses_back_across_the_range --skip every_single_byte_flip_never_panics --skip every_written_fst_passes --skip paths_longer_than_the_bound_are_rejected --skip truncations_are_rejected --skip norms::tests::monotonic --skip rounds_down_by_under_an_eighth"
miri_engine_codec_skips := "--skip a_diff_planned_on_an_older_manifest_applies_iff_its_segments_are_unchanged --skip any_json_form_with_every_option_reads_back --skip apply_accepts_only_valid_transitions --skip arbitrary_bytes_never_panic --skip commit_chains_round_trip --skip damaged_manifests_never_panic --skip diff_between_any_two_manifests_is_sound --skip golden_fixtures --skip constructors_accept_exactly_valid_names --skip generated_chunks_detect_truncation_and_bit_flips --skip records_round_trip_and_reencode_identically --skip writer_matches_its_model --skip every_truncation_is_unexpected_eof --skip exactly_one_encoding --skip head_damage_is_rejected_or_canonical --skip every_bit_flip --skip every_resealed --skip a_fault_at_every_call_poisons_the_writer --skip each_corruption_is_reported_at_its_offset --skip ids_and_payloads_at_their_limits_decode --skip truncating_at_every_byte --skip decoder_limits --skip every_non_canonical_rewrite --skip hostile_nesting --skip rotation_closes_chunks_at_the_target_and_keeps_order --skip a_stored_schema_leaving_any_option_to_defaults_is_corrupt --skip every_strict_prefix_is_corrupt"

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
    docker compose -f docker-compose.dev.yml down --volumes
