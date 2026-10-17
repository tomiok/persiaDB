//! Object keys and list prefixes (SPEC §10.1).
//!
//! Keys are validated when they are built, so a backend never sees a key that names a parent directory, an
//! absolute path or a Windows drive, or that an object store would reject (too long). Filesystem-specific limits
//! (segment length, Windows reserved names) are the local backend's job (SPEC §10.1).

use core::fmt;
use core::str::FromStr;

/// The name of an object: `/`-separated, non-empty segments that are not `.` or `..` (SPEC §10.1).
///
/// ```
/// use persia_storage::Key;
///
/// let key: Key = "manifests/00000000000000000001.mf".parse()?;
/// assert_eq!(key.as_str(), "manifests/00000000000000000001.mf");
/// assert!(Key::new("../etc/passwd").is_err());
/// assert!(Key::new("/abs").is_err());
/// assert!(Key::new("a//b").is_err());
/// # Ok::<(), persia_storage::InvalidKey>(())
/// ```
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(String);

impl Key {
    /// Longest key in bytes: the S3 and GCS object-name limit.
    pub const MAX_LEN: usize = 1024;

    /// Validates `key`.
    ///
    /// # Errors
    /// [`InvalidKey`] naming the first rule `key` breaks.
    pub fn new(key: impl Into<String>) -> Result<Self, InvalidKey> {
        let key = key.into();
        match check(&key, false) {
            Ok(()) => Ok(Self(key)),
            Err(defect) => Err(InvalidKey { key, defect }),
        }
    }

    /// The key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key's segments, in order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

/// A prefix that [`Storage::list`](crate::Storage::list) matches keys against by plain byte prefix, as S3 does.
///
/// The same rules as [`Key`], except that it may be empty (every key) and may end with `/`; its last segment is
/// incomplete, so it may be `.` or `..` (a prefix of `..x`).
///
/// A backend never resolves a prefix as a path (`a/..` is a valid prefix): it lists the directory part, up to the
/// last `/`, and filters names by the rest (SPEC §10.1).
///
/// ```
/// use persia_storage::{Key, KeyPrefix};
///
/// let prefix = KeyPrefix::new("manifests/")?;
/// assert!(prefix.matches(&Key::new("manifests/00000000000000000001.mf")?));
/// assert!(!prefix.matches(&Key::new("manifestsX")?));
/// assert!(KeyPrefix::new("../").is_err());
/// assert!(KeyPrefix::default().matches(&Key::new("anything")?));
/// # Ok::<(), persia_storage::InvalidKey>(())
/// ```
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyPrefix(String);

impl KeyPrefix {
    /// Validates `prefix`.
    ///
    /// # Errors
    /// [`InvalidKey`] naming the first rule `prefix` breaks.
    pub fn new(prefix: impl Into<String>) -> Result<Self, InvalidKey> {
        let prefix = prefix.into();
        match check(&prefix, true) {
            Ok(()) => Ok(Self(prefix)),
            Err(defect) => Err(InvalidKey {
                key: prefix,
                defect,
            }),
        }
    }

    /// The prefix text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `key` starts with this prefix.
    pub fn matches(&self, key: &Key) -> bool {
        key.as_str().starts_with(self.as_str())
    }
}

/// Every key is a valid prefix (of itself and of keys that extend it).
impl From<Key> for KeyPrefix {
    fn from(key: Key) -> Self {
        Self(key.0)
    }
}

fn check(s: &str, prefix: bool) -> Result<(), KeyDefect> {
    if s.len() > Key::MAX_LEN {
        return Err(KeyDefect::TooLong { len: s.len() });
    }
    if s.is_empty() {
        return if prefix {
            Ok(())
        } else {
            Err(KeyDefect::Empty)
        };
    }
    if let Some(ch) = s
        .chars()
        .find(|&c| c.is_ascii_control() || c == '\\' || c == ':')
    {
        return Err(KeyDefect::ForbiddenChar { ch });
    }
    let mut segments = s.split('/').peekable();
    while let Some(segment) = segments.next() {
        // A prefix's last segment is incomplete: empty (a trailing `/`), `.` and `..` can all still grow.
        if prefix && segments.peek().is_none() {
            break;
        }
        match segment {
            "" => return Err(KeyDefect::EmptySegment),
            "." | ".." => return Err(KeyDefect::DotSegment),
            _ => {}
        }
    }
    Ok(())
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({:?})", self.0)
    }
}

impl fmt::Display for KeyPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for KeyPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyPrefix({:?})", self.0)
    }
}

impl FromStr for Key {
    type Err = InvalidKey;

    fn from_str(s: &str) -> Result<Self, InvalidKey> {
        Self::new(s)
    }
}

impl FromStr for KeyPrefix {
    type Err = InvalidKey;

    fn from_str(s: &str) -> Result<Self, InvalidKey> {
        Self::new(s)
    }
}

impl TryFrom<String> for Key {
    type Error = InvalidKey;

    fn try_from(s: String) -> Result<Self, InvalidKey> {
        Self::new(s)
    }
}

impl TryFrom<&str> for Key {
    type Error = InvalidKey;

    fn try_from(s: &str) -> Result<Self, InvalidKey> {
        Self::new(s)
    }
}

impl AsRef<str> for Key {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

// `Eq`, `Ord` and `Hash` are derived from the inner `String`, so they agree with `str`'s, as `Borrow` requires: maps
// keyed by `Key` can then be searched by `&str` (e.g. a range starting at a prefix).
impl core::borrow::Borrow<str> for Key {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// A string rejected as a [`Key`] or [`KeyPrefix`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
// `{key:?}`: the text may hold control characters, which must not reach a terminal or log raw.
#[error("invalid key {key:?}: {defect}")]
pub struct InvalidKey {
    /// The rejected text.
    pub key: String,
    /// The first rule it breaks.
    pub defect: KeyDefect,
}

/// Why a string is not a valid [`Key`] or [`KeyPrefix`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyDefect {
    /// A key must not be empty.
    #[error("empty")]
    Empty,
    /// Longer than [`Key::MAX_LEN`] bytes.
    #[error("{len} bytes, longer than {}", Key::MAX_LEN)]
    TooLong {
        /// Length in bytes.
        len: usize,
    },
    /// A leading, trailing or doubled `/`.
    #[error("empty segment (leading, trailing or doubled '/')")]
    EmptySegment,
    /// A `.` or `..` segment.
    #[error("'.' or '..' segment")]
    DotSegment,
    /// A control character, `\` or `:` (a Windows drive or alternate data stream).
    #[error("forbidden character {ch:?}")]
    ForbiddenChar {
        /// The first forbidden character.
        ch: char,
    },
    /// A segment's file name in the local backend is longer than 255 bytes (SPEC §10.1.1). Only the local backend
    /// reports it.
    #[error("a segment's file name is {len} bytes, longer than 255")]
    NameTooLong {
        /// Length of the encoded file name in bytes.
        len: usize,
    },
    /// A segment's file name in the local backend is a reserved Windows name (`con`, `nul`, …) or ends in `.`
    /// (SPEC §10.1.1). Only the local backend on Windows reports it.
    #[error("a segment's file name is reserved on Windows")]
    WindowsName,
    /// The file path of the object in the local backend, or of its temporary file, is longer than the platform's
    /// limit (`PATH_MAX`: 4096 bytes on Linux, 1024 on macOS and the BSDs, the terminating NUL included; SPEC §10.1.1). Only the
    /// local backend on Unix reports it; it depends on the root's path too.
    #[error("the object's file path is {len} bytes, longer than {max}")]
    PathTooLong {
        /// Length of the longest path the object needs, in bytes.
        len: usize,
        /// The platform's limit.
        max: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defect(s: &str) -> Option<KeyDefect> {
        Key::new(s).err().map(|e| e.defect)
    }

    fn prefix_defect(s: &str) -> Option<KeyDefect> {
        KeyPrefix::new(s).err().map(|e| e.defect)
    }

    #[test]
    fn valid_keys() {
        for s in [
            "a",
            "LEASE",
            "manifests/00000000000000000001.mf",
            "segments/c1/s.seg",
            ".hidden",
            "..x/y",
            "a/.../b",
            "ünïcode/ключ",
            "with space",
        ] {
            let key = Key::new(s).unwrap();
            assert_eq!(key.as_str(), s);
            assert_eq!(key.to_string(), s);
            assert_eq!(s.parse::<Key>().unwrap(), key);
        }
        assert_eq!(
            Key::new("a/b/c").unwrap().segments().collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn traversal_and_malformed_keys_are_rejected() {
        let cases = [
            ("", KeyDefect::Empty),
            ("/", KeyDefect::EmptySegment),
            ("/etc/passwd", KeyDefect::EmptySegment),
            ("a/", KeyDefect::EmptySegment),
            ("a//b", KeyDefect::EmptySegment),
            (".", KeyDefect::DotSegment),
            ("..", KeyDefect::DotSegment),
            ("../x", KeyDefect::DotSegment),
            ("a/../../x", KeyDefect::DotSegment),
            ("a/./b", KeyDefect::DotSegment),
            ("a/..", KeyDefect::DotSegment),
            ("..\\x", KeyDefect::ForbiddenChar { ch: '\\' }),
            ("C:\\x", KeyDefect::ForbiddenChar { ch: ':' }),
            ("C:/x", KeyDefect::ForbiddenChar { ch: ':' }),
            ("a:b", KeyDefect::ForbiddenChar { ch: ':' }),
            ("a\0b", KeyDefect::ForbiddenChar { ch: '\0' }),
            ("a\nb", KeyDefect::ForbiddenChar { ch: '\n' }),
            ("a\x7f", KeyDefect::ForbiddenChar { ch: '\x7f' }),
        ];
        for (s, expected) in cases {
            assert_eq!(defect(s), Some(expected), "{s:?}");
        }
    }

    #[test]
    fn length_limit_is_in_bytes() {
        assert!(Key::new("a".repeat(Key::MAX_LEN)).is_ok());
        assert_eq!(
            defect(&"a".repeat(Key::MAX_LEN + 1)),
            Some(KeyDefect::TooLong {
                len: Key::MAX_LEN + 1
            })
        );
        // 2-byte characters: 513 of them are 1026 bytes.
        assert_eq!(
            defect(&"é".repeat(513)),
            Some(KeyDefect::TooLong { len: 1026 })
        );
        assert_eq!(
            prefix_defect(&"a".repeat(Key::MAX_LEN + 1)),
            Some(KeyDefect::TooLong {
                len: Key::MAX_LEN + 1
            })
        );
    }

    #[test]
    fn prefixes() {
        for s in ["", "m", "manifests/", "a/b", "a/.", "a/..", ".", "..", "a/"] {
            assert_eq!(KeyPrefix::new(s).unwrap().as_str(), s);
        }
        for (s, expected) in [
            ("/", KeyDefect::EmptySegment),
            ("/a", KeyDefect::EmptySegment),
            ("a//", KeyDefect::EmptySegment),
            ("../", KeyDefect::DotSegment),
            ("a/../b", KeyDefect::DotSegment),
            ("./", KeyDefect::DotSegment),
            ("a\\", KeyDefect::ForbiddenChar { ch: '\\' }),
        ] {
            assert_eq!(prefix_defect(s), Some(expected), "{s:?}");
        }
        let key = Key::new("manifests/1.mf").unwrap();
        assert!(KeyPrefix::new("manifests/").unwrap().matches(&key));
        assert!(KeyPrefix::new("manifests/1.mf").unwrap().matches(&key));
        assert!(!KeyPrefix::new("manifests/1.mf/").unwrap().matches(&key));
        assert!(!KeyPrefix::new("segments/").unwrap().matches(&key));
        assert!(KeyPrefix::from(key.clone()).matches(&key));
    }

    #[test]
    fn error_text_escapes_the_key() {
        let err = Key::new("a\nb").unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"invalid key "a\nb": forbidden character '\n'"#
        );
        assert_eq!(format!("{:?}", Key::new("k").unwrap()), r#"Key("k")"#);
    }
}
