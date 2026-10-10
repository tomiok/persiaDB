//! Fuzz `Checksummed<T>` decoding (ROADMAP 1.3.3): arbitrary bytes never panic, a wrong CRC is reported
//! exactly (offset, stored, computed) as the definition says, a failed decode moves nothing, and an accepted
//! region re-encodes to the same bytes (the CRC is a function of the rest).
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::checksummed::{Checksummed, ChecksummedLayout};
use persia_format::checksum::Crc32c;
use persia_format::{Corruption, Error, Reader, Result, Writer};

/// A miniature header: `magic u32, crc u32, seq u64, 4 reserved, flags u32` (CRC in the middle, SPEC §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mini {
    magic: u32,
    seq: u64,
    flags: u32,
}

impl ChecksummedLayout for Mini {
    const LEN: usize = 24;
    const CRC_OFFSET: usize = 4;
    fn encode_fields(&self, w: &mut Writer) {
        w.write_u32(self.magic);
        w.write_u32(0);
        w.write_u64(self.seq);
        w.write_zeros(4);
        w.write_u32(self.flags);
    }
    fn decode_fields(r: &mut Reader<'_>) -> Result<Self> {
        let magic = r.read_u32()?;
        r.skip(4)?;
        let seq = r.read_u64()?;
        r.expect_zeros(4)?;
        Ok(Self { magic, seq, flags: r.read_u32()? })
    }
}

/// The CRC verdict from the definition (SPEC §4.2): CRC32C over the region minus its own field.
fn reference_mismatch(region: &[u8]) -> Option<(u32, u32)> {
    let crc_end = Mini::CRC_OFFSET + 4;
    let stored = u32::from_le_bytes(region[Mini::CRC_OFFSET..crc_end].try_into().ok()?);
    let mut crc = Crc32c::new();
    crc.update(&region[..Mini::CRC_OFFSET]);
    crc.update(&region[crc_end..]);
    let computed = crc.finish();
    (stored != computed).then_some((stored, computed))
}

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);
    while !r.is_empty() {
        let start = r.position();
        let region = data.get(start..start + Mini::LEN);
        let result = Checksummed::<Mini>::decode(&mut r);
        // A wrong CRC is always reported, exactly, and nothing else is reported instead (SPEC §4.1).
        if let Some((stored, computed)) = region.and_then(reference_mismatch) {
            let expected = Error::Corrupt {
                offset: (start + Mini::CRC_OFFSET) as u64,
                reason: Corruption::ChecksumMismatch { stored, computed },
            };
            assert_eq!(result.as_ref().err(), Some(&expected), "CRC mismatch not reported exactly");
        }
        let Ok(Checksummed(value)) = result else {
            assert_eq!(r.position(), start, "a failed decode must not move the cursor");
            break;
        };
        assert_eq!(r.position() - start, Mini::LEN);
        let mut w = Writer::new();
        Checksummed(value).encode(&mut w).expect("decoded value re-encodes");
        assert_eq!(w.as_slice(), &data[start..r.position()], "accepted region re-encodes differently");
    }
});
