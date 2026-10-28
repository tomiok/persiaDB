//! The container's logical objects behind the [`Storage`] trait (SPEC §3 principle 2, §10.1, ROADMAP 6.3.6).
//!
//! A container stores frames, not names: what a frame is follows from the committed manifest, which locates every
//! segment and tombstone sidecar, and from the superblock, which locates the manifest and the WAL. The view names
//! them with the keys of the remote layout (SPEC §10.4), so code that reads objects reads them the same way from a
//! container and from an object store:
//!
//! | Key | Object |
//! |---|---|
//! | `manifests/<seq:020>.mf` | the committed Manifest frame |
//! | `segments/<collection>/<segment_id>.seg` | a live segment's frame |
//! | `tombstones/<collection>/<segment_id>.<version>.tomb` | a live segment's current sidecar frame |
//! | `wal/<frame_seq:020>.wal` | a `WalChunk` frame from the committed `wal_start` on |
//!
//! The view is read-only. Writes go through the commit protocol (SPEC §4.5), which appends frames and then writes a
//! superblock slot: a frame has no name to `put`, and a commit is not a `put_if_absent` of one object. `put`,
//! `put_if_absent` and `delete` are [`Error::Unsupported`], and [`Storage::capabilities`] claims nothing.
//!
//! Reads fetch and check the whole frame, also for a range of it, and `list` reads the frames of the manifest, the
//! sidecars and the WAL chunks to report their payload lengths: the view serves opens and tools, not hot paths.
//!
//! Every read checks the frame against what the manifest says it is (SPEC §8.7.1, ROADMAP 6.3.7): a frame of the
//! right kind and length, entirely below the committed `log_end`. Anything else is [`Error::Corrupt`].

use std::collections::BTreeMap;
use std::io;
use std::ops::Range;

use bytes::Bytes;
use persia_format::frame::FrameKind;
use persia_format::source::ByteSource;
use persia_format::{Corruption, Error as FormatError};
use persia_storage::container::{self, Container, ContainerFile, FrameRef};
use persia_storage::{
    Capabilities, Error, Key, KeyPrefix, ObjectMeta, Op, PutOutcome, Result, Storage,
};

use crate::manifest::{Location, MAX_MANIFEST_LEN, Manifest, SegmentEntry};
use crate::wal::MAX_CHUNK_LEN;

/// Largest payload a tombstone sidecar frame may hold: its layout is fixed by ROADMAP 6.4.2, and a full set of
/// deleted docids of a segment of `2^31` documents takes at most `2^28` bytes plus container headers.
const MAX_SIDECAR_LEN: usize = (1 << 28) + (1 << 20);

/// What a key names.
#[derive(Debug, Clone, Copy)]
enum Target {
    Manifest { offset: u64, len: u64 },
    Segment(SegmentEntry),
    Tombstones { offset: u64, len: u64 },
    Wal(FrameRef),
}

/// A read-only [`Storage`] over the committed objects of a container (see the module docs).
///
/// ```
/// use persia_engine::db::ContainerObjects;
/// use persia_engine::manifest::Manifest;
/// use persia_format::block::Compression;
/// use persia_format::frame::FrameKind;
/// use persia_storage::container::sim::SimFile;
/// use persia_storage::container::{Commit, Container};
/// use persia_storage::{Key, KeyPrefix, Storage};
///
/// let mut c = Container::create_on(SimFile::new(), [1; 16])?;
/// let manifest = Manifest::empty().apply(&Default::default())?;
/// let frame = c.append(FrameKind::Manifest, &manifest.encode()?, Compression::None)?;
/// c.commit(Commit { manifest: frame, wal_start: c.end(), fence_token: 0 })?;
///
/// let objects = ContainerObjects::new(&c, &manifest);
/// let key = Key::new("manifests/00000000000000000001.mf")?;
/// assert_eq!(objects.list(&KeyPrefix::new("")?)?.len(), 1);
/// assert_eq!(Manifest::decode(&objects.get(&key)?)?, manifest);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct ContainerObjects<'a, F> {
    container: &'a Container<F>,
    objects: BTreeMap<String, Target>,
}

/// The key of a segment (SPEC §10.4).
pub(crate) fn segment_key(collection: &str, entry: &SegmentEntry) -> String {
    format!("segments/{collection}/{}.seg", entry.id)
}

impl<'a, F: ContainerFile> ContainerObjects<'a, F> {
    /// The objects of `container` as `manifest` describes them. `manifest` must be the one the container's
    /// superblock commits ([`super::Database::objects`] passes it): another one gives a view whose keys disagree with
    /// the frames. With no commit yet (`seq` 0) there is no manifest object.
    pub fn new(container: &'a Container<F>, manifest: &Manifest) -> Self {
        let sb = container.superblock();
        let mut objects = BTreeMap::new();
        if sb.seq != 0 {
            let manifest = Target::Manifest {
                offset: sb.manifest_off,
                len: sb.manifest_len,
            };
            objects.insert(format!("manifests/{:020}.mf", sb.seq), manifest);
        }
        for collection in manifest.collections() {
            let name = collection.name();
            for entry in collection.segments() {
                objects.insert(segment_key(name, entry), Target::Segment(*entry));
                if let Some(t) = entry.tombstones {
                    // An object location here is caught by `Manifest::check_layout`; the view then has no sidecar.
                    if let Location::Frame { offset, len } = t.location {
                        let key = format!("tombstones/{name}/{}.{}.tomb", entry.id, t.version);
                        objects.insert(key, Target::Tombstones { offset, len });
                    }
                }
            }
        }
        for frame in container.frames() {
            if frame.kind == FrameKind::WalChunk && frame.end() <= sb.log_end {
                objects.insert(
                    format!("wal/{:020}.wal", frame.frame_seq),
                    Target::Wal(*frame),
                );
            }
        }
        Self { container, objects }
    }

    fn target(&self, key: &Key) -> Result<Target> {
        self.objects
            .get(key.as_str())
            .copied()
            .ok_or_else(|| Error::NotFound { key: key.clone() })
    }

    /// The payload of the object `key` names, after checking its frame.
    fn payload(&self, key: &Key, op: Op) -> Result<ByteSource> {
        let (kind, offset, len, max) = match self.target(key)? {
            Target::Manifest { offset, len } => {
                (FrameKind::Manifest, offset, len, MAX_MANIFEST_LEN)
            }
            Target::Segment(entry) => {
                let Location::Frame { offset, len } = entry.location else {
                    return Err(wrong_frame(key, FrameKind::Segment, 0, 0, None));
                };
                // A payload longer than `usize` cannot be read anyway; the frame check below reports it.
                let max = usize::try_from(entry.size).unwrap_or(usize::MAX);
                (FrameKind::Segment, offset, len, max)
            }
            Target::Tombstones { offset, len } => {
                (FrameKind::TombstoneSidecar, offset, len, MAX_SIDECAR_LEN)
            }
            Target::Wal(frame) => (FrameKind::WalChunk, frame.offset, frame.len, MAX_CHUNK_LEN),
        };
        let log_end = self.container.superblock().log_end;
        if offset.checked_add(len).is_none_or(|end| end > log_end) {
            return Err(wrong_frame(key, kind, offset, len, None));
        }
        let read = self
            .container
            .read_frame_source(offset, max)
            .map_err(|e| container_error(key, op, e))?;
        if read.frame.kind != kind || read.frame.len != len {
            return Err(wrong_frame(key, kind, offset, len, Some(read.frame)));
        }
        if let Target::Segment(entry) = self.target(key)?
            && u64::try_from(read.payload.len()).ok() != Some(entry.size)
        {
            return Err(Error::ShortRead {
                key: key.clone(),
                requested: 0..entry.size,
                returned: u64::try_from(read.payload.len()).unwrap_or(u64::MAX),
            });
        }
        Ok(read.payload)
    }
}

/// [`Corruption::WrongFrame`] for a location that names `found` (or no frame below `log_end`) instead of a frame of
/// `kind` and `len` bytes.
fn wrong_frame(
    key: &Key,
    kind: FrameKind,
    offset: u64,
    len: u64,
    found: Option<FrameRef>,
) -> Error {
    Error::Corrupt {
        key: key.clone(),
        source: FormatError::Corrupt {
            offset,
            reason: Corruption::WrongFrame {
                expected_kind: kind.to_u8(),
                expected_len: len,
                found_kind: found.map_or(0, |f| f.kind.to_u8()),
                found_len: found.map_or(0, |f| f.len),
            },
        },
    }
}

fn container_error(key: &Key, op: Op, e: container::Error) -> Error {
    match e {
        container::Error::Format(source) => Error::Corrupt {
            key: key.clone(),
            source,
        },
        container::Error::Io { source, .. } => Error::Io {
            op,
            target: key.as_str().to_owned(),
            source,
        },
        other => Error::Io {
            op,
            target: key.as_str().to_owned(),
            source: io::Error::other(other.to_string()),
        },
    }
}

impl<F: ContainerFile + Sync> Storage for ContainerObjects<'_, F> {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        persia_storage::check_range(key, &range)?;
        let payload = self.payload(key, Op::GetRange)?;
        let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        if range.end > len {
            return Err(Error::ShortRead {
                key: key.clone(),
                requested: range.clone(),
                returned: len.saturating_sub(range.start),
            });
        }
        let base = payload.base_offset();
        // Both ends are within the payload: checked above.
        let slice = payload
            .slice(base.saturating_add(range.start)..base.saturating_add(range.end))
            .map_err(|source| Error::Corrupt {
                key: key.clone(),
                source,
            })?;
        persia_storage::range_source(key, range, slice.into_bytes())
    }

    fn get(&self, key: &Key) -> Result<Bytes> {
        Ok(self.payload(key, Op::Get)?.into_bytes())
    }

    fn put(&self, _: &Key, _: Bytes) -> Result<()> {
        Err(Error::Unsupported { op: Op::Put })
    }

    fn put_if_absent(&self, _: &Key, _: Bytes) -> Result<PutOutcome> {
        Err(Error::Unsupported {
            op: Op::PutIfAbsent,
        })
    }

    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        let mut out = Vec::new();
        for (name, target) in self
            .objects
            .range(prefix.as_str().to_owned()..)
            .take_while(|(name, _)| name.starts_with(prefix.as_str()))
        {
            let key = Key::new(name.as_str())?;
            let size = match target {
                Target::Segment(entry) => entry.size,
                // The payload length is in the frame header; reading the frame also checks it.
                _ => u64::try_from(self.payload(&key, Op::List)?.len()).unwrap_or(u64::MAX),
            };
            out.push(ObjectMeta { key, size });
        }
        Ok(out)
    }

    fn delete(&self, _: &Key) -> Result<()> {
        Err(Error::Unsupported { op: Op::Delete })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
}
