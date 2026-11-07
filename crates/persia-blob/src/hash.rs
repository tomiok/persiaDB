//! The content hash that names a blob: SHA-256 of its bytes (SPEC §9.1, ADR-0002).
//!
//! Persia always computes the id itself from the bytes it receives; a caller's expected id is only compared with
//! it (SPEC §9.6, ADR-0034).

use core::fmt;

use persia_format::BlobId;
use sha2::{Digest, Sha256};

/// Incremental SHA-256 over a blob's bytes, producing its [`BlobId`].
///
/// Feeding the same bytes in any split gives the same id as [`blob_id_of`] over their concatenation.
///
/// ```
/// use persia_blob::{ContentHasher, blob_id_of};
///
/// let mut hasher = ContentHasher::new();
/// hasher.update(b"ab");
/// hasher.update(b"c");
/// assert_eq!(hasher.len(), 3);
/// let id = hasher.finish();
/// assert_eq!(id, blob_id_of(b"abc"));
/// assert_eq!(id.to_string(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
/// ```
#[derive(Clone, Default)]
pub struct ContentHasher {
    sha: Sha256,
    len: u64,
}

impl ContentHasher {
    /// A hasher that has seen no bytes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the next bytes of the blob.
    pub fn update(&mut self, bytes: &[u8]) {
        self.sha.update(bytes);
        // 2^64 bytes cannot be fed in practice; saturating keeps the count an upper bound rather than wrapping.
        self.len = self
            .len
            .saturating_add(persia_format::widen::u64_from_usize(bytes.len()));
    }

    /// Bytes fed so far.
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether no bytes have been fed.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The id of the bytes fed so far.
    pub fn finish(self) -> BlobId {
        BlobId::from_bytes(self.sha.finalize().into())
    }
}

impl fmt::Debug for ContentHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContentHasher")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// The id of a blob held in memory.
///
/// ```
/// // SHA-256 of the empty input.
/// assert_eq!(
///     persia_blob::blob_id_of(b"").to_string(),
///     "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
/// );
/// ```
pub fn blob_id_of(bytes: &[u8]) -> BlobId {
    BlobId::from_bytes(Sha256::digest(bytes).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn hex(id: BlobId) -> String {
        id.to_string()
    }

    /// FIPS 180-4 / NIST CSRC example vectors.
    #[test]
    fn known_vectors() {
        assert_eq!(
            hex(blob_id_of(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(blob_id_of(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(blob_id_of(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let mut hasher = ContentHasher::new();
        let block = [b'a'; 1000];
        for _ in 0..1000 {
            hasher.update(&block);
        }
        assert_eq!(hasher.len(), 1_000_000);
        assert_eq!(
            hex(hasher.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn empty_hasher_is_the_empty_id() {
        let hasher = ContentHasher::new();
        assert!(hasher.is_empty());
        assert_eq!(hasher.finish(), blob_id_of(b""));
    }

    #[test]
    fn debug_shows_only_the_length() {
        let mut hasher = ContentHasher::new();
        hasher.update(b"secret");
        assert_eq!(format!("{hasher:?}"), "ContentHasher { len: 6, .. }");
    }

    proptest! {
        /// Any split of the input, including empty pieces, gives the one-shot id and the total length.
        #[test]
        fn split_matches_one_shot(
            data in proptest::collection::vec(any::<u8>(), 0..4096),
            cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..8),
        ) {
            let mut points: Vec<usize> = cuts.iter().map(|c| c.index(data.len() + 1)).collect();
            points.sort_unstable();
            let mut hasher = ContentHasher::new();
            let mut start = 0;
            for p in points.into_iter().chain([data.len()]) {
                hasher.update(&data[start..p]);
                start = p;
            }
            prop_assert_eq!(hasher.len(), data.len() as u64);
            prop_assert_eq!(hasher.clone().finish(), blob_id_of(&data));
            prop_assert_eq!(hasher.finish(), blob_id_of(&data));
        }
    }
}
