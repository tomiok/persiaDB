//! Fuzz `Checksummed<T>` decoding (ROADMAP 1.3.3): arbitrary bytes never panic, a failed decode moves
//! nothing, and an accepted region re-encodes to the same bytes (the CRC is a function of the rest).
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_format::checksummed::{Checksummed, ChecksummedLayout};
use persia_format::{Reader, Result, Writer};

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

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);
    while !r.is_empty() {
        let start = r.position();
        let Ok(Checksummed(value)) = Checksummed::<Mini>::decode(&mut r) else {
            assert_eq!(r.position(), start, "a failed decode must not move the cursor");
            break;
        };
        assert_eq!(r.position() - start, Mini::LEN);
        let mut w = Writer::new();
        Checksummed(value).encode(&mut w).expect("decoded value re-encodes");
        assert_eq!(w.as_slice(), &data[start..r.position()], "accepted region re-encodes differently");
    }
});
