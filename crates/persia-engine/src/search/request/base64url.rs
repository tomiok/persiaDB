//! Base64url without padding (RFC 4648 §5), for the cursor token (SPEC §7.4, ADR-0062): text that needs no escaping
//! in JSON, a URL query or a header. Decoding is strict, so every byte string has exactly one text: an input with a
//! character outside the alphabet, padding, a length no encoding has, or unused bits set is refused.

/// The text of `bytes`.
pub(super) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let mut group = [0_u8; 4];
        for (to, from) in group.iter_mut().skip(1).zip(chunk) {
            *to = *from;
        }
        // 24 bits, the chunk's bytes first: a chunk of `n` bytes fills the first `n + 1` characters.
        let bits = u32::from_be_bytes(group);
        let shifts = [18_u32, 12, 6, 0];
        for shift in shifts.into_iter().take(chunk.len().saturating_add(1)) {
            out.push(character((bits >> shift) & 63));
        }
    }
    out
}

/// The bytes `text` encodes. `None` unless `text` is what [`encode`] gives for them.
pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    let mut out = Vec::with_capacity((text.len() / 4).saturating_mul(3).saturating_add(2));
    for chunk in text.chunks(4) {
        let mut bits = 0_u32;
        for &c in chunk {
            bits = (bits << 6) | value(c)?;
        }
        // Left-aligned in 24 bits, as `encode` read them.
        let missing = u32::try_from(4_usize.saturating_sub(chunk.len())).ok()?;
        let [_, a, b, c] = (bits << (6 * missing)).to_be_bytes();
        match chunk.len() {
            4 => out.extend([a, b, c]),
            // The bits past the last whole byte are zero in the one encoding `encode` gives.
            3 if c == 0 => out.extend([a, b]),
            2 if b == 0 => out.push(a),
            // One character holds 6 bits, less than a byte: no encoding has that length.
            _ => return None,
        }
    }
    Some(out)
}

/// The character of a 6-bit value.
fn character(sextet: u32) -> char {
    let offset = |base: u8, from: u32| {
        u8::try_from(sextet.saturating_sub(from))
            .ok()
            .and_then(|n| base.checked_add(n))
            .map_or('_', char::from)
    };
    match sextet {
        0..=25 => offset(b'A', 0),
        26..=51 => offset(b'a', 26),
        52..=61 => offset(b'0', 52),
        62 => '-',
        _ => '_',
    }
}

/// The 6-bit value of a character; `None` outside the alphabet (`=` padding included).
fn value(c: u8) -> Option<u32> {
    let sextet = match c {
        b'A'..=b'Z' => c.checked_sub(b'A')?,
        b'a'..=b'z' => c.checked_sub(b'a')?.checked_add(26)?,
        b'0'..=b'9' => c.checked_sub(b'0')?.checked_add(52)?,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    };
    Some(u32::from(sextet))
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn rfc_4648_vectors_without_padding() {
        // RFC 4648 §10, less the `=` padding.
        for (bytes, text) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(bytes.as_bytes()), text);
            assert_eq!(decode(text).as_deref(), Some(bytes.as_bytes()));
        }
    }

    #[test]
    fn the_url_safe_alphabet() {
        // 0xfb 0xff: sextets 62, 63, 60 — `-` and `_`, where standard base64 has `+` and `/`.
        assert_eq!(encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(decode("-_8"), Some(vec![0xfb, 0xff]));
        assert_eq!(decode("+/8"), None);
        // Every sextet, in order.
        let all: Vec<u8> = (0..=u8::MAX).collect();
        let text = encode(&all);
        assert!(
            text.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_eq!(decode(&text), Some(all));
        let alphabet = encode(&[
            0x00, 0x10, 0x83, 0x10, 0x51, 0x87, 0x20, 0x92, 0x8b, 0x30, 0xd3, 0x8f, 0x41, 0x14,
            0x93, 0x51, 0x55, 0x97, 0x61, 0x96, 0x9b, 0x71, 0xd7, 0x9f, 0x82, 0x18, 0xa3, 0x92,
            0x59, 0xa7, 0xa2, 0x9a, 0xab, 0xb2, 0xdb, 0xaf, 0xc3, 0x1c, 0xb3, 0xd3, 0x5d, 0xb7,
            0xe3, 0x9e, 0xbb, 0xf3, 0xdf, 0xbf,
        ]);
        assert_eq!(
            alphabet,
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        );
    }

    #[test]
    fn only_the_canonical_text_decodes() {
        // Padding, whitespace and characters outside the alphabet.
        for text in ["Zg==", "Zm8=", "Zm 9v", "Zm9v\n", "Zm9v.", "Zmé"] {
            assert_eq!(decode(text), None, "{text:?}");
        }
        // A length no encoding has: one character past whole groups.
        for text in ["Z", "Zm9vY"] {
            assert_eq!(decode(text), None, "{text:?}");
        }
        // Unused bits set: `Zh` and `Zm9` are not what any bytes encode to (`Zg`, `Zm8`).
        for text in ["Zh", "Zm9", "Zm9vYh", "Zm9vYmF"] {
            assert_eq!(decode(text), None, "{text:?}");
        }
    }

    #[test]
    fn every_length_round_trips() {
        let bytes: Vec<u8> = (0..=u8::MAX).rev().collect();
        for len in 0..bytes.len() {
            let text = encode(&bytes[..len]);
            assert_eq!(text.len(), (len * 4).div_ceil(3), "{len}");
            assert_eq!(decode(&text).as_deref(), Some(&bytes[..len]), "{len}");
        }
    }
}
