//! Peak memory of `SegmentWriter::finish` (ROADMAP 5.1.6): what a flush adds to the resident set on top of its builder
//! must stay within the bound `SegmentWriter::finish` documents. Linux only: it resets and reads the peak resident
//! set (`VmHWM`) through `/proc/self`. One test per binary, so no other test allocates meanwhile.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
use std::ops::Bound;

use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
use persia_format::ByteSource;

/// A field of `/proc/self/status`, in bytes.
fn status(field: &str) -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with(field)).unwrap();
    let kb: usize = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    kb * 1024
}

/// A deterministic word: a Zipf-like draw from 20 000 words, a few long ones.
fn word(state: &mut u64) -> String {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let r = *state >> 33;
    let a = r % 20_000 + 1;
    format!("w{}", r % a)
}

#[test]
fn a_flush_stays_within_its_documented_bound() {
    let schema = SchemaBuilder::new("c")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .field("n", FieldKind::I64 { stored: false })
        .build()
        .unwrap();
    let id = |name| schema.field(name).unwrap().id();
    let (body, tag, n) = (id("body"), id("tag"), id("n"));
    let mut b = SegmentBuilder::new(&schema, &AnalyzerRegistry::builtin()).unwrap();
    let mut state = 7_u64;
    let docs = 20_000_u32;
    for i in 0..docs {
        let text: Vec<String> = (0..60).map(|_| word(&mut state)).collect();
        let doc = Document::new(format!("doc-{i:06}"))
            .with(body, Value::String(text.join(" ")))
            .with(tag, Value::String(format!("t{}", i % 300)))
            .with(n, Value::I64(i64::from(i) * 7));
        b.add(&doc).unwrap();
    }

    let meta = SegmentMeta {
        segment_id: 1,
        created_at: 0,
        merge_generation: 0,
    };
    // "5" resets the peak to the current resident set.
    std::fs::write("/proc/self/clear_refs", "5").unwrap();
    let before = status("VmRSS:");
    let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
    let extra = status("VmHWM:").saturating_sub(before);

    // The bound `SegmentWriter::finish` documents, for `body`, the largest field: the output (up to twice its length
    // while it grows), 4 bytes per posting and 20 per term grouped by term, its norms and Skip entries, 8 bytes per
    // document of sorted ids, and about 4 MiB of blocks and a zstd context. The writer before ROADMAP 5.1.6 peaked
    // at 28.6 MB here, above this bound; this one at 10.3 MB.
    let seg = Segment::open(ByteSource::new(bytes.clone().into()), &schema).unwrap();
    let terms = seg.terms(body).unwrap();
    let (mut t, mut p) = (0_usize, 0_usize);
    let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
    while let Some(next) = stream.next() {
        t += 1;
        p += next.unwrap().1.df() as usize;
    }
    let n = docs as usize;
    let output = 2 * bytes.len();
    let field = 4 * p + 20 * t + n + 40 * p.div_ceil(128) + 8 * n;
    let bound = output + field + (4 << 20);
    eprintln!(
        "segment {} bytes, builder {} bytes, finish peak {extra} bytes over the builder, bound {bound}",
        bytes.len(),
        b.size_in_bytes()
    );
    assert!(extra <= bound, "{extra} > {bound}");
}
