//! The keys of the remote layout (SPEC §9.4), and their parsers.
//!
//! ```text
//! blobs/<hex[0..2]>/<hex(blob_id)>                 blob manifest
//! blobs/uploads/<upload_id:032x>/<chunk_idx:010>   staged chunk
//! ```
//!
//! `uploads` is not two hex digits, so the two kinds of key never meet. A listing of a shared bucket can return keys
//! Persia did not write: [`RemoteKey::parse`] accepts exactly the keys [`RemoteKey::key`] produces and nothing else,
//! so a sweep (ROADMAP 9.5.2) can never mistake a foreign object for a blob's.
//!
//! ```
//! use persia_blob::remote::RemoteKey;
//! use persia_blob::{UploadId, blob_id_of};
//!
//! let chunk = RemoteKey::Chunk { upload_id: UploadId::new(0xab), index: 7 };
//! let key = chunk.key()?;
//! assert_eq!(key.as_str(), "blobs/uploads/000000000000000000000000000000ab/0000000007");
//! assert_eq!(RemoteKey::parse(&key), Some(chunk));
//! let manifest = RemoteKey::Manifest(blob_id_of(b""));
//! assert!(manifest.key()?.as_str().starts_with("blobs/e3/e3b0c442"));
//! # Ok::<(), persia_blob::Error>(())
//! ```

use persia_format::BlobId;
use persia_storage::{Key, KeyPrefix};

use crate::error::{Error, Result};
use crate::manifest::UploadId;

/// The first segment of every key of the layout.
pub const BLOBS: &str = "blobs";
/// The second segment of a staged chunk's key.
pub const UPLOADS: &str = "uploads";
/// Digits of a chunk index in its key: `u32::MAX` has 10.
const INDEX_DIGITS: usize = 10;
/// Hex digits of an upload id in its key.
const UPLOAD_DIGITS: usize = 32;

/// A key of the remote layout, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RemoteKey {
    /// `blobs/<xx>/<id>`: the blob manifest that binds `blob_id`.
    Manifest(BlobId),
    /// `blobs/uploads/<upload_id>/<chunk_idx>`: chunk `index` staged under `upload_id`.
    Chunk {
        /// The upload.
        upload_id: UploadId,
        /// The chunk's index.
        index: u32,
    },
}

impl RemoteKey {
    /// The key's text.
    ///
    /// # Errors
    /// None in practice (every such key is valid, SPEC §10.1); a rejected key is reported as a storage error.
    pub fn key(&self) -> Result<Key> {
        let text = match self {
            Self::Manifest(blob_id) => {
                let hex = blob_id.to_string();
                format!("{BLOBS}/{}/{hex}", hex.get(..2).unwrap_or_default())
            }
            Self::Chunk { upload_id, index } => {
                format!("{BLOBS}/{UPLOADS}/{upload_id}/{index:010}")
            }
        };
        Key::new(text).map_err(|e| Error::Storage(e.into()))
    }

    /// Parses a key of the layout: exactly the form [`RemoteKey::key`] writes (lowercase hex, the manifest's prefix
    /// equal to its id's first byte, a 10-digit index of at most `u32::MAX`). `None` for any other key.
    pub fn parse(key: &Key) -> Option<Self> {
        let mut segments = key.segments();
        if segments.next()? != BLOBS {
            return None;
        }
        let second = segments.next()?;
        let third = segments.next()?;
        let fourth = segments.next();
        if segments.next().is_some() {
            return None;
        }
        match fourth {
            None => {
                let blob_id: BlobId = third.parse().ok()?;
                (third.get(..2)? == second && second.len() == 2).then_some(Self::Manifest(blob_id))
            }
            Some(index) if second == UPLOADS => Some(Self::Chunk {
                upload_id: parse_upload_id(third)?,
                index: parse_index(index)?,
            }),
            Some(_) => None,
        }
    }
}

/// `blobs/<hex(byte)>/`: the manifests whose id starts with `byte`, as a sweep lists them (SPEC §9.5: per prefix).
///
/// # Errors
/// As [`RemoteKey::key`].
pub fn manifest_prefix(first_byte: u8) -> Result<KeyPrefix> {
    prefix(format!("{BLOBS}/{first_byte:02x}/"))
}

/// `blobs/uploads/`: every staged chunk.
///
/// # Errors
/// As [`RemoteKey::key`].
pub fn uploads_prefix() -> Result<KeyPrefix> {
    prefix(format!("{BLOBS}/{UPLOADS}/"))
}

/// `blobs/uploads/<upload_id>/`: the chunks staged under `upload_id`.
///
/// # Errors
/// As [`RemoteKey::key`].
pub fn upload_prefix(upload_id: UploadId) -> Result<KeyPrefix> {
    prefix(format!("{BLOBS}/{UPLOADS}/{upload_id}/"))
}

fn prefix(text: String) -> Result<KeyPrefix> {
    KeyPrefix::new(text).map_err(|e| Error::Storage(e.into()))
}

/// 32 lowercase hex digits.
fn parse_upload_id(s: &str) -> Option<UploadId> {
    let canonical = s.len() == UPLOAD_DIGITS
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    canonical
        .then(|| u128::from_str_radix(s, 16).ok())
        .flatten()
        .map(UploadId::new)
}

/// 10 decimal digits, at most `u32::MAX`.
fn parse_index(s: &str) -> Option<u32> {
    (s.len() == INDEX_DIGITS && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::blob_id_of;
    use proptest::prelude::*;

    fn key(s: &str) -> Key {
        Key::new(s).unwrap()
    }

    #[test]
    fn keys_have_the_spec_form() {
        let id = blob_id_of(b"abc");
        assert_eq!(
            RemoteKey::Manifest(id).key().unwrap().as_str(),
            format!("blobs/ba/{id}")
        );
        let chunk = RemoteKey::Chunk {
            upload_id: UploadId::new(u128::MAX),
            index: u32::MAX,
        };
        assert_eq!(
            chunk.key().unwrap().as_str(),
            "blobs/uploads/ffffffffffffffffffffffffffffffff/4294967295"
        );
        assert_eq!(manifest_prefix(0x0a).unwrap().as_str(), "blobs/0a/");
        assert_eq!(uploads_prefix().unwrap().as_str(), "blobs/uploads/");
        assert_eq!(
            upload_prefix(UploadId::new(1)).unwrap().as_str(),
            "blobs/uploads/00000000000000000000000000000001/"
        );
        assert!(
            manifest_prefix(0xba)
                .unwrap()
                .matches(&RemoteKey::Manifest(id).key().unwrap())
        );
        assert!(uploads_prefix().unwrap().matches(&chunk.key().unwrap()));
    }

    #[allow(clippy::string_slice)]
    #[test]
    fn foreign_keys_do_not_parse() {
        let id = blob_id_of(b"abc").to_string();
        let up = "000000000000000000000000000000ab";
        for k in [
            "blobs".to_owned(),
            "blobs/ba".to_owned(),
            format!("other/ba/{id}"),
            format!("blobs/bb/{id}"),
            format!("blobs/b/{id}"),
            format!("blobs/BA/{}", id.to_uppercase()),
            format!("blobs/ba/{id}0"),
            format!("blobs/ba/{id}/manifest"),
            format!("blobs/ba/{id}/0000000000"),
            format!("blobs/uploads/{up}"),
            format!("blobs/uploads/{up}/000000000"),
            format!("blobs/uploads/{up}/00000000000"),
            format!("blobs/uploads/{up}/4294967296"),
            format!("blobs/uploads/{up}/+000000001"),
            format!("blobs/uploads/{up}/000000000a"),
            format!(
                "blobs/uploads/{}/0000000000",
                up.to_uppercase().replace('X', "x")
            ),
            format!("blobs/uploads/{}/0000000000", &up[1..]),
            format!("blobs/uploads/+{}/0000000000", &up[1..]),
            format!("blobs/uploads/{up}/0000000000/x"),
            format!("blobs/upload/{up}/0000000000"),
        ] {
            assert_eq!(RemoteKey::parse(&key(&k)), None, "{k}");
        }
    }

    proptest! {
        #[test]
        fn every_key_parses_back(bytes in any::<[u8; 32]>(), upload in any::<u128>(), index in any::<u32>()) {
            for k in [
                RemoteKey::Manifest(BlobId::from_bytes(bytes)),
                RemoteKey::Chunk { upload_id: UploadId::new(upload), index },
            ] {
                prop_assert_eq!(RemoteKey::parse(&k.key().unwrap()), Some(k));
            }
        }

        /// A real manifest key with any one letter's case changed (in `blobs`, the prefix or the id) is not a key Persia
        /// wrote, and the same for a chunk key.
        #[test]
        fn a_key_with_one_letter_recased_does_not_parse(
            bytes in any::<[u8; 32]>(),
            upload in any::<u128>(),
            index in any::<u32>(),
            at in any::<prop::sample::Index>(),
        ) {
            for k in [
                RemoteKey::Manifest(BlobId::from_bytes(bytes)),
                RemoteKey::Chunk { upload_id: UploadId::new(upload), index },
            ] {
                let text = k.key().unwrap().as_str().to_owned();
                let letters: Vec<usize> = text.char_indices().filter(|(_, c)| c.is_ascii_lowercase()).map(|(i, _)| i).collect();
                let i = letters[at.index(letters.len())];
                let mut recased = text.into_bytes();
                recased[i] = recased[i].to_ascii_uppercase();
                let recased = String::from_utf8(recased).unwrap();
                prop_assert_eq!(RemoteKey::parse(&key(&recased)), None, "{}", recased);
            }
        }

        /// A parsed key is exactly the key it was parsed from (one text per key). The texts are real keys, half of
        /// them with one character replaced by a near miss (another hex digit, its uppercase form, a sign, a letter
        /// past `f`), so the assertion runs on every unedited key and on every edit that still parses.
        #[test]
        fn a_parsed_key_is_canonical(
            bytes in any::<[u8; 32]>(),
            upload in any::<u128>(),
            index in any::<u32>(),
            manifest in any::<bool>(),
            edit in proptest::option::of((any::<prop::sample::Index>(), prop::sample::select(vec!['0', '9', 'a', 'f', 'A', 'F', '+', 'g']))),
        ) {
            let k = if manifest {
                RemoteKey::Manifest(BlobId::from_bytes(bytes))
            } else {
                RemoteKey::Chunk { upload_id: UploadId::new(upload), index }
            };
            let mut text = k.key().unwrap().as_str().to_owned();
            if let Some((at, c)) = edit {
                let mut chars: Vec<char> = text.chars().collect();
                let i = at.index(chars.len());
                chars[i] = c;
                text = chars.into_iter().collect();
            }
            let parsed = RemoteKey::parse(&key(&text));
            if edit.is_none() {
                prop_assert_eq!(parsed, Some(k));
            }
            if let Some(parsed) = parsed {
                let back = parsed.key().unwrap();
                prop_assert_eq!(back.as_str(), text.as_str());
            }
        }
    }
}
