//! Frame kinds 6 (`BlobChunk`) and 7 (`BlobIndex`) belong to the blob pack (SPEC §4.4, §9.3; ADR-0049 item 4). A
//! container holding them, which no correct writer produces, opens and recovers exactly as one holding an unknown
//! kind: the frames are listed and kept, committed or not, and nothing else changes.
#![allow(clippy::unwrap_used, clippy::panic, missing_docs)] // Tests: inputs are fixed.

use persia_format::block::Compression;
use persia_format::frame::{FrameKind, ScanEnd};
use persia_storage::container::{Commit, Container, Mode};

fn kinds_after_reopen(extra: FrameKind) -> (Vec<FrameKind>, Vec<FrameKind>, ScanEnd) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let mut c = Container::create(&path).unwrap();
    let wal = c
        .append(FrameKind::WalChunk, b"put", Compression::None)
        .unwrap();
    c.append(extra, b"not a container's", Compression::None)
        .unwrap();
    c.sync().unwrap();
    let manifest = c
        .append(FrameKind::Manifest, b"m", Compression::None)
        .unwrap();
    c.commit(Commit {
        manifest,
        wal_start: wal.offset,
        fence_token: 0,
    })
    .unwrap();
    c.append(extra, b"after log_end", Compression::None)
        .unwrap();
    c.sync().unwrap();
    drop(c);
    let rw = Container::open(&path, Mode::ReadWrite).unwrap();
    let ro = Container::open(&path, Mode::ReadOnly).unwrap();
    let kinds = |c: &Container| c.frames().iter().map(|f| f.kind).collect::<Vec<_>>();
    (kinds(&rw), kinds(&ro), rw.recovery().scan_end.clone())
}

#[test]
fn pack_kinds_in_a_container_behave_like_unknown_kinds() {
    let (writer_kinds, reader_kinds, scan_end) = kinds_after_reopen(FrameKind::from_u8(200));
    for kind in [FrameKind::BlobChunk, FrameKind::BlobIndex] {
        let (writer, reader, end) = kinds_after_reopen(kind);
        let as_unknown = |v: &[FrameKind]| {
            v.iter()
                .map(|&k| {
                    if k == kind {
                        FrameKind::from_u8(200)
                    } else {
                        k
                    }
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(as_unknown(&writer), writer_kinds, "{kind:?}");
        assert_eq!(as_unknown(&reader), reader_kinds, "{kind:?}");
        assert_eq!(end, scan_end, "{kind:?}");
    }
    assert_eq!(
        writer_kinds,
        [
            FrameKind::WalChunk,
            FrameKind::from_u8(200),
            FrameKind::Manifest,
            FrameKind::from_u8(200)
        ]
    );
}
