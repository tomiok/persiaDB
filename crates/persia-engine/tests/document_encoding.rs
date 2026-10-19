//! The binary document encoding, hardened (SPEC §6.4.2, ADR-0023): golden bytes for every head width and value
//! shape, "exactly one encoding" checked by mutating each item of real encodings, exhaustive bit flips and
//! truncations, hostile nesting, and the limits as the decoder sees them.
//!
//! Invariants:
//! 1. `decode` accepts exactly one encoding per document: every overlong head, indefinite length, tag, short
//!    float, out-of-order or duplicate map key is `Corrupt` at the offset of the offending item.
//! 2. Whatever `decode` accepts re-encodes to the same bytes, except a row holding what a new document cannot
//!    (an extra field named like a schema field it lacks, extras of a now-strict collection).
//! 3. Every strict prefix of an encoding is `Corrupt { UnexpectedEof }`; every damaged encoding is an error or a
//!    canonical document, never a panic.
//! 4. Nesting is bounded on decode and validate, so hostile depth cannot exhaust the stack.

// Helpers outside #[test] functions are test code too; long table-driven tests and small casts on test data.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;

use persia_engine::document::{
    DecodeError, Document, DocumentCorruption, DocumentError, FieldSet, MAX_DATETIME,
    MAX_DOCUMENT_FIELDS, MAX_DOCUMENT_LEN, MAX_ID_LEN, MAX_KEYWORD_LEN, MAX_STORED_DEPTH,
    MIN_DATETIME, Value,
};
use persia_engine::schema::{
    CollectionSchema, FieldId, FieldKind, FieldType, KeywordOptions, SchemaBuilder, TextOptions,
};
use persia_format::{BlobId, BlobRef};
use proptest::prelude::*;
use serde_json::json;

/// One field of each type, ids `a`=0 … `g`=6.
fn all_types(strict: bool) -> CollectionSchema {
    SchemaBuilder::new("all")
        .field("a", FieldKind::Text(TextOptions::default()))
        .field("b", FieldKind::Keyword(KeywordOptions::default()))
        .field("c", FieldKind::I64 { stored: true })
        .field("d", FieldKind::F64 { stored: true })
        .field("e", FieldKind::Bool { stored: true })
        .field("f", FieldKind::Datetime { stored: true })
        .field("g", FieldKind::Stored)
        .strict(strict)
        .build()
        .unwrap()
}

fn id(schema: &CollectionSchema, name: &str) -> FieldId {
    schema.field(name).unwrap().id()
}

fn corrupt(offset: u64, reason: DocumentCorruption) -> Result<Document, DecodeError> {
    Err(DecodeError::Corrupt { offset, reason })
}

/// 31 fields so ids 24 and 25 (`q`, `r`) need a one-byte argument: `a`..`g` as in [`all_types`], fillers `p00`..`p16`
/// (ids 7..=23), then `q` (bool) and `r` (datetime). Not strict.
fn wide() -> CollectionSchema {
    let base = SchemaBuilder::new("wide")
        .field("a", FieldKind::Text(TextOptions::default()))
        .field("b", FieldKind::Keyword(KeywordOptions::default()))
        .field("c", FieldKind::I64 { stored: true })
        .field("d", FieldKind::I64 { stored: true })
        .field("e", FieldKind::F64 { stored: true })
        .field("f", FieldKind::Datetime { stored: true })
        .field("g", FieldKind::Stored)
        .field("q", FieldKind::Bool { stored: true })
        .field("r", FieldKind::Datetime { stored: true });
    (0..17)
        .fold(base, |b, i| b.field(format!("p{i:02}"), FieldKind::Stored))
        .strict(false)
        .build()
        .unwrap()
}

fn wide_doc(schema: &CollectionSchema) -> Document {
    let mut doc = Document::new("abcdefghijklmnopqrstuvwx")
        .with(id(schema, "a"), Value::String("t".repeat(256)))
        .with(id(schema, "b"), Value::Strings(vec!["k".into(); 24]))
        .with(id(schema, "c"), Value::I64(i64::MIN))
        .with(id(schema, "d"), Value::I64(-257))
        .with(id(schema, "e"), Value::F64(f64::MIN_POSITIVE))
        .with(id(schema, "f"), Value::Datetime(MIN_DATETIME))
        .with(
            id(schema, "g"),
            Value::Json(json!({
                "é": [-1, -24, -25, -256, -257, -65_536, -65_537, -4_294_967_296i64, -4_294_967_297i64,
                      u64::MAX, -0.0, 5e-324],
                "a": {"b": {"c": []}},
                "B": true,
            })),
        )
        .with(id(schema, "q"), Value::Bool(false))
        .with(id(schema, "r"), Value::Datetime(MAX_DATETIME));
    doc.insert_extra("alpha", json!("x".repeat(65_536)));
    doc.insert_extra("a_b", json!([null]));
    doc.insert_extra("aZ", json!("s"));
    doc.insert_extra("Zed", json!({}));
    doc
}

/// Golden vector (SPEC §6.4.2): every head width (inline, 1, 2, 4 and 8 argument bytes) for text, arrays, maps,
/// field ids and integers of both signs; nested stored maps in byte order of their keys; extra fields in byte
/// order; the datetime range ends. `DocStore` persists these bytes, so they must never change.
#[test]
fn wide_heads_golden() {
    let schema = wide();
    assert_eq!(id(&schema, "q").get(), 24);
    assert_eq!(id(&schema, "r").get(), 25);
    let doc = wide_doc(&schema);
    let mut expected: Vec<u8> = vec![0x85, 0x01, 0x78, 0x18];
    expected.extend(b"abcdefghijklmnopqrstuvwx");
    expected.extend([0xf6, 0xa9]);
    // a: 256-byte text.
    expected.extend([0x00, 0x79, 0x01, 0x00]);
    expected.extend([b't'; 256]);
    // b: 24 keywords.
    expected.extend([0x01, 0x98, 0x18]);
    for _ in 0..24 {
        expected.extend([0x61, b'k']);
    }
    #[rustfmt::skip]
    expected.extend([
        // c: i64::MIN; d: -257.
        0x02, 0x3b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0x03, 0x39, 0x01, 0x00,
        // e: the smallest normal f64.
        0x04, 0xfb, 0x00, 0x10, 0, 0, 0, 0, 0, 0,
        // f: 0000-01-01T00:00:00Z = -62167219200000000 µs = -1 - 0x00dcdcc1a916ffff.
        0x05, 0x3b, 0x00, 0xdc, 0xdc, 0xc1, 0xa9, 0x16, 0xff, 0xff,
        // g: {"B": true, "a": {"b": {"c": []}}, "é": [...]} with keys in byte order.
        0x06, 0xa3,
        0x61, b'B', 0xf5,
        0x61, b'a', 0xa1, 0x61, b'b', 0xa1, 0x61, b'c', 0x80,
        0x62, 0xc3, 0xa9, 0x8c,
        0x20,
        0x37,
        0x38, 0x18,
        0x38, 0xff,
        0x39, 0x01, 0x00,
        0x39, 0xff, 0xff,
        0x3a, 0x00, 0x01, 0x00, 0x00,
        0x3a, 0xff, 0xff, 0xff, 0xff,
        0x3b, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xfb, 0x80, 0, 0, 0, 0, 0, 0, 0,
        0xfb, 0, 0, 0, 0, 0, 0, 0, 0x01,
        // q (id 24): false; r (id 25): 9999-12-31T23:59:59.999999Z = 0x0384440ccc735fff µs.
        0x18, 0x18, 0xf4,
        0x18, 0x19, 0x1b, 0x03, 0x84, 0x44, 0x0c, 0xcc, 0x73, 0x5f, 0xff,
        // Extra fields in byte order: "Zed" < "aZ" < "a_b" < "alpha".
        0xa4,
        0x63, b'Z', b'e', b'd', 0xa0,
        0x62, b'a', b'Z', 0x61, b's',
        0x63, b'a', b'_', b'b', 0x81, 0xf6,
        0x65, b'a', b'l', b'p', b'h', b'a', 0x7a, 0x00, 0x01, 0x00, 0x00,
    ]);
    expected.extend(std::iter::repeat_n(b'x', 65_536));
    let bytes = doc.encode(&schema, FieldSet::All).unwrap();
    assert_eq!(bytes.len(), expected.len());
    assert_eq!(bytes, expected);
    let back = Document::decode(&expected, &schema).unwrap();
    assert_eq!(back, doc);
    // The stored -0.0 and the subnormal keep their bits.
    let Some(Value::Json(g)) = back.get(id(&schema, "g")) else {
        panic!()
    };
    assert_eq!(g["é"][10].as_f64().unwrap().to_bits(), (-0.0f64).to_bits());
    assert_eq!(g["é"][11].as_f64().unwrap().to_bits(), 1);
    // Determinism: the same document always gives the same bytes.
    assert_eq!(doc.clone().encode(&schema, FieldSet::All).unwrap(), bytes);
}

/// Golden vector: a `DocStore` row keeps the id, the blob reference and the extra fields, and leaves out exactly
/// the fields with `stored: false` (SPEC §6.4.2).
#[test]
fn stored_row_golden() {
    let schema = SchemaBuilder::new("row")
        .field(
            "body",
            FieldKind::Text(TextOptions {
                stored: false,
                ..TextOptions::default()
            }),
        )
        .field("n", FieldKind::I64 { stored: false })
        .field("tag", FieldKind::Keyword(KeywordOptions::default()))
        .strict(false)
        .build()
        .unwrap();
    assert!(!schema.field("body").unwrap().kind().stored());
    let mut doc = Document::new("r")
        .with(id(&schema, "body"), Value::String("text".into()))
        .with(id(&schema, "n"), Value::I64(7))
        .with(id(&schema, "tag"), Value::Strings(vec![]));
    doc.set_blob_ref(Some(
        BlobRef::new(BlobId::from_bytes([1; 32]), 0, "x/y", 1, BTreeMap::new()).unwrap(),
    ));
    doc.insert_extra("x", json!(-1));
    let mut expected = vec![0x85, 0x01, 0x61, b'r', 0x58, 40, 1];
    expected.extend([1; 32]);
    expected.extend([0, 1, 3, b'x', b'/', b'y', 0]);
    expected.extend([0xa1, 0x02, 0x80, 0xa1, 0x61, b'x', 0x20]);
    let row = doc.encode(&schema, FieldSet::Stored).unwrap();
    assert_eq!(row, expected);
    let mut kept = doc.clone();
    kept.remove(id(&schema, "body"));
    kept.remove(id(&schema, "n"));
    assert_eq!(Document::decode(&row, &schema).unwrap(), kept);
}

// ---- A minimal CBOR tree, to rewrite single items of real encodings ----

/// One CBOR item as the production encoder writes it, with the head's additional info overridable.
#[derive(Debug, Clone)]
enum Node {
    Head {
        major: u8,
        arg: u64,
        /// `None`: the shortest form; `Some(24..=27)`: that many argument bytes; `Some(28..=31)`: the initial
        /// byte alone (reserved or indefinite).
        info: Option<u8>,
        /// The content of a byte or text string.
        payload: Vec<u8>,
        /// Array items, or map keys and values interleaved.
        children: Vec<Node>,
    },
    /// A simple value or float, as raw bytes.
    Raw(Vec<u8>),
}

fn parse(bytes: &[u8], pos: &mut usize) -> Node {
    let initial = bytes[*pos];
    *pos += 1;
    let major = initial >> 5;
    if major == 7 {
        let len = if initial == 0xfb { 8 } else { 0 };
        let raw = bytes[*pos - 1..*pos + len].to_vec();
        *pos += len;
        return Node::Raw(raw);
    }
    let width = match initial & 0x1f {
        0..24 => 0,
        24 => 1,
        25 => 2,
        26 => 4,
        27 => 8,
        other => panic!("not produced by the encoder: {other}"),
    };
    let arg = if width == 0 {
        u64::from(initial & 0x1f)
    } else {
        let mut be = [0; 8];
        be[8 - width..].copy_from_slice(&bytes[*pos..*pos + width]);
        *pos += width;
        u64::from_be_bytes(be)
    };
    let (mut payload, mut children) = (Vec::new(), Vec::new());
    match major {
        2 | 3 => {
            let len = usize::try_from(arg).unwrap();
            payload = bytes[*pos..*pos + len].to_vec();
            *pos += len;
        }
        4 => children = (0..arg).map(|_| parse(bytes, pos)).collect(),
        5 => children = (0..2 * arg).map(|_| parse(bytes, pos)).collect(),
        _ => {}
    }
    Node::Head {
        major,
        arg,
        info: None,
        payload,
        children,
    }
}

fn shortest_info(arg: u64) -> u8 {
    match arg {
        0..24 => 0,
        24..0x100 => 24,
        0x100..0x1_0000 => 25,
        0x1_0000..0x1_0000_0000 => 26,
        _ => 27,
    }
}

/// Writes `node`; `offsets` gets the offset of each item in pre-order.
fn write(node: &Node, out: &mut Vec<u8>, offsets: &mut Vec<u64>) {
    offsets.push(out.len() as u64);
    match node {
        Node::Raw(raw) => out.extend(raw),
        Node::Head {
            major,
            arg,
            info,
            payload,
            children,
        } => {
            let info = info.unwrap_or_else(|| shortest_info(*arg));
            let major = major << 5;
            match info {
                0..24 => out.push(major | u8::try_from(*arg).unwrap()),
                24..=27 => {
                    out.push(major | info);
                    let width = 1 << (info - 24);
                    out.extend(&arg.to_be_bytes()[8 - width..]);
                }
                _ => out.push(major | info),
            }
            out.extend(payload);
            for child in children {
                write(child, out, offsets);
            }
        }
    }
}

fn encode_tree(node: &Node) -> (Vec<u8>, Vec<u64>) {
    let (mut out, mut offsets) = (Vec::new(), Vec::new());
    write(node, &mut out, &mut offsets);
    (out, offsets)
}

/// The item at pre-order index `k`.
fn nth<'a>(node: &'a mut Node, k: &mut usize) -> Option<&'a mut Node> {
    if *k == 0 {
        return Some(node);
    }
    *k -= 1;
    match node {
        Node::Raw(_) => None,
        Node::Head { children, .. } => children.iter_mut().find_map(|c| nth(c, k)),
    }
}

fn at(tree: &Node, k: usize) -> Node {
    let mut tree = tree.clone();
    nth(&mut tree, &mut { k }).unwrap().clone()
}

fn with_item(tree: &Node, k: usize, f: impl FnOnce(&mut Node)) -> Node {
    let mut tree = tree.clone();
    f(nth(&mut tree, &mut { k }).unwrap());
    tree
}

/// Every way to rewrite one item of `bytes` into a non-canonical or out-of-subset form, each with the error the
/// decoder must report (SPEC §6.4.2).
fn non_canonical_rewrites(bytes: &[u8]) -> Vec<(Vec<u8>, u64, DocumentCorruption)> {
    let mut pos = 0;
    let tree = parse(bytes, &mut pos);
    assert_eq!(pos, bytes.len());
    let (same, offsets) = encode_tree(&tree);
    assert_eq!(same, bytes, "the test tree writes what it read");
    let mut out = Vec::new();
    for (k, &offset) in offsets.iter().enumerate() {
        // A tag in front of any item.
        let mut tagged = bytes.to_vec();
        tagged.insert(usize::try_from(offset).unwrap(), 0xc1);
        out.push((
            tagged,
            offset,
            DocumentCorruption::UnsupportedItem { initial: 0xc1 },
        ));
        match at(&tree, k) {
            Node::Head { major, arg, .. } => {
                // Every longer argument form.
                for info in (shortest_info(arg).max(23) + 1)..=27 {
                    let t = with_item(&tree, k, |n| {
                        if let Node::Head { info: i, .. } = n {
                            *i = Some(info);
                        }
                    });
                    out.push((
                        encode_tree(&t).0,
                        offset,
                        DocumentCorruption::NonCanonicalHead,
                    ));
                }
                // Reserved additional info, and indefinite lengths.
                for info in 28..=31 {
                    let t = with_item(&tree, k, |n| {
                        if let Node::Head { info: i, .. } = n {
                            *i = Some(info);
                        }
                    });
                    out.push((
                        encode_tree(&t).0,
                        offset,
                        DocumentCorruption::UnsupportedItem {
                            initial: (major << 5) | info,
                        },
                    ));
                }
            }
            Node::Raw(raw) if raw[0] == 0xfb => {
                // A 16- or 32-bit float in place of the 64-bit one.
                for (initial, len) in [(0xf9, 2), (0xfa, 4)] {
                    let mut short = vec![initial];
                    short.extend(&raw[1..=len]);
                    let t = with_item(&tree, k, |n| *n = Node::Raw(short));
                    out.push((
                        encode_tree(&t).0,
                        offset,
                        DocumentCorruption::UnsupportedItem { initial },
                    ));
                }
            }
            Node::Raw(_) => {}
        }
        // Map entries swapped, or one repeated: the later key is out of order.
        if let Node::Head {
            major: 5,
            children,
            arg,
            ..
        } = at(&tree, k)
        {
            let head_len = encode_tree(&Node::Head {
                major: 5,
                arg,
                info: None,
                payload: vec![],
                children: vec![],
            })
            .0
            .len() as u64;
            for i in 0..children.len() / 2 - usize::from(!children.is_empty()) {
                for repeat in [false, true] {
                    let mut entries = children.clone();
                    if repeat {
                        entries[2 * i + 2] = entries[2 * i].clone();
                        entries[2 * i + 3] = entries[2 * i + 1].clone();
                    } else {
                        entries[2 * i..2 * i + 4].rotate_left(2);
                    }
                    let before: u64 = entries[..2 * i + 2]
                        .iter()
                        .map(|c| encode_tree(c).0.len() as u64)
                        .sum();
                    let t = with_item(&tree, k, |n| {
                        if let Node::Head { children, .. } = n {
                            *children = entries;
                        }
                    });
                    out.push((
                        encode_tree(&t).0,
                        offset + head_len + before,
                        DocumentCorruption::KeyOrder,
                    ));
                }
            }
        }
    }
    out
}

#[test]
fn every_non_canonical_rewrite_of_the_wide_golden_is_corrupt_where_it_happens() {
    let schema = wide();
    let mut doc = wide_doc(&schema);
    // Keep it small enough to rewrite every item quickly.
    doc.insert(id(&schema, "a"), Value::String("t".repeat(300)));
    doc.insert_extra("alpha", json!("x"));
    doc.set_blob_ref(Some(
        BlobRef::new(
            BlobId::from_bytes([3; 32]),
            1 << 40,
            "a/b",
            BlobRef::MAX_CHUNK_SIZE,
            BTreeMap::new(),
        )
        .unwrap(),
    ));
    let bytes = doc.encode(&schema, FieldSet::All).unwrap();
    let rewrites = non_canonical_rewrites(&bytes);
    assert!(rewrites.len() > 300, "{}", rewrites.len());
    for (bad, offset, reason) in rewrites {
        assert_eq!(
            Document::decode(&bad, &schema),
            corrupt(offset, reason.clone()),
            "{reason:?} at {offset}"
        );
    }
}

/// Exhaustive single-bit flips: never a panic; an accepted result is canonical (SPEC §6.4.2).
#[test]
fn every_bit_flip_is_rejected_or_canonical() {
    let lax = all_types(false);
    let mut doc = Document::new("id-1")
        .with(
            id(&lax, "a"),
            Value::Strings(vec!["héllo".into(), String::new()]),
        )
        .with(id(&lax, "b"), Value::String("k".repeat(30)))
        .with(id(&lax, "c"), Value::I64(-300))
        .with(id(&lax, "d"), Value::F64(-2.5))
        .with(id(&lax, "e"), Value::Bool(true))
        .with(id(&lax, "f"), Value::Datetime(MAX_DATETIME))
        .with(
            id(&lax, "g"),
            Value::Json(json!({"k": [1.5, -1, u64::MAX, null, "s", {"n": {}}]})),
        );
    doc.insert_extra("Xtra", json!([true, -0.0]));
    doc.set_blob_ref(Some(
        BlobRef::new(BlobId::from_bytes([9; 32]), 99, "t/u", 8, BTreeMap::new()).unwrap(),
    ));
    let good = doc.encode(&lax, FieldSet::All).unwrap();
    let (mut accepted, mut rejected) = (0, 0);
    for i in 0..good.len() * 8 {
        let mut bad = good.clone();
        bad[i / 8] ^= 1 << (i % 8);
        match Document::decode(&bad, &lax) {
            Ok(back) => {
                accepted += 1;
                assert_eq!(back.encode(&lax, FieldSet::All).unwrap(), bad, "bit {i}");
            }
            Err(DecodeError::Corrupt { offset, .. }) => {
                rejected += 1;
                assert!(offset <= bad.len() as u64, "bit {i}: offset {offset}");
            }
            // Corrupt, or a blob reference version this build does not read (persia-format decides).
            Err(DecodeError::BlobRef(_)) => rejected += 1,
            Err(DecodeError::UnsupportedVersion { version }) => {
                rejected += 1;
                assert!(version > 1 && i / 8 == 1, "bit {i}: {version}");
            }
        }
    }
    // Flips in string content, blob ids and float mantissas give other valid documents.
    assert!(accepted > 0 && rejected > 300, "{accepted} / {rejected}");
}

/// Hostile nesting far past the limit is rejected at the 65th level, without exhausting the stack.
#[test]
fn hostile_nesting_on_decode() {
    let schema = all_types(false);
    let n = 1 << 20;
    // g (id 6): [[[[...0...]]]].
    let mut arrays = vec![0x85, 0x01, 0x61, b'1', 0xf6, 0xa1, 0x06];
    arrays.extend(std::iter::repeat_n(0x81, n));
    arrays.extend([0x00, 0xa0]);
    assert_eq!(
        Document::decode(&arrays, &schema),
        corrupt(7 + MAX_STORED_DEPTH as u64, DocumentCorruption::TooDeep)
    );
    // g: {"k": {"k": ...}}.
    let mut maps = vec![0x85, 0x01, 0x61, b'1', 0xf6, 0xa1, 0x06];
    for _ in 0..n / 3 {
        maps.extend([0xa1, 0x61, b'k']);
    }
    maps.extend([0x00, 0xa0]);
    assert_eq!(
        Document::decode(&maps, &schema),
        corrupt(7 + 3 * MAX_STORED_DEPTH as u64, DocumentCorruption::TooDeep)
    );
    // An extra field: same limit.
    let mut extra = vec![0x85, 0x01, 0x61, b'1', 0xf6, 0xa0, 0xa1, 0x61, b'X'];
    extra.extend(std::iter::repeat_n(0x81, n));
    extra.push(0x00);
    assert_eq!(
        Document::decode(&extra, &schema),
        corrupt(9 + MAX_STORED_DEPTH as u64, DocumentCorruption::TooDeep)
    );
    // Exactly the limit is fine.
    let mut ok = vec![0x85, 0x01, 0x61, b'1', 0xf6, 0xa0, 0xa1, 0x61, b'X'];
    ok.extend(std::iter::repeat_n(0x81, MAX_STORED_DEPTH));
    ok.push(0x00);
    assert!(Document::decode(&ok, &schema).is_ok());
}

/// Dismantles a deep value iteratively: `serde_json::Value` drops recursively.
fn drop_deep(mut v: serde_json::Value) {
    while let serde_json::Value::Array(mut items) = v {
        match items.pop() {
            Some(inner) => v = inner,
            None => break,
        }
    }
}

fn deep_array(depth: usize) -> serde_json::Value {
    (0..depth).fold(json!(0), |v, _| serde_json::Value::Array(vec![v]))
}

/// `validate` and `encode` of a typed document holding a value nested far past the limit (built in Rust, not
/// parsed) stop at the limit instead of recursing through it (SPEC §6.4.1).
#[test]
fn hostile_nesting_on_validate() {
    let lax = all_types(false);
    let g = id(&lax, "g");
    let mut doc = Document::new("1").with(g, Value::Json(deep_array(200_000)));
    let too_deep = DocumentError::TooDeep { field: "g".into() };
    assert_eq!(doc.validate(&lax), Err(too_deep.clone()));
    assert_eq!(doc.encode(&lax, FieldSet::All), Err(too_deep.clone()));
    assert_eq!(doc.encode(&lax, FieldSet::Stored), Err(too_deep));
    let Some(Value::Json(v)) = doc.remove(g) else {
        panic!()
    };
    drop_deep(v);
    doc.insert_extra("X", deep_array(200_000));
    assert_eq!(
        doc.validate(&lax),
        Err(DocumentError::TooDeep { field: "X".into() })
    );
    drop_deep(doc.insert_extra("X", json!(1)).unwrap());
}

/// Limits as the decoder enforces them (SPEC §6.4.1, "anything else is corruption").
#[test]
fn decoder_limits() {
    let schema = all_types(false);
    let b = id(&schema, "b");
    // id: 512 bytes fine, 513 corrupt; 0 corrupt.
    let max_id = Document::new("é".repeat(MAX_ID_LEN / 2))
        .encode(&schema, FieldSet::All)
        .unwrap();
    assert!(Document::decode(&max_id, &schema).is_ok());
    let mut long_id = vec![0x85, 0x01, 0x79, 0x02, 0x01];
    long_id.extend(std::iter::repeat_n(b'i', MAX_ID_LEN + 1));
    long_id.extend([0xf6, 0xa0, 0xa0]);
    assert_eq!(
        Document::decode(&long_id, &schema),
        corrupt(2, DocumentCorruption::Invalid { what: "id length" })
    );
    // A keyword at the limit, then one byte over, alone and in a list.
    let at_limit = Document::new("1")
        .with(b, Value::String("k".repeat(MAX_KEYWORD_LEN)))
        .encode(&schema, FieldSet::All)
        .unwrap();
    assert!(Document::decode(&at_limit, &schema).is_ok());
    let mut over = vec![
        0x85, 0x01, 0x61, b'1', 0xf6, 0xa1, 0x01, 0x82, 0x60, 0x79, 0x80, 0x01,
    ];
    over.extend(std::iter::repeat_n(b'k', MAX_KEYWORD_LEN + 1));
    over.extend([0xa0]);
    assert_eq!(
        Document::decode(&over, &schema),
        corrupt(
            9,
            DocumentCorruption::Invalid {
                what: "keyword length"
            }
        )
    );
    // A text value has no such limit.
    over[6] = 0x00;
    assert!(Document::decode(&over, &schema).is_ok());

    // 1024 fields in all decode; one more extra field is corrupt, at the extra field map.
    let doc = (0..schema.fields().len())
        .map(|i| (i, schema.fields()[i].field_type()))
        .fold(Document::new("1"), |d, (i, t)| {
            let v = match t {
                FieldType::Text | FieldType::Keyword => Value::String("s".into()),
                FieldType::I64 => Value::I64(1),
                FieldType::F64 => Value::F64(1.0),
                FieldType::Bool => Value::Bool(true),
                FieldType::Datetime => Value::Datetime(1),
                FieldType::Stored => Value::Json(json!(1)),
            };
            d.with(schema.fields()[i].id(), v)
        });
    let mut full = doc;
    let extras = MAX_DOCUMENT_FIELDS - schema.fields().len();
    for i in 0..extras {
        full.insert_extra(format!("x{i:04}"), json!(0));
    }
    let bytes = full.encode(&schema, FieldSet::All).unwrap();
    assert!(Document::decode(&bytes, &schema).is_ok());
    let head = [0xb9, 0x03, 0xf9]; // map(1017)
    assert_eq!(extras, 0x3f9);
    let map_at = bytes.windows(3).position(|w| w == head).unwrap();
    let mut over = bytes.clone();
    over[map_at + 2] = 0xfa;
    over.extend([0x61, b'y', 0x00]);
    assert_eq!(
        Document::decode(&over, &schema),
        corrupt(
            map_at as u64,
            DocumentCorruption::Invalid {
                what: "field count"
            }
        )
    );
    // Over 1024 schema field entries: corrupt at the field map, before reading any.
    let mut fields = vec![0x85, 0x01, 0x61, b'1', 0xf6, 0xb9, 0x04, 0x01];
    fields.extend(std::iter::repeat_n(0, 4096));
    assert_eq!(
        Document::decode(&fields, &schema),
        corrupt(
            5,
            DocumentCorruption::Invalid {
                what: "field count"
            }
        )
    );
    // Counts and lengths far beyond the input fail before anything is allocated for them.
    for head in [
        &[0xbb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff][..],
        &[0x9b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0x7b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    ] {
        let mut bytes = vec![0x85, 0x01, 0x61, b'1', 0xf6, 0xa1, 0x06];
        bytes.extend(head);
        let r = Document::decode(&bytes, &schema);
        assert!(
            matches!(
                r,
                Err(DecodeError::Corrupt {
                    reason: DocumentCorruption::UnexpectedEof,
                    ..
                })
            ),
            "{head:02x?}: {r:?}"
        );
    }
    // The whole encoding at exactly the limit decodes.
    let a = id(&schema, "a");
    let exact = Document::new("1").with(a, Value::String("t".repeat(MAX_DOCUMENT_LEN - 13)));
    let bytes = exact.encode(&schema, FieldSet::All).unwrap();
    assert_eq!(bytes.len(), MAX_DOCUMENT_LEN);
    assert_eq!(Document::decode(&bytes, &schema).unwrap(), exact);
    let over = Document::new("1").with(a, Value::String("t".repeat(MAX_DOCUMENT_LEN - 12)));
    assert_eq!(
        over.validate(&schema),
        Err(DocumentError::TooLarge {
            len: MAX_DOCUMENT_LEN + 1
        })
    );
}

/// Values only a corrupt encoding can hold.
#[test]
fn corrupt_values() {
    let schema = all_types(false);
    let doc = |field: u8, value: &[u8]| {
        let mut b = vec![0x85, 0x01, 0x61, b'1', 0xf6, 0xa1, field];
        b.extend(value);
        b.push(0xa0);
        b
    };
    let cases: Vec<(Vec<u8>, u64, DocumentCorruption)> = vec![
        // i64 field: -2^63 - 1 and 2^63.
        (
            doc(2, &[0x3b, 0x80, 0, 0, 0, 0, 0, 0, 0]),
            7,
            DocumentCorruption::OutOfRange { what: "i64" },
        ),
        (
            doc(2, &[0x1b, 0x80, 0, 0, 0, 0, 0, 0, 0]),
            7,
            DocumentCorruption::OutOfRange { what: "i64" },
        ),
        // f64 field: an integer, a negative zero, a negative NaN.
        (
            doc(3, &[0x00]),
            7,
            DocumentCorruption::Unexpected {
                expected: "a float",
                found: "an integer",
            },
        ),
        (
            doc(3, &[0xfb, 0xff, 0xf8, 0, 0, 0, 0, 0, 0]),
            7,
            DocumentCorruption::NonCanonicalFloat,
        ),
        (
            doc(3, &[0xfb, 0xff, 0xf0, 0, 0, 0, 0, 0, 0]),
            7,
            DocumentCorruption::NonCanonicalFloat,
        ),
        // datetime: one before the range.
        (
            doc(5, &[0x3b, 0x00, 0xdc, 0xdc, 0xc1, 0xa9, 0x17, 0x00, 0x00]),
            7,
            DocumentCorruption::OutOfRange { what: "datetime" },
        ),
        // stored: NaN and infinity inside, an integer key, a byte string, -2^64, a nested non-UTF-8 key.
        (
            doc(6, &[0x81, 0xfb, 0x7f, 0xf8, 0, 0, 0, 0, 0, 0]),
            8,
            DocumentCorruption::NonCanonicalFloat,
        ),
        (
            doc(6, &[0xfb, 0x7f, 0xf0, 0, 0, 0, 0, 0, 0]),
            7,
            DocumentCorruption::NonCanonicalFloat,
        ),
        (
            doc(6, &[0xa1, 0x00, 0x00]),
            8,
            DocumentCorruption::Unexpected {
                expected: "a text key",
                found: "an integer",
            },
        ),
        (
            doc(6, &[0x41, 0x00]),
            7,
            DocumentCorruption::Unexpected {
                expected: "a JSON value",
                found: "a byte string",
            },
        ),
        (
            doc(6, &[0x3b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            7,
            DocumentCorruption::OutOfRange {
                what: "stored integer",
            },
        ),
        (
            doc(6, &[0xa1, 0x61, 0xff, 0x00]),
            9,
            DocumentCorruption::InvalidUtf8,
        ),
        // text list: a non-string element.
        (
            doc(0, &[0x82, 0x60, 0xf6]),
            9,
            DocumentCorruption::Unexpected {
                expected: "a text string",
                found: "null",
            },
        ),
        // Stored map keys compare by bytes: "é" (c3 a9) before "z" is out of order.
        (
            doc(6, &[0xa2, 0x62, 0xc3, 0xa9, 0x00, 0x61, b'z', 0x00]),
            12,
            DocumentCorruption::KeyOrder,
        ),
    ];
    for (bytes, offset, reason) in cases {
        assert_eq!(
            Document::decode(&bytes, &schema),
            corrupt(offset, reason.clone()),
            "{bytes:02x?}"
        );
    }
    // Byte order, not char or case order: "Z" < "a" < "é" is fine.
    let ok = doc(
        6,
        &[
            0xa3, 0x61, b'Z', 0x00, 0x61, b'a', 0x00, 0x62, 0xc3, 0xa9, 0x00,
        ],
    );
    assert!(Document::decode(&ok, &schema).is_ok());
    // Extra field names out of byte order, repeated, and next to the schema field of the same name.
    let extras = |entries: &[u8]| {
        let mut b = vec![0x85, 0x01, 0x61, b'1', 0xf6, 0xa1, 0x02, 0x01];
        b.extend(entries);
        b
    };
    assert_eq!(
        Document::decode(
            &extras(&[0xa2, 0x61, b'x', 0x00, 0x61, b'X', 0x00]),
            &schema
        ),
        corrupt(12, DocumentCorruption::KeyOrder)
    );
    assert_eq!(
        Document::decode(
            &extras(&[0xa2, 0x61, b'X', 0x00, 0x61, b'X', 0x00]),
            &schema
        ),
        corrupt(12, DocumentCorruption::KeyOrder)
    );
    assert_eq!(
        Document::decode(&extras(&[0xa1, 0x61, b'c', 0x00]), &schema),
        corrupt(
            9,
            DocumentCorruption::Invalid {
                what: "extra field named like a field it has"
            }
        )
    );
    for bad_name in [&b""[..], b"_x", b"ID", b"bLoB", b"1x", b"x-y"] {
        let mut entry = vec![0xa1, 0x60 | u8::try_from(bad_name.len()).unwrap()];
        entry.extend(bad_name);
        entry.push(0x00);
        assert_eq!(
            Document::decode(&extras(&entry), &schema),
            corrupt(
                9,
                DocumentCorruption::Invalid {
                    what: "extra field name"
                }
            ),
            "{bad_name:?}"
        );
    }
}

/// What a row may hold and a new document may not (SPEC §6.4.2): it decodes and converts to JSON as it is, and
/// `encode` names the rule it breaks.
#[test]
fn rows_holding_what_new_documents_cannot() {
    let v1 = SchemaBuilder::new("e").strict(false).build().unwrap();
    let mut doc = Document::new("1");
    doc.insert_extra("color", json!(5));
    doc.insert_extra("size", json!("L"));
    let row = doc.encode(&v1, FieldSet::Stored).unwrap();
    // The schema adds `color` (keyword) and becomes strict.
    let v2 = v1
        .evolve(
            &SchemaBuilder::new("e")
                .field("color", FieldKind::Keyword(KeywordOptions::default()))
                .strict(true)
                .build()
                .unwrap(),
        )
        .unwrap();
    let old = Document::decode(&row, &v2).unwrap();
    assert_eq!(old, doc);
    assert_eq!(
        old.to_json(&v2).unwrap(),
        r#"{"id":"1","color":5,"size":"L"}"#
    );
    assert_eq!(
        old.encode(&v2, FieldSet::All),
        Err(DocumentError::ExtraIsSchemaField {
            name: "color".into()
        })
    );
    let mut only_size = Document::new("1");
    only_size.insert_extra("size", json!("L"));
    assert_eq!(
        only_size.encode(&v2, FieldSet::All),
        Err(DocumentError::UnknownField {
            name: "size".into()
        })
    );
    // Once the document has a `color` value, the old extra field next to it is corrupt in a row and an error in
    // JSON output.
    let both = old
        .clone()
        .with(id(&v2, "color"), Value::String("red".into()));
    assert_eq!(
        both.to_json(&v2),
        Err(DocumentError::ExtraIsSchemaField {
            name: "color".into()
        })
    );
}

// ---- Property tests ----

fn field_kind() -> impl Strategy<Value = FieldKind> {
    prop_oneof![
        any::<bool>().prop_map(|stored| FieldKind::Text(TextOptions {
            stored,
            ..TextOptions::default()
        })),
        any::<bool>().prop_map(|stored| FieldKind::Keyword(KeywordOptions {
            facet: false,
            stored
        })),
        any::<bool>().prop_map(|stored| FieldKind::I64 { stored }),
        any::<bool>().prop_map(|stored| FieldKind::F64 { stored }),
        any::<bool>().prop_map(|stored| FieldKind::Bool { stored }),
        any::<bool>().prop_map(|stored| FieldKind::Datetime { stored }),
        Just(FieldKind::Stored),
    ]
}

/// Up to 40 fields so some ids need a one-byte argument.
fn schema() -> impl Strategy<Value = CollectionSchema> {
    (
        prop::collection::btree_map("[a-z][a-z0-9_]{0,4}", field_kind(), 0..40),
        any::<bool>(),
    )
        .prop_filter("reserved names", |(fields, _)| {
            fields.keys().all(|n| n != "id" && n != "blob")
        })
        .prop_map(|(fields, strict)| {
            fields
                .into_iter()
                .fold(SchemaBuilder::new("p"), |b, (n, k)| b.field(n, k))
                .strict(strict)
                .build()
                .unwrap()
        })
}

fn json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        any::<u64>().prop_map(serde_json::Value::from),
        prop_oneof![
            any::<f64>().prop_filter("finite", |x| x.is_finite()),
            Just(-0.0),
            Just(5e-324)
        ]
        .prop_map(serde_json::Value::from),
        prop_oneof![".{0,4}", ".{20,30}"].prop_map(serde_json::Value::from),
    ];
    leaf.prop_recursive(4, 40, 30, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..30).prop_map(serde_json::Value::Array),
            prop::collection::btree_map(".{0,3}", inner, 0..26)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}

fn value(t: FieldType) -> BoxedStrategy<Value> {
    match t {
        FieldType::Text | FieldType::Keyword => prop_oneof![
            prop_oneof![".{0,3}", ".{23,26}"].prop_map(Value::String),
            prop::collection::vec(".{0,2}", 0..30).prop_map(Value::Strings),
        ]
        .boxed(),
        FieldType::I64 => prop_oneof![
            any::<i64>(),
            Just(i64::MIN),
            Just(i64::MAX),
            -300i64..300,
            Just(-65_537),
            Just(-4_294_967_297)
        ]
        .prop_map(Value::I64)
        .boxed(),
        FieldType::F64 => any::<f64>()
            .prop_filter("finite", |x| x.is_finite())
            .prop_map(Value::F64)
            .boxed(),
        FieldType::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
        FieldType::Datetime => prop_oneof![
            MIN_DATETIME..=MAX_DATETIME,
            Just(MIN_DATETIME),
            Just(MAX_DATETIME)
        ]
        .prop_map(Value::Datetime)
        .boxed(),
        FieldType::Stored => json_value()
            .prop_filter("not null", |v| !v.is_null())
            .prop_map(Value::Json)
            .boxed(),
    }
}

fn schema_and_doc() -> impl Strategy<Value = (CollectionSchema, Document)> {
    schema().prop_flat_map(|schema| {
        let fields: Vec<_> = schema
            .fields()
            .iter()
            .map(|f| (Just(f.id()), any::<bool>(), value(f.field_type())))
            .collect();
        let extra = prop::collection::btree_map(
            "[A-Z][A-Za-z0-9_]{0,3}",
            json_value().prop_filter("not null", |v| !v.is_null()),
            0..3,
        );
        let blob = prop::option::of(any::<[u8; 32]>());
        (Just(schema), "\\PC{1,30}", fields, extra, blob).prop_map(
            |(schema, doc_id, fields, extra, blob)| {
                let mut doc = Document::new(doc_id);
                for (field, present, value) in fields {
                    if present {
                        doc.insert(field, value);
                    }
                }
                if !schema.strict() {
                    for (name, value) in extra {
                        if !name.eq_ignore_ascii_case("id") && !name.eq_ignore_ascii_case("blob") {
                            doc.insert_extra(name, value);
                        }
                    }
                }
                doc.set_blob_ref(blob.map(|b| {
                    BlobRef::new(BlobId::from_bytes(b), 7, "a/b", 4, BTreeMap::new()).unwrap()
                }));
                (schema, doc)
            },
        )
    })
}

proptest! {
    // Each case checks every item, prefix or byte of an encoding; PROPTEST_CASES still overrides.
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(64),
        ..ProptestConfig::default()
    })]

    /// Invariant 1 over generated documents: every one-item rewrite into another encoding of the same data (or
    /// an item outside the subset) is corrupt at that item.
    #[test]
    fn exactly_one_encoding((schema, doc) in schema_and_doc()) {
        let bytes = doc.encode(&schema, FieldSet::All).unwrap();
        for (bad, offset, reason) in non_canonical_rewrites(&bytes) {
            prop_assert_eq!(Document::decode(&bad, &schema), corrupt(offset, reason));
        }
    }

    /// Invariant 3: every strict prefix ends early, cleanly, at or before the cut.
    #[test]
    fn every_truncation_is_unexpected_eof((schema, doc) in schema_and_doc(), stored in any::<bool>()) {
        let set = if stored { FieldSet::Stored } else { FieldSet::All };
        let bytes = doc.encode(&schema, set).unwrap();
        for cut in 0..bytes.len() {
            match Document::decode(&bytes[..cut], &schema) {
                Err(DecodeError::Corrupt { offset, reason: DocumentCorruption::UnexpectedEof }) => {
                    prop_assert!(offset <= cut as u64, "cut {}: offset {}", cut, offset);
                }
                other => prop_assert!(false, "cut {}: {:?}", cut, other),
            }
        }
    }

    /// Invariant 2 with damage aimed at heads: each byte replaced by each initial byte of interest.
    #[test]
    fn head_damage_is_rejected_or_canonical((schema, doc) in schema_and_doc(), at in any::<prop::sample::Index>()) {
        let bytes = doc.encode(&schema, FieldSet::All).unwrap();
        let i = at.index(bytes.len());
        for byte in [0x00, 0x17, 0x18, 0x1b, 0x20, 0x3b, 0x40, 0x60, 0x7f, 0x80, 0x9f, 0xa0, 0xbf, 0xc0,
                     0xf4, 0xf5, 0xf6, 0xf7, 0xf9, 0xfa, 0xfb, 0xff, bytes[i] ^ 0x20, bytes[i].wrapping_add(1)] {
            let mut bad = bytes.clone();
            bad[i] = byte;
            if let Ok(back) = Document::decode(&bad, &schema) {
                match back.encode(&schema, FieldSet::All) {
                    Ok(again) => prop_assert_eq!(again, bad),
                    Err(e) => prop_assert!(
                        matches!(e, DocumentError::ExtraIsSchemaField { .. } | DocumentError::UnknownField { .. }),
                        "{:?}", e
                    ),
                }
            }
        }
    }
}

/// The committed fuzz seeds mean what their names say: one canonical document, one nested too deep.
#[test]
fn fuzz_seeds_decode_as_intended() {
    let schema = all_types(false);
    let seed: &[u8] = include_bytes!("../../../fuzz/corpus/document/blob_extras_heads");
    let doc = Document::decode(seed, &schema).unwrap();
    assert!(doc.blob_ref().is_some() && doc.extra().len() == 2);
    assert_eq!(doc.encode(&schema, FieldSet::All).unwrap(), seed);
    let deep: &[u8] = include_bytes!("../../../fuzz/corpus/document/deep_stored");
    assert_eq!(
        Document::decode(deep, &schema),
        corrupt(7 + MAX_STORED_DEPTH as u64, DocumentCorruption::TooDeep)
    );
}
