//! The binary encoding of a document (SPEC §6.4.2, ADR-0023).

use persia_format::{BlobRef, Reader, Writer};

use super::cbor::{Decoder, Encoder, Failure, Item};
use super::{
    Document, FieldSet, MAX_DOCUMENT_FIELDS, MAX_DOCUMENT_LEN, MAX_ID_LEN, MAX_KEYWORD_LEN,
    MAX_STORED_DEPTH, Value, datetime,
};
use crate::schema::{CollectionSchema, FieldId, FieldType, field_name_problem};

/// The encoding version this build writes and reads.
const VERSION: u64 = 1;

/// A document encoding that cannot be decoded.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DecodeError {
    /// The bytes are not a valid encoding.
    #[error("corrupt document at offset {offset}: {reason}")]
    Corrupt {
        /// Offset from the start of the encoding.
        offset: u64,
        /// What was wrong.
        reason: DocumentCorruption,
    },
    /// The blob reference is corrupt or of an unsupported version; its offsets are from the start of the
    /// document encoding.
    #[error("document blob reference: {0}")]
    BlobRef(persia_format::Error),
    /// An encoding version above the one this build reads.
    #[error("document encoding version {version} (this build reads {VERSION})")]
    UnsupportedVersion {
        /// The version read.
        version: u64,
    },
}

impl DecodeError {
    /// Whether the input was corrupt, as opposed to valid input from a newer format.
    pub fn is_corruption(&self) -> bool {
        match self {
            Self::Corrupt { .. } => true,
            Self::BlobRef(e) => e.is_corruption(),
            Self::UnsupportedVersion { .. } => false,
        }
    }
}

/// Why a document encoding is corrupt (SPEC §6.4.2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocumentCorruption {
    /// The input ended inside an item, or a length runs past it.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// Bytes follow the document.
    #[error("trailing bytes after the document")]
    TrailingBytes,
    /// The input is longer than [`MAX_DOCUMENT_LEN`].
    #[error("{len} bytes, at most {MAX_DOCUMENT_LEN}")]
    TooLarge {
        /// Its length.
        len: usize,
    },
    /// An integer, length or count not in its shortest form.
    #[error("non-canonical (overlong) CBOR argument")]
    NonCanonicalHead,
    /// An item outside the CBOR subset: indefinite length, tag, 16- or 32-bit float, other simple value.
    #[error("unsupported CBOR item {initial:#04x}")]
    UnsupportedItem {
        /// Its initial byte.
        initial: u8,
    },
    /// An item of the wrong kind.
    #[error("expected {expected}, found {found}")]
    Unexpected {
        /// What the encoding requires here.
        expected: &'static str,
        /// What was found.
        found: &'static str,
    },
    /// A text string that is not UTF-8.
    #[error("text string is not UTF-8")]
    InvalidUtf8,
    /// Map keys not strictly increasing.
    #[error("map keys are not strictly increasing")]
    KeyOrder,
    /// A field id the schema does not have.
    #[error("field id {id} is not in the schema")]
    UnknownFieldId {
        /// The id read.
        id: u64,
    },
    /// An integer outside what its value allows (`i64`, a datetime, a stored integer), or version 0.
    #[error("{what} out of range")]
    OutOfRange {
        /// Which value.
        what: &'static str,
    },
    /// An `f64` field that is NaN, infinite or `-0.0`, or a non-finite float in a stored value.
    #[error("non-canonical float")]
    NonCanonicalFloat,
    /// A stored value nested deeper than [`MAX_STORED_DEPTH`](super::MAX_STORED_DEPTH).
    #[error("stored value nested too deep")]
    TooDeep,
    /// A value SPEC §6.4.1 rejects: id length, keyword length, extra field name, field count, a `null` stored value,
    /// or an extra field next to the schema field of the same name.
    #[error("invalid {what}")]
    Invalid {
        /// Which rule.
        what: &'static str,
    },
}

pub(super) fn encode(doc: &Document, schema: &CollectionSchema, set: FieldSet) -> Vec<u8> {
    let mut e = Encoder::default();
    write(&mut e, doc, schema, set);
    e.into_bytes()
}

/// The length of [`encode`] with every field, without building it.
pub(super) fn encoded_len(doc: &Document, schema: &CollectionSchema) -> usize {
    let mut e = Encoder::counting();
    write(&mut e, doc, schema, FieldSet::All);
    e.len()
}

fn write(e: &mut Encoder, doc: &Document, schema: &CollectionSchema, set: FieldSet) {
    e.array(5);
    e.uint(VERSION);
    e.text(&doc.id);
    match &doc.blob_ref {
        Some(blob) => {
            let mut w = Writer::new();
            blob.encode(&mut w);
            e.bytes(w.as_slice());
        }
        None => e.null(),
    }
    let included = |id: &FieldId| {
        set == FieldSet::All || schema.field_by_id(*id).is_some_and(|f| f.kind().stored())
    };
    e.map(doc.fields.keys().filter(|id| included(id)).count());
    for (id, value) in doc.fields.iter().filter(|(id, _)| included(id)) {
        e.uint(u64::from(id.get()));
        encode_value(e, value);
    }
    e.map(doc.extra.len());
    for (name, value) in &doc.extra {
        e.text(name);
        encode_json(e, value);
    }
}

fn encode_value(e: &mut Encoder, value: &Value) {
    match value {
        Value::String(s) => e.text(s),
        Value::Strings(list) => {
            e.array(list.len());
            for s in list {
                e.text(s);
            }
        }
        Value::I64(v) | Value::Datetime(v) => e.int(*v),
        Value::F64(v) => e.f64(*v),
        Value::Bool(v) => e.bool(*v),
        Value::Json(v) => encode_json(e, v),
    }
}

fn encode_json(e: &mut Encoder, value: &serde_json::Value) {
    use serde_json::Value as J;
    match value {
        J::Null => e.null(),
        J::Bool(v) => e.bool(*v),
        J::Number(n) => {
            if let Some(v) = n.as_u64() {
                e.uint(v);
            } else if let Some(v) = n.as_i64() {
                e.int(v);
            } else {
                // A serde_json number is an integer or a finite float.
                e.f64(n.as_f64().unwrap_or_default());
            }
        }
        J::String(s) => e.text(s),
        J::Array(items) => {
            e.array(items.len());
            for item in items {
                encode_json(e, item);
            }
        }
        J::Object(map) => {
            e.map(map.len());
            // The length does not depend on the order, so counting needs no sorted copy.
            if e.is_counting() {
                for (key, item) in map {
                    e.text(key);
                    encode_json(e, item);
                }
                return;
            }
            // serde_json's map is sorted unless a crate in the build enables `preserve_order`.
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            for (key, item) in entries {
                e.text(key);
                encode_json(e, item);
            }
        }
    }
}

pub(super) fn decode(bytes: &[u8], schema: &CollectionSchema) -> Result<Document, DecodeError> {
    let corrupt = |(offset, reason): Failure| DecodeError::Corrupt { offset, reason };
    if bytes.len() > MAX_DOCUMENT_LEN {
        return Err(corrupt((
            0,
            DocumentCorruption::TooLarge { len: bytes.len() },
        )));
    }
    let mut d = Decoder::new(bytes);
    let start = d.offset();
    // The version comes first, so a later version may change the arity and still be reported as unsupported.
    let items = match d.next().map_err(corrupt)? {
        Item::Array(n) if n >= 1 => n,
        other => {
            return Err(corrupt(unexpected(
                start,
                "a non-empty array",
                other.kind(),
            )));
        }
    };
    let at = d.offset();
    match d.next().map_err(corrupt)? {
        Item::Uint(VERSION) => {}
        Item::Uint(0) => {
            return Err(corrupt((
                at,
                DocumentCorruption::OutOfRange { what: "version" },
            )));
        }
        Item::Uint(version) => return Err(DecodeError::UnsupportedVersion { version }),
        other => return Err(corrupt(unexpected(at, "the version", other.kind()))),
    }
    if items != 5 {
        return Err(corrupt(unexpected(
            start,
            "an array of 5 items",
            "an array of another length",
        )));
    }
    let mut doc = Document::new(decode_id(&mut d).map_err(corrupt)?);
    doc.blob_ref = decode_blob_ref(&mut d)?;
    decode_fields(&mut d, schema, &mut doc).map_err(corrupt)?;
    decode_extra(&mut d, schema, &mut doc).map_err(corrupt)?;
    if !d.is_at_end() {
        return Err(corrupt((d.offset(), DocumentCorruption::TrailingBytes)));
    }
    Ok(doc)
}

fn unexpected(offset: u64, expected: &'static str, found: &'static str) -> Failure {
    (offset, DocumentCorruption::Unexpected { expected, found })
}

fn invalid(offset: u64, what: &'static str) -> Failure {
    (offset, DocumentCorruption::Invalid { what })
}

fn decode_id(d: &mut Decoder<'_>) -> Result<String, Failure> {
    let at = d.offset();
    match d.next()? {
        Item::Text(id) if (1..=MAX_ID_LEN).contains(&id.len()) => Ok(id.to_owned()),
        Item::Text(_) => Err(invalid(at, "id length")),
        other => Err(unexpected(at, "the id", other.kind())),
    }
}

fn decode_blob_ref(d: &mut Decoder<'_>) -> Result<Option<BlobRef>, DecodeError> {
    let at = d.offset();
    let corrupt = |(offset, reason): Failure| DecodeError::Corrupt { offset, reason };
    match d.next().map_err(corrupt)? {
        Item::Null => Ok(None),
        Item::Bytes(bytes) => {
            // The byte string's content ends where the decoder now is.
            let base = d
                .offset()
                .saturating_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            let mut r = Reader::with_base_offset(bytes, base);
            let blob = BlobRef::decode(&mut r).map_err(DecodeError::BlobRef)?;
            if !r.is_empty() {
                return Err(corrupt((r.offset(), DocumentCorruption::TrailingBytes)));
            }
            Ok(Some(blob))
        }
        other => Err(corrupt(unexpected(
            at,
            "a blob reference or null",
            other.kind(),
        ))),
    }
}

fn decode_fields(
    d: &mut Decoder<'_>,
    schema: &CollectionSchema,
    doc: &mut Document,
) -> Result<(), Failure> {
    let at = d.offset();
    let count = match d.next()? {
        Item::Map(n) if n <= MAX_DOCUMENT_FIELDS => n,
        Item::Map(_) => return Err(invalid(at, "field count")),
        other => return Err(unexpected(at, "the field map", other.kind())),
    };
    let mut previous = None;
    for _ in 0..count {
        let at = d.offset();
        let id = match d.next()? {
            Item::Uint(id) => id,
            other => return Err(unexpected(at, "a field id", other.kind())),
        };
        if previous.is_some_and(|p| p >= id) {
            return Err((at, DocumentCorruption::KeyOrder));
        }
        previous = Some(id);
        let field = u32::try_from(id)
            .ok()
            .and_then(|id| schema.fields().get(usize::try_from(id).ok()?))
            .ok_or((at, DocumentCorruption::UnknownFieldId { id }))?;
        let value = decode_value(d, field.field_type())?;
        doc.fields.insert(field.id(), value);
    }
    Ok(())
}

fn decode_value(d: &mut Decoder<'_>, field_type: FieldType) -> Result<Value, Failure> {
    let at = d.offset();
    let item = d.next()?;
    d.count_value(at)?;
    let wrong = |expected| unexpected(at, expected, item.kind());
    match field_type {
        FieldType::Text | FieldType::Keyword => {
            let keyword = field_type == FieldType::Keyword;
            match item {
                Item::Text(s) => Ok(Value::String(checked_str(at, s, keyword)?)),
                Item::Array(n) => {
                    let mut list = Vec::new();
                    for _ in 0..n {
                        let at = d.offset();
                        let item = d.next()?;
                        d.count_value(at)?;
                        match item {
                            Item::Text(s) => list.push(checked_str(at, s, keyword)?),
                            other => return Err(unexpected(at, "a text string", other.kind())),
                        }
                    }
                    Ok(Value::Strings(list))
                }
                _ => Err(wrong("a text string or an array")),
            }
        }
        FieldType::I64 => int(at, item, "i64").map(Value::I64),
        FieldType::Datetime => {
            let v = int(at, item, "datetime")?;
            if !datetime::in_range(v) {
                return Err((at, DocumentCorruption::OutOfRange { what: "datetime" }));
            }
            Ok(Value::Datetime(v))
        }
        FieldType::F64 => match item {
            Item::F64(v) if v.is_finite() && !(v == 0.0 && v.is_sign_negative()) => {
                Ok(Value::F64(v))
            }
            Item::F64(_) => Err((at, DocumentCorruption::NonCanonicalFloat)),
            _ => Err(wrong("a float")),
        },
        FieldType::Bool => match item {
            Item::Bool(v) => Ok(Value::Bool(v)),
            _ => Err(wrong("a bool")),
        },
        FieldType::Stored => match item {
            Item::Null => Err(invalid(at, "null stored value")),
            item => json(d, at, item, 0).map(Value::Json),
        },
    }
}

fn checked_str(at: u64, s: &str, keyword: bool) -> Result<String, Failure> {
    if keyword && s.len() > MAX_KEYWORD_LEN {
        return Err(invalid(at, "keyword length"));
    }
    Ok(s.to_owned())
}

fn int(at: u64, item: Item<'_>, what: &'static str) -> Result<i64, Failure> {
    let out_of_range = (at, DocumentCorruption::OutOfRange { what });
    match item {
        Item::Uint(v) => i64::try_from(v).map_err(|_| out_of_range),
        // -1 - n for n <= i64::MAX.
        Item::Nint(n) => i64::try_from(n).map(|n| -1 - n).map_err(|_| out_of_range),
        other => Err(unexpected(at, "an integer", other.kind())),
    }
}

/// A JSON value from `item` (already read at `at`) and the items after it; `depth` is the number of enclosing
/// arrays and objects.
fn json(
    d: &mut Decoder<'_>,
    at: u64,
    item: Item<'_>,
    depth: usize,
) -> Result<serde_json::Value, Failure> {
    use serde_json::Value as J;
    let nested = |d: &mut Decoder<'_>| -> Result<_, Failure> {
        let at = d.offset();
        let item = d.next()?;
        d.count_value(at)?;
        json(d, at, item, depth + 1)
    };
    match item {
        Item::Null => Ok(J::Null),
        Item::Bool(v) => Ok(J::Bool(v)),
        Item::Uint(v) => Ok(J::from(v)),
        Item::Nint(_) => int(at, item, "stored integer").map(J::from),
        Item::F64(v) => serde_json::Number::from_f64(v)
            .map(J::Number)
            .ok_or((at, DocumentCorruption::NonCanonicalFloat)),
        Item::Text(s) => Ok(J::String(s.to_owned())),
        Item::Bytes(_) => Err(unexpected(at, "a JSON value", "a byte string")),
        Item::Array(n) => {
            if depth >= MAX_STORED_DEPTH {
                return Err((at, DocumentCorruption::TooDeep));
            }
            let mut items = Vec::new();
            for _ in 0..n {
                items.push(nested(d)?);
            }
            Ok(J::Array(items))
        }
        Item::Map(n) => {
            if depth >= MAX_STORED_DEPTH {
                return Err((at, DocumentCorruption::TooDeep));
            }
            let mut map = serde_json::Map::new();
            let mut previous: Option<&str> = None;
            for _ in 0..n {
                let key_at = d.offset();
                let key = match d.next()? {
                    Item::Text(key) => key,
                    other => return Err(unexpected(key_at, "a text key", other.kind())),
                };
                if previous.is_some_and(|p| p.as_bytes() >= key.as_bytes()) {
                    return Err((key_at, DocumentCorruption::KeyOrder));
                }
                previous = Some(key);
                let value = nested(d)?;
                map.insert(key.to_owned(), value);
            }
            Ok(J::Object(map))
        }
    }
}

fn decode_extra(
    d: &mut Decoder<'_>,
    schema: &CollectionSchema,
    doc: &mut Document,
) -> Result<(), Failure> {
    let at = d.offset();
    let count = match d.next()? {
        Item::Map(n) => n,
        other => return Err(unexpected(at, "the extra field map", other.kind())),
    };
    if doc.fields.len().saturating_add(count) > MAX_DOCUMENT_FIELDS {
        return Err(invalid(at, "field count"));
    }
    let mut previous: Option<&str> = None;
    for _ in 0..count {
        let at = d.offset();
        let name = match d.next()? {
            Item::Text(name) => name,
            other => return Err(unexpected(at, "an extra field name", other.kind())),
        };
        if previous.is_some_and(|p| p.as_bytes() >= name.as_bytes()) {
            return Err((at, DocumentCorruption::KeyOrder));
        }
        previous = Some(name);
        if field_name_problem(name).is_some() {
            return Err(invalid(at, "extra field name"));
        }
        // Allowed alone (the field was added after the row was written, SPEC §6.4.2), never next to the field.
        if schema
            .field(name)
            .is_some_and(|f| doc.fields.contains_key(&f.id()))
        {
            return Err(invalid(at, "extra field named like a field it has"));
        }
        let value_at = d.offset();
        let item = d.next()?;
        d.count_value(value_at)?;
        let value = match item {
            Item::Null => return Err(invalid(value_at, "null stored value")),
            item => json(d, value_at, item, 0)?,
        };
        doc.extra.insert(name.to_owned(), value);
    }
    Ok(())
}
