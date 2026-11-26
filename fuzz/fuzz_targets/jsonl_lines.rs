//! Fuzz the JSON Lines reader of `persia import` (ROADMAP 10.4.2; SPEC §6.4.1): arbitrary bytes never panic; the
//! lines come numbered from 1 without a gap, each within the bound, valid UTF-8 and without a line feed; an error
//! names the line after the last one returned and ends the input; and an input read without an error is the lines
//! joined by their endings. Each line is also read as a document of a schema with every field type, as the import
//! reads it, which must not panic either.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia::internal::jsonl::{LineError, Lines};
use persia_engine::document::Document;
use persia_engine::schema::{
    CollectionSchema, FieldKind, KeywordOptions, SchemaBuilder, TextOptions,
};

fn schema() -> CollectionSchema {
    SchemaBuilder::new("fuzz")
        .field("a", FieldKind::Text(TextOptions::default()))
        .field("b", FieldKind::Keyword(KeywordOptions::default()))
        .field("c", FieldKind::I64 { stored: true })
        .field("d", FieldKind::F64 { stored: true })
        .field("e", FieldKind::Bool { stored: true })
        .field("f", FieldKind::Datetime { stored: true })
        .field("g", FieldKind::Stored)
        .strict(false)
        .build()
        .expect("a valid schema")
}

fuzz_target!(|data: &[u8]| {
    // The first byte picks a small bound, so long lines are reached; the rest is the input.
    let Some((&bound, input)) = data.split_first() else {
        return;
    };
    let max = usize::from(bound % 64);
    let schema = schema();
    let mut lines = Lines::with_max(input, max);
    let mut texts: Vec<String> = Vec::new();
    let mut expected = 1u64;
    let failed = loop {
        match lines.next_line() {
            Ok(Some(line)) => {
                assert_eq!(line.number, expected, "lines are numbered without a gap");
                assert!(line.text.len() <= max, "a line is within the bound");
                assert!(!line.text.contains('\n'), "a line holds no line feed");
                drop(Document::from_json(line.text, &schema));
                texts.push(line.text.to_owned());
                expected += 1;
            }
            Ok(None) => break None,
            Err(e) => {
                assert_eq!(e.line(), expected, "an error names the line being read");
                assert!(matches!(
                    e,
                    LineError::TooLong { .. } | LineError::NotUtf8 { .. }
                ));
                break Some(e);
            }
        }
    };
    if failed.is_some() {
        assert!(
            matches!(lines.next_line(), Ok(None)),
            "an error ends the input"
        );
        return;
    }
    // Without an error, the input is the lines joined by `\n` or `\r\n`, the last one's ending optional.
    let mut rest = input;
    for (i, text) in texts.iter().enumerate() {
        rest = rest
            .strip_prefix(text.as_bytes())
            .expect("the input starts with the line");
        let ending: &[u8] = if rest.starts_with(b"\r\n") {
            b"\r\n"
        } else if rest.starts_with(b"\n") {
            b"\n"
        } else {
            assert_eq!(i + 1, texts.len(), "only the last line may end the input");
            b""
        };
        rest = rest.get(ending.len()..).expect("the ending is there");
    }
    assert!(rest.is_empty(), "every byte is in a line");
});
