//! The response JSON of the embedded API on a page of large documents (ROADMAP 10.1.31; SPEC §6.4.1, §7.5.2):
//! `SearchResponse::to_json` of a page of 20 hits, each a document of about 64 KiB (a long `text` field with
//! characters JSON escapes, a `keyword`, numbers, a `datetime` and a `stored` object), and `Document::to_json` of one
//! such document. Only the writing is measured: the search runs once, outside the loop. Deterministic input (a
//! fixed-seed generator), so runs compare. Run: `cargo bench -p persia --bench response_json`.

#![allow(missing_docs, clippy::unwrap_used, clippy::indexing_slicing)] // A bench: inputs are fixed.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use persia::{Db, Document, SearchRequest};

/// Hits on the page.
const PAGE: usize = 20;
/// Bytes of each document's `body`, about.
const BODY_LEN: usize = 64 << 10;

/// xorshift64*: a fixed seed, so every run writes the same documents.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// About `len` bytes of words, one in 16 needing an escape or more than one UTF-8 byte (`"quoted"`, a newline,
/// `zapatería`), and the word `common` that every document holds.
fn body(rng: &mut Rng, len: usize) -> String {
    const WORDS: [&str; 16] = [
        "red",
        "running",
        "shoes",
        "trail",
        "blue",
        "green",
        "boots",
        "hat",
        "socks",
        "sandals",
        "leather",
        "canvas",
        "light",
        "heavy",
        "\"quoted\"",
        "zapatería\n",
    ];
    let mut text = String::from("common");
    while text.len() < len {
        text.push(' ');
        let i = usize::try_from(rng.next() % 16).unwrap();
        text.push_str(WORDS[i]);
    }
    text
}

/// A database with `PAGE` large documents in collection `docs`, and the collection's handle.
fn database(dir: &std::path::Path) -> (Db, persia::Collection) {
    let db = Db::open(dir.join("bench.persia")).unwrap();
    let docs = db
        .create_collection_json(
            r#"{"name": "docs", "fields": {
                "body": {"type": "text"},
                "brand": {"type": "keyword"},
                "price": {"type": "f64"},
                "stock": {"type": "i64"},
                "at": {"type": "datetime"},
                "meta": {"type": "stored"}
            }}"#,
        )
        .unwrap();
    let mut rng = Rng(0x1013_1000_0001);
    for i in 0..PAGE {
        let doc = serde_json::json!({
            "id": format!("doc-{i:03}"),
            "body": body(&mut rng, BODY_LEN),
            "brand": format!("brand-{}", rng.next() % 50),
            "price": f64::from(u32::try_from(rng.next() % 100_000).unwrap()) / 100.0,
            "stock": i64::try_from(rng.next() % 1000).unwrap() - 500,
            "at": "2023-11-14T22:13:20.123456Z",
            "meta": {"tags": ["a", "b", "c"], "rating": 4.5, "nested": {"k": [1, 2, 3]}},
        });
        docs.put_json(&doc.to_string()).unwrap();
    }
    docs.commit().unwrap();
    (db, docs)
}

fn bench(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let (_db, docs) = database(dir.path());
    let response = docs
        .search(
            &SearchRequest::new("common")
                .limit(PAGE)
                .relax(persia::RelaxOptions::disabled()),
        )
        .unwrap();
    assert_eq!(response.hits.len(), PAGE, "every document is on the page");
    let text = response.to_json().unwrap();
    let mut group = c.benchmark_group("response_json");
    group.throughput(Throughput::Bytes(u64::try_from(text.len()).unwrap()));
    group.bench_function("search_response_to_json_20x64KiB", |b| {
        b.iter(|| black_box(response.to_json().unwrap()));
    });
    let schema = docs.schema().unwrap();
    let document: &Document = &response.hits[0].document;
    let one = document.to_json(&schema).unwrap();
    assert!(
        one.len() > BODY_LEN,
        "the page holds the large documents: {} bytes",
        one.len()
    );
    group.throughput(Throughput::Bytes(u64::try_from(one.len()).unwrap()));
    group.bench_function("document_to_json_64KiB", |b| {
        b.iter(|| black_box(document.to_json(&schema).unwrap()));
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
