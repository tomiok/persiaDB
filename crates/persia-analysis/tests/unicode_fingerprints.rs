//! Which built-in analyzers a Unicode version bump changes (ROADMAP 4.4.6; SPEC §6.3.1, ADR-0025).
//!
//! Each current built-in analyzer version has a fingerprint of its output over every Unicode scalar value, alone and
//! next to letters and digits. Moving `UNICODE_VERSION` (a `unicode-segmentation`, `unicode-normalization` or
//! toolchain update) fails here for the analyzers whose output changes in the probe contexts, listed with their new fingerprints:
//! each gets a new version with the same chain, and its old version is superseded (ADR-0025).

use persia_analysis::{BUILTIN_ANALYZERS, current_builtin_version};

/// `(name, version, fingerprint)` of every current built-in analyzer version, under `UNICODE_VERSION`.
const FINGERPRINTS: &[(&str, u32, u64)] = &[
    ("en", 1, 0xdadf_8d19_5704_91a5),
    ("es", 1, 0xe8c3_0685_3dd8_b14a),
    ("keyword", 1, 0xa39e_5096_d574_d0b0),
    ("pt", 1, 0x6c2f_a889_1270_1099),
    ("standard", 1, 0x058e_1e10_2d60_e902),
    ("whitespace", 1, 0x8744_e2dd_23e1_ea3a),
];

#[test]
fn fingerprints_list_every_current_builtin() {
    let current: Vec<(&str, u32)> = BUILTIN_ANALYZERS
        .iter()
        .filter(|a| current_builtin_version(a.name) == Some(a.version))
        .map(|a| (a.name, a.version))
        .collect();
    let recorded: Vec<(&str, u32)> = FINGERPRINTS.iter().map(|&(n, v, _)| (n, v)).collect();
    assert_eq!(
        recorded, current,
        "FINGERPRINTS must list every current built-in analyzer version, in BUILTIN_ANALYZERS order"
    );
}

/// Seconds in release, minutes in debug: nightly runs it, and so must a Unicode version bump, with
/// `cargo test -p persia-analysis --release --features slow --test unicode_fingerprints`.
#[cfg(feature = "slow")]
mod slow {
    #![allow(clippy::unwrap_used)] // fixed inputs

    use persia_analysis::{Token, UNICODE_VERSION, builtin_analyzer};

    use super::FINGERPRINTS;

    /// Code points per analyzed chunk: large enough to amortize the calls, small enough to keep offsets small.
    const CHUNK: usize = 4096;

    /// FNV-1a, 64 bits: stable and dependency-free.
    struct Fnv(u64);

    impl Fnv {
        fn new() -> Self {
            Self(0xcbf2_9ce4_8422_2325)
        }

        fn write(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.0 ^= u64::from(b);
                self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
            }
        }

        fn token(&mut self, token: &Token) {
            self.write(token.text.as_bytes());
            self.write(&[0xff]);
            for n in [token.start, token.end] {
                self.write(&u64::try_from(n).unwrap().to_le_bytes());
            }
            self.write(&token.position.to_le_bytes());
        }
    }

    /// Where `c` goes in each probe (`#`), chosen so the UAX #29 classes it could move between analyze differently
    /// in at least one: alone; between letters and digits (`ALetter`, `Numeric`, `Extend`, `ExtendNumLet` join);
    /// after `a.` and `1.` (`MidLetter`/`MidNum` rules `WB6`/`WB7` vs `WB11`/`WB12`: `a.x` is one word, `a.5`
    /// two); before a quote (`Hebrew_Letter`, `WB7a`); doubled and around a period (self-joining classes such as
    /// `Katakana`, `WB13`; `MidNumLet`); after `é` (case, folding, composition).
    const PROBES: &[&str] = &[
        "#", "a#b", "1#2", "a.#", "1.#", "#'", "#.#", "##", "é#", "#\u{301}",
    ];

    /// The probes of `c`, one per line: a line break always ends a word (UAX #29 rules `WB3a` and `WB3b`), so
    /// probes never join.
    fn push_probes(c: char, text: &mut String) {
        for probe in PROBES {
            for p in probe.chars() {
                if p == '#' {
                    text.push(c);
                } else {
                    text.push(p);
                }
            }
            text.push('\n');
        }
    }

    fn fingerprint(name: &str, version: u32) -> u64 {
        let analyzer = builtin_analyzer(name, version).unwrap();
        let mut hash = Fnv::new();
        let mut text = String::new();
        let mut tokens = Vec::new();
        let scalars: Vec<char> = (0..=u32::from(char::MAX))
            .filter_map(char::from_u32)
            .collect();
        for chunk in scalars.chunks(CHUNK) {
            text.clear();
            for &c in chunk {
                push_probes(c, &mut text);
            }
            analyzer.analyze(&text, &mut tokens);
            for token in &tokens {
                hash.token(token);
            }
            hash.write(&[0xfe]);
        }
        hash.0
    }

    #[test]
    fn builtin_outputs_match_their_fingerprints() {
        // One thread per analyzer: each walks every scalar value.
        let now: Vec<u64> = std::thread::scope(|s| {
            let handles: Vec<_> = FINGERPRINTS
                .iter()
                .map(|&(name, version, _)| s.spawn(move || fingerprint(name, version)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let changed: Vec<String> = FINGERPRINTS
            .iter()
            .zip(now)
            .filter(|&(&(_, _, recorded), now)| now != recorded)
            .map(|(&(name, version, recorded), now)| {
                format!("{name}@{version}: recorded {recorded:#018x}, now {now:#018x}")
            })
            .collect();
        assert!(
            changed.is_empty(),
            "under Unicode {UNICODE_VERSION:?} these built-in analyzers analyze differently (SPEC §6.3.1, ADR-0025): \
             give each a new version with the same chain and record its new fingerprint\n{}",
            changed.join("\n")
        );
    }
}
