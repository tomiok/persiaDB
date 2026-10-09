//! Repo tooling for Persia DB, run as `cargo xtask <command>` (alias in `.cargo/config.toml`).
//!
//! Written in Rust so the only tool a contributor needs is the pinned toolchain.
#![forbid(unsafe_code)]

mod check_deps;
mod progress;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};

const USAGE: &str = "usage: cargo xtask <command>

commands:
  check-deps   enforce crate dependency rules (CLAUDE.md \"Architecture\", \"Dependencies\")
  progress     regenerate PROGRESS.md from ROADMAP.md [--record [--date YYYY-MM-DD] | --check]";

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Returns whether the command passed.
fn run() -> Result<bool> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("check-deps") => check_deps::run(&repo_root()?),
        Some("progress") => progress::run(&repo_root()?, rest),
        _ => {
            eprintln!("{USAGE}");
            Ok(false)
        }
    }
}

/// The repository root: the parent of this crate's directory.
fn repo_root() -> Result<PathBuf> {
    let xtask_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    xtask_dir
        .parent()
        .map(PathBuf::from)
        .context("xtask must live one level below the repository root")
}
