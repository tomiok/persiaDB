//! `Collection::search` and `search_json` (ROADMAP 10.1.3; SPEC §7; ADR-0062, ADR-0063): paging through the opaque
//! cursor, refused cursors, proximity and relaxation, read-only and closed databases.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::Duration;

use persia::{
    Collection, Db, ErrorCode, FacetRequest, Filter, FilterValue, OpenOptions, Order, RelaxLevel,
    RelaxOptions, SearchRequest, Sort, SortKey,
};
use persia_engine::db::Clock;

const SCHEMA: &str = r#"{"name": "items", "fields": {
    "title": {"type": "text"},
    "brand": {"type": "keyword", "facet": true, "stored": true},
    "rank": {"type": "i64", "stored": true}
}}"#;

fn items(db: &Db, n: usize) -> Collection {
    let c = db.create_collection_json(SCHEMA).unwrap();
    for i in 0..n {
        let brand = if i % 3 == 0 { "acme" } else { "zeta" };
        c.put_json(&format!(
            r#"{{"id": "d{i:03}", "title": "red shoe number {i}", "brand": "{brand}", "rank": {i}}}"#
        ))
        .unwrap();
    }
    c
}

fn exact() -> RelaxOptions {
    RelaxOptions {
        enabled: Some(false),
        ..RelaxOptions::default()
    }
}

fn by_rank(c: &Collection) -> Sort {
    let schema = c.schema().unwrap();
    let rank = schema.field("rank").unwrap().id();
    Sort::new([SortKey::field(rank, Order::Asc)], &schema).unwrap()
}

#[test]
fn the_cursor_pages_through_every_hit_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = items(&db, 25);
    db.commit().unwrap();
    // Part in a segment, part in the memtable: the cursor crosses both.
    for i in 25..31 {
        c.put_json(&format!(
            r#"{{"id": "d{i:03}", "title": "red shoe {i}", "brand": "acme", "rank": {i}}}"#
        ))
        .unwrap();
    }
    for sort in [None, Some(by_rank(&c))] {
        let base = SearchRequest::new("red shoe").relax(exact()).limit(7);
        let base = match &sort {
            Some(s) => base.sort(s.clone()),
            None => base,
        };
        let mut seen = Vec::new();
        let mut request = base.clone();
        let mut pages = 0;
        loop {
            let page = c.search(&request).unwrap();
            if page.hits.is_empty() {
                assert!(page.next_cursor().is_none());
                break;
            }
            assert!(page.hits.len() <= 7);
            seen.extend(page.hits.iter().map(|h| h.document.id().to_owned()));
            request = base.clone().after(page.next_cursor().unwrap());
            pages += 1;
            assert!(pages < 10, "the paging ends");
        }
        let unique: BTreeSet<&String> = seen.iter().collect();
        assert_eq!(unique.len(), seen.len(), "no duplicate: {seen:?}");
        assert_eq!(seen.len(), 31, "no gap: {seen:?}");
        if sort.is_some() {
            let expected: Vec<String> = (0..31).map(|i| format!("d{i:03}")).collect();
            assert_eq!(seen, expected, "in the sort's order");
        }
    }
}

#[test]
fn a_damaged_or_foreign_cursor_is_an_invalid_argument() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = items(&db, 10);
    let page = c
        .search(&SearchRequest::new("red").relax(exact()).limit(3))
        .unwrap();
    let token = page.next_cursor().unwrap().to_owned();

    let mut flipped = token.clone().into_bytes();
    let last = flipped.len() - 1;
    flipped[last] = if flipped[last] == b'A' { b'B' } else { b'A' };
    let flipped = String::from_utf8(flipped).unwrap();
    let cut = token.get(..token.len() / 2).unwrap().to_owned();
    for bad in ["", "not a token", "!!!!", cut.as_str(), flipped.as_str()] {
        let err = c
            .search(&SearchRequest::new("red").relax(exact()).after(bad))
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{bad:?}: {err}");
    }
    let json = format!(r#"{{"q": "red", "relax": false, "search_after": "{cut}"}}"#);
    assert_eq!(
        c.search_json(&json).unwrap_err().code(),
        ErrorCode::InvalidArgument
    );

    // A cursor of another sort is refused, and so is one from another collection.
    let err = c
        .search(
            &SearchRequest::new("red")
                .relax(exact())
                .sort(by_rank(&c))
                .after(token.as_str()),
        )
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
    let other = db
        .create_collection_json(&SCHEMA.replace(r#""items""#, r#""others""#))
        .unwrap();
    other
        .put_json(r#"{"id": "o1", "title": "red other", "brand": "acme", "rank": 1}"#)
        .unwrap();
    let err = other
        .search(
            &SearchRequest::new("red")
                .relax(exact())
                .after(token.as_str()),
        )
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
}

#[test]
fn proximity_needs_a_search_that_does_not_relax() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = items(&db, 5);
    let err = c
        .search(&SearchRequest::new("red shoe").proximity(true))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
    assert!(err.message().contains("relax"), "{err}");
    let ok = c
        .search(
            &SearchRequest::new("red shoe")
                .relax(exact())
                .proximity(true),
        )
        .unwrap();
    assert_eq!(ok.hits.len(), 5);
    assert_eq!(
        c.search_json(r#"{"q": "red shoe", "proximity": true}"#)
            .unwrap_err()
            .code(),
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        c.search_json(r#"{"q": "red shoe", "proximity": true, "relax": false}"#)
            .unwrap()
            .hits
            .len(),
        5
    );
}

#[test]
fn filters_facets_explain_and_relaxation() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = items(&db, 9);
    let schema = c.schema().unwrap();
    let brand = schema.field("brand").unwrap().id();
    let acme = Filter::Term {
        field: brand,
        value: FilterValue::Keyword("acme".into()),
    };
    let response = c
        .search(
            &SearchRequest::new("red")
                .filter(acme)
                .facets([FacetRequest::new(brand, 5, &schema).unwrap()])
                .explain(true)
                .relax(exact()),
        )
        .unwrap();
    assert_eq!(response.hits.len(), 3, "every third item is acme");
    assert!(response.hits.iter().all(|h| h.explanation.is_some()));
    assert!(response.hits.iter().all(|h| h.level == RelaxLevel::Exact));
    assert_eq!(response.facets.len(), 1);
    assert_eq!(response.facets[0].values[0].value, b"acme");
    assert_eq!(response.relaxation.level, RelaxLevel::Exact);

    // Never zero results by default: a word that matches nothing is relaxed away.
    let relaxed = c.search(&SearchRequest::new("red unicorn")).unwrap();
    assert!(!relaxed.hits.is_empty());
    assert_ne!(relaxed.relaxation.level, RelaxLevel::Exact);
    assert_ne!(relaxed.relaxation.applied, Vec::<String>::new());
    // The same request with relaxation off finds nothing.
    let none = c
        .search(&SearchRequest::new("red unicorn").relax(exact()))
        .unwrap();
    assert!(none.hits.is_empty());

    // A request JSON key this build does not know, or a field the schema lacks, is refused.
    for bad in [
        r#"{"query": "red"}"#,
        r#"{"q": "red", "fields": ["nope"]}"#,
        "not json",
    ] {
        assert_eq!(
            c.search_json(bad).unwrap_err().code(),
            ErrorCode::InvalidArgument,
            "{bad}"
        );
    }
    let err = c
        .search(&SearchRequest::new("red").fields(["nope"]))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument, "{err}");
}

#[test]
fn a_read_only_database_searches_what_its_last_refresh_found() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let writer = Db::open(&path).unwrap();
    let w = items(&writer, 2);
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    let r = reader.collection("items").unwrap();
    let request = SearchRequest::new("shoe").relax(exact());
    assert_eq!(r.search(&request).unwrap().hits.len(), 2);
    w.put_json(r#"{"id": "new", "title": "blue shoe", "brand": "acme", "rank": 9}"#)
        .unwrap();
    writer.commit().unwrap();
    assert_eq!(
        r.search(&request).unwrap().hits.len(),
        2,
        "before the refresh"
    );
    reader.refresh().unwrap();
    assert_eq!(r.search(&request).unwrap().hits.len(), 3);
}

#[test]
fn a_search_after_close_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = items(&db, 1);
    db.close().unwrap();
    assert_eq!(
        c.search(&SearchRequest::new("red")).unwrap_err().code(),
        ErrorCode::Closed
    );
    assert_eq!(
        c.search_json(r#"{"q": "red"}"#).unwrap_err().code(),
        ErrorCode::Closed
    );
}

/// Each read of the database's clock moves it 1 ms forward and is counted (ROADMAP 10.1.44): a search's timeout of
/// `T` ms passes at the `T`-th read after the one the search takes its start from.
#[derive(Debug)]
struct Ticking {
    start: std::time::Instant,
    reads: AtomicU64,
}

impl Clock for Ticking {
    fn now(&self) -> std::time::Instant {
        self.start + Duration::from_millis(self.reads.fetch_add(1, AtomicOrdering::Relaxed))
    }

    fn unix_micros(&self) -> i64 {
        1_700_000_000_000_000
    }
}

/// A search with a timeout (ROADMAP 10.1.44; SPEC §7.1.4, §7.3) fails with `Cancelled` at the first check that reads
/// the database's clock past it, never with an early or relaxed page; one the clock does not reach returns the page a
/// search without it does. The clock is injected and moves at each read: nothing sleeps.
#[test]
fn a_search_past_its_timeout_fails_at_the_next_check() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(Ticking {
        start: std::time::Instant::now(),
        reads: AtomicU64::new(0),
    });
    let options = OpenOptions::new()
        .clock(Arc::clone(&clock) as Arc<dyn Clock>)
        .hold_maintenance(true);
    let db = options.open(dir.path().join("app.persia")).unwrap();
    // In the memtable alone: one view, searched on the calling thread, so the reads come in one order.
    let c = items(&db, 12);
    // "zebra" is in no document: the relaxed search finds its hits after L0, and the budget never passes here.
    let generous = RelaxOptions {
        budget_ms: Some(60_000),
        ..RelaxOptions::default()
    };
    for (relax, q, relax_json) in [
        (exact(), "red shoe", "false"),
        (generous, "red zebra", r#"{"budget_ms": 60000}"#),
    ] {
        let request = SearchRequest::new(q).relax(relax);
        let ids = |page: &persia::SearchResponse| -> Vec<String> {
            page.hits
                .iter()
                .map(|h| h.document.id().to_owned())
                .collect()
        };
        let untimed = ids(&c.search(&request).unwrap());
        assert!(!untimed.is_empty(), "{q}");
        let timed = |timeout: Duration| {
            let before = clock.reads.load(AtomicOrdering::Relaxed);
            let found = c.search(&request.clone().timeout(timeout));
            (found, clock.reads.load(AtomicOrdering::Relaxed) - before)
        };
        let (whole, reads) = timed(Duration::from_secs(3_600));
        assert_eq!(ids(&whole.unwrap()), untimed);
        let mut failed = 0_u64;
        for ms in 0..reads {
            match timed(Duration::from_millis(ms)).0 {
                Err(e) => {
                    assert_eq!(e.code(), ErrorCode::Cancelled, "{ms} ms: {e}");
                    assert_eq!(failed, ms, "{ms} ms failed after a longer one did not");
                    failed += 1;
                }
                Ok(page) => assert_eq!(ids(&page), untimed, "{ms} ms"),
            }
        }
        assert!(failed >= 1, "{failed} of {reads} reads");
        // A timeout the clock cannot represent is no limit, not a panic.
        assert_eq!(ids(&timed(Duration::MAX).0.unwrap()), untimed);
        // The request JSON's `timeout_ms` is the same timeout.
        let json = |ms: u64| {
            c.search_json(&format!(
                r#"{{"q": "{q}", "relax": {relax_json}, "timeout_ms": {ms}}}"#
            ))
        };
        assert_eq!(json(0).unwrap_err().code(), ErrorCode::Cancelled);
        assert_eq!(ids(&json(3_600_000).unwrap()), untimed);
        assert_eq!(ids(&json(u64::MAX).unwrap()), untimed);
    }
}

/// The timeout is read on the database's clock, not the system's: on an injected clock that does not move, a
/// timeout of 1 ns never passes however long the search takes, and one of 0 has passed (the clock reads its end).
#[test]
fn the_timeout_reads_the_databases_clock() {
    let dir = tempfile::tempdir().unwrap();
    let still = Arc::new(persia_engine::db::ManualClock::new(1_700_000_000_000_000));
    let options = OpenOptions::new()
        .clock(Arc::clone(&still) as Arc<dyn Clock>)
        .hold_maintenance(true);
    let db = options.open(dir.path().join("app.persia")).unwrap();
    let c = items(&db, 5);
    db.commit().unwrap();
    for relax in [exact(), RelaxOptions::default()] {
        let request = SearchRequest::new("red shoe").relax(relax);
        let tiny = request.clone().timeout(Duration::from_nanos(1));
        assert_eq!(c.search(&tiny).unwrap().hits.len(), 5);
        let zero = request.timeout(Duration::ZERO);
        assert_eq!(c.search(&zero).unwrap_err().code(), ErrorCode::Cancelled);
    }
}

/// `timeout_ms` is read as strictly as every key of the request JSON (SPEC §7.1.4): a value of another shape and a
/// misspelt key are `InvalidArgument`, never a search without a timeout. `Duration::ZERO` and `timeout_ms: 0` have
/// passed at the first check, on the system clock too.
#[test]
fn timeout_values_and_misspellings() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = items(&db, 3);
    for relax in [exact(), RelaxOptions::default()] {
        let zero = SearchRequest::new("red")
            .relax(relax)
            .timeout(Duration::ZERO);
        assert_eq!(c.search(&zero).unwrap_err().code(), ErrorCode::Cancelled);
    }
    assert_eq!(
        c.search_json(r#"{"q": "red", "timeout_ms": 0}"#)
            .unwrap_err()
            .code(),
        ErrorCode::Cancelled
    );
    // The later setter call is the one kept.
    let kept = SearchRequest::new("red")
        .timeout(Duration::ZERO)
        .timeout(Duration::from_secs(3_600));
    assert_eq!(c.search(&kept).unwrap().hits.len(), 3);
    for bad in [
        r#"{"q": "red", "timeout_ms": -1}"#,
        r#"{"q": "red", "timeout_ms": -0}"#,
        r#"{"q": "red", "timeout_ms": 2.5}"#,
        r#"{"q": "red", "timeout_ms": 1e3}"#,
        r#"{"q": "red", "timeout_ms": 18446744073709551616}"#,
        r#"{"q": "red", "timeout_ms": "100"}"#,
        r#"{"q": "red", "timeout_ms": null}"#,
        r#"{"q": "red", "timeout": 100}"#,
        r#"{"q": "red", "timeoutMs": 100}"#,
        r#"{"q": "red", "deadline": 100}"#,
    ] {
        let err = c.search_json(bad).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument, "{bad}: {err}");
    }
}

/// ROADMAP 10.1.45 (SPEC §6.2.3, §7.2): a query over documents of equal text score ranks the one of higher static
/// rank first, its score the text score plus the contribution, and its explanation says so (`with_static_rank`).
#[test]
fn equal_text_scores_rank_by_the_collections_static_rank() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("app.persia")).unwrap();
    let c = db
        .create_collection_json(
            r#"{"name": "shoes", "fields": {"title": {"type": "text"}, "sales": {"type": "i64"}},
                "ranking": {"static_rank": {"field": "sales"}}}"#,
        )
        .unwrap();
    c.put_json(r#"{"id": "few", "title": "red shoes", "sales": 3}"#)
        .unwrap();
    c.put_json(r#"{"id": "many", "title": "red shoes", "sales": 900}"#)
        .unwrap();
    c.put_json(r#"{"id": "none", "title": "red shoes"}"#)
        .unwrap();
    for relax in [exact(), RelaxOptions::default()] {
        let page = c
            .search(&SearchRequest::new("red").relax(relax).explain(true))
            .unwrap();
        let ids: Vec<&str> = page.hits.iter().map(|h| h.document.id()).collect();
        assert_eq!(ids, ["many", "few", "none"]);
        let text = page.hits[2].score;
        assert_eq!(page.hits[0].score, text + 901_f64.ln());
        assert_eq!(page.hits[1].score, text + 4_f64.ln());
        let json: serde_json::Value = serde_json::from_str(&page.to_json().unwrap()).unwrap();
        let explanation = &json["hits"][0]["explanation"];
        let node = explanation
            .get("relaxed")
            .map_or(explanation, |r| &r["explanation"]);
        assert_eq!(
            node["with_static_rank"]["static_rank"]["value"], 900.0,
            "{json}"
        );
    }
}
