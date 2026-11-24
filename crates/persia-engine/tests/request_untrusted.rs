//! A request's untrusted input at its edges (ROADMAP 10.1.10; SPEC §7.1.4, §7.4, §17; ADR-0062): the cursor token's
//! text, and request JSON that is deep, huge or written to slip past the strict reader.
//!
//! Invariants:
//! 1. Any one change to a token's text (a bit flipped in a character, a prefix, a suffix appended or a character
//!    substituted) is refused as `Encoding`, `Version` or `Checksum`: never accepted, never a panic, and never
//!    `Malformed`. One base64url character holds 6 bits, and CRC-32C finds every burst of up to 32, so damage never
//!    reaches the field readers.
//! 2. A token of another version is `Version { version }` even under a matching checksum, from `Cursor::from_token`
//!    and from a request's `search_after` alike (SPEC §7.4: the version is read first).
//! 3. A decoded cursor is checked against the search it is given to (ADR-0062 decision 12): a relaxed search's token
//!    under `relax: false` or under other settings, and a token of another sort's value types, are refused.
//! 4. Nesting far deeper than any limit is an error, not a stack overflow; a filter exactly at `max_depth` parses and
//!    one level more is `TooDeep` (SPEC §7.1.3).
//! 5. Huge numbers are the error of their key, never a panic or an overflow; `offset + limit` at `usize::MAX` is a
//!    `Window` error of the run (SPEC §7.4).
//! 6. `search_after` with an `offset` skips `offset` hits after the cursor (SPEC §7.4: "only hits that rank after the
//!    cursor count, `offset` included"), for the plain search and the relaxed one.
//! 7. The fuzz targets' seed corpora are valid requests and tokens, so the fuzzers start past the parsers' errors.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    missing_docs
)] // Tests: inputs are fixed.

use std::sync::Arc;

use persia_engine::db::{
    CollectionSnapshot, Cursor, CursorOrigin, Database, DatabaseOptions, Located, ManualClock,
    SegmentIds, StepCursor, Threads,
};
use persia_engine::document::{Document, Value};
use persia_engine::query::{QueryError, QueryLimits};
use persia_engine::relax::{RelaxOptions, RelaxPlan};
use persia_engine::schema::{
    CollectionSchema, FieldKind, FilterRelaxation, KeywordOptions, RelaxationConfig, SchemaBuilder,
    TextOptions,
};
use persia_engine::search::request::{
    CURSOR_TOKEN_VERSION, CursorTokenError, QueryRequest, QueryRequestError, QueryResults,
    RunOptions,
};
use persia_engine::search::{RequestError, SortValue};
use persia_engine::wal::Durability;
use persia_format::checksum::crc32c;
use persia_storage::container::sim::SimFile;
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestRunner};
use serde_json::{Value as J, json};

const PROPTEST_SEED: u64 = 0x1011_0000_0002;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

// --- helpers ----------------------------------------------------------------------------------------------------------

/// The schema of the `request_json` fuzz target, so its seeds can be checked against it.
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
        .unwrap()
}

fn parse(json: &str) -> Result<QueryRequest, QueryRequestError> {
    QueryRequest::from_json(json, &schema(), &QueryLimits::default())
}

/// `(id, title, brand, price)`: the first four in a segment, the rest in the memtable.
const DOCS: [(&str, &str, &str, f64); 7] = [
    ("a", "red running shoes", "nike", 80.0),
    ("b", "red trail shoes", "acme", 120.0),
    ("c", "blue running shoes", "nike", 60.0),
    ("d", "red hat", "acme", 20.0),
    ("e", "red boots", "nike", 150.0),
    ("f", "red sandals", "nike", 40.0),
    ("g", "red shoes red laces", "acme", 95.0),
];

fn database() -> Database<SimFile> {
    let schema = schema();
    let field = |n: &str| schema.field(n).unwrap().id();
    let (title, brand, price) = (field("title"), field("brand"), field("price"));
    let options = DatabaseOptions {
        clock: Arc::new(ManualClock::new(1_700_000_000_000_000)) as _,
        segment_ids: SegmentIds::seeded(62),
        ..DatabaseOptions::default()
    };
    let mut db = Database::create_on(SimFile::new(), [62; 16], options).unwrap();
    db.create_collection(schema).unwrap();
    for (i, (id, text, by, cost)) in DOCS.into_iter().enumerate() {
        let doc = Document::new(id)
            .with(title, Value::String(text.into()))
            .with(brand, Value::String(by.into()))
            .with(price, Value::F64(cost));
        db.put("c", &doc, Durability::Fsync).unwrap();
        if i == 3 {
            db.commit().unwrap();
        }
    }
    db
}

/// On the calling thread, with a budget clock that never moves.
fn options() -> RunOptions {
    RunOptions::new()
        .threads(Threads::Current)
        .budget_clock(Arc::new(ManualClock::new(0)))
}

fn run(c: &CollectionSnapshot, json: &J) -> Result<QueryResults, QueryRequestError> {
    let request = QueryRequest::from_json(&json.to_string(), c.schema(), &QueryLimits::default())?;
    Ok(request.run(c, &options())?.results)
}

/// The ids of a page and the cursor of each hit, whichever search ran.
fn hits(results: QueryResults) -> Vec<(String, Cursor)> {
    match results {
        QueryResults::Relaxed(r) => r
            .hits
            .iter()
            .map(|h| (h.hit.document.id().to_owned(), h.cursor()))
            .collect(),
        QueryResults::Plain(r) => r
            .hits
            .iter()
            .map(|h| (h.document.id().to_owned(), h.cursor()))
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn ids(page: &[(String, Cursor)]) -> Vec<&str> {
    page.iter().map(|(id, _)| id.as_str()).collect()
}

/// Base64url without padding, written here, not taken from the engine.
fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .chain(std::iter::repeat(&0))
            .take(3)
            .fold(0_u32, |n, &b| (n << 8) | u32::from(b));
        for i in 0..=chunk.len() {
            let sextet = usize::try_from((n >> (18 - 6 * i)) & 63).unwrap();
            out.push(char::from(ALPHABET[sextet]));
        }
    }
    out
}

/// The bytes of base64url text this test wrote or the engine wrote (canonical input only).
fn b64_decode(text: &str) -> Vec<u8> {
    let sextet = |c: u8| u32::try_from(ALPHABET.iter().position(|&a| a == c).unwrap()).unwrap();
    let mut out = Vec::new();
    for chunk in text.as_bytes().chunks(4) {
        let n = chunk.iter().fold(0_u32, |n, &c| (n << 6) | sextet(c)) << (6 * (4 - chunk.len()));
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    out
}

/// Invariant 1: `token` is refused as damage is (`Encoding`, `Version` or `Checksum`), not read as a cursor.
fn refused_as_damage(token: &str) -> Result<(), String> {
    match Cursor::from_token(token) {
        Err(
            CursorTokenError::Encoding
            | CursorTokenError::Version { .. }
            | CursorTokenError::Checksum,
        ) => Ok(()),
        other => Err(format!("{token:?}: {other:?}")),
    }
}

fn plain_cursor() -> Cursor {
    Cursor {
        values: vec![SortValue::Score(1.5)],
        location: Located::Segment { index: 2, docid: 7 },
        origin: CursorOrigin {
            seq: 3,
            version: 9,
            collection: "items".into(),
        },
        relaxed: None,
        proximity: false,
        relax: None,
    }
}

/// Every field set, each kind of sort value among them.
fn full_cursor() -> Cursor {
    let options = RelaxOptions {
        min_results: Some(5),
        fill: Some(true),
        filters: Some(FilterRelaxation::LastResort),
        ..RelaxOptions::default()
    };
    Cursor {
        values: vec![
            SortValue::Missing,
            SortValue::Score(-0.0),
            SortValue::Key(u64::MAX),
            SortValue::Keyword(b"nike\xff\x00".to_vec()),
        ],
        location: Located::Memtable(u32::MAX),
        origin: CursorOrigin {
            seq: u64::MAX,
            version: 1 << 40,
            collection: "zapatillas ñ".into(),
        },
        relaxed: Some(StepCursor {
            step: 2,
            last: 300,
            hit: 4,
        }),
        proximity: true,
        relax: RelaxPlan::new(&RelaxationConfig::default(), &options)
            .unwrap()
            .settings(),
    }
}

/// `cases` by default; `PROPTEST_CASES` overrides it for a long run. The seed is fixed either way.
fn runner(cases: u32) -> TestRunner {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(cases);
    TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })
}

fn any_cursor() -> impl Strategy<Value = Cursor> {
    let value = prop_oneof![
        Just(SortValue::Missing),
        any::<u64>().prop_map(|b| SortValue::Score(f64::from_bits(b))),
        any::<u64>().prop_map(SortValue::Key),
        proptest::collection::vec(any::<u8>(), 0..24).prop_map(SortValue::Keyword),
    ];
    let location = prop_oneof![
        any::<u32>().prop_map(Located::Memtable),
        (any::<usize>(), any::<u32>()).prop_map(|(index, docid)| Located::Segment { index, docid }),
    ];
    let step =
        proptest::option::of(
            (any::<usize>(), any::<usize>(), any::<usize>())
                .prop_map(|(step, last, hit)| StepCursor { step, last, hit }),
        );
    let relax =
        proptest::option::of((1..=1_000_u32, any::<bool>(), any::<bool>())).prop_map(|settings| {
            let (min_results, fill, last_resort) = settings?;
            let options = RelaxOptions {
                min_results: Some(min_results),
                fill: Some(fill),
                filters: Some(if last_resort {
                    FilterRelaxation::LastResort
                } else {
                    FilterRelaxation::Never
                }),
                ..RelaxOptions::default()
            };
            RelaxPlan::new(&RelaxationConfig::default(), &options)
                .unwrap()
                .settings()
        });
    (
        proptest::collection::vec(value, 0..4),
        location,
        (any::<u64>(), any::<u64>(), ".{0,12}"),
        step,
        any::<bool>(),
        relax,
    )
        .prop_map(
            |(values, location, (seq, version, collection), relaxed, proximity, relax)| Cursor {
                values,
                location,
                origin: CursorOrigin {
                    seq,
                    version,
                    collection,
                },
                relaxed,
                proximity,
                relax,
            },
        )
}

// --- 1: damage to a token's text --------------------------------------------------------------------------------------

/// Every one-bit flip of every character, every prefix, and suffixes appended: each refused as damage.
fn every_single_change_is_refused(token: &str) -> Result<(), String> {
    let bytes = token.as_bytes();
    for at in 0..bytes.len() {
        for bit in 0..8 {
            let mut damaged = bytes.to_vec();
            damaged[at] ^= 1 << bit;
            // A flipped high bit is no `&str`: such text never reaches `from_token`.
            if let Ok(text) = String::from_utf8(damaged) {
                refused_as_damage(&text).map_err(|e| format!("flip {at}:{bit}: {e}"))?;
            }
        }
        // Every other character of the alphabet in its place.
        for &c in ALPHABET.iter().filter(|&&c| c != bytes[at]) {
            let mut damaged = bytes.to_vec();
            damaged[at] = c;
            refused_as_damage(std::str::from_utf8(&damaged).unwrap())
                .map_err(|e| format!("substitute {at}:{}: {e}", char::from(c)))?;
        }
    }
    // Every prefix: the empty text has no version byte, every other prefix is refused as damage.
    if Cursor::from_token("") != Err(CursorTokenError::Malformed { what: "empty" }) {
        return Err("the empty token".into());
    }
    for len in 1..token.len() {
        refused_as_damage(&token[..len]).map_err(|e| format!("prefix {len}: {e}"))?;
    }
    // Suffixes, and text before it.
    for &c in ALPHABET {
        let c = char::from(c);
        refused_as_damage(&format!("{token}{c}")).map_err(|e| format!("suffix {c}: {e}"))?;
        refused_as_damage(&format!("{c}{token}")).map_err(|e| format!("prefix {c}: {e}"))?;
    }
    for suffix in ["AA", "AAA", "AAAA", "AAAAAAAA", "____", token] {
        refused_as_damage(&format!("{token}{suffix}"))
            .map_err(|e| format!("suffix {suffix}: {e}"))?;
    }
    Ok(())
}

#[test]
fn every_single_change_to_a_tokens_text_is_refused() {
    for cursor in [plain_cursor(), full_cursor()] {
        let token = cursor.to_token();
        every_single_change_is_refused(&token).unwrap();
        // And the text is still the cursor's: nothing above changed it.
        assert_eq!(Cursor::from_token(&token).unwrap(), cursor);
    }
}

#[test]
fn every_single_change_to_any_cursors_token_is_refused() {
    runner(128)
        .run(&any_cursor(), |cursor| {
            let token = cursor.to_token();
            // The round trip, bit for bit (a NaN score included: its token holds its bits).
            prop_assert_eq!(
                Cursor::from_token(&token).unwrap().to_token(),
                token.clone()
            );
            if let Err(e) = every_single_change_is_refused(&token) {
                prop_assert!(false, "{}", e);
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn text_near_a_token_is_not_one() {
    let token = full_cursor().to_token();
    assert!(token.contains('-') || token.contains('_'), "{token}");
    // Standard base64's alphabet, padding, whitespace around it or inside it, and a quoted token.
    let standard = token.replace('-', "+").replace('_', "/");
    for text in [
        standard.as_str(),
        &format!("{token}="),
        &format!("{token}=="),
        &format!(" {token}"),
        &format!("{token}\n"),
        &format!("{}\n{}", &token[..8], &token[8..]),
        &format!("\"{token}\""),
        &token.to_lowercase(),
    ] {
        assert_eq!(
            Cursor::from_token(text).map(|c| c.to_token()),
            Err(CursorTokenError::Encoding),
            "{text:?}"
        );
    }
}

// --- 2: another version -----------------------------------------------------------------------------------------------

/// `token`'s bytes under version `version`, with a checksum that matches them.
fn under_version(token: &str, version: u8) -> String {
    let mut bytes = b64_decode(token);
    bytes[0] = version;
    let body = bytes.len() - 4;
    let sum = crc32c(&bytes[..body]).to_le_bytes();
    bytes[body..].copy_from_slice(&sum);
    b64_encode(&bytes)
}

#[test]
fn another_version_is_refused_by_the_token_and_by_the_request() {
    assert_eq!(CURSOR_TOKEN_VERSION, 1);
    for cursor in [plain_cursor(), full_cursor()] {
        let token = cursor.to_token();
        // The test's codec reads the engine's text: the re-framing below changes the version byte only.
        assert_eq!(b64_encode(&b64_decode(&token)), token);
        assert_eq!(under_version(&token, 1), token);
        for version in [0, 2, 3, 0x7f, 0x80, 0xff] {
            let other = under_version(&token, version);
            assert_eq!(
                Cursor::from_token(&other),
                Err(CursorTokenError::Version { version }),
                "{version}"
            );
            let json = json!({"q": "red", "search_after": other}).to_string();
            match parse(&json) {
                Err(QueryRequestError::Cursor(CursorTokenError::Version { version: got })) => {
                    assert_eq!(got, version);
                }
                other => panic!("{version}: {other:?}"),
            }
        }
    }
    // A later version may lay out the rest as it likes: whatever follows its version byte, it is the version that
    // is named, checksum or not.
    for rest in [&[][..], &[0][..], &[0xff; 3][..], &[7; 64][..]] {
        let mut bytes = vec![2];
        bytes.extend_from_slice(rest);
        assert_eq!(
            Cursor::from_token(&b64_encode(&bytes)),
            Err(CursorTokenError::Version { version: 2 }),
            "{rest:?}"
        );
    }
}

// --- 3: a decoded cursor is checked against its search ----------------------------------------------------------------

#[test]
fn a_token_from_another_kind_of_search_is_refused() {
    let db = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();

    // A relaxed search's token: it records its step and its cascade's settings.
    let relaxed = hits(run(c, &json!({"q": "red", "limit": 2})).unwrap());
    let (_, cursor) = &relaxed[0];
    assert!(cursor.relax.is_some(), "{cursor:?}");
    let token = cursor.to_token();
    // The same request pages on.
    run(c, &json!({"q": "red", "limit": 2, "search_after": token})).unwrap();
    // Under `relax: false`: the plain search records no settings.
    let err = run(
        c,
        &json!({"q": "red", "limit": 2, "relax": false, "search_after": token}),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            QueryRequestError::Request(RequestError::CursorRelax {
                cursor: Some(_),
                search: None
            })
        ),
        "{err}"
    );
    // Under other settings of the cascade: each of `min_results`, `fill`, `filters` and `levels`.
    for relax in [
        json!({"min_results": 3}),
        json!({"fill": true}),
        json!({"filters": "last_resort"}),
        json!({"levels": ["fuzzy"]}),
    ] {
        let json = json!({"q": "red", "limit": 2, "relax": relax, "search_after": token});
        let err = run(c, &json).unwrap_err();
        assert!(
            matches!(
                err,
                QueryRequestError::Request(RequestError::CursorRelax {
                    cursor: Some(_),
                    search: Some(_)
                })
            ),
            "{json}: {err}"
        );
    }

    // A token of a sort by `price` (a column key) given to a sort by `brand` (a keyword), and the reverse.
    let by = |field: &str, after: Option<&str>| {
        let mut json = json!({"q": "", "relax": false, "limit": 2, "sort": [{"field": field}]});
        if let Some(token) = after {
            json["search_after"] = json!(token);
        }
        json
    };
    let price = hits(run(c, &by("price", None)).unwrap())[1].1.to_token();
    let brand = hits(run(c, &by("brand", None)).unwrap())[1].1.to_token();
    run(c, &by("price", Some(&price))).unwrap();
    run(c, &by("brand", Some(&brand))).unwrap();
    for json in [by("brand", Some(&price)), by("price", Some(&brand))] {
        let err = run(c, &json).unwrap_err();
        assert!(
            matches!(err, QueryRequestError::Request(RequestError::Cursor { .. })),
            "{json}: {err}"
        );
    }
    // A sort of two keys given a token of one, and the reverse.
    let two = json!({"q": "", "relax": false, "limit": 2, "sort": [{"field": "price"}, "score"]});
    let mut with_one = two.clone();
    with_one["search_after"] = json!(price);
    assert!(matches!(
        run(c, &with_one),
        Err(QueryRequestError::Request(RequestError::Cursor { .. }))
    ));
    let two_token = hits(run(c, &two).unwrap())[1].1.to_token();
    assert!(matches!(
        run(c, &by("price", Some(&two_token))),
        Err(QueryRequestError::Request(RequestError::Cursor { .. }))
    ));
}

// --- 4: nesting -------------------------------------------------------------------------------------------------------

/// `{"not": ... {"exists": "brand"} ...}`, `nots` deep.
fn nested_not(nots: usize) -> String {
    let mut json = String::with_capacity(nots * 9 + 20);
    for _ in 0..nots {
        json.push_str("{\"not\":");
    }
    json.push_str("{\"exists\":\"brand\"}");
    for _ in 0..nots {
        json.push('}');
    }
    json
}

/// `{"or": [ ... {"exists": "brand"} ... ]}`, `ors` deep.
fn nested_or(ors: usize) -> String {
    format!(
        "{}{{\"exists\":\"brand\"}}{}",
        "{\"or\":[".repeat(ors),
        "]}".repeat(ors)
    )
}

fn kind_of(result: Result<QueryRequest, QueryRequestError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(QueryRequestError::Json(_)) => "json",
        Err(QueryRequestError::Query(QueryError::TooDeep { .. })) => "too deep",
        Err(QueryRequestError::Query(_)) => "query",
        Err(QueryRequestError::Relax(_)) => "relax",
        Err(QueryRequestError::Cursor(_)) => "cursor",
        Err(QueryRequestError::Request(_)) => "request",
        Err(other) => panic!("{other:?}"),
    }
}

#[test]
fn a_filter_is_held_to_max_depth_exactly() {
    let limits = QueryLimits::default();
    assert_eq!(limits.max_depth, 32);
    for key in ["filter", "soft_filter"] {
        // A leaf is 1, each `not` one more (SPEC §7.1.3): 31 of them are 32 deep.
        let at = format!("{{\"{key}\": {}}}", nested_not(31));
        assert_eq!(kind_of(parse(&at)), "ok", "{key}");
        let over = format!("{{\"{key}\": {}}}", nested_not(32));
        assert!(
            matches!(
                parse(&over),
                Err(QueryRequestError::Query(QueryError::TooDeep { limit: 32 }))
            ),
            "{key}"
        );
        // Deeper, but within what the JSON reader nests: still the filter's limit that refuses it.
        let deeper = format!("{{\"{key}\": {}}}", nested_not(100));
        assert_eq!(kind_of(parse(&deeper)), "too deep", "{key}");
    }
}

#[test]
fn nesting_far_past_every_limit_is_an_error_not_a_stack_overflow() {
    // Within the 1 MiB bound, so it is the nesting that is refused, under every key and under none.
    let arrays = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    let objects = |depth: usize| format!("{}1{}", "{\"a\":".repeat(depth), "}".repeat(depth));
    for depth in [129, 1_000, 100_000] {
        let deep_arrays = arrays(depth);
        let mut cases = vec![
            format!("{{\"filter\": {}}}", nested_not(depth)),
            format!("{{\"soft_filter\": {}}}", nested_not(depth)),
            format!("{{\"filter\": {{\"and\": [{}]}}}}", nested_or(depth)),
            deep_arrays.clone(),
            objects(depth),
        ];
        for key in [
            "q",
            "fields",
            "sort",
            "facets",
            "relax",
            "search_after",
            "limit",
            "unknown",
        ] {
            cases.push(format!("{{\"{key}\": {deep_arrays}}}"));
            cases.push(format!("{{\"{key}\": {}}}", objects(depth)));
        }
        cases.push(format!("{{\"relax\": {{\"levels\": {deep_arrays}}}}}"));
        cases.push(format!("{{\"sort\": [{{\"field\": {deep_arrays}}}]}}"));
        for json in cases {
            assert!(json.len() <= persia_engine::search::request::MAX_REQUEST_JSON_LEN);
            let got = kind_of(parse(&json));
            assert_ne!(got, "ok", "{depth}: {}", &json[..json.len().min(60)]);
        }
    }
}

// --- 5: huge numbers --------------------------------------------------------------------------------------------------

#[test]
fn huge_numbers_are_the_error_of_their_key() {
    let digits = "9".repeat(10_000);
    let cases: Vec<(String, &str)> = vec![
        // A count is a JSON integer that fits: not one past `u64::MAX`, not a thousand digits, not a float.
        (
            format!("{{\"limit\": {}}}", u128::from(u64::MAX) + 1),
            "json",
        ),
        (format!("{{\"offset\": {digits}}}"), "json"),
        (format!("{{\"limit\": -{digits}}}"), "json"),
        ("{\"limit\": 1e400}".into(), "json"),
        ("{\"offset\": 1e19}".into(), "json"),
        (format!("{{\"limit\": {}}}", i64::MIN), "json"),
        // A facet's size: an integer, and then held to the facet's bound.
        (
            format!(
                "{{\"facets\": [{{\"field\": \"brand\", \"size\": {}}}]}}",
                u64::MAX
            ),
            "request",
        ),
        (
            format!("{{\"facets\": [{{\"field\": \"brand\", \"size\": {digits}}}]}}"),
            "json",
        ),
        // `relax`'s numbers are `relax`'s errors.
        (
            format!("{{\"relax\": {{\"min_results\": {}}}}}", u64::MAX),
            "relax",
        ),
        (
            format!("{{\"relax\": {{\"min_results\": {digits}}}}}"),
            "json",
        ),
        (
            format!("{{\"relax\": {{\"budget_ms\": {}}}}}", u64::MAX),
            "relax",
        ),
        (
            format!(
                "{{\"relax\": {{\"min_results\": {}}}}}",
                u64::from(u32::MAX) + 1
            ),
            "relax",
        ),
        // A filter value out of its field's range is the filter's error, not a wrapped number.
        (
            format!(
                "{{\"filter\": {{\"term\": [\"stock\", {}]}}}}",
                u128::from(u64::MAX)
            ),
            "query",
        ),
        (
            format!(
                "{{\"filter\": {{\"term\": [\"stock\", {}]}}}}",
                i128::from(i64::MIN) - 1
            ),
            "query",
        ),
        (
            format!("{{\"filter\": {{\"term\": [\"stock\", {digits}]}}}}"),
            "json",
        ),
        (
            "{\"filter\": {\"range\": [\"price\", {\"lte\": 1e400}]}}".into(),
            "json",
        ),
        (
            "{\"filter\": {\"range\": [\"price\", {\"lte\": -1e400}]}}".into(),
            "json",
        ),
    ];
    for (json, want) in &cases {
        assert_eq!(
            kind_of(parse(json)),
            *want,
            "{}",
            &json[..json.len().min(80)]
        );
    }
    // At the edges of their types, values are read as they are.
    for json in [
        format!("{{\"filter\": {{\"term\": [\"stock\", {}]}}}}", i64::MIN),
        format!("{{\"filter\": {{\"term\": [\"stock\", {}]}}}}", i64::MAX),
        format!(
            "{{\"filter\": {{\"range\": [\"price\", {{\"gte\": {:e}, \"lte\": {:e}}}]}}}}",
            f64::MIN,
            f64::MAX
        ),
        "{\"relax\": {\"min_results\": 10000}}".to_owned(),
    ] {
        assert_eq!(kind_of(parse(&json)), "ok", "{json}");
    }
}

#[test]
fn a_window_at_usize_max_is_an_error_of_the_run_not_an_overflow() {
    let db = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    let max = usize::MAX;
    // Without a cursor and with one, for the plain search and the relaxed one.
    let token = hits(run(c, &json!({"q": "red", "relax": false, "limit": 1})).unwrap())[0]
        .1
        .to_token();
    for (offset, limit) in [
        (max, max),
        (max, 1),
        (1, max),
        (0, max),
        (max, 0),
        (5_000, 5_001),
    ] {
        for mut json in [
            json!({"q": "red", "relax": false}),
            json!({"q": "red"}),
            json!({"q": "red", "relax": false, "search_after": token}),
        ] {
            json["offset"] = json!(offset);
            json["limit"] = json!(limit);
            // It parses: the bound is the run's (SPEC §7.1.4).
            assert!(parse(&json.to_string()).is_ok(), "{json}");
            let err = run(c, &json).unwrap_err();
            assert!(
                matches!(
                    err,
                    QueryRequestError::Request(RequestError::Window { offset: o, limit: l, max: 10_000 })
                        if o == offset && l == limit
                ),
                "{json}: {err}"
            );
        }
    }
    // The bound itself is a page.
    for json in [
        json!({"q": "red", "relax": false, "offset": 5_000, "limit": 5_000}),
        json!({"q": "red", "offset": 0, "limit": 10_000}),
        json!({"q": "red", "relax": false, "offset": 9_999, "limit": 1, "search_after": token}),
    ] {
        run(c, &json).unwrap();
    }
}

// --- 6: a cursor and an offset ----------------------------------------------------------------------------------------

#[test]
fn an_offset_after_a_cursor_skips_hits_after_it() {
    let db = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    for request in [
        json!({"q": "red", "relax": false}),
        json!({"q": "red", "relax": false, "sort": [{"field": "price", "order": "desc"}]}),
        json!({"q": "red"}),
        json!({"q": "red shoes", "relax": {"min_results": 5}}),
        json!({"q": "red laces", "relax": {"min_results": 6, "fill": true}}),
    ] {
        let mut all = request.clone();
        all["limit"] = json!(100);
        let whole = hits(run(c, &all).unwrap());
        assert!(whole.len() >= 4, "{request}: {:?}", ids(&whole));
        let ranking = ids(&whole);
        for (i, (_, cursor)) in whole.iter().enumerate() {
            for offset in 0..=whole.len() {
                for limit in [1, 2, 3] {
                    let mut page = request.clone();
                    page["search_after"] = json!(cursor.to_token());
                    page["offset"] = json!(offset);
                    page["limit"] = json!(limit);
                    let got = hits(run(c, &page).unwrap());
                    let from = (i + 1 + offset).min(whole.len());
                    let to = (from + limit).min(whole.len());
                    assert_eq!(
                        ids(&got),
                        ranking[from..to],
                        "{request}: after {i}, offset {offset}, limit {limit}"
                    );
                }
            }
        }
    }
}

// --- 7: the fuzz seeds ------------------------------------------------------------------------------------------------

#[test]
fn the_fuzz_seeds_are_valid_requests_and_tokens() {
    // `fuzz/corpus/request_json`: each a request of the fuzz target's schema, which `schema()` repeats.
    let requests = [
        include_str!("../../../fuzz/corpus/request_json/all"),
        include_str!("../../../fuzz/corpus/request_json/plain"),
        include_str!("../../../fuzz/corpus/request_json/proximity"),
    ];
    let db = database();
    let snapshot = db.snapshot();
    let c = snapshot.collection("c").unwrap();
    for seed in requests {
        let request = parse(seed).unwrap_or_else(|e| panic!("{seed}: {e}"));
        // Each runs, or is refused by the run for its cursor of another snapshot: a seed reaches the search.
        match request.run(c, &options()) {
            Ok(_) | Err(QueryRequestError::Request(RequestError::CursorView { .. })) => {}
            Err(e) => panic!("{seed}: {e}"),
        }
    }
    // `fuzz/corpus/cursor_token`: each the canonical token of a cursor.
    for seed in [
        include_str!("../../../fuzz/corpus/cursor_token/full"),
        include_str!("../../../fuzz/corpus/cursor_token/plain"),
    ] {
        let cursor = Cursor::from_token(seed).unwrap_or_else(|e| panic!("{seed}: {e}"));
        assert_eq!(cursor.to_token(), seed);
    }
}

// --- the strict reader, at the level of JSON text ---------------------------------------------------------------------

#[test]
fn escaped_keys_are_the_keys_they_spell() {
    // `q` is `q`: read as the key, and repeated when `q` is there too.
    let escaped = parse(r#"{"q": "red"}"#).unwrap();
    assert_eq!(escaped.q(), "red");
    for json in [
        r#"{"q": "a", "q": "b"}"#,
        r#"{"limit": 1, "limit": 2}"#,
        r#"{"filter": {"term": ["brand", "a"], "term": ["brand", "b"]}}"#,
    ] {
        assert_eq!(kind_of(parse(json)), "json", "{json}");
    }
    // A key that only looks like one is unknown.
    for json in [
        r#"{"q\u0000": "a"}"#,
        r#"{"q ": "a"}"#,
        r#"{" q": "a"}"#,
        "{\"\u{ff51}\": \"a\"}",
        r#"{"limit\t": 1}"#,
    ] {
        assert_eq!(kind_of(parse(json)), "json", "{json}");
    }
    // Text that is no JSON string: a lone surrogate.
    assert_eq!(kind_of(parse(r#"{"q": "\ud800"}"#)), "json");
}
