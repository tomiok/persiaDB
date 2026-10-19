//! Fuzz the strict JSON collection definition (ROADMAP 3.1.2.1; SPEC §6.2, ADR-0022): arbitrary input never panics,
//! and an accepted schema serializes to JSON that parses back to the same schema with the same hash, and evolves to
//! itself.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::schema::CollectionSchema;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(schema) = CollectionSchema::from_json(text) else {
        return;
    };
    let json = schema.to_json().expect("a valid schema serializes");
    let back = CollectionSchema::from_json(&json).expect("serialized schema parses");
    assert_eq!(back, schema);
    assert_eq!(back.hash(), schema.hash());
    assert_eq!(schema.evolve(&schema).as_ref(), Ok(&schema));
});
