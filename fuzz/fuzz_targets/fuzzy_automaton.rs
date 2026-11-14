//! Fuzz the `fuzzy` automaton (ROADMAP 8.2.4.1; SPEC §7.1), which reads term bytes from a segment's term dictionary:
//! for any query and any term bytes, valid UTF-8 or not, it accepts the term exactly when the optimal string alignment
//! distance (over Unicode scalar values, every byte that is not part of valid UTF-8 one character) is within the
//! query's, `distance` reports that distance, and a walk pruned by `can_match` accepts the same terms as one that is not.
//! The first byte picks the distance; the query and the term are split at the first `0xFE` (no UTF-8 byte).
#![no_main]

use fst::Automaton;
use libfuzzer_sys::fuzz_target;
use persia_engine::search::Expansion;

/// Kept short: the oracle's table is quadratic.
const MAX: usize = 64;

fuzz_target!(|data: &[u8]| {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    let distance = 1 + first % 2;
    let (query, term) = match rest.iter().position(|&b| b == 0xFE) {
        Some(at) => (&rest[..at], &rest[at + 1..]),
        None => (rest, &[][..]),
    };
    let query: String = String::from_utf8_lossy(query).chars().take(MAX).collect();
    let term = &term[..term.len().min(MAX)];
    let a = Expansion::fuzzy(&query, distance);
    let q: Vec<u32> = query.chars().map(u32::from).collect();
    let d = osa(&q, &chars(term));
    let within = d <= usize::from(distance);
    assert_eq!(
        a.distance(term).map(usize::from),
        within.then_some(d),
        "{query:?} ~{distance} {term:?}"
    );

    let mut state = a.start();
    let mut pruned = false;
    for &b in term {
        pruned |= !a.can_match(&state);
        state = a.accept(&state, b);
    }
    assert_eq!(a.is_match(&state), within, "{query:?} ~{distance} {term:?}");
    // `can_match` false means no extension matches: a term it pruned on the way is not one.
    assert!(
        !(pruned && within),
        "pruned a match: {query:?} ~{distance} {term:?}"
    );
});

/// The textbook optimal string alignment table.
#[allow(clippy::needless_range_loop)] // The textbook form.
fn osa(a: &[u32], b: &[u32]) -> usize {
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0_usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=m {
        d[0][j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}

/// The characters of `bytes` as SPEC §7.1 counts them: valid UTF-8 by scalar value, every other byte one.
fn chars(bytes: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        match core::str::from_utf8(rest) {
            Ok(s) => {
                out.extend(s.chars().map(u32::from));
                break;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                out.extend(
                    core::str::from_utf8(valid)
                        .unwrap_or_default()
                        .chars()
                        .map(u32::from),
                );
                let bad = e.error_len().unwrap_or(after.len());
                out.extend(core::iter::repeat_n(u32::MAX, bad));
                rest = after.get(bad..).unwrap_or_default();
            }
        }
    }
    out
}
