//! Property tests for `Key` and `KeyPrefix` (ROADMAP 2.1.1): validation agrees with a direct reading of the rules
//! in SPEC §10.1, and every prefix of a valid key is a valid `KeyPrefix` that matches it.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::string_slice)]

use persia_storage::{Key, KeyPrefix};
use proptest::prelude::*;

/// Characters that hit every rule: separators, dots, forbidden characters, and a multi-byte one.
fn text(max_len: usize) -> impl Strategy<Value = String> {
    let ch = prop_oneof![
        4 => Just('a'),
        3 => Just('/'),
        3 => Just('.'),
        1 => Just('\\'),
        1 => Just('\0'),
        1 => Just('\x7f'),
        1 => Just('é'),
        1 => Just(':'),
    ];
    prop::collection::vec(ch, 0..max_len).prop_map(|cs| cs.into_iter().collect())
}

fn chars_ok(s: &str) -> bool {
    s.len() <= Key::MAX_LEN
        && !s
            .chars()
            .any(|c| matches!(c, '\0'..='\x1f' | '\x7f' | '\\' | ':'))
}

fn segment_ok(seg: &str) -> bool {
    !matches!(seg, "" | "." | "..")
}

fn reference_key(s: &str) -> bool {
    !s.is_empty() && chars_ok(s) && s.split('/').all(segment_ok)
}

fn reference_prefix(s: &str) -> bool {
    let segments: Vec<&str> = s.split('/').collect();
    chars_ok(s)
        && segments[..segments.len() - 1]
            .iter()
            .all(|seg| segment_ok(seg))
}

proptest! {
    #[test]
    fn key_validation_matches_the_rules(s in text(12)) {
        prop_assert_eq!(Key::new(s.as_str()).is_ok(), reference_key(&s), "{:?}", s);
        prop_assert_eq!(KeyPrefix::new(s.as_str()).is_ok(), reference_prefix(&s), "{:?}", s);
    }

    #[test]
    fn long_keys_hit_the_byte_limit(len in 1000usize..1100, multibyte in any::<bool>()) {
        let s: String = if multibyte { "é".repeat(len / 2) } else { "a".repeat(len) };
        prop_assert_eq!(Key::new(s.as_str()).is_ok(), s.len() <= Key::MAX_LEN);
    }

    #[test]
    fn prefixes_of_a_valid_key_are_valid_and_match(s in text(16)) {
        let Ok(key) = Key::new(s.as_str()) else { return Ok(()) };
        for (i, _) in s.char_indices().chain([(s.len(), ' ')]) {
            let prefix = KeyPrefix::new(&s[..i]).unwrap();
            prop_assert!(prefix.matches(&key));
        }
        prop_assert_eq!(key.as_str(), s.as_str());
    }
}
