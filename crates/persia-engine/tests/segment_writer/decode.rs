//! A test-only decoder of the segment layout (SPEC §5.1, §5.3), written from the SPEC rather than from the writer,
//! and strict: it asserts every layout rule it passes (alignment, order, padding, reserved bytes, offsets).

use std::collections::BTreeMap;

use persia_format::Reader;
use persia_format::bitpack::{BLOCK_LEN, BitWidth};
use persia_format::block::BlockReader;
use persia_format::checksum::crc32c;
use persia_format::for_codec::VALUES_PER_BLOCK;
use roaring::RoaringBitmap;

use super::model::{Index, Occurrence};

pub const SEGMENT_FIELD: u32 = u32::MAX;

#[derive(Debug)]
pub struct Footer {
    pub format: (u16, u16),
    pub segment_id: u128,
    pub schema_hash: u128,
    pub schema_encoding_version: u8,
    pub field_count: u32,
    pub doc_count: u32,
    pub merge_generation: u32,
    pub created_at: i64,
    /// field id -> (`docs_with_field`, `sum_field_len`)
    pub stats: BTreeMap<u32, (u32, u64)>,
    /// (kind, field) -> section bytes
    pub sections: BTreeMap<(u16, u32), Vec<u8>>,
    pub min_id: Vec<u8>,
    pub max_id: Vec<u8>,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Parses the footer and slices the sections, checking SPEC §5.1's layout rules.
pub fn parse(bytes: &[u8]) -> Footer {
    let n = bytes.len();
    assert_eq!(&bytes[n - 4..], b"PSEG", "magic");
    let footer_len = u32_at(bytes, n - 8) as usize;
    assert!(footer_len <= 1 << 20);
    let start = n - 8 - footer_len;
    assert_eq!(start % 8, 0, "footer alignment");
    let (body, crc) = bytes[start..n - 8].split_at(footer_len - 4);
    assert_eq!(crc32c(body), u32_at(crc, 0), "footer crc");

    let mut r = Reader::new(body);
    let format = (r.read_u16().unwrap(), r.read_u16().unwrap());
    assert_eq!(r.read_u32().unwrap(), 0, "reserved");
    assert_eq!(r.read_u64().unwrap(), 0, "incompat_flags");
    assert_eq!(r.read_u64().unwrap(), 0, "rocompat_flags");
    let segment_id = r.read_u128().unwrap();
    let schema_hash = r.read_u128().unwrap();
    let schema_encoding_version = r.read_u8().unwrap();
    r.expect_zeros(3).unwrap();
    let field_count = r.read_u32().unwrap();
    let doc_count = r.read_u32().unwrap();
    let merge_generation = r.read_u32().unwrap();
    let created_at = r.read_i64().unwrap();
    let stats_count = r.read_u32().unwrap();
    let section_count = r.read_u32().unwrap();
    assert_eq!(r.position(), 88, "fixed footer part");

    let mut stats = BTreeMap::new();
    let mut prev_field = None;
    for _ in 0..stats_count {
        let field = r.read_u32().unwrap();
        assert!(prev_field < Some(field), "stats order");
        prev_field = Some(field);
        stats.insert(field, (r.read_u32().unwrap(), r.read_u64().unwrap()));
    }

    let mut sections = BTreeMap::new();
    let mut prev_key = None;
    let mut end = 0_usize;
    for _ in 0..section_count {
        let kind = r.read_u16().unwrap();
        assert_eq!(r.read_u16().unwrap(), 0, "section reserved");
        let field = r.read_u32().unwrap();
        let offset = usize::try_from(r.read_u64().unwrap()).unwrap();
        let len = usize::try_from(r.read_u64().unwrap()).unwrap();
        assert!((1..=12).contains(&kind), "kind {kind}");
        let key = (field, kind);
        assert!(prev_key < Some(key), "section order {key:?}");
        prev_key = Some(key);
        assert_eq!(offset % 8, 0, "section alignment");
        assert!(offset >= end && offset - end < 8, "sections are packed");
        assert!(bytes[end..offset].iter().all(|&b| b == 0), "padding");
        end = offset + len;
        sections.insert((kind, field), bytes[offset..end].to_vec());
    }
    assert!(
        start >= end && start - end < 8,
        "footer follows the sections"
    );
    assert!(
        bytes[end..start].iter().all(|&b| b == 0),
        "padding before the footer"
    );

    let mut id = || {
        let len = usize::from(r.read_u16().unwrap());
        r.read_bytes(len).unwrap().to_vec()
    };
    let (min_id, max_id) = (id(), id());
    assert!(r.is_empty(), "footer has trailing bytes");
    Footer {
        format,
        segment_id,
        schema_hash,
        schema_encoding_version,
        field_count,
        doc_count,
        merge_generation,
        created_at,
        stats,
        sections,
        min_id,
        max_id,
    }
}

/// The logical stream of a block-framed section.
pub fn stream(section: &[u8]) -> Vec<u8> {
    let r = BlockReader::open(section, 0).unwrap();
    let mut out = Vec::new();
    r.read_range(0, r.len(), &mut out).unwrap();
    assert_eq!(out.len() as u64, r.len());
    out
}

/// The payload of a raw section, after checking its CRC trailer.
pub fn raw(section: &[u8]) -> &[u8] {
    let (payload, crc) = section.split_at(section.len() - 4);
    assert_eq!(crc32c(payload), u32_at(crc, 0), "raw section crc");
    payload
}

/// A record array (SPEC §5.3.6) of `n` records.
pub fn records(stream: &[u8], n: usize) -> Vec<Vec<u8>> {
    let mut r = Reader::new(stream);
    let ends: Vec<usize> = (0..n).map(|_| r.read_u64().unwrap() as usize).collect();
    let data = r.rest();
    assert_eq!(
        ends.last().copied().unwrap_or(0),
        data.len(),
        "records fill the stream"
    );
    let mut start = 0;
    ends.iter()
        .map(|&end| {
            let rec = data[start..end].to_vec();
            start = end;
            rec
        })
        .collect()
}

fn read_run(r: &mut Reader<'_>, n: usize) -> Vec<u32> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n / BLOCK_LEN {
        let width = r.read_bit_width().unwrap();
        let mut block = [0_u32; BLOCK_LEN];
        r.read_packed_block(width, &mut block).unwrap();
        // Writers use the minimal width.
        assert_eq!(BitWidth::of(&block), width);
        out.extend(block);
    }
    for _ in 0..n % BLOCK_LEN {
        out.push(r.read_varint_u32().unwrap());
    }
    out
}

/// What a field's term sections hold, as the model describes it, plus checks of TermInfo and Skip.
pub fn terms(f: &Footer, field: u32, norms: Option<&[u8]>, positions: bool) -> Index {
    let section = |kind: u16| f.sections.get(&(kind, field));
    let Some(dict) = section(1) else {
        assert!(
            (2..=7).all(|k| section(k).is_none()),
            "term sections without a TermDict"
        );
        return Index::default();
    };
    let dict = fst::Map::new(raw(dict).to_vec()).unwrap();
    let info = stream(section(2).unwrap());
    let postings = stream(section(3).unwrap());
    let freqs = stream(section(4).unwrap());
    let skip = section(5).map(|s| stream(s));
    let pos = section(6).map(|s| stream(s));
    assert_eq!(pos.is_some(), positions);
    assert_eq!(info.len(), dict.len() * 56);

    let mut index = Index::default();
    let mut any_skip = false;
    let mut stream = dict.stream();
    let mut ordinal = 0_u64;
    let (mut next_postings, mut next_freqs, mut next_pos, mut next_skip) = (0, 0, 0, 0);
    while let Some((term, value)) = fst::Streamer::next(&mut stream) {
        assert_eq!(value, ordinal, "FST value is the term's rank");
        let mut ri = Reader::new(&info[ordinal as usize * 56..][..56]);
        ordinal += 1;
        let (p_off, f_off, s_off, x_off) = (
            ri.read_u64().unwrap() as usize,
            ri.read_u64().unwrap() as usize,
            ri.read_u64().unwrap() as usize,
            ri.read_u64().unwrap() as usize,
        );
        let ttf = ri.read_u64().unwrap();
        let df = ri.read_u32().unwrap() as usize;
        let term_max_tf = ri.read_u32().unwrap();
        let term_min_norm = ri.read_u8().unwrap();
        ri.expect_zeros(7).unwrap();
        assert!(df >= 1);
        // Lists are laid out in term order, back to back.
        assert_eq!((p_off, f_off), (next_postings, next_freqs));
        assert_eq!(x_off, if positions { next_pos } else { 0 });

        let mut rp = Reader::new(&postings[p_off..]);
        let mut rf = Reader::new(&freqs[f_off..]);
        let mut rx = pos.as_ref().map(|p| Reader::new(&p[x_off..]));
        let mut prev = None;
        let mut occurrences = Vec::new();
        let mut blocks = Vec::new();
        let mut remaining = df;
        while remaining > 0 {
            let start = (
                p_off + rp.position(),
                f_off + rf.position(),
                rx.as_ref().map_or(0, |r| x_off + r.position()),
            );
            let mut docs = [0_u32; BLOCK_LEN];
            let n = if remaining >= BLOCK_LEN {
                let h = rp.read_postings_block(prev, &mut docs).unwrap();
                assert_eq!(h.last_docid, docs[BLOCK_LEN - 1]);
                BLOCK_LEN
            } else {
                let n = rp.read_postings_tail(prev, &mut docs).unwrap();
                assert_eq!(n, remaining);
                n
            };
            remaining -= n;
            let docs = &docs[..n];
            prev = docs.last().copied();
            let tfs: Vec<u32> = read_run(&mut rf, n).into_iter().map(|t| t + 1).collect();
            let mut block_positions = Vec::new();
            if let Some(rx) = &mut rx {
                let total: u32 = tfs.iter().sum();
                let deltas = read_run(rx, total as usize);
                let mut it = deltas.into_iter();
                for &tf in &tfs {
                    let mut p = 0;
                    let list: Vec<u32> = (0..tf)
                        .map(|_| {
                            p += it.next().unwrap();
                            p
                        })
                        .collect();
                    block_positions.push(list);
                }
            }
            let max_tf = *tfs.iter().max().unwrap();
            let min_norm = docs
                .iter()
                .map(|&d| norms.map_or(0, |n| n[d as usize]))
                .min()
                .unwrap();
            blocks.push((docs[n - 1], max_tf, start, min_norm));
            for (i, (&doc, &tf)) in docs.iter().zip(&tfs).enumerate() {
                let positions = block_positions.get(i).cloned().unwrap_or_default();
                occurrences.push(Occurrence { doc, tf, positions });
            }
        }
        next_postings = p_off + rp.position();
        next_freqs = f_off + rf.position();
        if let Some(rx) = &rx {
            next_pos = x_off + rx.position();
        }
        assert_eq!(
            ttf,
            occurrences.iter().map(|o| u64::from(o.tf)).sum::<u64>()
        );
        assert_eq!(
            term_max_tf,
            blocks.iter().map(|b| b.1).max().unwrap(),
            "term max_tf"
        );
        assert_eq!(
            term_min_norm,
            blocks.iter().map(|b| b.3).min().unwrap(),
            "term min_norm"
        );

        if df >= BLOCK_LEN {
            any_skip = true;
            let skip = skip.as_ref().expect("Skip section");
            assert_eq!(s_off, next_skip);
            let mut rs = Reader::new(&skip[s_off..]);
            for &(last, max_tf, (bp, bf, bx), min_norm) in &blocks {
                assert_eq!(rs.read_u32().unwrap(), last);
                assert_eq!(rs.read_u32().unwrap(), max_tf);
                assert_eq!(rs.read_u64().unwrap() as usize, bp);
                assert_eq!(rs.read_u64().unwrap() as usize, bf);
                assert_eq!(rs.read_u64().unwrap() as usize, bx);
                assert_eq!(rs.read_u8().unwrap(), min_norm);
                rs.expect_zeros(7).unwrap();
            }
            next_skip = s_off + rs.position();
        } else {
            assert_eq!(s_off, 0, "skip_off of a short list");
        }
        index.terms.insert(term.to_vec(), occurrences);
    }
    assert_eq!((next_postings, next_freqs), (postings.len(), freqs.len()));
    if let Some(p) = &pos {
        assert_eq!(next_pos, p.len());
    }
    assert_eq!(skip.is_some(), any_skip, "Skip present iff some df >= 128");
    if let Some(s) = &skip {
        assert_eq!(next_skip, s.len());
    }
    index
}

fn bitmap(r: &mut Reader<'_>) -> Option<RoaringBitmap> {
    let len = r.read_u64().unwrap() as usize;
    if len == 0 {
        return None;
    }
    Some(RoaringBitmap::deserialize_from(r.read_bytes(len).unwrap()).unwrap())
}

pub fn for_column(r: &mut Reader<'_>) -> Vec<u64> {
    let count = r.read_u64().unwrap() as usize;
    let blocks = count.div_ceil(VALUES_PER_BLOCK);
    let ends: Vec<usize> = (0..blocks)
        .map(|_| r.read_u64().unwrap() as usize)
        .collect();
    let data = r.read_bytes(ends.last().copied().unwrap_or(0)).unwrap();
    let mut values = Vec::new();
    let mut start = 0;
    for (i, &end) in ends.iter().enumerate() {
        let mut br = Reader::new(&data[start..end]);
        let block = br.read_for_block().unwrap();
        assert!(br.is_empty());
        let expected = if i + 1 < blocks {
            VALUES_PER_BLOCK
        } else {
            count - i * VALUES_PER_BLOCK
        };
        assert_eq!(block.len(), expected);
        values.extend(block.iter());
        start = end;
    }
    values
}

/// A numeric column: doc -> key.
pub fn numeric_column(stream: &[u8], doc_count: u32) -> BTreeMap<u32, u64> {
    let mut r = Reader::new(stream);
    assert_eq!(r.read_u8().unwrap(), 1);
    r.expect_zeros(7).unwrap();
    let presence = bitmap(&mut r);
    assert!(
        presence
            .as_ref()
            .and_then(RoaringBitmap::max)
            .is_none_or(|d| d < doc_count)
    );
    let keys = for_column(&mut r);
    assert!(r.is_empty());
    assert_eq!(keys.len(), doc_count as usize);
    (0..doc_count)
        .filter(|d| presence.as_ref().is_none_or(|p| p.contains(*d)))
        .map(|d| (d, keys[d as usize]))
        .collect()
}

/// A bool column: doc -> 0 | 1.
pub fn bool_column(stream: &[u8], doc_count: u32) -> BTreeMap<u32, u64> {
    let mut r = Reader::new(stream);
    assert_eq!(r.read_u8().unwrap(), 2);
    r.expect_zeros(7).unwrap();
    let presence = bitmap(&mut r);
    let trues = bitmap(&mut r).expect("true bitmap is always serialized");
    if let Some(p) = &presence {
        assert!(trues.is_subset(p), "true bits only for present documents");
    }
    assert!(
        trues.max().is_none_or(|d| d < doc_count),
        "true bits below doc_count"
    );
    assert!(r.is_empty());
    (0..doc_count)
        .filter(|d| presence.as_ref().is_none_or(|p| p.contains(*d)))
        .map(|d| (d, u64::from(trues.contains(d))))
        .collect()
}

/// A keyword column: doc -> its distinct values, ascending.
pub fn keyword_column(stream: &[u8], doc_count: u32) -> BTreeMap<u32, Vec<Vec<u8>>> {
    let mut r = Reader::new(stream);
    assert_eq!(r.read_u8().unwrap(), 3);
    r.expect_zeros(7).unwrap();
    let starts_off = r.read_u64().unwrap() as usize;
    let ords_off = r.read_u64().unwrap() as usize;
    let count = r.read_u64().unwrap() as usize;
    let ends: Vec<usize> = (0..count).map(|_| r.read_u64().unwrap() as usize).collect();
    let data = r.read_bytes(ends.last().copied().unwrap_or(0)).unwrap();
    let mut dict = Vec::new();
    let mut start = 0;
    for &end in &ends {
        dict.push(data[start..end].to_vec());
        start = end;
    }
    assert!(dict.windows(2).all(|w| w[0] < w[1]), "dictionary sorted");
    assert_eq!(r.position(), starts_off, "starts_off");
    let starts = for_column(&mut r);
    assert_eq!(r.position(), ords_off, "ords_off");
    let ords = for_column(&mut r);
    assert!(r.is_empty());
    assert_eq!(starts.len(), doc_count as usize + 1);
    assert_eq!(starts[0], 0);
    assert_eq!(*starts.last().unwrap() as usize, ords.len());
    let mut out = BTreeMap::new();
    for d in 0..doc_count as usize {
        let mine = &ords[starts[d] as usize..starts[d + 1] as usize];
        assert!(mine.windows(2).all(|w| w[0] < w[1]), "ords ascend");
        if !mine.is_empty() {
            out.insert(
                d as u32,
                mine.iter().map(|&o| dict[o as usize].clone()).collect(),
            );
        }
    }
    out
}
