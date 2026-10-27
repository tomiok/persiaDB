//! Structural check of an FST payload (`TermDict`, `IdMap`; SPEC §5.3.1, §5.3.6) before the `fst` crate touches it.
//!
//! `fst` 0.4.7 trusts its input: `fst::Map::new` checks the header and little else (it never checks the root
//! address unless it is 0, nor the checksum), and its node parser (`raw/node.rs`) indexes, slices, subtracts and
//! unpacks without bounds checks. A payload with a valid section CRC but crafted bytes makes it panic, and a
//! transition pointing back up the graph makes a stream run forever. So [`check`] parses the payload itself, with
//! checked arithmetic and `get`, the way `fst` will, and rejects anything `fst` could not read safely.
//!
//! Why `fst` cannot panic afterwards (SPEC §5.3.1): every `fst` operation the reader uses (`Map::get`, `Map::len`,
//! `Map::stream`, `Map::search` with an automaton and range bounds, as `TermIndex` and `Segment::docid_of` do) starts
//! at the root and only ever moves to `Transition::addr` of a node it already parsed, or to `EMPTY_ADDRESS`, which
//! `fst` turns into an empty final node without reading bytes. Those addresses are computed from the same bytes with
//! the same formulas as here, so the set of nodes `fst` can reach is exactly the set [`check`] visited. For each of
//! them [`check`] verified that every byte any `Node` method reads (`Node::new`: state, sizes, transition count,
//! final output, `end_addr`; then `find_input` with or without the 256-byte index, `transition(i)`,
//! `transition_addr(i)`, `input`, `output` for `i < len()`) lies in the node's span `[end, addr]`, inside the
//! payload; that every packed size `unpack_uint` asserts on is in `1..=8`; that no subtraction `fst` performs
//! (`end - delta`, `end - 1`, the backward offsets from `addr`) underflows; and that no sum of outputs along any path
//! (`Output::cat`, done by `get` and by every stream step) overflows a `u64`. Transitions point strictly below the
//! node's first byte, so the graph is acyclic and every stream ends; the stream's `inp.pop().unwrap()` holds because
//! no node below the root shares its address. Out-of-range index values (`>= len()`) mean "no transition" in `fst`
//! too, and `Transitions` only calls `transition(i)` for `i < len()`.

use std::cmp::max;

/// The only FST version the writer produces (`fst` 0.4's `VERSION`).
const VERSION: u64 = 3;
/// Version and type, two `u64`s; the first node starts right after.
const HEADER_LEN: usize = 16;
/// Key count (`u64`), root address (`u64`), checksum (`u32`).
const FOOTER_LEN: usize = 20;
/// The address of the empty final node, which takes no bytes.
const EMPTY_ADDRESS: usize = 0;
/// Nodes with more transitions than this carry a 256-byte input index.
const INDEX_THRESHOLD: usize = 32;
const INDEX_LEN: usize = 256;
/// Packed integers (`fst::bytes::unpack_uint`) take 1 to 8 bytes.
const MAX_PACK: usize = 8;

/// What a valid FST holds, from [`check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FstShape {
    /// The number of keys, which is also the footer's key count (`fst::Map::len`).
    pub(super) keys: u64,
    /// The length of the longest path from the root: no key is longer, and no stream goes deeper.
    pub(super) longest_path: u64,
}

/// Why [`check`] rejected an FST: the rule broken, and the byte offset in the payload where it was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FstDamage {
    pub(super) what: &'static str,
    pub(super) at: usize,
}

type Result<T> = core::result::Result<T, FstDamage>;

fn damage<T>(what: &'static str, at: usize) -> Result<T> {
    Err(FstDamage { what, at })
}

/// Checks that `data`, an FST payload, is one `fst` 0.4.7 can open and read without panicking or looping, and that
/// its footer's key count is right. Linear in the payload size (each reachable node is parsed twice).
///
/// # Errors
/// An [`FstDamage`] naming the first broken rule.
pub(super) fn check(data: &[u8]) -> Result<FstShape> {
    let len = data.len();
    let Some(footer) = len.checked_sub(FOOTER_LEN).filter(|&f| f >= HEADER_LEN) else {
        return damage("an FST shorter than its header and footer", 0);
    };
    if uint(data, 0, 8)? != VERSION {
        return damage("an FST version other than 3", 0);
    }
    let keys = uint(data, footer, 8)?;
    let root_at = footer.saturating_add(8);
    let root = usize::try_from(uint(data, root_at, 8)?).unwrap_or(usize::MAX);
    // `fst` converts both with `u64_to_usize`, which panics on a 32-bit target.
    if usize::try_from(keys).is_err() {
        return damage("an FST key count that does not fit a usize", footer);
    }
    if root == EMPTY_ADDRESS {
        // Only the map holding just the empty key with output 0 has no node bytes.
        return if footer == HEADER_LEN && keys == 1 {
            Ok(FstShape {
                keys,
                longest_path: 0,
            })
        } else {
            damage(
                "an FST whose root is the empty node but which is not empty",
                root_at,
            )
        };
    }
    // The root is the last node written: its state byte is the one right before the footer.
    if footer.checked_sub(1) != Some(root) {
        return damage("an FST root address other than the last node byte", root_at);
    }

    let reachable = reachable(data, root)?;
    let mut sums: Vec<Sums> = Vec::with_capacity(reachable.len());
    // Ascending addresses: every transition points lower, so its target's sums are already known.
    for &addr in &reachable {
        let node = Node::parse(data, addr)?;
        let mut s = Sums {
            keys: u64::from(node.is_final),
            max_output: node.final_output,
            longest_path: 0,
        };
        for i in 0..node.ntrans {
            let t = node.transition(i)?;
            let child = if t.target == EMPTY_ADDRESS {
                Sums::EMPTY_FINAL
            } else {
                reachable
                    .binary_search(&t.target)
                    .ok()
                    .and_then(|j| sums.get(j))
                    .copied()
                    .ok_or(FstDamage {
                        what: "an FST transition to an unvisited node",
                        at: addr,
                    })?
            };
            let overflow = |what| FstDamage { what, at: addr };
            s.keys = s
                .keys
                .checked_add(child.keys)
                .ok_or(overflow("an FST with more than 2^64 keys"))?;
            let output = t
                .output
                .checked_add(child.max_output)
                .ok_or(overflow("an FST output sum that overflows"))?;
            s.max_output = max(s.max_output, output);
            let path = child
                .longest_path
                .checked_add(1)
                .ok_or(overflow("an FST path that overflows"))?;
            s.longest_path = max(s.longest_path, path);
        }
        sums.push(s);
    }
    // The root has the highest address, so it is last.
    let Some(total) = sums.last() else {
        return damage("an FST without a root node", root);
    };
    if total.keys != keys {
        return damage("an FST key count other than the keys it holds", footer);
    }
    Ok(FstShape {
        keys,
        longest_path: total.longest_path,
    })
}

/// Per node: the keys below it (itself included if final), the largest output sum of any path from it (a final
/// output included), and its longest path.
#[derive(Debug, Clone, Copy)]
struct Sums {
    keys: u64,
    max_output: u64,
    longest_path: u64,
}

impl Sums {
    const EMPTY_FINAL: Self = Self {
        keys: 1,
        max_output: 0,
        longest_path: 0,
    };
}

/// Every node reachable from `root` (not the empty node), each parsed and checked once, by ascending address.
fn reachable(data: &[u8], root: usize) -> Result<Vec<usize>> {
    let mut seen = vec![false; data.len()];
    let mut todo = vec![root];
    if let Some(s) = seen.get_mut(root) {
        *s = true;
    }
    while let Some(addr) = todo.pop() {
        let node = Node::parse(data, addr)?;
        for i in 0..node.ntrans {
            let target = node.transition(i)?.target;
            if target == EMPTY_ADDRESS {
                continue;
            }
            match seen.get_mut(target) {
                Some(s) if !*s => {
                    *s = true;
                    todo.push(target);
                }
                Some(_) => {}
                None => return damage("an FST transition past its end", addr),
            }
        }
    }
    Ok(seen
        .iter()
        .enumerate()
        .filter_map(|(addr, &s)| s.then_some(addr))
        .collect())
}

/// A node's encoding (`fst::raw::node::State`), by the two high bits of its state byte.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// One transition, output 0, to the node whose last byte is right below this one.
    OneTransNext,
    /// One transition: output (`osize` bytes) at `end`, address delta (`tsize` bytes) after it.
    OneTrans,
    /// Any number of transitions. Going down from the state byte: transition count (if not in the state byte),
    /// sizes, the index (if any), inputs, address deltas, outputs, final output (if final); `inputs_end` and
    /// `deltas_end` are one past the inputs and the deltas.
    AnyTrans {
        inputs_end: usize,
        deltas_end: usize,
    },
}

/// A node parsed with every offset checked. It spans `[end, addr]`, `end >= HEADER_LEN`.
#[derive(Debug)]
struct Node<'a> {
    data: &'a [u8],
    end: usize,
    kind: Kind,
    ntrans: usize,
    tsize: usize,
    osize: usize,
    is_final: bool,
    final_output: u64,
}

/// One transition: its output and target address (`EMPTY_ADDRESS` for the empty final node).
#[derive(Debug, Clone, Copy)]
struct Trans {
    output: u64,
    target: usize,
}

impl<'a> Node<'a> {
    /// Parses the node whose state byte is at `addr`, as `fst::raw::node::Node::new` does.
    fn parse(data: &'a [u8], addr: usize) -> Result<Self> {
        let Some(&state) = data.get(addr) else {
            return damage("an FST node past its end", addr);
        };
        let below = |n: usize| sub(addr, n, addr);
        // Single-transition nodes keep their input in the state byte (a common input) or in the byte below.
        let input_len = usize::from(state.trailing_zeros() >= 6);
        let mut node = Self {
            data,
            end: 0,
            kind: Kind::OneTransNext,
            ntrans: 1,
            tsize: 0,
            osize: 0,
            is_final: false,
            final_output: 0,
        };
        match state >> 6 {
            0b11 => node.end = below(input_len)?,
            0b10 => {
                let sizes_at = below(add(input_len, 1, addr)?)?;
                (node.tsize, node.osize) = sizes(data, sizes_at)?;
                if node.tsize == 0 {
                    return damage("an FST transition without address bytes", addr);
                }
                node.kind = Kind::OneTrans;
                node.end = sub(sizes_at, add(node.tsize, node.osize, addr)?, addr)?;
            }
            _ => node.parse_any(state, addr)?,
        }
        if node.end < HEADER_LEN {
            return damage("an FST node overlapping the header", addr);
        }
        Ok(node)
    }

    fn parse_any(&mut self, state: u8, addr: usize) -> Result<()> {
        let data = self.data;
        self.is_final = state & 0b0100_0000 != 0;
        let (ntrans, count_len) = match state & 0b0011_1111 {
            0 => {
                let Some(&n) = data.get(sub(addr, 1, addr)?) else {
                    return damage("an FST node past its end", addr);
                };
                // 1 never needs this byte (it fits the state byte), so it stands for 256.
                (if n == 1 { 256 } else { usize::from(n) }, 1)
            }
            n => (usize::from(n), 0),
        };
        self.ntrans = ntrans;
        let sizes_at = sub(addr, add(count_len, 1, addr)?, addr)?;
        (self.tsize, self.osize) = sizes(data, sizes_at)?;
        if ntrans > 0 && self.tsize == 0 {
            return damage("an FST transition without address bytes", addr);
        }
        let index_len = if ntrans > INDEX_THRESHOLD {
            INDEX_LEN
        } else {
            0
        };
        let inputs_end = sub(sizes_at, index_len, addr)?;
        let deltas_end = sub(inputs_end, ntrans, addr)?;
        let outputs_end = sub(deltas_end, mul(ntrans, self.tsize, addr)?, addr)?;
        let final_len = if self.is_final { self.osize } else { 0 };
        let outputs_len = add(mul(ntrans, self.osize, addr)?, final_len, addr)?;
        self.end = sub(outputs_end, outputs_len, addr)?;
        self.kind = Kind::AnyTrans {
            inputs_end,
            deltas_end,
        };
        if self.is_final && self.osize > 0 {
            self.final_output = uint(data, self.end, self.osize)?;
        }
        // Not needed by `fst` to stay in bounds, but a stream relies on it for byte order, and `get` (through the
        // index) and a stream must agree on the transitions.
        let mut previous = None;
        for i in 0..ntrans {
            let input = self.input(i)?;
            if previous.is_some_and(|p| p >= input) {
                return damage("FST transitions out of byte order", addr);
            }
            previous = Some(input);
        }
        if index_len > 0 {
            self.check_index(inputs_end, addr)?;
        }
        Ok(())
    }

    /// The index maps each input byte to its transition, any value `>= ntrans` meaning none.
    fn check_index(&self, inputs_end: usize, addr: usize) -> Result<()> {
        let Some(index) = self.data.get(inputs_end..add(inputs_end, INDEX_LEN, addr)?) else {
            return damage("an FST node past its end", addr);
        };
        for (b, &i) in (0..=u8::MAX).zip(index) {
            let i = usize::from(i);
            let consistent = if i < self.ntrans {
                self.input(i)? == b
            } else {
                // No transition may have this input.
                (0..self.ntrans).try_fold(true, |ok, j| Ok(ok && self.input(j)? != b))?
            };
            if !consistent {
                return damage("an FST index that disagrees with the inputs", addr);
            }
        }
        Ok(())
    }

    /// Input byte of transition `i` of an `AnyTrans` node.
    fn input(&self, i: usize) -> Result<u8> {
        let Kind::AnyTrans { inputs_end, .. } = self.kind else {
            return damage("an FST input read from a single-transition node", self.end);
        };
        let at = sub(inputs_end, add(i, 1, self.end)?, self.end)?;
        match self.data.get(at) {
            Some(&b) => Ok(b),
            None => damage("an FST node past its end", at),
        }
    }

    /// Transition `i < ntrans`, as `fst`'s `transition(i)` computes it, with its target checked.
    fn transition(&self, i: usize) -> Result<Trans> {
        let at = self.end;
        let (output, delta) = match self.kind {
            Kind::OneTransNext => {
                // `fst`: `end - 1`, the last byte of the node written just before.
                let target = sub(self.end, 1, at)?;
                if target < HEADER_LEN {
                    return damage("an FST transition into the header", at);
                }
                return Ok(Trans { output: 0, target });
            }
            Kind::OneTrans => {
                let output = self.output_at(self.end)?;
                let delta = uint(self.data, add(self.end, self.osize, at)?, self.tsize)?;
                (output, delta)
            }
            Kind::AnyTrans { deltas_end, .. } => {
                let next = add(i, 1, at)?;
                let delta_at = sub(deltas_end, mul(next, self.tsize, at)?, at)?;
                let outputs_end = sub(deltas_end, mul(self.ntrans, self.tsize, at)?, at)?;
                let output_at = sub(outputs_end, mul(next, self.osize, at)?, at)?;
                let delta = uint(self.data, delta_at, self.tsize)?;
                (self.output_at(output_at)?, delta)
            }
        };
        // `fst::raw::node::unpack_delta`: 0 is the empty node, anything else is `end - delta`.
        let Ok(delta) = usize::try_from(delta) else {
            return damage("an FST transition address that does not fit a usize", at);
        };
        let target = if delta == 0 {
            EMPTY_ADDRESS
        } else {
            sub(self.end, delta, at)?
        };
        // `target < end` holds (delta >= 1): the graph only points down, so it has no cycle.
        if target != EMPTY_ADDRESS && target < HEADER_LEN {
            return damage("an FST transition into the header", at);
        }
        Ok(Trans { output, target })
    }

    fn output_at(&self, at: usize) -> Result<u64> {
        if self.osize == 0 {
            Ok(0)
        } else {
            uint(self.data, at, self.osize)
        }
    }
}

/// The transition address and output sizes of the sizes byte at `at`, each at most 8.
fn sizes(data: &[u8], at: usize) -> Result<(usize, usize)> {
    let Some(&b) = data.get(at) else {
        return damage("an FST node past its end", at);
    };
    let (tsize, osize) = (usize::from(b >> 4), usize::from(b & 0x0f));
    if tsize > MAX_PACK || osize > MAX_PACK {
        return damage("an FST packed size above 8 bytes", at);
    }
    Ok((tsize, osize))
}

/// A little-endian unsigned integer of `n` (1 to 8) bytes at `at` (`fst::bytes::unpack_uint`).
fn uint(data: &[u8], at: usize, n: usize) -> Result<u64> {
    if !(1..=MAX_PACK).contains(&n) {
        return damage("an FST packed size outside 1..=8", at);
    }
    let Some(bytes) = add(at, n, at).ok().and_then(|e| data.get(at..e)) else {
        return damage("an FST node past its end", at);
    };
    let mut buf = [0_u8; 8];
    for (d, s) in buf.iter_mut().zip(bytes) {
        *d = *s;
    }
    Ok(u64::from_le_bytes(buf))
}

fn sub(a: usize, b: usize, at: usize) -> Result<usize> {
    a.checked_sub(b)
        .map_or_else(|| damage("an FST node past its start", at), Ok)
}

fn add(a: usize, b: usize, at: usize) -> Result<usize> {
    a.checked_add(b)
        .map_or_else(|| damage("an FST offset that overflows", at), Ok)
}

fn mul(a: usize, b: usize, at: usize) -> Result<usize> {
    a.checked_mul(b)
        .map_or_else(|| damage("an FST offset that overflows", at), Ok)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use fst::{Automaton, IntoStreamer, MapBuilder, Streamer};

    use super::*;

    fn build<K: AsRef<[u8]>>(entries: impl IntoIterator<Item = (K, u64)>) -> Vec<u8> {
        let mut b = MapBuilder::memory();
        for (k, v) in entries {
            b.insert(k, v).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn ranks<K: AsRef<[u8]> + Ord + Clone>(keys: &[K]) -> Vec<u8> {
        let mut keys = keys.to_vec();
        keys.sort();
        keys.dedup();
        build(keys.into_iter().zip(0..))
    }

    /// Everything the reader does with an FST, with `fst` itself.
    fn exercise(data: &[u8]) -> usize {
        let map = fst::Map::new(data.to_vec()).unwrap();
        let mut n = 0;
        let mut stream = map.stream();
        while let Some((k, v)) = stream.next() {
            assert_eq!(map.get(k), Some(v));
            n += 1;
        }
        for probe in [&b""[..], b"a", b"ab", b"zz", b"\xff", b"\x00\x01"] {
            let _ = map.get(probe);
            let _ = map.range().ge(probe).into_stream().into_byte_vec();
            let _ = map.range().gt(probe).lt(b"m").into_stream().into_byte_vec();
            let starts = fst::automaton::Str::new("a").starts_with();
            let _ = map.search(starts).ge(probe).into_stream().into_byte_vec();
        }
        n
    }

    fn written() -> Vec<(&'static str, Vec<u8>)> {
        let all_bytes: Vec<Vec<u8>> = (0..=u8::MAX).map(|b| vec![b]).collect();
        let pairs: Vec<Vec<u8>> = (0..=u8::MAX).map(|b| vec![b'k', b, b'z']).collect();
        let shared: Vec<String> = (0..2000).map(|i| format!("prefix/{i:05}/suffix")).collect();
        let few: Vec<Vec<u8>> = (0..40_u8).map(|b| vec![b'q', b * 3]).collect();
        vec![
            ("empty", build(Vec::<(&[u8], u64)>::new())),
            ("empty key", build([(&b""[..], 0)])),
            ("empty key with output", build([(&b""[..], 7)])),
            ("one key", build([(&b"hello"[..], 0)])),
            ("one key, large output", build([(&b"x"[..], u64::MAX)])),
            ("shared prefixes", ranks(&shared)),
            (
                "large outputs",
                build((0..300_u64).map(|i| (format!("{i:04}"), u64::MAX - 300 + i))),
            ),
            ("all byte values", ranks(&all_bytes)),
            ("all second bytes", ranks(&pairs)),
            ("index node", ranks(&few)),
            (
                "ids",
                build((0..500_u64).map(|i| (format!("doc-{i:08x}"), i * 7919 % 500))),
            ),
        ]
    }

    #[test]
    fn every_written_fst_passes() {
        for (name, data) in written() {
            let shape = check(&data).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let map = fst::Map::new(&data[..]).unwrap();
            assert_eq!(shape.keys, map.len() as u64, "{name}");
            let longest = map
                .stream()
                .into_byte_keys()
                .iter()
                .map(Vec::len)
                .max()
                .unwrap_or(0);
            assert_eq!(shape.longest_path, longest as u64, "{name}");
            assert_eq!(exercise(&data) as u64, shape.keys, "{name}");
        }
    }

    /// `check` accepts or rejects `data`; when it accepts, `fst` must read it all without panicking.
    fn no_panic(data: &[u8]) -> bool {
        let accepted = check(data).is_ok();
        if accepted {
            let r = catch_unwind(AssertUnwindSafe(|| exercise(data)));
            assert!(r.is_ok(), "accepted an FST fst panics on: {data:?}");
        }
        accepted
    }

    #[test]
    fn truncations_are_rejected() {
        for (name, data) in written() {
            for n in 0..data.len() {
                assert!(check(&data[..n]).is_err(), "{name} cut at {n}");
            }
        }
    }

    #[test]
    fn every_single_byte_flip_never_panics() {
        // Every value at every offset of a small FST (one with an index node is too slow for that: it gets a few).
        let small = ranks(&["a", "ab", "abc", "b", "bcd", "c", "zebra", "\u{e9}t\u{e9}"]);
        let mut accepted = 0;
        for at in 0..small.len() {
            for v in 0..=u8::MAX {
                let mut d = small.clone();
                d[at] = v;
                accepted += usize::from(no_panic(&d));
            }
        }
        // Bytes outside the reachable nodes and the checksum are ignored; everything else is mostly caught.
        assert!(accepted < small.len() * 256 / 2, "{accepted} accepted");
        let indexed = ranks(&(0..40_u8).map(|b| vec![b'q', b * 3, b]).collect::<Vec<_>>());
        for at in 0..indexed.len() {
            let b = indexed[at];
            for v in [
                0,
                1,
                u8::MAX,
                b ^ 0x01,
                b ^ 0x10,
                b ^ 0x80,
                b.wrapping_add(1),
            ] {
                let mut d = indexed.clone();
                d[at] = v;
                no_panic(&d);
            }
        }
    }

    #[test]
    fn header_and_footer_damage_is_rejected() {
        let data = ranks(&["alpha", "beta", "gamma"]);
        let footer = data.len() - FOOTER_LEN;
        let set = |at: usize, v: u64| {
            let mut d = data.clone();
            d[at..at + 8].copy_from_slice(&v.to_le_bytes());
            d
        };
        assert!(check(&set(0, 2)).is_err(), "version 2");
        assert!(check(&set(0, 4)).is_err(), "version 4");
        assert!(check(&set(footer, 2)).is_err(), "key count too low");
        assert!(check(&set(footer, 4)).is_err(), "key count too high");
        assert!(check(&set(footer, u64::MAX)).is_err(), "key count huge");
        let root = (footer - 1) as u64;
        assert!(
            check(&set(footer + 8, root - 1)).is_err(),
            "root moved down"
        );
        assert!(check(&set(footer + 8, root + 1)).is_err(), "root moved up");
        assert!(
            check(&set(footer + 8, u64::MAX)).is_err(),
            "root past the end"
        );
        assert!(
            check(&set(footer + 8, 0)).is_err(),
            "empty root in a non-empty FST"
        );
        // fst's own checks pass on these, so its reader would have run on them.
        assert!(fst::Map::new(set(footer + 8, root - 1)).is_ok());
        let mut empty = build([(&b""[..], 0)]);
        empty[HEADER_LEN..HEADER_LEN + 8].copy_from_slice(&5_u64.to_le_bytes());
        assert!(check(&empty).is_err(), "empty FST with a wrong key count");
    }

    /// A hand-made one-key FST `"a" -> 0`: a final leaf with no transitions at 16..=18 (sizes 0, count 0, state
    /// final), then the root, a `OneTrans` on `a` whose address delta is `delta` (little-endian, `delta.len()` bytes),
    /// counted from its first byte, 19. Delta 1 is the leaf.
    fn one_trans(delta: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&VERSION.to_le_bytes());
        d.extend_from_slice(&0_u64.to_le_bytes());
        d.extend_from_slice(&[0, 0, 0b0100_0000]);
        d.extend_from_slice(delta);
        // Sizes (tsize, osize 0), the input (no common input), the state byte.
        d.extend_from_slice(&[(delta.len() as u8) << 4, b'a', 0b1000_0000]);
        let root = d.len() - 1;
        d.extend_from_slice(&1_u64.to_le_bytes());
        d.extend_from_slice(&(root as u64).to_le_bytes());
        d.extend_from_slice(&0_u32.to_le_bytes());
        d
    }

    #[test]
    fn transition_addresses_are_checked() {
        let leaf = one_trans(&[1]);
        assert_eq!(check(&leaf).map(|s| s.keys), Ok(1));
        assert_eq!(exercise(&leaf), 1);
        // Delta 0 is the empty final node; so is a delta of exactly `end`, which lands on address 0.
        for delta in [0, 19] {
            let d = one_trans(&[delta]);
            assert_eq!(check(&d).map(|s| s.keys), Ok(1), "delta {delta}");
            assert!(no_panic(&d));
        }
        // Into the header (19 - 4 = 15), and before the payload's start (fst would underflow).
        for delta in [4, 18, 20, 200] {
            let d = one_trans(&[delta]);
            assert!(fst::Map::new(&d[..]).is_ok(), "fst opens delta {delta}");
            assert!(check(&d).is_err(), "delta {delta}");
        }
    }

    #[test]
    fn a_transition_to_itself_or_above_is_rejected() {
        // Deltas are subtracted, so a target at or above the node only comes from `end - delta` wrapping around:
        // with an 8-byte delta of 2^64 - k, a release build of fst would go to `end + k` and loop (or index out of
        // bounds); a debug build panics on the subtraction. The root is at 19 + 8 + 2 = 29.
        for k in [1_u64, 5, 10, 11, 40] {
            let d = one_trans(&0_u64.wrapping_sub(k).to_le_bytes());
            assert!(fst::Map::new(&d[..]).is_ok(), "fst opens k = {k}");
            assert!(check(&d).is_err(), "k = {k}");
        }
        // The 8-byte delta itself is fine.
        assert_eq!(
            check(&one_trans(&1_u64.to_le_bytes())).map(|s| s.keys),
            Ok(1)
        );
    }

    #[test]
    fn a_node_reaching_into_the_header_is_rejected() {
        // An `AnyTrans` root with one transition but only its sizes and state bytes: its input and delta would be
        // the header's last two bytes (input 0, delta 0, the empty node), which fst would read happily.
        let mut d = Vec::new();
        d.extend_from_slice(&VERSION.to_le_bytes());
        d.extend_from_slice(&0_u64.to_le_bytes());
        d.extend_from_slice(&[0x10, 0b0000_0001]);
        let root = d.len() - 1;
        d.extend_from_slice(&1_u64.to_le_bytes());
        d.extend_from_slice(&(root as u64).to_le_bytes());
        d.extend_from_slice(&0_u32.to_le_bytes());
        assert!(check(&d).is_err());
    }

    #[test]
    fn a_self_loop_through_one_trans_next_is_rejected() {
        // OneTransNext targets `end - 1`: a node whose own bytes start at 16 would target 15, in the header.
        let mut d = Vec::new();
        d.extend_from_slice(&VERSION.to_le_bytes());
        d.extend_from_slice(&0_u64.to_le_bytes());
        d.extend_from_slice(&[b'a', 0b1100_0000]);
        let root = d.len() - 1;
        d.extend_from_slice(&1_u64.to_le_bytes());
        d.extend_from_slice(&(root as u64).to_le_bytes());
        d.extend_from_slice(&0_u32.to_le_bytes());
        assert!(fst::Map::new(&d[..]).is_ok(), "fst opens it");
        assert!(check(&d).is_err());
    }

    #[test]
    fn output_overflow_is_rejected() {
        // Two transitions whose outputs sum past u64::MAX: "a" -> MAX on the root, then a final output of 1.
        let ok = build([(&b"a"[..], u64::MAX)]);
        assert!(check(&ok).is_ok());
        let ab = build([(&b"a"[..], u64::MAX - 1), (&b"ab"[..], u64::MAX)]);
        let shape = check(&ab).unwrap();
        assert_eq!(shape.keys, 2);
        // Bump every 8-byte run equal to MAX - 1 to MAX: the "ab" path then sums MAX + 1.
        let needle = (u64::MAX - 1).to_le_bytes();
        let mut bumped = ab.clone();
        let mut changed = false;
        for at in 0..bumped.len().saturating_sub(8) {
            if bumped[at..at + 8] == needle {
                bumped[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
                changed = true;
            }
        }
        assert!(changed);
        assert!(check(&bumped).is_err());
    }
}
