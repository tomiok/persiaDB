//! Compaction: which segments to merge, and merging them (SPEC §5.5, §8.5, ADR-0031, ROADMAP 6.5).

mod policy;

pub use policy::{MergeInput, MergePlan, MergePolicy, MergeReason};
