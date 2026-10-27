//! Fuzz WAL replay (ROADMAP 6.2.5; SPEC §8.2.1, ADR-0028): arbitrary bytes as a chunk never panic, decoding or
//! replaying into the memtables of two fixed collections. A chunk whose records all decode re-encodes to exactly its
//! bytes (one encoding per chunk), and replaying it again into fresh memtables gives the same outcome. The first input
//! byte picks raw bytes (even) or bytes with every record CRC fixed (odd), which reach the bodies and documents.
#![no_main]

use std::collections::BTreeMap;
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use persia_analysis::AnalyzerRegistry;
use persia_engine::memtable::Memtable;
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_engine::wal::{Replay, WalOp, WalRecord, decode_chunk, encode_chunk};
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

    let mut first = memtables();
    let mut replay = Replay::new(&mut first);
    let outcome = replay.chunk(12_288, data).map_err(|e| e.to_string());
    let summary = replay.finish();
    let mut second = memtables();
    let mut again = Replay::new(&mut second);
    assert_eq!(
        again.chunk(12_288, data).map_err(|e| e.to_string()),
        outcome
    );
    assert_eq!(again.finish(), summary);
}
