//! Compaction: which segments to merge, merging them, and running merges in the background (SPEC §5.5, §8.5,
//! ADR-0031, ADR-0032, ROADMAP 6.5).

mod merger;
mod policy;
pub(crate) mod scheduler;

pub use merger::{MergeError, MergeSource, MergedSegment, merge, merge_until};
pub use policy::{MergeInput, MergePlan, MergePolicy, MergeReason};
pub use scheduler::MergeOptions;
