//! The placeholder binary fails loudly until the server exists (M11).

#![allow(missing_docs, clippy::unwrap_used)]

use std::process::Command;

#[test]
fn says_not_implemented_and_exits_non_zero() {
    let out = Command::new(env!("CARGO_BIN_EXE_persia-server"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "persia-server: not implemented (M11)\n"
    );
}
