//! The error type of the embedded API and its stable codes (ROADMAP 10.1.5, ADR-0064).
//!
//! Every fallible call of this crate returns [`Result`], whose error is the opaque [`Error`]: a stable
//! [`ErrorCode`], a message for people, and the underlying error as [`source`](std::error::Error::source). The
//! engine's, the blob store's and the storage layer's own error types are not part of the public API: they are
//! reachable only through `source()`, as `dyn Error`.
//!
//! The codes are the numbers of the C ABI's error table (`docs/sdk/c-abi.md`), so a language binding passes
//! [`ErrorCode::number`] through unchanged. They are append-only: a released number never changes its meaning.

mod classify;

use std::fmt;

pub(crate) use classify::Classified;

/// A boxed error kept as an [`Error`]'s source.
type Source = Box<dyn std::error::Error + Send + Sync + 'static>;

/// `std::result::Result` with this crate's [`Error`].
///
/// ```
/// fn collections(db: &persia::Db) -> persia::Result<bool> {
///     Ok(db.is_read_only())
/// }
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// # let db = persia::Db::open(dir.path().join("app.persia"))?;
/// # assert!(!collections(&db)?);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub type Result<T> = std::result::Result<T, Error>;

/// What kind of failure an [`Error`] is: the stable part of an error, to branch on.
///
/// The discriminants are the numbers of the C ABI's error table (`docs/sdk/c-abi.md`) and [`ErrorCode::as_str`]
/// gives the names there. `0` (`PERSIA_OK`) is success and has no variant. The list is append-only, and the enum is
/// `#[non_exhaustive]`: match with a wildcard arm.
///
/// ```
/// use persia::ErrorCode;
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// let missing = dir.path().join("missing.persia");
/// let err = persia::OpenOptions::new().create_if_missing(false).open(&missing).unwrap_err();
/// assert_eq!(err.code(), ErrorCode::NotFound);
/// assert_eq!((err.code().number(), err.code().as_str()), (1, "PERSIA_NOT_FOUND"));
/// match err.code() {
///     ErrorCode::NotFound => {} // create it, or tell the user
///     ErrorCode::Transient => {} // the only code to retry blindly
///     _ => {} // the enum is `#[non_exhaustive]`: codes are appended
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// `1 PERSIA_NOT_FOUND`: a missing collection or blob, or a database file this open does not create
    /// (SPEC §8.1). A missing document is not an error in Rust: `get` returns `None`.
    NotFound,
    /// `2 PERSIA_ALREADY_EXISTS`: creating a collection that is already there. Opening an existing database is
    /// not this code: the open opens it.
    AlreadyExists,
    /// `3 PERSIA_CONFLICT`: another writer holds the database (SPEC §8.4), or the file at the path is no longer
    /// the database a read-only handle follows (a refresh that was refused).
    Conflict,
    /// `4 PERSIA_INVALID_ARGUMENT`: the caller's input was rejected: a malformed path or URL, schema, document or
    /// query, or a value outside its range.
    InvalidArgument,
    /// `5 PERSIA_CORRUPT`: stored bytes failed a checksum or a format rule.
    Corrupt,
    /// `6 PERSIA_UNSUPPORTED`: a format version or feature this build does not read (SPEC §15), an analyzer it
    /// lacks, or a URL scheme it has no backend for.
    Unsupported,
    /// `7 PERSIA_IO`: a file system failure that is not a conflict. Also the code of a failure that fits no other
    /// code: memory that could not be allocated, or an internal invariant that did not hold (a bug, reported
    /// rather than acted on).
    Io,
    /// `8 PERSIA_TRANSIENT`: a failure that may go away if the call is repeated (SPEC §10.1): the only code to
    /// retry blindly.
    Transient,
    /// `9 PERSIA_BUSY`: reserved for the C ABI (a close while child handles are open). Never returned from Rust.
    Busy,
    /// `10 PERSIA_PANIC`: reserved for the C ABI (a Rust panic caught at the boundary). Never returned from Rust.
    Panic,
    /// `11 PERSIA_CLOSED`: a call on a handle whose database was closed or dropped.
    Closed,
    /// `12 PERSIA_LIMIT`: a limit of SPEC §17 was exceeded: document size, field count, term length, query terms,
    /// expansions, the result window.
    Limit,
    /// `13 PERSIA_READ_ONLY`: a write on a database opened read-only.
    ReadOnly,
    /// `14 PERSIA_POISONED`: an earlier write, sync or commit failed (or a thread panicked while writing), so the
    /// writer stopped: what reached the disk is known only after a reopen. Close the database and open it again.
    Poisoned,
    /// `15 PERSIA_CANCELLED`: the call was cancelled or ran past its deadline.
    Cancelled,
}

impl ErrorCode {
    /// Every code, in ascending [`ErrorCode::number`].
    ///
    /// ```
    /// use persia::ErrorCode;
    ///
    /// assert_eq!(ErrorCode::ALL.len(), 15);
    /// assert!(ErrorCode::ALL.windows(2).all(|pair| pair[0].number() < pair[1].number()));
    /// ```
    pub const ALL: [Self; 15] = [
        Self::NotFound,
        Self::AlreadyExists,
        Self::Conflict,
        Self::InvalidArgument,
        Self::Corrupt,
        Self::Unsupported,
        Self::Io,
        Self::Transient,
        Self::Busy,
        Self::Panic,
        Self::Closed,
        Self::Limit,
        Self::ReadOnly,
        Self::Poisoned,
        Self::Cancelled,
    ];

    /// The code's number in the C ABI's error table: stable, never reused, never `0` (which is success).
    ///
    /// ```
    /// assert_eq!(persia::ErrorCode::Conflict.number(), 3);
    /// ```
    #[must_use]
    pub const fn number(self) -> i32 {
        match self {
            Self::NotFound => 1,
            Self::AlreadyExists => 2,
            Self::Conflict => 3,
            Self::InvalidArgument => 4,
            Self::Corrupt => 5,
            Self::Unsupported => 6,
            Self::Io => 7,
            Self::Transient => 8,
            Self::Busy => 9,
            Self::Panic => 10,
            Self::Closed => 11,
            Self::Limit => 12,
            Self::ReadOnly => 13,
            Self::Poisoned => 14,
            Self::Cancelled => 15,
        }
    }

    /// The code's name in the C ABI's error table, such as `PERSIA_NOT_FOUND`: stable, like the number.
    ///
    /// ```
    /// assert_eq!(persia::ErrorCode::ReadOnly.as_str(), "PERSIA_READ_ONLY");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "PERSIA_NOT_FOUND",
            Self::AlreadyExists => "PERSIA_ALREADY_EXISTS",
            Self::Conflict => "PERSIA_CONFLICT",
            Self::InvalidArgument => "PERSIA_INVALID_ARGUMENT",
            Self::Corrupt => "PERSIA_CORRUPT",
            Self::Unsupported => "PERSIA_UNSUPPORTED",
            Self::Io => "PERSIA_IO",
            Self::Transient => "PERSIA_TRANSIENT",
            Self::Busy => "PERSIA_BUSY",
            Self::Panic => "PERSIA_PANIC",
            Self::Closed => "PERSIA_CLOSED",
            Self::Limit => "PERSIA_LIMIT",
            Self::ReadOnly => "PERSIA_READ_ONLY",
            Self::Poisoned => "PERSIA_POISONED",
            Self::Cancelled => "PERSIA_CANCELLED",
        }
    }

    /// The code with this number, if there is one: the reverse of [`ErrorCode::number`].
    ///
    /// ```
    /// use persia::ErrorCode;
    ///
    /// assert_eq!(ErrorCode::from_number(13), Some(ErrorCode::ReadOnly));
    /// assert_eq!(ErrorCode::from_number(0), None); // success is not an error
    /// ```
    #[must_use]
    pub fn from_number(number: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|code| code.number() == number)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a call of the embedded API failed.
///
/// Opaque: branch on [`Error::code`], show [`Error::message`] (or the `Display` form, which is the message) to
/// people, and walk [`source`](std::error::Error::source) for the underlying error. The message is not stable;
/// the code is. The source's concrete type belongs to the engine and is not part of the public API.
///
/// ```
/// use std::error::Error as _;
/// use persia::ErrorCode;
///
/// let err = persia::Db::open("file://example.com/app.persia").unwrap_err();
/// assert_eq!(err.code(), ErrorCode::InvalidArgument);
/// assert_eq!(err.to_string(), err.message());
/// assert!(err.source().is_none()); // found by this crate itself: nothing underneath
///
/// # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
/// # let dir = tempfile::tempdir()?;
/// let path = dir.path().join("notes.txt");
/// std::fs::write(&path, "not a database")?;
/// let err = persia::Db::open(&path).unwrap_err();
/// assert_eq!(err.code(), ErrorCode::Corrupt);
/// assert!(err.source().is_some()); // the engine's own error, as `dyn Error`
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Error {
    code: ErrorCode,
    message: String,
    source: Option<Source>,
}

impl Error {
    /// An error of `code` that this crate found itself: no source.
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source: None,
        }
    }

    /// `source`, an error of a lower layer, under the code its [`Classified`] impl gives it. The message is the
    /// source's.
    pub(crate) fn wrap<E>(source: E) -> Self
    where
        E: Classified + std::error::Error + Send + Sync + 'static,
    {
        Self {
            code: source.code(),
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    /// `source` under `code`, with `context` in front of its message.
    pub(crate) fn with_source(
        code: ErrorCode,
        context: impl fmt::Display,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            code,
            message: format!("{context}: {source}"),
            source: Some(Box::new(source)),
        }
    }

    /// This error with `context` in front of its message: what was being done.
    #[must_use]
    pub(crate) fn context(mut self, context: impl fmt::Display) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }

    /// A thread panicked while it held the writer: what its write left behind is unknown.
    pub(crate) fn writer_panicked() -> Self {
        Self::new(
            ErrorCode::Poisoned,
            "a thread panicked while writing to the database: close it and open it again",
        )
    }

    /// A call on a handle whose database was closed or dropped.
    #[cfg_attr(not(test), allow(dead_code))] // Returned by the collection handles (ROADMAP 10.1.2).
    pub(crate) fn closed() -> Self {
        Self::new(ErrorCode::Closed, "the database is closed")
    }

    /// A write on a database opened read-only.
    #[cfg_attr(not(test), allow(dead_code))] // Returned by the collection handles (ROADMAP 10.1.2).
    pub(crate) fn read_only(what: &str) -> Self {
        Self::new(
            ErrorCode::ReadOnly,
            format!("cannot {what}: the database is open read-only"),
        )
    }

    /// The stable code: what to branch on.
    ///
    /// ```
    /// use persia::ErrorCode;
    ///
    /// let err = persia::Db::open("s3://bucket/app").unwrap_err();
    /// assert_eq!(err.code(), ErrorCode::Unsupported);
    /// ```
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// A description for people. Not stable: do not parse it.
    ///
    /// ```
    /// let err = persia::Db::open("s3://bucket/app").unwrap_err();
    /// assert!(err.message().contains("s3"));
    /// ```
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("source", &self.source)
            .finish()
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // A coercion, not a cast: the source without its `Send + Sync` bounds.
        let source: &(dyn std::error::Error + 'static) = self.source.as_deref()?;
        Some(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The code after `code` in the table, `None` first and last. The match is exhaustive: a new, renamed or removed
    /// variant does not compile until this chain, and with it the snapshot below, is edited.
    const fn next(code: Option<ErrorCode>) -> Option<ErrorCode> {
        use ErrorCode::{
            AlreadyExists, Busy, Cancelled, Closed, Conflict, Corrupt, InvalidArgument, Io, Limit,
            NotFound, Panic, Poisoned, ReadOnly, Transient, Unsupported,
        };
        match code {
            None => Some(NotFound),
            Some(NotFound) => Some(AlreadyExists),
            Some(AlreadyExists) => Some(Conflict),
            Some(Conflict) => Some(InvalidArgument),
            Some(InvalidArgument) => Some(Corrupt),
            Some(Corrupt) => Some(Unsupported),
            Some(Unsupported) => Some(Io),
            Some(Io) => Some(Transient),
            Some(Transient) => Some(Busy),
            Some(Busy) => Some(Panic),
            Some(Panic) => Some(Closed),
            Some(Closed) => Some(Limit),
            Some(Limit) => Some(ReadOnly),
            Some(ReadOnly) => Some(Poisoned),
            Some(Poisoned) => Some(Cancelled),
            Some(Cancelled) => None,
        }
    }

    fn walk() -> Vec<ErrorCode> {
        let mut codes = Vec::new();
        let mut code = next(None);
        while let Some(found) = code {
            codes.push(found);
            code = next(code);
        }
        codes
    }

    /// The stable table (`docs/sdk/c-abi.md`): append-only. Editing a row here is an ABI break.
    const SNAPSHOT: &[(i32, &str)] = &[
        (1, "PERSIA_NOT_FOUND"),
        (2, "PERSIA_ALREADY_EXISTS"),
        (3, "PERSIA_CONFLICT"),
        (4, "PERSIA_INVALID_ARGUMENT"),
        (5, "PERSIA_CORRUPT"),
        (6, "PERSIA_UNSUPPORTED"),
        (7, "PERSIA_IO"),
        (8, "PERSIA_TRANSIENT"),
        (9, "PERSIA_BUSY"),
        (10, "PERSIA_PANIC"),
        (11, "PERSIA_CLOSED"),
        (12, "PERSIA_LIMIT"),
        (13, "PERSIA_READ_ONLY"),
        (14, "PERSIA_POISONED"),
        (15, "PERSIA_CANCELLED"),
    ];

    #[test]
    fn the_codes_are_the_snapshot() {
        let table: Vec<(i32, &str)> = walk()
            .into_iter()
            .map(|code| (code.number(), code.as_str()))
            .collect();
        assert_eq!(table, SNAPSHOT);
    }

    #[test]
    fn all_lists_every_code_once_in_order() {
        assert_eq!(ErrorCode::ALL.to_vec(), walk());
    }

    #[test]
    fn numbers_and_names_read_back() {
        for code in walk() {
            assert_eq!(ErrorCode::from_number(code.number()), Some(code));
            assert_eq!(code.to_string(), code.as_str());
            assert_ne!(code.number(), 0, "0 is success");
        }
        for number in [i32::MIN, -1, 0, 16, i32::MAX] {
            assert_eq!(ErrorCode::from_number(number), None);
        }
    }

    #[test]
    fn the_c_abi_table_lists_the_same_codes() {
        // The document is the contract of the language bindings: it must not drift from the enum.
        let doc = include_str!("../../../docs/sdk/c-abi.md");
        for (number, name) in SNAPSHOT {
            let row = format!("| {number} | `{name}` |");
            assert!(doc.contains(&row), "docs/sdk/c-abi.md lacks the row {row}");
        }
        assert!(doc.contains("| 0 | `PERSIA_OK` |"));
        let rows = doc
            .lines()
            .filter(|line| line.starts_with("| ") && line.contains("| `PERSIA_"))
            .count();
        assert_eq!(rows, SNAPSHOT.len() + 1, "a code the enum does not have");
    }

    #[test]
    fn an_error_shows_its_message_and_keeps_its_source() {
        use std::error::Error as _;
        let plain = Error::new(ErrorCode::Closed, "the database is closed");
        assert_eq!(plain.code(), ErrorCode::Closed);
        assert_eq!(plain.to_string(), "the database is closed");
        assert_eq!(plain.message(), "the database is closed");
        assert!(plain.source().is_none());

        let io = std::io::Error::other("disk on fire");
        let wrapped = Error::with_source(ErrorCode::Io, "creating /x", io);
        assert_eq!(wrapped.code(), ErrorCode::Io);
        assert_eq!(wrapped.message(), "creating /x: disk on fire");
        let source = wrapped.source().unwrap();
        assert_eq!(source.to_string(), "disk on fire");
        assert!(source.downcast_ref::<std::io::Error>().is_some());
        let debug = format!("{wrapped:?}");
        assert!(
            debug.contains("Io") && debug.contains("disk on fire"),
            "{debug}"
        );

        assert_eq!(Error::closed().code(), ErrorCode::Closed);
        let read_only = Error::read_only("put");
        assert_eq!(read_only.code(), ErrorCode::ReadOnly);
        assert!(read_only.message().contains("put"));
    }

    #[test]
    fn errors_cross_threads() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Error>();
        assert_send_sync::<ErrorCode>();
    }
}
