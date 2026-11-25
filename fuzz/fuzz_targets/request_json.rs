//! Fuzz the request JSON (ROADMAP 10.1.10; SPEC §7.1, ADR-0062): no input panics `QueryRequest::from_json`, an
//! accepted request holds only parts its own parsers accept, and running it on a small collection returns, with hits
//! or an error, never a panic.
#![no_main]

use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use persia_engine::db::{
    Cursor, Database, DatabaseOptions, ManualClock, SegmentIds, Snapshot, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::QueryLimits;
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::search::request::{QueryRequest, RunOptions};
use persia_engine::search::{MAX_FACETS, MAX_SORT_KEYS, Projection};
use persia_engine::wal::Durability;
use persia_storage::container::sim::SimFile;

fn schema() -> CollectionSchema {
    let facet = KeywordOptions {
        facet: true,
        ..KeywordOptions::default()
    };
    SchemaBuilder::new("c")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("brand", FieldKind::Keyword(facet))
        .field("price", FieldKind::F64 { stored: true })
        .field("stock", FieldKind::I64 { stored: true })
        .field("active", FieldKind::Bool { stored: true })
        .field("at", FieldKind::Datetime { stored: true })
        .field("notes", FieldKind::Stored)
        .build()
        .expect("a valid schema")
}

/// A snapshot of a few documents, half in a segment and half in the memtable, built once.
fn snapshot() -> &'static Arc<Snapshot> {
    static SNAPSHOT: OnceLock<Arc<Snapshot>> = OnceLock::new();
    SNAPSHOT.get_or_init(|| {
        let schema = schema();
        let field = |name: &str| schema.field(name).expect("a field of the schema").id();
        let (title, brand, price) = (field("title"), field("brand"), field("price"));
        let options = DatabaseOptions {
            clock: Arc::new(ManualClock::new(0)),
            segment_ids: SegmentIds::seeded(1),
            ..DatabaseOptions::default()
        };
        let mut db = Database::create_on(SimFile::new(), [1; 16], options).expect("a new database");
        db.create_collection(schema).expect("a new collection");
        let docs = [
            ("a", "red running shoes", "nike", 80.0),
            ("b", "red trail shoes", "acme", 120.0),
            ("c", "blue running shoes", "nike", 60.0),
            ("d", "red hat", "acme", 20.0),
        ];
        for (i, (id, text, by, cost)) in docs.into_iter().enumerate() {
            let doc = Document::new(id)
                .with(title, Value::String(text.into()))
                .with(brand, Value::String(by.into()))
                .with(price, Value::F64(cost));
            db.put("c", &doc, Durability::Buffered)
                .expect("a valid document");
            if i == 1 {
                db.commit().expect("a commit");
            }
        }
        db.snapshot()
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(json) = std::str::from_utf8(data) else {
        return;
    };
    let snapshot = snapshot();
    let Some(collection) = snapshot.collection("c") else {
        return;
    };
    let limits = QueryLimits::default();
    let Ok(request) = QueryRequest::from_json(json, collection.schema(), &limits) else {
        return;
    };
    for filter in [request.filter(), request.soft_filter()]
        .into_iter()
        .flatten()
    {
        assert_eq!(filter.validate(collection.schema(), &limits), Ok(()));
    }
    assert_eq!(request.relax().validate(), Ok(()));
    let page = request.page();
    assert!(page.facets.len() <= MAX_FACETS);
    assert!(page.sort.keys().len() <= MAX_SORT_KEYS);
    // A projection names only fields the schema stores (SPEC §7.5.1); the schema is `strict`, so no extra field.
    if let Projection::Fields { fields, extra } = &page.projection {
        assert!(extra.is_empty());
        for field in fields {
            let kind = collection
                .schema()
                .field_by_id(*field)
                .map(|f| f.kind().stored());
            assert_eq!(kind, Some(true));
        }
    }
    if let Some(cursor) = &page.search_after {
        let token = cursor.to_token();
        assert_eq!(Cursor::from_token(&token).map(|c| c.to_token()), Ok(token));
    }
    // On the calling thread, with a budget clock that never moves: the run is the input's alone.
    let options = RunOptions::new()
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)));
    let _ = request.run(collection, &options);
});
