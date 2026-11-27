//! Fuzz opening a database (ROADMAP 6.3; SPEC §4.5.1, §8.6, §8.7.1): container recovery, the manifest and its checks
//! against the superblock and the frames, segment opens and WAL replay, on one input, never panic. A database that
//! opens also commits what it replayed and opens again. The compaction-only open (ADR-0068) runs on a copy of the same
//! bytes too: an edit that makes a schema name an analyzer the built-ins lack has it skip that field and pre-scan the
//! WAL (ROADMAP 10.4.8), which never panics either; where the writer's open succeeds nothing is skipped, and it
//! succeeds too.
//!
//! The first input byte picks the mode. Even: the rest is the container file. Odd: the rest is a list of edits
//! (`u16` offset from the first frame, then a byte to XOR) applied to the golden database
//! (`tests/fixtures/database/v1.0/blob-ref.persia`), after which every frame's CRC is fixed, so edits reach the
//! manifest, the segment and the WAL chunk instead of stopping at a frame checksum.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::db::{CompactOnlyDatabase, Database, DatabaseOptions, ManualClock, SegmentIds};
use persia_format::checksum::crc32c;
use persia_storage::container::sim::SimFile;
use persia_storage::container::{Container, Mode};
use std::sync::Arc;

const GOLDEN: &[u8] = include_bytes!("../../tests/fixtures/database/v1.0/blob-ref.persia");
const FIRST_FRAME: usize = 12_288;

fn options() -> DatabaseOptions {
    DatabaseOptions {
        clock: Arc::new(ManualClock::new(0)),
        segment_ids: SegmentIds::seeded(1),
        ..DatabaseOptions::default()
    }
}

/// Recomputes the CRC of every frame from the first one on, as far as their headers chain (SPEC §4.4).
fn fix_frame_crcs(file: &mut [u8]) {
    let mut at = FIRST_FRAME;
    while let Some(header) = file.get(at..at + 24) {
        let payload_len = u64::from_le_bytes(header[8..16].try_into().unwrap_or_default());
        let Some(padded) = usize::try_from(payload_len)
            .ok()
            .and_then(|n| n.checked_next_multiple_of(8))
        else {
            return;
        };
        let Some(crc_at) = at.checked_add(24).and_then(|n| n.checked_add(padded)) else {
            return;
        };
        let (Some(covered), true) = (file.get(at + 4..crc_at), crc_at + 8 <= file.len()) else {
            return;
        };
        let crc = crc32c(covered);
        file[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
        at = crc_at + 8;
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let bytes = if mode % 2 == 0 {
        rest.to_vec()
    } else {
        let mut file = GOLDEN.to_vec();
        for edit in rest.as_chunks::<3>().0 {
            let offset = FIRST_FRAME + usize::from(u16::from_le_bytes([edit[0], edit[1]]));
            if let Some(b) = file.get_mut(offset) {
                *b ^= edit[2];
            }
        }
        fix_frame_crcs(&mut file);
        file
    };
    // The compaction-only open's error, if any, in debug form.
    let compact_only = match Container::open_on(SimFile::from_bytes(bytes.clone()), Mode::ReadWrite)
    {
        Ok(container) => CompactOnlyDatabase::open_container(container, options())
            .err()
            .map(|e| format!("{e:?}")),
        Err(e) => Some(format!("{e:?}")),
    };
    let file = SimFile::from_bytes(bytes);
    let Ok(mut db) = Database::open_on(file.clone(), options()) else {
        return;
    };
    assert!(
        compact_only.is_none(),
        "the writer's open succeeded, the compaction-only open did not: {compact_only:?}"
    );
    let snapshot = db.snapshot();
    for (_, collection) in snapshot.collections() {
        for segment in collection.segments() {
            for docid in 0..segment.doc_count() {
                let _ = segment.document(docid);
            }
        }
    }
    if db.commit().is_ok() {
        drop(db);
        Database::open_on(file, options()).expect("a database that committed opens again");
    }
});
