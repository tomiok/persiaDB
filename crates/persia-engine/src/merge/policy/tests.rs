use std::collections::HashSet;

use proptest::prelude::*;

use super::{MergeInput, MergePlan, MergePolicy, MergeReason};
use crate::manifest::{Location, SegmentEntry, SegmentId, Tombstones};

const MIB: u64 = 1 << 20;

/// No plans.
const NONE: [(MergeReason, Vec<u128>); 0] = [];

/// A segment `id` of `doc_count` documents and `size` bytes, `deleted` of them deleted (at sidecar version 3).
fn entry(id: u128, doc_count: u32, size: u64, deleted: u32) -> SegmentEntry {
    SegmentEntry {
        id: SegmentId::new(id),
        doc_count,
        size,
        location: Location::Object,
        tombstones: (deleted > 0).then_some(Tombstones {
            version: 3,
            deleted,
            location: Location::Object,
        }),
    }
}

fn flush(id: u128, size: u64) -> SegmentEntry {
    entry(id, 1_000, size, 0)
}

fn ids(plan: &MergePlan) -> Vec<u128> {
    plan.inputs.iter().map(|i| i.id.get()).collect()
}

fn summary(plans: &[MergePlan]) -> Vec<(MergeReason, Vec<u128>)> {
    plans.iter().map(|p| (p.reason, ids(p))).collect()
}

fn plan(policy: &MergePolicy, segments: &[SegmentEntry]) -> Vec<(MergeReason, Vec<u128>)> {
    summary(&policy.plan(segments, &HashSet::new()))
}

/// A policy with small numbers: tier 0 below 100 bytes, tier 1 below 1000, and so on.
fn small() -> MergePolicy {
    MergePolicy {
        tier_floor: 10,
        tier_factor: 10,
        segments_per_merge: 4,
        deleted_percent: 20,
        max_merged_bytes: 10_000,
        max_merged_docs: 1 << 20,
    }
}

#[test]
fn defaults_are_the_spec_values() {
    let p = MergePolicy::default();
    assert_eq!(
        (
            p.tier_floor,
            p.tier_factor,
            p.segments_per_merge,
            p.deleted_percent
        ),
        (2 * MIB, 10, 8, 20)
    );
    assert_eq!((p.max_merged_bytes, p.max_merged_docs), (5 << 30, 1 << 31));
}

#[test]
fn no_segments_no_plans() {
    assert_eq!(plan(&MergePolicy::default(), &[]), NONE);
}

#[test]
fn a_tier_merges_once_it_has_enough_segments() {
    let p = MergePolicy::default();
    let seven: Vec<_> = (1..=7).map(|id| flush(id, MIB)).collect();
    assert_eq!(plan(&p, &seven), NONE);
    let eight: Vec<_> = (1..=8).map(|id| flush(id, MIB)).collect();
    assert_eq!(
        plan(&p, &eight),
        [(MergeReason::Tier(0), (1..=8).collect::<Vec<_>>())]
    );
}

#[test]
fn a_tier_merges_its_smallest_segments_in_manifest_order() {
    // Sizes 9, 8, ..., 1 bytes × 10: the four smallest are the last four, listed in manifest order.
    let segments: Vec<_> = (1..=9)
        .map(|id| flush(id, u64::try_from(10 - id).unwrap() * 10))
        .collect();
    let mut p = small();
    p.tier_floor = 1_000;
    assert_eq!(
        plan(&p, &segments),
        [
            (MergeReason::Tier(0), vec![6, 7, 8, 9]),
            (MergeReason::Tier(0), vec![2, 3, 4, 5]),
        ]
    );
}

#[test]
fn equal_sizes_break_ties_by_manifest_order() {
    let segments: Vec<_> = (1..=5).map(|id| flush(id, 50)).collect();
    assert_eq!(
        plan(&small(), &segments),
        [(MergeReason::Tier(0), vec![1, 2, 3, 4])]
    );
}

#[test]
fn tiers_plan_from_the_lowest_up() {
    // Tier 1 (100..1000 bytes) listed first, tier 0 after.
    let mut segments: Vec<_> = (1..=4).map(|id| flush(id, 500)).collect();
    segments.extend((5..=8).map(|id| flush(id, 50)));
    assert_eq!(
        plan(&small(), &segments),
        [
            (MergeReason::Tier(0), vec![5, 6, 7, 8]),
            (MergeReason::Tier(1), vec![1, 2, 3, 4]),
        ]
    );
}

#[test]
fn tier_boundaries() {
    let p = small();
    for (size, tier) in [
        (0, 0),
        (1, 0),
        (99, 0),
        (100, 1),
        (999, 1),
        (1_000, 2),
        (9_999, 2),
        (10_000, 3),
        (u64::MAX, 18),
    ] {
        assert_eq!(p.tier_of(size), tier, "live size {size}");
    }
    assert_eq!(MergePolicy::default().tier_of(20 * MIB - 1), 0);
    assert_eq!(MergePolicy::default().tier_of(20 * MIB), 1);
}

#[test]
fn segments_of_one_tier_split_by_tier_not_by_count() {
    // Three in tier 0 and three in tier 1: neither tier has four.
    let mut segments: Vec<_> = (1..=3).map(|id| flush(id, 50)).collect();
    segments.extend((4..=6).map(|id| flush(id, 500)));
    assert_eq!(plan(&small(), &segments), NONE);
}

#[test]
fn busy_segments_are_left_out() {
    let segments: Vec<_> = (1..=5).map(|id| flush(id, 50)).collect();
    let busy: HashSet<_> = [SegmentId::new(2)].into();
    assert_eq!(
        summary(&small().plan(&segments, &busy)),
        [(MergeReason::Tier(0), vec![1, 3, 4, 5])]
    );
    let busy: HashSet<_> = [SegmentId::new(2), SegmentId::new(3)].into();
    assert_eq!(summary(&small().plan(&segments, &busy)), NONE);
    // A busy segment is not dropped or rewritten either.
    let dead = [entry(1, 10, 50, 10), entry(2, 10, 50, 5)];
    let busy: HashSet<_> = [SegmentId::new(1), SegmentId::new(2)].into();
    assert_eq!(summary(&small().plan(&dead, &busy)), NONE);
}

#[test]
fn fully_deleted_segments_are_dropped_together() {
    let segments = [
        entry(1, 10, 50, 10),
        flush(2, 50),
        entry(3, 1, 9_000_000, 1),
    ];
    assert_eq!(plan(&small(), &segments), [(MergeReason::Drop, vec![1, 3])]);
}

#[test]
fn deletion_rewrites_start_above_the_ratio() {
    let p = small();
    // 20% exactly stays; one more document deleted is above it.
    assert_eq!(plan(&p, &[entry(1, 100, 50, 20)]), NONE);
    assert_eq!(
        plan(&p, &[entry(1, 100, 50, 21)]),
        [(MergeReason::Deletes, vec![1])]
    );
    let mut none = p;
    none.deleted_percent = 100;
    assert_eq!(plan(&none, &[entry(1, 100, 50, 99)]), NONE);
    let mut any = p;
    any.deleted_percent = 0;
    assert_eq!(
        plan(&any, &[entry(1, 100, 50, 1)]),
        [(MergeReason::Deletes, vec![1])]
    );
}

#[test]
fn rewrites_follow_merges_in_manifest_order_and_skip_merged_segments() {
    let segments = [
        entry(1, 100, 5_000, 50),
        entry(2, 100, 50, 50),
        flush(3, 50),
        flush(4, 50),
        flush(5, 50),
        entry(6, 100, 50_000, 30),
    ];
    // 2 merges in tier 0 with 3, 4, 5; 1 (live 2500, tier 2) and 6 (live 35000, above the cap) are rewritten.
    assert_eq!(
        plan(&small(), &segments),
        [
            (MergeReason::Tier(0), vec![2, 3, 4, 5]),
            (MergeReason::Deletes, vec![1]),
            (MergeReason::Deletes, vec![6]),
        ]
    );
}

#[test]
fn deletions_lower_a_segments_tier() {
    // 90% deleted: 500 bytes count as 50, tier 0 with the small flushes.
    let segments = [
        entry(1, 100, 500, 90),
        flush(2, 50),
        flush(3, 50),
        flush(4, 50),
    ];
    assert_eq!(
        plan(&small(), &segments),
        [(MergeReason::Tier(0), vec![1, 2, 3, 4])]
    );
}

#[test]
fn the_byte_cap_stops_a_tier_and_excludes_large_segments() {
    let mut p = small();
    // Four tier-2 segments of 2600 live bytes: 10 400 is above the cap.
    let segments: Vec<_> = (1..=4).map(|id| flush(id, 2_600)).collect();
    assert_eq!(plan(&p, &segments), NONE);
    // At the cap: fine.
    let segments: Vec<_> = (1..=4).map(|id| flush(id, 2_500)).collect();
    assert_eq!(
        plan(&p, &segments),
        [(MergeReason::Tier(2), vec![1, 2, 3, 4])]
    );
    // A segment at the cap takes no part in tier merges.
    p.max_merged_bytes = 2_500;
    assert_eq!(plan(&p, &segments), NONE);
    // Once a group is over the cap, larger ones in the same tier are not tried.
    let mut p = small();
    p.max_merged_bytes = 4_100;
    let segments: Vec<_> = [1_000, 1_000, 1_000, 1_200, 1_000, 1_000, 1_000, 1_000]
        .iter()
        .zip(1..)
        .map(|(&size, id)| flush(id, size))
        .collect();
    assert_eq!(
        plan(&p, &segments),
        [(MergeReason::Tier(2), vec![1, 2, 3, 5])]
    );
}

#[test]
fn the_doc_cap_stops_a_tier() {
    let mut p = small();
    p.max_merged_docs = 3_999;
    let segments: Vec<_> = (1..=4).map(|id| flush(id, 50)).collect();
    assert_eq!(plan(&p, &segments), NONE);
    p.max_merged_docs = 4_000;
    assert_eq!(
        plan(&p, &segments),
        [(MergeReason::Tier(0), vec![1, 2, 3, 4])]
    );
}

#[test]
fn nonsense_knobs_are_raised() {
    let p = MergePolicy {
        tier_floor: 0,
        tier_factor: 0,
        segments_per_merge: 0,
        deleted_percent: 20,
        max_merged_bytes: u64::MAX,
        max_merged_docs: u32::MAX,
    };
    // Floor 1, factor 2: tier 0 below 2 bytes, tier 1 below 4.
    assert_eq!(
        (p.tier_of(1), p.tier_of(2), p.tier_of(3), p.tier_of(4)),
        (0, 1, 1, 2)
    );
    let segments = [flush(1, 1), flush(2, 1), flush(3, 1)];
    assert_eq!(plan(&p, &segments), [(MergeReason::Tier(0), vec![1, 2])]);
    // `max_merged_docs` above the segment limit counts as the limit.
    let full = [entry(1, 1 << 31, 1, 0), entry(2, 1, 1, 0)];
    assert_eq!(plan(&p, &full), NONE);
}

#[test]
fn inputs_carry_the_sidecar_version_read() {
    let segments = [entry(1, 100, 50, 30), flush(2, 50)];
    let plans = small().plan(&segments, &HashSet::new());
    assert_eq!(
        plans[0].inputs,
        [MergeInput {
            id: SegmentId::new(1),
            sidecar: 3,
        }]
    );
    let plans = small().plan(
        &(1..=4).map(|id| flush(id, 50)).collect::<Vec<_>>(),
        &HashSet::new(),
    );
    assert!(plans[0].inputs.iter().all(|i| i.sidecar == 0));
}

fn arb_entry() -> impl Strategy<Value = (u32, u64, u32)> {
    (
        1u32..=2_000,
        prop_oneof![1u64..200, 200u64..20_000, 1u64..u64::MAX],
        0u32..=100,
    )
        .prop_map(|(docs, size, percent)| {
            let deleted = u32::try_from(u64::from(docs) * u64::from(percent) / 100).unwrap_or(docs);
            (docs, size, deleted)
        })
}

proptest! {
    /// Plans are disjoint, leave busy segments out, list inputs in manifest order with their sidecar version, and
    /// respect each rule's shape.
    #[test]
    fn plans_are_well_formed(
        raw in prop::collection::vec(arb_entry(), 0..60),
        busy_mask in prop::collection::vec(any::<bool>(), 60),
        width in 2usize..6,
    ) {
        let segments: Vec<_> = raw
            .iter()
            .zip(1..)
            .map(|(&(docs, size, deleted), id)| entry(id, docs, size, deleted))
            .collect();
        let busy: HashSet<_> = segments
            .iter()
            .zip(&busy_mask)
            .filter(|(_, b)| **b)
            .map(|(e, _)| e.id)
            .collect();
        let mut p = small();
        p.segments_per_merge = width;
        let plans = p.plan(&segments, &busy);
        let position = |id: SegmentId| segments.iter().position(|e| e.id == id);
        let mut seen = HashSet::new();
        for plan in &plans {
            prop_assert!(!plan.inputs.is_empty());
            let positions: Vec<_> = plan.inputs.iter().map(|i| position(i.id)).collect();
            prop_assert!(positions.windows(2).all(|w| w[0] < w[1]), "manifest order");
            for input in &plan.inputs {
                prop_assert!(!busy.contains(&input.id));
                prop_assert!(seen.insert(input.id), "{} in two plans", input.id);
                let e = segments[position(input.id).unwrap()];
                prop_assert_eq!(input.sidecar, e.tombstones.map_or(0, |t| t.version));
                let deleted = e.tombstones.map_or(0, |t| t.deleted);
                match plan.reason {
                    MergeReason::Drop => prop_assert_eq!(deleted, e.doc_count),
                    MergeReason::Deletes => prop_assert!(deleted * 100 > e.doc_count * 20),
                    MergeReason::Tier(t) => {
                        prop_assert!(deleted < e.doc_count);
                        let live = u64::from(e.doc_count - deleted);
                        let live_size = u64::try_from(
                            u128::from(e.size) * u128::from(live) / u128::from(e.doc_count),
                        ).unwrap();
                        prop_assert_eq!(p.tier_of(live_size), t);
                    }
                }
            }
            match plan.reason {
                MergeReason::Tier(_) => prop_assert_eq!(plan.inputs.len(), width),
                MergeReason::Deletes => prop_assert_eq!(plan.inputs.len(), 1),
                MergeReason::Drop => {}
            }
        }
        // Every fully deleted free segment is dropped; nothing that should be rewritten is left out.
        for e in &segments {
            if busy.contains(&e.id) {
                continue;
            }
            let deleted = e.tombstones.map_or(0, |t| t.deleted);
            if deleted * 100 > e.doc_count * 20 {
                prop_assert!(seen.contains(&e.id), "{} left out", e.id);
            }
        }
        // Pure: the same input plans the same.
        prop_assert_eq!(p.plan(&segments, &busy), plans);
    }
}
