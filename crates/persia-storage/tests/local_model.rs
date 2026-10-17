//! `LocalStorage` against `MemoryStorage` over keys that exercise the file-name codec (SPEC §10.1, §10.1.1,
//! ADR-0012): uppercase, non-ASCII, `%`, `@`, space and `.` characters, which the shared conformance model does not
//! generate. Each operation runs on one of two store instances on the same root, as two processes would.
//!
//! Invariants: every answer equals the in-memory reference; `list` is sorted by key bytes, which interleaves
//! objects and directories (`a%` < `a/b` < `a@`), and `list(p)` equals `list("")` filtered by `p`.

// Test-only code: unwraps and indexing are the point.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::string_slice)]

use bytes::Bytes;
use persia_storage::{Key, KeyPrefix, LocalStorage, MemoryStorage, PutOutcome, Storage};
use proptest::prelude::*;

const CASES: u32 = if cfg!(feature = "slow") { 1024 } else { 64 };

/// Characters around every boundary of the codec and of byte order: `%` (0x25) and `.` (0x2E) sort before `/`
/// (0x2F), `@` and the letters after it; `A`/`a` and `é`/`e\u{301}` collide on case-folding and normalizing
/// filesystems.
fn segment() -> impl Strategy<Value = String> {
    let ch = prop_oneof![
        Just('a'),
        Just('A'),
        Just('b'),
        Just('%'),
        Just('@'),
        Just('.'),
        Just(' '),
        Just('é'),
        Just('e'),
        Just('\u{301}'),
        Just('~'),
        Just('-'),
    ];
    prop::collection::vec(ch, 1..4)
        .prop_map(|cs| cs.into_iter().collect::<String>())
        // Windows rejects a file name ending in `.` (SPEC §10.1.1); `local.rs` tests that error.
        .prop_filter("a name ending in . on Windows", |s| {
            !(cfg!(windows) && s.ends_with('.'))
        })
}

fn key_text() -> impl Strategy<Value = String> {
    prop::collection::vec(segment(), 1..4).prop_map(|segs| segs.join("/"))
}

fn model_key() -> impl Strategy<Value = Key> {
    // Dot segments (`.`, `..`) are not keys; everything else here is.
    key_text().prop_filter_map("dot segment", |s| Key::new(s).ok())
}

/// A byte-level prefix of some key, cut on a character boundary: prefixes ending mid-segment, at `/`, or empty.
fn model_prefix() -> impl Strategy<Value = KeyPrefix> {
    (key_text(), any::<prop::sample::Index>()).prop_filter_map("invalid prefix", |(text, cut)| {
        let bounds: Vec<usize> = (0..=text.len())
            .filter(|&i| text.is_char_boundary(i))
            .collect();
        let end = bounds[cut.index(bounds.len())];
        KeyPrefix::new(&text[..end]).ok()
    })
}

#[derive(Debug, Clone)]
enum Op {
    Put(Key, Bytes),
    PutIfAbsent(Key, Bytes),
    Delete(Key),
    Get(Key),
    List(KeyPrefix),
}

fn op() -> impl Strategy<Value = (bool, Op)> {
    // Few distinct contents, so `put_if_absent` often finds equal bytes.
    let data = prop_oneof![
        Just(Bytes::new()),
        Just(Bytes::from_static(b"x")),
        Just(Bytes::from_static(b"yy")),
        prop::collection::vec(any::<u8>(), 0..16).prop_map(Bytes::from),
    ];
    let op = prop_oneof![
        3 => (model_key(), data.clone()).prop_map(|(k, d)| Op::Put(k, d)),
        2 => (model_key(), data).prop_map(|(k, d)| Op::PutIfAbsent(k, d)),
        1 => model_key().prop_map(Op::Delete),
        1 => model_key().prop_map(Op::Get),
        2 => model_prefix().prop_map(Op::List),
    ];
    (any::<bool>(), op)
}

fn listed(s: &dyn Storage, p: &KeyPrefix) -> Vec<(String, u64)> {
    s.list(p)
        .unwrap()
        .into_iter()
        .map(|m| (m.key.as_str().to_owned(), m.size))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn local_matches_memory_over_codec_heavy_keys(ops in prop::collection::vec(op(), 1..40)) {
        let dir = tempfile::tempdir().unwrap();
        let stores = [LocalStorage::open(dir.path()).unwrap(), LocalStorage::open(dir.path()).unwrap()];
        let model = MemoryStorage::new();
        for (second, op) in &ops {
            let s = &stores[usize::from(*second)];
            match op {
                Op::Put(k, d) => {
                    s.put(k, d.clone()).unwrap();
                    model.put(k, d.clone()).unwrap();
                }
                Op::PutIfAbsent(k, d) => {
                    let got: PutOutcome = s.put_if_absent(k, d.clone()).unwrap();
                    prop_assert_eq!(got, model.put_if_absent(k, d.clone()).unwrap(), "{:?}", op);
                }
                Op::Delete(k) => {
                    s.delete(k).unwrap();
                    model.delete(k).unwrap();
                }
                Op::Get(k) => {
                    let got = s.get(k).map_err(|e| e.kind());
                    prop_assert_eq!(got, model.get(k).map_err(|e| e.kind()), "{:?}", op);
                }
                Op::List(p) => {
                    let got = listed(s, p);
                    prop_assert_eq!(&got, &listed(&model, p), "{:?}", op);
                    let mut sorted = got.clone();
                    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                    prop_assert_eq!(&got, &sorted, "not sorted by key bytes");
                    let all: Vec<(String, u64)> = listed(s, &KeyPrefix::new("").unwrap())
                        .into_iter()
                        .filter(|(k, _)| k.starts_with(p.as_str()))
                        .collect();
                    prop_assert_eq!(got, all, "list({:?}) disagrees with the full listing", p);
                }
            }
        }
        // The end state, through either instance, and nothing but object files and directories on disk.
        let everything = KeyPrefix::new("").unwrap();
        for s in &stores {
            prop_assert_eq!(listed(s, &everything), listed(&model, &everything));
        }
        for meta in model.list(&everything).unwrap() {
            prop_assert_eq!(stores[1].get(&meta.key).unwrap(), model.get(&meta.key).unwrap());
        }
    }
}
