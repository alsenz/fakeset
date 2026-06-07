//! Lower cover segmentation via Bernoulli factoring. `plan_segments` enumerates all
//! feasible membership subsets for a lower cover group via branch-and-bound DFS,
//! then applies Iterative Proportional Fitting to restore declared marginal ratios.
use anyhow::{Result, bail};
use fixedbitset::FixedBitSet;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::constraints::{FieldConstraints, Merge};
use crate::models::{CountSpec, RingBounds, SyntheticDataset};

/// Hard cap on the number of feasible segments produced by DFS enumeration.
/// For tightly constrained groups (e.g. mutually exclusive tiers) K = N+1.
/// Only fully-compatible groups with no conflicting constraints approach 2^N.
pub const MAX_FEASIBLE_SEGMENTS: usize = 1_000_000;

/// A segment's membership mask: bit `i` ⟺ member `i` is included. A `FixedBitSet`
/// (rather than a fixed-width integer) imposes **no member-count ceiling** — the
/// variant cross-products VAR-EXPAND lowering can produce are unbounded. It costs a
/// small heap clone per DFS branch, but the DFS runs once per group at *plan* time
/// (not per row), so it is off the hot path; `Hash + Eq` let it key the feasible map.
pub(crate) type SegMask = FixedBitSet;

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

/// A set of lowered cases of one tagged union (VAR-EXPAND), grouping members of a
/// parent's lower cover that arose from lowering a single `type: variant` field.
///
/// Exactly one case fires per row — mutual exclusion is *structural* in the DFS
/// (the group branches over "no case" ∪ "pick one case"), so **no discriminant column
/// is materialised** to enforce it. The categorical prior factor weights each case by its
/// absolute ratio `r_M·vᵢ`, with `1 − Σ ratios` reserved for "member absent"
/// (= 0 for a mandatory union).
#[derive(Clone, Debug)]
pub struct ExclusionGroup {
    /// Indices into the lower cover's member slice — the union's lowered cases.
    pub members: Vec<usize>,
    /// Absolute case ratios `r_M·vᵢ`, parallel to `members`.
    pub ratios: Vec<f64>,
    /// True when the union is mandatory (`Σ ratios ≈ 1`): "no case" has weight 0.
    pub mandatory: bool,
}

/// The product-Bernoulli prior factor a union contributes to a segment: the chosen
/// case's absolute ratio, or `1 − Σ ratios` for "no case". Computing this per-union
/// (rather than as a product of independent per-case Bernoulli trials) is what zeroes
/// the illegal "no case of a mandatory union" mass at the source — see VAR-EXPAND.
pub(crate) fn categorical_prior_factor(group: &ExclusionGroup, chosen: Option<usize>) -> f64 {
    match chosen {
        Some(k) => group.ratios[k],
        None => (1.0 - group.ratios.iter().sum::<f64>()).max(0.0),
    }
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
        seg.ring = Some(RingBounds {
            start: cursor,
            end: slice_end,
        });
        cursor = slice_end;
    }
    // Clamp the last slice to exactly parent_ring.end to avoid f64 rounding drift.
    if let Some(r) = segments.last_mut().and_then(|s| s.ring.as_mut()) {
        r.end = parent_ring.end;
    }
}

/// Largest-remainder (Hamilton) rounding: floor each weight's proportional share of `total`,
/// then hand the leftover `total − Σfloor` units to the cells with the largest fractional parts
/// (ties → input order, so callers order their input for determinism). Unbiased per cell (each
/// gets floor or floor+1) *and* conserves `total` exactly — both matter:
///   - unbiased + minimal per-cell error keeps marginal distributions faithful (a plain
///     `round()` is biased toward common cells; in a many-celled variant cross-product that
///     bias becomes χ² failures);
///   - exact total conservation keeps a member-that-is-also-a-parent's row count equal to the
///     count its own step produced, so the precomputed-reuse path never pad-generates rows.
///
/// The single rounding primitive for both segment row counts (`plan_segments`) and pinned-variant
/// subdivision (`plan::subdivide_for_pinned_variants`).
pub(crate) fn largest_remainder(weights: &[f64], total: usize) -> Vec<usize> {
    let sum: f64 = weights.iter().sum();
    if sum <= 0.0 || weights.is_empty() {
        return vec![0; weights.len()];
    }
    let scaled: Vec<f64> = weights.iter().map(|w| w / sum * total as f64).collect();
    let mut counts: Vec<usize> = scaled.iter().map(|s| s.floor() as usize).collect();
    let leftover = total.saturating_sub(counts.iter().sum());
    let mut order: Vec<usize> = (0..weights.len()).collect();
    // Stable sort by descending fractional part; equal fractions keep input order.
    order.sort_by(|&a, &b| {
        let (fa, fb) = (scaled[a].fract(), scaled[b].fract());
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    for &i in order.iter().take(leftover) {
        counts[i] += 1;
    }
    counts
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
pub fn plan_segments(
    parent_rows: usize,
    members: &[LowerCoverMember],
    groups: &[ExclusionGroup],
) -> Result<Vec<Segment>> {
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

    // Collapse each tagged union's lowered cases into one categorical DFS entry; all
    // other members stay independent Bernoulli entries. With `groups == &[]` every
    // entry is a lone member, in member order — byte-identical to the pre-VAR-EXPAND DFS.
    let entries = build_dfs_entries(n, groups);

    let mut feasible: HashMap<SegMask, HashMap<String, FieldConstraints>> = HashMap::new();
    let mut weights: HashMap<SegMask, f64> = HashMap::new();
    enumerate_segments_dfs(
        0,
        FixedBitSet::with_capacity(n),
        HashMap::new(),
        1.0,
        &entries,
        members,
        groups,
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

    // Round the feasible cells' weights to integer row counts conserving `parent_rows`. Order
    // the cells by member-index list first so the rounding tie-break (and thus the output) is
    // deterministic — HashMap iteration order is not.
    let mut cells: Vec<Vec<usize>> = feasible.keys().map(|m| m.ones().collect()).collect();
    cells.sort();
    let cell_weights: Vec<f64> = cells
        .iter()
        .map(|ones| {
            let mut mask = FixedBitSet::with_capacity(n);
            ones.iter().for_each(|&i| mask.insert(i));
            weights[&mask]
        })
        .collect();
    let counts = largest_remainder(&cell_weights, parent_rows);
    let segments: Vec<Segment> = cells
        .iter()
        .zip(counts)
        .filter(|(_, rows)| *rows > 0)
        .map(|(ones, rows)| {
            let mut mask = FixedBitSet::with_capacity(n);
            let member_paths: Vec<PathBuf> = ones
                .iter()
                .map(|&idx| {
                    mask.insert(idx);
                    members[idx].path.clone()
                })
                .collect();
            Segment {
                members: member_paths,
                rows,
                field_constraints: feasible[&mask].clone(),
                ring: None,
            }
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

/// One step of the branch-and-bound DFS: either a lone Bernoulli member or one
/// tagged-union exclusion group (whose lowered cases branch categorically).
enum DfsEntry {
    /// A lone member — index into the member slice.
    Lone(usize),
    /// A tagged union — index into the `groups` slice.
    Group(usize),
}

/// Build the DFS entry plan. Each member belongs to at most one exclusion group;
/// grouped members collapse into a single `Group` entry, emitted at the position of
/// the group's first member. Ungrouped members become `Lone` entries in member order.
/// With `groups == &[]` this is exactly `[Lone(0), …, Lone(n-1)]`.
fn build_dfs_entries(n: usize, groups: &[ExclusionGroup]) -> Vec<DfsEntry> {
    let mut group_of: Vec<Option<usize>> = vec![None; n];
    for (g, grp) in groups.iter().enumerate() {
        for &m in &grp.members {
            group_of[m] = Some(g);
        }
    }
    let mut emitted = vec![false; groups.len()];
    let mut entries = Vec::new();
    for (i, slot) in group_of.iter().enumerate().take(n) {
        match slot {
            None => entries.push(DfsEntry::Lone(i)),
            Some(g) if !emitted[*g] => {
                emitted[*g] = true;
                entries.push(DfsEntry::Group(*g));
            }
            Some(_) => {}
        }
    }
    entries
}

/// Enumerate all feasible segments via branch-and-bound DFS over the entry plan.
///
/// - A `Lone` member branches include/exclude, exactly as the pre-VAR-EXPAND DFS did.
/// - A `Group` branches over "no case" (when its weight is non-zero) plus one branch
///   per lowered case — so at most one case of a union is ever set, making mutual
///   exclusion structural. A mandatory union's "no case" weight is 0 and is pruned,
///   keeping the feasible set bounded.
///
/// Either branch's include is pruned when the new member pairwise-conflicts with an
/// already-included member or its constraints cannot be merged. Weights are products
/// of marginal probabilities; leaves populate `feasible` and `weights`.
#[allow(clippy::too_many_arguments)]
fn enumerate_segments_dfs(
    entry_idx: usize,
    mask: SegMask,
    merged: HashMap<String, FieldConstraints>,
    weight: f64,
    entries: &[DfsEntry],
    members: &[LowerCoverMember],
    groups: &[ExclusionGroup],
    conflict_masks: &[SegMask],
    member_constraints: &[HashMap<String, FieldConstraints>],
    feasible: &mut HashMap<SegMask, HashMap<String, FieldConstraints>>,
    weights: &mut HashMap<SegMask, f64>,
) -> Result<()> {
    if entry_idx == entries.len() {
        if feasible.len() >= MAX_FEASIBLE_SEGMENTS {
            bail!(
                "lower cover group produced more than {} feasible segments. \
                 Add conflicting field constraints between members to reduce the feasible set.",
                MAX_FEASIBLE_SEGMENTS
            );
        }
        feasible.insert(mask.clone(), merged);
        weights.insert(mask, weight);
        return Ok(());
    }

    let next = entry_idx + 1;
    match &entries[entry_idx] {
        DfsEntry::Lone(i) => {
            let i = *i;
            let ratio = members[i].ratio;

            // Branch A: exclude member i (clone merged — still needed for Branch B).
            enumerate_segments_dfs(
                next,
                mask.clone(),
                merged.clone(),
                weight * (1.0 - ratio),
                entries,
                members,
                groups,
                conflict_masks,
                member_constraints,
                feasible,
                weights,
            )?;

            // Branch B: include member i — prune on conflict / unmergeable constraints.
            if mask.is_disjoint(&conflict_masks[i])
                && let Some(new_merged) = try_merge_incremental(merged, &member_constraints[i])
            {
                let mut inc = mask;
                inc.insert(i);
                enumerate_segments_dfs(
                    next,
                    inc,
                    new_merged,
                    weight * ratio,
                    entries,
                    members,
                    groups,
                    conflict_masks,
                    member_constraints,
                    feasible,
                    weights,
                )?;
            }
        }
        DfsEntry::Group(g) => {
            let grp = &groups[*g];

            // Branch: no case of this union (member absent). Pruned when its weight is
            // 0 — i.e. a mandatory union — which is precisely the illegal-mass cell.
            let none_factor = categorical_prior_factor(grp, None);
            if none_factor > f64::EPSILON {
                enumerate_segments_dfs(
                    next,
                    mask.clone(),
                    merged.clone(),
                    weight * none_factor,
                    entries,
                    members,
                    groups,
                    conflict_masks,
                    member_constraints,
                    feasible,
                    weights,
                )?;
            }

            // Branch: pick exactly one case. Structural mutual exclusion — at most one
            // case member's bit is ever set across these branches.
            for (k, &ci) in grp.members.iter().enumerate() {
                if !mask.is_disjoint(&conflict_masks[ci]) {
                    continue;
                }
                if let Some(new_merged) =
                    try_merge_incremental(merged.clone(), &member_constraints[ci])
                {
                    let mut inc = mask.clone();
                    inc.insert(ci);
                    enumerate_segments_dfs(
                        next,
                        inc,
                        new_merged,
                        weight * categorical_prior_factor(grp, Some(k)),
                        entries,
                        members,
                        groups,
                        conflict_masks,
                        member_constraints,
                        feasible,
                        weights,
                    )?;
                }
            }
        }
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
fn precompute_conflicts(members: &[LowerCoverMember]) -> Vec<SegMask> {
    let n = members.len();
    let constraints: Vec<HashMap<String, FieldConstraints>> =
        members.iter().map(lower_cover_field_constraints).collect();
    let mut conflict_masks = vec![FixedBitSet::with_capacity(n); n];
    for i in 0..n {
        for j in (i + 1)..n {
            if constraints_conflict(&constraints[i], &constraints[j]) {
                conflict_masks[i].insert(j);
                conflict_masks[j].insert(i);
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
    weights: &mut HashMap<SegMask, f64>,
    members: &[LowerCoverMember],
    feasible: &HashMap<SegMask, HashMap<String, FieldConstraints>>,
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
                .filter(|m| m.contains(i))
                .map(|m| weights[m])
                .sum();
            let mass_out: f64 = feasible
                .keys()
                .filter(|m| !m.contains(i))
                .map(|m| weights[m])
                .sum();

            if mass_in <= EPS || mass_out <= EPS {
                continue;
            }
            if (mass_in - target).abs() > TOL {
                converged = false;
                let scale_in = target / mass_in;
                let scale_out = (1.0 - target) / mass_out;
                // Snapshot keys to satisfy the borrow checker (mutating `weights` while
                // reading `feasible`'s keys is fine, but keep it explicit and simple).
                let keys: Vec<SegMask> = feasible.keys().cloned().collect();
                for m in keys {
                    let scale = if m.contains(i) { scale_in } else { scale_out };
                    if let Some(w) = weights.get_mut(&m) {
                        *w *= scale;
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

    #[test]
    fn largest_remainder_conserves_total_and_is_unbiased() {
        // Exact proportional split.
        assert_eq!(largest_remainder(&[0.5, 0.5], 10), vec![5, 5]);
        // Subset whose weights sum to <1 must renormalise (not dump the tail on the last cell)
        // — the property `subdivide_for_pinned_variants` and the `one_of` carrier rely on.
        assert_eq!(largest_remainder(&[0.25, 0.25], 100), vec![50, 50]);
        // Hamilton: the largest fractional remainder wins the leftover unit.
        assert_eq!(largest_remainder(&[0.34, 0.33, 0.33], 10), vec![4, 3, 3]);
        // Total is always conserved exactly, even with awkward fractions.
        for total in [0, 1, 7, 99, 1000] {
            let counts = largest_remainder(&[0.2, 0.3, 0.5], total);
            assert_eq!(
                counts.iter().sum::<usize>(),
                total,
                "total {total} conserved"
            );
        }
        // Degenerate inputs.
        assert_eq!(largest_remainder(&[], 5), Vec::<usize>::new());
        assert_eq!(largest_remainder(&[1.0, 1.0], 0), vec![0, 0]);
        assert_eq!(largest_remainder(&[0.0, 0.0], 4), vec![0, 0]);
    }

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
        let segs = plan_segments(100, &members, &[]).unwrap();
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
        let segs = plan_segments(100, &members, &[]).unwrap();
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
        let segs = plan_segments(1000, &members, &[]).unwrap();
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
        let segs = plan_segments(100, &members, &[]).unwrap();
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
        let segs = plan_segments(100, &members, &[]).unwrap();
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
        let segs = plan_segments(100, &members, &[]).unwrap();
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
        let segs = plan_segments(50, &[], &[]).unwrap();
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
        let segs = plan_segments(1000, &members, &[]).unwrap();
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
        let result = plan_segments(1000, &members, &[]);
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
        let segs = plan_segments(100, &members, &[]).unwrap();
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

    // --- VAR-EXPAND exclusion groups (PR 2 dormant machinery) ---
    //
    // Drive `plan_segments` with hand-built groups (no lowering pass yet) to verify the
    // entry-DFS + categorical factor produce structurally-exclusive, marginal-correct
    // segments — including the `r_M < 1` (optional member) case the Q-prototype omitted.

    /// Rows in segments whose member set contains `path`.
    fn rows_for(segs: &[Segment], path: &str) -> usize {
        let p = PathBuf::from(path);
        segs.iter()
            .filter(|s| s.members.contains(&p))
            .map(|s| s.rows)
            .sum()
    }

    #[test]
    fn exclusion_group_mandatory_union_is_exclusive_and_exact() {
        // A mandatory union (Σ ratios == 1): cases c0 (0.6) and c1 (0.4).
        let members = vec![
            make_member("c0", 0.6, vec![]),
            make_member("c1", 0.4, vec![]),
        ];
        let group = ExclusionGroup {
            members: vec![0, 1],
            ratios: vec![0.6, 0.4],
            mandatory: true,
        };
        let segs = plan_segments(1000, &members, &[group]).unwrap();

        // No segment carries both cases (structural mutual exclusion).
        let c0 = PathBuf::from("c0");
        let c1 = PathBuf::from("c1");
        assert!(
            !segs
                .iter()
                .any(|s| s.members.contains(&c0) && s.members.contains(&c1)),
            "no segment may contain two cases of one union"
        );
        // Mandatory ⇒ no "member absent" (empty) segment survives.
        assert!(
            !segs.iter().any(|s| s.members.is_empty()),
            "a mandatory union leaves no holder-less rows"
        );
        // Marginals restored ~600 / ~400 (Bernoulli rounding ± a few).
        assert!((rows_for(&segs, "c0") as i64 - 600).abs() <= 25, "c0 ≈ 600");
        assert!((rows_for(&segs, "c1") as i64 - 400).abs() <= 25, "c1 ≈ 400");
    }

    #[test]
    fn exclusion_group_optional_union_keeps_member_absent_cell() {
        // An optional member (r_M = 0.5) with a union split 0.6/0.4 within it ⇒ absolute
        // case ratios 0.3 / 0.2, and a legitimate "member absent" mass of 0.5.
        let members = vec![
            make_member("c0", 0.3, vec![]),
            make_member("c1", 0.2, vec![]),
        ];
        let group = ExclusionGroup {
            members: vec![0, 1],
            ratios: vec![0.3, 0.2],
            mandatory: false,
        };
        let segs = plan_segments(1000, &members, &[group]).unwrap();

        // The "no case" (member absent) segment exists and carries ~500 rows.
        let absent: usize = segs
            .iter()
            .filter(|s| s.members.is_empty())
            .map(|s| s.rows)
            .sum();
        assert!(
            (absent as i64 - 500).abs() <= 30,
            "member-absent ≈ 500, got {absent}"
        );
        assert!((rows_for(&segs, "c0") as i64 - 300).abs() <= 25, "c0 ≈ 300");
        assert!((rows_for(&segs, "c1") as i64 - 200).abs() <= 25, "c1 ≈ 200");
    }

    #[test]
    fn exclusion_group_with_plain_member_factors_jointly() {
        // Mandatory union {c0 0.6, c1 0.4} alongside an independent plain member s (0.5).
        let members = vec![
            make_member("c0", 0.6, vec![]),
            make_member("c1", 0.4, vec![]),
            make_member("s", 0.5, vec![]),
        ];
        let group = ExclusionGroup {
            members: vec![0, 1],
            ratios: vec![0.6, 0.4],
            mandatory: true,
        };
        let segs = plan_segments(1000, &members, &[group]).unwrap();

        let c0 = PathBuf::from("c0");
        let c1 = PathBuf::from("c1");
        assert!(
            !segs
                .iter()
                .any(|s| s.members.contains(&c0) && s.members.contains(&c1)),
            "union cases stay mutually exclusive even with a plain member present"
        );
        // Every segment picks exactly one case (mandatory union, no empty segments).
        assert!(
            segs.iter()
                .all(|s| s.members.contains(&c0) || s.members.contains(&c1))
        );
        // Marginals: each case ~ its ratio; the plain member ~ 0.5.
        assert!((rows_for(&segs, "c0") as i64 - 600).abs() <= 30, "c0 ≈ 600");
        assert!((rows_for(&segs, "c1") as i64 - 400).abs() <= 30, "c1 ≈ 400");
        assert!((rows_for(&segs, "s") as i64 - 500).abs() <= 40, "s ≈ 500");
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
        assert!(
            (r(2).end - 1.0).abs() < 1e-9,
            "last slice must clamp to parent end"
        );
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
            assert!(
                (end - next_start).abs() < 1e-9,
                "gap/overlap at boundary {i}"
            );
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

/// Q-prototype for VAR-EXPAND (tagged-union lowering). See `specs/VAR-EXPAND.md`
/// §Q-prototype. Validates, ahead of any lowering implementation, the two load-bearing
/// claims of the chosen synthesis:
///
///   1. The **categorical prior factor** gives a mandatory union's "no case chosen"
///      cells prior weight 0 (`M₀ = 0`), and the real `ipf_rescale_sparse` — being an
///      I-projection — preserves that structural zero. So no row can ever land in an
///      illegal "took no case of a mandatory union" segment.
///   2. With cases lowered to ordinary members, **vanilla per-member IPF** restores
///      every case marginal `vᵢ` (no per-variant extension), and interior IPF converges
///      fast even on stacked mandatory unions with cross-union (specialisation-style)
///      conflicts.
///
/// It also demonstrates *why* the categorical factor is needed: the naive
/// product-Bernoulli prior leaves real **illegal mass** that only decays asymptotically,
/// so a low iteration cap leaks bad rows.
///
/// No lowering plumbing is required — feasible sets and priors are hand-built and fed to
/// the production `ipf_rescale_sparse`; an instrumented mirror of its loop measures
/// per-sweep convergence.
#[cfg(test)]
mod var_expand_prototype {
    use super::{LowerCoverMember, SegMask, ipf_rescale_sparse};
    use crate::constraints::FieldConstraints;
    use crate::models::{Format, SyntheticDataset};
    use std::collections::HashMap;
    use std::path::PathBuf;

    // Mirror the production loop's constants so the instrumented study matches.
    const TOL: f64 = 1e-9;
    const EPS: f64 = 1e-12;

    /// A synthetic lowering configuration over a single parent's lower cover.
    struct Config {
        /// `unions[u]` is the case-ratio vector of mandatory union `u` (entries sum to 1.0,
        /// so the "no case" categorical factor `1 − Σvᵢ` is 0 — the union is mandatory).
        unions: Vec<Vec<f64>>,
        /// Ratios of plain (non-union) lower-cover members.
        plain: Vec<f64>,
        /// Cross-union conflicts: `((uₐ, caseₐ), (u_b, case_b))` cannot co-occur — the shape
        /// VAR-SPECIALIZE pruning produces. Drives genuine (non-trivial) IPF work.
        conflicts: Vec<((usize, usize), (usize, usize))>,
    }

    /// Flat member layout: union 0's cases, union 1's cases, …, then plain members.
    /// Member index == bit index in a segment mask (matching `ipf_rescale_sparse`).
    struct Layout {
        offsets: Vec<usize>,
        plain_off: usize,
        total: usize,
    }

    fn layout(cfg: &Config) -> Layout {
        let mut offsets = Vec::new();
        let mut acc = 0;
        for u in &cfg.unions {
            offsets.push(acc);
            acc += u.len();
        }
        Layout {
            offsets,
            plain_off: acc,
            total: acc + cfg.plain.len(),
        }
    }

    fn case_bit(l: &Layout, u: usize, c: usize) -> usize {
        l.offsets[u] + c
    }
    fn plain_bit(l: &Layout, p: usize) -> usize {
        l.plain_off + p
    }
    /// Test bit `b` of the raw `u128` enumeration value (subsets are counted as integers
    /// here, then converted to `SegMask` keys via `to_mask` for the real IPF).
    fn bit(raw: u128, b: usize) -> bool {
        raw & (1 << b) != 0
    }
    /// Build the `SegMask` (FixedBitSet) key for raw enumeration value `raw`.
    fn to_mask(raw: u128, total: usize) -> SegMask {
        let mut m = SegMask::with_capacity(total);
        for b in 0..total {
            if raw & (1 << b) != 0 {
                m.insert(b);
            }
        }
        m
    }

    /// A minimal `LowerCoverMember`: `ipf_rescale_sparse` only reads `.ratio` (and uses the
    /// member's position as its bit), so the rest is dummy.
    fn member(idx: usize, ratio: f64) -> LowerCoverMember {
        LowerCoverMember {
            path: PathBuf::from(format!("m{idx}")),
            dataset: SyntheticDataset {
                name: format!("m{idx}"),
                format: Format::Parquet,
                locale: None,
                rows: None,
                output: None,
                outputs: vec![],
                include: None,
                import: None,
                links: vec![],
                data: vec![],
            },
            ratio,
            cardinality: None,
            reference: "p".to_string(),
            is_witness_source: false,
        }
    }

    fn members(cfg: &Config) -> Vec<LowerCoverMember> {
        let mut v = Vec::new();
        for u in &cfg.unions {
            for &r in u {
                v.push(member(v.len(), r));
            }
        }
        for &r in &cfg.plain {
            v.push(member(v.len(), r));
        }
        v
    }

    /// Enumerate the segments the discriminant leaves feasible (≤ 1 case per union, plus the
    /// declared cross-conflicts), and assign **both** prior weightings to each:
    ///
    ///   - `cat`   — the synthesis's categorical factor (`vᵢ` for the chosen case,
    ///     `1 − Σvᵢ = 0` for a mandatory union with no case);
    ///   - `naive` — the product-Bernoulli prior (each case an independent trial).
    ///
    /// Returns the `feasible` map (keys only matter to IPF), the two weight maps, and the list
    /// of *illegal* masks (some mandatory union took no case).
    #[allow(clippy::type_complexity)]
    fn enumerate(
        cfg: &Config,
        l: &Layout,
    ) -> (
        HashMap<SegMask, HashMap<String, FieldConstraints>>,
        HashMap<SegMask, f64>,
        HashMap<SegMask, f64>,
        Vec<SegMask>,
    ) {
        let mut feasible = HashMap::new();
        let mut cat = HashMap::new();
        let mut naive = HashMap::new();
        let mut illegal = Vec::new();

        for raw in 0..(1u128 << l.total) {
            // Discriminant: at most one case per union.
            let multi_case = cfg.unions.iter().enumerate().any(|(u, cases)| {
                (0..cases.len())
                    .filter(|&c| bit(raw, case_bit(l, u, c)))
                    .count()
                    > 1
            });
            if multi_case {
                continue;
            }
            // Cross-union (specialisation) conflicts.
            let conflicts = cfg.conflicts.iter().any(|&((ua, ca), (ub, cb))| {
                bit(raw, case_bit(l, ua, ca)) && bit(raw, case_bit(l, ub, cb))
            });
            if conflicts {
                continue;
            }

            let mut w_cat = 1.0;
            let mut w_naive = 1.0;
            let mut is_illegal = false;
            for (u, cases) in cfg.unions.iter().enumerate() {
                match (0..cases.len()).find(|&c| bit(raw, case_bit(l, u, c))) {
                    Some(c) => w_cat *= cases[c],
                    None => {
                        w_cat *= 1.0 - cases.iter().sum::<f64>(); // 0 for a mandatory union
                        is_illegal = true;
                    }
                }
                for (c, &v) in cases.iter().enumerate() {
                    w_naive *= if bit(raw, case_bit(l, u, c)) {
                        v
                    } else {
                        1.0 - v
                    };
                }
            }
            for (p, &r) in cfg.plain.iter().enumerate() {
                let s = bit(raw, plain_bit(l, p));
                let f = if s { r } else { 1.0 - r };
                w_cat *= f;
                w_naive *= f;
            }

            let mask = to_mask(raw, l.total);
            feasible.insert(mask.clone(), HashMap::new());
            cat.insert(mask.clone(), w_cat);
            naive.insert(mask.clone(), w_naive);
            if is_illegal {
                illegal.push(mask);
            }
        }
        (feasible, cat, naive, illegal)
    }

    fn normalize(w: &mut HashMap<SegMask, f64>) {
        let s: f64 = w.values().sum();
        if s > 0.0 {
            for v in w.values_mut() {
                *v /= s;
            }
        }
    }

    fn illegal_mass(w: &HashMap<SegMask, f64>, illegal: &[SegMask]) -> f64 {
        illegal.iter().map(|m| w[m]).sum()
    }

    /// Marginal mass of member `i` (sum of weights over masks where bit `i` is set).
    fn marginal(w: &HashMap<SegMask, f64>, i: usize) -> f64 {
        let mut s = 0.0;
        for (m, &x) in w.iter() {
            if m.contains(i) {
                s += x;
            }
        }
        s
    }

    /// Faithful mirror of `ipf_rescale_sparse`'s loop, returning the sweep count at which it
    /// converged (== `max_iter` if it did not). Used to *measure* convergence and to run a
    /// deliberately low cap for the leak demonstration. Correctness is asserted against the
    /// real `ipf_rescale_sparse` elsewhere.
    fn ipf_mirror(
        weights: &mut HashMap<SegMask, f64>,
        members: &[LowerCoverMember],
        feasible: &HashMap<SegMask, HashMap<String, FieldConstraints>>,
        max_iter: usize,
    ) -> usize {
        for iter in 0..max_iter {
            let mut converged = true;
            for (i, m) in members.iter().enumerate() {
                let target = m.ratio;
                let mass_in: f64 = feasible
                    .keys()
                    .filter(|k| k.contains(i))
                    .map(|k| weights[k])
                    .sum();
                let mass_out: f64 = feasible
                    .keys()
                    .filter(|k| !k.contains(i))
                    .map(|k| weights[k])
                    .sum();
                if mass_in <= EPS || mass_out <= EPS {
                    continue;
                }
                if (mass_in - target).abs() > TOL {
                    converged = false;
                    let si = target / mass_in;
                    let so = (1.0 - target) / mass_out;
                    for (k, w) in weights.iter_mut() {
                        *w *= if k.contains(i) { si } else { so };
                    }
                }
            }
            if converged {
                return iter;
            }
        }
        max_iter
    }

    fn uniform(k: usize) -> Vec<f64> {
        vec![1.0 / k as f64; k]
    }

    /// Claim 1 + 2 (no conflicts): across stacked mandatory unions of varying arity and
    /// ratio skew, the categorical prior has zero illegal mass, the *real*
    /// `ipf_rescale_sparse` preserves it, and every case marginal equals `vᵢ`.
    #[test]
    fn categorical_prior_zeroes_illegal_mass_and_restores_marginals() {
        let profiles: Vec<Vec<f64>> = vec![
            uniform(2),
            uniform(3),
            uniform(5),
            vec![0.9, 0.1],         // skewed
            vec![0.97, 0.02, 0.01], // extreme: min vᵢ = 0.01
        ];

        for n_unions in 1..=4 {
            for prof in &profiles {
                let cfg = Config {
                    unions: vec![prof.clone(); n_unions],
                    plain: vec![0.5], // one independent plain member
                    conflicts: vec![],
                };
                let l = layout(&cfg);
                let mem = members(&cfg);
                let (feasible, mut cat, _naive, illegal) = enumerate(&cfg, &l);
                normalize(&mut cat);

                // M₀ = 0 by construction (mandatory "no case" factor is 0).
                assert!(
                    illegal_mass(&cat, &illegal) <= TOL,
                    "categorical prior should start with zero illegal mass (n_unions={n_unions}, prof={prof:?})"
                );

                ipf_rescale_sparse(&mut cat, &mem, &feasible);

                // I-projection preserves the structural zero.
                assert!(
                    illegal_mass(&cat, &illegal) <= 1e-9,
                    "illegal mass must remain zero after IPF (n_unions={n_unions}, prof={prof:?})"
                );
                // Every member (case) marginal is restored to its declared ratio.
                for (i, m) in mem.iter().enumerate() {
                    assert!(
                        (marginal(&cat, i) - m.ratio).abs() < 1e-6,
                        "member {i} marginal {} != ratio {} (n_unions={n_unions}, prof={prof:?})",
                        marginal(&cat, i),
                        m.ratio
                    );
                }
            }
        }
    }

    /// Claim 2 (with conflicts): stacked mandatory unions plus cross-union
    /// (specialisation-style) conflicts force genuine IPF redistribution. Assert the zero is
    /// preserved, all marginals are restored, and interior IPF converges in few sweeps.
    #[test]
    fn stacked_mandatory_unions_with_conflicts_converge_fast() {
        let cfg = Config {
            unions: vec![uniform(3), uniform(3), uniform(3)],
            plain: vec![0.5, 0.3],
            // (u0,c0) excludes (u1,c0); (u1,c1) excludes (u2,c2).
            conflicts: vec![((0, 0), (1, 0)), ((1, 1), (2, 2))],
        };
        let l = layout(&cfg);
        let mem = members(&cfg);
        let (feasible, mut cat, _naive, illegal) = enumerate(&cfg, &l);
        normalize(&mut cat);

        // Measure convergence with the mirror (real function asserts correctness below).
        let mut probe = cat.clone();
        let sweeps = ipf_mirror(&mut probe, &mem, &feasible, 200);
        eprintln!("stacked-unions+conflicts: converged in {sweeps} sweeps");
        assert!(
            sweeps < 50,
            "interior IPF should converge fast; took {sweeps} sweeps"
        );

        // Correctness via the production function.
        ipf_rescale_sparse(&mut cat, &mem, &feasible);
        assert!(
            illegal_mass(&cat, &illegal) <= 1e-9,
            "illegal mass must stay zero under conflicts"
        );
        for (i, m) in mem.iter().enumerate() {
            assert!(
                (marginal(&cat, i) - m.ratio).abs() < 1e-6,
                "member {i} marginal {} != ratio {} after conflict redistribution",
                marginal(&cat, i),
                m.ratio
            );
        }
    }

    /// Why the categorical factor is needed: the naive product-Bernoulli prior puts real
    /// illegal mass on "no case of a mandatory union", and under a low iteration cap it does
    /// *not* vanish — i.e. it leaks bad rows. The categorical prior is at zero from sweep 0.
    /// (Models the worked example: a mandatory 0.6/0.4 union + one 0.5 plain member.)
    #[test]
    fn naive_prior_leaks_under_low_iteration_cap() {
        let cfg = Config {
            unions: vec![vec![0.6, 0.4]],
            plain: vec![0.5],
            conflicts: vec![],
        };
        let l = layout(&cfg);
        let mem = members(&cfg);
        let (feasible, mut cat, mut naive, illegal) = enumerate(&cfg, &l);
        normalize(&mut cat);
        normalize(&mut naive);

        // Naive prior starts with substantial illegal mass (≈ 0.24 here).
        let naive_prior_leak = illegal_mass(&naive, &illegal);
        assert!(
            naive_prior_leak > 0.2,
            "naive prior should leak (got {naive_prior_leak})"
        );
        assert!(
            illegal_mass(&cat, &illegal) <= TOL,
            "categorical prior must not leak"
        );

        // Under a deliberately low cap the naive leak does not clear.
        let leaked = ipf_mirror(&mut naive, &mem, &feasible, 4);
        eprintln!(
            "naive leak after 4 sweeps: {:.4} (converged flag sweeps={leaked})",
            illegal_mass(&naive, &illegal)
        );
        assert!(
            illegal_mass(&naive, &illegal) > 1e-3,
            "naive illegal mass should still be material under a low cap — this is the leak the \
             categorical factor removes"
        );

        // Categorical prior stays exactly zero regardless of cap (I-projection).
        let _ = ipf_mirror(&mut cat, &mem, &feasible, 4);
        assert!(
            illegal_mass(&cat, &illegal) <= 1e-9,
            "categorical illegal mass stays zero"
        );
    }
}
