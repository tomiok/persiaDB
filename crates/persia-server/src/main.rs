//! Persia DB gRPC + HTTP server (SPEC §12).
//!
//! **Placeholder for M11.** No server is implemented: the binary says so and exits with status 1, so a script or a
//! container that starts it fails instead of appearing to run.
#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("persia-server: not implemented (M11)");
    ExitCode::FAILURE
}
