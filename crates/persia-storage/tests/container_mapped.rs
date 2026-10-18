//! Mapped reads of committed container frames (ROADMAP 2.3.6; SPEC §4.7, ADR-0017): which frames come from the
//! mapping, remapping after commits and compaction, and that a mapping never stops a reader seeing the right bytes.
//!
//! A frame read from the mapping is a slice of it, so two reads of the same frame share their bytes (`as_ptr`
//! equal); a frame read with `pread` gets a fresh buffer each time.

// Test-only code: unwraps, indexing and casts are the point.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::single_range_in_vec_init // a list of one live frame is the point
)]

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use persia_format::Writer;
use persia_format::block::Compression;
use persia_format::frame::{FRAME_HEADER_LEN, FrameKind, ScanEnd, encode_frame};
use persia_format::superblock::FIRST_FRAME_OFFSET;
use persia_storage::container::{Commit, Container, Error, FrameRef, Mode};
use proptest::prelude::*;

const MAX: usize = 1 << 20;
const NONE: Compression = Compression::None;
const ZSTD: Compression = Compression::Zstd { level: 3 };
const F: u64 = FIRST_FRAME_OFFSET;

fn path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("app.persia")
}

/// Appends `wal` as WAL frames and a Manifest, and commits them; returns the frames in order.
fn commit_batch(c: &mut Container, wal: &[&[u8]], manifest: &[u8]) -> Vec<FrameRef> {
    let mut frames: Vec<FrameRef> = wal
        .iter()
        .map(|p| c.append(FrameKind::WalChunk, p, NONE).unwrap())
        .collect();
    c.sync().unwrap();
    let m = c.append(FrameKind::Manifest, manifest, NONE).unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: c.superblock().wal_start,
        fence_token: 0,
    })
    .unwrap();
    frames.push(m);
    frames
}

/// Whether the frame at `offset` comes from the mapping: two reads share their bytes. Needs a non-empty payload.
fn is_mapped(c: &Container, offset: u64) -> bool {
    let a = c.read_frame_source(offset, MAX).unwrap().payload;
    let b = c.read_frame_source(offset, MAX).unwrap().payload;
    assert_eq!(a, b);
    a.as_ptr() == b.as_ptr()
}

fn payload(c: &Container, offset: u64) -> Vec<u8> {
    let source = c.read_frame_source(offset, MAX).unwrap();
    let read = c.read_frame(offset, MAX).unwrap();
    assert_eq!(
        (source.frame, &*source.payload),
        (read.frame, &*read.payload)
    );
    read.payload
}

/// A container with one committed batch, reopened read-only.
fn committed(path: &Path) -> (Container, Vec<FrameRef>) {
    let mut w = Container::create(path).unwrap();
    let frames = commit_batch(&mut w, &[b"wal one", b"wal two"], b"manifest 1");
    drop(w);
    (Container::open(path, Mode::ReadOnly).unwrap(), frames)
}

#[test]
fn off_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let (c, frames) = committed(&path(&dir));
    assert_eq!(c.mapped_range(), None);
    for f in &frames {
        assert!(!is_mapped(&c, f.offset));
    }
}

#[test]
fn committed_frames_come_from_the_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, frames) = committed(&path(&dir));
    c.enable_mapped_reads().unwrap();
    assert_eq!(c.mapped_range(), Some(F..c.superblock().log_end));
    for (f, want) in frames
        .iter()
        .zip([&b"wal one"[..], b"wal two", b"manifest 1"])
    {
        assert!(is_mapped(&c, f.offset));
        assert_eq!(payload(&c, f.offset), want);
        let source = c.read_frame_source(f.offset, MAX).unwrap();
        assert_eq!(source.frame, *f);
        assert_eq!(
            source.payload.base_offset(),
            f.offset + FRAME_HEADER_LEN as u64
        );
    }
}

#[test]
fn a_mapped_read_still_validates_the_frame() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, frames) = committed(&path(&dir));
    c.enable_mapped_reads().unwrap();
    // The bomb limit applies to mapped payloads too.
    assert!(matches!(
        c.read_frame_source(frames[0].offset, 3),
        Err(Error::Format(_))
    ));
    // An offset inside a frame is not a frame.
    assert!(c.read_frame_source(frames[0].offset + 8, MAX).is_err());
    assert!(matches!(
        c.read_frame_source(F - 8, MAX),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn frames_after_log_end_are_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let mut w = Container::create(&path).unwrap();
    commit_batch(&mut w, &[b"committed"], b"manifest 1");
    w.enable_mapped_reads().unwrap();
    let log_end = w.superblock().log_end;
    // Synced WAL frames after the last commit (SPEC §8.2): never mapped, since recovery may truncate there.
    let wal = w
        .append(FrameKind::WalChunk, b"not yet committed", NONE)
        .unwrap();
    w.sync().unwrap();
    assert_eq!(w.mapped_range(), Some(F..log_end));
    assert!(!is_mapped(&w, wal.offset));
    assert_eq!(payload(&w, wal.offset), b"not yet committed");

    // A read-only open sees it after `log_end` too.
    let mut r = Container::open(&path, Mode::ReadOnly).unwrap();
    r.enable_mapped_reads().unwrap();
    assert_eq!(r.uncommitted_frames(), &[wal]);
    assert!(!is_mapped(&r, wal.offset));
    assert_eq!(payload(&r, wal.offset), b"not yet committed");
}

#[test]
fn a_commit_remaps() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Container::create(path(&dir)).unwrap();
    w.enable_mapped_reads().unwrap();
    // A new container has no frames: an empty mapping.
    assert_eq!(w.mapped_range(), Some(F..F));
    let first = commit_batch(&mut w, &[b"a"], b"manifest 1");
    assert_eq!(w.mapped_range(), Some(F..w.superblock().log_end));
    let second = commit_batch(&mut w, &[b"b"], b"manifest 2");
    assert_eq!(w.mapped_range(), Some(F..w.superblock().log_end));
    for (f, want) in
        first
            .iter()
            .chain(&second)
            .zip([&b"a"[..], b"manifest 1", b"b", b"manifest 2"])
    {
        assert!(is_mapped(&w, f.offset));
        assert_eq!(payload(&w, f.offset), want);
    }
}

#[test]
fn compressed_payloads_are_decompressed_at_offset_zero() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Container::create(path(&dir)).unwrap();
    w.enable_mapped_reads().unwrap();
    let data = vec![42_u8; 10_000];
    let wal = w.append(FrameKind::WalChunk, &data, ZSTD).unwrap();
    w.sync().unwrap();
    let m = w.append(FrameKind::Manifest, b"m", NONE).unwrap();
    w.commit(Commit {
        manifest: m,
        wal_start: wal.offset,
        fence_token: 0,
    })
    .unwrap();
    let source = w.read_frame_source(wal.offset, MAX).unwrap();
    assert_eq!(&*source.payload, &data[..]);
    assert_eq!(source.payload.base_offset(), 0);
    assert!(wal.len < 1000, "the frame is stored compressed");
    // The bomb limit applies to the decompressed size.
    assert!(w.read_frame_source(wal.offset, data.len() - 1).is_err());
}

#[test]
fn an_in_place_compaction_maps_the_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let mut w = Container::create(&path).unwrap();
    let seg = w.append(FrameKind::Segment, b"live segment", NONE).unwrap();
    w.append(FrameKind::Segment, b"dead segment", NONE).unwrap();
    let wal = w.append(FrameKind::WalChunk, b"wal", NONE).unwrap();
    w.sync().unwrap();
    let m = w.append(FrameKind::Manifest, b"manifest 2", NONE).unwrap();
    // The segments lie below `wal_start`, where compaction keeps only the live ones.
    w.commit(Commit {
        manifest: m,
        wal_start: wal.offset,
        fence_token: 0,
    })
    .unwrap();
    w.enable_mapped_reads().unwrap();

    // A reader of the old file, mapped too: compaction must not change what it reads (SPEC §4.6).
    let mut old = Container::open(&path, Mode::ReadOnly).unwrap();
    old.enable_mapped_reads().unwrap();
    let old_range = old.mapped_range();

    let relocation = w
        .compact(&[seg.offset..seg.end()], |_| b"manifest 3".to_vec(), NONE)
        .unwrap();
    let new_seg = relocation.get(seg.offset).unwrap();
    let new_wal = relocation.get(wal.offset).unwrap();
    assert_eq!(w.mapped_range(), Some(F..w.superblock().log_end));
    for (f, want) in [(new_seg, &b"live segment"[..]), (new_wal, b"wal")] {
        assert!(is_mapped(&w, f.offset));
        assert_eq!(payload(&w, f.offset), want);
    }
    assert_eq!(payload(&w, w.superblock().manifest_off), b"manifest 3");

    assert_eq!(old.mapped_range(), old_range);
    assert!(is_mapped(&old, seg.offset));
    assert_eq!(payload(&old, seg.offset), b"live segment");
    assert_eq!(payload(&old, old.superblock().manifest_off), b"manifest 2");
}

#[test]
fn a_mapping_survives_appends_and_commits_of_another_handle() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let (mut r, frames) = committed(&path);
    r.enable_mapped_reads().unwrap();
    let mut w = Container::open(&path, Mode::ReadWrite).unwrap();
    for i in 0..20 {
        let wal = format!("wal {i}");
        commit_batch(&mut w, &[wal.as_bytes(), &[9; 5000]], b"manifest n");
    }
    assert_eq!(r.mapped_range(), Some(F..frames.last().unwrap().end()));
    assert_eq!(payload(&r, frames[1].offset), b"wal two");
    assert_eq!(payload(&r, r.superblock().manifest_off), b"manifest 1");
}

#[test]
fn a_mapping_and_a_writer_truncating_a_torn_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let (mut r, frames) = committed(&path);
    r.enable_mapped_reads().unwrap();
    // Half of the next frame: a writer that died mid-append (SPEC §4.4).
    let mut torn = Writer::new();
    encode_frame(&mut torn, FrameKind::WalChunk, 4, &[1; 300], NONE).unwrap();
    let torn = torn.into_inner();
    let len_before = std::fs::metadata(&path).unwrap().len();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&torn[..torn.len() / 2])
        .unwrap();

    let opened = Container::open(&path, Mode::ReadWrite);
    // Windows keeps a mapped file from being truncated (ADR-0017): that open fails, and harms nothing.
    if cfg!(windows) && matches!(opened, Err(Error::Io { .. })) {
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            len_before + torn.len() as u64 / 2
        );
    } else {
        let w = opened.unwrap();
        assert!(matches!(w.recovery().scan_end, ScanEnd::TornTail { .. }));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), len_before);
    }
    // The truncation lies after `log_end`, past the mapping: the reader is unaffected.
    for (f, want) in frames
        .iter()
        .zip([&b"wal one"[..], b"wal two", b"manifest 1"])
    {
        assert_eq!(payload(&r, f.offset), want);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Differential: a mapped and an unmapped handle read every frame identically, whatever was committed.
    #[test]
    fn mapped_and_read_frames_agree(
        batches in proptest::collection::vec(
            proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..2000), 0..4),
            1..5,
        ),
        tail in proptest::collection::vec(any::<u8>(), 0..500),
        compress in any::<bool>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = path(&dir);
        let mut w = Container::create(&path).unwrap();
        w.enable_mapped_reads().unwrap();
        let compression = if compress { ZSTD } else { NONE };
        let mut all = Vec::new();
        for batch in &batches {
            for p in batch {
                all.push(w.append(FrameKind::Segment, p, compression).unwrap());
            }
            all.extend(commit_batch(&mut w, &[], b"manifest"));
        }
        all.push(w.append(FrameKind::WalChunk, &tail, compression).unwrap());
        w.sync().unwrap();

        let plain = Container::open(&path, Mode::ReadOnly).unwrap();
        let mut mapped = Container::open(&path, Mode::ReadOnly).unwrap();
        mapped.enable_mapped_reads().unwrap();
        let log_end = w.superblock().log_end;
        for f in &all {
            let want = plain.read_frame(f.offset, MAX).unwrap();
            for c in [&w, &mapped] {
                let got = c.read_frame_source(f.offset, MAX).unwrap();
                prop_assert_eq!(got.frame, want.frame);
                prop_assert_eq!(&*got.payload, &want.payload[..]);
                prop_assert_eq!(c.read_frame(f.offset, MAX).unwrap(), want.clone());
            }
            if !compress && !want.payload.is_empty() {
                prop_assert_eq!(is_mapped(&mapped, f.offset), f.end() <= log_end);
            }
        }
    }
}

#[test]
fn a_corrupt_segment_below_wal_start_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let mut w = Container::create(&path).unwrap();
    let seg = w.append(FrameKind::Segment, &[5; 4000], NONE).unwrap();
    w.sync().unwrap();
    let m = w.append(FrameKind::Manifest, b"m", NONE).unwrap();
    w.commit(Commit {
        manifest: m,
        wal_start: m.end(),
        fence_token: 0,
    })
    .unwrap();
    drop(w);
    // Opens do not validate frames below `wal_start`; a mapped read must, like a read does.
    let f = OpenOptions::new().write(true).open(&path).unwrap();
    persia_storage::container::ContainerFile::write_all_at(&f, &[6], seg.offset + 1000).unwrap();
    drop(f);
    let mut c = Container::open(&path, Mode::ReadOnly).unwrap();
    c.enable_mapped_reads().unwrap();
    assert_eq!(c.mapped_range(), Some(F..m.end()));
    assert!(matches!(
        c.read_frame_source(seg.offset, MAX),
        Err(Error::Format(_))
    ));
    assert!(matches!(
        c.read_frame(seg.offset, MAX),
        Err(Error::Format(_))
    ));
    assert_eq!(payload(&c, m.offset), b"m");
}

#[test]
fn a_compaction_that_fails_keeps_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Container::create(path(&dir)).unwrap();
    let frames = commit_batch(&mut w, &[b"wal"], b"manifest 1");
    w.enable_mapped_reads().unwrap();
    let range = w.mapped_range();
    // Not a frame below `wal_start`: refused before anything is written.
    let err = w
        .compact(&[F + 1..F + 2], |_| b"manifest 2".to_vec(), NONE)
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    assert_eq!(w.mapped_range(), range);
    assert!(is_mapped(&w, frames[0].offset));
    assert_eq!(payload(&w, frames[0].offset), b"wal");
}

#[test]
fn enabling_twice_maps_anew() {
    let dir = tempfile::tempdir().unwrap();
    let path = path(&dir);
    let (mut r, frames) = committed(&path);
    r.enable_mapped_reads().unwrap();
    let first = r.read_frame_source(frames[0].offset, MAX).unwrap().payload;
    r.enable_mapped_reads().unwrap();
    assert_eq!(r.mapped_range(), Some(F..r.superblock().log_end));
    let second = r.read_frame_source(frames[0].offset, MAX).unwrap().payload;
    // A payload from the old mapping stays valid after it was replaced.
    assert_eq!(first, second);
    assert!(is_mapped(&r, frames[0].offset));
}

/// The automatic choice (ROADMAP 2.3.10, ADR-0020): a container in the temporary directory, a local disk on every
/// CI platform, and on Linux one on tmpfs, are mapped; reads then come from the mapping.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn a_container_on_a_local_disk_is_mapped_automatically() {
    let tmp = tempfile::tempdir().unwrap();
    let mut dirs = vec![tmp.path().to_owned()];
    let shm = if cfg!(target_os = "linux") {
        tempfile::tempdir_in("/dev/shm").ok()
    } else {
        None
    };
    dirs.extend(shm.iter().map(|d| d.path().to_owned()));
    for dir in dirs {
        let path = dir.join("app.persia");
        let mut writer = Container::create(&path).unwrap();
        let m = writer
            .append(FrameKind::Manifest, b"m", Compression::None)
            .unwrap();
        writer
            .commit(Commit {
                manifest: m,
                wal_start: m.end(),
                fence_token: 0,
            })
            .unwrap();
        drop(writer);
        let mut c = Container::open(&path, Mode::ReadOnly).unwrap();
        assert!(c.is_on_local_disk().unwrap(), "{}", dir.display());
        assert!(
            c.enable_mapped_reads_if_local().unwrap(),
            "{}",
            dir.display()
        );
        assert_eq!(c.mapped_range(), Some(FIRST_FRAME_OFFSET..m.end()));
        let first = c.read_frame_source(m.offset, 1 << 10).unwrap();
        let again = c.read_frame_source(m.offset, 1 << 10).unwrap();
        assert_eq!(
            first.payload.as_ptr(),
            again.payload.as_ptr(),
            "from the mapping"
        );
    }
}
