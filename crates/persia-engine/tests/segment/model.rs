//! A naive model of what a segment holds, computed straight from the documents (SPEC §5.3).

use std::collections::BTreeMap;

use persia_analysis::{Analyzer, TokenStream};
use persia_engine::document::{Document, Value};
use persia_engine::schema::FieldId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub doc: u32,
    pub tf: u32,
    /// Empty when the field has no positions.
    pub positions: Vec<u32>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Index {
    pub terms: BTreeMap<Vec<u8>, Vec<Occurrence>>,
}

/// The inverted index of one field, and each document's field length (only those above 0).
pub fn index(
    docs: &[Document],
    field: FieldId,
    analyzer: Option<&Analyzer>,
    positions: bool,
) -> (Index, BTreeMap<u32, u32>) {
    let mut terms: BTreeMap<Vec<u8>, BTreeMap<u32, Vec<u32>>> = BTreeMap::new();
    let mut lengths = BTreeMap::new();
    for (doc, d) in (0_u32..).zip(docs) {
        let values: Vec<&str> = match d.get(field) {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Strings(l)) => l.iter().map(String::as_str).collect(),
            _ => continue,
        };
        let mut len = 0;
        let mut base = 0;
        for (i, value) in (0_u32..).zip(&values) {
            match analyzer {
                Some(a) => {
                    let mut ts = TokenStream::new();
                    a.analyze(value, &mut ts);
                    let mut end = 0;
                    for t in ts.iter() {
                        terms
                            .entry(t.text.as_bytes().to_vec())
                            .or_default()
                            .entry(doc)
                            .or_default()
                            .push(base + t.position);
                        end = end.max(t.position + 1);
                        len += 1;
                    }
                    base += end + 100;
                }
                None => {
                    terms
                        .entry(value.as_bytes().to_vec())
                        .or_default()
                        .entry(doc)
                        .or_default()
                        .push(i);
                    len += 1;
                }
            }
        }
        if len > 0 {
            lengths.insert(doc, len);
        }
    }
    let terms = terms
        .into_iter()
        .map(|(term, docs)| {
            let list = docs
                .into_iter()
                .map(|(doc, mut p)| {
                    p.sort_unstable();
                    let tf = u32::try_from(p.len()).unwrap();
                    Occurrence {
                        doc,
                        tf,
                        positions: if positions { p } else { Vec::new() },
                    }
                })
                .collect();
            (term, list)
        })
        .collect();
    (Index { terms }, lengths)
}

/// Lucene's `SmallFloat.intToByte4`, the norms quantization SPEC §5.3.4 adopts.
pub fn norm(len: u32) -> u8 {
    let len = len.min(i32::MAX as u32);
    if len < 24 {
        return len as u8;
    }
    let x = len - 24;
    let bits = 32 - x.leading_zeros();
    let int4 = if bits < 4 {
        x
    } else {
        let shift = bits - 4;
        ((x >> shift) & 7) | ((shift + 1) << 3)
    };
    (24 + int4) as u8
}
