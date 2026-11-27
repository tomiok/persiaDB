//! The website's guides whose Rust samples are doctests of this crate (ROADMAP 10.1.34): each page is the doc of an
//! item here, so `cargo test` (rustdoc) compiles and runs every fenced `rust` block of it, and a sample that names a
//! removed or renamed item fails CI. Only rustdoc's doctest pass sees this module (`#[cfg(doctest)]` in `lib.rs`):
//! nothing of it is built or documented.
//!
//! A doctest builds with this crate's dev-dependency features (`test-hooks` turns on `internal`) and every dependency
//! as an `--extern`, so a sample could still reach `persia_engine`, `persia::internal` or a `#[doc(hidden)]` item.
//! `guide_samples_are_doctests_of_the_public_api` in `xtask/src/repo_checks.rs` refuses the ones a guide has
//! reached for (`persia_engine`, `persia::internal`, `QueryLimits`, the hidden `Filter::from_json`) in these pages'
//! samples, and checks that this module is wired and that every page it lists exists. Review catches the rest.
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
