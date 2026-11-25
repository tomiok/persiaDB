//! `persia stats` (ROADMAP 10.4.1): what a read-only open of a database can report, without the writer lock and
//! without writing anything (module `open`), so it runs beside a live writer and shows the state that writer last made
//! durable. It is the whole-database counterpart of `persia inspect container` (the frames) and `persia inspect
//! segment` (one segment file): there is no separate `inspect database`.
//!
//! The text output is for people: `key  value` rows, then one block per collection with a row per segment. `--json`
//! writes one JSON object on one line, with these keys and no others (a stable shape: keys are only ever added):
//!
//! ```text
//! {
//!   "path":      the container's canonical path,
//!   "format":    {"major": 1, "minor": 0},                 the container header's format version (SPEC §4.2)
//!   "db_uuid":   32 lowercase hex digits,
//!   "seq":       the last commit's seq (SPEC §4.3),
//!   "container": {
//!     "bytes":              the end of the valid frames (SPEC §4.6 "Space accounting"),
//!     "live_bytes":         the prefix, the frames the manifest lists below wal_start, and all from wal_start on,
//!     "dead_bytes":         bytes - live_bytes: what a compaction reclaims,
//!     "compaction_due":     whether a writer with the DEFAULT thresholds would compact (40 % and 1 MiB dead):
//!                           the application's own `CompactionOptions` are not stored, so they cannot be read,
//!     "uncommitted_frames": valid frames after log_end (SPEC §4.5.1),
//!     "uncommitted_bytes":  their total length
//!   },
//!   "wal": {
//!     "wal_start":           where replay starts (SPEC §8.6),
//!     "log_end":             the end of the last commit,
//!     "replayed_records":    WAL records the open replayed: written since the last flush,
//!     "frames_after_commit": WAL chunk frames after log_end,
//!     "bytes_after_commit":  their total length
//!   },
//!   "collections": [ in ascending byte order of their names
//!     {
//!       "name":   the collection,
//!       "schema": {"hash": 32 hex digits (SPEC §6.2.5), "strict": bool, "fields": [{"name": .., "type": ..}]},
//!       "documents":         live documents, as a reader sees them: the segments' without the deleted and the
//!                            ones a WAL record replaces or deletes, plus the WAL's,
//!       "segment_documents": documents in the segments, deleted ones included,
//!       "deleted":           of those, the ones the committed tombstones delete (SPEC §8.3),
//!       "wal_documents":     live documents of WAL records not yet in a segment,
//!       "segment_bytes":     the segments' total size,
//!       "segments": [ oldest first, as the manifest lists them
//!         {"id": 32 hex digits, "documents": .., "deleted": .., "bytes": ..}
//!       ]
//!     }
//!   ],
//!   "blob_pack": {"present": bool, "bytes": the size of <name>.persia.blobs, or null without one (SPEC §9.3)}
//! }
//! ```
//!
//! The dead bytes are what the manifest does not list. A live writer may still keep some of them for its readers
//! (obsolete segments a snapshot holds, SPEC §8.5), which only that writer knows.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use persia_engine::db::{CollectionSnapshot, CompactionOptions};
use persia_engine::manifest::{CollectionEntry, Location, Manifest};
use persia_engine::view::SearchView;
use persia_format::frame::FrameKind;
use persia_format::header::FORMAT_MAJOR;
use persia_storage::container::{FrameRef, SpaceUsage};
use serde_json::{Value, json};

use crate::open;

/// What `persia stats` reports (the module docs describe each number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    path: PathBuf,
    format_minor: u16,
    db_uuid: [u8; 16],
    seq: u64,
    space: SpaceUsage,
    compaction_due: bool,
    uncommitted: Frames,
    wal_start: u64,
    log_end: u64,
    replayed: u64,
    wal_after_commit: Frames,
    collections: Vec<CollectionStats>,
    blob_pack: Option<u64>,
}

/// How many frames, and their total length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Frames {
    count: u64,
    bytes: u64,
}

impl Frames {
    fn of<'a>(frames: impl Iterator<Item = &'a FrameRef>) -> Self {
        frames.fold(Self::default(), |sum, f| Self {
            count: sum.count.saturating_add(1),
            bytes: sum.bytes.saturating_add(f.len),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CollectionStats {
    name: String,
    schema_hash: String,
    strict: bool,
    /// Each field's name and type name (SPEC §6.1).
    fields: Vec<(String, &'static str)>,
    documents: u64,
    wal_documents: u64,
    segments: Vec<SegmentStats>,
}

impl CollectionStats {
    fn segment_documents(&self) -> u64 {
        sum(self.segments.iter().map(|s| u64::from(s.documents)))
    }

    fn deleted(&self) -> u64 {
        sum(self.segments.iter().map(|s| u64::from(s.deleted)))
    }

    fn segment_bytes(&self) -> u64 {
        sum(self.segments.iter().map(|s| s.bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SegmentStats {
    id: String,
    documents: u32,
    deleted: u32,
    bytes: u64,
}

fn sum(values: impl Iterator<Item = u64>) -> u64 {
    values.fold(0, u64::saturating_add)
}

/// Opens the database at `path` read-only and writes its statistics to `out`, as JSON with `json`.
pub fn stats(out: &mut dyn Write, path: &Path, json: bool) -> Result<()> {
    let stats = gather(path)?;
    if json {
        writeln!(out, "{}", stats.to_json())?;
    } else {
        stats.write_text(out)?;
    }
    Ok(())
}

/// The statistics of the database at `path`, read through one read-only open.
pub fn gather(path: &Path) -> Result<Stats> {
    let db = open::read_only(path)?;
    let container = db.container();
    let snapshot = db.snapshot();
    let manifest = snapshot.manifest();
    let superblock = container.superblock();
    let space = container
        .space_usage(&live_below_wal(manifest, superblock.wal_start))
        .with_context(|| format!("measuring the space of {}", path.display()))?;
    let uncommitted = container.uncommitted_frames();
    let mut collections = Vec::new();
    for entry in manifest.collections() {
        let collection = snapshot.collection(entry.name()).ok_or_else(|| {
            anyhow!(
                "the snapshot of {} lacks the collection {:?} its manifest lists",
                path.display(),
                entry.name()
            )
        })?;
        collections.push(
            collection_stats(entry, collection)
                .with_context(|| format!("reading the collection {:?}", entry.name()))?,
        );
    }
    let canonical = container.path().unwrap_or(path).to_path_buf();
    Ok(Stats {
        blob_pack: pack_size(&canonical)?,
        path: canonical,
        format_minor: container.header().format_minor,
        db_uuid: container.header().db_uuid,
        seq: snapshot.seq(),
        space,
        compaction_due: compaction_due(space, &CompactionOptions::default()),
        uncommitted: Frames::of(uncommitted.iter()),
        wal_start: superblock.wal_start,
        log_end: superblock.log_end,
        replayed: db.replayed(),
        wal_after_commit: Frames::of(uncommitted.iter().filter(|f| f.kind == FrameKind::WalChunk)),
        collections,
    })
}

/// Whether a writer with the thresholds `options` would compact a container with this space on its own (SPEC §4.6):
/// the dead bytes reach both the floor and the ratio, as `Database::compact_if_due` decides.
fn compaction_due(space: SpaceUsage, options: &CompactionOptions) -> bool {
    space.dead() >= options.min_dead_bytes && space.is_due(options.dead_space_percent)
}

/// The frames below `wal_start` the manifest lists: its segments and their tombstone sidecars (SPEC §4.6). Every
/// frame from `wal_start` on is live whatever it holds.
fn live_below_wal(manifest: &Manifest, wal_start: u64) -> Vec<std::ops::Range<u64>> {
    manifest
        .collections()
        .flat_map(|c| c.segments().iter())
        .flat_map(|s| std::iter::once(s.location).chain(s.tombstones.map(|t| t.location)))
        .filter_map(|location| match location {
            Location::Frame { offset, len } => Some(offset..offset.saturating_add(len)),
            Location::Object => None,
        })
        .filter(|range| range.end <= wal_start)
        .collect()
}

fn collection_stats(
    entry: &CollectionEntry,
    collection: &CollectionSnapshot,
) -> Result<CollectionStats> {
    let schema = collection.schema();
    let memtable = collection.memtable();
    // The views leave out what the committed tombstones delete and what a WAL record replaces or deletes (SPEC §8.3).
    let views = collection.views(&memtable)?;
    let mut live = u64::from(memtable.live_count());
    for view in views.segments() {
        let hidden = view.deleted()?.len();
        live = live.saturating_add(u64::from(view.doc_count()).saturating_sub(hidden));
    }
    Ok(CollectionStats {
        name: entry.name().to_owned(),
        schema_hash: schema.hash().to_string(),
        strict: schema.strict(),
        fields: schema
            .fields()
            .iter()
            .map(|f| (f.name().to_owned(), f.field_type().name()))
            .collect(),
        documents: live,
        wal_documents: u64::from(memtable.live_count()),
        segments: entry
            .segments()
            .iter()
            .map(|s| SegmentStats {
                id: s.id.to_string(),
                documents: s.doc_count,
                deleted: s.tombstones.map_or(0, |t| t.deleted),
                bytes: s.size,
            })
            .collect(),
    })
}

/// The size of the blob pack beside the container at `container`; `None` when the database has none.
fn pack_size(container: &Path) -> Result<Option<u64>> {
    let pack = persia::internal::pack_path(container);
    match std::fs::metadata(&pack) {
        Ok(meta) => Ok(Some(meta.len())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading the size of {}", pack.display())),
    }
}

/// The `compaction` row: whether a compaction is due, and by which thresholds (the default ones, `options`; the
/// application's own are not stored in the file).
fn due_text(due: bool, options: &CompactionOptions) -> String {
    let state = if due { "due" } else { "not due" };
    format!(
        "{state} with the default thresholds ({}% and {} bytes dead)",
        options.dead_space_percent, options.min_dead_bytes
    )
}

/// A `db_uuid` as 32 lowercase hex digits, in the order of its bytes.
fn hex(db_uuid: [u8; 16]) -> String {
    format!("{:032x}", u128::from_be_bytes(db_uuid))
}

impl Stats {
    /// The `--json` object (the module docs give its shape).
    pub fn to_json(&self) -> Value {
        // The header does not store `format_major`: `Header::decode` refuses any major but this build's (SPEC §4.2),
        // so a container that opened has `FORMAT_MAJOR`.
        json!({
            "path": self.path.display().to_string(),
            "format": {"major": FORMAT_MAJOR, "minor": self.format_minor},
            "db_uuid": hex(self.db_uuid),
            "seq": self.seq,
            "container": {
                "bytes": self.space.total,
                "live_bytes": self.space.live,
                "dead_bytes": self.space.dead(),
                "compaction_due": self.compaction_due,
                "uncommitted_frames": self.uncommitted.count,
                "uncommitted_bytes": self.uncommitted.bytes,
            },
            "wal": {
                "wal_start": self.wal_start,
                "log_end": self.log_end,
                "replayed_records": self.replayed,
                "frames_after_commit": self.wal_after_commit.count,
                "bytes_after_commit": self.wal_after_commit.bytes,
            },
            "collections": self.collections.iter().map(CollectionStats::to_json).collect::<Vec<_>>(),
            "blob_pack": {"present": self.blob_pack.is_some(), "bytes": self.blob_pack},
        })
    }

    /// The text report.
    pub fn write_text(&self, out: &mut dyn Write) -> Result<()> {
        let space = self.space;
        let percent = u128::from(space.dead())
            .saturating_mul(100)
            .checked_div(u128::from(space.total))
            .unwrap_or(0);
        let rows = [
            ("path", self.path.display().to_string()),
            ("format", format!("{FORMAT_MAJOR}.{}", self.format_minor)),
            ("db_uuid", hex(self.db_uuid)),
            ("seq", self.seq.to_string()),
            (
                "size",
                format!(
                    "{} bytes ({} live, {} dead, {percent}%)",
                    space.total,
                    space.live,
                    space.dead()
                ),
            ),
            (
                "compaction",
                due_text(self.compaction_due, &CompactionOptions::default()),
            ),
            (
                "uncommitted",
                format!(
                    "{} frames ({} bytes)",
                    self.uncommitted.count, self.uncommitted.bytes
                ),
            ),
            (
                "wal",
                format!(
                    "{} records replayed, {} frames ({} bytes) after the last commit",
                    self.replayed, self.wal_after_commit.count, self.wal_after_commit.bytes
                ),
            ),
            (
                "blob pack",
                match self.blob_pack {
                    Some(bytes) => format!("{bytes} bytes"),
                    None => "none".to_owned(),
                },
            ),
            ("collections", self.collections.len().to_string()),
        ];
        for (key, value) in rows {
            writeln!(out, "{key:<18}{value}")?;
        }
        for collection in &self.collections {
            collection.write_text(out)?;
        }
        Ok(())
    }
}

impl CollectionStats {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "schema": {
                "hash": self.schema_hash,
                "strict": self.strict,
                "fields": self
                    .fields
                    .iter()
                    .map(|(name, kind)| json!({"name": name, "type": kind}))
                    .collect::<Vec<_>>(),
            },
            "documents": self.documents,
            "segment_documents": self.segment_documents(),
            "deleted": self.deleted(),
            "wal_documents": self.wal_documents,
            "segment_bytes": self.segment_bytes(),
            "segments": self
                .segments
                .iter()
                .map(|s| json!({"id": s.id, "documents": s.documents, "deleted": s.deleted, "bytes": s.bytes}))
                .collect::<Vec<_>>(),
        })
    }

    fn write_text(&self, out: &mut dyn Write) -> Result<()> {
        let fields: Vec<String> = self
            .fields
            .iter()
            .map(|(name, kind)| format!("{name:?} {kind}"))
            .collect();
        let rows = [
            (
                "schema",
                format!(
                    "{} fields ({}), {}, hash {}",
                    self.fields.len(),
                    fields.join(", "),
                    if self.strict { "strict" } else { "not strict" },
                    self.schema_hash
                ),
            ),
            (
                "documents",
                format!(
                    "{} live ({} in the WAL), {} of {} deleted in segments",
                    self.documents,
                    self.wal_documents,
                    self.deleted(),
                    self.segment_documents()
                ),
            ),
            (
                "segments",
                format!("{} ({} bytes)", self.segments.len(), self.segment_bytes()),
            ),
        ];
        writeln!(out, "\ncollection {:?}", self.name)?;
        for (key, value) in rows {
            writeln!(out, "  {key:<16}{value}")?;
        }
        if !self.segments.is_empty() {
            writeln!(
                out,
                "  {:<32} {:>10} {:>10} {:>12}",
                "segment", "documents", "deleted", "bytes"
            )?;
        }
        for s in &self.segments {
            writeln!(
                out,
                "  {:<32} {:>10} {:>10} {:>12}",
                s.id, s.documents, s.deleted, s.bytes
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Stats {
        Stats {
            path: PathBuf::from("/data/app.persia"),
            format_minor: 0,
            db_uuid: [0xab; 16],
            seq: 7,
            space: SpaceUsage {
                total: 20_000,
                live: 15_000,
            },
            compaction_due: false,
            uncommitted: Frames {
                count: 3,
                bytes: 480,
            },
            wal_start: 14_000,
            log_end: 19_520,
            replayed: 5,
            wal_after_commit: Frames {
                count: 2,
                bytes: 320,
            },
            collections: vec![
                CollectionStats {
                    name: "empty".into(),
                    schema_hash: "00".repeat(16),
                    strict: false,
                    fields: Vec::new(),
                    documents: 0,
                    wal_documents: 0,
                    segments: Vec::new(),
                },
                CollectionStats {
                    name: "notes".into(),
                    schema_hash: "0f".repeat(16),
                    strict: true,
                    fields: vec![("body".into(), "text"), ("tag\n".into(), "keyword")],
                    documents: 9,
                    wal_documents: 2,
                    segments: vec![
                        SegmentStats {
                            id: "11".repeat(16),
                            documents: 8,
                            deleted: 3,
                            bytes: 900,
                        },
                        SegmentStats {
                            id: "22".repeat(16),
                            documents: 4,
                            deleted: 0,
                            bytes: 500,
                        },
                    ],
                },
            ],
            blob_pack: Some(4096),
        }
    }

    /// The whole object, key by key: the shape scripts read (the module docs). `serde_json` writes an object's keys
    /// in ascending order.
    #[test]
    fn the_json_object_has_its_documented_shape() {
        let expected = concat!(
            r#"{"blob_pack":{"bytes":4096,"present":true},"#,
            r#""collections":["#,
            r#"{"deleted":0,"documents":0,"name":"empty","#,
            r#""schema":{"fields":[],"hash":"00000000000000000000000000000000","strict":false},"#,
            r#""segment_bytes":0,"segment_documents":0,"segments":[],"wal_documents":0},"#,
            r#"{"deleted":3,"documents":9,"name":"notes","#,
            r#""schema":{"fields":[{"name":"body","type":"text"},{"name":"tag\n","type":"keyword"}],"#,
            r#""hash":"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f","strict":true},"#,
            r#""segment_bytes":1400,"segment_documents":12,"segments":["#,
            r#"{"bytes":900,"deleted":3,"documents":8,"id":"11111111111111111111111111111111"},"#,
            r#"{"bytes":500,"deleted":0,"documents":4,"id":"22222222222222222222222222222222"}],"#,
            r#""wal_documents":2}],"#,
            r#""container":{"bytes":20000,"compaction_due":false,"dead_bytes":5000,"live_bytes":15000,"#,
            r#""uncommitted_bytes":480,"uncommitted_frames":3},"#,
            r#""db_uuid":"abababababababababababababababab","#,
            r#""format":{"major":1,"minor":0},"#,
            r#""path":"/data/app.persia","#,
            r#""seq":7,"#,
            r#""wal":{"bytes_after_commit":320,"frames_after_commit":2,"log_end":19520,"replayed_records":5,"#,
            r#""wal_start":14000}}"#,
        );
        assert_eq!(sample().to_json().to_string(), expected);
    }

    #[test]
    fn a_database_without_a_pack_reports_null_bytes() {
        let stats = Stats {
            blob_pack: None,
            ..sample()
        };
        assert_eq!(
            stats.to_json()["blob_pack"].to_string(),
            r#"{"bytes":null,"present":false}"#
        );
    }

    #[test]
    fn the_text_report_states_every_number() {
        let mut out = Vec::new();
        let stats = Stats {
            compaction_due: true,
            ..sample()
        };
        stats.write_text(&mut out).unwrap();
        let expected = "\
path              /data/app.persia
format            1.0
db_uuid           abababababababababababababababab
seq               7
size              20000 bytes (15000 live, 5000 dead, 25%)
compaction        due with the default thresholds (40% and 1048576 bytes dead)
uncommitted       3 frames (480 bytes)
wal               5 records replayed, 2 frames (320 bytes) after the last commit
blob pack         4096 bytes
collections       2

collection \"empty\"
  schema          0 fields (), not strict, hash 00000000000000000000000000000000
  documents       0 live (0 in the WAL), 0 of 0 deleted in segments
  segments        0 (0 bytes)

collection \"notes\"
  schema          2 fields (\"body\" text, \"tag\\n\" keyword), strict, hash 0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f
  documents       9 live (2 in the WAL), 3 of 12 deleted in segments
  segments        2 (1400 bytes)
  segment                           documents    deleted        bytes
  11111111111111111111111111111111          8          3          900
  22222222222222222222222222222222          4          0          500
";
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    /// An empty container cannot divide by its size.
    #[test]
    fn the_dead_percentage_of_an_empty_container_is_zero() {
        let mut out = Vec::new();
        let stats = Stats {
            space: SpaceUsage { total: 0, live: 0 },
            blob_pack: None,
            ..sample()
        };
        stats.write_text(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("size              0 bytes (0 live, 0 dead, 0%)\n"),
            "{text}"
        );
        assert!(text.contains("compaction        not due with the default thresholds (40% and 1048576 bytes dead)\n"), "{text}");
        assert!(text.contains("blob pack         none\n"), "{text}");
    }

    /// Both thresholds of SPEC §4.6 must be reached: the ratio alone (a small file) or the floor alone (a large,
    /// mostly live file) is not due.
    #[test]
    fn compaction_is_due_only_past_the_ratio_and_the_floor() {
        let options = CompactionOptions {
            automatic: true,
            dead_space_percent: 40,
            min_dead_bytes: 1000,
        };
        let space = |total, live| SpaceUsage { total, live };
        assert!(compaction_due(space(2500, 1500), &options)); // 1000 dead, 40 %
        assert!(!compaction_due(space(2500, 1501), &options)); // 999 dead: under the floor (and the ratio)
        assert!(!compaction_due(space(1000, 100), &options)); // 90 % dead, under the floor
        assert!(!compaction_due(space(10_000, 8000), &options)); // 2000 dead, only 20 %
        assert!(compaction_due(space(10_000, 6000), &options));
    }

    #[test]
    fn a_db_uuid_is_written_in_the_order_of_its_bytes() {
        let uuid = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0xff,
        ];
        assert_eq!(hex(uuid), "000102030405060708090a0b0c0d0eff");
    }

    /// Only a missing file means the database has no pack: a pack that cannot be examined is the command's error,
    /// not a database reported without one.
    #[cfg(unix)]
    #[test]
    fn a_pack_that_cannot_be_examined_is_an_error_not_a_missing_pack() {
        let dir = tempfile::tempdir().unwrap();
        let container = dir.path().join("app.persia");
        assert_eq!(pack_size(&container).unwrap(), None);
        let pack = dir.path().join("app.persia.blobs");
        // A link that names itself: following it never ends, which is not "no such file".
        std::os::unix::fs::symlink(&pack, &pack).unwrap();
        let err = pack_size(&container).unwrap_err();
        assert!(
            format!("{err:#}").contains("reading the size of"),
            "{err:#}"
        );
        std::fs::remove_file(&pack).unwrap();
        std::fs::write(&pack, b"12345").unwrap();
        assert_eq!(pack_size(&container).unwrap(), Some(5));
    }

    #[test]
    fn frames_are_counted_with_their_lengths() {
        let frame = |len| FrameRef {
            offset: 12_288,
            len,
            kind: FrameKind::WalChunk,
            frame_seq: 1,
        };
        let frames = [frame(64), frame(96)];
        assert_eq!(
            Frames::of(frames.iter()),
            Frames {
                count: 2,
                bytes: 160
            }
        );
        assert_eq!(Frames::of([].iter()), Frames::default());
    }

    /// Only frames that end at or below `wal_start` are named: the container counts everything from there on as
    /// live itself, and refuses a range that crosses it.
    #[test]
    fn the_live_frames_are_the_manifests_below_wal_start() {
        use persia_engine::db::{Database, DatabaseOptions};
        use persia_engine::document::Document;
        use persia_engine::schema::SchemaBuilder;
        use persia_engine::wal::Durability;
        use persia_storage::container::sim::SimFile;

        let mut db =
            Database::create_on(SimFile::new(), [7; 16], DatabaseOptions::default()).unwrap();
        db.create_collection(SchemaBuilder::new("c").build().unwrap())
            .unwrap();
        // Two segments, and a sidecar for the first one.
        for ids in [&["a", "b"][..], &["c"]] {
            for id in ids {
                db.put("c", &Document::new(*id), Durability::Fsync).unwrap();
            }
            db.commit().unwrap();
        }
        db.delete("c", "a", Durability::Fsync).unwrap();
        db.commit().unwrap();
        let manifest = db.manifest();
        let wal_start = db.container().superblock().wal_start;
        let live = live_below_wal(manifest, wal_start);
        assert_eq!(live.len(), 3);
        assert_eq!(
            db.container().space_usage(&live).unwrap(),
            db.space_usage().unwrap()
        );
        // With a `wal_start` below every frame, none is named.
        assert_eq!(live_below_wal(manifest, 12_288), []);
        let first = live.iter().map(|r| r.end).min().unwrap();
        assert_eq!(live_below_wal(manifest, first).len(), 1);
        assert_eq!(live_below_wal(manifest, first - 1), []);
    }
}
