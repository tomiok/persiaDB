//! The write-ahead log (SPEC §8.2, §8.2.1, ADR-0028): one ordered log of `put` and `delete` records for the whole
//! database, in chunks (locally, `WalChunk` frames of the container).
//!
//! [`WalWriter`] groups records into chunks and syncs ([`Durability`]), closes a chunk at a target size (rotation),
//! and tracks `wal_start`: the oldest chunk still holding a record of a collection that has not been flushed.
//! On open, [`replay_container`] (or [`Replay`] chunk by chunk) applies the chunks from `wal_start` to the
//! collections' memtables.
//!
//! ```
//! use std::collections::BTreeMap;
//!
//! use persia_analysis::AnalyzerRegistry;
//! use persia_engine::document::{Document, FieldSet, Value};
//! use persia_engine::memtable::{IdState, Memtable};
//! use persia_engine::schema::{FieldKind, SchemaBuilder, TextOptions};
//! use persia_engine::wal::{Durability, WalRecord, WalWriter, replay_container};
//! use persia_storage::container::sim::{CrashLoss, SimFile};
//! use persia_storage::container::{Container, Mode};
//!
//! let schema = SchemaBuilder::new("notes")
//!     .field("body", FieldKind::Text(TextOptions::default()))
//!     .build()?;
//! let body = schema.field("body").map(|f| f.id()).ok_or("no field")?;
//! let doc = Document::new("n1").with(body, Value::String("hello".into()));
//! let encoded = doc.encode(&schema, FieldSet::All)?;
//!
//! let file = SimFile::new();
//! let mut wal = WalWriter::new(Container::create_on(file.clone(), [7; 16])?);
//! wal.append_batch(
//!     &[WalRecord::put("notes", &encoded)?, WalRecord::delete("notes", "n0")?],
//!     Durability::Fsync,
//! )?;
//! drop(wal);
//!
//! // After a crash that loses every unsynced write, the synced chunk is found after `log_end` and replayed.
//! let container = Container::open_on(file.crash(CrashLoss::All), Mode::ReadOnly)?;
//! let mut memtables = BTreeMap::from([(
//!     "notes".to_owned(),
//!     Memtable::new(&schema, &AnalyzerRegistry::builtin())?,
//! )]);
//! let summary = replay_container(&container, &mut memtables)?;
//! assert_eq!(summary.records, 2);
//! let view = memtables["notes"].view();
//! assert_eq!(view.lookup("n1"), Some(IdState::Live(0)));
//! assert_eq!(view.lookup("n0"), Some(IdState::Deleted));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod chunk;
mod replay;
mod writer;

pub use chunk::{
    ChunkFull, ChunkRecords, MAGIC, MAX_CHUNK_LEN, RecordError, VERSION, WalCorruption, WalError,
    WalOp, WalRecord, decode_chunk, encode_chunk,
};
pub use replay::{Replay, ReplayError, ReplaySummary, replay_container};
pub use writer::{DEFAULT_CHUNK_TARGET, Durability, WalLog, WalWriteError, WalWriter};
