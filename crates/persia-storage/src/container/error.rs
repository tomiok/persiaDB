//! Errors of the single-file container (SPEC §4).

use core::fmt;
use std::io;
use std::path::PathBuf;

/// Result alias for the container.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Why a container cannot be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// It was opened with [`Mode::ReadOnly`](super::Mode::ReadOnly).
    OpenedReadOnly,
    /// Its header sets `rocompat_flags` bits this build does not know (SPEC §4.2): it may be read, not written.
    UnknownRocompatFlags {
        /// The unknown bits only.
        flags: u64,
    },
}

impl fmt::Display for ReadOnlyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenedReadOnly => f.write_str("opened read-only"),
            Self::UnknownRocompatFlags { flags } => {
                write!(f, "unknown read-only-compatible feature flags {flags:#x}")
            }
        }
    }
}

/// Errors from a [`Container`](super::Container).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file system failed.
    #[error("I/O error in container {op}: {source}")]
    Io {
        /// What the container was doing.
        op: &'static str,
        /// The OS error.
        source: io::Error,
    },
    /// A `persia_format` decode error: mostly the container's bytes violate the format, or use a format or feature this
    /// build does not support ([`persia_format::Error::Corrupt`] or [`persia_format::Error::Unsupported`]), but also
    /// an allocation that failed while decompressing a payload ([`persia_format::Error::OutOfMemory`], from
    /// [`Container::read_frame`](super::Container::read_frame)); [`Error::is_corruption`] tells them apart.
    #[error(transparent)]
    Format(#[from] persia_format::Error),
    /// The frames after the committed `log_end` end in corruption (SPEC §4.4), and a read-write open may not
    /// discard what follows (ADR-0014): a valid frame there may hold an acknowledged write (`acknowledged`), or (with
    /// `acknowledged` `None`) the bytes cannot be quarantined because the container was not opened by path, or the
    /// search for such frames exceeded its read budget. A
    /// read-only open can still read the committed state; [`Repair::Quarantine`](super::Repair::Quarantine) moves the
    /// bytes into a quarantine file and opens with what precedes them.
    #[error(
        "frames after the last commit are corrupt at offset {offset} ({source}){}; open read-only to read the \
         committed state, or repair it with Repair::Quarantine",
        acknowledged.map(|at| format!(", and the valid frame at {at} may hold acknowledged writes")).unwrap_or_default()
    )]
    CorruptTail {
        /// Where the valid frames end.
        offset: u64,
        /// Why the frame there was rejected.
        source: persia_format::Error,
        /// The first later frame that may hold an acknowledged write, if one was found.
        acknowledged: Option<u64>,
    },
    /// Another handle, in this process or another, holds the writer lock (SPEC §8.4).
    #[error("container is locked by another writer")]
    Locked,
    /// [`Container::create`](super::Container::create) found a file at the path; it never replaces one (SPEC §4.7).
    #[error("{} already exists", path.display())]
    AlreadyExists {
        /// The container path.
        path: PathBuf,
    },
    /// A write was attempted on a container that cannot be written.
    #[error("container is read-only: {reason}")]
    ReadOnly {
        /// Why.
        reason: ReadOnlyReason,
    },
    /// An earlier write, truncation or sync failed, so what reached the disk is unknown (after a failed `fsync` the
    /// OS may have dropped the data). Every later write fails; reopening runs recovery, which decides what survived.
    #[error("an earlier write or sync of the container failed; reopen it to recover")]
    Poisoned,
    /// The caller passed an argument the container cannot accept.
    #[error("invalid argument: {0}")]
    InvalidInput(String),
    /// [`Container::compact`](super::Container::compact) renamed the new file over the container, but syncing the
    /// directory failed, so the rename may not be durable. The handle now works on the new file and is poisoned:
    /// reopen it. `relocation` says where the frames moved; offsets from before the compaction must not be used on
    /// this handle.
    #[error("compacted the container, but syncing its directory failed ({source}); reopen it")]
    CompactedNotDurable {
        /// Where compaction moved each frame.
        relocation: super::Relocation,
        /// The OS error.
        source: io::Error,
    },
    /// Reading a whole frame ([`Container::read_frame`](super::Container::read_frame), compaction) needed a buffer of
    /// `bytes` bytes that could not be allocated. Recovery reads in bounded chunks and never needs one.
    #[error("cannot allocate {bytes} bytes to read the container")]
    OutOfMemory {
        /// The requested size.
        bytes: u64,
    },
    /// A frame could not be encoded (e.g. a bad compression level).
    #[error(transparent)]
    Encode(#[from] persia_format::EncodeError),
}

impl Error {
    /// Whether the stored bytes are corrupt.
    pub fn is_corruption(&self) -> bool {
        match self {
            Self::Format(err) => err.is_corruption(),
            Self::CorruptTail { .. } => true,
            _ => false,
        }
    }

    pub(super) fn io(op: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io { op, source }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corruption_classes() {
        let corrupt = persia_format::Error::Corrupt {
            offset: 9,
            reason: persia_format::Corruption::BadFrameMagic,
        };
        assert!(Error::Format(corrupt.clone()).is_corruption());
        assert!(
            Error::CorruptTail {
                offset: 9,
                source: corrupt,
                acknowledged: Some(64),
            }
            .is_corruption()
        );
        for err in [
            Error::Locked,
            Error::Poisoned,
            Error::InvalidInput("x".into()),
            Error::ReadOnly {
                reason: ReadOnlyReason::OpenedReadOnly,
            },
            Error::io("sync")(io::Error::other("disk gone")),
        ] {
            assert!(!err.is_corruption(), "{err}");
        }
    }

    #[test]
    fn messages() {
        let ro = Error::ReadOnly {
            reason: ReadOnlyReason::UnknownRocompatFlags { flags: 0x10 },
        };
        assert_eq!(
            ro.to_string(),
            "container is read-only: unknown read-only-compatible feature flags 0x10"
        );
        assert_eq!(
            Error::io("sync")(io::Error::other("disk gone")).to_string(),
            "I/O error in container sync: disk gone"
        );
    }

    #[test]
    fn error_is_send_sync_static() {
        fn assert<E: std::error::Error + Send + Sync + 'static>() {}
        assert::<Error>();
    }
}
