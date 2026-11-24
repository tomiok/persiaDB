//! Fuzz the cursor token (ROADMAP 10.1.10; SPEC §7.4, ADR-0062): no text panics `Cursor::from_token`, and a token it
//! accepts is the one encoding of its cursor (`to_token` gives the same text back).
//!
//! Each input is read twice: as the token's text, and as the bytes after the version byte, framed here with the
//! version and a matching CRC-32C and written as base64url, so the fuzzer reaches every field's reader instead of
//! stopping at the checksum.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::db::Cursor;
use persia_engine::search::request::CURSOR_TOKEN_VERSION;
use persia_format::checksum::crc32c;

/// Base64url without padding, written here and not taken from the engine.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .chain(std::iter::repeat(&0))
            .take(3)
            .fold(0_u32, |n, &b| (n << 8) | u32::from(b));
        for i in 0..=chunk.len() {
            let sextet = usize::try_from((n >> (18 - 6 * i)) & 63).unwrap_or(0);
            out.push(char::from(ALPHABET[sextet]));
        }
    }
    out
}

fn check(token: &str) {
    if let Ok(cursor) = Cursor::from_token(token) {
        assert_eq!(cursor.to_token(), token);
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        check(text);
    }
    let mut framed = Vec::with_capacity(data.len() + 5);
    framed.push(CURSOR_TOKEN_VERSION);
    framed.extend_from_slice(data);
    framed.extend(crc32c(&framed).to_le_bytes());
    check(&base64url(&framed));
});
