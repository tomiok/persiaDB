//! Samples a page shows with a fixed path (`app.persia` in the working directory), which rustdoc cannot run as shown,
//! each as the doctest twin it must equal (ROADMAP 10.3.3, 10.1.51): a temporary directory made by a hidden line,
//! and `dir.path().join("app.persia")` for the path. `shown_samples_equal_a_doctest` in
//! `xtask/src/repo_checks/samples.rs` holds each shown block to its twin. The README quickstart's twin is the
//! getting-started guide's first sample (`guides.rs`), so it needs no item here. Only rustdoc's doctest pass sees this
//! module (`#[cfg(doctest)]` in `lib.rs`).

/// `website/src/content/docs/index.mdx`: the landing page's sample.
#[doc = include_str!("samples/landing.md")]
pub struct Landing;
