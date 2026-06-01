//! Lower cover segmentation via Bernoulli factoring. `plan_segments` enumerates all
//! feasible membership subsets for a lower cover group via branch-and-bound DFS,
//! then applies Iterative Proportional Fitting to restore declared marginal ratios.
use anyhow::{Result, bail};
use fake::Fake;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::constraints::{FieldConstraints, Merge};
use crate::models::{CountSpec, RingBounds, SyntheticDataset};

/// Hard cap on the number of feasible segments produced by DFS enumeration.
/// For tightly constrained groups (e.g. mutually exclusive tiers) K = N+1.
/// Only fully-compatible groups with no conflicting constraints approach 2^N.
pub const MAX_FEASIBLE_SEGMENTS: usize = 1_000_000;

/// A lower cover member: a dataset that directly includes a common parent, along with its
/// include metadata needed for constraint extraction.
#[derive(Clone, Debug)]
pub struct LowerCoverMember {
    pub path: PathBuf,
    /// Dataset after the rewrite pass (ref fields resolved).
    pub dataset: SyntheticDataset,
    /// Fraction of the parent's population this member represents.
    pub ratio: f64,
    /// How many child rows to generate per parent-row slot (top-level cardinality).
    pub cardinality: Option<CountSpec>,
    /// The `ref:` name from the include declaration pointing to the parent.
    pub reference: String,
    /// True for witness-source members (arose from a `content: {from: <ref>}` list-link field).
    /// Witness-source members do not generate standalone batches — they only contribute field
    /// constraints to the parent's segment generation, and their rows must be placed
    /// first in the parent batch so `GenerateWitness`'s `n_eligible` boundary is correct.
    pub is_witness_source: bool,
}

/// A planned generation segment produced by Bernoulli factoring.
///
/// Each segment represents the subset of parent rows that belong simultaneously
/// to a specific combination of lower cover members (possibly none — the remainder
/// segment where no member claims the row).
#[derive(Debug, Clone)]
pub struct Segment {
    /// Lower cover member paths whose output files should also receive rows from this
    /// segment (empty ⟹ remainder rows).
    pub members: Vec<PathBuf>,
    /// Row count to generate for this segment.
    pub rows: usize,
    /// Merged constraints from all members in `members`, keyed by the parent
    /// field name they override. Applied during parent-schema generation for
    /// this segment.
    pub field_constraints: HashMap<String, FieldConstraints>,
    /// Hash ring slice assigned to this segment by `assign_ring_slices`.
    /// `None` on segments belonging to non-imported parents.
    pub ring: Option<RingBounds>,
}

/// Tile `parent_ring` across `segments` proportionally to their row counts,
/// writing `segment.ring` for each. Segments with zero rows receive a
/// zero-width slice (the executor skips them as ⊥).
///
/// Only called for imported-dataset parents; segments of non-imported datasets
/// are left with `ring: None`.
pub fn assign_ring_slices(segments: &mut [Segment], parent_ring: &RingBounds) {
    let total: usize = segments.iter().map(|s| s.rows).sum();
    if total == 0 {
        return;
    }
    let span = parent_ring.end - parent_ring.start;
    let mut cursor = parent_ring.start;
    for seg in segments.iter_mut() {
        let frac = seg.rows as f64 / total as f64;
        let slice_end = cursor + span * frac;
        seg.ring = Some(RingBounds { start: cursor, end: slice_end });
        cursor = slice_end;
    }
    // Clamp the last slice to exactly parent_ring.end to avoid f64 rounding drift.
    if let Some(r) = segments.last_mut().and_then(|s| s.ring.as_mut()) {
        r.end = parent_ring.end;
    }
}

#[inline]
fn in_subset(mask: usize, i: usize) -> bool {
    mask & (1 << i) != 0
}

/// Plan the product-Bernoulli segments for a parent dataset given its lower cover.
///
/// Algorithm:
/// 1. Precompute pairwise conflict masks and per-member field constraints.
/// 2. Branch-and-bound DFS: enumerate only feasible membership subsets,
///    pruning any branch where the new member conflicts with an already-included
///    member or where constraints are mutually contradictory. Weights are
///    accumulated as products of marginal (in/out) probabilities along the DFS path.
/// 3. Apply IPF over the feasible set to restore declared marginal distributions.
/// 4. Bernoulli-round to integer row counts; drop zero-row segments.
pub fn plan_segments(parent_rows: usize, members: &[LowerCoverMember]) -> Result<Vec<Segment>> {
    let n = members.len();

    if n == 0 {
        return Ok(vec![Segment {
            members: vec![],
            rows: parent_rows,
            field_constraints: HashMap::new(),
            ring: None,
        }]);
    }

    let conflict_masks = precompute_conflicts(members);
    let member_constraints: Vec<HashMap<String, FieldConstraints>> =
        members.iter().map(lower_cover_field_constraints).collect();

    let mut feasible: HashMap<usize, HashMap<String, FieldConstraints>> = HashMap::new();
    let mut weights: HashMap<usize, f64> = HashMap::new();
    enumerate_segments_dfs(
        0,
        0,
        HashMap::new(),
        1.0,
        members,
        &conflict_masks,
        &member_constraints,
        &mut feasible,
        &mut weights,
    )?;

    let surviving_total: f64 = weights.values().copied().sum();
    if surviving_total <= 0.0 {
        return Ok(vec![Segment {
            members: vec![],
            rows: parent_rows,
            field_constraints: HashMap::new(),
            ring: None,
        }]);
    }

    // IPF over the sparse feasible set to restore declared marginal distributions.
    // Skipped when all subsets are feasible (surviving_total == 1.0) — no pruning occurred.
    if surviving_total < 1.0 - 1e-9 {
        let scale = 1.0 / surviving_total;
        for w in weights.values_mut() {
            *w *= scale;
        }
        ipf_rescale_sparse(&mut weights, members, &feasible);
    }

    // --- Bernoulli rounding ---
    let total_weight: f64 = weights.values().copied().sum();
    let segments: Vec<Segment> = feasible
        .keys()
        .filter_map(|&mask| {
            let raw = (weights[&mask] / total_weight) * parent_rows as f64;
            let rows = if raw >= 1.0 {
                raw.round() as usize
            } else {
                if (0.0f64..1.0f64).fake::<f64>() < raw {
                    1
                } else {
                    0
                }
            };
            if rows == 0 {
                return None;
            }
            let member_paths: Vec<PathBuf> = (0..n)
                .filter(|&i| in_subset(mask, i))
                .map(|i| members[i].path.clone())
                .collect();
            Some(Segment {
                members: member_paths,
                rows,
                field_constraints: feasible[&mask].clone(),
                ring: None,
            })
        })
        .collect();

    if segments.is_empty() {
        return Ok(vec![Segment {
            members: vec![],
            rows: parent_rows,
            field_constraints: HashMap::new(),
            ring: None,
        }]);
    }

    Ok(segments)
}

/// Enumerate all feasible membership subsets via branch-and-bound DFS.
///
/// At each step, we decide whether to include or exclude `members[idx]`.
/// The include branch is pruned when:
/// - any already-included member conflicts with `members[idx]` (pairwise conflict check), or
/// - `members[idx]`'s constraints cannot be merged with the running partial constraints.
///
/// Weights are products of marginal probabilities accumulated along the path.
/// Both `feasible` and `weights` are populated at leaves (when `idx == members.len()`).
#[allow(clippy::too_many_arguments)]
fn enumerate_segments_dfs(
    idx: usize,
    mask: usize,
    merged: HashMap<String, FieldConstraints>,
    weight: f64,
    members: &[LowerCoverMember],
    conflict_masks: &[usize],
    member_constraints: &[HashMap<String, FieldConstraints>],
    feasible: &mut HashMap<usize, HashMap<String, FieldConstraints>>,
    weights: &mut HashMap<usize, f64>,
) -> Result<()> {
    if idx == members.len() {
        if feasible.len() >= MAX_FEASIBLE_SEGMENTS {
            bail!(
                "lower cover group produced more than {} feasible segments. \
                 Add conflicting field constraints between members to reduce the feasible set.",
                MAX_FEASIBLE_SEGMENTS
            );
        }
        feasible.insert(mask, merged);
        weights.insert(mask, weight);
        return Ok(());
    }

    let ratio = members[idx].ratio;

    // Branch A: exclude member idx (clone merged — still needed for Branch B)
    enumerate_segments_dfs(
        idx + 1,
        mask,
        merged.clone(),
        weight * (1.0 - ratio),
        members,
        conflict_masks,
        member_constraints,
        feasible,
        weights,
    )?;

    // Branch B: include member idx — prune if any already-included member conflicts
    if (mask & conflict_masks[idx]) == 0
        && let Some(new_merged) = try_merge_incremental(merged, &member_constraints[idx])
    {
        enumerate_segments_dfs(
            idx + 1,
            mask | (1 << idx),
            new_merged,
            weight * ratio,
            members,
            conflict_masks,
            member_constraints,
            feasible,
            weights,
        )?;
    }

    Ok(())
}

/// Incrementally merge one member's field constraints into a running partial set.
/// Returns `None` if any field in `extra` conflicts with the corresponding entry in `base`.
pub(crate) fn try_merge_incremental(
    mut base: HashMap<String, FieldConstraints>,
    extra: &HashMap<String, FieldConstraints>,
) -> Option<HashMap<String, FieldConstraints>> {
    for (field_name, fc) in extra {
        match base.get(field_name) {
            None => {
                base.insert(field_name.clone(), fc.clone());
            }
            Some(existing) => {
                let merged = existing.merge(fc)?;
                base.insert(field_name.clone(), merged);
            }
        }
    }
    Some(base)
}

/// For each lower cover member i, build a bitmask of all members j that are pairwise
/// incompatible with i (their merged constraints are infeasible). Any mask containing
/// such a pair can be eliminated without further constraint computation.
fn precompute_conflicts(members: &[LowerCoverMember]) -> Vec<usize> {
    let n = members.len();
    let constraints: Vec<HashMap<String, FieldConstraints>> =
        members.iter().map(lower_cover_field_constraints).collect();
    let mut conflict_masks = vec![0usize; n];
    for i in 0..n {
        for j in (i + 1)..n {
            if constraints_conflict(&constraints[i], &constraints[j]) {
                conflict_masks[i] |= 1 << j;
                conflict_masks[j] |= 1 << i;
            }
        }
    }
    conflict_masks
}

pub(crate) fn constraints_conflict(
    a: &HashMap<String, FieldConstraints>,
    b: &HashMap<String, FieldConstraints>,
) -> bool {
    a.iter()
        .any(|(field, fc_a)| b.get(field).is_some_and(|fc_b| fc_a.merge(fc_b).is_none()))
}

/// IPF over the sparse feasible set. Iterates only feasible mask keys rather than
/// all 2^N entries, making each round O(K) where K = |feasible|.
fn ipf_rescale_sparse(
    weights: &mut HashMap<usize, f64>,
    members: &[LowerCoverMember],
    feasible: &HashMap<usize, HashMap<String, FieldConstraints>>,
) {
    let n = members.len();
    const EPS: f64 = 1e-12;
    const TOL: f64 = 1e-9;

    for _ in 0..200 {
        let mut converged = true;
        for (i, member) in members.iter().enumerate().take(n) {
            let target = member.ratio;
            let mass_in: f64 = feasible
                .keys()
                .filter(|&&m| in_subset(m, i))
                .map(|&m| weights[&m])
                .sum();
            let mass_out: f64 = feasible
                .keys()
                .filter(|&&m| !in_subset(m, i))
                .map(|&m| weights[&m])
                .sum();

            if mass_in <= EPS || mass_out <= EPS {
                continue;
            }
            if (mass_in - target).abs() > TOL {
                converged = false;
                let scale_in = target / mass_in;
                let scale_out = (1.0 - target) / mass_out;
                for &m in feasible.keys() {
                    if in_subset(m, i) {
                        if let Some(w) = weights.get_mut(&m) {
                            *w *= scale_in;
                        }
                    } else {
                        if let Some(w) = weights.get_mut(&m) {
                            *w *= scale_out;
                        }
                    }
                }
            }
        }
        if converged {
            break;
        }
    }
}

/// Extract the FieldConstraints this lower cover member imposes on the parent's fields
/// via its ref fields. The ref field string has the form `include_ref.field_name`;
/// we filter by the member's known `include_ref` and return constraints keyed
/// by the parent field name.
pub(crate) fn lower_cover_field_constraints(
    member: &LowerCoverMember,
) -> HashMap<String, FieldConstraints> {
    let prefix = format!("{}.", member.reference);
    let mut map = HashMap::new();
    for field in &member.dataset.data {
        if let Some(ref_str) = field.simple_ref()
            && let Some(parent_field_name) = ref_str.strip_prefix(prefix.as_str())
        {
            map.insert(parent_field_name.to_string(), FieldConstraints::from(field));
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Field, FieldType, Format, RefsSpec};
    use serde_yaml::Value as YamlValue;

    fn make_member(
        path: &str,
        ratio: f64,
        ref_constraints: Vec<(&str, FieldConstraints)>,
    ) -> LowerCoverMember {
        let fields = ref_constraints
            .into_iter()
            .map(|(fname, fc)| {
                let range = fc.to_range();
                Field {
                    name: fname.to_string(),
                    field_type: Some(FieldType::String),
                    refs: Some(RefsSpec::Single(format!("parent_ref.{fname}"))),
                    generator: fc.generator,
                    range,
                    value: fc.value,
                    ..Default::default()
                }
            })
            .collect();

        LowerCoverMember {
            path: PathBuf::from(path),
            dataset: SyntheticDataset {
                name: path.to_string(),
                format: Format::Parquet,
                rows: None,
                output: None,
                outputs: vec![],
                locale: None,
                include: None,
                import: None,
                links: vec![],
                data: fields,
                variants: vec![],
            },
            ratio,
            cardinality: None,
            reference: "parent_ref".to_string(),
            is_witness_source: false,
        }
    }

    fn value_str(s: &str) -> FieldConstraints {
        FieldConstraints {
            value: Some(YamlValue::String(s.into())),
            ..Default::default()
        }
    }

    fn bounds(min: f64, max: f64) -> FieldConstraints {
        FieldConstraints {
            min: Some(min),
            max: Some(max),
            ..Default::default()
        }
    }

    // --- single lower cover member ---

    #[test]
    fn single_lower_cover_member_two_segments() {
        // One lower cover member at 60%: segment {A}=60 rows, remainder=40 rows.
        let members = vec![make_member("a", 0.6, vec![])];
        let segs = plan_segments(100, &members).unwrap();
        assert_eq!(segs.len(), 2);
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 100);
        let a_seg = segs.iter().find(|s| !s.members.is_empty()).unwrap();
        assert_eq!(a_seg.rows, 60);
    }

    // --- two lower cover members, compatible constraints ---

    #[test]
    fn two_lower_cover_members_four_segments() {
        let members = vec![make_member("a", 0.5, vec![]), make_member("b", 0.4, vec![])];
        let segs = plan_segments(100, &members).unwrap();
        // All four subsets survive (no conflicts).
        assert_eq!(segs.len(), 4);
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn two_lower_cover_members_rows_sum_to_parent() {
        let members = vec![
            make_member("a", 0.95, vec![("status", value_str("micro"))]),
            make_member("b", 0.04, vec![("status", value_str("small"))]),
        ];
        let segs = plan_segments(1000, &members).unwrap();
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 1000);
    }

    // --- conflict: incompatible constants ---

    #[test]
    fn conflicting_constants_zeroed_and_redistributed() {
        // A says status="micro", B says status="small": joint segment {A,B} conflicts.
        // Both d=0.5 → Σd=1.0. IPF converges to the categorical distribution:
        // w({A})=0.5, w({B})=0.5, w({})→0. Parent-only segment drops out.
        let members = vec![
            make_member("a", 0.5, vec![("status", value_str("micro"))]),
            make_member("b", 0.5, vec![("status", value_str("small"))]),
        ];
        let segs = plan_segments(100, &members).unwrap();
        let joint = segs.iter().find(|s| s.members.len() == 2);
        assert!(
            joint.is_none(),
            "conflicting joint segment should be absent"
        );
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert!(
            (99..=101).contains(&total),
            "total should be ~100, got {total}"
        );
        // Marginals should match the declared distributions.
        let a_rows = segs
            .iter()
            .find(|s| s.members == vec![PathBuf::from("a")])
            .map_or(0, |s| s.rows);
        let b_rows = segs
            .iter()
            .find(|s| s.members == vec![PathBuf::from("b")])
            .map_or(0, |s| s.rows);
        assert_eq!(a_rows, 50);
        assert_eq!(b_rows, 50);
    }

    // --- compatible bounds ---

    #[test]
    fn overlapping_bounds_merge_into_joint_segment() {
        let members = vec![
            make_member("a", 0.5, vec![("n", bounds(0.0, 100.0))]),
            make_member("b", 0.5, vec![("n", bounds(50.0, 200.0))]),
        ];
        let segs = plan_segments(100, &members).unwrap();
        // All four subsets survive; joint segment should have min=50, max=100.
        let joint = segs.iter().find(|s| s.members.len() == 2).unwrap();
        let fc = joint.field_constraints.get("n").unwrap();
        assert_eq!(fc.min, Some(50.0));
        assert_eq!(fc.max, Some(100.0));
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 100);
    }

    // --- non-overlapping bounds conflict ---

    #[test]
    fn non_overlapping_bounds_zeroed() {
        let members = vec![
            make_member("a", 0.5, vec![("n", bounds(0.0, 30.0))]),
            make_member("b", 0.5, vec![("n", bounds(60.0, 100.0))]),
        ];
        let segs = plan_segments(100, &members).unwrap();
        // The joint {a,b} segment is infeasible and must be absent.
        let joint = segs.iter().find(|s| s.members.len() == 2);
        assert!(joint.is_none());
        // IPF drives the parent-only {} segment toward 0 but never reaches it in
        // finite rounds, so Bernoulli rounding may produce ±1 on the total.
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert!(
            (99..=101).contains(&total),
            "total should be ~100, got {total}"
        );
    }

    // --- no lower cover members ---

    #[test]
    fn no_lower_cover_members_single_remainder_segment() {
        let segs = plan_segments(50, &[]).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].rows, 50);
        assert!(segs[0].members.is_empty());
    }

    // --- large mutually exclusive group (N=20) ---

    #[test]
    fn dfs_n20_mutually_exclusive() {
        // 20 members each with a distinct status constant — all pairs conflict.
        // DFS should produce exactly 21 feasible segments: 20 singletons + empty remainder.
        let members: Vec<LowerCoverMember> = (0..20)
            .map(|i| {
                make_member(
                    &format!("m{i}"),
                    0.04,
                    vec![("status", value_str(&format!("tier{i}")))],
                )
            })
            .collect();
        let segs = plan_segments(1000, &members).unwrap();
        // 21 segments: 20 singletons + empty (which may round to 0 and be dropped when Σd=0.8)
        // At least the 20 singletons must be present.
        let singleton_count = segs.iter().filter(|s| s.members.len() == 1).count();
        assert_eq!(
            singleton_count, 20,
            "expected 20 singleton segments, got {singleton_count}"
        );
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 1000);
        // Each singleton should have approximately 40 rows (4% of 1000).
        for seg in segs.iter().filter(|s| s.members.len() == 1) {
            assert!(
                seg.rows >= 30 && seg.rows <= 50,
                "singleton segment has {} rows, expected ~40",
                seg.rows
            );
        }
    }

    // --- K-cap fires for fully-compatible large group ---

    #[test]
    fn dfs_n20_fully_compatible_hits_cap() {
        // 20 members with no constraints and equal ratios — K = 2^20 > MAX_FEASIBLE_SEGMENTS.
        let members: Vec<LowerCoverMember> = (0..20)
            .map(|i| make_member(&format!("m{i}"), 0.5, vec![]))
            .collect();
        let result = plan_segments(1000, &members);
        assert!(
            result.is_err(),
            "expected cap error for fully-compatible N=20 group"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("feasible segments"),
            "error should mention 'feasible segments', got: {msg}"
        );
    }

    // --- incremental constraint merge: partial compatibility ---

    #[test]
    fn dfs_incremental_constraint_merge() {
        // A: n in [0, 50]; B: n in [30, 100]; C: n in [60, 100]
        // {A,B}: n in [30,50] — compatible
        // {A,C}: n in [60,50] — infeasible (min > max)
        // {A,B,C}: infeasible (A and C conflict)
        // {B,C}: n in [60,100] — compatible
        let members = vec![
            make_member("a", 0.5, vec![("n", bounds(0.0, 50.0))]),
            make_member("b", 0.5, vec![("n", bounds(30.0, 100.0))]),
            make_member("c", 0.5, vec![("n", bounds(60.0, 100.0))]),
        ];
        let segs = plan_segments(100, &members).unwrap();
        let has = |ms: &[&str]| {
            let paths: Vec<PathBuf> = ms.iter().map(|&s| PathBuf::from(s)).collect();
            segs.iter().any(|s| {
                let mut sp = s.members.clone();
                sp.sort();
                let mut pp = paths.clone();
                pp.sort();
                sp == pp
            })
        };
        assert!(has(&["a", "b"]), "{{a,b}} should be present");
        assert!(has(&["b", "c"]), "{{b,c}} should be present");
        assert!(
            !has(&["a", "c"]),
            "{{a,c}} should be absent (infeasible bounds)"
        );
        assert!(!has(&["a", "b", "c"]), "{{a,b,c}} should be absent");
    }
}

#[cfg(test)]
mod ring_tests {
    use super::*;

    fn seg(rows: usize) -> Segment {
        Segment {
            members: vec![],
            rows,
            field_constraints: HashMap::new(),
            ring: None,
        }
    }

    fn parent(start: f64, end: f64) -> RingBounds {
        RingBounds { start, end }
    }

    #[test]
    fn single_segment_gets_full_parent_ring() {
        let mut segs = vec![seg(100)];
        assign_ring_slices(&mut segs, &parent(0.0, 1.0));
        let r = segs[0].ring.as_ref().unwrap();
        assert!((r.start - 0.0).abs() < 1e-9);
        assert!((r.end - 1.0).abs() < 1e-9);
    }

    #[test]
    fn two_equal_segments_split_evenly() {
        let mut segs = vec![seg(50), seg(50)];
        assign_ring_slices(&mut segs, &parent(0.0, 1.0));
        let r0 = segs[0].ring.as_ref().unwrap();
        let r1 = segs[1].ring.as_ref().unwrap();
        assert!((r0.start - 0.0).abs() < 1e-9);
        assert!((r0.end - 0.5).abs() < 1e-9);
        assert!((r1.start - 0.5).abs() < 1e-9);
        assert!((r1.end - 1.0).abs() < 1e-9);
    }

    #[test]
    fn proportional_split_three_segments() {
        // rows 30 / 20 / 50 → fractions 0.3 / 0.2 / 0.5
        let mut segs = vec![seg(30), seg(20), seg(50)];
        assign_ring_slices(&mut segs, &parent(0.0, 1.0));
        let r = |i: usize| segs[i].ring.clone().unwrap();
        assert!((r(0).start - 0.0).abs() < 1e-9);
        assert!((r(0).end - 0.3).abs() < 1e-9);
        assert!((r(1).start - 0.3).abs() < 1e-9);
        assert!((r(1).end - 0.5).abs() < 1e-9);
        assert!((r(2).start - 0.5).abs() < 1e-9);
        assert!((r(2).end - 1.0).abs() < 1e-9, "last slice must clamp to parent end");
    }

    #[test]
    fn tiling_within_sub_range() {
        // Parent ring is [0.4, 0.8) — two equal segments each get 0.2 of the full range.
        let mut segs = vec![seg(1), seg(1)];
        assign_ring_slices(&mut segs, &parent(0.4, 0.8));
        let r0 = segs[0].ring.as_ref().unwrap();
        let r1 = segs[1].ring.as_ref().unwrap();
        assert!((r0.start - 0.4).abs() < 1e-9);
        assert!((r0.end - 0.6).abs() < 1e-9);
        assert!((r1.start - 0.6).abs() < 1e-9);
        assert!((r1.end - 0.8).abs() < 1e-9);
    }

    #[test]
    fn slices_tile_without_gap_or_overlap() {
        let mut segs = vec![seg(17), seg(33), seg(50)];
        assign_ring_slices(&mut segs, &parent(0.0, 1.0));
        // Each slice's end must equal the next slice's start.
        for i in 0..segs.len() - 1 {
            let end = segs[i].ring.as_ref().unwrap().end;
            let next_start = segs[i + 1].ring.as_ref().unwrap().start;
            assert!((end - next_start).abs() < 1e-9, "gap/overlap at boundary {i}");
        }
        // Last slice ends exactly at 1.0.
        assert!((segs.last().unwrap().ring.as_ref().unwrap().end - 1.0).abs() < 1e-9);
    }

    #[test]
    fn empty_segment_list_is_noop() {
        let mut segs: Vec<Segment> = vec![];
        assign_ring_slices(&mut segs, &parent(0.0, 1.0)); // must not panic
    }
}
