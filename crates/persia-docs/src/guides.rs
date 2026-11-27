//! The website's guides whose Rust samples are doctests of this crate (ROADMAP 10.1.34, 10.1.42): each page is the
//! doc of an item here, so `cargo test` (rustdoc) compiles and runs every Rust block of it, and a sample that names a
//! removed or renamed item fails CI. Only rustdoc's doctest pass sees this module (`#[cfg(doctest)]` in `lib.rs`):
//! nothing of it is built or documented. It lives in this never-published crate, not in `persia`, because the pages
//! are outside `crates/persia/` and `cargo package` could not ship them.
//!
//! What a sample can name: a doctest gets an `--extern` for each of this crate's dependencies, which is `persia`
//! alone, with the features a user relies on (the default and `async`). Built that way, by
//! `cargo test -p persia-docs` (its own step in CI's test job and in `just test`), `persia::internal` (private
//! without the `internal` feature) and `persia_engine` (no `--extern`) fail to compile. Inside
//! `cargo test --workspace` they do not all: cargo's resolver unifies features across every package one invocation
//! builds, and `persia-cli` (`internal`) and `persia`'s own tests (`test-hooks`, which turns on `internal`) are among
//! them, so the one `persia` the doctests link has `persia::internal` public. A `#[doc(hidden)]` item compiles in both.
//! `guide_samples_are_doctests_of_the_public_api` in `xtask/src/repo_checks.rs` therefore also refuses, in these
//! pages' samples, what a guide has reached for (`persia_engine`, `persia::internal`, `QueryLimits`, the hidden
//! `Filter::from_json`), and checks that this module is wired and that every page it lists exists.
//!
//! The pages are Markdown for the website (Starlight, Expressive Code), which shows every line of a block, so their
//! samples are whole programs (a `fn main`) with no hidden `# ` lines; the other blocks are tagged (`json`, `text`),
//! which rustdoc does not run. A guide whose samples are not checked here is listed nowhere else: add a page to this
//! module when its samples are meant to compile.

/// `website/src/content/docs/guide/filtered-search.md`.
#[doc = include_str!("../../../website/src/content/docs/guide/filtered-search.md")]
pub struct FilteredSearch;

/// `website/src/content/docs/guide/typo-tolerant-search.md`.
#[doc = include_str!("../../../website/src/content/docs/guide/typo-tolerant-search.md")]
pub struct TypoTolerantSearch;
