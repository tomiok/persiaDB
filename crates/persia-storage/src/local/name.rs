//! File names of the local backend: key segments encoded to ASCII (SPEC §10.1.1, ADR-0012).
//!
//! Bytes `a`–`z`, `0`–`9`, `.`, `_`, `-` are kept and every other byte is `%XX` (uppercase hex), so names survive
//! case-insensitive and normalizing filesystems. A directory name is an encoded segment plus [`DIR_MARKER`], which
//! an encoded segment never contains, so the object `a` and the directory of `a/…` never collide.

use crate::key::KeyDefect;

/// Ends every directory name.
pub(super) const DIR_MARKER: char = '@';

/// Longest file name, in bytes, that the common local filesystems (ext4, XFS, APFS, NTFS) accept.
pub(super) const MAX_NAME_LEN: usize = 255;

/// Longest path, in bytes, `open` and friends accept: `PATH_MAX` minus the terminating NUL. Windows has no such limit
/// for the absolute paths the backend uses (the standard library passes them as `\\?\` paths).
/// Linux and Android define `PATH_MAX` as 4096; macOS, the BSDs and illumos as 1024.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) const MAX_PATH_LEN: Option<usize> = Some(4095);
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
pub(super) const MAX_PATH_LEN: Option<usize> = Some(1023);
#[cfg(not(unix))]
pub(super) const MAX_PATH_LEN: Option<usize> = None;

/// Longest temporary file name (`@tmp.<16 hex digits>.<u64>`, SPEC §10.1.1): it lives next to the object.
pub(super) const MAX_TEMP_NAME_LEN: usize = 5 + 16 + 1 + 20;

// Kept as is: lowercase only, so a case-insensitive filesystem never folds two names together.
fn is_plain(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')
}

/// The encoded form of `segment`; not checked against [`MAX_NAME_LEN`].
pub(super) fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        if is_plain(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_char(byte / 16));
            out.push(hex_char(byte % 16));
        }
    }
    out
}

fn hex_char(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16).map_or('?', |c| c.to_ascii_uppercase())
}

fn hex_digit(byte: u8) -> Option<u8> {
    if byte.is_ascii_lowercase() {
        return None; // lowercase hex is not canonical
    }
    char::from(byte)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// The segment `name` encodes, if `name` is canonical (exactly what [`encode`] produces) and decodes to a
/// non-empty UTF-8 segment without `/`. Any other name was not written by Persia.
pub(super) fn decode(name: &str) -> Option<String> {
    let mut out = Vec::with_capacity(name.len());
    let mut bytes = name.bytes();
    while let Some(byte) = bytes.next() {
        if is_plain(byte) {
            out.push(byte);
            continue;
        }
        if byte != b'%' {
            return None;
        }
        let high = hex_digit(bytes.next()?)?;
        let low = hex_digit(bytes.next()?)?;
        let decoded = high.checked_mul(16)?.checked_add(low)?;
        if is_plain(decoded) {
            return None; // `%61` for `a`: a second name for the same segment
        }
        out.push(decoded);
    }
    if out.is_empty() || out.contains(&b'/') {
        return None;
    }
    String::from_utf8(out).ok()
}

/// The file name of an object whose last key segment is `segment`.
pub(super) fn file_name(segment: &str) -> Result<String, KeyDefect> {
    checked(encode(segment))
}

/// The name of the directory holding the objects under `segment/`.
pub(super) fn dir_name(segment: &str) -> Result<String, KeyDefect> {
    let mut name = encode(segment);
    name.push(DIR_MARKER);
    checked(name)
}

fn checked(name: String) -> Result<String, KeyDefect> {
    if name.len() > MAX_NAME_LEN {
        return Err(KeyDefect::NameTooLong { len: name.len() });
    }
    if cfg!(windows) && is_windows_reserved(&name) {
        return Err(KeyDefect::WindowsName);
    }
    Ok(name)
}

/// Whether Windows rejects or rewrites `name` (an encoded name, so ASCII without spaces): a reserved device name,
/// alone or before a `.`, in any case, or a trailing `.`.
pub(super) fn is_windows_reserved(name: &str) -> bool {
    const DEVICES: [&str; 4] = ["con", "prn", "aux", "nul"];
    let stem = name.split('.').next().unwrap_or(name);
    let numbered = |base: &str| {
        stem.len() == 4
            && stem.get(..3).is_some_and(|s| s.eq_ignore_ascii_case(base))
            && matches!(stem.as_bytes().get(3), Some(b'1'..=b'9'))
    };
    name.ends_with('.')
        || DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d))
        || numbered("com")
        || numbered("lpt")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden: these names are on disk in every local store (ADR-0012); changing one breaks existing stores.
    #[test]
    fn encoding_is_pinned() {
        for (segment, name) in [
            ("00000000000000000001.mf", "00000000000000000001.mf"),
            ("seg_1-a.seg", "seg_1-a.seg"),
            ("LEASE", "%4C%45%41%53%45"),
            ("a b", "a%20b"),
            ("100%", "100%25"),
            ("x@y", "x%40y"),
            ("É", "%C3%89"),
            ("e\u{301}", "e%CC%81"), // NFD stays distinct from NFC
            ("..x", "..x"),
            ("~", "%7E"),
        ] {
            assert_eq!(encode(segment), name, "{segment:?}");
            assert_eq!(decode(name).as_deref(), Some(segment), "{name:?}");
        }
        assert_eq!(dir_name("segments").unwrap(), "segments@");
        assert_eq!(dir_name("C").unwrap(), "%43@");
    }

    #[test]
    fn only_canonical_names_decode() {
        for name in [
            "",         // nothing
            "%61",      // escaped plain byte
            "%4c",      // lowercase hex
            "%4",       // truncated escape
            "%",        // bare escape
            "%GG",      // not hex
            "A",        // unescaped uppercase
            "a b",      // unescaped space
            "seg@",     // a directory name, not a file name
            "@tmp.1.2", // a temporary file
            "%2F",      // '/' inside a segment
            "%FF",      // not UTF-8
            "%C3",      // truncated UTF-8
        ] {
            assert_eq!(decode(name), None, "{name:?}");
        }
    }

    // Every one-byte escape, in both hex cases: exactly the uppercase escape of a non-plain ASCII byte other than
    // `/` decodes, and to that byte. A name from disk is hostile input; no escape may alias another name.
    #[test]
    fn every_single_escape_decodes_only_when_canonical() {
        for byte in 0..=u8::MAX {
            let upper = format!("%{byte:02X}");
            let lower = format!("%{byte:02x}");
            let want = (!is_plain(byte) && byte.is_ascii() && byte != b'/')
                .then(|| char::from(byte).to_string());
            assert_eq!(decode(&upper), want, "{upper}");
            if lower != upper {
                assert_eq!(decode(&lower), None, "{lower}");
            }
            // Nothing may follow `@` in an encoded name, and a trailing marker is never a file name.
            assert_eq!(decode(&format!("{upper}@")), None, "{upper}@");
        }
    }

    #[test]
    fn hostile_names_never_decode() {
        for name in [
            "a@b",          // unescaped marker inside
            "@",            // marker alone
            "@tmp",         // temporary prefix without numbers
            "%%41",         // escape of '%' not followed by hex
            "%2",           // truncated at the end
            "a%",           // bare escape at the end
            "%+1",          // sign is not hex
            "% 1",          // space is not hex
            "%\u{e9}1",     // non-ASCII after '%'
            "\u{e9}",       // raw non-ASCII
            "%C3%28",       // invalid UTF-8 sequence
            "%ED%A0%80",    // a UTF-16 surrogate encoded as UTF-8
            "%C0%80",       // overlong NUL
            "%F4%90%80%80", // above U+10FFFF
            "a/b",          // raw separator
        ] {
            assert_eq!(decode(name), None, "{name:?}");
        }
    }

    #[test]
    fn names_are_case_fold_and_normalization_safe() {
        // Distinct segments that a case-insensitive or normalizing filesystem would merge stay distinct, and every
        // name is ASCII.
        let segments = ["a", "A", "é", "e\u{301}", "É", "%41", "ß", "ss"];
        let folded: std::collections::HashSet<String> = segments
            .iter()
            .map(|s| {
                let name = encode(s);
                assert!(name.is_ascii(), "{name:?}");
                name.to_ascii_lowercase()
            })
            .collect();
        assert_eq!(folded.len(), segments.len());
    }

    #[test]
    fn names_over_255_bytes_are_rejected() {
        assert_eq!(file_name(&"x".repeat(255)).unwrap().len(), 255);
        assert_eq!(
            file_name(&"x".repeat(256)),
            Err(KeyDefect::NameTooLong { len: 256 })
        );
        assert_eq!(
            dir_name(&"x".repeat(255)),
            Err(KeyDefect::NameTooLong { len: 256 })
        );
        assert_eq!(dir_name(&"x".repeat(254)).unwrap().len(), 255);
        // Escapes count: 86 escaped bytes are 258 name bytes.
        assert_eq!(
            file_name(&"X".repeat(86)),
            Err(KeyDefect::NameTooLong { len: 258 })
        );
    }

    #[test]
    fn windows_reserved_names() {
        for name in [
            "con", "CON", "nul", "aux.txt", "prn.a.b", "com1", "lpt9.x", "x.", "a..",
        ] {
            assert!(is_windows_reserved(name), "{name:?}");
        }
        for name in [
            "con1", "console", "com0", "com10", "lpt", "x.y", "con@", ".x", "a%2E", "%43ON",
        ] {
            assert!(!is_windows_reserved(name), "{name:?}");
        }
        // Uppercase is escaped, so only a lowercase key segment reaches a reserved name.
        assert!(!is_windows_reserved(&encode("CON")));
        assert!(is_windows_reserved(&encode("con")));
    }
}

#[cfg(test)]
mod props {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn every_segment_round_trips(segment in "[^/]{1,40}") {
            let name = encode(&segment);
            prop_assert!(name.is_ascii() && !name.contains(DIR_MARKER), "{:?}", name);
            prop_assert_eq!(decode(&name), Some(segment));
        }

        // A directory name is never a file name, and without its marker it is the segment's file name.
        #[test]
        fn dir_names_round_trip_and_never_decode_as_files(segment in "[^/]{1,20}") {
            // 20 chars of at most 4 UTF-8 bytes, 3 name bytes each: within 255 (Windows also rejects reserved names).
            let (Ok(name), Ok(file)) = (dir_name(&segment), file_name(&segment)) else {
                prop_assume!(cfg!(windows));
                return Ok(());
            };
            prop_assert_eq!(decode(&name), None);
            prop_assert_eq!(name.strip_suffix(DIR_MARKER).and_then(decode), Some(segment));
            prop_assert_eq!(name.strip_suffix(DIR_MARKER), Some(file.as_str()));
        }

        // Two distinct segments never share a name, even after the case folding of NTFS and APFS.
        #[test]
        fn distinct_segments_never_fold_together(a in "[^/]{1,8}", b in "[^/]{1,8}") {
            prop_assume!(a != b);
            prop_assert_ne!(encode(&a).to_ascii_lowercase(), encode(&b).to_ascii_lowercase());
        }

        // Arbitrary bytes from a foreign tree, as the escapes that would spell them: decoding never panics and
        // accepts only canonical names of valid UTF-8 segments.
        #[test]
        fn escaped_arbitrary_bytes_decode_only_when_valid(bytes in prop::collection::vec(any::<u8>(), 0..12),
                                                          lower in any::<bool>()) {
            let name: String = bytes
                .iter()
                .map(|b| if lower { format!("%{b:02x}") } else { format!("%{b:02X}") })
                .collect();
            let valid = !bytes.is_empty()
                && !bytes.iter().any(|&b| is_plain(b) || b == b'/')
                && core::str::from_utf8(&bytes).is_ok()
                && (!lower || name == name.to_ascii_uppercase());
            prop_assert_eq!(decode(&name).is_some(), valid, "{:?}", name);
        }

        // A name read from disk is hostile input: decoding never panics, and what decodes is the one name of its
        // segment, so two files never list as one key.
        #[test]
        fn decoded_names_are_canonical(name in "([a-zA-Z0-9._%@/ -]|%[0-9A-Fa-f]{2}|\\PC){0,24}") {
            if let Some(segment) = decode(&name) {
                prop_assert_eq!(encode(&segment), name);
            }
        }
    }
}
