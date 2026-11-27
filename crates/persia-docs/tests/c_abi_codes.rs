//! The C ABI's code table (`docs/sdk/c-abi.md`) is the contract of the language bindings (ADR-0018, ADR-0063 item 7):
//! it must not drift from `persia::ErrorCode`. `persia`'s own unit test pins `ErrorCode::ALL` to its append-only
//! snapshot; this one pins the document to `ErrorCode::ALL`. It lives here, not in `persia`, because it reads a file
//! outside `crates/persia/` (ROADMAP 10.1.42).

use persia::ErrorCode;

#[test]
fn the_c_abi_table_lists_the_same_codes() {
    let doc = include_str!("../../../docs/sdk/c-abi.md");
    for code in ErrorCode::ALL {
        let row = format!("| {} | `{}` |", code.number(), code.as_str());
        assert!(doc.contains(&row), "docs/sdk/c-abi.md lacks the row {row}");
    }
    assert!(doc.contains("| 0 | `PERSIA_OK` |"));
    let rows = doc
        .lines()
        .filter(|line| line.starts_with("| ") && line.contains("| `PERSIA_"))
        .count();
    assert_eq!(
        rows,
        ErrorCode::ALL.len() + 1,
        "a code the enum does not have"
    );
}
