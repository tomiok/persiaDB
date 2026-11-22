//! Reader leases against GC rounds, many readers at once, on a simulated file system (SPEC §9.3, §9.5; ROADMAP
//! 9.2.16; ADR-0054 items 4 and 9).
//!
//! A seeded sequence of operations: the writer puts (each put commits, and a put to an existing id replaces its
//! blob), deletes (and commits) and collects garbage; read-only handles open (lease first, then the documents),
//! refresh to the newest documents, and drop; and a handle whose documents were opened before a commit and its lease
//! after is refused. The container, the pack and the leases share one `SimFs`, whose locks belong to the handle, so
//! the writer judges the leases of handles in its own process.
//!
//! Invariants, after every operation:
//! 1. Every live handle's lease is open at its documents' seq.
//! 2. GC sweeps if and only if every live handle's documents are at the committed seq: `held_back` says so, the
//!    lease counts match the handles (live, current, behind; none opening, ahead or unreadable), and a held-back GC
//!    deletes nothing (its sweep is the empty report).
//! 3. A blob a live handle's documents show is never swept: the writer's pack holds it, and the handle reads it back
//!    whole.
//! 4. A GC that is not held back drops every blob no current document references.
//! 5. Documents older than the lease's taken seq are `OpenedBeforeLease`, and that refused open leaves no lease file.
//!
//! Seeds: fixed (`RngSeed::Fixed`), printed by proptest on failure. Not run under Miri (the engine's zstd is C).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::result_large_err
)] // Tests: inputs are generated.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::Arc;

use persia::internal::blob::Blobs;
use persia::internal::documents::{
    Blob, BlobDatabase, DocumentLog, Error, ReadOnlyBlobDatabase, ReaderLease,
};
use persia::internal::gc::{BlobReferences, GcOptions};
use persia_blob::gc::SweepReport;
use persia_blob::pack::lease::leases_dir;
use persia_blob::{BlobId, BlobRef, BlobStore, PackStore};
use persia_engine::db::{Database, DatabaseError, DatabaseOptions, ReadOnlyDatabase, SegmentIds};
use persia_engine::document::Document;
use persia_engine::schema::SchemaBuilder;
use persia_engine::wal::{Durability, WalMark};
use persia_storage::container::{Container, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

const DB: &str = "/app.persia";
const PACK: &str = "/app.persia.blobs";
/// Document ids `d0..d{DOCS}`.
const DOCS: u8 = 5;
const MAX_READERS: usize = 4;

type Writer = BlobDatabase<PackStore<SimFsFile>, SimLog>;

/// The engine's database on the simulated file system, as GC's document log: `BlobReferences` is implemented for the
/// real file system's `Database` only, so a test wraps the generic one.
struct SimLog(Database<SimFsFile>);

impl DocumentLog for SimLog {
    fn put(
        &mut self,
        collection: &str,
        doc: &Document,
        durability: Durability,
    ) -> Result<(), DatabaseError> {
        self.0.put(collection, doc, durability)
    }

    fn wal_mark(&self) -> WalMark {
        self.0.wal_mark()
    }

    fn durable_mark(&self) -> WalMark {
        self.0.durable_mark()
    }

    fn container_db_uuid(&self) -> Option<[u8; 16]> {
        Some(self.0.container().header().db_uuid)
    }

    fn commit(&mut self) -> Result<u64, DatabaseError> {
        self.0.commit()
    }
}

impl BlobReferences for SimLog {
    fn referenced_blobs(&self) -> Result<HashSet<BlobId>, DatabaseError> {
        self.0.referenced_blobs()
    }

    fn committed_seq(&self) -> u64 {
        self.0.seq()
    }
}
type Reader = ReadOnlyBlobDatabase<SimFsFile>;

#[derive(Debug, Clone)]
enum Op {
    /// Put `d<doc>` with new bytes (`large`: a multi-chunk blob), then commit.
    Put {
        doc: u8,
        large: bool,
    },
    /// Delete `d<doc>` if it is live, then commit.
    Delete {
        doc: u8,
    },
    Gc,
    /// A new read-only handle: its lease, then its documents.
    Open,
    /// Handle `n % live` refreshes to the newest documents.
    Refresh {
        n: usize,
    },
    /// Handle `n % live` is dropped.
    Drop {
        n: usize,
    },
    /// Documents opened, then a put of `d<doc>` commits, then the lease: refused.
    OpenBehind {
        doc: u8,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..DOCS, prop::bool::weighted(0.2)).prop_map(|(doc, large)| Op::Put { doc, large }),
        3 => (0..DOCS).prop_map(|doc| Op::Delete { doc }),
        4 => Just(Op::Gc),
        2 => Just(Op::Open),
        3 => any::<usize>().prop_map(|n| Op::Refresh { n }),
        1 => any::<usize>().prop_map(|n| Op::Drop { n }),
        1 => (0..DOCS).prop_map(|doc| Op::OpenBehind { doc }),
    ]
}

struct World {
    fs: SimFs,
    vfs: Arc<dyn Vfs<File = SimFsFile>>,
    writer: Writer,
    /// The live documents of each committed seq: id to reference.
    history: BTreeMap<u64, BTreeMap<u8, BlobRef>>,
    /// Every blob ever put, and its bytes.
    blobs: BTreeMap<BlobId, (BlobRef, Vec<u8>)>,
    readers: Vec<Reader>,
    puts: u32,
}

fn options() -> DatabaseOptions {
    DatabaseOptions {
        segment_ids: SegmentIds::seeded(71),
        ..DatabaseOptions::default()
    }
}

impl World {
    fn new() -> Self {
        let fs = SimFs::new();
        let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
        let container = Container::create_in(Arc::clone(&vfs), DB).unwrap();
        let mut db = Database::create_container(container, options()).unwrap();
        db.create_collection(SchemaBuilder::new("c").build().unwrap())
            .unwrap();
        let store = PackStore::open_writer_in(Arc::clone(&vfs), PACK, &db.writer_proof()).unwrap();
        let writer = BlobDatabase::new(SimLog(db), Blobs::new(Arc::new(store)));
        let mut history = BTreeMap::new();
        history.insert(writer.log().0.seq(), BTreeMap::new());
        Self {
            fs,
            vfs,
            writer,
            history,
            blobs: BTreeMap::new(),
            readers: Vec::new(),
            puts: 0,
        }
    }

    fn seq(&self) -> u64 {
        self.writer.log().0.seq()
    }

    fn current(&self) -> &BTreeMap<u8, BlobRef> {
        &self.history[&self.seq()]
    }

    fn documents(&self) -> ReadOnlyDatabase<SimFsFile> {
        let container = Container::open_in(Arc::clone(&self.vfs), DB, Mode::ReadOnly).unwrap();
        ReadOnlyDatabase::open_container(container, &options().registry).unwrap()
    }

    fn committed(&mut self, docs: BTreeMap<u8, BlobRef>) {
        let seq = self.writer.commit().unwrap();
        assert_eq!(seq, self.seq());
        self.history.insert(seq, docs);
    }

    fn put(&mut self, doc: u8, large: bool) {
        self.puts += 1;
        let len = if large { (128 << 10) + 17 } else { 40 };
        let bytes: Vec<u8> = (0..len)
            .map(|i: usize| {
                (i as u8) ^ (self.puts as u8).wrapping_mul(37) ^ ((self.puts >> 8) as u8)
            })
            .chain(self.puts.to_le_bytes())
            .collect();
        let r = self
            .writer
            .put(
                "c",
                &Document::new(format!("d{doc}")),
                Blob::from_reader(&bytes[..]),
                Durability::Fsync,
            )
            .unwrap();
        self.writer.release_durable();
        self.blobs.insert(r.blob_id(), (r.clone(), bytes));
        let mut docs = self.current().clone();
        docs.insert(doc, r);
        self.committed(docs);
    }

    fn delete(&mut self, doc: u8) {
        if !self.current().contains_key(&doc) {
            return;
        }
        self.writer
            .log_mut()
            .0
            .delete("c", &format!("d{doc}"), Durability::Fsync)
            .unwrap();
        self.writer.release_durable();
        let mut docs = self.current().clone();
        docs.remove(&doc);
        self.committed(docs);
    }

    fn open(&mut self) {
        if self.readers.len() >= MAX_READERS {
            return;
        }
        let lease = ReaderLease::take_in(&self.vfs, Path::new(PACK), Path::new(DB)).unwrap();
        assert_eq!(lease.taken_seq(), self.seq());
        let reader = ReadOnlyBlobDatabase::with_lease_in(
            lease,
            self.documents(),
            Arc::clone(&self.vfs),
            PACK,
        )
        .unwrap();
        self.readers.push(reader);
    }

    fn open_behind(&mut self, doc: u8) {
        let docs = self.documents();
        let before = self.lease_files();
        self.put(doc, false);
        let lease = ReaderLease::take_in(&self.vfs, Path::new(PACK), Path::new(DB)).unwrap();
        let e = ReadOnlyBlobDatabase::with_lease_in(lease, docs, Arc::clone(&self.vfs), PACK)
            .unwrap_err();
        assert!(
            matches!(e, Error::OpenedBeforeLease { opened, taken } if taken == self.seq() && opened < taken),
            "{e:?}"
        );
        assert_eq!(
            self.lease_files(),
            before,
            "the refused handle's lease is gone"
        );
    }

    fn lease_files(&self) -> BTreeSet<String> {
        self.fs
            .read_dir(&leases_dir(Path::new(PACK)))
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|e| e.name.into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn refresh(&mut self, n: usize) {
        if self.readers.is_empty() {
            return;
        }
        let i = n % self.readers.len();
        let reopened = self.documents();
        self.readers[i].refresh(reopened).unwrap();
        assert_eq!(self.readers[i].database().seq(), self.seq());
    }

    fn drop_reader(&mut self, n: usize) {
        if self.readers.is_empty() {
            return;
        }
        let i = n % self.readers.len();
        drop(self.readers.swap_remove(i));
    }

    fn gc(&mut self) {
        let before = self.stored();
        let report = self
            .writer
            .gc(&GcOptions {
                rewrite_ratio: 0.0,
                ..GcOptions::default()
            })
            .unwrap();
        let seq = self.seq();
        let Some(readers) = report.readers.clone() else {
            // No pack file yet: nothing was ever stored, so nothing to sweep.
            assert!(self.blobs.is_empty(), "{report:?}");
            return;
        };
        let seqs: Vec<u64> = self.readers.iter().map(|r| r.database().seq()).collect();
        let behind = seqs.iter().filter(|&&s| s < seq).count() as u64;
        let current = seqs.iter().filter(|&&s| s == seq).count() as u64;
        assert_eq!(
            (
                readers.live,
                readers.current,
                readers.behind,
                readers.opening,
                readers.ahead,
                readers.unreadable,
                readers.stale_removed
            ),
            (seqs.len() as u64, current, behind, 0, 0, 0, 0),
            "seqs {seqs:?} at {seq}: {readers:?}"
        );
        assert_eq!(
            readers.oldest_seq,
            seqs.iter().min().copied(),
            "{readers:?}"
        );
        let held_back = behind > 0;
        assert_eq!(
            readers.held_back, held_back,
            "seqs {seqs:?} at {seq}: {readers:?}"
        );
        let after = self.stored();
        if held_back {
            assert_eq!(report.sweep, SweepReport::default(), "{report:?}");
            assert_eq!(after, before, "a held-back GC dropped a blob");
        } else {
            let referenced: BTreeSet<BlobId> =
                self.current().values().map(BlobRef::blob_id).collect();
            assert_eq!(
                after, referenced,
                "a sweep keeps exactly what is referenced: {report:?}"
            );
            assert_eq!(
                report.sweep.blobs_deleted,
                (before.len() - after.len()) as u64,
                "{report:?}"
            );
        }
    }

    /// The blobs ever put that the writer's pack still holds.
    fn stored(&self) -> BTreeSet<BlobId> {
        self.blobs
            .iter()
            .filter(|(_, (r, _))| self.writer.blobs().store().contains(r).unwrap())
            .map(|(id, _)| *id)
            .collect()
    }

    fn check(&self) {
        for (i, reader) in self.readers.iter().enumerate() {
            let seq = reader.database().seq();
            let record = reader.lease().unwrap().record().unwrap();
            assert_eq!(record.manifest_seq(), Some(seq), "reader {i}");
            assert_eq!(
                record.db_uuid(),
                Some(self.writer.log().0.container().header().db_uuid)
            );
            for (doc, r) in &self.history[&seq] {
                assert!(
                    self.writer.blobs().store().contains(r).unwrap(),
                    "reader {i} at seq {seq} shows d{doc}, whose blob was swept"
                );
                let read = reader.get_blob_range(r, 0..r.size()).unwrap();
                assert!(
                    read == self.blobs[&r.blob_id()].1,
                    "reader {i} read d{doc} wrong"
                );
            }
        }
        assert_eq!(
            self.lease_files().len(),
            self.readers.len(),
            "one lease file per handle"
        );
    }

    fn apply(&mut self, op: &Op) {
        match *op {
            Op::Put { doc, large } => self.put(doc, large),
            Op::Delete { doc } => self.delete(doc),
            Op::Gc => self.gc(),
            Op::Open => self.open(),
            Op::Refresh { n } => self.refresh(n),
            Op::Drop { n } => self.drop_reader(n),
            Op::OpenBehind { doc } => self.open_behind(doc),
        }
        self.check();
    }
}

proptest! {
    #![proptest_config(Config {
        cases: if cfg!(feature = "slow") { 512 } else { 48 },
        rng_seed: RngSeed::Fixed(9216_0016),
        failure_persistence: None,
        ..Config::default()
    })]

    #[test]
    fn gc_sweeps_only_when_every_reader_is_current_and_never_a_blob_a_reader_shows(
        ops in prop::collection::vec(op(), 1..40),
    ) {
        let mut world = World::new();
        for op in &ops {
            world.apply(op);
        }
        // Once every handle is gone, a GC sweeps whatever nothing references.
        world.readers.clear();
        world.apply(&Op::Gc);
        let referenced: BTreeSet<BlobId> = world.current().values().map(BlobRef::blob_id).collect();
        prop_assert_eq!(world.stored(), referenced);
        prop_assert!(world.lease_files().is_empty());
    }
}

/// A fixed case of the property: two readers, one refreshes, the other holds GC back until it refreshes too.
#[test]
fn two_readers_hold_gc_back_until_both_are_current() {
    let mut world = World::new();
    for op in [
        Op::Put {
            doc: 0,
            large: true,
        },
        Op::Put {
            doc: 1,
            large: false,
        },
        Op::Open,
        Op::Open,
        Op::Delete { doc: 0 },
        Op::Gc,
        Op::Refresh { n: 0 },
        Op::Gc,
        Op::Refresh { n: 1 },
        Op::Gc,
    ] {
        world.apply(&op);
    }
    assert_eq!(
        world.stored().len(),
        1,
        "d0's blob went once both refreshed"
    );
}
