//! Storage errors (SPEC §10.1).
//!
//! Backends translate their native errors into [`Error`] so callers decide on its [`ErrorKind`] alone, and
//! never inspect backend-specific errors. Above all, a backend decides which failures are [`ErrorKind::Transient`]:
//! that is the only kind [`Error::is_retryable`] accepts.

use core::fmt;
use core::ops::Range;
use std::io;

use crate::key::{InvalidKey, Key};

/// Result alias for this crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// A backend's own error, kept as the source of an [`Error::Transient`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A [`Storage`](crate::Storage) operation, named in errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    /// [`Storage::get`](crate::Storage::get).
    Get,
    /// [`Storage::get_range`](crate::Storage::get_range).
    GetRange,
    /// [`Storage::put`](crate::Storage::put).
    Put,
    /// [`Storage::put_if_absent`](crate::Storage::put_if_absent).
    PutIfAbsent,
    /// [`Storage::list`](crate::Storage::list).
    List,
    /// [`Storage::delete`](crate::Storage::delete).
    Delete,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Get => "get",
            Self::GetRange => "get_range",
            Self::Put => "put",
            Self::PutIfAbsent => "put_if_absent",
            Self::List => "list",
            Self::Delete => "delete",
        })
    }
}

/// Errors from a [`Storage`](crate::Storage) backend.
///
/// `target` fields hold the key, or the prefix for [`Op::List`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No object under `key`.
    #[error("object {key} not found")]
    NotFound {
        /// The missing key.
        key: Key,
    },
    /// A conditional update of `key` lost a race with another writer. (`put_if_absent` reports
    /// [`PutOutcome::AlreadyExists`](crate::PutOutcome::AlreadyExists) instead.)
    #[error("conflicting concurrent update of {key}")]
    Conflict {
        /// The contended key.
        key: Key,
    },
    /// A failure that may go away if the operation is repeated: throttling, a timeout, a 5xx response.
    #[error("transient failure in {op} of {target:?}: {source}")]
    Transient {
        /// The failed operation.
        op: Op,
        /// The key or prefix.
        target: String,
        /// The backend's error.
        source: BoxError,
    },
    /// The bytes stored under `key` violate the format.
    #[error("corrupt object {key}: {source}")]
    Corrupt {
        /// The corrupt object.
        key: Key,
        /// Where and how decoding failed (absolute offsets).
        source: persia_format::Error,
    },
    /// A ranged read did not return exactly the requested bytes, e.g. the range extends past the object's end.
    /// Corruption: ranges come from committed metadata.
    #[error("short read of {key}: requested {}..{}, got {returned} bytes", requested.start, requested.end)]
    ShortRead {
        /// The object read.
        key: Key,
        /// The requested byte range.
        requested: Range<u64>,
        /// How many bytes came back.
        returned: u64,
    },
    /// A local I/O failure.
    #[error("I/O error in {op} of {target:?}: {source}")]
    Io {
        /// The failed operation.
        op: Op,
        /// The key or prefix.
        target: String,
        /// The OS error.
        source: io::Error,
    },
    /// A string is not a valid key or prefix.
    #[error(transparent)]
    InvalidKey(#[from] InvalidKey),
    /// The caller asked for an inverted range (`start > end`).
    #[error("invalid range {}..{} of {key}", range.start, range.end)]
    InvalidRange {
        /// The object.
        key: Key,
        /// The requested range.
        range: Range<u64>,
    },
    /// The backend lacks the capability `op` needs (e.g. `put_if_absent` without
    /// [`Capabilities::conditional_put`](crate::Capabilities::conditional_put)).
    #[error("{op} is not supported by this backend")]
    Unsupported {
        /// The unsupported operation.
        op: Op,
    },
}

/// The class of an [`Error`], which is all a caller should branch on (SPEC §10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// [`Error::NotFound`].
    NotFound,
    /// [`Error::Conflict`].
    Conflict,
    /// [`Error::Transient`]: the only retryable kind.
    Transient,
    /// [`Error::Corrupt`] or [`Error::ShortRead`].
    Corrupt,
    /// [`Error::Io`].
    Io,
    /// [`Error::InvalidKey`] or [`Error::InvalidRange`]: a caller error.
    InvalidInput,
    /// [`Error::Unsupported`].
    Unsupported,
}

impl Error {
    /// The error's class.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::Conflict { .. } => ErrorKind::Conflict,
            Self::Transient { .. } => ErrorKind::Transient,
            Self::Corrupt { .. } | Self::ShortRead { .. } => ErrorKind::Corrupt,
            Self::Io { .. } => ErrorKind::Io,
            Self::InvalidKey(_) | Self::InvalidRange { .. } => ErrorKind::InvalidInput,
            Self::Unsupported { .. } => ErrorKind::Unsupported,
        }
    }

    /// Whether the stored data is corrupt.
    pub fn is_corruption(&self) -> bool {
        self.kind() == ErrorKind::Corrupt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::new("segments/c/1.seg").unwrap()
    }

    #[allow(clippy::reversed_empty_ranges)] // an inverted range is one of the errors
    fn all() -> Vec<(Error, ErrorKind)> {
        let corrupt = persia_format::Error::Corrupt {
            offset: 7,
            reason: persia_format::Corruption::OverlongVarint,
        };
        vec![
            (Error::NotFound { key: key() }, ErrorKind::NotFound),
            (Error::Conflict { key: key() }, ErrorKind::Conflict),
            (
                Error::Transient {
                    op: Op::Get,
                    target: "k".into(),
                    source: "503 slow down".into(),
                },
                ErrorKind::Transient,
            ),
            (
                Error::Corrupt {
                    key: key(),
                    source: corrupt,
                },
                ErrorKind::Corrupt,
            ),
            (
                Error::ShortRead {
                    key: key(),
                    requested: 10..20,
                    returned: 4,
                },
                ErrorKind::Corrupt,
            ),
            (
                Error::Io {
                    op: Op::Put,
                    target: "k".into(),
                    source: io::Error::other("disk full"),
                },
                ErrorKind::Io,
            ),
            (
                Key::new("../x").unwrap_err().into(),
                ErrorKind::InvalidInput,
            ),
            (
                Error::InvalidRange {
                    key: key(),
                    range: 5..4,
                },
                ErrorKind::InvalidInput,
            ),
            (
                Error::Unsupported {
                    op: Op::PutIfAbsent,
                },
                ErrorKind::Unsupported,
            ),
        ]
    }

    #[test]
    fn every_variant_has_its_kind() {
        for (err, kind) in all() {
            assert_eq!(err.kind(), kind, "{err}");
            assert_eq!(err.is_corruption(), kind == ErrorKind::Corrupt, "{err}");
        }
    }

    #[test]
    fn messages_name_the_object() {
        let texts: Vec<String> = all().iter().map(|(e, _)| e.to_string()).collect();
        assert_eq!(
            texts,
            [
                "object segments/c/1.seg not found",
                "conflicting concurrent update of segments/c/1.seg",
                r#"transient failure in get of "k": 503 slow down"#,
                "corrupt object segments/c/1.seg: corrupt data at offset 7: overlong varint (non-canonical encoding)",
                "short read of segments/c/1.seg: requested 10..20, got 4 bytes",
                r#"I/O error in put of "k": disk full"#,
                r#"invalid key "../x": '.' or '..' segment"#,
                "invalid range 5..4 of segments/c/1.seg",
                "put_if_absent is not supported by this backend",
            ]
        );
    }
}
