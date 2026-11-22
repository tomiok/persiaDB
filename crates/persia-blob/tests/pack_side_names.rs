//! The pack's temporary names, as the two sweeps that remove them match them (SPEC §4.5.1, §9.3; ROADMAP 9.2.6,
//! 9.2.10): the pack's own sweep at a read-write open and the container's `sweep_stale_files`.
//!
//! Invariants:
//! 1. Differential: for any name beside the container, the container's sweep removes it exactly when the pack's own
//!    sweep does: a regular file named `<name>.persia.blobs.tmp.<16 lowercase hex>.<decimal>`, nothing else (not
//!    uppercase hex, another nonce length, a missing or non-decimal counter, a non-ASCII digit, another container's).
//! 2. A directory with such a name is never removed by either, and the container's listing leaves it out.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure.
#![allow(clippy::unwrap_used, clippy::panic, missing_docs)] // Tests: inputs are generated or fixed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_blob::{PackStore, pack_path};
use persia_storage::container::{Container, ContainerFile, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use proptest::prelude::*;

const DB: &str = "/db/app.persia";

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

/// A container at `DB` and an entry `/db/<name>`: a regular file, or a directory if `dir`.
fn with_entry(name: &str, dir: bool) -> (SimFs, PathBuf) {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    drop(Container::create_in(vfs(&fs), DB).unwrap());
    let path = Path::new("/db").join(name);
    if dir {
        fs.create_dir(&path).unwrap();
    } else {
        let f = fs.create_new(&path).unwrap();
        f.write_all_at(b"left behind", 0).unwrap();
        f.sync_data().unwrap();
    }
    fs.sync_dir(Path::new("/db")).unwrap();
    (fs, path)
}

fn exists(fs: &SimFs, path: &Path) -> bool {
    fs.entry_kind(path).is_ok()
}

/// Whether the container's `sweep_stale_files` removes `/db/<name>`, and whether its `side_files` lists it.
fn container_sweeps(name: &str, dir: bool) -> (bool, bool) {
    let (fs, path) = with_entry(name, dir);
    let listed = Container::side_files_in(&fs, DB)
        .unwrap()
        .iter()
        .any(|f| f.path == path);
    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    let removed = c.sweep_stale_files().unwrap().contains(&path);
    assert_eq!(removed, !exists(&fs, &path), "{name}");
    (removed, listed)
}

/// Whether the pack's read-write open sweeps `/db/<name>`.
fn pack_sweeps(name: &str, dir: bool) -> bool {
    let (fs, path) = with_entry(name, dir);
    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    let store = PackStore::open_writer_in(
        vfs(&fs),
        pack_path(Path::new(DB)),
        &c.writer_proof().unwrap(),
    )
    .unwrap();
    let swept = store.recovery().swept == 1;
    assert_eq!(swept, !exists(&fs, &path), "{name}");
    swept
}

/// Both sweeps agree on `name`, as a file and as a directory; returns whether they remove the file. A directory is
/// never removed nor listed.
fn agree(name: &str) -> bool {
    let (container, listed) = container_sweeps(name, false);
    let pack = pack_sweeps(name, false);
    assert_eq!(
        container, pack,
        "{name}: container {container}, pack {pack}"
    );
    assert_eq!(listed, container, "{name}: listed {listed}");
    assert_eq!(container_sweeps(name, true), (false, false), "{name}/");
    assert!(!pack_sweeps(name, true), "{name}/");
    container
}

#[test]
fn both_sweeps_remove_exactly_the_names_the_pack_makes() {
    for name in [
        "app.persia.blobs.tmp.0123456789abcdef.0",
        "app.persia.blobs.tmp.ffffffffffffffff.18446744073709551615",
        // Longer than a u64: still digits, as the pack's own matcher reads them.
        "app.persia.blobs.tmp.0000000000000000.000184467440737095516150",
    ] {
        assert!(agree(name), "{name}");
    }
    for name in [
        "app.persia.blobs.tmp.",
        "app.persia.blobs.tmp..1",
        "app.persia.blobs.tmp.0123456789abcdef",
        "app.persia.blobs.tmp.0123456789abcdef.",
        "app.persia.blobs.tmp.0123456789abcdeF.1",
        "app.persia.blobs.tmp.0123456789ABCDEF.1",
        "app.persia.blobs.tmp.0123456789abcde.1",
        "app.persia.blobs.tmp.0123456789abcdef0.1",
        "app.persia.blobs.tmp.0123456789abcdeg.1",
        "app.persia.blobs.tmp.0123456789abcdef.-1",
        "app.persia.blobs.tmp.0123456789abcdef.+1",
        "app.persia.blobs.tmp.0123456789abcdef.1.2",
        "app.persia.blobs.tmp.0123456789abcdef.1 ",
        "app.persia.blobs.tmp.0123456789abcdef.\u{FF11}",
        "app.persia.blobs.tmp.\u{FF10}123456789abcde.1",
        "app.persia.blobs.tmp.0123456789abcdef.1.tmp.0123456789abcdef.1x",
        "app.persia.blobs.TMP.0123456789abcdef.1",
        "app.persia.blobs.tmp_0123456789abcdef.1",
        "app.persia.blobs..tmp.0123456789abcdef.1",
        "app.persia.blobs.compact.0123456789abcdef.1",
        "app.persia.blobs.lock.tmp.0123456789abcdef.1",
        "app.persia.blobs.blobs.tmp.0123456789abcdef.1",
        "App.persia.blobs.tmp.0123456789abcdef.1",
        "xapp.persia.blobs.tmp.0123456789abcdef.1",
        "other.persia.blobs.tmp.0123456789abcdef.1",
    ] {
        assert!(!agree(name), "{name}");
    }
}

/// Names around the pattern, most of them one character from a name the pack makes: the pack's prefix or a near miss
/// of it (none a container temporary's), a nonce of mostly 16 characters, mostly lowercase hex, and a counter of
/// mostly digits, or none.
fn near_names() -> impl Strategy<Value = String> {
    let nonce_char = prop_oneof![
        12 => prop::sample::select(b"0123456789abcdef".to_vec()),
        1 => prop::sample::select(b"ABCDEFg.-".to_vec()),
    ];
    let nonce_len = prop_oneof![4 => Just(16_usize), 1 => 14..=18_usize];
    let counter_char = prop_oneof![
        8 => prop::sample::select(b"0123456789".to_vec()),
        1 => prop::sample::select(b"x.+ ".to_vec()),
    ];
    (
        prop_oneof![
            6 => Just("app.persia.blobs.tmp."),
            1 => prop::sample::select(vec![
                "app.persia.blobs.tmp",
                "app.persia.blob.tmp.",
                "app.persia.blobs.compact.",
                "other.persia.blobs.tmp.",
            ]),
        ],
        nonce_len.prop_flat_map(move |n| prop::collection::vec(nonce_char.clone(), n)),
        prop::option::weighted(0.9, prop::collection::vec(counter_char, 0..=3)),
    )
        .prop_map(|(prefix, nonce, counter)| {
            let mut name = prefix.to_owned();
            name.extend(nonce.into_iter().map(char::from));
            if let Some(counter) = counter {
                name.push('.');
                name.extend(counter.into_iter().map(char::from));
            }
            name
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(if cfg!(feature = "slow") { 1024 } else { 128 }),
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x9210),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Invariants 1 and 2 on names around the pattern: the two sweeps never disagree.
    #[test]
    fn the_container_and_the_pack_sweep_the_same_names(name in near_names()) {
        agree(&name);
    }
}
