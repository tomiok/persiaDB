//! Commit planning (SPEC §8.7.2): a [`ManifestDiff`] says what a commit changes, [`Manifest::apply`] checks it against
//! a base manifest and builds the next one, and [`Manifest::diff`] recovers the diff between two manifests.
//!
//! A diff names segments by id and never by position, so it can be planned against one manifest and applied to a
//! later one (a merge that ran while other commits landed): applying fails if the base no longer allows it.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    CollectionEntry, EntryProblem, Location, Manifest, SegmentEntry, SegmentId, Tombstones,
};
use crate::schema::{CollectionSchema, EvolutionError};

/// The changes of one commit (SPEC §8.7.2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManifestDiff {
    /// The committing writer's fencing token; `None` keeps the base's. Never below the base's (SPEC §11.3).
    pub fence_token: Option<u64>,
    /// The new WAL start; `None` keeps the base's. Never below the base's (SPEC §8.2).
    pub wal_start: Option<u64>,
    /// Changes by collection name.
    pub collections: BTreeMap<String, CollectionDiff>,
}

/// What a commit does to one collection.
#[derive(Debug, Clone, PartialEq)]
pub enum CollectionDiff {
    /// Adds a collection that does not exist, with its first segments (oldest first).
    Create {
        /// Its schema; its name is the key of the diff.
        schema: CollectionSchema,
        /// Its segments, oldest first.
        segments: Vec<SegmentEntry>,
    },
    /// Removes a collection and all its segments.
    Drop,
    /// Changes an existing collection.
    Change(CollectionChange),
}

/// Changes to an existing collection. Kept segments stay in their order; added ones follow, in the order given.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CollectionChange {
    /// The schema that replaces the current one: the result of [`CollectionSchema::evolve`] on it.
    pub schema: Option<CollectionSchema>,
    /// Live segments to remove (merged away), each with the sidecar version the plan read it at (0 for none).
    ///
    /// A merge drops the documents deleted at that version; if the segment has a newer sidecar by the time the
    /// diff is applied, its later deletions would come back in the merged segment, so the diff fails instead.
    pub removed: BTreeMap<SegmentId, u64>,
    /// New entries for kept segments: the next version of their tombstone sidecar, or their first; never the entry
    /// as it is.
    pub updated: Vec<SegmentEntry>,
    /// New segments, oldest first; their ids must not appear in the base manifest.
    pub added: Vec<SegmentEntry>,
}

impl CollectionChange {
    /// Whether it changes nothing.
    pub fn is_empty(&self) -> bool {
        self.schema.is_none()
            && self.removed.is_empty()
            && self.updated.is_empty()
            && self.added.is_empty()
    }
}

/// Why a diff does not apply to a manifest (SPEC §8.7.2).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ApplyError {
    /// The base's `seq` is `u64::MAX`.
    #[error("manifest seq overflows")]
    SeqOverflow,
    /// The fencing token is below the base's: a stale writer (SPEC §11.3).
    #[error("fence token {proposed} is below the current {current}")]
    FenceRegressed {
        /// The base's token.
        current: u64,
        /// The diff's token.
        proposed: u64,
    },
    /// The WAL start is below the base's (SPEC §8.2).
    #[error("wal_start {proposed} is below the current {current}")]
    WalRegressed {
        /// The base's WAL start.
        current: u64,
        /// The diff's WAL start.
        proposed: u64,
    },
    /// `Create` for a collection that exists.
    #[error("collection {name:?} already exists")]
    CollectionExists {
        /// The collection.
        name: String,
    },
    /// `Drop` or `Change` for a collection that does not exist.
    #[error("no collection {name:?}")]
    NoSuchCollection {
        /// The collection.
        name: String,
    },
    /// A schema whose name is not the collection's.
    #[error("collection {collection:?} given a schema named {schema:?}")]
    SchemaName {
        /// The collection.
        collection: String,
        /// The schema's name.
        schema: String,
    },
    /// A replacement schema that the current one cannot evolve into (SPEC §6.2.4).
    #[error("collection {collection:?}: {source}")]
    Evolution {
        /// The collection.
        collection: String,
        /// Why.
        source: EvolutionError,
    },
    /// A replacement schema that is not what [`CollectionSchema::evolve`] gives (e.g. renumbered fields).
    #[error("collection {collection:?}: the new schema is not an evolution of the current one")]
    NotEvolved {
        /// The collection.
        collection: String,
    },
    /// A removed or updated segment that is not live in the collection.
    #[error("collection {collection:?} has no live segment {id}")]
    NoSuchSegment {
        /// The collection.
        collection: String,
        /// The segment.
        id: SegmentId,
    },
    /// A segment id listed twice, added again, or already in the base manifest.
    #[error("segment {id} is listed twice or already exists")]
    DuplicateSegment {
        /// The segment.
        id: SegmentId,
    },
    /// An entry that breaks a rule of SPEC §8.7.1.
    #[error("segment {id}: {problem}")]
    Entry {
        /// The segment.
        id: SegmentId,
        /// The rule.
        problem: EntryProblem,
    },
    /// A removed segment whose sidecar changed since the diff was planned: its later deletions would be lost.
    #[error("segment {id} was planned at sidecar version {planned}, but is at {current}")]
    Conflict {
        /// The segment.
        id: SegmentId,
        /// The sidecar version the diff names (0 for none).
        planned: u64,
        /// The base's sidecar version (0 for none).
        current: u64,
    },
    /// An entry that is not a valid step from the segment's current one (SPEC §8.7.2).
    #[error("segment {id}: {problem}")]
    Update {
        /// The segment.
        id: SegmentId,
        /// What is wrong.
        problem: UpdateProblem,
    },
    /// [`Manifest::relocate`]: no frame location of the manifest is at this offset.
    #[error("no frame location at offset {offset}")]
    UnknownFrame {
        /// The offset.
        offset: u64,
    },
    /// [`Manifest::diff`]: the next manifest's segments are not the kept ones in order followed by the added ones.
    #[error("collection {collection:?}: segment order cannot result from one commit")]
    SegmentOrder {
        /// The collection.
        collection: String,
    },
}

/// Why an updated or added entry is not a valid step (SPEC §8.7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UpdateProblem {
    /// `doc_count`, `size` or the location changed; only [`Manifest::relocate`] moves a segment.
    #[error("doc_count, size and location of a segment never change in a commit")]
    SegmentChanged,
    /// A first sidecar (of a new segment, or of one that had none) whose version is not 1.
    #[error("a first tombstone sidecar has version 1")]
    FirstVersion,
    /// A sidecar removed, or a deleted count lowered.
    #[error("deletions cannot be removed")]
    DeletionsRemoved,
    /// A sidecar whose version is neither the current one nor one above it.
    #[error("a rewritten sidecar's version is one above the current")]
    VersionStep,
    /// The current sidecar version with different contents.
    #[error("a sidecar version never changes")]
    SidecarChanged,
    /// The entry as it is: an update writes a new sidecar. Two deletes planned on the same manifest would
    /// otherwise both write the next version, and the second, its deletions lost, would apply as a no-op.
    #[error("an update writes the next tombstone sidecar")]
    Unchanged,
}

impl Manifest {
    /// The manifest that commit `diff` publishes on top of `self`, with `seq` one above (SPEC §8.7.2).
    ///
    /// The result is not bounded in size: one past [`super::MAX_MANIFEST_LEN`] is found by [`Manifest::encode`].
    ///
    /// # Errors
    /// [`ApplyError`] for the first rule `diff` breaks against `self`; `self` is unchanged.
    pub fn apply(&self, diff: &ManifestDiff) -> Result<Manifest, ApplyError> {
        let seq = self.seq.checked_add(1).ok_or(ApplyError::SeqOverflow)?;
        let fence_token = diff.fence_token.unwrap_or(self.fence_token);
        if fence_token < self.fence_token {
            return Err(ApplyError::FenceRegressed {
                current: self.fence_token,
                proposed: fence_token,
            });
        }
        let wal_start = diff.wal_start.unwrap_or(self.wal_start);
        if wal_start < self.wal_start {
            return Err(ApplyError::WalRegressed {
                current: self.wal_start,
                proposed: wal_start,
            });
        }
        let base_ids: BTreeSet<SegmentId> = self.segment_ids().collect();
        let mut collections = self.collections.clone();
        for (name, change) in &diff.collections {
            match change {
                CollectionDiff::Create { schema, segments } => {
                    if collections.contains_key(name) {
                        return Err(ApplyError::CollectionExists { name: name.clone() });
                    }
                    check_schema_name(name, schema)?;
                    for segment in segments {
                        check_new(segment, &base_ids)?;
                    }
                    let entry = CollectionEntry {
                        schema: schema.clone(),
                        segments: segments.clone(),
                    };
                    collections.insert(name.clone(), entry);
                }
                CollectionDiff::Drop => {
                    if collections.remove(name).is_none() {
                        return Err(ApplyError::NoSuchCollection { name: name.clone() });
                    }
                }
                CollectionDiff::Change(change) => {
                    let entry = collections
                        .get_mut(name)
                        .ok_or_else(|| ApplyError::NoSuchCollection { name: name.clone() })?;
                    apply_change(name, entry, change, &base_ids)?;
                }
            }
        }
        let next = Manifest {
            seq,
            fence_token,
            wal_start,
            collections,
        };
        let mut seen = BTreeSet::new();
        if let Some(id) = next.segment_ids().find(|id| !seen.insert(*id)) {
            return Err(ApplyError::DuplicateSegment { id });
        }
        Ok(next)
    }

    /// The diff that [`Self::apply`] turns `self` into `next` with, `seq` aside (SPEC §8.7.2).
    ///
    /// # Errors
    /// [`ApplyError`] if no single commit leads from `self` to `next`: the rule its diff breaks, or
    /// [`ApplyError::SegmentOrder`].
    pub fn diff(&self, next: &Manifest) -> Result<ManifestDiff, ApplyError> {
        let mut collections = BTreeMap::new();
        for name in self.collections.keys() {
            if !next.collections.contains_key(name) {
                collections.insert(name.clone(), CollectionDiff::Drop);
            }
        }
        for (name, new) in &next.collections {
            match self.collections.get(name) {
                None => {
                    let create = CollectionDiff::Create {
                        schema: new.schema.clone(),
                        segments: new.segments.clone(),
                    };
                    collections.insert(name.clone(), create);
                }
                Some(old) => {
                    let change = collection_change(old, new);
                    if !change.is_empty() {
                        collections.insert(name.clone(), CollectionDiff::Change(change));
                    }
                }
            }
        }
        let diff = ManifestDiff {
            fence_token: (next.fence_token != self.fence_token).then_some(next.fence_token),
            wal_start: (next.wal_start != self.wal_start).then_some(next.wal_start),
            collections,
        };
        let applied = self.apply(&diff)?;
        if let Some((name, _)) = next
            .collections
            .iter()
            .find(|(name, entry)| applied.collections.get(*name) != Some(*entry))
        {
            return Err(ApplyError::SegmentOrder {
                collection: name.clone(),
            });
        }
        Ok(diff)
    }

    /// The manifest a container compaction publishes (SPEC §4.6): every frame location at an offset in `moves` is
    /// moved to the mapped offset (frames are copied whole, so lengths stay), `wal_start` is set, `seq` goes one up,
    /// and nothing else changes. Not a diff: compaction moves frames down, so `wal_start` may go below the
    /// current one, which no commit may do.
    ///
    /// # Errors
    /// [`ApplyError::UnknownFrame`] for an offset of `moves` that no location holds, [`ApplyError::Entry`] for a
    /// target that is not a valid frame location, [`ApplyError::SeqOverflow`].
    pub fn relocate(
        &self,
        moves: &BTreeMap<u64, u64>,
        wal_start: u64,
    ) -> Result<Manifest, ApplyError> {
        let seq = self.seq.checked_add(1).ok_or(ApplyError::SeqOverflow)?;
        let mut used = BTreeSet::new();
        let mut collections = self.collections.clone();
        for segment in collections.values_mut().flat_map(|c| c.segments.iter_mut()) {
            let mut entry = *segment;
            entry.location = relocated(segment.location, moves, &mut used);
            if let Some(t) = &mut entry.tombstones {
                t.location = relocated(t.location, moves, &mut used);
            }
            check_entry(&entry)?;
            *segment = entry;
        }
        if let Some(offset) = moves.keys().find(|o| !used.contains(*o)) {
            return Err(ApplyError::UnknownFrame { offset: *offset });
        }
        Ok(Manifest {
            seq,
            fence_token: self.fence_token,
            wal_start,
            collections,
        })
    }

    fn segment_ids(&self) -> impl Iterator<Item = SegmentId> + '_ {
        self.collections
            .values()
            .flat_map(|c| c.segments.iter().map(|s| s.id))
    }
}

fn relocated(location: Location, moves: &BTreeMap<u64, u64>, used: &mut BTreeSet<u64>) -> Location {
    match location {
        Location::Frame { offset, len } => match moves.get(&offset) {
            Some(to) => {
                used.insert(offset);
                Location::Frame { offset: *to, len }
            }
            None => location,
        },
        Location::Object => location,
    }
}

/// The sidecar version of an entry, 0 for none.
fn sidecar_version(segment: &SegmentEntry) -> u64 {
    segment.tombstones.map_or(0, |t| t.version)
}

fn collection_change(old: &CollectionEntry, new: &CollectionEntry) -> CollectionChange {
    let kept: BTreeSet<SegmentId> = new.segments.iter().map(|s| s.id).collect();
    let mut change = CollectionChange {
        schema: (new.schema != old.schema).then(|| new.schema.clone()),
        removed: old
            .segments
            .iter()
            .filter(|s| !kept.contains(&s.id))
            .map(|s| (s.id, sidecar_version(s)))
            .collect(),
        ..CollectionChange::default()
    };
    for segment in &new.segments {
        match old.segment(segment.id) {
            None => change.added.push(*segment),
            Some(before) if before != segment => change.updated.push(*segment),
            Some(_) => {}
        }
    }
    change
}

fn apply_change(
    name: &str,
    entry: &mut CollectionEntry,
    change: &CollectionChange,
    base_ids: &BTreeSet<SegmentId>,
) -> Result<(), ApplyError> {
    if let Some(schema) = &change.schema {
        check_schema_name(name, schema)?;
        let evolved = entry
            .schema
            .evolve(schema)
            .map_err(|source| ApplyError::Evolution {
                collection: name.to_owned(),
                source,
            })?;
        if evolved != *schema {
            return Err(ApplyError::NotEvolved {
                collection: name.to_owned(),
            });
        }
    }
    let no_such = |id| ApplyError::NoSuchSegment {
        collection: name.to_owned(),
        id,
    };
    for (id, planned) in &change.removed {
        let segment = entry.segment(*id).ok_or_else(|| no_such(*id))?;
        let current = sidecar_version(segment);
        if current != *planned {
            return Err(ApplyError::Conflict {
                id: *id,
                planned: *planned,
                current,
            });
        }
    }
    let mut updates = BTreeMap::new();
    for update in &change.updated {
        if change.removed.contains_key(&update.id) || updates.insert(update.id, update).is_some() {
            return Err(ApplyError::DuplicateSegment { id: update.id });
        }
    }
    let mut segments = Vec::with_capacity(entry.segments.len());
    for segment in &entry.segments {
        if change.removed.contains_key(&segment.id) {
            continue;
        }
        match updates.remove(&segment.id) {
            Some(update) => {
                check_update(segment, update)?;
                segments.push(*update);
            }
            None => segments.push(*segment),
        }
    }
    if let Some(id) = updates.keys().next() {
        return Err(no_such(*id));
    }
    for segment in &change.added {
        check_new(segment, base_ids)?;
        segments.push(*segment);
    }
    if let Some(schema) = &change.schema {
        entry.schema = schema.clone();
    }
    entry.segments = segments;
    Ok(())
}

fn check_schema_name(name: &str, schema: &CollectionSchema) -> Result<(), ApplyError> {
    if schema.name() == name {
        Ok(())
    } else {
        Err(ApplyError::SchemaName {
            collection: name.to_owned(),
            schema: schema.name().to_owned(),
        })
    }
}

fn check_entry(segment: &SegmentEntry) -> Result<(), ApplyError> {
    segment.check().map_err(|problem| ApplyError::Entry {
        id: segment.id,
        problem,
    })
}

/// A segment a commit adds: an id not in the base, and a first tombstone sidecar if any.
fn check_new(segment: &SegmentEntry, base_ids: &BTreeSet<SegmentId>) -> Result<(), ApplyError> {
    check_entry(segment)?;
    if base_ids.contains(&segment.id) {
        return Err(ApplyError::DuplicateSegment { id: segment.id });
    }
    if segment.tombstones.is_some_and(|t| t.version != 1) {
        return Err(ApplyError::Update {
            id: segment.id,
            problem: UpdateProblem::FirstVersion,
        });
    }
    Ok(())
}

/// A kept segment's new entry: the same segment where it was, with the first or the next sidecar.
fn check_update(old: &SegmentEntry, new: &SegmentEntry) -> Result<(), ApplyError> {
    check_entry(new)?;
    let step = if (old.doc_count, old.size, old.location) == (new.doc_count, new.size, new.location)
    {
        match (old.tombstones, new.tombstones) {
            (None, None) => Err(UpdateProblem::Unchanged),
            (None, Some(t)) if t.version == 1 => Ok(()),
            (None, Some(_)) => Err(UpdateProblem::FirstVersion),
            (Some(_), None) => Err(UpdateProblem::DeletionsRemoved),
            (Some(o), Some(n)) => check_sidecar_step(o, n),
        }
    } else {
        Err(UpdateProblem::SegmentChanged)
    };
    step.map_err(|problem| ApplyError::Update {
        id: new.id,
        problem,
    })
}

fn check_sidecar_step(old: Tombstones, new: Tombstones) -> Result<(), UpdateProblem> {
    if new.version == old.version {
        if new == old {
            Err(UpdateProblem::Unchanged)
        } else {
            Err(UpdateProblem::SidecarChanged)
        }
    } else if Some(new.version) != old.version.checked_add(1) {
        Err(UpdateProblem::VersionStep)
    } else if new.deleted < old.deleted {
        Err(UpdateProblem::DeletionsRemoved)
    } else {
        Ok(())
    }
}
