//! The double-buffered superblock: two CRC-protected slots after the header, the newer valid one wins
//! (SPEC §4.3; ADR-0007).
//!
//! [`Superblock::decode`] checks a slot structurally (CRC, reserved bytes). [`Superblock::read_active`] decodes
//! both slots and applies the selection table of SPEC §4.3. Offsets such as `manifest_off` and `log_end` are
//! validated against the container by its reader (ROADMAP 2.4.4), which knows the file length.

use crate::checksummed::{Checksummed, ChecksummedLayout};
use crate::error::{Corruption, EncodeError, Error, Result};
use crate::header::HEADER_LEN;
use crate::reader::Reader;
use crate::writer::Writer;

/// Size of one superblock slot: a whole page, so the slots never share a sector (ADR-0008).
pub const SLOT_LEN: usize = 4096;
/// Size of both slots together, as read by [`Superblock::read_active`].
pub const SLOTS_LEN: usize = 2 * SLOT_LEN;
/// Where the first frame starts: right after slot B (SPEC §4.2).
pub const FIRST_FRAME_OFFSET: u64 = (HEADER_LEN + SLOTS_LEN) as u64;

/// Bytes 48..4092.
const RESERVED_LEN: usize = SLOT_LEN - 48 - 4;

/// One of the two superblock slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The slot at byte 4096.
    A,
    /// The slot at byte 8192.
    B,
}

impl Slot {
    /// Absolute offset of the slot in the container.
    pub const fn offset(self) -> u64 {
        match self {
            Self::A => HEADER_LEN as u64,
            Self::B => (HEADER_LEN + SLOT_LEN) as u64,
        }
    }

    /// The other slot: where the next commit goes when this one is active (SPEC §4.5).
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// The contents of one superblock slot (SPEC §4.3).
///
/// ```
/// use persia_format::superblock::{SLOT_LEN, Superblock};
/// use persia_format::{Reader, Writer};
///
/// let sb = Superblock { seq: 3, log_end: 8192, ..Superblock::default() };
/// let mut w = Writer::new();
/// sb.encode(&mut w)?;
/// assert_eq!(w.len(), SLOT_LEN);
/// assert_eq!(Superblock::decode(&mut Reader::new(w.as_slice()))?, sb);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Superblock {
    /// Monotonic commit counter.
    pub seq: u64,
    /// Offset of the Manifest frame.
    pub manifest_off: u64,
    /// Length of the Manifest frame.
    pub manifest_len: u64,
    /// End of the last durable frame.
    pub log_end: u64,
    /// First WAL frame still needed.
    pub wal_start: u64,
    /// Writer fencing token (SPEC §11.3).
    pub fence_token: u64,
}

/// The slot a container opens with, and what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveSuperblock {
    /// Which slot was chosen; the next commit writes [`Slot::other`].
    pub slot: Slot,
    /// Its contents.
    pub superblock: Superblock,
}

impl Superblock {
    /// Appends the [`SLOT_LEN`] slot bytes, CRC included.
    ///
    /// # Errors
    /// Only [`EncodeError::LayoutLen`], which would be a bug in this module: every field value is representable.
    pub fn encode(&self, w: &mut Writer) -> Result<(), EncodeError> {
        Checksummed(*self).encode(w)
    }

    /// Reads one slot and checks its CRC and reserved bytes.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a short input, a CRC mismatch (also what a never-written, all-zero slot gives) or
    /// non-zero reserved bytes. On error the position is unchanged.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Checksummed::<Self>::decode(r).map(|Checksummed(sb)| sb)
    }

    /// Reads both slots ([`SLOTS_LEN`] bytes, slot A first) and picks the active one (SPEC §4.3):
    ///
    /// | Slot A  | Slot B  | Active |
    /// |---------|---------|--------|
    /// | valid   | valid   | higher `seq`; equal `seq` and contents ⇒ A; equal `seq`, different contents ⇒ error |
    /// | valid   | invalid | A |
    /// | invalid | valid   | B |
    /// | invalid | invalid | error |
    ///
    /// ```
    /// use persia_format::superblock::{Slot, Superblock};
    /// use persia_format::{Reader, Writer};
    ///
    /// let mut w = Writer::new();
    /// Superblock { seq: 4, ..Superblock::default() }.encode(&mut w)?;
    /// Superblock { seq: 5, ..Superblock::default() }.encode(&mut w)?;
    /// let active = Superblock::read_active(&mut Reader::new(w.as_slice()))?;
    /// assert_eq!((active.slot, active.superblock.seq), (Slot::B, 5));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::Corrupt`] with [`Corruption::UnexpectedEof`] if fewer than [`SLOTS_LEN`] bytes remain,
    /// [`Corruption::NoValidSuperblock`] (at slot A) if neither slot is valid, or
    /// [`Corruption::SuperblockConflict`] (at slot B) for equal `seq` with different contents. On error the
    /// position is unchanged. Why each slot is invalid is not reported: diagnostics (`persia verify`) call
    /// [`Superblock::decode`] on each slot.
    pub fn read_active(r: &mut Reader<'_>) -> Result<ActiveSuperblock> {
        let mut ahead = r.clone();
        let mut slots = ahead.sub_reader(SLOTS_LEN)?;
        let a_offset = slots.offset();
        let a = Self::decode(&mut slots.sub_reader(SLOT_LEN)?);
        let b_offset = slots.offset();
        let b = Self::decode(&mut slots.sub_reader(SLOT_LEN)?);
        let active = match (a, b) {
            (Ok(a), Ok(b)) if a.seq == b.seq && a != b => {
                return Err(Error::Corrupt {
                    offset: b_offset,
                    reason: Corruption::SuperblockConflict { seq: a.seq },
                });
            }
            (Ok(a), Ok(b)) if b.seq > a.seq => ActiveSuperblock {
                slot: Slot::B,
                superblock: b,
            },
            (Ok(a), _) => ActiveSuperblock {
                slot: Slot::A,
                superblock: a,
            },
            (Err(_), Ok(b)) => ActiveSuperblock {
                slot: Slot::B,
                superblock: b,
            },
            (Err(_), Err(_)) => {
                return Err(Error::Corrupt {
                    offset: a_offset,
                    reason: Corruption::NoValidSuperblock,
                });
            }
        };
        *r = ahead;
        Ok(active)
    }
}

impl ChecksummedLayout for Superblock {
    const LEN: usize = SLOT_LEN;
    const CRC_OFFSET: usize = SLOT_LEN - 4;

    fn encode_fields(&self, w: &mut Writer) {
        w.write_u64(self.seq);
        w.write_u64(self.manifest_off);
        w.write_u64(self.manifest_len);
        w.write_u64(self.log_end);
        w.write_u64(self.wal_start);
        w.write_u64(self.fence_token);
        w.write_zeros(RESERVED_LEN);
        w.write_u32(0); // CRC placeholder
    }

    fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
        let sb = Self {
            seq: r.read_u64()?,
            manifest_off: r.read_u64()?,
            manifest_len: r.read_u64()?,
            log_end: r.read_u64()?,
            wal_start: r.read_u64()?,
            fence_token: r.read_u64()?,
        };
        r.expect_zeros(RESERVED_LEN)?;
        r.skip(4)?; // CRC
        Ok(sb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checksum::crc32c;

    const BASE: u64 = 4096;

    fn sb(seq: u64) -> Superblock {
        Superblock {
            seq,
            manifest_off: seq.wrapping_add(0x1_0000),
            manifest_len: 0x200,
            log_end: 0x2_0000,
            wal_start: 0x8000,
            fence_token: 9,
        }
    }

    fn encode(sb: &Superblock) -> Vec<u8> {
        let mut w = Writer::new();
        sb.encode(&mut w).unwrap();
        w.into_inner()
    }

    /// A slot that fails decoding in one of the ways SPEC §4.3 calls invalid.
    #[derive(Debug, Clone, Copy)]
    enum Invalid {
        NeverWritten,
        BitFlip,
        Reserved,
    }

    fn invalid(kind: Invalid, seq: u64) -> Vec<u8> {
        let mut bytes = encode(&sb(seq));
        match kind {
            Invalid::NeverWritten => bytes.fill(0),
            Invalid::BitFlip => bytes[3] ^= 0x10,
            Invalid::Reserved => {
                bytes[100] = 1;
                let crc = crc32c(&bytes[..SLOT_LEN - 4]);
                bytes[SLOT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
            }
        }
        bytes
    }

    fn read_active(a: &[u8], b: &[u8]) -> Result<ActiveSuperblock> {
        let bytes = [a, b].concat();
        let mut r = Reader::with_base_offset(&bytes, BASE);
        let result = Superblock::read_active(&mut r);
        let expected_pos = if result.is_ok() { SLOTS_LEN } else { 0 };
        assert_eq!(r.position(), expected_pos);
        result
    }

    #[allow(clippy::unnecessary_wraps)] // compared against `read_active` results
    fn active(slot: Slot, superblock: Superblock) -> Result<ActiveSuperblock> {
        Ok(ActiveSuperblock { slot, superblock })
    }

    #[test]
    fn golden_slot() {
        let mut expected = Vec::new();
        for v in [7_u64, 0x1_0007, 0x200, 0x2_0000, 0x8000, 9] {
            expected.extend_from_slice(&v.to_le_bytes());
        }
        expected.resize(SLOT_LEN - 4, 0);
        let crc = crc32c(&expected);
        expected.extend_from_slice(&crc.to_le_bytes());
        assert_eq!(encode(&sb(7)), expected);
        // Pins the CRC32C itself, independently of the computation above.
        assert_eq!(crc, 0x5c53_1d71);
        assert_eq!(Superblock::decode(&mut Reader::new(&expected)), Ok(sb(7)));
    }

    #[test]
    fn slots() {
        assert_eq!((Slot::A.offset(), Slot::B.offset()), (4096, 8192));
        assert_eq!(FIRST_FRAME_OFFSET, 12288);
        assert_eq!((Slot::A.other(), Slot::B.other()), (Slot::B, Slot::A));
    }

    #[test]
    fn invalid_slots_fail_decode() {
        for (kind, at) in [
            (Invalid::NeverWritten, SLOT_LEN - 4),
            (Invalid::BitFlip, SLOT_LEN - 4),
            (Invalid::Reserved, 100),
        ] {
            let bytes = invalid(kind, 1);
            let mut r = Reader::new(&bytes);
            let Err(err) = Superblock::decode(&mut r) else {
                panic!("{kind:?} accepted")
            };
            assert_eq!(
                err.corruption().map(|(o, _)| o),
                Some(at as u64),
                "{kind:?}"
            );
            assert_eq!(r.position(), 0);
        }
    }

    /// SPEC §4.3 selection table, every row, with every way of being invalid.
    #[test]
    fn truth_table() {
        // Both valid: higher seq wins, whichever slot holds it, including at the u64 extremes.
        for (a, b, slot, seq) in [
            (1, 2, Slot::B, 2),
            (2, 1, Slot::A, 2),
            (0, u64::MAX, Slot::B, u64::MAX),
            (u64::MAX, 0, Slot::A, u64::MAX),
        ] {
            assert_eq!(
                read_active(&encode(&sb(a)), &encode(&sb(b))),
                active(slot, sb(seq))
            );
        }
        // Equal seq and contents (a freshly created container): A.
        assert_eq!(
            read_active(&encode(&sb(0)), &encode(&sb(0))),
            active(Slot::A, sb(0))
        );
        // Equal seq, but B differs only in a reserved byte (CRC fixed): B is invalid, not a conflict.
        assert_eq!(
            read_active(&encode(&sb(5)), &invalid(Invalid::Reserved, 5)),
            active(Slot::A, sb(5))
        );
        // Equal seq, different contents: corruption at slot B.
        let other = Superblock {
            fence_token: 10,
            ..sb(5)
        };
        assert_eq!(
            read_active(&encode(&sb(5)), &encode(&other)),
            Err(Error::Corrupt {
                offset: BASE + SLOT_LEN as u64,
                reason: Corruption::SuperblockConflict { seq: 5 },
            })
        );
        for kind in [Invalid::NeverWritten, Invalid::BitFlip, Invalid::Reserved] {
            // One invalid: the other wins even with a lower seq.
            assert_eq!(
                read_active(&encode(&sb(1)), &invalid(kind, 9)),
                active(Slot::A, sb(1)),
                "{kind:?}"
            );
            assert_eq!(
                read_active(&invalid(kind, 9), &encode(&sb(1))),
                active(Slot::B, sb(1)),
                "{kind:?}"
            );
            // Both invalid.
            for other in [Invalid::NeverWritten, Invalid::BitFlip, Invalid::Reserved] {
                assert_eq!(
                    read_active(&invalid(kind, 1), &invalid(other, 2)),
                    Err(Error::Corrupt {
                        offset: BASE,
                        reason: Corruption::NoValidSuperblock,
                    }),
                    "{kind:?} {other:?}"
                );
            }
        }
    }

    #[test]
    fn short_input_is_corrupt_even_with_a_valid_slot() {
        let bytes = [encode(&sb(1)), encode(&sb(2))].concat();
        for len in [0, SLOT_LEN, SLOTS_LEN - 1] {
            let Err(err) = read_active(&bytes[..len], &[]) else {
                panic!("accepted {len} bytes")
            };
            assert!(matches!(
                err.corruption(),
                Some((_, Corruption::UnexpectedEof { .. }))
            ));
        }
    }
}
