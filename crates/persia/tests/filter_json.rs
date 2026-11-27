//! A filter given in JSON on a `SearchRequest` (ROADMAP 10.1.35; SPEC §7.1.2, §7.1.4; ADR-0063): `filter_json` and
//! `soft_filter_json` select what the same filter inside a request JSON selects, for any filter (seeded cases, the seed
//! in every failure message), and fail with the same `ErrorCode` when it is not a filter or is past a query limit.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use persia::{Collection, Db, ErrorCode, Filter, FilterValue, SearchRequest};
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestRunner};
use serde_json::{Value as J, json};

const SCHEMA: &str = r#"{"name": "items", "fields": {
    "title": {"type": "text"},
    "brand": {"type": "keyword", "stored": true},
    "price": {"type": "f64", "stored": true},
    "stock": {"type": "i64", "stored": true},
    "active": {"type": "bool", "stored": true},
    "at": {"type": "datetime", "stored": true},
    "notes": {"type": "stored"}
}}"#;

const BRANDS: [&str; 3] = ["acme", "zeta", "nova"];
const WORDS: [&str; 4] = ["red", "shoes", "blue", "coat"];

/// A collection of 30 items whose values cycle at different rates, so filters cut it in many ways; some items leave
/// a field out, for `exists` and for the documents a `term` or a `range` does not select.
fn items() -> (tempfile::TempDir, Db, Collection) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("items.persia")).unwrap();
    let c = db.create_collection_json(SCHEMA).unwrap();
    for n in 0_usize..30 {
        let i = i64::try_from(n).unwrap();
        let mut doc = json!({
            "id": format!("d{n:02}"),
            "title": format!("{} {}", WORDS[n % 4], WORDS[(n / 4) % 4]),
            "stock": i % 7,
            "at": 1_700_000_000_000_000_i64 + i * 1_000_000,
        });
        if i % 5 != 0 {
            doc["brand"] = json!(BRANDS[n % 3]);
        }
        if i % 4 != 1 {
            doc["price"] = json!(f64::from(u8::try_from(n).unwrap()) * 2.5);
        }
        if i % 3 != 2 {
            doc["active"] = json!(i % 2 == 0);
        }
        c.put_json(&doc.to_string()).unwrap();
    }
    (dir, db, c)
}

/// A filter's JSON: valid ones mostly, and some the parser refuses (an empty range, an unknown or `stored` field, a
/// value of another type), whose error must be the same both ways.
fn any_filter() -> impl Strategy<Value = J> {
    let bound = || proptest::option::of((any::<bool>(), -5_i64..80));
    let range = |field: &'static str, scale: i64| {
        (bound(), bound()).prop_map(move |(lower, upper)| {
            let mut bounds = serde_json::Map::new();
            if let Some((inclusive, x)) = lower {
                bounds.insert(
                    if inclusive { "gte" } else { "gt" }.into(),
                    json!(x * scale),
                );
            }
            if let Some((inclusive, x)) = upper {
                bounds.insert(
                    if inclusive { "lte" } else { "lt" }.into(),
                    json!(x * scale),
                );
            }
            json!({"range": [field, bounds]})
        })
    };
    let leaf = prop_oneof![
        4 => prop::sample::select(&BRANDS[..]).prop_map(|b| json!({"term": ["brand", b]})),
        4 => (-1_i64..8).prop_map(|s| json!({"term": ["stock", s]})),
        4 => any::<bool>().prop_map(|a| json!({"term": ["active", a]})),
        4 => (0_u8..30).prop_map(|p| json!({"term": ["price", f64::from(p) * 2.5]})),
        4 => prop::collection::vec(prop::sample::select(&BRANDS[..]), 0..4)
            .prop_map(|bs| json!({"terms": ["brand", bs]})),
        4 => range("price", 1),
        4 => range("stock", 1),
        4 => range("at", 1_000_000).prop_map(|mut r| {
            // Datetimes of the documents' span: shift the bounds onto it.
            if let Some(bounds) = r["range"][1].as_object_mut() {
                for v in bounds.values_mut() {
                    *v = json!(1_700_000_000_000_000_i64 + v.as_i64().unwrap_or(0) / 3);
                }
            }
            r
        }),
        4 => prop::sample::select(&["title", "brand", "price", "stock", "active", "at"][..])
            .prop_map(|f| json!({"exists": f})),
        // Refused by the parser: an unknown field, a `stored` one, a value of another type.
        1 => Just(json!({"term": ["colour", "red"]})),
        1 => Just(json!({"exists": "notes"})),
        1 => Just(json!({"term": ["stock", "seven"]})),
    ];
    leaf.prop_recursive(3, 16, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(|all| json!({"and": all})),
            prop::collection::vec(inner.clone(), 0..3).prop_map(|any| json!({"or": any})),
            inner.prop_map(|f| json!({"not": f})),
        ]
    })
}

/// What a search returned, to compare: the response JSON (hits, scores, cursors, relaxation, all of it), or the code
/// of the error.
fn outcome(result: persia::Result<persia::SearchResponse>) -> Result<String, ErrorCode> {
    match result {
        Ok(response) => Ok(response.to_json().unwrap()),
        Err(e) => Err(e.code()),
    }
}

/// The search of `filter` given to `filter_json` (or `soft_filter_json` when `soft`), and of the same filter inside a
/// request JSON, with `q` and relaxation on or off.
fn both(
    c: &Collection,
    q: &str,
    filter: &J,
    soft: bool,
    relax: bool,
) -> [Result<String, ErrorCode>; 2] {
    let text = filter.to_string();
    let mut request = SearchRequest::new(q).limit(100);
    request = if soft {
        request.soft_filter_json(text)
    } else {
        request.filter_json(text)
    };
    let mut json = json!({"q": q, "limit": 100});
    json[if soft { "soft_filter" } else { "filter" }] = filter.clone();
    if !relax {
        request = request.relax(persia::RelaxOptions {
            enabled: Some(false),
            ..persia::RelaxOptions::default()
        });
        json["relax"] = json!(false);
    }
    [
        outcome(c.search(&request)),
        outcome(c.search_json(&json.to_string())),
    ]
}

#[test]
fn a_filter_given_alone_and_inside_a_request_json_give_the_same_response() {
    const SEED: u64 = 0x1013_5000_0001;
    let (_dir, _db, c) = items();
    let config = ProptestConfig {
        cases: if cfg!(feature = "slow") { 1024 } else { 96 },
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(SEED),
        ..ProptestConfig::default()
    };
    let words = prop::sample::select(&["", "red", "shoes", "red shoes", "blue coat", "zebra"][..]);
    let ok = std::cell::Cell::new(0_usize);
    TestRunner::new(config)
        .run(
            &(any_filter(), words, any::<bool>(), any::<bool>()),
            |(filter, q, soft, relax)| {
                let [alone, inside] = both(&c, q, &filter, soft, relax);
                prop_assert_eq!(
                    &alone,
                    &inside,
                    "seed {:#x}, q {:?}, soft {}, relax {}, filter {}",
                    SEED,
                    q,
                    soft,
                    relax,
                    filter
                );
                ok.set(ok.get() + usize::from(alone.is_ok()));
                Ok(())
            },
        )
        .unwrap();
    // The comparison is of searches, not only of errors: most generated filters are valid.
    let ok = ok.get();
    assert!(ok > 48, "seed {SEED:#x}: only {ok} searches ran");
}

fn ids(c: &Collection, request: &SearchRequest) -> Vec<String> {
    c.search(request)
        .unwrap()
        .hits
        .iter()
        .map(|h| h.document.id().to_owned())
        .collect()
}

fn exact() -> persia::RelaxOptions {
    persia::RelaxOptions {
        enabled: Some(false),
        ..persia::RelaxOptions::default()
    }
}

#[test]
fn a_filter_json_selects_what_its_typed_filter_selects() {
    let (_dir, _db, c) = items();
    let schema = c.schema().unwrap();
    let brand = schema.field("brand").unwrap().id();
    let stock = schema.field("stock").unwrap().id();
    let typed = Filter::And(vec![
        Filter::Term {
            field: brand,
            value: FilterValue::Keyword("acme".into()),
        },
        Filter::Not(Box::new(Filter::Term {
            field: stock,
            value: FilterValue::I64(0),
        })),
    ]);
    let json = r#"{"and": [{"term": ["brand", "acme"]}, {"not": {"term": ["stock", 0]}}]}"#;
    let by_json = ids(
        &c,
        &SearchRequest::new("")
            .filter_json(json)
            .relax(exact())
            .limit(100),
    );
    assert_eq!(
        by_json,
        ids(
            &c,
            &SearchRequest::new("")
                .filter(typed)
                .relax(exact())
                .limit(100)
        )
    );
    assert!(by_json.len() > 1, "{by_json:?}");
}

#[test]
fn the_later_of_filter_and_filter_json_is_the_one_kept() {
    let (_dir, _db, c) = items();
    let schema = c.schema().unwrap();
    let brand = schema.field("brand").unwrap().id();
    let acme = Filter::Term {
        field: brand,
        value: FilterValue::Keyword("acme".into()),
    };
    let zeta = r#"{"term": ["brand", "zeta"]}"#;
    let request = |r: SearchRequest| ids(&c, &r.relax(exact()).limit(100));
    let only_zeta = request(SearchRequest::new("").filter_json(zeta));
    let only_acme = request(SearchRequest::new("").filter(acme.clone()));
    assert_ne!(only_zeta, only_acme);
    assert_eq!(
        request(
            SearchRequest::new("")
                .filter(acme.clone())
                .filter_json(zeta)
        ),
        only_zeta
    );
    assert_eq!(
        request(
            SearchRequest::new("")
                .filter_json(zeta)
                .filter(acme.clone())
        ),
        only_acme
    );
    // The soft slot is its own: a hard and a soft filter both apply.
    let both = request(
        SearchRequest::new("")
            .filter(acme.clone())
            .soft_filter_json(zeta),
    );
    assert!(both.is_empty(), "no item is of two brands: {both:?}");
    assert_eq!(
        request(
            SearchRequest::new("")
                .soft_filter(acme)
                .soft_filter_json(zeta)
        ),
        only_zeta
    );
}

#[test]
fn a_cursor_of_a_filter_json_search_pages_the_same_request_in_json() {
    let (_dir, _db, c) = items();
    let filter = r#"{"range": ["stock", {"gte": 2}]}"#;
    let first = c
        .search(
            &SearchRequest::new("")
                .filter_json(filter)
                .relax(exact())
                .limit(3),
        )
        .unwrap();
    let token = first.next_cursor().unwrap();
    let json =
        format!(r#"{{"filter": {filter}, "relax": false, "limit": 3, "search_after": "{token}"}}"#);
    let by_json = c.search_json(&json).unwrap();
    let by_setter = c
        .search(
            &SearchRequest::new("")
                .filter_json(filter)
                .relax(exact())
                .limit(3)
                .after(token),
        )
        .unwrap();
    assert_eq!(by_json.to_json().unwrap(), by_setter.to_json().unwrap());
    assert_eq!(by_setter.hits.len(), 3);
}

/// The code of a search of `filter` given to `filter_json`, checked to be the code of the same filter inside a
/// request JSON and of it given to `soft_filter_json`.
fn code_of(c: &Collection, filter: &str) -> ErrorCode {
    let alone = c
        .search(&SearchRequest::new("red").filter_json(filter))
        .unwrap_err()
        .code();
    let soft = c
        .search(&SearchRequest::new("red").soft_filter_json(filter))
        .unwrap_err()
        .code();
    // Text that is not JSON makes the request JSON malformed, which is `InvalidArgument` too.
    let inside = c
        .search_json(&format!(r#"{{"q": "red", "filter": {filter}}}"#))
        .unwrap_err()
        .code();
    assert_eq!(inside, alone, "{filter}");
    assert_eq!(soft, alone, "{filter}");
    alone
}

#[test]
fn text_that_is_not_a_filter_is_an_invalid_argument() {
    let (_dir, _db, c) = items();
    for filter in [
        "",
        "{",
        "not json",
        "[]",
        "{}",
        r#"{"term": ["brand", "acme"], "exists": "brand"}"#,
        r#"{"term": ["brand", "acme"], "term": ["brand", "zeta"]}"#,
        r#"{"prefix": ["brand", "ac"]}"#,
        r#"{"term": ["colour", "red"]}"#,
        r#"{"term": ["title", "red"]}"#,
        r#"{"range": ["stock", {}]}"#,
        r#"{"range": ["stock", {"gt": 5, "lt": 3}]}"#,
        r#"{"term": ["stock", 1.5]}"#,
        r#"{"exists": "notes"}"#,
    ] {
        assert_eq!(code_of(&c, filter), ErrorCode::InvalidArgument, "{filter}");
    }
    // Longer than the 1 MiB a filter's JSON may be (SPEC §7.1.2), but valid: refused for its length alone.
    let padded = format!(r#"{{"term": ["brand", "acme"]{}}}"#, " ".repeat(1 << 20));
    let err = c
        .search(&SearchRequest::new("red").filter_json(padded))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument);
}

// One test per query limit (SPEC §7.1.3) a filter can pass: `max_terms`, `max_clauses` and `max_depth`. The other two,
// `max_expansions` and `max_text_len`, count prefix and fuzzy expansions and the bytes of `q`, which a filter has
// none of. A filter past a limit is `Limit`, as it is inside a request JSON (ADR-0063 item 8).

#[test]
fn a_filter_past_max_terms_is_a_limit() {
    let (_dir, _db, c) = items();
    let values = |n: usize| {
        (0..n)
            .map(|i| format!(r#""b{i}""#))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // The limit is 1024 values: the filter alone holds 1025. (The search joins it to `q`, whose terms count too.)
    let under = format!(r#"{{"terms": ["brand", [{}]]}}"#, values(1000));
    assert!(c.search(&SearchRequest::new("").filter_json(under)).is_ok());
    let past = format!(r#"{{"terms": ["brand", [{}]]}}"#, values(1025));
    assert_eq!(code_of(&c, &past), ErrorCode::Limit);
}

#[test]
fn a_filter_past_max_clauses_is_a_limit() {
    let (_dir, _db, c) = items();
    // An `and` of n `exists` is n + 1 clauses: 1025, past the 1024, for the filter alone. (The search joins it to `q`,
    // whose clauses count too.)
    let and = |n: usize| {
        format!(
            r#"{{"and": [{}]}}"#,
            vec![r#"{"exists": "brand"}"#; n].join(", ")
        )
    };
    assert!(
        c.search(&SearchRequest::new("").filter_json(and(1000)))
            .is_ok()
    );
    assert_eq!(code_of(&c, &and(1024)), ErrorCode::Limit);
}

#[test]
fn a_filter_past_max_depth_is_a_limit() {
    let (_dir, _db, c) = items();
    // n `not`s around a leaf are n + 1 levels deep: 33, past the 32, for the filter alone. (The search joins it to `q`,
    // a level more.)
    let nested = |n: usize| {
        format!(
            "{}{}{}",
            r#"{"not": "#.repeat(n),
            r#"{"exists": "brand"}"#,
            "}".repeat(n)
        )
    };
    assert!(
        c.search(&SearchRequest::new("").filter_json(nested(20)))
            .is_ok()
    );
    assert_eq!(code_of(&c, &nested(32)), ErrorCode::Limit);
}
