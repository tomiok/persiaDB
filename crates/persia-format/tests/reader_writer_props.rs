//! Property tests for `Reader`/`Writer` (ROADMAP 1.1.1.3): round-trips, truncation, and the `Reader`
//! against a naive slice-indexing reference model. `PROPTEST_CASES` scales them (nightly uses 10 000).

// Test-only code: the model indexes slices and unwraps constants on purpose. clippy.toml's test exemptions
// cover #[test] fns but not helpers in integration-test files, hence this file-scoped allow.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

use std::num::NonZeroUsize;

use persia_format::{Corruption, Error, Reader, Writer};
use proptest::prelude::*;
use proptest::sample::Index;

/// One encodable item. `Zeros` and `Pad` exercise reserved fields and absolute alignment (SPEC §4.1).
#[derive(Debug, Clone, PartialEq)]
enum Value {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(u128),
    I32(i32),
    I64(i64),
    Bytes(Vec<u8>),
    Zeros(usize),
    Pad(NonZeroUsize),
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<u8>().prop_map(Value::U8),
        any::<u16>().prop_map(Value::U16),
        any::<u32>().prop_map(Value::U32),
        any::<u64>().prop_map(Value::U64),
        any::<u128>().prop_map(Value::U128),
        any::<i32>().prop_map(Value::I32),
        any::<i64>().prop_map(Value::I64),
        prop::collection::vec(any::<u8>(), 0..48).prop_map(Value::Bytes),
        (0_usize..24).prop_map(Value::Zeros),
        (1_usize..=16).prop_map(|a| Value::Pad(NonZeroUsize::new(a).unwrap())),
    ]
}

/// Bases below 2^48 keep `base + len` far from the (unreachable) saturation at `u64::MAX`.
fn base() -> impl Strategy<Value = u64> {
    prop_oneof![Just(0_u64), 0_u64..64, 0_u64..(1 << 48)]
}

fn encode(w: &mut Writer, v: &Value) {
    match v {
        Value::U8(x) => w.write_u8(*x),
        Value::U16(x) => w.write_u16(*x),
        Value::U32(x) => w.write_u32(*x),
        Value::U64(x) => w.write_u64(*x),
        Value::U128(x) => w.write_u128(*x),
        Value::I32(x) => w.write_i32(*x),
        Value::I64(x) => w.write_i64(*x),
        Value::Bytes(b) => w.write_bytes(b),
        Value::Zeros(n) => w.write_zeros(*n),
        Value::Pad(a) => w.pad_to(*a),
    }
}

/// Decodes the item shaped like `v` (its payload is ignored, only its kind and length matter).
fn decode(r: &mut Reader<'_>, v: &Value) -> Result<Value, Error> {
    Ok(match v {
        Value::U8(_) => Value::U8(r.read_u8()?),
        Value::U16(_) => Value::U16(r.read_u16()?),
        Value::U32(_) => Value::U32(r.read_u32()?),
        Value::U64(_) => Value::U64(r.read_u64()?),
        Value::U128(_) => Value::U128(r.read_u128()?),
        Value::I32(_) => Value::I32(r.read_i32()?),
        Value::I64(_) => Value::I64(r.read_i64()?),
        Value::Bytes(b) => Value::Bytes(r.read_bytes(b.len())?.to_vec()),
        Value::Zeros(n) => {
            r.expect_zeros(*n)?;
            Value::Zeros(*n)
        }
        Value::Pad(a) => {
            r.skip_padding(*a)?;
            Value::Pad(*a)
        }
    })
}

/// Encodes `values` and returns the bytes plus each item's absolute `(start, end)` offsets.
fn encode_all(base: u64, values: &[Value]) -> (Vec<u8>, Vec<(u64, u64)>) {
    let mut w = Writer::with_base_offset(base);
    let spans = values
        .iter()
        .map(|v| {
            let start = w.offset();
            encode(&mut w, v);
            (start, w.offset())
        })
        .collect();
    (w.into_inner(), spans)
}

fn eof(offset: u64, needed: u64, available: u64) -> Error {
    Error::Corrupt {
        offset,
        reason: Corruption::UnexpectedEof { needed, available },
    }
}

proptest! {
    /// decode(encode(xs)) == xs, consuming exactly the encoded bytes, at any base offset.
    #[test]
    fn round_trip(base in base(), values in prop::collection::vec(value(), 0..48)) {
        let (bytes, _) = encode_all(base, &values);
        let mut r = Reader::with_base_offset(&bytes, base);
        for v in &values {
            prop_assert_eq!(decode(&mut r, v), Ok(v.clone()));
        }
        prop_assert!(r.is_empty());
        prop_assert_eq!(r.offset(), base + bytes.len() as u64);
    }

    /// Same input, same bytes (CLAUDE.md golden-file determinism).
    #[test]
    fn encoding_is_deterministic(base in base(), values in prop::collection::vec(value(), 0..48)) {
        prop_assert_eq!(encode_all(base, &values), encode_all(base, &values));
    }

    /// Truncating the encoding anywhere yields exactly the items that fit, then one clean EOF at the
    /// start of the first item that does not, and the reader stays there.
    #[test]
    fn truncation_yields_complete_prefix_then_exact_eof(
        base in base(),
        values in prop::collection::vec(value(), 1..32),
        cut in any::<Index>(),
    ) {
        let (bytes, spans) = encode_all(base, &values);
        let cut = cut.index(bytes.len() + 1);
        let end = base + cut as u64;
        let mut r = Reader::with_base_offset(&bytes[..cut], base);
        for (v, &(start, stop)) in values.iter().zip(&spans) {
            if stop <= end {
                prop_assert_eq!(decode(&mut r, v), Ok(v.clone()));
            } else {
                prop_assert_eq!(decode(&mut r, v), Err(eof(start, stop - start, end - start)));
                prop_assert_eq!(r.offset(), start);
                break;
            }
        }
    }
}

/// Reader operations driven against a naive model of the same buffer.
#[derive(Debug, Clone)]
enum Op {
    Bytes(usize),
    U32,
    U64,
    Skip(usize),
    Zeros(usize),
    Pad(NonZeroUsize),
    Sub(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0_usize..80).prop_map(Op::Bytes),
        Just(Op::U32),
        Just(Op::U64),
        (0_usize..80).prop_map(Op::Skip),
        (0_usize..24).prop_map(Op::Zeros),
        (1_usize..=16).prop_map(|a| Op::Pad(NonZeroUsize::new(a).unwrap())),
        (0_usize..40).prop_map(Op::Sub),
    ]
}

/// Mostly-zero buffers too, so `expect_zeros`/`skip_padding` succeed often enough to matter.
fn buffer() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..96),
        prop::collection::vec(prop_oneof![9 => Just(0_u8), 1 => any::<u8>()], 0..96),
    ]
}

/// What the model says an op returns: the bytes it consumes, or the error it must produce.
fn model(buf: &[u8], base: u64, pos: usize, op: &Op) -> Result<usize, Error> {
    let abs = base + pos as u64;
    let available = buf.len() - pos;
    let len = match op {
        Op::Bytes(n) | Op::Skip(n) | Op::Zeros(n) | Op::Sub(n) => *n,
        Op::U32 => 4,
        Op::U64 => 8,
        Op::Pad(a) => {
            let a = a.get() as u64;
            ((a - abs % a) % a) as usize
        }
    };
    if len > available {
        return Err(eof(abs, len as u64, available as u64));
    }
    if matches!(op, Op::Zeros(_) | Op::Pad(_))
        && let Some(i) = buf[pos..pos + len].iter().position(|&b| b != 0)
    {
        return Err(Error::Corrupt {
            offset: abs + i as u64,
            reason: Corruption::NonZeroReserved {
                value: buf[pos + i],
            },
        });
    }
    Ok(len)
}

proptest! {
    /// Every Reader op matches the model: same result, same bytes, and a failed op never moves the cursor.
    #[test]
    fn reader_matches_naive_model(
        buf in buffer(),
        base in base(),
        ops in prop::collection::vec(op(), 0..48),
    ) {
        let mut r = Reader::with_base_offset(&buf, base);
        let mut pos = 0;
        for op in &ops {
            let expected = model(&buf, base, pos, op);
            let got = match op {
                Op::Bytes(n) => r.read_bytes(*n).map(|b| {
                    assert_eq!(b, &buf[pos..pos + n]);
                    *n
                }),
                Op::U32 => r.read_u32().map(|x| {
                    assert_eq!(x.to_le_bytes(), buf[pos..pos + 4]);
                    4
                }),
                Op::U64 => r.read_u64().map(|x| {
                    assert_eq!(x.to_le_bytes(), buf[pos..pos + 8]);
                    8
                }),
                Op::Skip(n) => r.skip(*n).map(|()| *n),
                Op::Zeros(n) => r.expect_zeros(*n).map(|()| *n),
                Op::Pad(a) => r.skip_padding(*a).map(|()| expected.clone().unwrap_or(0)),
                Op::Sub(n) => r.sub_reader(*n).map(|sub| {
                    assert_eq!(sub.rest(), &buf[pos..pos + n]);
                    assert_eq!(sub.offset(), base + pos as u64);
                    *n
                }),
            };
            prop_assert_eq!(&got, &expected, "op {:?} at pos {}", op, pos);
            if let Ok(n) = got {
                pos += n;
            }
            prop_assert_eq!(r.position(), pos);
            prop_assert_eq!(r.offset(), base + pos as u64);
            prop_assert_eq!(r.remaining(), buf.len() - pos);
            prop_assert_eq!(r.rest(), &buf[pos..]);
        }
    }
}
