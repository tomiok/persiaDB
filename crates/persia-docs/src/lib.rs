//! Checks of the repository's documentation against `persia`'s public API (ROADMAP 10.1.34, 10.1.42). Never
//! published: it reads files outside its directory (the website's guides, `docs/sdk/c-abi.md`), which `cargo package`
//! cannot ship, so `persia` itself reaches nothing outside `crates/persia/` (a repo check in `xtask/src/repo_checks.rs`)
//! and can be published.
//!
//! - `guides`: the website guides whose Rust samples run as doctests of this crate;
//! - `tests/c_abi_codes.rs`: the C ABI's code table lists `persia::ErrorCode::ALL`.
//!
//! Nothing here is built outside rustdoc's doctest pass and `cargo test`: the library is empty.
#![forbid(unsafe_code)]

// Only rustdoc's doctest pass compiles it.
#[cfg(doctest)]
mod guides;
