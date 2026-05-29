# SEG-1 — Branch-and-bound segment enumeration

## Status

Complete — implemented and merged.

## Background

`plan_segments` in `lib/segment.rs` enumerates all membership subsets for a lower
cover group, prunes contradictory subsets, applies IPF over the surviving set, then
rounds to integer row counts.

The current implementation allocates a dense `Vec<f64>` of 2^N entries upfront —
65,536 entries at N=16, 4 GB at N=36 — and guards against N>16 with a hard cap
(`DEFAULT_MAX_LOWER_COVER = 16`).  The pairwise conflict pruning and IPF already
operate on sparse sets, but the weight allocation and the initial `weights[mask] = 0.0`
pruning loop still touch all 2^N entries.

Two pressures make this a blocker:

1. **Schemas are getting larger.** A multi-tier registry with many size classes, or
   an insurance product with many policy types, can plausibly hit 20–30 lower cover
   members.  The hard cap is a user-visible wall that produces no useful output.

2. **VAR-2 virtual expansion.** Two-level variant factoring (see VAR-2.md) needs
   `plan_segments` to be called from inside the executor's per-segment loop for Level
   2 sub-distribution.  Each Level 2 call has a small synthetic "member set" (the
   variant choices of one member), but the outer call at Level 1 may have grown.
   Having a correct O(K) algorithm in place removes the whole class of "2^N blows up"
   concerns from both levels.

The fix is straightforward: replace the dense pass with a depth-first search that
only visits reachable, feasible nodes.

## Algorithm

The replacement (`enumerate_segments_dfs`) visits the same logical space as the
current algorithm but never allocates 2^N entries.

```
enumerate_segments_dfs(
    idx:          usize,                             // current member index (0..N)
    mask:         usize,                             // running inclusion bitmask
    merged:       HashMap<String, FieldConstraints>, // partial constraint merge
    weight:       f64,                               // running Bernoulli weight
    members:      &[LowerCoverMember],
    conflict_masks: &[usize],                        // precomputed pairwise conflict masks
    member_constraints: &[HashMap<...>],             // precomputed per-member constraints
    feasible:     &mut HashMap<usize, (HashMap<...>, f64)>,  // output
):
  if idx == N:
    feasible.insert(mask, (merged, weight))
    return
  // Branch A: exclude member idx
  enumerate_segments_dfs(idx+1, mask, merged.clone(),
                          weight * (1.0 − members[idx].ratio), ...)
  // Branch B: include member idx — prune if already-included member conflicts
  if (mask & conflict_masks[idx]) == 0:
    if let Some(new_merged) = try_merge(merged.clone(), &member_constraints[idx]):
      enumerate_segments_dfs(idx+1, mask | (1<<idx), new_merged,
                              weight * members[idx].ratio, ...)
```

**`try_merge`** is incremental constraint merging: fold `member_constraints[idx]` into
`merged` field by field; return `None` on the first conflict.  This replaces the
per-mask call to `merge_segment_constraints` with an O(F) operation where F is the
number of constrained fields in the new member.

**Singleton guarantee**: DFS naturally visits every path where exactly one member is
included, so the force-include singleton pass is no longer needed.

**Sorted-budget pass**: also removed.  DFS only visits feasible paths; there are no
budget-prune-then-force-include edge cases to handle.

### Complexity

| Scenario | K (feasible segments) | Old | New |
|---|---|---|---|
| N mutually exclusive members | N + 1 | O(N · 2^N) time, O(2^N) mem | O(N²) time, O(N) mem |
| N fully compatible members | 2^N | same | same (K-cap fires first) |
| Typical schema (mix) | K ≪ 2^N | O(N · 2^N) | O(K · N) |

**Stack depth**: N frames per DFS path — safe for all practical N.

## Data structures

The dense `Vec<f64>` disappears entirely.  In its place, DFS populates two parallel
`HashMap`s — both passed as `&mut` to `enumerate_segments_dfs` and handed directly
to the IPF step:

```rust
// populated by DFS
let mut feasible: HashMap<usize, HashMap<String, FieldConstraints>> = HashMap::new();
let mut weights:  HashMap<usize, f64>                               = HashMap::new();

enumerate_segments_dfs(
    0, 0,
    HashMap::new(), // empty partial constraint merge
    1.0,            // initial Bernoulli weight
    members,
    &conflict_masks,
    &member_constraints,
    &mut feasible,
    &mut weights,
)?;
```

The pseudocode above shows a single combined map `HashMap<usize, (constraints, f64)>`
for readability; the actual implementation uses the two-map form to keep the IPF
call site clean.

### `enumerate_segments_dfs` signature

```rust
fn enumerate_segments_dfs(
    idx:              usize,
    mask:             usize,
    merged:           HashMap<String, FieldConstraints>,
    weight:           f64,
    members:          &[LowerCoverMember],
    conflict_masks:   &[usize],
    member_constraints: &[HashMap<String, FieldConstraints>],
    feasible:         &mut HashMap<usize, HashMap<String, FieldConstraints>>,
    weights:          &mut HashMap<usize, f64>,
) -> Result<()>
```

K-cap check goes here, before inserting at the leaf:

```rust
if idx == members.len() {
    if feasible.len() >= MAX_FEASIBLE_SEGMENTS {
        bail!("lower cover group produced more than {} feasible segments …", MAX_FEASIBLE_SEGMENTS);
    }
    feasible.insert(mask, merged);
    weights.insert(mask, weight);
    return Ok(());
}
```

### `try_merge_incremental` signature

```rust
fn try_merge_incremental(
    base:   HashMap<String, FieldConstraints>,
    extra:  &HashMap<String, FieldConstraints>,
) -> Option<HashMap<String, FieldConstraints>>
```

Folds `extra` into `base` field by field; returns `None` on the first conflict.
This is the same merge logic as `merge_segment_constraints` but applied to an
already-partial result rather than rebuilding from a bitmask.

### Updated `ipf_rescale_sparse` signature

```rust
fn ipf_rescale_sparse(
    weights: &mut HashMap<usize, f64>,          // was &mut Vec<f64>
    members: &[LowerCoverMember],
    feasible: &HashMap<usize, HashMap<String, FieldConstraints>>,
)
```

The body is mechanically the same.  Two substitutions throughout:

| Old (Vec) | New (HashMap) |
|---|---|
| `weights[m]` (read) | `weights[&m]` |
| `weights[m] *= scale` (write) | `if let Some(w) = weights.get_mut(&m) { *w *= scale }` |

`in_subset` is kept — it is still used inside `ipf_rescale_sparse` and
`mask_has_conflict`.  The `mask` key type stays `usize` (up to 64 members on
64-bit targets; well above any practical schema).

### Pre-IPF normalisation and Bernoulli rounding

Both steps already iterate `feasible.keys()` — no structural change needed.
The only substitution is `weights[m]` → `weights[&m]` for reads:

```rust
// surviving_total — before IPF
let surviving_total: f64 = weights.values().copied().sum();

// rounding — after IPF
let total_weight: f64 = weights.values().copied().sum();
let raw = (weights[&mask] / total_weight) * parent_rows as f64;
```

## Cap replacement

The N-based cap (`--max-lower-cover`, `DEFAULT_MAX_LOWER_COVER`, the `max_lower_cover`
parameter) is removed.  An internal constant `MAX_FEASIBLE_SEGMENTS` (default:
1,000,000) aborts if the feasible set grows too large:

```
if feasible.len() > MAX_FEASIBLE_SEGMENTS {
    bail!(
        "lower cover group produced more than {} feasible segments. \
         Add conflicting field constraints between members to reduce the feasible set.",
        MAX_FEASIBLE_SEGMENTS
    );
}
```

This triggers only for pathologically large, fully-compatible lower cover groups
(e.g. 20 unconstrained members — 2^20 = 1,048,576).  Typical schemas with
categorical constraints (mutually exclusive tiers, policy types) will have K ≪ 1,000.

## Files

| File | Change |
|------|--------|
| `lib/segment.rs` | Replace `plan_segments` Passes 1–3 with `enumerate_segments_dfs`; remove dense `weights: Vec<f64>` allocation and `in_subset` bit-test on dense range; remove sorted-budget pass and singleton force-include pass; remove `DEFAULT_MAX_LOWER_COVER` constant and `max_lower_cover` parameter; add `MAX_FEASIBLE_SEGMENTS` constant; precompute `member_constraints` before DFS entry; keep `precompute_conflicts`, `constraints_conflict`, `mask_has_conflict`, `lower_cover_field_constraints`, `ipf_rescale_sparse`, Bernoulli rounding; add `try_merge_incremental` helper |
| `lib/plan.rs` | Remove `max_lower_cover` parameter from `build_plan`; remove all `plan_segments(…, max_lower_cover)` call sites |
| `src/main.rs` | Remove `--max-lower-cover` CLI flag and `max_lower_cover` struct field |

## Test plan

All existing `segment::tests` must continue to pass without modification.

Add:

- `dfs_N20_mutually_exclusive` — 20 members each with a distinct `status` constant;
  all pairs conflict.  Assert K = 21 segments (20 singletons + empty remainder);
  assert row total = parent_rows; assert each marginal ≈ declared ratio.

- `dfs_N20_fully_compatible` — 20 members with no constraints.  Assert that
  `plan_segments` returns `Err(...)` containing "feasible segments" (K-cap fires).

- `dfs_incremental_constraint_merge` — 3 members where members A and B are
  compatible but member C conflicts with A.  Assert joint {A,B} segment exists with
  merged bounds; assert {A,C} and {A,B,C} are absent; assert {B,C} exists.
