//! Container compaction driven by the writer (SPEC §4.6, ROADMAP 6.5.8).
//!
//! The container copies frames without reading them; the writer says which ones are live below `wal_start` (the
//! frames its manifest lists, and the obsolete ones a reader may still read, SPEC §8.5) and builds the manifest of the
//! new file from where they moved (`Manifest::relocate`, SPEC §8.7.2). Once the new file is in place, everything the
//! writer holds that names a container offset follows it: the manifest, the WAL positions that hold `wal_start` back,
//! and the obsolete objects. Open segments hold their bytes (read into memory, or mapped from the old file, which
//! stays valid, SPEC §4.7), so readers and merges in flight are not disturbed.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use persia_format::block::Compression;
use persia_format::frame::frame_len;
use persia_format::superblock::{FIRST_FRAME_OFFSET, Superblock};
use persia_storage::container::{self, ContainerFile, Relocation, SpaceUsage};

use super::{Database, DatabaseError, Location, Manifest, snapshot_of};

/// When a [`Database`] compacts its container on its own (SPEC §4.6, §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionOptions {
    /// Whether [`Database::merge_step`] compacts when compaction is due ([`Database::compact_if_due`]).
    pub automatic: bool,
    /// Compaction is due when dead bytes reach this percentage of the file (SPEC §4.6, default 40) ...
    pub dead_space_percent: u8,
    /// ... and at least this many bytes (default 1 MiB), so a small file is not rewritten for a few dead frames.
    pub min_dead_bytes: u64,
}

impl CompactionOptions {
    /// The default `min_dead_bytes` (SPEC §14).
    pub const DEFAULT_MIN_DEAD_BYTES: u64 = 1 << 20;
}

impl Default for CompactionOptions {
    fn default() -> Self {
        Self {
            automatic: true,
            dead_space_percent: container::DEFAULT_DEAD_SPACE_PERCENT,
            min_dead_bytes: Self::DEFAULT_MIN_DEAD_BYTES,
        }
    }
}

/// What a compaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compacted {
    /// The `seq` of the commit the new file holds (one above the one before).
    pub seq: u64,
    /// The space of the old file.
    pub before: SpaceUsage,
    /// The space of the new one.
    pub after: SpaceUsage,
}

impl<F: ContainerFile + Sync + 'static> Database<F> {
    /// Rewrites the container without its dead frames (SPEC §4.6): replaced segments, superseded sidecars, old
    /// manifests and consumed WAL chunks, and the obsolete segments and sidecars no reader holds any more
    /// ([`Database::obsolete`]; those stop being tracked). The new file is renamed over the old one, so the path names
    /// a complete database at every instant. It holds the same snapshot, committed with `seq + 1`; readers and
    /// merges in flight go on unaffected. Writes wait meanwhile, since this holds the writer.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use persia_engine::db::{Database, DatabaseOptions};
    /// use persia_engine::document::Document;
    /// use persia_engine::schema::{FieldKind, SchemaBuilder};
    /// use persia_engine::wal::Durability;
    /// use persia_storage::container::Container;
    /// use persia_storage::vfs::sim::SimFs;
    ///
    /// let fs = Arc::new(SimFs::new());
    /// let container = Container::create_in(fs, "/app.persia")?;
    /// let mut db = Database::create_container(container, DatabaseOptions::default())?;
    /// db.create_collection(SchemaBuilder::new("items").field("n", FieldKind::I64 { stored: true }).build()?)?;
    /// for _ in 0..3 {
    ///     db.put("items", &Document::new("a"), Durability::Fsync)?; // replaces "a"
    ///     db.commit()?;
    /// }
    /// let done = db.compact()?;
    /// assert_eq!(done.after.dead(), 0);
    /// assert!(done.after.total < done.before.total);
    /// assert_eq!(db.snapshot().collection("items").and_then(|c| c.get("a").ok().flatten()).is_some(), true);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`DatabaseError::Poisoned`]; [`DatabaseError::Container`] if the container was not opened from a path, or
    /// the compaction fails. Before the rename, a failure leaves the old file and the database usable (a failed sync
    /// of the old file excepted, which poisons it, SPEC §4.5); a failed directory sync after it
    /// ([`container::Error::CompactedNotDurable`]) leaves the database on the new file, poisoned.
    pub fn compact(&mut self) -> Result<Compacted, DatabaseError> {
        self.usable()?;
        let before = self.space_usage()?;
        let live = self.live_below_wal();
        let base = Arc::clone(&self.manifest);
        let old = *self.wal.log().superblock();
        let mut built: Option<Result<Manifest, DatabaseError>> = None;
        let result = self.wal.log_mut().compact(
            &live,
            |relocation| {
                let next = relocated(&base, &old, relocation).and_then(|m| {
                    let bytes = m.encode()?;
                    Ok((m, bytes))
                });
                match next {
                    Ok((m, bytes)) => {
                        built = Some(Ok(m));
                        Ok(bytes)
                    }
                    Err(e) => {
                        built = Some(Err(e));
                        Err(container::Error::InvalidInput(
                            "the relocated manifest cannot be built".into(),
                        ))
                    }
                }
            },
            Compression::None,
        );
        let (relocation, failure) = match result {
            Ok(relocation) => (relocation, None),
            Err(container::Error::CompactedNotDurable { relocation, source }) => {
                let again = relocation.clone();
                let e = container::Error::CompactedNotDurable { relocation, source };
                (again, Some(e))
            }
            // The old file is in place: nothing the writer holds moved.
            Err(e) => {
                return Err(match built {
                    Some(Err(why)) => why,
                    _ => e.into(),
                });
            }
        };
        // From here the container is on the new file: everything that names an offset must follow it.
        let applied = self.follow(&relocation, built);
        if applied.is_err() || failure.is_some() {
            self.poisoned = true;
        }
        applied?;
        if let Some(e) = failure {
            return Err(e.into());
        }
        Ok(Compacted {
            seq: self.manifest.seq(),
            before,
            after: self.space_usage()?,
        })
    }

    /// [`Database::compact`] if the container was opened from a path and its dead bytes reach both thresholds of
    /// [`CompactionOptions`]; returns what it did, `None` if it was not due.
    ///
    /// # Errors
    /// As [`Database::compact`].
    pub fn compact_if_due(&mut self) -> Result<Option<Compacted>, DatabaseError> {
        self.usable()?;
        if self.wal.log().path().is_none() {
            return Ok(None);
        }
        let usage = self.space_usage()?;
        let options = self.options.compaction;
        if usage.dead() < options.min_dead_bytes || !usage.is_due(options.dead_space_percent) {
            return Ok(None);
        }
        self.compact().map(Some)
    }

    /// Moves the writer's state to the new file: the relocated manifest the compaction committed, the WAL positions
    /// and the obsolete objects; then publishes the snapshot of the new manifest.
    fn follow(
        &mut self,
        relocation: &Relocation,
        built: Option<Result<Manifest, DatabaseError>>,
    ) -> Result<(), DatabaseError> {
        let Some(Ok(next)) = built else {
            return Err(DatabaseError::Internal {
                what: "a compaction committed without building its manifest",
            });
        };
        let sb = self.wal.log().superblock();
        if (next.seq(), next.wal_start(), next.fence_token())
            != (sb.seq, sb.wal_start, sb.fence_token)
        {
            return Err(DatabaseError::Internal {
                what: "a compaction's manifest disagrees with its superblock",
            });
        }
        let moved = |offset| relocation.get(offset).map(|f| f.offset);
        self.wal
            .relocate(moved)
            .map_err(|_| DatabaseError::Internal {
                what: "a compaction did not copy a WAL chunk still needed",
            })?;
        self.merges.obsolete.relocate(moved);
        self.manifest = Arc::new(next);
        self.published
            .store(Arc::new(snapshot_of(&self.manifest, &self.collections)));
        Ok(())
    }
}

impl<F: ContainerFile + Sync> Database<F> {
    /// The frames below `wal_start` a compaction must keep: those the manifest lists, and the obsolete ones a reader
    /// may still read (SPEC §4.6, §8.5). Frames from `wal_start` on are kept anyway.
    pub(super) fn live_below_wal(&self) -> Vec<Range<u64>> {
        let wal_start = self.wal.log().superblock().wal_start;
        let listed = self
            .manifest
            .collections()
            .flat_map(|c| c.segments().iter())
            .flat_map(|s| std::iter::once(s.location).chain(s.tombstones.map(|t| t.location)));
        let pinned = self
            .merges
            .obsolete
            .all()
            .filter(|(_, referenced)| *referenced)
            .map(|(o, _)| o.location);
        listed
            .chain(pinned)
            .filter_map(|location| match location {
                Location::Frame { offset, len } => Some(offset..offset.saturating_add(len)),
                Location::Object => None,
            })
            .filter(|r| r.end <= wal_start)
            .collect()
    }
}

/// The manifest of the compacted file (SPEC §8.7.2 "Relocation"): `base` with every frame location moved where
/// `relocation` copied it, and the new `wal_start`. `old` is the superblock of the file compacted.
///
/// The new `wal_start` is where the old `wal_start` frame moved, or, with an empty WAL range, the end of the new
/// Manifest frame (SPEC §4.6 step 3). That frame follows the last frame copied below `log_end`, and its length does
/// not depend on the `wal_start` it holds (a fixed-width field), so it is known before the manifest is final.
fn relocated(
    base: &Manifest,
    old: &Superblock,
    relocation: &Relocation,
) -> Result<Manifest, DatabaseError> {
    let not_copied = DatabaseError::Internal {
        what: "a compaction did not copy a frame the manifest lists",
    };
    let mut moves = BTreeMap::new();
    for segment in base.collections().flat_map(|c| c.segments().iter()) {
        let sidecar = segment.tombstones.map(|t| t.location);
        for location in std::iter::once(segment.location).chain(sidecar) {
            if let Location::Frame { offset, .. } = location {
                let to = relocation.get(offset).ok_or(DatabaseError::Internal {
                    what: "a compaction did not copy a frame the manifest lists",
                })?;
                moves.insert(offset, to.offset);
            }
        }
    }
    let wal_start = if old.wal_start < old.log_end {
        relocation.get(old.wal_start).ok_or(not_copied)?.offset
    } else {
        let manifest_at = relocation
            .iter()
            .last()
            .map_or(FIRST_FRAME_OFFSET, |(_, new)| new.end());
        let payload = base.relocate(&moves, 0)?.encode()?;
        let len = u64::try_from(payload.len())
            .ok()
            .and_then(frame_len)
            .and_then(|len| manifest_at.checked_add(len));
        len.ok_or(DatabaseError::Internal {
            what: "a compacted manifest's frame does not fit a u64",
        })?
    };
    Ok(base.relocate(&moves, wal_start)?)
}
