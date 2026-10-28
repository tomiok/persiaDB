//! Fuzz WAL replay (ROADMAP 6.2.5; SPEC §8.2.1, ADR-0028): arbitrary bytes as a chunk never panic, decoding or
//! replaying into the memtables of two fixed collections. A chunk whose records all decode re-encodes to exactly its
//! bytes. Replay runs on every input: into fresh memtables twice, and again on top of the first, each time with the
//! same outcome and the same documents per id. The first input byte picks raw bytes (even) or bytes with every record
//! CRC fixed (odd), which reach the bodies and documents.
#![no_main]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use persia_analysis::AnalyzerRegistry;
use persia_engine::document::Document;
use persia_engine::memtable::{IdState, Memtable};
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::view::SearchView;
use persia_engine::wal::{Replay, ReplaySummary, WalOp, WalRecord, decode_chunk, encode_chunk};
use persia_format::checksum::crc32c;

static SCHEMAS: LazyLock<Vec<CollectionSchema>> = LazyLock::new(|| {
    let notes = SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .expect("valid schema");
    let tasks = SchemaBuilder::new("tasks")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("n", FieldKind::I64 { stored: true })
        .build()
        .expect("valid schema");
    vec![notes, tasks]
});

fn memtables() -> BTreeMap<String, Memtable> {
    let registry = AnalyzerRegistry::builtin();
    SCHEMAS
        .iter()
        .map(|s| {
            let memtable = Memtable::new(s, &registry).expect("builtin analyzers");
            (s.name().to_owned(), memtable)
        })
        .collect()
}

/// `data` with every record CRC recomputed, as far as the record lengths go: CRC32C is checked first, so random
/// bytes would otherwise never reach the record bodies and the documents.
fn fix_crcs(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    let mut at = 12usize;
    while let Some(len) = out
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize)
    {
        let Some(crc_at) = at.checked_add(4 + len).filter(|&c| c + 4 <= out.len()) else {
            break;
        };
        let crc = crc32c(&out[at..crc_at]);
        out[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
        at = crc_at + 4;
    }
    out
}

fuzz_target!(|input: &[u8]| {
    // The first byte picks raw bytes or fixed CRCs.
    let Some((&mode, rest)) = input.split_first() else {
        return;
    };
    let fixed;
    let data = if mode & 1 == 0 {
        rest
    } else {
        fixed = fix_crcs(rest);
        &fixed
    };
    check(data);
});

fn check(data: &[u8]) {
    round_trip(data);
    // Replay runs on every input, malformed ones included: its error path is fuzzed too.
    let ids = ids(data);
    let mut first = memtables();
    let outcome = replay(&mut first, data);
    let documents = state(&first, &ids);
    let mut fresh = memtables();
    assert_eq!(replay(&mut fresh, data), outcome);
    assert_eq!(state(&fresh, &ids), documents);
    // Again on top of the first replay: the same outcome and the same documents (SPEC §8.2.1).
    assert_eq!(replay(&mut first, data), outcome);
    assert_eq!(state(&first, &ids), documents);
}

/// A chunk whose records all decode re-encodes to exactly its bytes.
fn round_trip(data: &[u8]) {
    let Ok(records) = decode_chunk(data) else {
        return;
    };
    let Ok(records) = records.collect::<Result<Vec<_>, _>>() else {
        return;
    };
    // Rebuild each record through its constructor: the decoder accepts exactly what they accept.
    let rebuilt: Vec<WalRecord<'_>> = records
        .iter()
        .map(|r| match r.op() {
            WalOp::Put => WalRecord::put(r.collection(), r.payload()),
            WalOp::Delete => WalRecord::delete(r.collection(), r.deleted_id().expect("an id")),
        })
        .collect::<Result<_, _>>()
        .expect("a decoded record is a valid record");
    assert_eq!(rebuilt, records);
    assert_eq!(encode_chunk(&records).expect("encodes"), data);
}

/// The replay's outcome: its error as text, and what it applied.
fn replay(
    memtables: &mut BTreeMap<String, Memtable>,
    data: &[u8],
) -> (Result<(), String>, ReplaySummary) {
    let mut replay = Replay::new(memtables);
    let result = replay.chunk(12_288, data).map_err(|e| e.to_string());
    (result, replay.finish())
}

/// The (collection, id) of every record that decodes, up to the first that does not.
fn ids(data: &[u8]) -> BTreeSet<(String, String)> {
    let Ok(records) = decode_chunk(data) else {
        return BTreeSet::new();
    };
    records
        .map_while(Result::ok)
        .filter_map(|r| {
            let id = match r.op() {
                WalOp::Delete => r.deleted_id()?.to_owned(),
                WalOp::Put => {
                    let schema = SCHEMAS.iter().find(|s| s.name() == r.collection())?;
                    Document::decode(r.payload(), schema).ok()?.id().to_owned()
                }
            };
            Some((r.collection().to_owned(), id))
        })
        .collect()
}

/// What the memtables hold for each id: absent, deleted (`Some(None)`), or the live document.
fn state(
    memtables: &BTreeMap<String, Memtable>,
    ids: &BTreeSet<(String, String)>,
) -> Vec<Option<Option<Document>>> {
    ids.iter()
        .map(|(c, id)| {
            let view = memtables.get(c)?.view();
            Some(match view.lookup(id)? {
                IdState::Live(docid) => Some(view.document(docid).expect("a live document reads")),
                IdState::Deleted => None,
            })
        })
        .collect()
}
