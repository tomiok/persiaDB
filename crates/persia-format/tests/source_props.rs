//! Property tests for `ByteSource` and `MmapFile` (ROADMAP 1.8.1, 1.8.2): slicing agrees with plain `Vec` slicing
//! at any base offset, nested slices compose, and a mapping of any range holds exactly the file's bytes.

// Test-only helpers index and cast on purpose (see reader_writer_props.rs).
#![allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::unwrap_used
)]

use std::io::Write;

use persia_format::mmap::MmapFile;
use persia_format::{ByteSource, Error};
use proptest::prelude::*;

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7) as u8).collect()
}

proptest! {
    // Differential against slicing the same Vec: same bytes, same offsets, and nested slices agree.
    #[test]
    fn slice_matches_vec(
        len in 0usize..300,
        base in prop_oneof![0u64..10_000, (u64::MAX - 1000)..=u64::MAX - 320],
        a in 0usize..320, b in 0usize..320, c in 0usize..320, d in 0usize..320,
    ) {
        let data = data(len);
        let s = ByteSource::with_base_offset(data.clone().into(), base);
        let at = |rel: usize| base + rel as u64;
        let (start, end) = (at(a), at(b));
        match s.slice(start..end) {
            Ok(sub) => {
                prop_assert!(a <= b && b <= len);
                prop_assert_eq!(&*sub, &data[a..b]);
                prop_assert_eq!(sub.base_offset(), start);
                prop_assert_eq!(sub.end_offset(), end);
                let nested = sub.slice(at(c)..at(d));
                prop_assert_eq!(nested.is_ok(), a <= c && c <= d && d <= b);
                if let Ok(n) = nested {
                    prop_assert_eq!(&*n, &data[c..d]);
                }
            }
            Err(e) => {
                prop_assert!(a > b || b > len);
                let is_range = matches!(e, Error::RangeOutOfBounds { .. });
                prop_assert!(is_range);
            }
        }
    }

    // Any range of a file (unaligned, page-straddling, empty, or past the end) maps exactly its bytes or is
    // rejected, and a ByteSource over the mapping slices like the file.
    #[test]
    fn mapping_matches_file(len in 0usize..20_000, a in 0usize..20_100, b in 0usize..20_100) {
        let data = data(len);
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&data).unwrap();
        match MmapFile::map(&file, a as u64..b as u64) {
            Ok(map) => {
                prop_assert!(a <= b && b <= len);
                prop_assert_eq!(&*map, &data[a..b]);
                let s = ByteSource::from(map);
                let mid = a + (b - a) / 2;
                prop_assert_eq!(&*s.slice(mid as u64..b as u64).unwrap(), &data[mid..b]);
            }
            Err(e) => {
                prop_assert!(a > b || b > len);
                prop_assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
            }
        }
    }
}
