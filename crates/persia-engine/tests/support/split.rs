//! Re-laying out a segment with one block-framed section's stream replaced, every CRC valid (SPEC §5.1): damage
//! that only the semantic checks of `open` and `verify` can find. Shared by the merge tests.

#![allow(dead_code, clippy::cast_possible_truncation)]

use persia_engine::segment::SectionKind;
use persia_format::block::{BlockReader, BlockWriter, Compression};
use persia_format::checksum::crc32c;

pub fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

pub fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Offset of section-table entry `i` in the footer body (SPEC §5.1).
pub fn table_entry(body: &[u8], i: usize) -> usize {
    88 + 16 * le_u32(body, 80) as usize + 24 * i
}

/// A valid segment split as the segment fuzz target's mode 1 does: its footer body, its sections in table order, and
/// the decoded stream and block size of each block-framed one.
pub struct Split {
    pub body: Vec<u8>,
    pub sections: Vec<Vec<u8>>,
    pub framed: Vec<(usize, SectionKind, Vec<u8>, u32)>,
}

impl Split {
    pub fn new(bytes: &[u8]) -> Self {
        let n = bytes.len();
        let footer_len = le_u32(bytes, n - 8) as usize;
        let body = bytes[n - 8 - footer_len..n - 12].to_vec();
        let mut sections = Vec::new();
        let mut framed = Vec::new();
        for i in 0..le_u32(&body, 84) as usize {
            let e = table_entry(&body, i);
            let kind = SectionKind::from_code(u16::from_le_bytes([body[e], body[e + 1]])).unwrap();
            let off = le_u64(&body, e + 8) as usize;
            let len = le_u64(&body, e + 16) as usize;
            let section = bytes[off..off + len].to_vec();
            if kind.is_block_framed() {
                let r = BlockReader::open(&section, 0).unwrap();
                let mut stream = Vec::new();
                r.read_range(0, r.len(), &mut stream).unwrap();
                framed.push((i, kind, stream, r.block_size()));
            }
            sections.push(section);
        }
        Self {
            body,
            sections,
            framed,
        }
    }

    /// The segment with block-framed section `which`'s stream replaced by `stream`, re-framed and laid out again
    /// with a valid footer: every CRC holds (SPEC §5.1), only the semantic checks stand between it and the reader.
    pub fn with_stream(&self, which: usize, stream: &[u8]) -> Vec<u8> {
        let (index, _, _, block_size) = self.framed[which];
        let mut sections = self.sections.clone();
        let mut w = BlockWriter::new(block_size, Compression::None).unwrap();
        w.write(stream).unwrap();
        sections[index] = w.finish().unwrap();
        let pad = |out: &mut Vec<u8>| out.resize(out.len().next_multiple_of(8), 0);
        let mut body = self.body.clone();
        let mut out = Vec::new();
        for (i, s) in sections.iter().enumerate() {
            pad(&mut out);
            let e = table_entry(&body, i);
            body[e + 8..e + 16].copy_from_slice(&(out.len() as u64).to_le_bytes());
            body[e + 16..e + 24].copy_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(s);
        }
        pad(&mut out);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32c(&body).to_le_bytes());
        out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
        out.extend_from_slice(b"PSEG");
        out
    }
}
