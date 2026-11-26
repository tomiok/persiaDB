//! JSON Lines input, one document per line (ROADMAP 10.4.2): the line reader of `persia import`, here so a fuzz target
//! can reach it (the CLI is a binary).
//!
//! [`Lines`] splits untrusted bytes into numbered lines and decides nothing about JSON: a line is the bytes up to a
//! `\n` (a `\r` right before it is dropped too, so files written with `\r\n` read the same), or up to the end of the
//! input for a last line without one. Each line is checked in bounded memory before it is returned:
//! - at most `max` bytes, the line ending left out; [`MAX_DOCUMENT_JSON_LEN`] (16 MiB, SPEC §6.4.1) by default, since
//!   a document's JSON text is never longer. A longer line is [`LineError::TooLong`] after reading at most `max + 2`
//!   of its bytes, never the whole line;
//! - valid UTF-8 ([`LineError::NotUtf8`], with the offset of the first invalid byte).
//!
//! Lines are numbered from 1, every line counting, blank ones included, so an error names the line an editor shows.
//! The input ends at its first error: after one, [`Lines::next_line`] returns `Ok(None)`.

use std::io::{self, BufRead, Read};

use persia_engine::document::MAX_DOCUMENT_JSON_LEN;

/// Why [`Lines::next_line`] could not return a line. Each names the 1-based line it was reading.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LineError {
    /// Reading the input failed.
    #[error("line {line}: reading the input failed")]
    Io {
        /// The line being read.
        line: u64,
        /// The reader's error.
        #[source]
        source: io::Error,
    },
    /// The line is longer than the bound, its line ending left out.
    #[error("line {line}: longer than {max} bytes")]
    TooLong {
        /// The line.
        line: u64,
        /// The bound, in bytes.
        max: usize,
    },
    /// The line is not valid UTF-8.
    #[error("line {line}: not valid UTF-8 (byte {at} of the line)")]
    NotUtf8 {
        /// The line.
        line: u64,
        /// The offset in the line of the first byte that is not valid UTF-8.
        at: usize,
    },
}

impl LineError {
    /// The 1-based number of the line the error is about.
    ///
    /// ```
    /// use persia::internal::jsonl::Lines;
    ///
    /// let mut lines = Lines::new(&b"{}\n\xff\n"[..]);
    /// assert!(lines.next_line()?.is_some());
    /// assert_eq!(lines.next_line().unwrap_err().line(), 2);
    /// # Ok::<(), persia::internal::jsonl::LineError>(())
    /// ```
    pub fn line(&self) -> u64 {
        match self {
            Self::Io { line, .. } | Self::TooLong { line, .. } | Self::NotUtf8 { line, .. } => {
                *line
            }
        }
    }
}

/// One line of the input: its 1-based number and its text, without the line ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line<'a> {
    /// The line's number, from 1.
    pub number: u64,
    /// The line's text, without its `\n` or `\r\n`.
    pub text: &'a str,
}

/// The lines of a JSON Lines input (module docs): numbered, bounded, valid UTF-8.
#[derive(Debug)]
pub struct Lines<R> {
    reader: R,
    buf: Vec<u8>,
    /// The number of the last line returned or failed.
    number: u64,
    max: usize,
    /// An error ended the input.
    ended: bool,
}

impl<R: BufRead> Lines<R> {
    /// The lines of `reader`, each at most [`MAX_DOCUMENT_JSON_LEN`] bytes.
    ///
    /// ```
    /// use persia::internal::jsonl::Lines;
    ///
    /// let mut lines = Lines::new(&b"{\"id\": \"a\"}\r\n\n{\"id\": \"b\"}"[..]);
    /// let mut read = Vec::new();
    /// while let Some(line) = lines.next_line()? {
    ///     read.push((line.number, line.text.to_owned()));
    /// }
    /// assert_eq!(read, [(1, r#"{"id": "a"}"#.to_owned()), (2, String::new()), (3, r#"{"id": "b"}"#.to_owned())]);
    /// # Ok::<(), persia::internal::jsonl::LineError>(())
    /// ```
    pub fn new(reader: R) -> Self {
        Self::with_max(reader, MAX_DOCUMENT_JSON_LEN)
    }

    /// The lines of `reader`, each at most `max` bytes, the line ending left out.
    ///
    /// ```
    /// use persia::internal::jsonl::{LineError, Lines};
    ///
    /// let mut lines = Lines::with_max(&b"1234\n12345\n"[..], 4);
    /// assert_eq!(lines.next_line()?.map(|l| l.text), Some("1234"));
    /// assert!(matches!(lines.next_line(), Err(LineError::TooLong { line: 2, max: 4 })));
    /// assert!(lines.next_line()?.is_none()); // the input ends at its first error
    /// # Ok::<(), LineError>(())
    /// ```
    pub fn with_max(reader: R, max: usize) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            number: 0,
            max,
            ended: false,
        }
    }

    /// The next line, `Ok(None)` at the end of the input or after an error.
    ///
    /// # Errors
    /// [`LineError::Io`], [`LineError::TooLong`], [`LineError::NotUtf8`], naming the line.
    pub fn next_line(&mut self) -> Result<Option<Line<'_>>, LineError> {
        if self.ended {
            return Ok(None);
        }
        let line = self.number.saturating_add(1);
        match self.read_line(line) {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(e) => {
                self.ended = true;
                self.number = line;
                return Err(e);
            }
        }
        self.number = line;
        match std::str::from_utf8(&self.buf) {
            Ok(text) => Ok(Some(Line { number: line, text })),
            Err(e) => {
                self.ended = true;
                Err(LineError::NotUtf8 {
                    line,
                    at: e.valid_up_to(),
                })
            }
        }
    }

    /// Reads line `line` into `buf`, its ending dropped: `false` at the end of the input. Reads at most `max + 2`
    /// bytes (the longest line with a `\r\n`), so a longer line fails without being read whole.
    fn read_line(&mut self, line: u64) -> Result<bool, LineError> {
        self.buf.clear();
        // `max + 2` fits in a u64 for any usize `max` on every platform Rust supports; saturate rather than assume.
        let limit = u64::try_from(self.max)
            .unwrap_or(u64::MAX)
            .saturating_add(2);
        let read = (&mut self.reader)
            .take(limit)
            .read_until(b'\n', &mut self.buf)
            .map_err(|source| LineError::Io { line, source })?;
        if read == 0 {
            return Ok(false);
        }
        if self.buf.last() == Some(&b'\n') {
            self.buf.pop();
            if self.buf.last() == Some(&b'\r') {
                self.buf.pop();
            }
        }
        if self.buf.len() > self.max {
            return Err(LineError::TooLong {
                line,
                max: self.max,
            });
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::*;

    /// Every line of `input`, or the error that ended it, as owned values.
    fn all(input: &[u8], max: usize) -> (Vec<(u64, String)>, Option<LineError>) {
        let mut lines = Lines::with_max(input, max);
        let mut read = Vec::new();
        loop {
            match lines.next_line() {
                Ok(Some(line)) => read.push((line.number, line.text.to_owned())),
                Ok(None) => return (read, None),
                Err(e) => {
                    assert!(
                        lines.next_line().unwrap().is_none(),
                        "the input ends at an error"
                    );
                    return (read, Some(e));
                }
            }
        }
    }

    fn texts(read: &[(u64, String)]) -> Vec<&str> {
        read.iter().map(|(_, t)| t.as_str()).collect()
    }

    #[test]
    fn lines_split_on_newlines_and_drop_a_carriage_return_before_one() {
        let (read, err) = all(b"a\r\nb\n\nc\r\r\nd", 8);
        assert!(err.is_none());
        assert_eq!(texts(&read), ["a", "b", "", "c\r", "d"]);
        assert_eq!(
            read.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
        // A last line without an ending, and a lone `\r` at the end, are kept as they are.
        assert_eq!(texts(&all(b"x\r", 8).0), ["x\r"]);
        assert_eq!(all(b"", 8).0, []);
        assert_eq!(texts(&all(b"\n", 8).0), [""]);
    }

    /// The bound is on the text: a line of exactly `max` bytes passes with either ending or none; one more byte fails
    /// with that line's number.
    #[test]
    fn a_line_of_max_bytes_passes_and_one_more_fails() {
        for ending in [&b""[..], b"\n", b"\r\n"] {
            let mut input = b"ok\n".to_vec();
            input.extend_from_slice(b"1234");
            input.extend_from_slice(ending);
            let (read, err) = all(&input, 4);
            assert!(err.is_none(), "{ending:?}: {err:?}");
            assert_eq!(texts(&read), ["ok", "1234"]);

            let mut input = b"ok\n".to_vec();
            input.extend_from_slice(b"12345");
            input.extend_from_slice(ending);
            input.extend_from_slice(b"\nnever read\n");
            let (read, err) = all(&input, 4);
            assert_eq!(texts(&read), ["ok"]);
            assert!(
                matches!(err, Some(LineError::TooLong { line: 2, max: 4 })),
                "{ending:?}: {err:?}"
            );
        }
        // `max` 0: only empty lines.
        assert!(matches!(
            all(b"\n\nx", 0).1,
            Some(LineError::TooLong { line: 3, max: 0 })
        ));
    }

    /// A line too long is rejected after at most `max + 2` of its bytes: the rest of a huge line is never read.
    #[test]
    fn a_long_line_is_not_read_whole() {
        struct Counting<'a> {
            inner: &'a [u8],
            read: usize,
        }
        impl Read for Counting<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.inner.read(buf)?;
                self.read += n;
                Ok(n)
            }
        }
        let huge = vec![b'x'; 1 << 20];
        let mut counting = Counting {
            inner: &huge,
            read: 0,
        };
        let mut lines = Lines::with_max(io::BufReader::with_capacity(16, &mut counting), 100);
        assert!(matches!(
            lines.next_line(),
            Err(LineError::TooLong { line: 1, .. })
        ));
        drop(lines);
        assert!(counting.read <= 102 + 16, "read {} bytes", counting.read);
    }

    #[test]
    fn invalid_utf8_names_its_line_and_offset() {
        let (read, err) = all(b"{}\n\"ok\"\nab\xffc\nlater\n", 64);
        assert_eq!(texts(&read), ["{}", "\"ok\""]);
        let err = err.unwrap();
        assert!(
            matches!(err, LineError::NotUtf8 { line: 3, at: 2 }),
            "{err:?}"
        );
        assert_eq!(err.line(), 3);
        assert_eq!(
            err.to_string(),
            "line 3: not valid UTF-8 (byte 2 of the line)"
        );
        // A sequence cut by the end of the line is invalid too: lines never join.
        assert!(matches!(
            all("é".as_bytes().get(..1).unwrap(), 8).1,
            Some(LineError::NotUtf8 { line: 1, at: 0 })
        ));
    }

    /// A reader that fails after its first line.
    struct Failing(bool);

    impl Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if std::mem::replace(&mut self.0, true) {
                return Err(io::Error::other("disk gone"));
            }
            let line = b"first\n";
            buf.get_mut(..line.len()).unwrap().copy_from_slice(line);
            Ok(line.len())
        }
    }

    #[test]
    fn a_read_error_names_the_line_being_read() {
        let mut lines = Lines::new(io::BufReader::new(Failing(false)));
        assert_eq!(lines.next_line().unwrap().map(|l| l.text), Some("first"));
        let err = lines.next_line().unwrap_err();
        assert!(matches!(&err, LineError::Io { line: 2, .. }), "{err:?}");
        assert_eq!(err.line(), 2);
        assert_eq!(err.to_string(), "line 2: reading the input failed");
        assert!(lines.next_line().unwrap().is_none());
    }

    #[test]
    fn the_default_bound_is_the_document_json_limit() {
        let mut lines = Lines::new(&b""[..]);
        assert_eq!(lines.max, MAX_DOCUMENT_JSON_LEN);
        assert!(lines.next_line().unwrap().is_none());
    }

    fn line_text() -> impl Strategy<Value = String> {
        // Any text without a line ending; a `\r` inside a line is kept (only `\r\n` ends one). A `\r` at the end of
        // a line is a unit test's: before a `\n` it is that ending.
        prop_oneof![
            3 => "[^\n\r]{0,12}",
            1 => "[^\n\r]{0,3}\r[^\n\r]{1,3}",
            1 => Just(String::new()),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(if cfg!(feature = "slow") { 2048 } else { 256 }))]

        /// The round trip: lines joined with `\n` or `\r\n` (the last one with or without an ending) read back as
        /// the same lines, numbered from 1, when none is longer than the bound; otherwise the input ends at the first
        /// long one, with its number, after the lines before it.
        #[test]
        fn joined_lines_read_back_as_themselves(
            lines in vec((line_text(), any::<bool>()), 0..12),
            last_ending in any::<bool>(),
            max in 0usize..16,
        ) {
            let mut input = Vec::new();
            for (i, (text, crlf)) in lines.iter().enumerate() {
                input.extend_from_slice(text.as_bytes());
                if i + 1 < lines.len() || last_ending {
                    input.extend_from_slice(if *crlf { b"\r\n" } else { b"\n" });
                }
            }
            // A last line that is empty and has no ending is no line at all.
            let expected: Vec<&str> = lines
                .iter()
                .enumerate()
                .filter(|(i, (text, _))| !(i + 1 == lines.len() && !last_ending && text.is_empty()))
                .map(|(_, (text, _))| text.as_str())
                .collect();
            let (read, err) = all(&input, max);
            let long = expected.iter().position(|t| t.len() > max);
            prop_assert_eq!(texts(&read), &expected[..long.unwrap_or(expected.len())]);
            for (i, (number, _)) in read.iter().enumerate() {
                prop_assert_eq!(*number, u64::try_from(i).unwrap() + 1);
            }
            match (long, err) {
                (None, None) => {}
                (Some(at), Some(LineError::TooLong { line, max: m })) => {
                    prop_assert_eq!(line, u64::try_from(at).unwrap() + 1);
                    prop_assert_eq!(m, max);
                }
                (long, err) => prop_assert!(false, "{:?} {:?}", long, err),
            }
        }

        /// Arbitrary bytes never panic, and every line returned is within the bound and holds no line feed.
        #[test]
        fn arbitrary_bytes_give_bounded_lines(input in vec(any::<u8>(), 0..64), max in 0usize..8) {
            let (read, _) = all(&input, max);
            for (_, text) in &read {
                prop_assert!(text.len() <= max && !text.contains('\n'));
            }
        }
    }
}
