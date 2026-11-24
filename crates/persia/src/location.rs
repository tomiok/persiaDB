//! Where a database is: a filesystem path, or a URL (SPEC §8.1, §10.2; ROADMAP 10.1.1).
//!
//! `Db::open` takes one argument. It is a URL only when it starts with `scheme://`; anything else is a path, so
//! `C:\data\app.persia` and `./app.persia` are paths. The only scheme this build opens is `file://`, with an empty
//! host or `localhost` and a percent-encoded absolute path. `s3://`, `gs://`, `az://` and `memory://` are the
//! schemes of backends that do not exist yet (ROADMAP M12): they are *unsupported*, naming the scheme. Any other
//! scheme is an invalid argument. The parser is written by hand (no `url` crate) and never panics.

use std::path::{Path, PathBuf};

use crate::{Error, ErrorCode, Result};

/// The schemes SPEC §10.2 names that this build has no backend for.
const FUTURE_SCHEMES: [&str; 4] = ["s3", "gs", "az", "memory"];

/// The filesystem path `location` names: itself, or the path of a `file://` URL.
///
/// # Errors
/// [`ErrorCode::Unsupported`] for a scheme of [`FUTURE_SCHEMES`]; [`ErrorCode::InvalidArgument`] for any other
/// scheme, a `file://` URL that is malformed, and an empty location.
pub(crate) fn path_of(location: &Path) -> Result<PathBuf> {
    if location.as_os_str().is_empty() {
        return Err(invalid("the path is empty"));
    }
    // A path that is not Unicode cannot be a URL: every URL this parser accepts is ASCII.
    let Some(text) = location.to_str() else {
        return Ok(location.to_path_buf());
    };
    let Some((scheme, rest)) = split_scheme(text) else {
        return Ok(location.to_path_buf());
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme == "file" {
        return file_path(rest);
    }
    if FUTURE_SCHEMES.contains(&scheme.as_str()) {
        return Err(Error::new(
            ErrorCode::Unsupported,
            format!(
                "{scheme}:// URLs are not supported by this build: it opens paths and file:// URLs"
            ),
        ));
    }
    Err(invalid(format!(
        "unknown URL scheme {scheme:?}: this build opens paths and file:// URLs"
    )))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidArgument, message)
}

/// `text` as `(scheme, rest)` when it starts with `scheme://` (RFC 3986: a letter, then letters, digits, `+`, `-`
/// or `.`); `rest` is what follows the two slashes.
fn split_scheme(text: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = text.split_once("://")?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    let valid = first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some((scheme, rest))
}

/// The path of a `file://` URL, given what follows `file://`: `[host]/percent-encoded-path`.
fn file_path(rest: &str) -> Result<PathBuf> {
    let Some(slash) = rest.find('/') else {
        return Err(invalid(
            "a file:// URL needs an absolute path, as in file:///data/app.persia",
        ));
    };
    let (host, encoded) = rest.split_at(slash);
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
        return Err(invalid(format!(
            "a file:// URL names a local file: its host must be empty or localhost, not {host:?}"
        )));
    }
    // A query or a fragment would be dropped silently, and another file opened than the text suggests.
    if encoded.contains(['?', '#']) {
        return Err(invalid(
            "a file:// URL takes no query or fragment: write ? as %3F and # as %23",
        ));
    }
    let bytes = percent_decode(encoded)?;
    if bytes.contains(&0) {
        return Err(invalid("a file:// URL's path holds a NUL byte"));
    }
    path_from_bytes(bytes)
}

/// `encoded` with every `%XX` replaced by the byte it names.
fn percent_decode(encoded: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes().iter().copied();
    while let Some(byte) = rest.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = rest.next().and_then(hex_digit);
        let low = rest.next().and_then(hex_digit);
        let (Some(high), Some(low)) = (high, low) else {
            return Err(invalid(
                "a file:// URL has a % that is not followed by two hex digits",
            ));
        };
        bytes.push(high << 4 | low);
    }
    Ok(bytes)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The decoded bytes of a `file://` URL's path as a path of this platform: any bytes on Unix.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // The other platforms' version fails.
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// The decoded bytes of a `file://` URL's path as a path of this platform: UTF-8 elsewhere, and on Windows
/// `/C:/dir/app.persia` is `C:/dir/app.persia`.
#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf> {
    let text = String::from_utf8(bytes)
        .map_err(|_| invalid("a file:// URL's path is not UTF-8 once decoded"))?;
    Ok(PathBuf::from(strip_drive_slash(&text)))
}

/// `/C:/dir` without its first slash, when it is followed by a drive letter and a colon.
#[cfg(not(unix))]
fn strip_drive_slash(text: &str) -> &str {
    let mut chars = text.chars();
    let drive = chars.next() == Some('/')
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.next() == Some(':');
    if cfg!(windows) && drive {
        text.get(1..).unwrap_or(text)
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use proptest::prelude::*;

    use super::*;

    fn path(location: &str) -> PathBuf {
        path_of(Path::new(location)).unwrap()
    }

    fn code(location: &str) -> ErrorCode {
        path_of(Path::new(location)).unwrap_err().code()
    }

    #[test]
    fn a_plain_string_is_a_path() {
        for plain in [
            "app.persia",
            "./data/app.persia",
            "/var/lib/app.persia",
            r"C:\data\app.persia",
            "C:/data/app.persia",
            "file.persia",
            "file:app.persia",  // no `//`: not a URL
            "file:/app.persia", // one slash: not a URL
            "s3:bucket",
            "://app.persia",    // no scheme
            "1s3://bucket/app", // a scheme starts with a letter
            "my app://x",       // a space is not a scheme character
            "a_b://x",
            "dir/with://inside",
            "100%.persia", // a path is not percent-decoded
        ] {
            assert_eq!(path(plain), PathBuf::from(plain), "{plain}");
        }
    }

    #[test]
    fn an_empty_location_is_invalid() {
        assert_eq!(code(""), ErrorCode::InvalidArgument);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_unicode_is_a_path() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"s3://\xff/app.persia");
        assert_eq!(path_of(Path::new(raw)).unwrap(), PathBuf::from(raw));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_url_is_its_decoded_path() {
        assert_eq!(
            path("file:///data/app.persia"),
            PathBuf::from("/data/app.persia")
        );
        assert_eq!(
            path("file://localhost/data/app.persia"),
            PathBuf::from("/data/app.persia")
        );
        assert_eq!(
            path("FILE://LocalHost/data/app.persia"),
            PathBuf::from("/data/app.persia")
        );
        assert_eq!(
            path("file:///my%20data/a%2Fb%3f%23%25.persia"),
            PathBuf::from("/my data/a/b?#%.persia")
        );
        assert_eq!(path("file:///"), PathBuf::from("/"));
        // `+` is not a space in a path, and a decoded byte need not be UTF-8.
        assert_eq!(path("file:///a+b"), PathBuf::from("/a+b"));
        assert_eq!(
            std::os::unix::ffi::OsStrExt::as_bytes(path("file:///%ff%FE").as_os_str()),
            b"/\xff\xfe"
        );
        // Not decoded twice.
        assert_eq!(path("file:///a%2520b"), PathBuf::from("/a%20b"));
    }

    #[test]
    fn a_malformed_file_url_is_invalid() {
        for bad in [
            "file://",
            "file://localhost",
            "file://app.persia",           // `app.persia` is a host
            "file://./app.persia",         // so is `.`
            "file://server/share/app",     // a remote host
            "file://localhost:80/app",     // no port
            "file://user@localhost/app",   // no user
            "file:///app.persia?mode=ro",  // no query
            "file:///app.persia#fragment", // no fragment
            "file:///a%",
            "file:///a%2",
            "file:///a%zz",
            "file:///a%2g",
            "file:///a%é0",
            "file:///a%00b", // NUL
        ] {
            let err = path_of(Path::new(bad)).unwrap_err();
            assert_eq!(err.code(), ErrorCode::InvalidArgument, "{bad}: {err}");
        }
    }

    #[test]
    fn a_scheme_without_a_backend_is_unsupported_and_named() {
        for (url, scheme) in [
            ("s3://bucket/prefix", "s3"),
            ("gs://bucket/prefix", "gs"),
            ("az://account/container/prefix", "az"),
            ("memory://", "memory"),
            ("S3://Bucket", "s3"),
            ("Memory://x", "memory"),
        ] {
            let err = path_of(Path::new(url)).unwrap_err();
            assert_eq!(err.code(), ErrorCode::Unsupported, "{url}");
            assert!(err.message().starts_with(&format!("{scheme}://")), "{err}");
        }
    }

    #[test]
    fn any_other_scheme_is_invalid_and_named() {
        for (url, scheme) in [
            ("http://example.com/app.persia", "http"),
            ("https://example.com/app.persia", "https"),
            ("sqlite://app.db", "sqlite"),
            ("s3a://bucket", "s3a"),
            ("files:///app.persia", "files"),
            ("x+y-z.1://a", "x+y-z.1"),
        ] {
            let err = path_of(Path::new(url)).unwrap_err();
            assert_eq!(err.code(), ErrorCode::InvalidArgument, "{url}");
            assert!(err.message().contains(scheme), "{err}");
        }
    }

    #[test]
    fn hex_digits() {
        assert_eq!(hex_digit(b'0'), Some(0));
        assert_eq!(hex_digit(b'9'), Some(9));
        assert_eq!(hex_digit(b'a'), Some(10));
        assert_eq!(hex_digit(b'f'), Some(15));
        assert_eq!(hex_digit(b'A'), Some(10));
        assert_eq!(hex_digit(b'F'), Some(15));
        for other in [b'g', b'G', b'/', b':', b'@', b'`', b' ', 0xff] {
            assert_eq!(hex_digit(other), None);
        }
        assert_eq!(percent_decode("%0f%F0%7e").unwrap(), [0x0f, 0xf0, 0x7e]);
    }

    /// `bytes` as the path of a `file://` URL: every byte but unreserved ASCII and `/` percent-encoded.
    fn encode(bytes: &[u8], upper: bool) -> String {
        let mut url = String::from("file://");
        for &byte in bytes {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
                url.push(char::from(byte));
            } else if upper {
                write!(url, "%{byte:02X}").unwrap();
            } else {
                write!(url, "%{byte:02x}").unwrap();
            }
        }
        url
    }

    proptest! {
        /// Whatever the text, the parser returns a path or an error: no panic, and a text that is not `scheme://`
        /// comes back unchanged.
        #[test]
        fn any_text_parses_or_is_refused(text in any::<String>()) {
            let parsed = path_of(Path::new(&text));
            if split_scheme(&text).is_none() && !text.is_empty() {
                prop_assert_eq!(parsed.unwrap(), PathBuf::from(&text));
            }
        }

        /// URL-shaped text, which the generator above rarely produces: the scheme decides the outcome.
        #[test]
        fn any_url_parses_or_is_refused(
            scheme in "[a-zA-Z][a-zA-Z0-9+.-]{0,8}",
            rest in "[ -~é%/]{0,24}",
        ) {
            let url = format!("{scheme}://{rest}");
            match path_of(Path::new(&url)) {
                Ok(_) => prop_assert!(scheme.eq_ignore_ascii_case("file")),
                Err(e) if e.code() == ErrorCode::Unsupported => {
                    prop_assert!(FUTURE_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str()));
                }
                Err(e) => prop_assert_eq!(e.code(), ErrorCode::InvalidArgument),
            }
        }
    }

    #[cfg(unix)]
    proptest! {
        /// Any absolute path without a NUL, percent-encoded into a `file://` URL in either hex case and with either
        /// spelling of the host, decodes to the same bytes.
        #[test]
        fn a_file_url_round_trips_any_path(
            tail in proptest::collection::vec(1u8..=255, 0..64),
            upper in any::<bool>(),
            localhost in any::<bool>(),
        ) {
            use std::os::unix::ffi::OsStrExt;
            let mut bytes = vec![b'/'];
            bytes.extend(tail);
            let mut url = encode(&bytes, upper);
            if localhost {
                url = url.replacen("file://", "file://localhost", 1);
            }
            let parsed = path_of(Path::new(&url)).unwrap();
            prop_assert_eq!(parsed.as_os_str().as_bytes(), &bytes[..]);
        }
    }
}
