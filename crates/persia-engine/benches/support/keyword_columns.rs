//! The database of the `keyword_columns` bench and the step of its write group, shared with
//! `tests/keyword_columns_bench.rs`, which checks the write group's requests read every write.

use std::sync::Arc;

use persia_engine::db::{
    Database, DatabaseOptions, Execution, ManualClock, SearchRequest, SearchResults, SegmentIds,
    Snapshot,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{Query, TermQuery};
use persia_engine::schema::{FieldId, FieldKind, KeywordOptions, SchemaBuilder, TextOptions};
use persia_engine::search::{Order, Projection, SearchPlan, Sort, SortKey};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

/// How many documents the database holds: `segments` committed segments of `per_segment` documents, then
/// `in_memtable` in the memtable.
pub struct Shape {
    pub segments: u64,
    pub per_segment: u64,
    pub in_memtable: u64,
}

impl Shape {
    /// The id of the memtable document write `n` upserts.
    pub fn upserted(&self, n: u64) -> String {
        format!(
            "d{}",
            self.segments * self.per_segment + n % self.in_memtable
        )
    }
}

/// The `body`, `price` and `sku` fields.
#[derive(Clone, Copy)]
pub struct Fields {
    pub body: FieldId,
    pub price: FieldId,
    pub sku: FieldId,
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
}

/// A database of `shape`, each document with its own random `sku` and a `price`.
pub fn database(shape: &Shape) -> (Database<SimFile>, Fields) {
    let schema = SchemaBuilder::new("bench")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("price", FieldKind::I64 { stored: false })
        .field(
            "sku",
            FieldKind::Keyword(KeywordOptions {
                facet: true,
                ..KeywordOptions::default()
            }),
        )
        .build()
        .unwrap();
    let id = |name| schema.field(name).unwrap().id();
    let fields = Fields {
        body: id("body"),
        price: id("price"),
        sku: id("sku"),
    };
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [1; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let committed = shape.segments * shape.per_segment;
    for i in 0..committed + shape.in_memtable {
        let words: Vec<String> = (0..8).map(|_| format!("w{}", rng.next() % 20)).collect();
        let doc = Document::new(format!("d{i}"))
            .with(fields.body, Value::String(words.join(" ")))
            .with(
                fields.price,
                Value::I64(i64::try_from(rng.next() % 1000).unwrap()),
            )
            .with(
                fields.sku,
                Value::String(format!("sku-{:016x}", rng.next())),
            );
        db.put("bench", &doc, Durability::Buffered).unwrap();
        if (i + 1) % shape.per_segment == 0 && i < committed {
            db.commit().unwrap();
        }
    }
    (db, fields)
}

/// Write `n` (from 1): upserts memtable document [`Shape::upserted`], a `w3` hit whose `sku` (`sku-{n:016x}`) sorts
/// before the generated ones. Returns the snapshot after it, the one the next request reads: one taken before the
/// write does not see it, nor any flush since.
pub fn upsert(db: &mut Database<SimFile>, fields: Fields, shape: &Shape, n: u64) -> Arc<Snapshot> {
    let doc = Document::new(shape.upserted(n))
        .with(fields.body, Value::String(format!("w3 w{}", n % 20)))
        .with(fields.price, Value::I64(i64::try_from(n % 1000).unwrap()))
        .with(fields.sku, Value::String(format!("sku-{n:016x}")));
    db.put("bench", &doc, Durability::Buffered).unwrap();
    db.snapshot()
}

/// The write group's request: 10 hits of `w3` sorted by `sku`.
pub struct SortSku {
    query: Query,
    request: SearchRequest,
}

impl SortSku {
    pub fn new(snapshot: &Snapshot, fields: Fields) -> Self {
        let schema = snapshot.collection("bench").unwrap().schema();
        Self {
            query: Query::Term(TermQuery {
                field: fields.body,
                term: "w3".into(),
            }),
            request: SearchRequest {
                sort: Sort::new([SortKey::field(fields.sku, Order::Asc)], schema).unwrap(),
                limit: 10,
                projection: Projection::None,
                ..SearchRequest::default()
            },
        }
    }

    /// The request on `snapshot`, from a fresh memtable view, as a request runs.
    pub fn run(&self, snapshot: &Snapshot, exec: &Execution) -> SearchResults {
        let coll = snapshot.collection("bench").unwrap();
        let schema = coll.schema();
        let memtable = coll.memtable();
        let stats = coll.search_statistics(&memtable);
        let plan = SearchPlan::new(&self.query, schema, &stats).unwrap();
        let views = coll.views(&memtable).unwrap();
        views.search_with(&plan, &self.request, exec).unwrap()
    }
}
