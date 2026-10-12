//! The `Storage` trait every backend implements (SPEC §10.1).

use core::ops::Range;

use bytes::Bytes;
use persia_format::ByteSource;

use crate::error::{Error, Result};
use crate::key::{Key, KeyPrefix};
use crate::meta::{Capabilities, ObjectMeta, PutOutcome};

/// A flat namespace of immutable-by-convention objects: memory, a local directory, or an object store.
///
/// The engine is synchronous and reaches I/O only through this trait; asynchronous backends block inside it.
/// Semantics shared by every backend (SPEC §10.1):
/// - a missing key is [`Error::NotFound`] for reads and succeeds for [`delete`](Self::delete);
/// - [`put`](Self::put) is atomic: readers see the old or the new bytes, never a mix;
/// - only [`ErrorKind::Transient`](crate::ErrorKind::Transient) errors are worth repeating.
///
/// ```
/// use persia_storage::{Key, KeyPrefix, Result, Storage};
///
/// /// The highest-numbered manifest, as a reader finds it (SPEC §10.4).
/// fn latest_manifest(storage: &dyn Storage) -> Result<Option<Key>> {
///     let prefix = KeyPrefix::new("manifests/")?;
///     Ok(storage.list(&prefix)?.pop().map(|meta| meta.key)) // sorted by key
/// }
/// ```
pub trait Storage: Send + Sync {
    /// Bytes `range` of the object, as a [`ByteSource`] whose base offset is `range.start`, so decoders report
    /// absolute offsets. Implementations build the result with [`range_source`].
    ///
    /// # Errors
    /// [`Error::InvalidRange`] if `range` is inverted, [`Error::NotFound`], and [`Error::ShortRead`] if the range
    /// extends past the object's end.
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource>;

    /// The whole object.
    ///
    /// # Errors
    /// [`Error::NotFound`], or a backend failure.
    fn get(&self, key: &Key) -> Result<Bytes>;

    /// Creates or replaces the object, atomically.
    ///
    /// # Errors
    /// A backend failure; the old object (or none) is then still visible.
    fn put(&self, key: &Key, data: Bytes) -> Result<()>;

    /// Creates the object only if no object has the key, atomically: the commit primitive (SPEC §10.4).
    ///
    /// A backend that retries resolves the ambiguity of a retried attempt before answering, with
    /// [`RetryPolicy::run_put_if_absent`](crate::retry::RetryPolicy::run_put_if_absent): `AlreadyExists` always
    /// means another writer won.
    ///
    /// # Errors
    /// [`Error::Unsupported`] without [`Capabilities::conditional_put`], or a backend failure.
    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome>;

    /// Every object whose key starts with `prefix`, sorted by key bytes. Without
    /// [`Capabilities::strong_list`] a just-written object may be missing.
    ///
    /// # Errors
    /// A backend failure.
    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>>;

    /// Removes the object; succeeds if it does not exist.
    ///
    /// # Errors
    /// A backend failure.
    fn delete(&self, key: &Key) -> Result<()>;

    /// What this backend guarantees beyond the base contract.
    fn capabilities(&self) -> Capabilities;
}

/// Rejects an inverted `range` of `key`. Backends call it before any I/O.
///
/// # Errors
/// [`Error::InvalidRange`] if `range.start > range.end`.
pub fn check_range(key: &Key, range: &Range<u64>) -> Result<()> {
    if range.start > range.end {
        return Err(Error::InvalidRange {
            key: key.clone(),
            range: range.clone(),
        });
    }
    Ok(())
}

/// The result of reading `range` of `key`, checked to be exactly the requested length.
///
/// A backend passes whatever its read returned, after classifying a transfer it knows was cut off as
/// [`Error::Transient`]; a short read (the object ends inside the range)
/// response) or a long one (a server that ignored the range) becomes an error instead of misaligned bytes.
///
/// ```
/// use bytes::Bytes;
/// use persia_storage::{Key, range_source};
///
/// let key = Key::new("segments/c/1.seg")?;
/// let src = range_source(&key, 4096..4100, Bytes::from_static(b"abcd"))?;
/// assert_eq!((src.base_offset(), &*src), (4096, &b"abcd"[..]));
/// let err = range_source(&key, 4096..4100, Bytes::from_static(b"ab")).unwrap_err();
/// assert!(err.is_corruption());
/// # Ok::<(), persia_storage::Error>(())
/// ```
///
/// # Errors
/// [`Error::InvalidRange`] if `range` is inverted; [`Error::ShortRead`] if `bytes` is not exactly
/// `range.end - range.start` long.
pub fn range_source(key: &Key, range: Range<u64>, bytes: Bytes) -> Result<ByteSource> {
    check_range(key, &range)?;
    ByteSource::for_range(bytes, range.clone()).map_err(|err| match err {
        persia_format::Error::InvalidInputLen { len, .. } => Error::ShortRead {
            key: key.clone(),
            requested: range,
            returned: len,
        },
        persia_format::Error::RangeOutOfBounds { .. } => Error::InvalidRange {
            key: key.clone(),
            range,
        },
        // `for_range` fails in no other way; should it ever, the bytes are not what the range promised.
        source => Error::Corrupt {
            key: key.clone(),
            source,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    fn key() -> Key {
        Key::new("k").unwrap()
    }

    // The engine holds backends as trait objects.
    #[test]
    fn storage_is_object_safe() {
        fn takes(_: Option<&dyn Storage>) {}
        takes(None);
    }

    #[test]
    fn range_source_checks_the_length() {
        let src = range_source(&key(), 10..14, Bytes::from_static(b"wxyz")).unwrap();
        assert_eq!((src.base_offset(), src.end_offset()), (10, 14));
        assert!(range_source(&key(), 7..7, Bytes::new()).unwrap().is_empty());
        for (bytes, returned) in [(&b"wxy"[..], 3), (b"wxyz!", 5), (b"", 0)] {
            let err = range_source(&key(), 10..14, Bytes::from_static(bytes)).unwrap_err();
            assert!(
                matches!(&err, Error::ShortRead { requested, returned: r, .. } if *requested == (10..14) && *r == returned),
                "{err}"
            );
            assert_eq!(err.kind(), ErrorKind::Corrupt);
        }
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn inverted_ranges_are_caller_errors() {
        assert_eq!(
            check_range(&key(), &(5..4)).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        let err = range_source(&key(), 5..4, Bytes::new()).unwrap_err();
        assert!(matches!(err, Error::InvalidRange { range, .. } if range == (5..4)));
        assert!(check_range(&key(), &(u64::MAX..u64::MAX)).is_ok());
    }
}
