//! Fuzz the location `Db::open` takes (ROADMAP 10.1.1; SPEC §8.1, §10.2): arbitrary text never panics; text that is
//! not `scheme://...` is the path itself; a `file://` URL is the only one that yields a path, and every refusal is
//! `InvalidArgument` or `Unsupported`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia::ErrorCode;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let url = text.split_once("://").is_some_and(|(scheme, _)| {
        let mut chars = scheme.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    match persia::internal::location_path(text) {
        Ok(path) => {
            if !url {
                assert_eq!(path.as_os_str(), text, "a path is taken as it is");
            } else {
                assert!(
                    text.get(..7)
                        .is_some_and(|s| s.eq_ignore_ascii_case("file://"))
                );
            }
        }
        Err(e) => {
            assert!(url || text.is_empty(), "a path is never refused: {text:?}");
            assert!(matches!(
                e.code(),
                ErrorCode::InvalidArgument | ErrorCode::Unsupported
            ));
        }
    }
});
