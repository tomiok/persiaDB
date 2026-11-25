//! The request fingerprint a cursor records (SPEC §7.4, ROADMAP 10.1.24, ADR-0062 item 11 amended): a hash of the parts
//! of a request its ranking depends on that the cursor records nowhere else, so a token given back with another `q`,
//! `fields`, `filter`, `soft_filter` or limits is refused instead of paging another ranking.
//!
//! The hash is XXH3-64 of a canonical binary encoding (integers little-endian, `varint` the LEB128 of SPEC §4.1), not of
//! any `Debug` text, so it is the same for equal requests on every build of one version:
//!
//! ```text
//! varint  q's length, then its UTF-8 bytes
//! varint  number of default fields, then each: varint field id, u64 weight (the f64's bits)
//!         (`fields` as the search uses them: when left out, every `text` field with its schema boost)
//! u8      0 no filter | 1 then the filter                    (`filter`)
//! u8      0 no filter | 1 then the filter                    (`soft_filter`)
//! u64 ×5  max_terms, max_clauses, max_expansions, max_depth, max_text_len
//!
//! filter: u8 1 term (varint field, value) | 2 terms (varint field, varint count, values) |
//!         3 range (varint field, bound, bound) | 4 exists (varint field) | 5 and | 6 or (varint count, filters) |
//!         7 not (filter)
//! bound:  u8 0 unbounded | 1 included, value | 2 excluded, value
//! value:  u8 1 keyword (varint length, bytes) | 2 i64 (u64) | 3 f64 (u64 bits; -0.0 as 0.0, which it matches as) |
//!         4 bool (u8) | 5 datetime (u64)
//! ```
//!
//! What does not change the ranking is left out: the page (`offset`, `limit`), the facets, the projection, `explain`.
//! The sort, the proximity setting and the relaxation settings are recorded beside it, each on its own.

use std::ops::Bound;

use persia_format::Writer;
use persia_format::hash::xxh3_64;
use persia_format::widen::u64_from_usize;

use crate::query::{DefaultFields, Filter, FilterValue, QueryLimits};

/// The fingerprint of a request of `q` over `fields`, with `filter` and `soft_filter`, held to `limits`.
pub(super) fn fingerprint(
    q: &str,
    fields: &DefaultFields,
    (filter, soft_filter): (Option<&Filter>, Option<&Filter>),
    limits: &QueryLimits,
) -> u64 {
    let mut w = Writer::new();
    bytes(&mut w, q.as_bytes());
    w.write_varint_u64(u64_from_usize(fields.fields().len()));
    for &(field, weight) in fields.fields() {
        w.write_varint_u32(field.get());
        w.write_u64(weight.to_bits());
    }
    for filter in [filter, soft_filter] {
        match filter {
            None => w.write_u8(0),
            Some(filter) => {
                w.write_u8(1);
                write_filter(&mut w, filter);
            }
        }
    }
    for limit in [
        limits.max_terms,
        limits.max_clauses,
        limits.max_expansions,
        limits.max_depth,
        limits.max_text_len,
    ] {
        w.write_u64(u64_from_usize(limit));
    }
    xxh3_64(w.as_slice())
}

/// A filter, depth first. A request's filters were held to its limits' depth before (SPEC §7.1.3), so the recursion is
/// as deep as the validation's.
fn write_filter(w: &mut Writer, filter: &Filter) {
    match filter {
        Filter::Term { field, value } => {
            w.write_u8(1);
            w.write_varint_u32(field.get());
            write_value(w, value);
        }
        Filter::Terms { field, values } => {
            w.write_u8(2);
            w.write_varint_u32(field.get());
            w.write_varint_u64(u64_from_usize(values.len()));
            for value in values {
                write_value(w, value);
            }
        }
        Filter::Range {
            field,
            lower,
            upper,
        } => {
            w.write_u8(3);
            w.write_varint_u32(field.get());
            write_bound(w, lower.as_ref());
            write_bound(w, upper.as_ref());
        }
        Filter::Exists { field } => {
            w.write_u8(4);
            w.write_varint_u32(field.get());
        }
        Filter::And(filters) | Filter::Or(filters) => {
            w.write_u8(if matches!(filter, Filter::And(_)) {
                5
            } else {
                6
            });
            w.write_varint_u64(u64_from_usize(filters.len()));
            for filter in filters {
                write_filter(w, filter);
            }
        }
        Filter::Not(filter) => {
            w.write_u8(7);
            write_filter(w, filter);
        }
    }
}

fn write_bound(w: &mut Writer, bound: Bound<&FilterValue>) {
    match bound {
        Bound::Unbounded => w.write_u8(0),
        Bound::Included(value) => {
            w.write_u8(1);
            write_value(w, value);
        }
        Bound::Excluded(value) => {
            w.write_u8(2);
            write_value(w, value);
        }
    }
}

fn write_value(w: &mut Writer, value: &FilterValue) {
    match value {
        FilterValue::Keyword(s) => {
            w.write_u8(1);
            bytes(w, s.as_bytes());
        }
        FilterValue::I64(n) => {
            w.write_u8(2);
            w.write_u64(u64::from_ne_bytes(n.to_ne_bytes()));
        }
        FilterValue::F64(x) => {
            w.write_u8(3);
            // `-0.0` matches as `0.0` (SPEC §7.1.2): the same filter.
            let x = if *x == 0.0 { 0.0 } else { *x };
            w.write_u64(x.to_bits());
        }
        FilterValue::Bool(b) => {
            w.write_u8(4);
            w.write_u8(u8::from(*b));
        }
        FilterValue::Datetime(micros) => {
            w.write_u8(5);
            w.write_u64(u64::from_ne_bytes(micros.to_ne_bytes()));
        }
    }
}

fn bytes(w: &mut Writer, bytes: &[u8]) {
    w.write_varint_u64(u64_from_usize(bytes.len()));
    w.write_bytes(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldId, FieldKind, SchemaBuilder, TextOptions};

    fn schema() -> crate::schema::CollectionSchema {
        SchemaBuilder::new("c")
            .field("a", FieldKind::Text(TextOptions::default()))
            .field("b", FieldKind::Text(TextOptions::default()))
            .build()
            .unwrap()
    }

    fn fields() -> DefaultFields {
        DefaultFields::all(&schema())
    }

    fn term(field: u32, value: FilterValue) -> Filter {
        Filter::Term {
            field: FieldId::new(field),
            value,
        }
    }

    /// The fingerprint of this version, pinned: a change to the encoding changes every token's, and so takes a new
    /// token version (ADR-0062 item 11).
    #[test]
    fn golden_fingerprints() {
        let limits = QueryLimits::default();
        let f = fields();
        assert_eq!(
            fingerprint("", &f, (None, None), &limits),
            0x1b9a_449a_515d_315d
        );
        let filter = Filter::And(vec![
            term(1, FilterValue::Keyword("nike".into())),
            Filter::Not(Box::new(Filter::Exists {
                field: FieldId::new(2),
            })),
            Filter::Range {
                field: FieldId::new(3),
                lower: Bound::Included(FilterValue::F64(1.5)),
                upper: Bound::Excluded(FilterValue::I64(-7)),
            },
            Filter::Or(vec![Filter::Terms {
                field: FieldId::new(4),
                values: vec![FilterValue::Bool(true), FilterValue::Datetime(9)],
            }]),
        ]);
        assert_eq!(
            fingerprint("red shoes", &f, (Some(&filter), None), &limits),
            0x5bdf_80f0_cfe6_7be2
        );
    }

    /// Each part of the request changes the fingerprint; `-0.0` and `0.0` are one value.
    #[test]
    fn each_part_counts() {
        let limits = QueryLimits::default();
        let f = fields();
        let one =
            |q: &str, filter: Option<&Filter>, soft: Option<&Filter>, limits: &QueryLimits| {
                fingerprint(q, &f, (filter, soft), limits)
            };
        let base = one("red", None, None, &limits);
        assert_ne!(one("red ", None, None, &limits), base);
        assert_ne!(one("Red", None, None, &limits), base);
        let a = term(1, FilterValue::Keyword("a".into()));
        let b = term(1, FilterValue::Keyword("b".into()));
        // The same filter as `filter` or as `soft_filter` is another request.
        assert_ne!(one("red", Some(&a), None, &limits), base);
        assert_ne!(
            one("red", None, Some(&a), &limits),
            one("red", Some(&a), None, &limits)
        );
        assert_ne!(
            one("red", Some(&a), None, &limits),
            one("red", Some(&b), None, &limits)
        );
        let wider = QueryLimits {
            max_expansions: limits.max_expansions + 1,
            ..limits
        };
        assert_ne!(one("red", None, None, &wider), base);
        let schema = schema();
        for specs in [&["a"][..], &["a", "b^2"], &["b", "a"]] {
            let other = DefaultFields::parse(&schema, specs).unwrap();
            assert_ne!(
                fingerprint("red", &other, (None, None), &limits),
                base,
                "{specs:?}"
            );
        }
        // The default fields written out are the default.
        let written = DefaultFields::parse(&schema, &["a", "b"]).unwrap();
        assert_eq!(fingerprint("red", &written, (None, None), &limits), base);
        let zero = term(1, FilterValue::F64(0.0));
        let negative_zero = term(1, FilterValue::F64(-0.0));
        assert_eq!(
            one("", Some(&zero), None, &limits),
            one("", Some(&negative_zero), None, &limits)
        );
        // An `and` and an `or` of the same filters differ.
        let and = Filter::And(vec![a.clone(), b.clone()]);
        let or = Filter::Or(vec![a, b]);
        assert_ne!(
            one("", Some(&and), None, &limits),
            one("", Some(&or), None, &limits)
        );
    }
}
