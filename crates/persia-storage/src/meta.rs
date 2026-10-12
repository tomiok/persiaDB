//! Values returned by [`Storage`](crate::Storage) operations (SPEC §10.1).

use crate::key::Key;

/// One object found by [`Storage::list`](crate::Storage::list).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectMeta {
    /// The object's key.
    pub key: Key,
    /// The object's length in bytes.
    pub size: u64,
}

/// Result of [`Storage::put_if_absent`](crate::Storage::put_if_absent), the commit primitive (SPEC §10.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "AlreadyExists means another writer won: the data was not written"]
pub enum PutOutcome {
    /// The object did not exist and now holds the data.
    Created,
    /// An object already existed under the key and was left unchanged.
    AlreadyExists,
}

/// What a backend guarantees beyond the base contract of [`Storage`](crate::Storage) (SPEC §10.1).
///
/// The default is the weakest backend: no guarantees.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[allow(clippy::struct_excessive_bools)] // independent flags, like a feature set
pub struct Capabilities {
    /// `put_if_absent` is atomic. Without it the backend returns `Unsupported` and cannot host the remote commit
    /// protocol (SPEC §10.4).
    pub conditional_put: bool,
    /// `list` sees every object whose `put` has returned (read-after-write listing).
    pub strong_list: bool,
    /// Objects can be appended to in place (e.g. a local file), so a WAL need not be one object per batch.
    pub append: bool,
}
