//! Fuzz the remote blob key parser (ROADMAP 9.3.1; SPEC §9.4, ADR-0050): a listing returns keys Persia may not have
//! written (a shared bucket), so `RemoteKey::parse` reads hostile text. Arbitrary input never panics, and a key that
//! parses is exactly the key its value writes (one text per key), so a foreign key is never taken for a blob's.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_blob::remote::RemoteKey;
use persia_storage::Key;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    let Ok(key) = Key::new(text) else {
        return;
    };
    let Some(parsed) = RemoteKey::parse(&key) else {
        return;
    };
    let back = parsed.key().expect("a parsed key writes back");
    assert_eq!(back.as_str(), text, "a parsed key is canonical");
});
