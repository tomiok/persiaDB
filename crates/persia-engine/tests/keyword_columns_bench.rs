//! The write group of the `keyword_columns` bench (ROADMAP 7.5.7) reads every write: each mode starts from a fresh
//! database, and each request reads the snapshot after its write, so a flush between the writes and the request does
//! not leave it on the flushed memtable. On a small database of the bench's shape.
#![allow(clippy::unwrap_used)] // Fixed inputs.

#[path = "../benches/support/keyword_columns.rs"]
mod support;

use persia_engine::db::{Execution, SearchResults, Threads};
use persia_engine::search::SortValue;
use support::{Shape, SortSku, database, upsert};

const SHAPE: Shape = Shape {
    segments: 2,
    per_segment: 50,
    in_memtable: 20,
};

fn ids(results: &SearchResults) -> Vec<String> {
    results
        .hits
        .iter()
        .map(|h| h.document.id().to_owned())
        .collect()
}

/// Writes 1..=3, a flush, then writes 4..=5: the request after them finds all five, first by `sku`.
fn run_mode(exec: &Execution) {
    let (mut db, fields) = database(&SHAPE);
    let request = SortSku::new(&db.snapshot(), fields);
    let mut snapshot = db.snapshot();
    for n in 1..=3 {
        snapshot = upsert(&mut db, fields, &SHAPE, n);
    }
    let before_flush = snapshot.clone();
    db.commit().unwrap();
    for n in 4..=5 {
        snapshot = upsert(&mut db, fields, &SHAPE, n);
    }
    let upserted: Vec<String> = (1..=5).map(|n| SHAPE.upserted(n)).collect();
    let hits = ids(&request.run(&snapshot, exec));
    assert_eq!(hits.get(..5), Some(&upserted[..]), "{hits:?}");
    // The snapshot taken before the flush misses the writes after it.
    let before = request.run(&before_flush, exec);
    let stale = ids(&before);
    assert_eq!(stale.get(..3), upserted.get(..3), "{stale:?}");
    // By write 4's own `sku`: its id is also a generated document of the snapshot before the flush.
    let write_4 = SortValue::Keyword(format!("sku-{:016x}", 4).into_bytes());
    let skus: Vec<&SortValue> = before.hits.iter().filter_map(|h| h.sort.first()).collect();
    assert!(!skus.contains(&&write_4), "{skus:?}");
}

#[test]
fn both_write_modes_read_every_write_across_a_flush() {
    // In the bench's order: the second mode does not see the first one's writes or flush.
    run_mode(&Execution::default().threads(Threads::Current));
    run_mode(&Execution::default().threads(Threads::Global));
}
