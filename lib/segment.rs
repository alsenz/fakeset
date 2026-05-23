use anyhow::{bail, Result};
use fake::Fake;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::constraints::{FieldConstraints, Merge};
use crate::models::{CountSpec, SyntheticDataset};

/// Default cap on the number of siblings in a single sibling group.
/// Segment enumeration is 2^N, so 16 siblings → 65 536 subsets.
/// Override at plan time via `--max-siblings` if you have the RAM.
pub const DEFAULT_MAX_SIBLINGS: usize = 16;

/// A sibling dataset that includes a common parent, along with its
/// include metadata needed for constraint extraction.
#[derive(Clone, Debug)]
pub struct Sibling {
    pub path: PathBuf,
    /// Dataset after the rewrite pass (ref fields resolved).
    pub dataset: SyntheticDataset,
    /// Fraction of the parent's population this sibling represents.
    pub ratio: f64,
    /// How many child rows to generate per parent-row slot (top-level cardinality).
    pub cardinality: Option<CountSpec>,
    /// The `ref:` name from the include declaration pointing to the parent.
    pub reference: String,
    /// True for nested-include pool siblings (created from `content: {include: {...}}` fields).
    /// Pool siblings do not generate standalone batches — they only contribute field
    /// constraints to the parent's segment generation, and their rows must be placed
    /// first in the parent batch so `GenerateInnerFlat`'s pool_size index is correct.
    pub is_pool: bool,
}

/// A planned generation segment produced by Bernoulli factoring.
///
/// Each segment represents the subset of parent rows that belong simultaneously
/// to a specific combination of siblings (possibly none — the parent-only
/// segment where no sibling claims the row).
#[derive(Debug, Clone)]
pub struct Segment {
    /// Sibling paths whose output files should also receive rows from this
    /// segment (empty ⟹ parent-only rows).
    pub siblings: Vec<PathBuf>,
    /// Row count to generate for this segment.
    pub rows: usize,
    /// Merged constraints from all siblings in `siblings`, keyed by the parent
    /// field name they override. Applied during parent-schema generation for
    /// this segment.
    pub field_constraints: HashMap<String, FieldConstraints>,
}

#[inline]
fn in_subset(mask: usize, i: usize) -> bool {
    mask & (1 << i) != 0
}

/// Plan the product-Bernoulli segments for a parent dataset given its siblings.
///
/// Algorithm:
/// 1. Compute product-Bernoulli weights for all 2^N subset masks (dense, O(N·2^N)).
/// 2. Precompute pairwise sibling conflicts; zero any mask containing a conflicting pair.
///    This eliminates entire lattice regions without constraint computation — critical
///    for one-of/variant groups where nearly all joint masks are infeasible.
/// 3. Sort surviving masks by weight descending; compute constraints sparsely, stopping
///    once the accumulated weight fraction means remaining masks can contribute < 1 row.
/// 4. Apply IPF over the sparse feasible set to restore declared marginal distributions.
/// 5. Bernoulli-round to integer row counts; drop zero-row segments.
pub fn plan_segments(parent_rows: usize, siblings: &[Sibling], max_siblings: usize) -> Result<Vec<Segment>> {
    let n = siblings.len();

    if n > max_siblings {
        bail!(
            "sibling group of {} datasets exceeds the maximum of {} \
             (2^{} = {} subsets would be enumerated). \
             Raise the limit with --max-siblings if you have sufficient RAM.",
            n, max_siblings, n, 1usize << n
        );
    }

    if n == 0 {
        return Ok(vec![Segment {
            siblings: vec![],
            rows: parent_rows,
            field_constraints: HashMap::new(),
        }]);
    }

    let total_subsets = 1usize << n;

    // --- Pass 1: Bernoulli weights (dense) ---
    let mut weights: Vec<f64> = (0..total_subsets)
        .map(|mask| {
            let mut w = 1.0_f64;
            for (i, sib) in siblings.iter().enumerate() {
                w *= if in_subset(mask, i) { sib.ratio } else { 1.0 - sib.ratio };
            }
            w
        })
        .collect();

    // --- Opt A: Pairwise conflict pruning ---
    // Zero any mask that contains a pair of siblings with mutually incompatible
    // constraints. This collapses exponentially many infeasible masks for
    // categorical/one-of groups without touching a single HashMap merge.
    let conflict_masks = precompute_conflicts(siblings);
    for mask in 1..total_subsets {
        if mask_has_conflict(mask, &conflict_masks, n) {
            weights[mask] = 0.0;
        }
    }

    let post_conflict_weight: f64 = weights.iter().sum();
    if post_conflict_weight <= 0.0 {
        return Ok(vec![Segment { siblings: vec![], rows: parent_rows, field_constraints: HashMap::new() }]);
    }

    // --- Opt B: Sorted-budget sparse constraint computation ---
    // Process masks in descending weight order. Stop once accumulated feasible weight
    // covers all but at most 1 remaining expected row — everything after that is pruned
    // by Bernoulli rounding anyway.
    let mut sorted_masks: Vec<usize> = (0..total_subsets)
        .filter(|&m| weights[m] > 0.0)
        .collect();
    sorted_masks.sort_unstable_by(|&a, &b| {
        weights[b].partial_cmp(&weights[a]).unwrap_or(std::cmp::Ordering::Equal)
    });

    let stop_threshold = post_conflict_weight * (1.0 - 1.0 / parent_rows as f64);
    let mut accumulated_weight = 0.0;
    let mut cut_idx = sorted_masks.len();
    let mut feasible: HashMap<usize, HashMap<String, FieldConstraints>> = HashMap::new();

    for (idx, &mask) in sorted_masks.iter().enumerate() {
        if accumulated_weight >= stop_threshold {
            cut_idx = idx;
            break;
        }
        let constraints = if mask == 0 {
            HashMap::new()
        } else {
            match merge_segment_constraints(mask, siblings, n) {
                Some(c) => c,
                None => { weights[mask] = 0.0; continue; }
            }
        };
        accumulated_weight += weights[mask];
        feasible.insert(mask, constraints);
    }
    // Force-include every singleton mask (one sibling bit set) that survived
    // conflict pruning, regardless of the budget threshold.
    //
    // Budget pruning uses raw Bernoulli weights, which catastrophically
    // underestimate post-IPF contribution for mutually-exclusive siblings:
    // a 4% sibling alongside a 95% peer has raw weight ≈ 0.2% (conditioned on
    // "not in the 95% segment"), but its actual post-IPF marginal is 4%.
    // Joint masks (2+ siblings) can still be budget-pruned — they have many
    // alternatives and Bernoulli rounding will drop near-zero ones anyway.
    for i in 0..n {
        let mask = 1usize << i;
        if weights[mask] > 0.0 {
            feasible.entry(mask).or_insert_with(|| {
                merge_segment_constraints(mask, siblings, n).unwrap_or_default()
            });
        }
    }
    // Zero weights for budget-pruned masks so IPF doesn't count them.
    // Singletons re-added above are in feasible and must not be zeroed.
    for &mask in &sorted_masks[cut_idx..] {
        if !feasible.contains_key(&mask) {
            weights[mask] = 0.0;
        }
    }

    // --- IPF over sparse feasible set ---
    let surviving_total: f64 = feasible.keys().map(|&m| weights[m]).sum();
    if surviving_total <= 0.0 {
        return Ok(vec![Segment { siblings: vec![], rows: parent_rows, field_constraints: HashMap::new() }]);
    }

    if surviving_total < 1.0 - 1e-9 {
        let scale = 1.0 / surviving_total;
        for &m in feasible.keys() {
            weights[m] *= scale;
        }
        ipf_rescale_sparse(&mut weights, siblings, &feasible);
    }

    // --- Bernoulli rounding ---
    let total_weight: f64 = feasible.keys().map(|&m| weights[m]).sum();
    let segments: Vec<Segment> = feasible.keys()
        .filter_map(|&mask| {
            let raw = (weights[mask] / total_weight) * parent_rows as f64;
            let rows = if raw >= 1.0 {
                raw.round() as usize
            } else {
                if (0.0f64..1.0f64).fake::<f64>() < raw { 1 } else { 0 }
            };
            if rows == 0 {
                return None;
            }
            let sibling_paths: Vec<PathBuf> = (0..n)
                .filter(|&i| in_subset(mask, i))
                .map(|i| siblings[i].path.clone())
                .collect();
            Some(Segment {
                siblings: sibling_paths,
                rows,
                field_constraints: feasible[&mask].clone(),
            })
        })
        .collect();

    if segments.is_empty() {
        return Ok(vec![Segment { siblings: vec![], rows: parent_rows, field_constraints: HashMap::new() }]);
    }

    Ok(segments)
}

/// For each sibling i, build a bitmask of all siblings j that are pairwise incompatible
/// with i (their merged constraints are infeasible). Any mask containing such a pair
/// can be eliminated without further constraint computation.
fn precompute_conflicts(siblings: &[Sibling]) -> Vec<usize> {
    let n = siblings.len();
    let constraints: Vec<HashMap<String, FieldConstraints>> =
        siblings.iter().map(sibling_field_constraints).collect();
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

fn constraints_conflict(
    a: &HashMap<String, FieldConstraints>,
    b: &HashMap<String, FieldConstraints>,
) -> bool {
    a.iter().any(|(field, fc_a)| {
        b.get(field).is_some_and(|fc_b| fc_a.merge(fc_b).is_none())
    })
}

fn mask_has_conflict(mask: usize, conflict_masks: &[usize], n: usize) -> bool {
    (0..n).any(|i| in_subset(mask, i) && (mask & conflict_masks[i]) != 0)
}

/// IPF over the sparse feasible set. Iterates only feasible mask keys rather than
/// all 2^N entries, making each round O(K) where K = |feasible| ≪ 2^N.
fn ipf_rescale_sparse(
    weights: &mut Vec<f64>,
    siblings: &[Sibling],
    feasible: &HashMap<usize, HashMap<String, FieldConstraints>>,
) {
    let n = siblings.len();
    const EPS: f64 = 1e-12;
    const TOL: f64 = 1e-9;

    for _ in 0..200 {
        let mut converged = true;
        for i in 0..n {
            let target = siblings[i].ratio;
            let mass_in: f64 = feasible.keys()
                .filter(|&&m| in_subset(m, i))
                .map(|&m| weights[m])
                .sum();
            let mass_out: f64 = feasible.keys()
                .filter(|&&m| !in_subset(m, i))
                .map(|&m| weights[m])
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
                        weights[m] *= scale_in;
                    } else {
                        weights[m] *= scale_out;
                    }
                }
            }
        }
        if converged {
            break;
        }
    }
}

/// Try to merge constraints from all siblings in the given subset bitmask.
/// Returns `None` if any two siblings impose conflicting constraints on the same field.
fn merge_segment_constraints(
    mask: usize,
    siblings: &[Sibling],
    n: usize,
) -> Option<HashMap<String, FieldConstraints>> {
    let mut field_map: HashMap<String, FieldConstraints> = HashMap::new();
    for i in 0..n {
        if !in_subset(mask, i) {
            continue;
        }
        for (field_name, fc) in sibling_field_constraints(&siblings[i]) {
            match field_map.get(&field_name) {
                None => {
                    field_map.insert(field_name, fc);
                }
                Some(existing) => {
                    let merged = existing.merge(&fc)?;
                    field_map.insert(field_name, merged);
                }
            }
        }
    }
    Some(field_map)
}

/// Extract the FieldConstraints this sibling imposes on the parent's fields
/// via its ref fields. The ref field string has the form `include_ref.field_name`;
/// we filter by the sibling's known `include_ref` and return constraints keyed
/// by the parent field name.
pub(crate) fn sibling_field_constraints(sibling: &Sibling) -> HashMap<String, FieldConstraints> {
    let prefix = format!("{}.", sibling.reference);
    let mut map = HashMap::new();
    for field in &sibling.dataset.data {
        if let Some(ref_str) = field.simple_ref() {
            if let Some(parent_field_name) = ref_str.strip_prefix(prefix.as_str()) {
                map.insert(parent_field_name.to_string(), FieldConstraints::from(field));
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Field, FieldType, Format, Range, RefsSpec};
    use serde_yaml::Value as YamlValue;

    fn make_sibling(path: &str, ratio: f64, ref_constraints: Vec<(&str, FieldConstraints)>) -> Sibling {
        let fields = ref_constraints
            .into_iter()
            .map(|(fname, fc)| Field {
                name: fname.to_string(),
                field_type: Some(FieldType::String),
                refs: Some(RefsSpec::Single(format!("parent_ref.{fname}"))),
                generator: fc.generator,
                range: if fc.min.is_some() || fc.max.is_some() {
                    Some(Range { min: fc.min, max: fc.max })
                } else {
                    None
                },
                value: fc.value,
                ..Default::default()
            })
            .collect();

        Sibling {
            path: PathBuf::from(path),
            dataset: SyntheticDataset {
                name: path.to_string(),
                format: Format::Parquet,
                rows: None,
                output_file: None,
                locale: None,
                include: None,
                links: vec![],
                data: fields,
                variants: vec![],
            },
            ratio,
            cardinality: None,
            reference: "parent_ref".to_string(),
            is_pool: false,
        }
    }

    fn value_str(s: &str) -> FieldConstraints {
        FieldConstraints { value: Some(YamlValue::String(s.into())), ..Default::default() }
    }

    fn bounds(min: f64, max: f64) -> FieldConstraints {
        FieldConstraints { min: Some(min), max: Some(max), ..Default::default() }
    }

    // --- single sibling ---

    #[test]
    fn single_sibling_two_segments() {
        // One sibling at 60%: segment {A}=60 rows, parent-only=40 rows.
        let sibs = vec![make_sibling("a", 0.6, vec![])];
        let segs = plan_segments(100, &sibs, DEFAULT_MAX_SIBLINGS).unwrap();
        assert_eq!(segs.len(), 2);
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 100);
        let a_seg = segs.iter().find(|s| !s.siblings.is_empty()).unwrap();
        assert_eq!(a_seg.rows, 60);
    }

    // --- two siblings, compatible constraints ---

    #[test]
    fn two_siblings_four_segments() {
        let sibs = vec![
            make_sibling("a", 0.5, vec![]),
            make_sibling("b", 0.4, vec![]),
        ];
        let segs = plan_segments(100, &sibs, DEFAULT_MAX_SIBLINGS).unwrap();
        // All four subsets survive (no conflicts).
        assert_eq!(segs.len(), 4);
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn two_siblings_rows_sum_to_parent() {
        let sibs = vec![
            make_sibling("a", 0.95, vec![("status", value_str("micro"))]),
            make_sibling("b", 0.04, vec![("status", value_str("small"))]),
        ];
        let segs = plan_segments(1000, &sibs, DEFAULT_MAX_SIBLINGS).unwrap();
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 1000);
    }

    // --- conflict: incompatible constants ---

    #[test]
    fn conflicting_constants_zeroed_and_redistributed() {
        // A says status="micro", B says status="small": joint segment {A,B} conflicts.
        // Both d=0.5 → Σd=1.0. IPF converges to the categorical distribution:
        // w({A})=0.5, w({B})=0.5, w({})→0. Parent-only segment drops out.
        let sibs = vec![
            make_sibling("a", 0.5, vec![("status", value_str("micro"))]),
            make_sibling("b", 0.5, vec![("status", value_str("small"))]),
        ];
        let segs = plan_segments(100, &sibs, DEFAULT_MAX_SIBLINGS).unwrap();
        let joint = segs.iter().find(|s| s.siblings.len() == 2);
        assert!(joint.is_none(), "conflicting joint segment should be absent");
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 100);
        // Marginals should match the declared distributions.
        let a_rows = segs.iter().find(|s| s.siblings == vec![PathBuf::from("a")]).map_or(0, |s| s.rows);
        let b_rows = segs.iter().find(|s| s.siblings == vec![PathBuf::from("b")]).map_or(0, |s| s.rows);
        assert_eq!(a_rows, 50);
        assert_eq!(b_rows, 50);
    }

    // --- compatible bounds ---

    #[test]
    fn overlapping_bounds_merge_into_joint_segment() {
        let sibs = vec![
            make_sibling("a", 0.5, vec![("n", bounds(0.0, 100.0))]),
            make_sibling("b", 0.5, vec![("n", bounds(50.0, 200.0))]),
        ];
        let segs = plan_segments(100, &sibs, DEFAULT_MAX_SIBLINGS).unwrap();
        // All four subsets survive; joint segment should have min=50, max=100.
        let joint = segs.iter().find(|s| s.siblings.len() == 2).unwrap();
        let fc = joint.field_constraints.get("n").unwrap();
        assert_eq!(fc.min, Some(50.0));
        assert_eq!(fc.max, Some(100.0));
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert_eq!(total, 100);
    }

    // --- non-overlapping bounds conflict ---

    #[test]
    fn non_overlapping_bounds_zeroed() {
        let sibs = vec![
            make_sibling("a", 0.5, vec![("n", bounds(0.0, 30.0))]),
            make_sibling("b", 0.5, vec![("n", bounds(60.0, 100.0))]),
        ];
        let segs = plan_segments(100, &sibs, DEFAULT_MAX_SIBLINGS).unwrap();
        // The joint {a,b} segment is infeasible and must be absent.
        let joint = segs.iter().find(|s| s.siblings.len() == 2);
        assert!(joint.is_none());
        // IPF drives the parent-only {} segment toward 0 but never reaches it in
        // finite rounds, so Bernoulli rounding may produce ±1 on the total.
        let total: usize = segs.iter().map(|s| s.rows).sum();
        assert!(total >= 99 && total <= 101, "total should be ~100, got {total}");
    }

    // --- no siblings ---

    #[test]
    fn no_siblings_single_parent_only_segment() {
        let segs = plan_segments(50, &[], DEFAULT_MAX_SIBLINGS).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].rows, 50);
        assert!(segs[0].siblings.is_empty());
    }
}