//! `Collection::search` and `search_json` (ROADMAP 10.1.3; SPEC §7; ADR-0062, ADR-0063): paging through the opaque
//! cursor, refused cursors, proximity and relaxation, read-only and closed databases.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::collections::BTreeSet;

use persia::{
    Collection, Db, ErrorCode, FacetRequest, Filter, FilterValue, OpenOptions, Order, RelaxLevel,
    RelaxOptions, SearchRequest, Sort, SortKey,
};

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
