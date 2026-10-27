//! The multi-block golden segment (ROADMAP 5.5.2): `tests/fixtures/segment/v1.0/multiblock.pseg`, frozen like
//! `basic.pseg` (SPEC §15). `basic.pseg` holds every section in one 16 KiB block; this one pins what only many blocks
//! show: 512-byte blocks, so streams, postings lists, positions runs and `DocStore` rows span blocks; 1100 documents,
//! so numeric columns and keyword starts hold two FOR blocks; lists of hundreds of documents with many `Skip` entries.
//! It reads with `basic.schema.json`.

use persia_engine::document::{Document, Value};
use persia_engine::schema::CollectionSchema;
use persia_engine::segment::WriteOptions;

use super::{TAGS, WORDS, assert_same_content, blob, build, check, fixture_path, id, schema};

const OPTIONS: WriteOptions = WriteOptions {
    block_size: 512,
    zstd_level: 3,
};

const DOCS: u32 = 1100;

fn docs(schema: &CollectionSchema) -> Vec<Document> {
    (0..DOCS)
        .map(|i| {
            let w = |k: u32| WORDS[((i * 3 + k) % WORDS.len() as u32) as usize];
            // Ids out of docid order, so the IdMap is not the identity.
            let mut d = Document::new(format!("m-{:04}", (i * 389) % DOCS));
            if i % 4 != 3 {
                // Repeated words: term frequencies above 1, positions runs of 128+ values per block.
                let title =
                    format!("{} {} {} the {}", w(0), w(1), w(0), w(2)).repeat(1 + (i % 3) as usize);
                d.insert(id(schema, "title"), Value::String(title));
            }
            d.insert(
                id(schema, "body"),
                Value::String(format!("running runs {}", WORDS[(i % 5) as usize])),
            );
            if i % 6 != 0 {
                let mut tags = vec![TAGS[(i % 5) as usize].to_owned()];
                if i % 2 == 0 {
                    tags.push("red".to_owned());
                }
                d.insert(id(schema, "tag"), Value::Strings(tags));
            }
            if i % 3 != 1 {
                d.insert(id(schema, "price"), Value::F64(f64::from(i) * 0.25 - 50.0));
            }
            d.insert(
                id(schema, "qty"),
                Value::I64(i64::from(i) * 1_000 - 500_000),
            );
            if i % 5 != 0 {
                d.insert(
                    id(schema, "at"),
                    Value::Datetime(1_700_000_000_000_000 + i64::from(i) * 60_000_000),
                );
            }
            if i % 4 == 0 {
                d.insert(id(schema, "flag"), Value::Bool(i % 8 == 0));
            }
            d.insert(
                id(schema, "raw"),
                Value::Json(serde_json::json!({ "i": i })),
            );
            if i % 97 == 5 {
                d.set_blob_ref(Some(blob(u8::try_from(i % 256).unwrap())));
            }
            d
        })
        .collect()
}

#[test]
fn golden_multiblock_segment() {
    let schema = schema();
    let docs = docs(&schema);
    let bytes = build(&schema, &docs, OPTIONS);
    check(&schema, &docs, &bytes);
    let path = fixture_path().with_file_name("multiblock.pseg");
    if std::env::var_os("PERSIA_WRITE_SEGMENT_FIXTURES").is_some() && !path.exists() {
        std::fs::write(&path, &bytes).unwrap();
    }
    let stored = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (PERSIA_WRITE_SEGMENT_FIXTURES=1 writes it)",
            path.display()
        )
    });
    // As for basic.pseg: the stored file reads as its documents, and today's writer produces the same content.
    check(&schema, &docs, &stored);
    assert_same_content(&bytes, &stored);
    super::read::check_reads(&schema, &docs, &stored);

    // It pins what it is for: many blocks per section, two FOR blocks, several Skip entries per long list.
    let f = super::decode::parse(&stored);
    let blocks = |kind: u16, field: u32| {
        persia_format::block::BlockReader::open(&f.sections[&(kind, field)], 0)
            .unwrap()
            .block_count()
    };
    let title = id(&schema, "title").get();
    let qty = id(&schema, "qty").get();
    assert!(
        blocks(3, title) > 1 && blocks(6, title) > 1,
        "title postings and positions span blocks"
    );
    assert!(
        blocks(9, super::decode::SEGMENT_FIELD) > 10,
        "DocStore over many blocks"
    );
    assert!(blocks(8, qty) > 1, "qty column over blocks");
    assert!(f.sections.contains_key(&(5, title)), "Skip entries");
    assert!(
        stored.len() < 1 << 20,
        "fixtures stay under 1 MiB (CLAUDE.md)"
    );
}
