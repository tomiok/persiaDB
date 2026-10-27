//! Peak memory of `SegmentWriter::finish` (ROADMAP 5.1.6): what a flush allocates on top of its builder must stay
//! within the bound `SegmentWriter::finish` documents.
//!
//! A counting global allocator (ADR-0026) tracks the bytes the current thread holds and their peak. A reallocation
//! counts the old and the new block as live at once, as a copying allocator holds them.
#![allow(unsafe_code, clippy::unwrap_used)] // ADR-0026: the counting allocator; fixed inputs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ops::Bound;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::schema::{FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
use persia_engine::segment::{Segment, SegmentBuilder, SegmentMeta, SegmentWriter};
use persia_format::ByteSource;

thread_local! {
    /// Bytes this thread allocated and has not freed (a block freed by another thread makes it drift, which this
    /// single-threaded test does not do), and the largest value it took. `const`-initialized: never allocates.
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

/// The system allocator, tracking the calling thread's live bytes.
struct Counting;

// SAFETY: every method forwards to `System` with the caller's arguments, so `Counting` upholds `GlobalAlloc`'s
// contract exactly when `System` does; the counters are thread-local `Cell`s with `const` initializers, which never
// allocate or unwind. `try_with` ignores the thread's teardown, when the thread-locals are gone.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add(layout.size(), true);
        // SAFETY: the caller's `layout` is forwarded unchanged (non-zero size, per `GlobalAlloc::alloc`).
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        add(layout.size(), true);
        // SAFETY: as in `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        add(layout.size(), false);
        // SAFETY: `ptr` came from this allocator, that is from `System`, with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        add(new_size, true);
        add(layout.size(), false);
        // SAFETY: `ptr` came from `System` with `layout`; `new_size` is the caller's, valid per `GlobalAlloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn add(bytes: usize, grow: bool) {
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    let _ignored = LIVE.try_with(|live| {
        let now = if grow {
            live.get().saturating_add(bytes)
        } else {
            live.get().saturating_sub(bytes)
        };
        live.set(now);
        let _ignored = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

/// A deterministic word: a Zipf-like draw from 20 000 words.
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
        let mut text: Vec<String> = (0..60).map(|_| word(&mut state)).collect();
        // One token of its own per document: many terms, where the per-term cost shows.
        text.push(format!("u{i}"));
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
    let before = LIVE.with(Cell::get);
    PEAK.with(|p| p.set(before));
    let bytes = SegmentWriter::new(&b, meta).finish().unwrap();
    let extra = usize::try_from(PEAK.with(Cell::get) - before).unwrap();

    // The bound `SegmentWriter::finish` documents, for `body`, the field with the most postings: the output (three
    // times its length: a doubling buffer and its old copy), 4 bytes per posting and 12 per term grouped by term, its
    // norms and Skip entries, 8 bytes per document of sorted ids, and about 4 MiB of blocks, a zstd context and an
    // FST registry. The writer before ROADMAP 5.1.6 needs more.
    let seg = Segment::open(ByteSource::new(bytes.clone().into()), &schema).unwrap();
    let terms = seg.terms(body).unwrap();
    let (mut t, mut p) = (0_usize, 0_usize);
    let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
    while let Some(next) = stream.next() {
        t += 1;
        p += next.unwrap().1.df() as usize;
    }
    let n = docs as usize;
    let output = 3 * bytes.len();
    let field = 4 * p + 12 * t + n + 40 * p.div_ceil(128) + 8 * n;
    let bound = output + field + (4 << 20);
    eprintln!(
        "segment {} bytes, {p} postings, {t} terms, finish peak {extra} bytes over the builder, bound {bound}",
        bytes.len(),
    );
    assert!(extra <= bound, "{extra} > {bound}");
}
