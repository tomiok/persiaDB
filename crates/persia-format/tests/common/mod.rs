//! Shared helpers for the corruption tests: error constructors and byte-level mutation strategies.

// Each test crate uses a subset of these helpers; test-only code indexes and casts on purpose.
#![allow(dead_code, clippy::indexing_slicing)]

use persia_format::{Corruption, Error};
use proptest::prelude::*;
use proptest::sample::Index;

pub fn corrupt(offset: u64, reason: Corruption) -> Error {
    Error::Corrupt { offset, reason }
}

pub fn eof(offset: u64, needed: usize, available: usize) -> Error {
    corrupt(
        offset,
        Corruption::UnexpectedEof {
            needed: u64::try_from(needed).unwrap_or(u64::MAX),
            available: u64::try_from(available).unwrap_or(u64::MAX),
        },
    )
}

/// A byte-level edit to a valid encoding: the realistic corruption patterns plus pure noise.
#[derive(Debug, Clone)]
pub enum Mutation {
    Keep,
    FlipBit(Index, u8),
    SetByte(Index, u8),
    Truncate(Index),
    Append(Vec<u8>),
    Replace(Vec<u8>),
}

pub fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        1 => Just(Mutation::Keep),
        4 => (any::<Index>(), 0_u8..8).prop_map(|(i, b)| Mutation::FlipBit(i, b)),
        2 => (any::<Index>(), any::<u8>()).prop_map(|(i, v)| Mutation::SetByte(i, v)),
        2 => any::<Index>().prop_map(Mutation::Truncate),
        1 => proptest::collection::vec(any::<u8>(), 1..20).prop_map(Mutation::Append),
        1 => proptest::collection::vec(any::<u8>(), 0..600).prop_map(Mutation::Replace),
    ]
}

pub fn apply(mut bytes: Vec<u8>, m: &Mutation) -> Vec<u8> {
    match m {
        Mutation::FlipBit(i, b) if !bytes.is_empty() => {
            let at = i.index(bytes.len());
            bytes[at] ^= 1 << b;
        }
        Mutation::SetByte(i, v) if !bytes.is_empty() => {
            let at = i.index(bytes.len());
            bytes[at] = *v;
        }
        Mutation::Truncate(i) => bytes.truncate(i.index(bytes.len() + 1)),
        Mutation::Append(tail) => bytes.extend(tail),
        Mutation::Replace(noise) => bytes.clone_from(noise),
        Mutation::Keep | Mutation::FlipBit(..) | Mutation::SetByte(..) => {}
    }
    bytes
}

/// Absolute placement of the block: a base offset (up to near the top of u64) and a junk prefix the reader
/// has already consumed, so error offsets must include both.
pub fn placement() -> impl Strategy<Value = (u64, Vec<u8>)> {
    (
        prop_oneof![Just(0_u64), 0_u64..1 << 40, Just(u64::MAX - (1 << 20))],
        proptest::collection::vec(any::<u8>(), 0..5),
    )
}
