//! Fuzz the manifest encoding (ROADMAP 3.3.4; SPEC §8.7, ADR-0024): arbitrary bytes never panic, and an accepted
//! manifest survives encode → decode unchanged, re-encoding to the same bytes the second time. The first encoding
//! may differ from the input only in the schema text, which `decode` accepts in any valid form (SPEC §8.7).
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::manifest::{Manifest, ManifestEncodeError};

fuzz_target!(|data: &[u8]| {
    let Ok(manifest) = Manifest::decode(data) else {
        return;
    };
    let bytes = match manifest.encode() {
        Ok(bytes) => bytes,
        // The engine's schema JSON may be longer than the stored form (`1` is written `1.0`), so a manifest near the
        // limit can decode but not re-encode. The writer then fails the commit; nothing is lost.
        Err(ManifestEncodeError::TooLarge { .. }) => return,
        Err(e) => panic!("a decoded manifest fails to encode: {e}"),
    };
    let again = Manifest::decode(&bytes).expect("an encoded manifest decodes");
    assert_eq!(again, manifest);
    assert_eq!(again.encode().expect("encodes"), bytes);
});
