//! The CBOR subset of SPEC §6.4.2 (RFC 8949): definite lengths, shortest arguments, no tags, 64-bit floats, and
//! only the simple values `false`, `true` and `null`. Map key order is checked by the caller.

use super::DocumentCorruption;

const UINT: u8 = 0;
const NINT: u8 = 1;
const BYTES: u8 = 2;
const TEXT: u8 = 3;
const ARRAY: u8 = 4;
const MAP: u8 = 5;
const TAG: u8 = 6;
const SIMPLE: u8 = 7;

const FALSE: u8 = 0xf4;
const TRUE: u8 = 0xf5;
const NULL: u8 = 0xf6;
const FLOAT64: u8 = 0xfb;

/// Appends CBOR items to a buffer.
#[derive(Debug, Default)]
pub(super) struct Encoder(pub(super) Vec<u8>);

impl Encoder {
    // Each arm's range makes the narrowing exact.
    #[allow(clippy::cast_possible_truncation)]
    fn head(&mut self, major: u8, arg: u64) {
        let major = major << 5;
        match arg {
            0..24 => self.0.push(major | arg as u8),
            24..0x100 => self.0.extend([major | 0x18, arg as u8]),
            0x100..0x1_0000 => {
                self.0.push(major | 0x19);
                self.0.extend((arg as u16).to_be_bytes());
            }
            0x1_0000..0x1_0000_0000 => {
                self.0.push(major | 0x1a);
                self.0.extend((arg as u32).to_be_bytes());
            }
            _ => {
                self.0.push(major | 0x1b);
                self.0.extend(arg.to_be_bytes());
            }
        }
    }

    pub(super) fn uint(&mut self, v: u64) {
        self.head(UINT, v);
    }

    pub(super) fn int(&mut self, v: i64) {
        match u64::try_from(v) {
            Ok(v) => self.head(UINT, v),
            // -1 - v = |v| - 1, which cannot overflow.
            Err(_) => self.head(NINT, v.unsigned_abs().saturating_sub(1)),
        }
    }

    pub(super) fn bytes(&mut self, b: &[u8]) {
        self.head(BYTES, len_u64(b.len()));
        self.0.extend_from_slice(b);
    }

    pub(super) fn text(&mut self, s: &str) {
        self.head(TEXT, len_u64(s.len()));
        self.0.extend_from_slice(s.as_bytes());
    }

    pub(super) fn array(&mut self, len: usize) {
        self.head(ARRAY, len_u64(len));
    }

    pub(super) fn map(&mut self, len: usize) {
        self.head(MAP, len_u64(len));
    }

    pub(super) fn f64(&mut self, v: f64) {
        self.0.push(FLOAT64);
        self.0.extend(v.to_bits().to_be_bytes());
    }

    pub(super) fn bool(&mut self, v: bool) {
        self.0.push(if v { TRUE } else { FALSE });
    }

    pub(super) fn null(&mut self) {
        self.0.push(NULL);
    }
}

fn len_u64(len: usize) -> u64 {
    // usize is at most 64 bits on every supported target.
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// One decoded CBOR item; arrays and maps give their length, their elements follow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Item<'a> {
    /// An unsigned integer.
    Uint(u64),
    /// The negative integer `-1 - n`.
    Nint(u64),
    Bytes(&'a [u8]),
    Text(&'a str),
    /// An array of this many items; at most the bytes left.
    Array(usize),
    /// A map of this many key/value pairs; at most half the bytes left.
    Map(usize),
    Bool(bool),
    Null,
    F64(f64),
}

impl Item<'_> {
    pub(super) const fn kind(&self) -> &'static str {
        match self {
            Self::Uint(_) | Self::Nint(_) => "an integer",
            Self::Bytes(_) => "a byte string",
            Self::Text(_) => "a text string",
            Self::Array(_) => "an array",
            Self::Map(_) => "a map",
            Self::Bool(_) => "a bool",
            Self::Null => "null",
            Self::F64(_) => "a float",
        }
    }
}

/// A decoding failure at an offset relative to the start of the input.
pub(super) type Failure = (u64, DocumentCorruption);

/// Reads CBOR items from a buffer.
#[derive(Debug)]
pub(super) struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub(super) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// The offset of the next byte.
    pub(super) fn offset(&self) -> u64 {
        len_u64(self.pos)
    }

    pub(super) fn is_at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Failure> {
        let eof = (self.offset(), DocumentCorruption::UnexpectedEof);
        let end = self.pos.checked_add(len).ok_or(eof.clone())?;
        let bytes = self.buf.get(self.pos..end).ok_or(eof)?;
        self.pos = end;
        Ok(bytes)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], Failure> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// The next item; its offset is [`Self::offset`] before the call.
    pub(super) fn next(&mut self) -> Result<Item<'a>, Failure> {
        let start = self.offset();
        let [initial] = self.take_array()?;
        let unsupported = (start, DocumentCorruption::UnsupportedItem { initial });
        let major = initial >> 5;
        let info = initial & 0x1f;
        if major == SIMPLE {
            return match initial {
                FALSE => Ok(Item::Bool(false)),
                TRUE => Ok(Item::Bool(true)),
                NULL => Ok(Item::Null),
                FLOAT64 => Ok(Item::F64(f64::from_bits(u64::from_be_bytes(
                    self.take_array()?,
                )))),
                _ => Err(unsupported),
            };
        }
        if major == TAG {
            return Err(unsupported);
        }
        let (arg, min) = match info {
            0..24 => (u64::from(info), 0),
            24 => (u64::from(u8::from_be_bytes(self.take_array()?)), 24),
            25 => (u64::from(u16::from_be_bytes(self.take_array()?)), 0x100),
            26 => (u64::from(u32::from_be_bytes(self.take_array()?)), 0x1_0000),
            27 => (u64::from_be_bytes(self.take_array()?), 0x1_0000_0000),
            _ => return Err(unsupported),
        };
        if arg < min {
            return Err((start, DocumentCorruption::NonCanonicalHead));
        }
        let eof = (self.offset(), DocumentCorruption::UnexpectedEof);
        // Lengths beyond the input are corrupt before anything is allocated for them.
        let len = usize::try_from(arg).map_err(|_| eof.clone());
        match major {
            UINT => Ok(Item::Uint(arg)),
            NINT => Ok(Item::Nint(arg)),
            BYTES => self.take(len?).map(Item::Bytes),
            TEXT => {
                let at = self.offset();
                let bytes = self.take(len?)?;
                std::str::from_utf8(bytes)
                    .map(Item::Text)
                    .map_err(|_| (at, DocumentCorruption::InvalidUtf8))
            }
            ARRAY => {
                let len = len?;
                if len > self.remaining() {
                    return Err(eof);
                }
                Ok(Item::Array(len))
            }
            MAP => {
                let len = len?;
                if len > self.remaining() / 2 {
                    return Err(eof);
                }
                Ok(Item::Map(len))
            }
            _ => Err(unsupported),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_one(bytes: &[u8]) -> Result<Item<'_>, Failure> {
        let mut d = Decoder::new(bytes);
        let item = d.next()?;
        assert!(d.is_at_end(), "{bytes:02x?} left bytes");
        Ok(item)
    }

    #[test]
    fn integer_heads_use_the_shortest_form() {
        // RFC 8949 Appendix A.
        let cases: [(i64, &[u8]); 12] = [
            (0, &[0x00]),
            (23, &[0x17]),
            (24, &[0x18, 0x18]),
            (255, &[0x18, 0xff]),
            (256, &[0x19, 0x01, 0x00]),
            (65_536, &[0x1a, 0x00, 0x01, 0x00, 0x00]),
            (1 << 32, &[0x1b, 0, 0, 0, 1, 0, 0, 0, 0]),
            (-1, &[0x20]),
            (-24, &[0x37]),
            (-25, &[0x38, 0x18]),
            (-1000, &[0x39, 0x03, 0xe7]),
            (
                i64::MIN,
                &[0x3b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            ),
        ];
        for (v, bytes) in cases {
            let mut e = Encoder::default();
            e.int(v);
            assert_eq!(e.0, bytes, "{v}");
            let expected =
                u64::try_from(v).map_or_else(|_| Item::Nint(v.unsigned_abs() - 1), Item::Uint);
            assert_eq!(decode_one(bytes), Ok(expected), "{v}");
        }
        let mut e = Encoder::default();
        e.uint(u64::MAX);
        assert_eq!(e.0, [0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn overlong_heads_are_corrupt() {
        for bytes in [
            &[0x18, 0x17][..],
            &[0x19, 0x00, 0xff],
            &[0x1a, 0x00, 0x00, 0xff, 0xff],
            &[0x1b, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff],
            &[0x78, 0x00],
            &[0x98, 0x01, 0x00],
        ] {
            assert_eq!(
                decode_one(bytes),
                Err((0, DocumentCorruption::NonCanonicalHead)),
                "{bytes:02x?}"
            );
        }
    }

    #[test]
    fn items_outside_the_subset_are_unsupported() {
        // Indefinite lengths, reserved additional info, tags, half/single floats, undefined, other simple values.
        for initial in [
            0x1c, 0x1f, 0x5f, 0x7f, 0x9f, 0xbf, 0xc0, 0xc1, 0xd8, 0xf0, 0xf7, 0xf8, 0xf9, 0xfa,
            0xff,
        ] {
            let bytes = [initial, 0, 0, 0, 0, 0, 0, 0, 0];
            assert_eq!(
                Decoder::new(&bytes).next(),
                Err((0, DocumentCorruption::UnsupportedItem { initial })),
                "{initial:#04x}"
            );
        }
    }

    #[test]
    fn lengths_beyond_the_input_are_corrupt_before_allocation() {
        let eof = |at| Err((at, DocumentCorruption::UnexpectedEof));
        assert_eq!(
            decode_one(&[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            eof(9)
        );
        assert_eq!(decode_one(&[0xbb, 0, 0, 0, 1, 0, 0, 0, 0]), eof(9));
        assert_eq!(decode_one(&[0x62, b'a']), eof(1));
        assert_eq!(
            decode_one(&[0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            eof(9)
        );
        assert_eq!(decode_one(&[0x19, 0x01]), eof(1));
        assert_eq!(decode_one(&[]), eof(0));
        let mut d = Decoder::new(&[0x82, 0, 0]);
        assert_eq!(d.next(), Ok(Item::Array(2)));
        assert_eq!(Decoder::new(&[0x83, 0, 0]).next(), eof(1));
        assert_eq!(Decoder::new(&[0xa2, 0, 0, 0]).next(), eof(1));
    }

    #[test]
    fn text_must_be_utf8() {
        assert_eq!(
            decode_one(&[0x62, 0xc3, 0x28]),
            Err((1, DocumentCorruption::InvalidUtf8))
        );
        assert_eq!(decode_one(&[0x62, 0xc3, 0xa9]), Ok(Item::Text("é")));
    }

    #[test]
    fn simple_values_and_floats() {
        let mut e = Encoder::default();
        e.bool(false);
        e.bool(true);
        e.null();
        e.f64(-0.0);
        e.f64(1.5);
        let mut d = Decoder::new(&e.0);
        assert_eq!(d.next(), Ok(Item::Bool(false)));
        assert_eq!(d.next(), Ok(Item::Bool(true)));
        assert_eq!(d.next(), Ok(Item::Null));
        assert!(matches!(d.next(), Ok(Item::F64(x)) if x.to_bits() == (-0.0f64).to_bits()));
        assert_eq!(d.next(), Ok(Item::F64(1.5)));
        assert!(d.is_at_end());
        assert_eq!(
            e.0[3..],
            [
                0xfb, 0x80, 0, 0, 0, 0, 0, 0, 0, 0xfb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0
            ]
        );
    }
}
