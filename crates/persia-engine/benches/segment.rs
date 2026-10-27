//! Indexing throughput: [`SegmentBuilder::add`] over synthetic documents, and [`SegmentWriter::finish`] (ROADMAP 5.1,
//! 5.2); reading the segment they build: open, term lookup, postings, skipping, positions, documents (ROADMAP 5.3);
//! opening a segment with a large term dictionary; the memtable: `put` and postings through a view (ROADMAP 6.1).
//! Deterministic input (a fixed-seed generator), so runs compare; baselines in `docs/benchmarks.md`.
//! Run: `cargo bench -p persia-engine --bench segment`.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;
use std::ops::Bound;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, Value};
use persia_engine::memtable::Memtable;
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::segment::{
    Segment, SegmentBuilder, SegmentMeta, SegmentWriter, TermInfo, WriteOptions,
};
use persia_engine::view::{PostingsIterator, SearchView};
use persia_format::ByteSource;

const DOCS: u64 = 2_000;
const VOCABULARY: u64 = 5_000;
const META: SegmentMeta = SegmentMeta {
    segment_id: 1,
    created_at: 0,
    merge_generation: 0,
};

fn schema() -> CollectionSchema {
    SchemaBuilder::new("bench")
        .field(
            "title",
            FieldKind::Text(TextOptions {
                positions: true,
                ..TextOptions::default()
            }),
        )
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("brand", FieldKind::Keyword(KeywordOptions::default()))
        .field("price", FieldKind::F64 { stored: true })
        .build()
        .unwrap()
}

/// xorshift64*: a fixed seed, so every run indexes the same documents.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A word with a skewed (roughly Zipfian) rank.
    fn word(&mut self) -> String {
        let a = self.next() % VOCABULARY + 1;
        let b = self.next() % a + 1;
        format!("w{b}")
    }
}

fn documents(schema: &CollectionSchema) -> Vec<Document> {
    let field = |name: &str| {
        schema
            .field(name)
            .map(persia_engine::schema::Field::id)
            .unwrap()
    };
    let (title, body, brand, price) = (
        field("title"),
        field("body"),
        field("brand"),
        field("price"),
    );
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    (0..DOCS)
        .map(|i| {
            let title_text: Vec<String> = (0..8).map(|_| rng.word()).collect();
            let body_text: Vec<String> = (0..120).map(|_| rng.word()).collect();
            #[allow(clippy::cast_precision_loss)]
            let p = (rng.next() % 100_000) as f64 / 100.0;
            Document::new(format!("doc-{i}"))
                .with(title, Value::String(title_text.join(" ")))
                .with(body, Value::String(body_text.join(" ")))
                .with(brand, Value::String(format!("brand-{}", rng.next() % 50)))
                .with(price, Value::F64(p))
        })
        .collect()
}

fn build(
    schema: &CollectionSchema,
    registry: &AnalyzerRegistry,
    docs: &[Document],
) -> SegmentBuilder {
    let mut b = SegmentBuilder::new(schema, registry).unwrap();
    for d in docs {
        b.add(d).unwrap();
    }
    b
}

fn bench(c: &mut Criterion) {
    let schema = schema();
    let registry = AnalyzerRegistry::builtin();
    let docs = documents(&schema);

    let mut group = c.benchmark_group("segment");
    group.throughput(Throughput::Elements(DOCS));
    group.bench_function("add", |b| {
        b.iter(|| black_box(build(&schema, &registry, &docs)));
    });
    let built = build(&schema, &registry, &docs);
    group.bench_function("finish", |b| {
        b.iter(|| black_box(SegmentWriter::new(&built, META).finish().unwrap()));
    });
    group.finish();
}

/// Terms of `field` with their `TermInfo`, in term order.
fn all_terms(seg: &Segment, field: FieldId) -> Vec<(Vec<u8>, TermInfo)> {
    let terms = seg.terms(field).unwrap();
    let mut stream = terms.range(Bound::Unbounded, Bound::Unbounded);
    let mut out = Vec::new();
    while let Some(next) = stream.next() {
        let (term, info) = next.unwrap();
        out.push((term.to_vec(), info));
    }
    out
}

fn total_df(terms: &[(Vec<u8>, TermInfo)]) -> u64 {
    terms.iter().map(|(_, i)| u64::from(i.df())).sum()
}

/// Reading the segment the indexing benchmarks build (ROADMAP 5.3): open, term lookup, postings iteration,
/// skipping, positions and document fetch.
fn bench_read(c: &mut Criterion) {
    let schema = schema();
    let registry = AnalyzerRegistry::builtin();
    let docs = documents(&schema);
    let built = build(&schema, &registry, &docs);
    let bytes = SegmentWriter::new(&built, META).finish().unwrap();
    let source = ByteSource::new(bytes.into());
    let seg = Segment::open(source.clone(), &schema).unwrap();
    let field = |name: &str| schema.field(name).unwrap().id();
    let (title, body) = (field("title"), field("body"));
    let body_terms = all_terms(&seg, body);
    let title_terms = all_terms(&seg, title);
    // The frequent terms (df >= 200 of 2 000): lists of two or more 128-blocks, so `advance` skips blocks.
    let frequent: Vec<TermInfo> = body_terms
        .iter()
        .filter(|(_, i)| i.df() >= 200)
        .map(|(_, i)| i.clone())
        .collect();
    assert!(frequent.len() >= 4, "too few long postings lists");

    let mut group = c.benchmark_group("segment_read");
    group.throughput(Throughput::Elements(1));
    group.bench_function("open", |b| {
        b.iter(|| black_box(Segment::open(source.clone(), &schema).unwrap()));
    });

    group.throughput(Throughput::Elements(body_terms.len() as u64));
    group.bench_function("term_get", |b| {
        let mut terms = seg.terms(body).unwrap();
        b.iter(|| {
            for (term, _) in &body_terms {
                black_box(terms.get(term).unwrap());
            }
        });
    });

    // Every posting of every body term, with its frequency.
    group.throughput(Throughput::Elements(total_df(&body_terms)));
    group.bench_function("postings_freq", |b| {
        let terms = seg.terms(body).unwrap();
        b.iter(|| {
            let mut sum = 0_u64;
            for (_, info) in &body_terms {
                let mut p = terms.postings(info).unwrap();
                while let Some(doc) = p.next().unwrap() {
                    sum += u64::from(doc) + u64::from(p.freq().unwrap());
                }
            }
            black_box(sum)
        });
    });

    // `advance` to every 64th docid on the frequent terms; a target past the current block skips by its `Skip` entries.
    let targets: Vec<u32> = (0..u32::try_from(DOCS).unwrap()).step_by(64).collect();
    group.throughput(Throughput::Elements(
        (frequent.len() * targets.len()) as u64,
    ));
    group.bench_function("advance", |b| {
        let terms = seg.terms(body).unwrap();
        b.iter(|| {
            let mut sum = 0_u64;
            for info in &frequent {
                let mut p = terms.postings(info).unwrap();
                for &t in &targets {
                    match p.advance(t).unwrap() {
                        Some(doc) => sum += u64::from(doc),
                        None => break,
                    }
                }
            }
            black_box(sum)
        });
    });

    // Every posting of every title term, with its positions.
    group.throughput(Throughput::Elements(total_df(&title_terms)));
    group.bench_function("positions", |b| {
        let terms = seg.terms(title).unwrap();
        b.iter(|| {
            let mut sum = 0_u64;
            for (_, info) in &title_terms {
                let mut p = terms.postings(info).unwrap();
                while p.next().unwrap().is_some() {
                    sum += p
                        .positions()
                        .unwrap()
                        .iter()
                        .map(|&x| u64::from(x))
                        .sum::<u64>();
                }
            }
            black_box(sum)
        });
    });

    group.throughput(Throughput::Elements(DOCS));
    group.bench_function("document", |b| {
        b.iter(|| {
            for d in 0..u32::try_from(DOCS).unwrap() {
                black_box(seg.document(d).unwrap());
            }
        });
    });
    group.finish();
}

/// Distinct keyword values in the large-dictionary segment.
const LARGE_TERMS: u64 = 200_000;

/// `Segment::open` of a segment whose cost is its two FSTs: one `keyword` field with a distinct value per document, so a
/// `TermDict` of 200 000 terms and an `IdMap` of as many ids (every segment has one, SPEC §5.3.6). Open
/// checks each node by node before using it (`fst_check`, SPEC §5.3 "Framing"), then walks it once for its values.
fn bench_open_large_dict(c: &mut Criterion) {
    let schema = SchemaBuilder::new("dict")
        .field("sku", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let sku = schema.field("sku").unwrap().id();
    let registry = AnalyzerRegistry::builtin();
    let mut rng = Rng(0x5EED_D1C7);
    let docs: Vec<Document> = (0..LARGE_TERMS)
        .map(|i| {
            let value = format!("{}-{}-{i}", rng.word(), rng.word());
            Document::new(format!("d{i}")).with(sku, Value::String(value))
        })
        .collect();
    let built = build(&schema, &registry, &docs);
    let source = ByteSource::new(SegmentWriter::new(&built, META).finish().unwrap().into());
    let seg = Segment::open(source.clone(), &schema).unwrap();
    assert_eq!(seg.terms(sku).unwrap().len(), LARGE_TERMS);

    let mut group = c.benchmark_group("segment_open");
    group.throughput(Throughput::Elements(LARGE_TERMS));
    group.bench_function("large_dict", |b| {
        b.iter(|| black_box(Segment::open(source.clone(), &schema).unwrap()));
    });
    group.finish();
}
/// Documents in [`bench_records_over_many_blocks`]: at 1 KiB blocks, record arrays of thousands of blocks.
const MANY_BLOCK_DOCS: u32 = 50_000;
/// Every 50th document: 1000 lookups spread over the whole section.
const STEP: usize = 50;

/// Record lookups in sections of many blocks (ROADMAP 5.3.10): each `id_of` reads one record, so its cost must not
/// grow with the section's block count (it did while every lookup re-validated the whole block table).
fn bench_records_over_many_blocks(c: &mut Criterion) {
    let schema = SchemaBuilder::new("ids")
        .field("sku", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap();
    let sku = schema.field("sku").unwrap().id();
    let registry = AnalyzerRegistry::builtin();
    let docs: Vec<Document> = (0..MANY_BLOCK_DOCS)
        .map(|i| {
            Document::new(format!("doc-{i:08}")).with(sku, Value::String(format!("s{}", i % 97)))
        })
        .collect();
    let built = build(&schema, &registry, &docs);
    let options = WriteOptions {
        block_size: 1024,
        ..WriteOptions::default()
    };
    let bytes = SegmentWriter::new(&built, META)
        .with_options(options)
        .finish()
        .unwrap();
    let seg = Segment::open(ByteSource::new(bytes.into()), &schema).unwrap();

    let mut group = c.benchmark_group("segment_records");
    group.throughput(Throughput::Elements(1000));
    group.bench_function("id_of_1000", |b| {
        b.iter(|| {
            for d in (0..MANY_BLOCK_DOCS).step_by(STEP) {
                black_box(seg.id_of(d).unwrap());
            }
        });
    });
    group.finish();
}

/// Documents of the column benchmark: f64 keys span the whole key range, so FOR blocks are wide (64 bits) and many
/// cross a framed block.
const COLUMN_DOCS: u32 = 50_000;

/// Every document's numeric key, then its keyword ords, read through the lazy column readers (ROADMAP 5.3.12), by
/// docid and through a cursor (ROADMAP 5.3.16).
fn bench_columns(c: &mut Criterion) {
    let schema = schema();
    let price = schema.field("price").unwrap().id();
    let brand = schema.field("brand").unwrap().id();
    let registry = AnalyzerRegistry::builtin();
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let docs: Vec<Document> = (0..COLUMN_DOCS)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let p = (rng.next() % 1_000_000_007) as f64 / 7.0 - 7e7;
            Document::new(format!("doc-{i:08}"))
                .with(price, Value::F64(p))
                .with(brand, Value::String(format!("brand-{}", rng.next() % 500)))
        })
        .collect();
    let built = build(&schema, &registry, &docs);
    let bytes = SegmentWriter::new(&built, META).finish().unwrap();
    let seg = Segment::open(ByteSource::new(bytes.into()), &schema).unwrap();

    let mut group = c.benchmark_group("segment_columns");
    group.throughput(Throughput::Elements(u64::from(COLUMN_DOCS)));
    group.bench_function("numeric_key_scan", |b| {
        let col = seg.numeric_column(price).unwrap();
        b.iter(|| {
            for d in 0..COLUMN_DOCS {
                black_box(col.key(d).unwrap());
            }
        });
    });
    group.bench_function("keyword_ords_scan", |b| {
        let col = seg.keyword_column(brand).unwrap();
        b.iter(|| {
            for d in 0..COLUMN_DOCS {
                black_box(col.ords(d).unwrap());
            }
        });
    });
    group.bench_function("numeric_key_cursor", |b| {
        let col = seg.numeric_column(price).unwrap();
        b.iter(|| {
            let mut cursor = col.cursor();
            for d in 0..COLUMN_DOCS {
                black_box(cursor.key(d).unwrap());
            }
        });
    });
    group.bench_function("keyword_ords_cursor", |b| {
        let col = seg.keyword_column(brand).unwrap();
        b.iter(|| {
            let mut cursor = col.cursor();
            for d in 0..COLUMN_DOCS {
                black_box(cursor.ords(d).unwrap());
            }
        });
    });
    group.finish();
}
/// Every document's `body` norm, as scoring reads them (ROADMAP 5.3.14, 5.3.16).
fn bench_norms(c: &mut Criterion) {
    let schema = schema();
    let registry = AnalyzerRegistry::builtin();
    let built = build(&schema, &registry, &documents(&schema));
    let bytes = SegmentWriter::new(&built, META).finish().unwrap();
    let seg = Segment::open(ByteSource::new(bytes.into()), &schema).unwrap();
    let body = schema.field("body").unwrap().id();
    let n = seg.doc_count();
    let mut group = c.benchmark_group("segment_read");
    group.throughput(Throughput::Elements(u64::from(n)));
    group.bench_function("norms_get", |b| {
        let norms = seg.norms(body).unwrap();
        b.iter(|| {
            for d in 0..n {
                black_box(norms.get(d).unwrap());
            }
        });
    });
    group.bench_function("norms_cursor", |b| {
        let norms = seg.norms(body).unwrap();
        b.iter(|| {
            let mut cursor = norms.cursor();
            for d in 0..n {
                black_box(cursor.get(d).unwrap());
            }
        });
    });
    group.finish();
}

/// The memtable over the same documents (ROADMAP 6.1): `put` for all of them (as `segment/add`, plus the per-term
/// chains and the full document encoding), and every `body` term's postings read through a view (copied out under
/// the read lock, then iterated with frequencies).
fn bench_memtable(c: &mut Criterion) {
    let schema = schema();
    let registry = AnalyzerRegistry::builtin();
    let docs = documents(&schema);
    let fill = || {
        let mut mem = Memtable::new(&schema, &registry).unwrap();
        for d in &docs {
            mem.put(d).unwrap();
        }
        mem
    };

    let mut group = c.benchmark_group("memtable");
    group.throughput(Throughput::Elements(DOCS));
    group.bench_function("put", |b| b.iter(|| black_box(fill())));

    let seg = Segment::open(
        ByteSource::new(
            SegmentWriter::new(&build(&schema, &registry, &docs), META)
                .finish()
                .unwrap()
                .into(),
        ),
        &schema,
    )
    .unwrap();
    let body = schema.field("body").unwrap().id();
    let terms = all_terms(&seg, body);
    let view = fill().view();
    group.throughput(Throughput::Elements(total_df(&terms)));
    group.bench_function("postings_freq", |b| {
        b.iter(|| {
            let mut sum = 0_u64;
            for (term, _) in &terms {
                let mut p = view.postings(body, term).unwrap().unwrap();
                while p.next().unwrap().is_some() {
                    sum += u64::from(p.freq().unwrap());
                }
            }
            black_box(sum)
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench,
    bench_read,
    bench_norms,
    bench_open_large_dict,
    bench_records_over_many_blocks,
    bench_columns,
    bench_memtable
);
criterion_main!(benches);
