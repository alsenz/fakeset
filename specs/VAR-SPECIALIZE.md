# VAR-SPECIALIZE — Child specialisation of variant fields

## Status

Future — needs design sign-off.  Depends on VAR-2.

## What this is

Allow a child dataset to restrict a parent's `type: variant` field to a subset of
the parent's variant choices.

**Example**: `animals.yaml` declares `eats: type: variant [birds, mice, grass, fish]`.
`cats.yaml` includes `animals.yaml` and specialises `eats` to `[birds, mice]` — cats
never eat grass or fish.  Any row in the `cats` subset will have
`eats ∈ {birds, mice}`; the parent `animals` dataset still covers all four.

## Why it needs its own spec (and its own IPF pass)

VAR-2 introduces Level 2 inner variant sub-distribution.  In VAR-2 as implemented,
Level 2 uses direct proportional distribution: surviving variants are renormalised
and rows distributed proportionally.  No IPF is needed because the only pruning is
a simple feasibility check (`constraints_conflict`).

VAR-SPECIALIZE changes this.  When a child specialises a parent's variant field, the
specialisation constraint arrives at Level 2 as a `FieldConstraints` value pinning
the variant field's value to the allowed subset.  Level 2 then prunes incompatible
variants — and the surviving weights no longer sum to the original marginals.
Specifically:

- At Level 1 (outer Bernoulli), a joint segment may include the specialising child
  alongside other lower cover members.  Each of those other members may further
  constrain the same field (e.g. a sibling that pins `eats = "grass"` conflicts
  entirely with the `cats` specialisation).
- At Level 2 (inner variant distribution), the pruned variant weights must be
  redistributed to restore the declared marginals — exactly the IPF step that
  `plan_segments` already performs at Level 1.

The implementation of Level 2's IPF pass is structurally identical to `plan_segments`:
enumerate feasible (variant, joint-sibling-constraint) combinations via
branch-and-bound DFS (SEG-1 machinery), prune infeasible ones, run IPF over the
surviving set, Bernoulli-round to integer row counts.  The same functions
(`constraints_conflict`, `try_merge_incremental`, `ipf_rescale_sparse`) should be
reused rather than duplicated.

## Dependencies

| Spec | Reason |
|------|--------|
| SEG-1 (complete) | Branch-and-bound + IPF machinery required for Level 2 |
| VAR-2 | Level 2 inner variant sub-distribution must exist before its distribution algorithm can be upgraded to IPF |

## What needs to be designed

### 1. YAML syntax for specialisation

Option A — inline restriction on the child's ref field:
```yaml
# cats.yaml
include:
  file: animals.yaml
  ref: animal
data:
  - name: eats
    refs: animal.eats
    variants: [birds, mice]   # restrict to this subset
```

Option B — a `restrict:` key on the include stanza:
```yaml
include:
  file: animals.yaml
  ref: animal
  restrict:
    eats: [birds, mice]
```

Option A is more consistent with how other field constraints are expressed.
Option B keeps the include stanza self-contained.  **Decision needed.**

### 2. Constraint propagation

Whichever syntax is chosen, `resolve_refs` (or a new pre-execution rewrite pass)
must translate the variant restriction into a `FieldConstraints` that pins `eats`
to one of the allowed values.  This is a set-valued constraint rather than a single
value — `FieldConstraints` currently only holds a single `value: Option<YamlValue>`.
Either:

- Add `allowed_values: Option<Vec<YamlValue>>` to `FieldConstraints` (new field,
  backward-compatible default `None`).
- Or encode the restriction as multiple sibling `FieldConstraints` (one per allowed
  value), but this requires set-union semantics in `Merge` rather than intersection.

**Decision needed before implementation.**

### 3. Level 2 IPF upgrade

When VAR-SPECIALIZE constraints are in play, upgrade the Level 2 distribution in
`generate_member_batch` from `resolve_distributions + distribute_rows` to a full
branch-and-bound + IPF pass.  This should be a drop-in replacement at the Level 2
call site in `executor.rs`; the interface matches `plan_segments` (returns
`Vec<Segment>` where each segment corresponds to one surviving variant).

### 4. Validation

Add a check in `validate.rs` that:
- A variant restriction only names choices that exist in the parent's variant field.
- The restriction is non-empty.
- The restriction is a strict subset (not the full set — that's a no-op).

## Files (preliminary)

| File | Expected change |
|------|----------------|
| `lib/models.rs` | Add `allowed_values` to `FieldConstraints` (if Option A syntax chosen) |
| `lib/constraints.rs` | `Merge` for `allowed_values` (intersection); `Satisfiable` for empty intersection |
| `lib/validate.rs` | Variant restriction validation checks |
| `lib/rewrite.rs` | `resolve_refs`: propagate variant restriction into `FieldConstraints` on the child's ref field |
| `lib/executor.rs` | Upgrade Level 2 in `generate_member_batch` from proportional distribution to branch-and-bound + IPF |
| `src/docgen.rs` | Document new YAML syntax |
| `docs/…/yaml-schema.mdx` | New YAML field entry |
