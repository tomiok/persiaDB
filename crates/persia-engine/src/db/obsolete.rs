//! Objects a commit stopped listing, kept until no snapshot can read them (SPEC §8.5, ADR-0032, ROADMAP 6.5.4).
//!
//! A merge commit removes its inputs and their sidecars from the manifest, and a commit that deletes from a segment
//! supersedes its sidecar. Older snapshots may still read them, so they are *obsolete*, not yet garbage. Each one is
//! tracked with a weak reference to what readers hold of it (the open [`Segment`], the decoded [`Tombstones`]):
//! every snapshot, merge job and reader that got it from one holds a strong reference, so once the weak one no
//! longer upgrades, no reader can reach the object again and it may be deleted. Counting references, rather than
//! comparing snapshot `seq`s with an epoch, frees an object as soon as its last reader is gone, whatever older
//! snapshots are still held for other reasons.

use std::sync::{Arc, Weak};

use crate::manifest::{Location, SegmentId};
use crate::segment::{Segment, Tombstones};

/// What an obsolete object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsoleteKind {
    /// A segment a merge removed.
    Segment(SegmentId),
    /// A segment's tombstone sidecar at `version`: superseded by the next version, or removed with its segment.
    Sidecar {
        /// The segment.
        segment: SegmentId,
        /// The sidecar's version.
        version: u64,
    },
}

/// An object no commit since `obsolete_since` lists (SPEC §8.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObsoleteObject {
    /// Its collection.
    pub collection: String,
    /// What it is.
    pub kind: ObsoleteKind,
    /// Where it is stored, as the last manifest that listed it said.
    pub location: Location,
    /// The `seq` of the commit that stopped listing it: snapshots from this one on cannot read it.
    pub obsolete_since: u64,
}

/// What readers hold of an obsolete object.
#[derive(Debug)]
pub(crate) enum Pin {
    Segment(Weak<Segment>),
    Sidecar(Weak<Tombstones>),
}

impl Pin {
    pub fn segment(segment: &Arc<Segment>) -> Self {
        Self::Segment(Arc::downgrade(segment))
    }

    pub fn sidecar(tombstones: &Arc<Tombstones>) -> Self {
        Self::Sidecar(Arc::downgrade(tombstones))
    }

    fn referenced(&self) -> bool {
        match self {
            Self::Segment(w) => w.strong_count() > 0,
            Self::Sidecar(w) => w.strong_count() > 0,
        }
    }
}

/// The obsolete objects not yet handed out for deletion, oldest first.
#[derive(Debug, Default)]
pub(crate) struct Obsolete {
    objects: Vec<(ObsoleteObject, Pin)>,
}

impl Obsolete {
    /// Records that the commit `seq` stopped listing an object, which readers hold through `pin`.
    pub fn retire(
        &mut self,
        collection: &str,
        kind: ObsoleteKind,
        location: Location,
        seq: u64,
        pin: Pin,
    ) {
        let object = ObsoleteObject {
            collection: collection.to_owned(),
            kind,
            location,
            obsolete_since: seq,
        };
        self.objects.push((object, pin));
    }

    /// Every obsolete object not yet taken, and whether a reader may still read it.
    pub fn all(&self) -> impl Iterator<Item = (&ObsoleteObject, bool)> {
        self.objects.iter().map(|(o, pin)| (o, pin.referenced()))
    }

    /// Removes and returns the objects no reader can read any more.
    pub fn take_unreferenced(&mut self) -> Vec<ObsoleteObject> {
        let (free, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.objects)
            .into_iter()
            .partition(|(_, pin)| !pin.referenced());
        self.objects = kept;
        free.into_iter().map(|(o, _)| o).collect()
    }
}
