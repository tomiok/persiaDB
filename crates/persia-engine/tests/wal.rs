//! The WAL (ROADMAP 6.2; SPEC §8.2.1, §8.6, ADR-0028): golden chunks, the chunk decoder's errors, record round-trips,
//! the writer's durability levels, rotation and `wal_start`, faults at every write, replay after truncating the
//! container at every byte, idempotent replay, and replay errors.
//!
//! Invariants:
//! 1. The golden chunks in `tests/fixtures/wal/v1/` decode to the records below, which encode to exactly those
//!    bytes. They are never regenerated once released (SPEC §15); `PERSIA_WRITE_WAL_FIXTURES=1` writes missing ones.
//! 2. Truncating a container at any byte after the committed `log_end` reopens it, and replay gives exactly the state
//!    of the operations in the chunks that survived whole: a prefix of the operations (ROADMAP 6.2.4.1). Truncating
//!    inside the committed range fails the open, never panics.
//! 3. Replaying twice, into fresh memtables or on top of the first replay, gives the same documents (SPEC §8.2.1).
//! 4. Decoding (SPEC §8.2.1): every strict prefix of a chunk and every single bit flip is `Corrupt`, except that a
//!    flip in the version field may be `UnsupportedVersion`; never a panic. Each `WalCorruption` is reported at the
//!    offset of the field that breaks the rule. A version above 1 is `UnsupportedVersion`, not corruption. After an
//!    error the record iterator yields nothing more.
//! 5. Records with valid collection names, put payloads and delete ids round-trip through `encode_chunk` and
//!    `decode_chunk` exactly, and re-encode to identical bytes. `WalRecord::put`/`delete` reject names that break the
//!    §6.2.1 rule, empty or oversized payloads and ids, with the right `RecordError`.
//! 6. Writer durability (SPEC §8.2.1): `Buffered` writes nothing; `Async` writes a chunk without syncing (a crash
//!    losing unsynced writes loses it); `Fsync` survives any crash; an `Fsync` batch takes exactly one sync, however
//!    many chunks it rotates into.
//! 7. Rotation: the writer closes a chunk as soon as it reaches the target size and never later; chunks hold the
//!    records in append order, and replay applies them in that order.
//! 8. `wal_start` is the oldest written chunk holding a record of a collection not flushed since, else the end of
//!    the log; it never moves back. `WalWriter::resume` with `ReplaySummary::pending` keeps it there after a reopen.
//! 9. Faults (SPEC §4.5): a fault at any mutating call of an `Fsync` workload fails that call with
//!    `WalWriteError::Log` and every later one with `Poisoned`; after any crash, reopening and replaying gives the
//!    state of a prefix of the operations that includes every acknowledged one.
//! 10. Replay errors (SPEC §8.2.1): a record of a collection the memtables lack is `UnknownCollection` (corruption),
//!     a put that does not decode with its schema is `Document`, a bad chunk is `Chunk`, each at the record's
//!     offset; records before the error stay applied.
//! 11. A flush covers only the records before its `WalMark`; a commit that takes `wal_start` from `commit_point` keeps
//!     buffered records of a collection it drops out of replay (SPEC §8.2.1).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    missing_docs
)] // Tests: inputs are generated or fixed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use persia_analysis::AnalyzerRegistry;
use persia_engine::document::{Document, FieldSet, MAX_DOCUMENT_LEN, MAX_ID_LEN, Value};
use persia_engine::memtable::{IdState, Memtable};
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, MAX_NAME_LEN, NameProblem, SchemaBuilder,
    TextOptions,
};
use persia_engine::view::SearchView;
use persia_engine::wal::{
    ChunkFull, DEFAULT_CHUNK_TARGET, Durability, LogHoldsChunks, MAX_CHUNK_LEN, RecordError,
    Replay, ReplayError, WalCorruption, WalError, WalOp, WalRecord, WalWriteError, WalWriter,
    decode_chunk, encode_chunk, replay_container,
};
use persia_format::block::Compression;
use persia_format::checksum::crc32c;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Commit, Container, Error as ContainerError, Mode};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

fn notes() -> CollectionSchema {
    SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

fn tasks() -> CollectionSchema {
    SchemaBuilder::new("tasks")
        .field("title", FieldKind::Text(TextOptions::default()))
        .field("n", FieldKind::I64 { stored: true })
        .build()
        .unwrap()
}

fn schemas() -> BTreeMap<String, CollectionSchema> {
    [notes(), tasks()]
        .into_iter()
        .map(|s| (s.name().to_owned(), s))
        .collect()
}

fn memtables() -> BTreeMap<String, Memtable> {
    let registry = AnalyzerRegistry::builtin();
    schemas()
        .into_iter()
        .map(|(name, s)| (name, Memtable::new(&s, &registry).unwrap()))
        .collect()
}

/// One operation as the test's reference model sees it.
#[derive(Debug, Clone)]
enum Op {
    Put(&'static str, Document),
    Delete(&'static str, String),
}

impl Op {
    fn collection(&self) -> &'static str {
        match self {
            Self::Put(c, _) | Self::Delete(c, _) => c,
        }
    }
}

fn note(id: &str, body: &str, tag: &str) -> Op {
    let s = notes();
    let field = |n| s.field(n).unwrap().id();
    Op::Put(
        "notes",
        Document::new(id)
            .with(field("body"), Value::String(body.into()))
            .with(field("tag"), Value::String(tag.into())),
    )
}

fn task(id: &str, title: &str, n: i64) -> Op {
    let s = tasks();
    let field = |f| s.field(f).unwrap().id();
    Op::Put(
        "tasks",
        Document::new(id)
            .with(field("title"), Value::String(title.into()))
            .with(field("n"), Value::I64(n)),
    )
}

fn delete(collection: &'static str, id: &str) -> Op {
    Op::Delete(collection, id.to_owned())
}

/// An op's record, owned: its op, collection and payload.
struct Owned(WalOp, &'static str, Vec<u8>);

impl Owned {
    fn of(op: &Op) -> Self {
        match op {
            Op::Put(c, doc) => Self(
                WalOp::Put,
                c,
                doc.encode(&schemas()[*c], FieldSet::All).unwrap(),
            ),
            Op::Delete(c, id) => Self(WalOp::Delete, c, id.as_bytes().to_vec()),
        }
    }

    fn record(&self) -> WalRecord<'_> {
        match self.0 {
            WalOp::Put => WalRecord::put(self.1, &self.2).unwrap(),
            WalOp::Delete => {
                WalRecord::delete(self.1, std::str::from_utf8(&self.2).unwrap()).unwrap()
            }
        }
    }
}

fn owned(ops: &[Op]) -> Vec<Owned> {
    ops.iter().map(Owned::of).collect()
}

/// The documents after `ops`, by collection and id: `Some` for a live one, `None` for a deleted one.
type State = BTreeMap<(String, String), Option<Document>>;

fn reference(ops: &[Op]) -> State {
    let mut state = State::new();
    for op in ops {
        match op {
            Op::Put(c, doc) => {
                state.insert(((*c).to_owned(), doc.id().to_owned()), Some(doc.clone()))
            }
            Op::Delete(c, id) => state.insert(((*c).to_owned(), id.clone()), None),
        };
    }
    state
}

/// What the memtables hold for every id in `universe`.
fn observed(memtables: &BTreeMap<String, Memtable>, universe: &State) -> State {
    let mut state = State::new();
    for (c, id) in universe.keys() {
        let view = memtables[c].view();
        match view.lookup(id) {
            Some(IdState::Live(docid)) => {
                state.insert((c.clone(), id.clone()), Some(view.document(docid).unwrap()));
            }
            Some(IdState::Deleted) => {
                state.insert((c.clone(), id.clone()), None);
            }
            None => {}
        }
    }
    state
}

// ---- Golden fixtures (invariant 1) ----

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/wal/v1")
}

fn goldens() -> Vec<(&'static str, Vec<Op>)> {
    vec![
        ("delete.wal", vec![delete("notes", "n1")]),
        (
            "mixed.wal",
            vec![
                note("n1", "hello world", "x"),
                task("t1", "write the wal", 7),
                delete("notes", "n0"),
                note("n1", "hello again", "y"),
            ],
        ),
    ]
}

fn encode_ops(ops: &[Op]) -> Vec<u8> {
    let owned = owned(ops);
    let records: Vec<_> = owned.iter().map(Owned::record).collect();
    encode_chunk(&records).unwrap()
}

#[test]
fn golden_fixtures_decode_reencode_and_replay() {
    let write = std::env::var_os("PERSIA_WRITE_WAL_FIXTURES").is_some();
    for (name, ops) in goldens() {
        let path = fixture_dir().join(name);
        let encoded = encode_ops(&ops);
        if write && !path.exists() {
            std::fs::create_dir_all(fixture_dir()).unwrap();
            std::fs::write(&path, &encoded).unwrap();
        }
        let stored = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (PERSIA_WRITE_WAL_FIXTURES=1 writes it)",
                path.display()
            )
        });
        assert_eq!(encoded, stored, "{name}: encoding changed");
        let records: Vec<_> = decode_chunk(&stored)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(records.len(), ops.len(), "{name}");
        for (record, op) in records.iter().zip(&ops) {
            assert_eq!(record.collection(), op.collection(), "{name}");
        }
        let mut mem = memtables();
        let mut replay = Replay::new(&mut mem);
        replay.chunk(12_288, &stored).unwrap();
        assert_eq!(replay.finish().records, ops.len() as u64);
        let expected = reference(&ops);
        assert_eq!(observed(&mem, &expected), expected, "{name}");
    }
}

#[test]
fn delete_golden_bytes() {
    let mut body = vec![2, 5];
    body.extend_from_slice(b"notes");
    body.extend_from_slice(b"n1");
    let mut record = (body.len() as u32).to_le_bytes().to_vec();
    record.extend_from_slice(&body);
    let crc = crc32c(&record);
    record.extend_from_slice(&crc.to_le_bytes());
    let mut expected = b"PWAL".to_vec();
    expected.extend_from_slice(&1u32.to_le_bytes()); // version
    expected.extend_from_slice(&1u32.to_le_bytes()); // record count
    expected.extend_from_slice(&record);
    assert_eq!(encode_ops(&[delete("notes", "n1")]), expected);
}

// ---- Truncation at every byte (invariant 2, ROADMAP 6.2.4.1) ----

/// A history over two collections: upserts, deletes of present and absent ids, and a batch in one chunk.
fn history() -> Vec<Vec<Op>> {
    vec![
        vec![note("n1", "first note", "x")],
        vec![task("t1", "first task", 1)],
        vec![
            note("n2", "second note", "y"),
            delete("tasks", "t9"),
            task("t2", "batch", 2),
        ],
        vec![note("n1", "first note edited", "z")],
        vec![delete("notes", "n2")],
        vec![task("t1", "first task again", 3)],
        vec![delete("tasks", "t2"), note("n3", "third", "x")],
        vec![note("n2", "second note is back", "x")],
    ]
}

/// Writes `history` as one chunk per group, committing after the first `committed` groups with `wal_start` at the
/// first chunk (nothing flushed). Returns the file and, per group, the end of its chunk.
fn write_history(history: &[Vec<Op>], committed: usize) -> (SimFile, Vec<u64>, u64) {
    let file = SimFile::new();
    let mut wal = WalWriter::new(Container::create_on(file.clone(), [7; 16]).unwrap()).unwrap();
    let mut ends = Vec::new();
    let mut log_end = wal.log().end();
    for (i, group) in history.iter().enumerate() {
        let owned = owned(group);
        let records: Vec<_> = owned.iter().map(Owned::record).collect();
        wal.append_batch(&records, Durability::Fsync).unwrap();
        ends.push(wal.log().end());
        if i + 1 == committed {
            let wal_start = wal.commit_point().unwrap();
            let manifest = wal
                .log_mut()
                .append(FrameKind::Manifest, b"manifest", Compression::None)
                .unwrap();
            wal.log_mut()
                .commit(Commit {
                    manifest,
                    wal_start,
                    fence_token: 0,
                })
                .unwrap();
            log_end = wal.log().end();
        }
    }
    (file, ends, log_end)
}

#[test]
fn truncating_at_every_byte_recovers_a_prefix_of_the_operations() {
    let history = history();
    let committed = 3;
    let (file, ends, log_end) = write_history(&history, committed);
    let bytes = file.bytes();
    let universe = reference(&history.concat());
    let mut recovered_prefixes = std::collections::BTreeSet::new();
    for cut in 0..=bytes.len() {
        let truncated = SimFile::from_bytes(bytes[..cut].to_vec());
        let opened = Container::open_on(truncated, Mode::ReadWrite);
        if (cut as u64) < log_end {
            assert!(
                opened.is_err(),
                "cut {cut} inside the committed range opened"
            );
            continue;
        }
        let container = opened.unwrap_or_else(|e| panic!("cut {cut}: {e}"));
        let mut mem = memtables();
        replay_container(&container, &mut mem).unwrap_or_else(|e| panic!("cut {cut}: {e}"));
        // Group `i`'s chunk survives exactly when the cut keeps all of it; the commit keeps its frames anyway.
        let groups = ends
            .iter()
            .filter(|&&end| end <= cut as u64)
            .count()
            .max(committed);
        let expected = reference(&history[..groups].concat());
        assert_eq!(
            observed(&mem, &universe),
            expected,
            "cut {cut}: {groups} groups"
        );
        recovered_prefixes.insert(groups);
    }
    // Every prefix from the committed one to the whole history was seen.
    assert_eq!(
        recovered_prefixes.into_iter().collect::<Vec<_>>(),
        (committed..=history.len()).collect::<Vec<_>>()
    );
}

// ---- Idempotence (invariant 3) ----

#[test]
fn replay_is_idempotent() {
    let history = history();
    let (file, _, _) = write_history(&history, 2);
    let container = Container::open_on(file.crash(CrashLoss::All), Mode::ReadOnly).unwrap();
    let universe = reference(&history.concat());

    let mut once = memtables();
    let first = replay_container(&container, &mut once).unwrap();
    let mut fresh = memtables();
    assert_eq!(replay_container(&container, &mut fresh).unwrap(), first);
    assert_eq!(observed(&fresh, &universe), observed(&once, &universe));

    // Replaying on top of the first replay, as when chunks hold records already flushed, changes no document.
    replay_container(&container, &mut once).unwrap();
    assert_eq!(observed(&once, &universe), universe);
    // Each collection is pending from the first chunk holding one of its records: groups 0 and 1.
    let chunk = |i: usize| container.frames()[i].offset;
    assert_eq!(
        first.pending,
        BTreeMap::from([("notes".into(), chunk(0)), ("tasks".into(), chunk(1))])
    );
}

// ---- Shared helpers for invariants 4–10 ----

/// The fixed proptest seed; `PROPTEST_RNG_SEED` overrides it. A failure prints the seed it ran with.
const PROPTEST_SEED: u64 = 0x5741_4C06_2000;

fn proptest_config(default_cases: u32) -> ProptestConfig {
    let seed = std::env::var("PROPTEST_RNG_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(PROPTEST_SEED);
    ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(default_cases),
        rng_seed: RngSeed::Fixed(seed),
        ..ProptestConfig::default()
    }
}

/// Decodes a whole chunk: its header, then every record.
fn decode_all(bytes: &[u8]) -> Result<Vec<WalRecord<'_>>, WalError> {
    decode_chunk(bytes)?.collect()
}

/// The corruption decoding `bytes` reports; panics on success or an unsupported version.
fn corruption(bytes: &[u8]) -> (u64, WalCorruption) {
    match decode_all(bytes) {
        Err(WalError::Corrupt { offset, reason }) => (offset, reason),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

/// A record with a valid CRC around `body`, whatever the body holds.
fn raw_body(body: &[u8]) -> Vec<u8> {
    let mut record = (body.len() as u32).to_le_bytes().to_vec();
    record.extend_from_slice(body);
    let crc = crc32c(&record);
    record.extend_from_slice(&crc.to_le_bytes());
    record
}

/// A record with a valid CRC: `op`, the name's length as a byte, the name, the payload.
fn raw_record(op: u8, name: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut body = vec![op, name.len() as u8];
    body.extend_from_slice(name);
    body.extend_from_slice(payload);
    raw_body(&body)
}

fn raw_chunk(version: u32, count: u32, records: &[&[u8]]) -> Vec<u8> {
    let mut chunk = b"PWAL".to_vec();
    chunk.extend_from_slice(&version.to_le_bytes());
    chunk.extend_from_slice(&count.to_le_bytes());
    for r in records {
        chunk.extend_from_slice(r);
    }
    chunk
}

fn mixed_golden() -> Vec<u8> {
    encode_ops(&goldens()[1].1)
}

// ---- Decoder errors (invariant 4) ----

#[test]
fn every_strict_prefix_of_a_chunk_is_corrupt() {
    for (name, ops) in goldens() {
        let chunk = encode_ops(&ops);
        for cut in 0..chunk.len() {
            match decode_all(&chunk[..cut]) {
                Err(WalError::Corrupt { offset, .. }) => {
                    assert!(offset <= cut as u64, "{name} cut {cut}: offset {offset}");
                }
                other => panic!("{name} cut {cut}: {other:?}"),
            }
        }
    }
}

#[test]
fn every_single_bit_flip_is_detected() {
    let chunk = mixed_golden();
    for bit in 0..chunk.len() * 8 {
        let mut flipped = chunk.clone();
        flipped[bit / 8] ^= 1 << (bit % 8);
        let in_version = (4..8).contains(&(bit / 8));
        match decode_all(&flipped) {
            Err(WalError::Corrupt { .. }) => {}
            Err(WalError::UnsupportedVersion { .. }) if in_version => {}
            other => panic!("bit {bit} (byte {}): {other:?}", bit / 8),
        }
    }
}

#[test]
fn each_corruption_is_reported_at_its_offset() {
    let good = raw_record(2, b"notes", b"n1"); // 4 + 9 + 4 = 17 bytes
    let ok = raw_chunk(1, 1, &[&good]);
    assert_eq!(decode_all(&ok).unwrap().len(), 1, "the base chunk is valid");

    let mut bad_magic = ok.clone();
    bad_magic[0] = b'X';
    let mut bad_crc = ok.clone();
    let crc_at = 12 + 4 + 9;
    bad_crc[crc_at] ^= 1;
    let stored = u32::from_le_bytes(bad_crc[crc_at..crc_at + 4].try_into().unwrap());
    let computed = crc32c(&bad_crc[12..crc_at]);
    let mut trailing = ok.clone();
    trailing.push(0);
    let huge_body = {
        let mut r = good.clone();
        r[..4].copy_from_slice(&1000u32.to_le_bytes());
        r
    };
    let id_too_long = "x".repeat(MAX_ID_LEN + 1);
    let doc_too_long = vec![0u8; MAX_DOCUMENT_LEN + 1];

    // Body fields: op at 16, name_len at 17, name at 18, payload at 18 + name_len.
    let cases: Vec<(&str, Vec<u8>, u64, WalCorruption)> = vec![
        ("magic", bad_magic, 0, WalCorruption::Magic),
        (
            "zero version",
            raw_chunk(0, 1, &[&good]),
            4,
            WalCorruption::ZeroVersion,
        ),
        (
            "no records",
            raw_chunk(1, 0, &[]),
            8,
            WalCorruption::NoRecords,
        ),
        ("empty input", Vec::new(), 0, WalCorruption::UnexpectedEof),
        (
            "header without count",
            ok[..10].to_vec(),
            10,
            WalCorruption::UnexpectedEof,
        ),
        (
            "missing record",
            raw_chunk(1, 2, &[&good]),
            ok.len() as u64,
            WalCorruption::UnexpectedEof,
        ),
        (
            "body past the input",
            raw_chunk(1, 1, &[&huge_body]),
            (12 + huge_body.len()) as u64,
            WalCorruption::UnexpectedEof,
        ),
        (
            "trailing bytes",
            trailing,
            ok.len() as u64,
            WalCorruption::TrailingBytes,
        ),
        (
            "body too short",
            raw_chunk(1, 1, &[&raw_body(&[2, 0])]),
            12,
            WalCorruption::BodyLen { len: 2 },
        ),
        (
            "empty body",
            raw_chunk(1, 1, &[&raw_body(&[])]),
            12,
            WalCorruption::BodyLen { len: 0 },
        ),
        (
            "checksum",
            bad_crc,
            crc_at as u64,
            WalCorruption::ChecksumMismatch { stored, computed },
        ),
        (
            "op 0",
            raw_chunk(1, 1, &[&raw_record(0, b"notes", b"n1")]),
            16,
            WalCorruption::Op { op: 0 },
        ),
        (
            "op 3",
            raw_chunk(1, 1, &[&raw_record(3, b"notes", b"n1")]),
            16,
            WalCorruption::Op { op: 3 },
        ),
        (
            "uppercase name",
            raw_chunk(1, 1, &[&raw_record(2, b"Notes", b"n1")]),
            17,
            WalCorruption::CollectionName,
        ),
        (
            "empty name",
            raw_chunk(1, 1, &[&raw_record(2, b"", b"n1")]),
            17,
            WalCorruption::CollectionName,
        ),
        (
            "name too long",
            raw_chunk(1, 1, &[&raw_record(2, &[b'a'; MAX_NAME_LEN + 1], b"n1")]),
            17,
            WalCorruption::CollectionName,
        ),
        (
            "name not UTF-8",
            raw_chunk(1, 1, &[&raw_record(2, &[b'a', 0xFF], b"n1")]),
            17,
            WalCorruption::CollectionName,
        ),
        (
            "name past the body",
            raw_chunk(1, 1, &[&raw_body(&[2, 50, b'a'])]),
            17,
            WalCorruption::CollectionName,
        ),
        (
            "empty document",
            raw_chunk(1, 1, &[&raw_record(1, b"notes", b"")]),
            23,
            WalCorruption::DocumentLen { len: 0 },
        ),
        (
            "document too long",
            raw_chunk(1, 1, &[&raw_record(1, b"notes", &doc_too_long)]),
            23,
            WalCorruption::DocumentLen {
                len: MAX_DOCUMENT_LEN + 1,
            },
        ),
        (
            "empty id",
            raw_chunk(1, 1, &[&raw_record(2, b"notes", b"")]),
            23,
            WalCorruption::Id,
        ),
        (
            "id too long",
            raw_chunk(1, 1, &[&raw_record(2, b"notes", id_too_long.as_bytes())]),
            23,
            WalCorruption::Id,
        ),
        (
            "id not UTF-8",
            raw_chunk(1, 1, &[&raw_record(2, b"notes", &[b'n', 0xC3])]),
            23,
            WalCorruption::Id,
        ),
        (
            "second record at its offset",
            raw_chunk(1, 2, &[&good, &raw_record(9, b"notes", b"n2")]),
            (12 + good.len() + 4) as u64,
            WalCorruption::Op { op: 9 },
        ),
    ];
    for (name, bytes, offset, reason) in cases {
        let got = decode_all(&bytes);
        assert_eq!(
            got,
            Err(WalError::Corrupt {
                offset,
                reason: reason.clone()
            }),
            "{name}"
        );
        assert!(got.unwrap_err().is_corruption(), "{name}");
    }

    // Too large: checked before anything else, so the bytes need not even be a chunk.
    let mut too_large = vec![0u8; MAX_CHUNK_LEN + 1];
    too_large[..4].copy_from_slice(b"PWAL");
    assert_eq!(
        corruption(&too_large),
        (
            0,
            WalCorruption::TooLarge {
                len: MAX_CHUNK_LEN + 1
            }
        )
    );
}

#[test]
fn an_empty_chunk_cannot_be_encoded() {
    // SPEC §8.2.1: record_count >= 1.
    assert_eq!(encode_chunk(&[]), Err(ChunkFull));
}

#[test]
fn ids_and_payloads_at_their_limits_decode() {
    let id = "é".repeat(MAX_ID_LEN / 2); // 512 bytes of two-byte characters
    let name = "a".repeat(MAX_NAME_LEN);
    let doc = vec![7u8; MAX_DOCUMENT_LEN];
    let records = [
        WalRecord::delete(&name, &id).unwrap(),
        WalRecord::put("n", &doc).unwrap(),
    ];
    let chunk = encode_chunk(&records).unwrap();
    assert_eq!(decode_all(&chunk).unwrap(), records);
}

#[test]
fn newer_versions_are_unsupported_not_corrupt() {
    let good = raw_record(2, b"notes", b"n1");
    for version in [2, 3, u32::MAX] {
        let chunk = raw_chunk(version, 1, &[&good]);
        let got = decode_all(&chunk);
        assert_eq!(got, Err(WalError::UnsupportedVersion { version }));
        assert!(!got.unwrap_err().is_corruption());
    }
}

#[test]
fn the_record_iterator_ends_after_an_error() {
    let good = raw_record(2, b"notes", b"n1");
    let mut bad = raw_record(2, b"notes", b"n2");
    let last = bad.len() - 1;
    bad[last] ^= 0x80; // CRC
    let third = raw_record(2, b"notes", b"n3");
    let cases = [
        (
            "bad second record",
            raw_chunk(1, 3, &[&good, &bad, &third]),
            1,
        ),
        (
            "trailing bytes",
            {
                let mut c = raw_chunk(1, 1, &[&good]);
                c.extend_from_slice(&third);
                c
            },
            1,
        ),
        ("missing record", raw_chunk(1, 3, &[&good, &third]), 2),
    ];
    for (name, chunk, oks) in cases {
        let mut records = decode_chunk(&chunk).unwrap();
        for i in 0..oks {
            assert!(matches!(records.next(), Some(Ok(_))), "{name}: record {i}");
        }
        assert!(matches!(records.next(), Some(Err(_))), "{name}");
        for _ in 0..3 {
            assert!(records.next().is_none(), "{name}: yielded after an error");
        }
    }
    // A valid chunk ends once, and stays ended.
    let chunk = raw_chunk(1, 1, &[&good]);
    let mut records = decode_chunk(&chunk).unwrap();
    assert!(matches!(records.next(), Some(Ok(_))));
    assert_eq!(records.offset(), chunk.len() as u64);
    assert!(records.next().is_none());
    assert!(records.next().is_none());
}

// ---- Record constructors and round-trips (invariant 5) ----

#[test]
fn record_constructors_reject_bad_input() {
    let invalid = NameProblem::InvalidChars {
        pattern: "[a-z][a-z0-9_-]*",
    };
    let long_name = "a".repeat(MAX_NAME_LEN + 1);
    let name_cases: Vec<(&str, NameProblem)> = vec![
        ("", NameProblem::Empty),
        (
            &long_name,
            NameProblem::TooLong {
                len: MAX_NAME_LEN + 1,
            },
        ),
        ("Notes", invalid),
        ("1notes", invalid),
        ("_notes", invalid),
        ("-notes", invalid),
        ("no tes", invalid),
        ("notés", invalid),
        ("notes.x", invalid),
    ];
    for (name, problem) in name_cases {
        let expected = RecordError::CollectionName {
            name: name.to_owned(),
            problem,
        };
        assert_eq!(
            WalRecord::put(name, b"x"),
            Err(expected.clone()),
            "{name:?}"
        );
        assert_eq!(WalRecord::delete(name, "x"), Err(expected), "{name:?}");
    }
    for name in ["a", "notes", "a-b_c9", &"z".repeat(MAX_NAME_LEN)] {
        assert!(WalRecord::put(name, b"x").is_ok(), "{name:?}");
        assert!(WalRecord::delete(name, "x").is_ok(), "{name:?}");
    }

    assert_eq!(
        WalRecord::put("notes", b""),
        Err(RecordError::DocumentLen { len: 0 })
    );
    let too_long = vec![0u8; MAX_DOCUMENT_LEN + 1];
    assert_eq!(
        WalRecord::put("notes", &too_long),
        Err(RecordError::DocumentLen {
            len: MAX_DOCUMENT_LEN + 1
        })
    );
    assert!(WalRecord::put("notes", &too_long[1..]).is_ok());

    assert_eq!(
        WalRecord::delete("notes", ""),
        Err(RecordError::IdLen { len: 0 })
    );
    let id = "x".repeat(MAX_ID_LEN + 1);
    assert_eq!(
        WalRecord::delete("notes", &id),
        Err(RecordError::IdLen {
            len: MAX_ID_LEN + 1
        })
    );
    assert!(WalRecord::delete("notes", &"x".repeat(MAX_ID_LEN)).is_ok());
    // Bytes, not characters: 257 two-byte characters are 514 bytes.
    assert_eq!(
        WalRecord::delete("notes", &"é".repeat(257)),
        Err(RecordError::IdLen { len: 514 })
    );

    let delete = WalRecord::delete("notes", "n1").unwrap();
    assert_eq!(
        (
            delete.op(),
            delete.collection(),
            delete.payload(),
            delete.deleted_id()
        ),
        (WalOp::Delete, "notes", &b"n1"[..], Some("n1"))
    );
    let put = WalRecord::put("notes", b"doc").unwrap();
    assert_eq!(
        (put.op(), put.collection(), put.payload(), put.deleted_id()),
        (WalOp::Put, "notes", &b"doc"[..], None)
    );
}

/// A generated record, owned.
#[derive(Debug, Clone)]
struct GenRecord(WalOp, String, Vec<u8>);

impl GenRecord {
    fn record(&self) -> WalRecord<'_> {
        match self.0 {
            WalOp::Put => WalRecord::put(&self.1, &self.2).unwrap(),
            WalOp::Delete => {
                WalRecord::delete(&self.1, std::str::from_utf8(&self.2).unwrap()).unwrap()
            }
        }
    }
}

/// Collection names by the §6.2.1 rule.
fn collection_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_-]{0,63}"
}

/// Delete ids: 1..=512 bytes of UTF-8, any characters, up to the limit.
fn document_id() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::collection::vec(any::<char>(), 1..=128).prop_map(|cs| cs.into_iter().collect()),
        (1..=MAX_ID_LEN).prop_map(|n| "i".repeat(n)),
    ]
}

fn gen_record(max_payload: usize) -> impl Strategy<Value = GenRecord> {
    prop_oneof![
        (
            collection_name(),
            prop::collection::vec(any::<u8>(), 1..=max_payload)
        )
            .prop_map(|(n, p)| GenRecord(WalOp::Put, n, p)),
        (collection_name(), document_id()).prop_map(|(n, id)| GenRecord(
            WalOp::Delete,
            n,
            id.into_bytes()
        )),
    ]
}

/// The §6.2.1 collection rule, written out independently of the engine.
fn is_collection_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_NAME_LEN
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

proptest! {
    #![proptest_config(proptest_config(128))]

    /// Invariant 5: records round-trip exactly, and the decoded records re-encode to identical bytes.
    #[test]
    fn records_round_trip_and_reencode_identically(
        records_in in prop::collection::vec(gen_record(512), 1..24),
    ) {
        let records: Vec<_> = records_in.iter().map(GenRecord::record).collect();
        let chunk = encode_chunk(&records).unwrap();
        let decoded = decode_all(&chunk).unwrap();
        prop_assert_eq!(&decoded, &records);
        for (d, g) in decoded.iter().zip(&records_in) {
            prop_assert_eq!(d.deleted_id().is_some(), g.0 == WalOp::Delete);
        }
        prop_assert_eq!(encode_chunk(&decoded).unwrap(), chunk);
    }

    /// Invariant 4 over generated chunks: every strict prefix and sampled bit flips are detected.
    #[test]
    fn generated_chunks_detect_truncation_and_bit_flips(
        records_in in prop::collection::vec(gen_record(48), 1..5),
        flips in prop::collection::vec(any::<prop::sample::Index>(), 64),
    ) {
        let records: Vec<_> = records_in.iter().map(GenRecord::record).collect();
        let chunk = encode_chunk(&records).unwrap();
        for cut in 0..chunk.len() {
            let got = decode_all(&chunk[..cut]);
            prop_assert!(matches!(got, Err(WalError::Corrupt { .. })), "cut {}: {:?}", cut, got);
        }
        for flip in flips {
            let bit = flip.index(chunk.len() * 8);
            let mut flipped = chunk.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            let got = decode_all(&flipped);
            let ok = match got {
                Err(WalError::Corrupt { .. }) => true,
                Err(WalError::UnsupportedVersion { .. }) => (4..8).contains(&(bit / 8)),
                Ok(_) => false,
            };
            prop_assert!(ok, "bit {}: {:?}", bit, got);
        }
    }

    /// Invariant 5: the constructors accept exactly the names of the §6.2.1 rule.
    #[test]
    fn constructors_accept_exactly_valid_names(name in "[a-zA-Z0-9_. é-]{0,70}|[a-z][a-z0-9_-]{0,70}") {
        let valid = is_collection_name(&name);
        prop_assert_eq!(WalRecord::put(&name, b"x").is_ok(), valid);
        prop_assert_eq!(WalRecord::delete(&name, "x").is_ok(), valid);
    }
}

// ---- Writer durability (invariant 6) ----

type Wal = WalWriter<Container<SimFile>>;

fn new_wal() -> (SimFile, Wal) {
    let file = SimFile::new();
    let wal = WalWriter::new(Container::create_on(file.clone(), [7; 16]).unwrap()).unwrap();
    (file, wal)
}

/// The WAL chunks of `container`, from the committed `wal_start` on: position and payload.
fn chunks(container: &Container<SimFile>) -> Vec<(u64, Vec<u8>)> {
    container
        .frames()
        .iter()
        .filter(|f| f.kind == FrameKind::WalChunk)
        .map(|f| {
            (
                f.offset,
                container
                    .read_frame(f.offset, MAX_CHUNK_LEN)
                    .unwrap()
                    .payload,
            )
        })
        .collect()
}

/// Replays the file left by a crash with `loss`, after a read-write open.
fn replay_after(file: &SimFile, loss: CrashLoss, universe: &State) -> State {
    let container = Container::open_on(file.crash(loss), Mode::ReadWrite).unwrap();
    let mut mem = memtables();
    replay_container(&container, &mut mem).unwrap();
    observed(&mem, universe)
}

fn append_ops(wal: &mut Wal, ops: &[Op], durability: Durability) {
    let owned = owned(ops);
    let records: Vec<_> = owned.iter().map(Owned::record).collect();
    wal.append_batch(&records, durability).unwrap();
}

#[test]
fn buffered_writes_nothing_and_a_crash_loses_it() {
    let (file, mut wal) = new_wal();
    let ops = [note("n1", "buffered", "x"), delete("tasks", "t1")];
    let (calls, bytes, end) = (file.mutations(), file.bytes(), wal.log().end());
    append_ops(&mut wal, &ops, Durability::Buffered);
    assert_eq!(file.mutations(), calls, "Buffered made a file call");
    assert_eq!(file.bytes(), bytes);
    assert_eq!(wal.log().end(), end);
    assert_eq!(chunks(wal.log()), Vec::<(u64, Vec<u8>)>::new());
    assert!(wal.buffered_len() > 0);
    assert_eq!(wal.wal_start(), end);
    drop(wal);
    let universe = reference(&ops);
    assert_eq!(
        replay_after(&file, CrashLoss::Nothing, &universe),
        State::new()
    );
}

#[test]
fn async_writes_a_chunk_without_syncing() {
    let (file, mut wal) = new_wal();
    let ops = [note("n1", "async", "x"), delete("tasks", "t1")];
    let calls = file.mutations();
    append_ops(&mut wal, &ops, Durability::Async);
    assert_eq!(file.mutations(), calls + 1, "one write, no sync");
    assert_eq!(chunks(wal.log()).len(), 1);
    assert_eq!(wal.buffered_len(), 0);
    assert!(wal.log().has_unsynced());
    assert_ne!(file.bytes(), file.durable_bytes());
    drop(wal);
    let universe = reference(&ops);
    assert_eq!(replay_after(&file, CrashLoss::All, &universe), State::new());
    assert_eq!(replay_after(&file, CrashLoss::Nothing, &universe), universe);
}

#[test]
fn fsync_survives_any_crash_and_covers_earlier_buffered_records() {
    let (file, mut wal) = new_wal();
    let ops = [
        note("n1", "buffered", "x"),
        task("t1", "async", 1),
        delete("notes", "n0"),
        note("n2", "fsync", "y"),
    ];
    append_ops(&mut wal, &ops[..1], Durability::Buffered);
    append_ops(&mut wal, &ops[1..2], Durability::Async);
    append_ops(&mut wal, &ops[2..], Durability::Fsync);
    assert_eq!(file.bytes(), file.durable_bytes());
    assert!(!wal.log().has_unsynced());
    // A sync with nothing new is free.
    let calls = file.mutations();
    wal.sync().unwrap();
    append_ops(&mut wal, &[], Durability::Fsync);
    assert_eq!(file.mutations(), calls);
    drop(wal);
    let universe = reference(&ops);
    for loss in [CrashLoss::All, CrashLoss::Nothing, CrashLoss::Seeded(1)] {
        assert_eq!(replay_after(&file, loss, &universe), universe, "{loss:?}");
    }
}

#[test]
fn an_fsync_batch_takes_one_sync_however_many_chunks() {
    for target in [1, 64, 200, DEFAULT_CHUNK_TARGET] {
        let (file, wal) = new_wal();
        let mut wal = wal.with_chunk_target(target);
        let ops: Vec<Op> = (0..12)
            .map(|i| note(&format!("n{i}"), "a note of some words", "x"))
            .collect();
        let calls = file.mutations();
        let frames = wal.log().frames().len();
        append_ops(&mut wal, &ops, Durability::Fsync);
        let written = (wal.log().frames().len() - frames) as u64;
        assert!(written >= 1, "target {target}");
        // Each chunk is one write; whatever is left is the syncs.
        assert_eq!(file.mutations() - calls - written, 1, "target {target}");
        assert_eq!(file.bytes(), file.durable_bytes(), "target {target}");
        if target == 1 {
            assert_eq!(written, ops.len() as u64);
        }
        if target == DEFAULT_CHUNK_TARGET {
            assert_eq!(written, 1);
        }
    }
}

// ---- Rotation (invariant 7) ----

#[test]
fn rotation_closes_chunks_at_the_target_and_keeps_order() {
    let ops: Vec<Op> = (0..40)
        .map(|i| match i % 5 {
            0 => note("n1", &format!("version {i}"), "x"),
            1 => delete("notes", "n1"),
            2 => task(
                &format!("t{}", i % 3),
                &"long title ".repeat(i % 7),
                i64::try_from(i).unwrap(),
            ),
            3 => delete("tasks", &format!("t{}", i % 4)),
            _ => note(&format!("n{}", i % 6), "body", "y"),
        })
        .collect();
    let universe = reference(&ops);
    for target in [1, 50, 120, 400, 1500] {
        let (file, wal) = new_wal();
        let mut wal = wal.with_chunk_target(target);
        // Mixed durabilities: Buffered and Async records rotate too.
        for (i, part) in ops.chunks(7).enumerate() {
            let durability = [Durability::Buffered, Durability::Async, Durability::Fsync][i % 3];
            append_ops(&mut wal, part, durability);
        }
        wal.sync().unwrap();
        let written = chunks(wal.log());
        let owned = owned(&ops);
        let expected: Vec<_> = owned.iter().map(Owned::record).collect();
        let mut decoded_all = Vec::new();
        for (i, (at, chunk)) in written.iter().enumerate() {
            let records = decode_all(chunk).unwrap();
            let last = records.last().unwrap();
            let last_len = 8 + 2 + last.collection().len() + last.payload().len();
            // A one-record chunk only holds the header before its record, which may pass a tiny target.
            assert!(
                records.len() == 1 || chunk.len() - last_len < target,
                "target {target}, chunk {i} at {at}: should have closed before its last record"
            );
            decoded_all.extend(records);
        }
        assert_eq!(decoded_all, expected, "target {target}: order");
        if target <= 400 {
            assert!(written.len() > 1, "target {target}: no rotation");
        }
        drop(wal);
        assert_eq!(
            replay_after(&file, CrashLoss::All, &universe),
            universe,
            "target {target}"
        );
    }
}

#[test]
fn rotation_closes_a_chunk_once_it_reaches_the_target() {
    // With no sync points, every chunk but the buffered tail was closed by rotation: at or past the target.
    let target = 100;
    let (_file, wal) = new_wal();
    let mut wal = wal.with_chunk_target(target);
    let ops: Vec<Op> = (0..30)
        .map(|i| delete("notes", &format!("id-{i}")))
        .collect();
    append_ops(&mut wal, &ops, Durability::Buffered);
    let written = chunks(wal.log());
    assert!(written.len() >= 2);
    for (at, chunk) in &written {
        assert!(
            chunk.len() >= target,
            "chunk at {at}: {} bytes",
            chunk.len()
        );
    }
    assert!(wal.buffered_len() < target);
}

// ---- wal_start (invariant 8) ----

#[test]
fn wal_start_tracks_the_oldest_unflushed_collection() {
    let (_file, mut wal) = new_wal();
    let end = |w: &Wal| w.log().end();
    assert_eq!(wal.wal_start(), end(&wal));
    append_ops(&mut wal, &[delete("notes", "a")], Durability::Buffered);
    assert_eq!(wal.wal_start(), end(&wal), "only buffered");

    let chunk_a = end(&wal);
    append_ops(&mut wal, &[delete("notes", "b")], Durability::Fsync); // chunk A: notes
    assert_eq!(wal.wal_start(), chunk_a);
    let chunk_b = end(&wal);
    append_ops(&mut wal, &[delete("tasks", "a")], Durability::Fsync); // chunk B: tasks
    let chunk_c = end(&wal);
    append_ops(&mut wal, &[delete("notes", "c")], Durability::Fsync); // chunk C: notes
    assert_eq!(wal.wal_start(), chunk_a);

    wal.flushed("notes", wal.mark());
    assert_eq!(wal.wal_start(), chunk_b, "tasks' oldest chunk");
    let chunk_d = end(&wal);
    append_ops(&mut wal, &[delete("notes", "d")], Durability::Fsync); // chunk D: notes
    assert_eq!(wal.wal_start(), chunk_b);
    wal.flushed("tasks", wal.mark());
    assert_eq!(
        wal.wal_start(),
        chunk_d,
        "notes' oldest chunk since its flush"
    );
    wal.flushed("ghost", wal.mark());
    assert_eq!(wal.wal_start(), chunk_d);
    wal.flushed("notes", wal.mark());
    assert_eq!(wal.wal_start(), end(&wal));

    // A record buffered before a flush is covered by it, even if written out later with another collection's.
    append_ops(&mut wal, &[delete("notes", "e")], Durability::Buffered);
    wal.flushed("notes", wal.mark());
    let chunk_e = end(&wal);
    append_ops(&mut wal, &[delete("tasks", "b")], Durability::Fsync);
    assert_eq!(wal.wal_start(), chunk_e);
    wal.flushed("tasks", wal.mark());
    assert_eq!(wal.wal_start(), end(&wal));
    assert!(chunk_c > chunk_b && chunk_b > chunk_a);
}

#[test]
fn resume_keeps_wal_start_at_the_replayed_chunks() {
    let (file, mut wal) = new_wal();
    let a = wal.log().end();
    append_ops(&mut wal, &[note("n1", "one", "x")], Durability::Fsync);
    let b = wal.log().end();
    append_ops(&mut wal, &[task("t1", "one", 1)], Durability::Fsync);
    append_ops(&mut wal, &[delete("notes", "n0")], Durability::Fsync);
    drop(wal);

    // `crash` returns a new file: keep it, the resumed writer writes there.
    let file = file.crash(CrashLoss::All);
    let container = Container::open_on(file.clone(), Mode::ReadWrite).unwrap();
    let mut mem = memtables();
    let summary = replay_container(&container, &mut mem).unwrap();
    assert_eq!(summary.records, 3);
    assert_eq!(
        summary.pending,
        BTreeMap::from([("notes".into(), a), ("tasks".into(), b)])
    );
    let mut wal = WalWriter::resume(container, summary.pending);
    assert_eq!(wal.wal_start(), a);
    append_ops(&mut wal, &[delete("notes", "n1")], Durability::Fsync);
    assert_eq!(wal.wal_start(), a);
    wal.flushed("notes", wal.mark());
    assert_eq!(wal.wal_start(), b);

    // Commit with wal_start at B: the next open replays from B only.
    wal.sync().unwrap();
    let wal_start = wal.wal_start();
    let manifest = wal
        .log_mut()
        .append(FrameKind::Manifest, b"manifest", Compression::None)
        .unwrap();
    wal.log_mut()
        .commit(Commit {
            manifest,
            wal_start,
            fence_token: 0,
        })
        .unwrap();
    drop(wal);
    let container = Container::open_on(file.crash(CrashLoss::All), Mode::ReadWrite).unwrap();
    let mut mem = memtables();
    let summary = replay_container(&container, &mut mem).unwrap();
    // Chunks B (tasks), the notes delete, and the notes delete after resume.
    assert_eq!(summary.records, 3);
    assert_eq!(summary.pending["tasks"], b);
    let wal = WalWriter::resume(container, summary.pending);
    assert_eq!(wal.wal_start(), b);
}

#[derive(Debug, Clone, Copy)]
enum WriterOp {
    Append {
        collection: usize,
        count: usize,
        durability: Durability,
    },
    Sync,
    Flushed(usize),
}

fn writer_op() -> impl Strategy<Value = WriterOp> {
    let durability = prop_oneof![
        Just(Durability::Buffered),
        Just(Durability::Async),
        Just(Durability::Fsync)
    ];
    prop_oneof![
        4 => (0..2usize, 1..4usize, durability).prop_map(|(collection, count, durability)| {
            WriterOp::Append { collection, count, durability }
        }),
        1 => Just(WriterOp::Sync),
        1 => (0..2usize).prop_map(WriterOp::Flushed),
    ]
}

proptest! {
    #![proptest_config(proptest_config(128))]

    /// Invariants 6–8 against a model: written chunks hold a prefix of the appended records in order, sync points
    /// leave nothing buffered or unsynced, and `wal_start` is the oldest written chunk of a collection not flushed
    /// since its first record, never moving back.
    #[test]
    fn writer_matches_its_model(
        ops in prop::collection::vec(writer_op(), 1..40),
        target in prop_oneof![Just(1usize), Just(40), Just(120), Just(DEFAULT_CHUNK_TARGET)],
    ) {
        const COLLECTIONS: [&str; 2] = ["notes", "tasks"];
        let (file, wal) = new_wal();
        let mut wal = wal.with_chunk_target(target);
        let mut appended: Vec<(&str, String)> = Vec::new();
        let mut first_unflushed: [Option<usize>; 2] = [None, None];
        let mut last_start = wal.wal_start();
        for op in ops {
            match op {
                WriterOp::Append { collection, count, durability } => {
                    let c = COLLECTIONS[collection];
                    let ids: Vec<String> = (appended.len()..appended.len() + count).map(|i| format!("d{i}")).collect();
                    let records: Vec<_> = ids.iter().map(|id| WalRecord::delete(c, id).unwrap()).collect();
                    first_unflushed[collection].get_or_insert(appended.len());
                    appended.extend(ids.iter().map(|id| (c, id.clone())));
                    wal.append_batch(&records, durability).unwrap();
                    if durability != Durability::Buffered {
                        prop_assert_eq!(wal.buffered_len(), 0);
                    }
                    if durability == Durability::Fsync {
                        prop_assert_eq!(file.bytes(), file.durable_bytes());
                    }
                }
                WriterOp::Sync => {
                    wal.sync().unwrap();
                    prop_assert_eq!(wal.buffered_len(), 0);
                    prop_assert_eq!(file.bytes(), file.durable_bytes());
                }
                WriterOp::Flushed(collection) => {
                    wal.flushed(COLLECTIONS[collection], wal.mark());
                    first_unflushed[collection] = None;
                }
            }
            // Written records: a prefix of the appended ones, in order; the rest is buffered.
            let mut chunk_of = Vec::new(); // record index -> chunk position
            let mut written: Vec<(&str, String)> = Vec::new();
            for (at, chunk) in chunks(wal.log()) {
                for r in decode_all(&chunk).unwrap() {
                    let c = COLLECTIONS.iter().find(|&&c| c == r.collection()).unwrap();
                    written.push((c, r.deleted_id().unwrap().to_owned()));
                    chunk_of.push(at);
                }
            }
            prop_assert_eq!(&written[..], &appended[..written.len()]);
            prop_assert_eq!(wal.buffered_len() == 0, written.len() == appended.len());
            let model = first_unflushed
                .iter()
                .flatten()
                .filter_map(|&i| chunk_of.get(i).copied())
                .min()
                .unwrap_or_else(|| wal.log().end());
            prop_assert_eq!(wal.wal_start(), model, "after {:?}", op);
            prop_assert!(wal.wal_start() >= last_start, "wal_start moved back after {:?}", op);
            last_start = wal.wal_start();
        }
    }
}

// ---- Faults at every call (invariant 9) ----

const FAULT_SEEDS: u64 = 8;

#[test]
fn a_fault_at_every_call_poisons_the_writer_and_keeps_acknowledged_writes() {
    let groups = history();
    let all: Vec<Op> = groups.concat();
    let universe = reference(&all);
    let target = 64; // rotation inside batches too
    let run = |fault: Option<SimFault>| {
        let file = SimFile::new();
        let mut wal = WalWriter::new(Container::create_on(file.clone(), [7; 16]).unwrap())
            .unwrap()
            .with_chunk_target(target);
        let base = file.mutations();
        file.set_fault(fault.map(|f| match f {
            SimFault::FailAt(n) => SimFault::FailAt(base + n),
            SimFault::ApplyThenFail(n) => SimFault::ApplyThenFail(base + n),
        }));
        let mut acked = 0;
        let mut failed = false;
        for (g, group) in groups.iter().enumerate() {
            let owned = owned(group);
            let records: Vec<_> = owned.iter().map(Owned::record).collect();
            match (wal.append_batch(&records, Durability::Fsync), failed) {
                (Ok(()), false) => acked += group.len(),
                (Err(WalWriteError::Log(_)), false) => failed = true,
                (Err(WalWriteError::Poisoned), true) => {}
                (other, _) => panic!("{fault:?}, group {g}: {other:?} (failed before: {failed})"),
            }
        }
        (file, wal, base, acked, failed)
    };
    let (clean, _, base, acked, failed) = run(None);
    assert!(!failed);
    assert_eq!(acked, all.len());
    let calls = clean.mutations() - base;
    assert!(
        calls > 2 * groups.len() as u64,
        "the workload should rotate inside batches"
    );

    let mut corrupt_tails = 0;
    for n in 0..calls {
        for fault in [SimFault::FailAt(n), SimFault::ApplyThenFail(n)] {
            let (file, mut wal, _, acked, failed) = run(Some(fault));
            assert!(failed && file.crashed(), "{fault:?}: no failure");
            assert!(
                matches!(wal.sync(), Err(WalWriteError::Poisoned)),
                "{fault:?}: sync after failure"
            );
            let record = WalRecord::delete("notes", "late").unwrap();
            for durability in [Durability::Buffered, Durability::Async, Durability::Fsync] {
                assert!(
                    matches!(
                        wal.append(&record, durability),
                        Err(WalWriteError::Poisoned)
                    ),
                    "{fault:?}: {durability:?} append after failure"
                );
            }
            drop(wal);
            let losses = [CrashLoss::All, CrashLoss::Nothing]
                .into_iter()
                .chain((0..FAULT_SEEDS).map(CrashLoss::Seeded));
            for loss in losses {
                let ctx = format!("{fault:?} (call {n} after create), crash {loss:?}");
                let crashed = file.crash(loss);
                let container = match Container::open_on(crashed.clone(), Mode::ReadWrite) {
                    Ok(c) => c,
                    // SPEC §4.4: unsynced sectors reached the disk out of order, leaving a hole before non-zero
                    // bytes. A read-write open of a container without a path may not discard them (ADR-0014); a
                    // read-only open reads what precedes the hole, which must still hold every acknowledged write.
                    Err(ContainerError::CorruptTail { .. }) => {
                        corrupt_tails += 1;
                        Container::open_on(crashed, Mode::ReadOnly)
                            .unwrap_or_else(|e| panic!("{ctx}: read-only open: {e}"))
                    }
                    Err(e) => panic!("{ctx}: open: {e}"),
                };
                let mut mem = memtables();
                replay_container(&container, &mut mem)
                    .unwrap_or_else(|e| panic!("{ctx}: replay: {e}"));
                let got = observed(&mem, &universe);
                let prefix = (acked..=all.len()).find(|&k| reference(&all[..k]) == got);
                assert!(
                    prefix.is_some(),
                    "{ctx}: not a prefix of at least the {acked} acknowledged operations: {got:?}"
                );
            }
        }
    }
    // Deterministic for the seeds above; shown so a change in how often the fallback runs is visible.
    eprintln!("{corrupt_tails} crashes left a corrupt tail (read-only fallback)");
}

// ---- Replay errors (invariant 10) ----

fn replay_one(chunk: &[u8]) -> (BTreeMap<String, Memtable>, Result<(), ReplayError>, u64) {
    let mut mem = memtables();
    let mut replay = Replay::new(&mut mem);
    let result = replay.chunk(4096, chunk);
    let records = replay.finish().records;
    (mem, result, records)
}

#[test]
fn replay_rejects_a_record_of_an_unknown_collection() {
    let first = Owned::of(&note("n1", "kept", "x"));
    let records = [
        first.record(),
        WalRecord::delete("ghost", "g1").unwrap(),
        WalRecord::delete("notes", "n2").unwrap(),
    ];
    let chunk = encode_chunk(&records).unwrap();
    let second_at = 12 + records[0].payload().len() as u64 + 8 + 2 + 5;
    let (mem, result, applied) = replay_one(&chunk);
    let err = result.unwrap_err();
    assert!(err.is_corruption());
    match err {
        ReplayError::UnknownCollection {
            chunk,
            offset,
            collection,
        } => assert_eq!(
            (chunk, offset, collection.as_str()),
            (4096, second_at, "ghost")
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(applied, 1);
    let view = mem["notes"].view();
    assert_eq!(
        view.lookup("n1"),
        Some(IdState::Live(0)),
        "earlier record stays applied"
    );
    assert_eq!(view.lookup("n2"), None, "later record not applied");
}

#[test]
fn replay_rejects_a_put_that_does_not_decode() {
    // A `tasks` document is not a `notes` one, and garbage is nothing.
    let task_doc = Owned::of(&task("t1", "title", 3));
    for (name, payload) in [
        ("tasks document", task_doc.2.clone()),
        ("garbage", vec![0xFF; 9]),
    ] {
        let first = WalRecord::delete("notes", "n0").unwrap();
        let records = [first, WalRecord::put("notes", &payload).unwrap()];
        let chunk = encode_chunk(&records).unwrap();
        let (mem, result, applied) = replay_one(&chunk);
        match result {
            Err(ReplayError::Document {
                chunk,
                offset,
                error,
            }) => {
                assert_eq!((chunk, offset), (4096, 12 + 8 + 2 + 5 + 2), "{name}");
                let err = ReplayError::Document {
                    chunk,
                    offset,
                    error: error.clone(),
                };
                assert_eq!(err.is_corruption(), error.is_corruption(), "{name}");
            }
            other => panic!("{name}: {other:?}"),
        }
        assert_eq!(applied, 1, "{name}");
        assert_eq!(
            mem["notes"].view().lookup("n0"),
            Some(IdState::Deleted),
            "{name}"
        );
    }
}

#[test]
fn replay_stops_at_a_corrupt_record_keeping_earlier_ones() {
    let first = WalRecord::delete("notes", "n0").unwrap();
    let mut chunk = encode_chunk(&[first, WalRecord::delete("tasks", "t0").unwrap()]).unwrap();
    let last = chunk.len() - 1;
    chunk[last] ^= 1; // the second record's CRC
    let (mem, result, applied) = replay_one(&chunk);
    match result {
        Err(ReplayError::Chunk {
            chunk: 4096,
            error:
                WalError::Corrupt {
                    reason: WalCorruption::ChecksumMismatch { .. },
                    ..
                },
        }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(applied, 1);
    assert_eq!(mem["notes"].view().lookup("n0"), Some(IdState::Deleted));
    assert_eq!(mem["tasks"].view().lookup("t0"), None);

    // A chunk of a newer encoding is an error too, but not corruption.
    let mut newer = encode_chunk(&[first]).unwrap();
    newer[4] = 2;
    let (_, result, applied) = replay_one(&newer);
    let err = result.unwrap_err();
    assert!(!err.is_corruption(), "{err}");
    assert_eq!(applied, 0);
}

// ---- Review follow-ups: flush cuts, commit points, records replay cannot apply ----

#[test]
fn a_flush_covers_only_the_records_before_its_mark() {
    let mut wal = WalWriter::new(Container::create_on(SimFile::new(), [7; 16]).unwrap()).unwrap();
    wal.append(
        &WalRecord::delete("notes", "n1").unwrap(),
        Durability::Fsync,
    )
    .unwrap();
    let frozen = wal.mark();
    wal.append(
        &WalRecord::delete("notes", "n2").unwrap(),
        Durability::Fsync,
    )
    .unwrap();
    let second = wal.log().frames()[1].offset;
    // "n2" was logged while the flush ran: its chunk stays needed.
    wal.flushed("notes", frozen);
    assert_eq!(wal.wal_start(), second);
    wal.flushed("notes", wal.mark());
    assert_eq!(wal.wal_start(), wal.log().end());
}

/// Drops "tasks" while one of its records is buffered: every memtable is flushed, then the commit is made with the
/// `wal_start` from `commit_point` (or, wrongly, from `wal_start` without a sync). Returns the reopened container.
fn drop_with_a_buffered_record(use_commit_point: bool) -> Container<SimFile> {
    let file = SimFile::new();
    let mut wal = WalWriter::new(Container::create_on(file.clone(), [7; 16]).unwrap()).unwrap();
    wal.append(
        &WalRecord::delete("tasks", "t1").unwrap(),
        Durability::Buffered,
    )
    .unwrap();
    wal.flushed("notes", wal.mark());
    wal.flushed("tasks", wal.mark());
    let wal_start = if use_commit_point {
        wal.commit_point().unwrap()
    } else {
        wal.wal_start()
    };
    let manifest = wal
        .log_mut()
        .append(
            FrameKind::Manifest,
            b"manifest without tasks",
            Compression::None,
        )
        .unwrap();
    wal.log_mut()
        .commit(Commit {
            manifest,
            wal_start,
            fence_token: 0,
        })
        .unwrap();
    wal.sync().unwrap();
    drop(wal);
    Container::open_on(file.crash(CrashLoss::All), Mode::ReadWrite).unwrap()
}

#[test]
fn a_commit_point_keeps_buffered_records_of_a_dropped_collection_out_of_replay() {
    let notes_only = || {
        let mut m = memtables();
        m.remove("tasks");
        m
    };
    let container = drop_with_a_buffered_record(true);
    let summary = replay_container(&container, &mut notes_only()).unwrap();
    assert_eq!(summary.records, 0);
    // Without the sync, the buffered chunk lands after the Manifest and every later replay fails.
    let container = drop_with_a_buffered_record(false);
    let err = replay_container(&container, &mut notes_only()).unwrap_err();
    assert!(
        matches!(err, ReplayError::UnknownCollection { ref collection, .. } if collection == "tasks"),
        "{err}"
    );
}

#[test]
fn a_mark_taken_right_after_resume_covers_the_replayed_chunks_only() {
    let mut first = WalWriter::new(Container::create_on(SimFile::new(), [7; 16]).unwrap()).unwrap();
    first
        .append(
            &WalRecord::delete("notes", "n1").unwrap(),
            Durability::Fsync,
        )
        .unwrap();
    let replayed = first.log().frames()[0].offset;
    let mut wal = WalWriter::resume(
        first.into_log(),
        BTreeMap::from([("notes".into(), replayed)]),
    );
    let m = wal.mark();
    wal.append(
        &WalRecord::delete("notes", "n2").unwrap(),
        Durability::Fsync,
    )
    .unwrap();
    let new = wal.log().frames()[1].offset;
    assert_eq!(wal.wal_start(), replayed);
    wal.flushed("notes", m);
    assert_eq!(wal.wal_start(), new);
}

#[test]
fn new_refuses_a_log_that_holds_chunks() {
    let file = SimFile::new();
    let mut wal = WalWriter::new(Container::create_on(file.clone(), [7; 16]).unwrap()).unwrap();
    wal.append(
        &WalRecord::delete("notes", "n1").unwrap(),
        Durability::Fsync,
    )
    .unwrap();
    drop(wal);
    let reopened = Container::open_on(file.crash(CrashLoss::All), Mode::ReadWrite).unwrap();
    assert_eq!(WalWriter::new(reopened).unwrap_err(), LogHoldsChunks);
}

#[test]
fn buffered_len_counts_record_bytes_only() {
    let mut wal = WalWriter::new(Container::create_on(SimFile::new(), [7; 16]).unwrap()).unwrap();
    // body_len, op, name_len, "notes", "n1", CRC.
    wal.append(
        &WalRecord::delete("notes", "n1").unwrap(),
        Durability::Buffered,
    )
    .unwrap();
    assert_eq!(wal.buffered_len(), 4 + 1 + 1 + 5 + 2 + 4);
}
