//! The table of error codes: every variant of every error enum this crate wraps, with the code it gets.
//!
//! The enums this crate can name are matched exhaustively in `classify.rs`, so the compiler keeps them complete; the
//! rows here pin each code. The enums of `persia-storage` are named here through the dev-dependency: `container_name`
//! and `storage_kind_name` match on them exhaustively and give the name the classifier looks for, so a variant added
//! or renamed there fails to compile or fails a row, never falls back silently.

#![allow(clippy::too_many_lines)] // Tables: one row per variant.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use persia_blob::{BlobId, BlobRef, UploadId, blob_id_of};
use persia_engine::db::{DatabaseError, RelaxedSearchError};
use persia_engine::document::{DecodeError, Document, DocumentCorruption, DocumentError};
use persia_engine::manifest::{
    ApplyError, Layout, LayoutError, ManifestCorruption, ManifestEncodeError, ManifestError,
    SegmentId,
};
use persia_engine::memtable::MemtableError;
use persia_engine::query::QueryError;
use persia_engine::relax::{RelaxError, RelaxOptions};
use persia_engine::schema::{
    CollectionSchema, EvolutionError, FieldId, FieldKind, FieldType, KeywordOptions, NameProblem,
    SchemaBuilder, SchemaError,
};
use persia_engine::score::ScoreError;
use persia_engine::search::{Interrupt, RequestError, SearchError};
use persia_engine::segment::{
    ReadError, SectionKind, SegmentCorruption, SegmentError, UnsupportedSegment,
};
use persia_engine::tombstone::SidecarError;
use persia_engine::wal::{
    ChunkFull, RecordError, ReplayError, WalCorruption, WalError, WalWriteError,
};
use persia_storage::container::{Error as ContainerError, ReadOnlyReason};
use persia_storage::{Error as StorageError, ErrorKind, Key, Op};

use super::*;
use crate::Error;
use ErrorCode::{
    AlreadyExists, Cancelled, Conflict, Corrupt, InvalidArgument, Io, Limit, NotFound, Poisoned,
    ReadOnly, Transient, Unsupported,
};

fn schema() -> CollectionSchema {
    SchemaBuilder::new("c")
        .field("k", FieldKind::Keyword(KeywordOptions::default()))
        .build()
        .unwrap()
}

fn field_id() -> FieldId {
    schema().field("k").unwrap().id()
}

fn segment_id() -> SegmentId {
    SegmentId::new(7)
}

fn blob_id() -> BlobId {
    blob_id_of(b"blob")
}

fn os_error() -> io::Error {
    io::Error::other("disk on fire")
}

/// A document's encoding whose blob reference starts with `version`: the decoder returns the `persia_format::Error`
/// of `BlobRef::decode` (corruption for version 0, unsupported above the current one), a type this crate cannot
/// name.
fn blob_ref_error(version: u8) -> DecodeError {
    // CBOR: array(5) [1, "a", bytes(1) [version], {}, {}] (SPEC §6.4.2).
    let bytes = [0x85, 0x01, 0x61, b'a', 0x41, version, 0xa0, 0xa0];
    Document::decode(&bytes, &schema()).unwrap_err()
}

/// `$name` bound to a `persia_format::Error`: corrupt, or of a newer format.
macro_rules! format_error {
    (corrupt, $name:ident) => {
        let DecodeError::BlobRef($name) = blob_ref_error(0) else {
            panic!("version 0 is a corrupt blob reference")
        };
        assert!($name.is_corruption());
    };
    (unsupported, $name:ident) => {
        let DecodeError::BlobRef($name) = blob_ref_error(0x17) else {
            panic!("version 23 is a newer blob reference")
        };
        assert!(!$name.is_corruption());
    };
}

/// A `persia_format::EncodeError`.
macro_rules! encode_error {
    () => {
        BlobRef::new(blob_id(), 1, "", 4 << 20, BTreeMap::new()).unwrap_err()
    };
}

fn key() -> Key {
    Key::new("segments/c/1.seg").unwrap()
}

/// Each variant's name, by an exhaustive match: a new variant does not compile here.
fn container_name(error: &ContainerError) -> &'static str {
    match error {
        ContainerError::Io { .. } => "Io",
        ContainerError::Format(_) => "Format",
        ContainerError::CorruptTail { .. } => "CorruptTail",
        ContainerError::Locked => "Locked",
        ContainerError::AlreadyExists { .. } => "AlreadyExists",
        ContainerError::ReadOnly { .. } => "ReadOnly",
        ContainerError::Poisoned => "Poisoned",
        ContainerError::InvalidInput(_) => "InvalidInput",
        ContainerError::CompactedNotDurable { .. } => "CompactedNotDurable",
        ContainerError::OutOfMemory { .. } => "OutOfMemory",
        ContainerError::Encode(_) => "Encode",
    }
}

/// Every variant of the container's error that can be built here, with its code. `CompactedNotDurable` holds a
/// relocation table only a compaction can build here: its kind's code is in [`every_storage_and_format_kind_has_its_code`],
/// and `persia-storage` tests its kind.
fn container_cases() -> Vec<(ErrorCode, ContainerError)> {
    format_error!(corrupt, corrupt);
    format_error!(corrupt, corrupt_tail);
    format_error!(unsupported, unsupported);
    vec![
        (
            Io,
            ContainerError::Io {
                op: "read",
                source: os_error(),
            },
        ),
        (Corrupt, ContainerError::Format(corrupt)),
        (Unsupported, ContainerError::Format(unsupported)),
        (
            Corrupt,
            ContainerError::CorruptTail {
                offset: 12_288,
                source: corrupt_tail,
                acknowledged: None,
            },
        ),
        (Conflict, ContainerError::Locked),
        (
            AlreadyExists,
            ContainerError::AlreadyExists {
                path: PathBuf::from("app.persia"),
            },
        ),
        (
            ReadOnly,
            ContainerError::ReadOnly {
                reason: ReadOnlyReason::OpenedReadOnly,
            },
        ),
        (Poisoned, ContainerError::Poisoned),
        (
            InvalidArgument,
            ContainerError::InvalidInput("Locked is not a variant here".into()),
        ),
        (FALLBACK, ContainerError::OutOfMemory { bytes: 1 << 40 }),
        (FALLBACK, ContainerError::Encode(encode_error!())),
    ]
}

#[test]
fn container_errors_by_variant() {
    let mut seen = std::collections::BTreeSet::new();
    for (expected, error) in container_cases() {
        let name = container_name(&error);
        seen.insert(name);
        let shown = error.to_string();
        // Directly, and through each engine error that carries one.
        assert_eq!(container_kind(error.kind()), expected, "{shown}");
        assert_eq!(DatabaseError::Container(error).code(), expected, "{shown}");
    }
    // Every variant but the one that cannot be built here (see `container_cases`).
    assert_eq!(seen.len(), 10);
    assert!(!seen.contains("CompactedNotDurable"));
    for (expected, error) in container_cases() {
        let shown = error.to_string();
        let replay = DatabaseError::Replay(ReplayError::Container(error));
        assert_eq!(replay.code(), expected, "{shown}");
    }
    for (expected, error) in container_cases() {
        let shown = error.to_string();
        let wal = DatabaseError::Wal(WalWriteError::Log(error));
        assert_eq!(wal.code(), expected, "{shown}");
    }
    for (expected, error) in container_cases() {
        let shown = error.to_string();
        assert_eq!(
            persia_blob::Error::Container(error).code(),
            expected,
            "{shown}"
        );
    }
}

/// Each kind's name, by an exhaustive match.
fn storage_kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "NotFound",
        ErrorKind::Conflict => "Conflict",
        ErrorKind::Transient => "Transient",
        ErrorKind::Corrupt => "Corrupt",
        ErrorKind::Io => "Io",
        ErrorKind::InvalidInput => "InvalidInput",
        ErrorKind::Unsupported => "Unsupported",
    }
}

#[allow(clippy::reversed_empty_ranges)] // An inverted range is one of the errors.
fn storage_cases() -> Vec<(ErrorCode, ErrorKind, StorageError)> {
    format_error!(corrupt, corrupt);
    vec![
        (
            NotFound,
            ErrorKind::NotFound,
            StorageError::NotFound { key: key() },
        ),
        (
            Conflict,
            ErrorKind::Conflict,
            StorageError::Conflict { key: key() },
        ),
        (
            Transient,
            ErrorKind::Transient,
            StorageError::Transient {
                op: Op::Get,
                target: "k".into(),
                source: "throttled".into(),
            },
        ),
        (
            Corrupt,
            ErrorKind::Corrupt,
            StorageError::Corrupt {
                key: key(),
                source: corrupt,
            },
        ),
        (
            Corrupt,
            ErrorKind::Corrupt,
            StorageError::ShortRead {
                key: key(),
                requested: 0..8,
                returned: 3,
            },
        ),
        (
            Io,
            ErrorKind::Io,
            StorageError::Io {
                op: Op::Get,
                target: "k".into(),
                source: os_error(),
            },
        ),
        (
            InvalidArgument,
            ErrorKind::InvalidInput,
            StorageError::InvalidKey(Key::new("").unwrap_err()),
        ),
        (
            InvalidArgument,
            ErrorKind::InvalidInput,
            StorageError::InvalidRange {
                key: key(),
                range: 9..1,
            },
        ),
        (
            Unsupported,
            ErrorKind::Unsupported,
            StorageError::Unsupported { op: Op::Put },
        ),
    ]
}

#[test]
fn storage_errors_by_kind() {
    let mut kinds = std::collections::BTreeSet::new();
    for (expected, kind, error) in storage_cases() {
        assert_eq!(error.kind(), kind);
        let name = storage_kind_name(kind);
        kinds.insert(name);
        assert_eq!(storage_kind(kind), expected, "{name}");
        assert_eq!(DatabaseError::Storage(error).code(), expected, "{name}");
    }
    assert_eq!(kinds.len(), 7, "every storage kind has a row");
    for (expected, _, error) in storage_cases() {
        let shown = error.to_string();
        assert_eq!(
            persia_blob::Error::Storage(error).code(),
            expected,
            "{shown}"
        );
    }
}

/// Every class of every lower error, with its code. The `match`es in [`every_kind_is_listed`] have no wildcard, so a
/// class added or renamed in `persia-storage` or `persia-format` fails to compile until it has a row here.
#[test]
fn every_storage_and_format_kind_has_its_code() {
    use persia_engine::error_kinds::{
        ContainerErrorKind as C, FormatErrorKind as F, StorageErrorKind as S,
    };
    let formats = [
        (F::Corrupt, Corrupt),
        (F::Unsupported, Unsupported),
        (F::OutOfMemory, FALLBACK),
        (F::InvalidInput, FALLBACK),
    ];
    let containers = [
        (C::Io, FALLBACK),
        (C::CompactedNotDurable, FALLBACK),
        (C::Corrupt, Corrupt),
        (C::Unsupported, Unsupported),
        (C::OutOfMemory, FALLBACK),
        (C::FormatInput, FALLBACK),
        (C::Locked, Conflict),
        (C::AlreadyExists, AlreadyExists),
        (C::ReadOnly, ReadOnly),
        (C::Poisoned, Poisoned),
        (C::InvalidInput, InvalidArgument),
        (C::Encode, FALLBACK),
    ];
    let storages = [
        (S::NotFound, NotFound),
        (S::Conflict, Conflict),
        (S::Transient, Transient),
        (S::Corrupt, Corrupt),
        (S::Io, FALLBACK),
        (S::InvalidInput, InvalidArgument),
        (S::Unsupported, Unsupported),
    ];
    assert_eq!(FALLBACK, Io);
    for (kind, code) in formats {
        assert_eq!(format_kind(kind), code, "{kind:?}");
    }
    for (kind, code) in containers {
        assert_eq!(container_kind(kind), code, "{kind:?}");
    }
    for (kind, code) in storages {
        assert_eq!(storage_kind(kind), code, "{kind:?}");
    }
    every_kind_is_listed(
        &formats.map(|r| r.0),
        &containers.map(|r| r.0),
        &storages.map(|r| r.0),
    );
}

/// Fails to compile when a class is added, and fails when a listed class has no row.
fn every_kind_is_listed(
    formats: &[FormatErrorKind],
    containers: &[ContainerErrorKind],
    storages: &[StorageErrorKind],
) {
    use persia_engine::error_kinds::{
        ContainerErrorKind as C, FormatErrorKind as F, StorageErrorKind as S,
    };
    let format_count = |k: F| match k {
        F::Corrupt | F::Unsupported | F::OutOfMemory | F::InvalidInput => 4,
    };
    let container_count = |k: C| match k {
        C::Io
        | C::CompactedNotDurable
        | C::Corrupt
        | C::Unsupported
        | C::OutOfMemory
        | C::FormatInput
        | C::Locked
        | C::AlreadyExists
        | C::ReadOnly
        | C::Poisoned
        | C::InvalidInput
        | C::Encode => 12,
    };
    let storage_count = |k: S| match k {
        S::NotFound
        | S::Conflict
        | S::Transient
        | S::Corrupt
        | S::Io
        | S::InvalidInput
        | S::Unsupported => 7,
    };
    let distinct = |n: usize, count: usize| assert_eq!(n, count, "a class without a row");
    distinct(
        formats
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        format_count(F::Corrupt),
    );
    distinct(
        containers
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        container_count(C::Io),
    );
    distinct(
        storages
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        storage_count(S::Io),
    );
}

/// Asserts each row's code, directly and behind `Error::wrap`, whose message is the source's.
fn check<E>(rows: Vec<(ErrorCode, E)>)
where
    E: Classified + std::error::Error + Send + Sync + 'static,
{
    use std::error::Error as _;
    for (expected, error) in rows {
        let shown = error.to_string();
        assert_eq!(error.code(), expected, "{shown}");
        let wrapped = Error::wrap(error);
        assert_eq!(wrapped.code(), expected, "{shown}");
        assert_eq!(wrapped.message(), shown);
        assert_eq!(wrapped.source().map(ToString::to_string), Some(shown));
    }
}

fn document_rows() -> Vec<(ErrorCode, DocumentError)> {
    let field = || "f".to_owned();
    vec![
        (InvalidArgument, DocumentError::Json("eof".into())),
        (InvalidArgument, DocumentError::IdLen { len: 0 }),
        (Limit, DocumentError::IdLen { len: 513 }),
        (
            InvalidArgument,
            DocumentError::UnknownField { name: field() },
        ),
        (
            InvalidArgument,
            DocumentError::FieldName {
                name: field(),
                problem: NameProblem::Empty,
            },
        ),
        (
            InvalidArgument,
            DocumentError::ExtraIsSchemaField { name: field() },
        ),
        (
            InvalidArgument,
            DocumentError::UnknownFieldId { id: field_id() },
        ),
        (
            InvalidArgument,
            DocumentError::WrongType {
                field: field(),
                expected: "a string",
                found: "a number",
            },
        ),
        (
            InvalidArgument,
            DocumentError::OutOfRange {
                field: field(),
                value: "NaN".into(),
                allowed: "finite",
            },
        ),
        (
            InvalidArgument,
            DocumentError::InvalidDatetime {
                field: field(),
                value: "yesterday".into(),
            },
        ),
        (
            Limit,
            DocumentError::KeywordTooLong {
                field: field(),
                len: 40_000,
            },
        ),
        (
            InvalidArgument,
            DocumentError::NullStored { field: field() },
        ),
        (Limit, DocumentError::TooDeep { field: field() }),
        (Limit, DocumentError::TooManyValues { count: 2 << 20 }),
        (Limit, DocumentError::TooManyFields { count: 1025 }),
        (Limit, DocumentError::TooLarge { len: 17 << 20 }),
        (InvalidArgument, DocumentError::Blob("no size".into())),
    ]
}

#[test]
fn document_errors() {
    check(document_rows());
}

#[test]
fn schema_and_evolution_errors() {
    let name = || "f".to_owned();
    check(vec![
        (InvalidArgument, SchemaError::Json("eof".into())),
        (
            InvalidArgument,
            SchemaError::CollectionName {
                name: name(),
                problem: NameProblem::Empty,
            },
        ),
        (
            InvalidArgument,
            SchemaError::FieldName {
                name: name(),
                problem: NameProblem::Empty,
            },
        ),
        (
            InvalidArgument,
            SchemaError::DuplicateField { name: name() },
        ),
        (Limit, SchemaError::TooManyFields { count: 1025 }),
        (
            InvalidArgument,
            SchemaError::FieldIds {
                field_count: 2,
                problem: "gap".into(),
            },
        ),
        (
            InvalidArgument,
            SchemaError::OutOfRange {
                option: "fields.f.boost".into(),
                value: "0".into(),
                allowed: "0 < boost <= 1000",
            },
        ),
        (
            InvalidArgument,
            SchemaError::UnknownAnalyzer {
                field: name(),
                analyzer: "xx".into(),
            },
        ),
        (
            Unsupported,
            SchemaError::UnsupportedAnalyzer {
                field: name(),
                analyzer: "es@9".into(),
            },
        ),
        (
            InvalidArgument,
            SchemaError::StaticRankField {
                field: name(),
                problem: "not numeric",
            },
        ),
        (
            InvalidArgument,
            SchemaError::Levels {
                problem: "repeated",
            },
        ),
        (
            InvalidArgument,
            SchemaError::Synonyms {
                group: 0,
                problem: "one term".into(),
            },
        ),
    ]);
    check(vec![
        (
            InvalidArgument,
            EvolutionError::CollectionRenamed {
                current: "a".into(),
                proposed: "b".into(),
            },
        ),
        (
            InvalidArgument,
            EvolutionError::FieldRemoved { field: name() },
        ),
    ]);
}

fn read_rows() -> Vec<(ErrorCode, ReadError)> {
    format_error!(corrupt, corrupt);
    format_error!(unsupported, unsupported);
    format_error!(corrupt, corrupt_block);
    vec![
        (
            Corrupt,
            ReadError::Corrupt {
                offset: 0,
                reason: SegmentCorruption::BadMagic,
            },
        ),
        (
            Corrupt,
            ReadError::Section {
                kind: SectionKind::TermDict,
                field: 0,
                position: 0,
                reason: SegmentCorruption::BadMagic,
            },
        ),
        (
            Corrupt,
            ReadError::Block {
                kind: SectionKind::TermDict,
                field: 0,
                source: corrupt_block,
            },
        ),
        (Corrupt, ReadError::Format(corrupt)),
        (Unsupported, ReadError::Format(unsupported)),
        (
            Unsupported,
            ReadError::Unsupported {
                offset: 0,
                reason: UnsupportedSegment::FormatMajor { major: 9 },
            },
        ),
        (
            Corrupt,
            ReadError::Document {
                docid: 0,
                source: DecodeError::Corrupt {
                    offset: 0,
                    reason: DocumentCorruption::UnexpectedEof,
                },
            },
        ),
        (
            Unsupported,
            ReadError::Document {
                docid: 0,
                source: DecodeError::UnsupportedVersion { version: 2 },
            },
        ),
        (
            Corrupt,
            ReadError::Tombstone {
                docid: 9,
                doc_count: 1,
            },
        ),
        (
            FALLBACK,
            ReadError::DocOutOfRange {
                docid: 9,
                doc_count: 1,
            },
        ),
        (
            FALLBACK,
            ReadError::FieldType {
                field: field_id(),
                field_type: FieldType::Keyword,
                what: "postings",
            },
        ),
        (
            FALLBACK,
            ReadError::OrdinalOutOfRange { ordinal: 9, len: 1 },
        ),
        (FALLBACK, ReadError::ForeignTermInfo),
        (FALLBACK, ReadError::UnknownField { field: field_id() }),
        (Cancelled, ReadError::Interrupted(Interrupt::Cancelled)),
        (
            Cancelled,
            ReadError::Interrupted(Interrupt::DeadlineExceeded),
        ),
    ]
}

#[test]
fn read_decode_and_sidecar_errors() {
    check(read_rows());
    check(vec![
        (Corrupt, blob_ref_error(0)),
        (Unsupported, blob_ref_error(0x17)),
        (
            Corrupt,
            DecodeError::Corrupt {
                offset: 0,
                reason: DocumentCorruption::UnexpectedEof,
            },
        ),
        (Unsupported, DecodeError::UnsupportedVersion { version: 2 }),
    ]);
    check(vec![
        (Corrupt, SidecarError::Corrupt { what: "magic" }),
        (Unsupported, SidecarError::UnsupportedVersion { version: 2 }),
        (FALLBACK, SidecarError::Invalid { what: "version 0" }),
    ]);
    check(vec![
        (Cancelled, Interrupt::Cancelled),
        (Cancelled, Interrupt::DeadlineExceeded),
    ]);
}

fn segment_rows() -> Vec<(ErrorCode, SegmentError)> {
    let mut rows = vec![
        (
            InvalidArgument,
            SegmentError::UnknownAnalyzer {
                field: "f".into(),
                analyzer: "xx@1".into(),
            },
        ),
        (FALLBACK, SegmentError::DuplicateId { id: "a".into() }),
        (Limit, SegmentError::Full { max_docs: 1 << 31 }),
        (FALLBACK, SegmentError::Empty),
        (
            Limit,
            SegmentError::TermTooLong {
                field: field_id(),
                len: 40_000,
            },
        ),
        (Limit, SegmentError::TooLarge { what: "terms" }),
        (FALLBACK, SegmentError::Internal { what: "order" }),
        (FALLBACK, SegmentError::Encode(encode_error!())),
    ];
    // A rejected document keeps its own code.
    rows.extend(
        document_rows()
            .into_iter()
            .map(|(code, e)| (code, SegmentError::Document(e))),
    );
    rows
}

fn memtable_rows() -> Vec<(ErrorCode, MemtableError)> {
    let mut rows = vec![(Poisoned, MemtableError::Poisoned)];
    rows.extend(
        segment_rows()
            .into_iter()
            .map(|(code, e)| (code, MemtableError::Segment(e))),
    );
    rows
}

#[test]
fn segment_memtable_and_wal_errors() {
    check(segment_rows());
    check(memtable_rows());
    check(vec![
        (
            InvalidArgument,
            RecordError::CollectionName {
                name: "C".into(),
                problem: NameProblem::Empty,
            },
        ),
        (Limit, RecordError::DocumentLen { len: 17 << 20 }),
        (InvalidArgument, RecordError::IdLen { len: 0 }),
        (Limit, RecordError::IdLen { len: 513 }),
    ]);
    check(vec![
        (
            Corrupt,
            WalError::Corrupt {
                offset: 0,
                reason: WalCorruption::UnexpectedEof,
            },
        ),
        (Unsupported, WalError::UnsupportedVersion { version: 2 }),
    ]);
    let mut replay = vec![
        (
            Corrupt,
            ReplayError::Chunk {
                chunk: 0,
                error: WalError::Corrupt {
                    offset: 0,
                    reason: WalCorruption::UnexpectedEof,
                },
            },
        ),
        (
            Unsupported,
            ReplayError::Chunk {
                chunk: 0,
                error: WalError::UnsupportedVersion { version: 2 },
            },
        ),
        (
            Corrupt,
            ReplayError::UnknownCollection {
                chunk: 0,
                offset: 0,
                collection: "gone".into(),
            },
        ),
        (
            Corrupt,
            ReplayError::Document {
                chunk: 0,
                offset: 0,
                error: blob_ref_error(0),
            },
        ),
        (
            Unsupported,
            ReplayError::Document {
                chunk: 0,
                offset: 0,
                error: DecodeError::UnsupportedVersion { version: 2 },
            },
        ),
        (
            Poisoned,
            ReplayError::Memtable {
                chunk: 0,
                offset: 0,
                error: MemtableError::Poisoned,
            },
        ),
        (
            Limit,
            ReplayError::Memtable {
                chunk: 0,
                offset: 0,
                error: MemtableError::Segment(SegmentError::Full { max_docs: 1 }),
            },
        ),
    ];
    replay.extend(
        container_cases()
            .into_iter()
            .map(|(code, e)| (code, ReplayError::Container(e))),
    );
    check(replay);
}

#[test]
fn manifest_errors() {
    check(vec![
        (
            Corrupt,
            ManifestError::Corrupt {
                offset: 0,
                reason: ManifestCorruption::UnexpectedEof,
            },
        ),
        (
            Unsupported,
            ManifestError::UnsupportedVersion { version: 2 },
        ),
        (
            Unsupported,
            ManifestError::UnsupportedAnalyzer {
                offset: 0,
                field: "f".into(),
                analyzer: "es@9".into(),
            },
        ),
    ]);
    check(vec![
        (Limit, ManifestEncodeError::TooLarge { len: 1 << 30 }),
        (
            FALLBACK,
            ManifestEncodeError::Schema(SchemaError::Json("eof".into())),
        ),
    ]);
    // One code for every variant: a diff the database planned itself and that does not apply is a bug.
    check(vec![
        (FALLBACK, ApplyError::SeqOverflow),
        (FALLBACK, ApplyError::CollectionExists { name: "c".into() }),
        (FALLBACK, ApplyError::NoSuchCollection { name: "c".into() }),
    ]);
    check(vec![(
        Corrupt,
        LayoutError {
            layout: Layout::Container,
            collection: "c".into(),
            segment: segment_id(),
        },
    )]);
}

fn database_rows() -> Vec<(ErrorCode, DatabaseError)> {
    let c = || "c".to_owned();
    let mut rows = vec![
        (
            Corrupt,
            DatabaseError::Manifest(ManifestError::Corrupt {
                offset: 0,
                reason: ManifestCorruption::UnexpectedEof,
            }),
        ),
        (
            Unsupported,
            DatabaseError::Manifest(ManifestError::UnsupportedVersion { version: 2 }),
        ),
        (
            Limit,
            DatabaseError::ManifestEncode(ManifestEncodeError::TooLarge { len: 1 << 30 }),
        ),
        (FALLBACK, DatabaseError::Apply(ApplyError::SeqOverflow)),
        (
            Corrupt,
            DatabaseError::Layout(LayoutError {
                layout: Layout::Container,
                collection: c(),
                segment: segment_id(),
            }),
        ),
        (
            Corrupt,
            DatabaseError::SlotMismatch {
                field: "seq",
                manifest: 1,
                superblock: 2,
            },
        ),
        (
            Corrupt,
            DatabaseError::SegmentMismatch {
                collection: c(),
                id: segment_id(),
                field: "doc_count",
            },
        ),
        (
            Corrupt,
            DatabaseError::Sidecar {
                collection: c(),
                id: segment_id(),
                source: SidecarError::Corrupt { what: "magic" },
            },
        ),
        (
            Unsupported,
            DatabaseError::Sidecar {
                collection: c(),
                id: segment_id(),
                source: SidecarError::UnsupportedVersion { version: 2 },
            },
        ),
        (
            Corrupt,
            DatabaseError::SidecarMismatch {
                collection: c(),
                id: segment_id(),
                field: "deleted",
            },
        ),
        (
            Corrupt,
            DatabaseError::LiveTwice {
                collection: c(),
                id: "a".into(),
            },
        ),
        (Corrupt, DatabaseError::CorruptTail { offset: 12_288 }),
        (
            Unsupported,
            DatabaseError::UnregisteredAnalyzer {
                collection: c(),
                field: "f".into(),
                analyzer: "mine@1".into(),
            },
        ),
        (
            Conflict,
            DatabaseError::RefreshRefused {
                other_database: true,
                current: 3,
                found: 1,
            },
        ),
        (
            Conflict,
            DatabaseError::RefreshRefused {
                other_database: false,
                current: 3,
                found: 1,
            },
        ),
        (
            InvalidArgument,
            DatabaseError::BatchRejected {
                index: 2,
                source: MemtableError::Segment(SegmentError::Document(DocumentError::Json(
                    "eof".into(),
                ))),
            },
        ),
        (
            Limit,
            DatabaseError::BatchRejected {
                index: 2,
                source: MemtableError::Segment(SegmentError::Document(DocumentError::TooLarge {
                    len: 17 << 20,
                })),
            },
        ),
        (
            Limit,
            DatabaseError::Record(RecordError::DocumentLen { len: 17 << 20 }),
        ),
        (
            InvalidArgument,
            DatabaseError::Record(RecordError::IdLen { len: 0 }),
        ),
        (Poisoned, DatabaseError::Wal(WalWriteError::Poisoned)),
        (
            Limit,
            DatabaseError::Wal(WalWriteError::ChunkFull(ChunkFull)),
        ),
        (
            Corrupt,
            DatabaseError::Replay(ReplayError::UnknownCollection {
                chunk: 0,
                offset: 0,
                collection: c(),
            }),
        ),
        (NotFound, DatabaseError::NoSuchCollection { name: c() }),
        (AlreadyExists, DatabaseError::CollectionExists { name: c() }),
        (
            InvalidArgument,
            DatabaseError::Evolution {
                collection: c(),
                source: EvolutionError::FieldRemoved { field: "f".into() },
            },
        ),
        (FALLBACK, DatabaseError::Internal { what: "order" }),
        (Io, DatabaseError::MergeThread(os_error())),
        (Poisoned, DatabaseError::Poisoned),
        // The first failure decides, whatever followed it, unless what followed is corruption.
        (
            NotFound,
            DatabaseError::ThenFailed {
                error: Box::new(DatabaseError::NoSuchCollection { name: c() }),
                step: "syncing the WAL",
                later: Box::new(DatabaseError::Poisoned),
            },
        ),
        (
            Corrupt,
            DatabaseError::ThenFailed {
                error: Box::new(DatabaseError::NoSuchCollection { name: c() }),
                step: "syncing the WAL",
                later: Box::new(DatabaseError::CorruptTail { offset: 0 }),
            },
        ),
        (
            Corrupt,
            DatabaseError::ThenFailed {
                error: Box::new(DatabaseError::ThenFailed {
                    error: Box::new(DatabaseError::Record(RecordError::DocumentLen { len: 0 })),
                    step: "a",
                    later: Box::new(DatabaseError::Poisoned),
                }),
                step: "b",
                later: Box::new(DatabaseError::CorruptTail { offset: 0 }),
            },
        ),
    ];
    for (code, source) in read_rows() {
        rows.push((
            code,
            DatabaseError::OpenSegment {
                collection: c(),
                id: segment_id(),
                source: source.clone(),
            },
        ));
        rows.push((
            code,
            DatabaseError::ReadSegment {
                collection: c(),
                id: segment_id(),
                source,
            },
        ));
    }
    rows.extend(
        segment_rows()
            .into_iter()
            .map(|(code, e)| (code, DatabaseError::Segment(e))),
    );
    rows.extend(
        memtable_rows()
            .into_iter()
            .map(|(code, e)| (code, DatabaseError::Memtable(e))),
    );
    rows.extend(
        container_cases()
            .into_iter()
            .map(|(code, e)| (code, DatabaseError::Container(e))),
    );
    rows.extend(
        storage_cases()
            .into_iter()
            .map(|(code, _, e)| (code, DatabaseError::Storage(e))),
    );
    rows
}

#[test]
fn database_errors() {
    check(database_rows());
}

fn blob_rows() -> Vec<(ErrorCode, persia_blob::Error)> {
    use persia_blob::Error as B;
    format_error!(unsupported, unsupported);
    let invalid_chunking = match persia_blob::ChunkSize::from_encoding(0) {
        Err(e @ B::InvalidChunking(_)) => e,
        other => panic!("a chunk size of 0 is invalid chunking: {other:?}"),
    };
    let token = persia_blob::ResumeToken {
        upload_id: UploadId::new(1),
        chunk_size: persia_blob::ChunkSize::DEFAULT,
    };
    let mut rows = vec![
        (InvalidArgument, invalid_chunking),
        (
            InvalidArgument,
            B::ChunkSizeBelowMin {
                chunk_size: 1,
                min: 64 << 10,
            },
        ),
        (Io, B::Read(os_error())),
        (FALLBACK, B::ChunkerFailed),
        (
            Corrupt,
            B::Corrupt {
                encoding: "blob manifest",
                what: "magic",
            },
        ),
        (
            Unsupported,
            B::UnsupportedVersion {
                encoding: "blob manifest",
                version: 2,
            },
        ),
        (
            Unsupported,
            B::UnsupportedFlags {
                encoding: "blob pack",
                flags: 8,
            },
        ),
        (FALLBACK, B::InvalidManifest { what: "no chunks" }),
        (NotFound, B::BlobNotFound { blob_id: blob_id() }),
        (
            InvalidArgument,
            B::RangeOutOfBounds {
                start: 9,
                end: 1,
                size: 4,
            },
        ),
        (
            Conflict,
            B::StagedChunkConflict {
                upload_id: UploadId::new(1),
                index: 0,
            },
        ),
        (Transient, B::BindOutcomeUnknown(Box::new(os_error()))),
        (Poisoned, B::BindOutcomeUnknown(Box::new(B::PackPoisoned))),
        (
            Io,
            B::PackIo {
                op: "write",
                source: os_error(),
            },
        ),
        (Poisoned, B::PackPoisoned),
        (ReadOnly, B::PackReadOnly),
        (Unsupported, B::PackUnsupported(unsupported)),
        (
            Conflict,
            B::PackReplaced {
                opened: 1,
                found: 2,
            },
        ),
        (Conflict, B::PackOfAnotherDatabase { found: [9; 16] }),
        (
            FALLBACK,
            B::ChunkNotStaged {
                upload_id: UploadId::new(1),
                index: 0,
            },
        ),
        (Unsupported, B::ConditionalPutUnsupported),
        (InvalidArgument, B::InvalidResume { what: "short" }),
        (
            InvalidArgument,
            B::BlobIdMismatch {
                expected: blob_id(),
                actual: blob_id_of(b"other"),
            },
        ),
        (InvalidArgument, B::InvalidBlobRef(encode_error!())),
        (
            InvalidArgument,
            B::BindBudgetAboveMax {
                budget: Duration::from_secs(7200),
                max: Duration::from_secs(3600),
            },
        ),
        (
            Corrupt,
            B::CorruptAt {
                encoding: "blob pack",
                what: "crc",
                offset: 4096,
            },
        ),
        (Conflict, B::PackLocked),
        // What stopped the upload decides.
        (
            Io,
            B::UploadInterrupted {
                token,
                source: Box::new(B::Read(os_error())),
            },
        ),
        (
            Corrupt,
            B::UploadInterrupted {
                token,
                source: Box::new(B::Corrupt {
                    encoding: "blob chunk",
                    what: "crc",
                }),
            },
        ),
        (Unsupported, B::ResumeUnsupported),
        (InvalidArgument, B::ResumeUnchecked),
        (
            InvalidArgument,
            B::BlobSizeMismatch {
                expected: 4,
                actual: 5,
            },
        ),
        (Transient, B::LeaseContended { attempts: 3 }),
    ];
    rows.extend(
        container_cases()
            .into_iter()
            .map(|(code, e)| (code, B::Container(e))),
    );
    rows.extend(
        storage_cases()
            .into_iter()
            .map(|(code, _, e)| (code, B::Storage(e))),
    );
    rows
}

#[test]
fn blob_and_documents_errors() {
    check(blob_rows());
    let seqs = (3, 2, 5, 4, 900, 800);
    let mut rows = vec![
        (
            NotFound,
            DocumentsError::Open {
                path: PathBuf::from("photo.jpg"),
                source: io::Error::from(io::ErrorKind::NotFound),
            },
        ),
        (
            Io,
            DocumentsError::Open {
                path: PathBuf::from("photo.jpg"),
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            },
        ),
        (
            NotFound,
            DocumentsError::BlobNotStored { blob_id: blob_id() },
        ),
        (
            AlreadyExists,
            DocumentsError::PackExists {
                path: PathBuf::from("app.persia.blobs"),
            },
        ),
        (
            Conflict,
            DocumentsError::RefreshedElsewhere {
                current: None,
                reopened: Some(PathBuf::from("copy.persia")),
            },
        ),
        (
            Conflict,
            DocumentsError::RefreshedOlder {
                current: seqs.0,
                reopened: seqs.1,
                current_replayed: seqs.2,
                reopened_replayed: seqs.3,
                current_end: seqs.4,
                reopened_end: seqs.5,
            },
        ),
        (
            Conflict,
            DocumentsError::RefreshDiverged {
                current: seqs.0,
                reopened: seqs.0,
                current_replayed: seqs.2,
                reopened_replayed: seqs.3,
                current_end: seqs.5,
                reopened_end: seqs.4,
            },
        ),
        (
            Transient,
            DocumentsError::OpenedBeforeLease {
                opened: 1,
                taken: 2,
            },
        ),
        (
            Conflict,
            DocumentsError::LeaseElsewhere {
                lease: PathBuf::from("a.leases"),
                expected: PathBuf::from("b.leases"),
            },
        ),
        (FALLBACK, DocumentsError::NoContainerPath),
    ];
    rows.extend(
        blob_rows()
            .into_iter()
            .map(|(code, e)| (code, DocumentsError::Blob(e))),
    );
    rows.extend(
        database_rows()
            .into_iter()
            .map(|(code, e)| (code, DocumentsError::Database(Box::new(e)))),
    );
    check(rows);
}

fn query_rows() -> Vec<(ErrorCode, QueryError)> {
    let field = || "f".to_owned();
    vec![
        (InvalidArgument, QueryError::UnknownField { field: field() }),
        (
            InvalidArgument,
            QueryError::UnknownFieldId { field: field_id() },
        ),
        (
            InvalidArgument,
            QueryError::WrongFieldType {
                field: field(),
                field_type: FieldType::Keyword,
                what: "a phrase",
            },
        ),
        (InvalidArgument, QueryError::NoPositions { field: field() }),
        (
            Limit,
            QueryError::TermTooLong {
                field: field(),
                len: 40_000,
            },
        ),
        (InvalidArgument, QueryError::EmptyPrefix { field: field() }),
        (
            InvalidArgument,
            QueryError::InvalidPhrase { field: field() },
        ),
        (InvalidArgument, QueryError::EmptyCombined),
        (
            InvalidArgument,
            QueryError::DuplicateField { field: field() },
        ),
        (InvalidArgument, QueryError::InvalidBoost { value: 0.0 }),
        (InvalidArgument, QueryError::InvalidScore { value: -1.0 }),
        (InvalidArgument, QueryError::InvalidDistance { distance: 3 }),
        (InvalidArgument, QueryError::NoExpansions),
        (Limit, QueryError::TooManyTerms { limit: 64 }),
        (Limit, QueryError::TooManyClauses { limit: 64 }),
        (Limit, QueryError::TooManyExpansions { limit: 64 }),
        (Limit, QueryError::TooDeep { limit: 8 }),
        (
            InvalidArgument,
            QueryError::UnknownAnalyzer {
                field: field(),
                analyzer: "xx@1".into(),
            },
        ),
        (
            InvalidArgument,
            QueryError::InvalidFieldSpec { spec: "^".into() },
        ),
        (InvalidArgument, QueryError::FilterSyntax("eof".into())),
        (
            InvalidArgument,
            QueryError::FilterValue {
                field: field(),
                expected: "an integer",
                found: "a string".into(),
            },
        ),
        (
            InvalidArgument,
            QueryError::InvalidRange {
                field: field(),
                problem: "no bounds",
            },
        ),
    ]
}

fn request_rows() -> Vec<(ErrorCode, RequestError)> {
    let mut rows = vec![
        (
            InvalidArgument,
            RequestError::SortField {
                field: field_id(),
                reason: "no column",
            },
        ),
        (Limit, RequestError::TooManySortKeys { count: 9, max: 8 }),
        (
            Limit,
            RequestError::Window {
                offset: 10_000,
                limit: 10,
                max: 10_000,
            },
        ),
        (
            InvalidArgument,
            RequestError::Cursor {
                reason: "one value",
            },
        ),
        (
            InvalidArgument,
            RequestError::CursorView {
                cursor_seq: 1,
                cursor_version: 1,
                cursor_collection: "a".into(),
                seq: 2,
                version: 1,
                collection: "a".into(),
            },
        ),
        (
            InvalidArgument,
            RequestError::CursorProximity {
                cursor: true,
                plan: false,
            },
        ),
        (
            InvalidArgument,
            RequestError::CursorRelax {
                cursor: None,
                search: None,
            },
        ),
        (
            InvalidArgument,
            RequestError::CursorStep {
                step: 3,
                last: 2,
                reason: "past the cascade",
            },
        ),
        (FALLBACK, RequestError::MismatchedSortValue),
        (
            FALLBACK,
            RequestError::HitSegment {
                index: 3,
                segments: 1,
            },
        ),
        (
            InvalidArgument,
            RequestError::FacetField {
                field: field_id(),
                reason: "not a facet",
            },
        ),
        (Limit, RequestError::TooManyFacets { count: 99, max: 16 }),
        (Limit, RequestError::FacetSize { size: 0, max: 1000 }),
        (
            InvalidArgument,
            RequestError::Projection {
                name: "f".into(),
                reason: "not stored",
            },
        ),
        (Cancelled, RequestError::Interrupted(Interrupt::Cancelled)),
        (
            Cancelled,
            RequestError::Interrupted(Interrupt::DeadlineExceeded),
        ),
    ];
    rows.extend(
        read_rows()
            .into_iter()
            .map(|(code, e)| (code, RequestError::Read(e))),
    );
    rows
}

fn search_rows() -> Vec<(ErrorCode, SearchError)> {
    let mut rows = vec![
        (
            InvalidArgument,
            SearchError::Score(ScoreError::Field {
                field: field_id(),
                what: "text",
            }),
        ),
        (
            InvalidArgument,
            SearchError::Score(ScoreError::Params { field: field_id() }),
        ),
        (InvalidArgument, SearchError::Score(ScoreError::NoFields)),
        (FALLBACK, SearchError::NeedsTerms { what: "prefix" }),
        (Cancelled, SearchError::Interrupted(Interrupt::Cancelled)),
    ];
    for (code, e) in read_rows() {
        rows.push((code, SearchError::Score(ScoreError::Read(e.clone()))));
        rows.push((code, SearchError::Read(e)));
    }
    rows.extend(
        query_rows()
            .into_iter()
            .map(|(code, e)| (code, SearchError::Query(e))),
    );
    rows
}

#[test]
fn request_errors() {
    assert_eq!(REQUEST_FALLBACK, InvalidArgument);
    check(query_rows());
    check(request_rows());
    check(search_rows());
    let mut relaxed: Vec<(ErrorCode, RelaxedSearchError)> = search_rows()
        .into_iter()
        .map(|(code, e)| (code, RelaxedSearchError::Plan(e)))
        .collect();
    relaxed.extend(
        request_rows()
            .into_iter()
            .map(|(code, e)| (code, RelaxedSearchError::Request(e))),
    );
    check(relaxed);
    let relax: Vec<(ErrorCode, RelaxError)> = [
        r#"{"min_results": 0}"#,
        r#"{"levels": ["fuzzy", "synonyms"]}"#,
        r#"{"minimum": 5}"#,
    ]
    .into_iter()
    .map(|json| (InvalidArgument, RelaxOptions::from_json(json).unwrap_err()))
    .collect();
    assert!(matches!(relax[0].1, RelaxError::OutOfRange { .. }));
    assert!(matches!(relax[1].1, RelaxError::Levels { .. }));
    assert!(matches!(relax[2].1, RelaxError::Json(_)));
    check(relax);
}

/// The whole request (ROADMAP 10.1.10): each wrapped error keeps its code, a deadline or cancellation anywhere in it
/// is `CANCELLED`, and every way a cursor token is not one is the caller's.
#[test]
fn query_request_and_cursor_token_errors() {
    use persia_engine::db::Cursor;
    use persia_engine::search::request::{CursorTokenError, QueryRequestError};

    let tokens: Vec<(ErrorCode, CursorTokenError)> = ["", "!", "AAAA", &"A".repeat(1 << 20)]
        .into_iter()
        .map(|t| (InvalidArgument, Cursor::from_token(t).unwrap_err()))
        .collect();
    check(tokens.clone());
    let mut rows: Vec<(ErrorCode, QueryRequestError)> = vec![
        (InvalidArgument, QueryRequestError::Json("not json".into())),
        (InvalidArgument, QueryRequestError::ProximityRelaxed),
        (
            InvalidArgument,
            QueryRequestError::Schema {
                collection: "c".to_owned(),
            },
        ),
    ];
    rows.extend(
        tokens
            .into_iter()
            .map(|(code, e)| (code, QueryRequestError::Cursor(e))),
    );
    rows.extend(
        query_rows()
            .into_iter()
            .map(|(code, e)| (code, QueryRequestError::Query(e))),
    );
    rows.extend(
        request_rows()
            .into_iter()
            .map(|(code, e)| (code, QueryRequestError::Request(e))),
    );
    rows.extend(
        search_rows()
            .into_iter()
            .map(|(code, e)| (code, QueryRequestError::Plan(e))),
    );
    rows.push((
        InvalidArgument,
        QueryRequestError::Relax(RelaxOptions::from_json(r#"{"min_results": 0}"#).unwrap_err()),
    ));
    for interrupt in [Interrupt::DeadlineExceeded, Interrupt::Cancelled] {
        rows.push((
            Cancelled,
            QueryRequestError::Request(RequestError::Interrupted(interrupt)),
        ));
        rows.push((
            Cancelled,
            QueryRequestError::Plan(SearchError::Interrupted(interrupt)),
        ));
    }
    check(rows);
}
