# MULT-1: Include multiplicity

## Unified concept

Two parameters jointly control how rows from an included dataset appear in the output:

- **`distribution`** (existing): the fraction of the included dataset's unique rows that contribute. Defaults to 1.0.
- **`multiplicity`** (new): how many times each contributing row appears. Defaults to 1.

Together: `total output rows ≈ D_n × parent_rows × E[M_n]`. For example, `distribution: 0.3, multiplicity: 3` yields a dataset roughly 90% the size of the parent, drawn from 30% of its unique rows, each appearing three times.

Both parameters apply consistently across both include contexts:

| Context | `distribution` | `multiplicity` |
|---------|---------------|----------------|
| Top-level `includes:` on a dataset | fraction of parent rows belonging to this sibling (Bernoulli; drives segmentation) | how many child rows per parent-row slot |
| Nested include `content.includes:` on a `list` field | fraction of the pool eligible for sampling | how many items drawn per outer row (replaces `count` for this case) |

`count` is retained but restricted to **vanilla lists** (list fields with no `content.includes`). Using `count` on a nested-include list becomes a validation error.

## Conceptual model

### Nested include lists

Each list item is a **new generated row** whose include-scoped ref fields (`ref: include_ref.field`) sample their values from an already-computed pool, outer-scoped refs replicate from the enclosing outer row, and plain fields are generated fresh. Multiplicity M_n controls how many such items are produced per outer row.

Concretely, for each outer row:

- A **pool** of `D_n × include.rows` already-computed rows is drawn from the included dataset's batch (`D_n` = `distribution`, defaulting to 1.0).
- `M_n = sample(multiplicity)` new item rows are assembled, each independently sampling one pool row for its include-scoped ref fields.
- The same pool row may be sampled more than once (with-replacement), producing items that share include-sourced values but differ in their fresh fields.

This is the existing `execute_inner_flat` mechanism. MULT-1 renames `count` → `multiplicity` to reflect the shared concept; no execution logic changes for nested includes.

### Top-level includes with multiplicity

When `multiplicity` is set on a top-level `Include`:

1. **Segmentation is unchanged.** `distribution` continues to determine which parent-row slots belong to this child via Bernoulli membership. The segmentation algorithm in `plan_segments` is not affected.

2. **Slot expansion.** For each parent-row slot assigned to this child, `M_n = sample(multiplicity)` child rows are generated instead of the current one. All M_n rows carry the same values for ref-wired fields (pinned by the segment's field constraints); each copy has independently generated values for fresh (non-ref) fields.

3. **Hidden `_slot_idx` column.** Each child row is tagged with a `_slot_idx` value (0..parent_rows-1) identifying its parent-row slot. This is the join key used when assembling the parent, analogous to `_outer_idx` in nested includes.

4. **Parent assembly.** `grow_parent_from_children` groups child rows by `_slot_idx`. Within each group, ref-wired fields are identical by construction — no value conflict. Fresh child fields are child-scoped and do not propagate to the parent. Assembly is therefore always trivial in V1.

5. **Children of multiplied datasets.** If a further dataset includes the multiplied child, it sees the full expanded row set naturally — the multiplied dataset just looks like a larger dataset. The `_slot_idx` column is inherited as a hidden prefill into any such grandchild dataset, providing the foundation for cross-hierarchy interactions in MULT-2 without requiring architectural changes.

## Reference reduction

Within a single slot group, all M_n child rows carry identical ref-wired field values by construction, and fresh child fields do not propagate to the parent. **No value conflict arises during parent assembly in MULT-1.**

The non-trivial reduction case — a field referencing across two includes with different multiplicities — is deferred to MULT-2. The `_slot_idx` hidden column is the mechanism that will make configurable reducers (sum, max, collect-into-list) implementable without revisiting the core architecture.

## Requirements

### YAML

#### Nested include lists — rename `count` → `multiplicity`

```yaml
# vanilla list — count unchanged
- name: tags
  type: list
  count: {min: 1, max: 5}
```

```yaml
# nested include list — before
- name: attendees
  type: list
  count: {min: 1, max: 4}
  content:
    includes:
      - file: people.yaml
        ref: person
        distribution: 0.5
```

```yaml
# nested include list — after
- name: attendees
  type: list
  multiplicity: {min: 1, max: 4}
  content:
    includes:
      - file: people.yaml
        ref: person
        distribution: 0.5    # optional, defaults to 1.0
```

#### Top-level includes — new `multiplicity` key

`distribution` and `multiplicity` may coexist freely on the same include:

```yaml
includes:
  - file: individuals.yaml
    ref: individual
    distribution: 0.3
    multiplicity: {min: 1, max: 3}
```

When both are absent the current 1:1 behaviour is preserved. `multiplicity` accepts the same `CountSpec` forms as `count`: plain integer, `{min, max}`, or `{mean, std_dev}`.

### Models (`models.rs`)

- Add `multiplicity: Option<CountSpec>` to `Include`.
- Add `multiplicity: Option<CountSpec>` to `Field` (alongside the existing `count`).
- No new types — `CountSpec` already covers all three forms.

### Plan (`plan.rs`)

- **Nested includes**: read `field.multiplicity` instead of `field.count` for the per-outer-row draw count when building `ExecutionStep::GenerateInnerFlat`.
- **Top-level multiplicity**: for each segment assigned to a child with `multiplicity`, record the per-slot draw count `M_n = sample(multiplicity)` so the executor knows how many rows to generate per slot. The child dataset must not declare an explicit `rows` — the count is derived.

### Execution (`executor.rs`)

- **`execute_inner_flat`**: read `multiplicity` from the field; logic otherwise unchanged.
- **Top-level multiplicity**: in the segment generation loop, generate `M_n` child rows per parent-row slot. Prepend a `_slot_idx` column (rather than the current `_row_idx`) to the child batch. Pass `_slot_idx` as a hidden prefill to any grandchild datasets that include this child. In `grow_parent_from_children`, group child rows by `_slot_idx` and take one representative per group (ref fields are identical within a group; fresh fields are discarded).

### Fixture YAML updates

All fixture and example YAMLs using `count` on a nested-include list field must be updated to `multiplicity`:

| File | Field |
|------|-------|
| `tests/fixtures/execute/rich_list/events.yaml` | `attendees` |
| `tests/fixtures/execute/bernoulli_rich_list/events.yaml` | `picks` |
| `tests/fixtures/execute/rich_list_plain/records.yaml` | `entries` |
| `tests/fixtures/execute/count_normal/outer.yaml` | `samples` |
| `examples/corporate-registry/organisations.yaml` | `directors` |

### Validation

- `count` on a `list` field with `content.includes` → error: use `multiplicity` instead.
- `multiplicity` on a `list` field without `content.includes` → error: use `count` instead.
- At most one top-level `Include` per dataset may carry `distribution` (prevents conflicting implied row counts across different included parents).
- `multiplicity.min` < 1 → error.
- Uniform form: `min > max` → error.
- Top-level include with `multiplicity` set and explicit `rows` on the same dataset → error: `rows` is derived when `multiplicity` is present.

### Tests

**Nested include rename (regression):**
- All existing nested-include executor and plan tests pass with `multiplicity` in place of `count`.
- A `count` field on a nested-include list now produces a clear validation error.

**Top-level multiplicity — behaviour:**
- Fixed: `multiplicity: 2` → child output has exactly `2 × parent.rows` rows.
- Uniform: `multiplicity: {min: 1, max: 3}` → child row count in `[parent.rows, 3 × parent.rows]`.
- Ref-field consistency: across all M rows for a given slot, ref-wired fields carry identical values.
- Fresh fields vary independently across the M rows for the same slot.
- Combined: `distribution: 0.5, multiplicity: 2` → child row count ≈ `parent.rows` (half the slots, each twice).
- Child-of-multiplied: a dataset including a multiplied child sees the full expanded row set and generates correctly against it.

**Validation:**
- `count` on a nested-include list → error.
- `multiplicity` on a vanilla list → error.
- Multiple top-level includes with `distribution` on the same dataset → error.
- `multiplicity.min: 0` → error.
- Explicit `rows` + `multiplicity` on same dataset → error.

## Future (MULT-2)

- **Cross-include reducers**: when a field refs across two includes with different multiplicities, a configurable reducer (sum, max, min, collect-into-list) resolves the value. The `_slot_idx` hidden column is the foundation. The collect-into-list reducer is the direct prerequisite for REL.
- **Full `_slot_idx` hierarchy propagation**: enabling a grandchild dataset to interact with both a multiplied intermediate and its parent in a single consistent hierarchy.
- **Without-replacement sampling** for nested includes: enforcing item uniqueness within each outer row's list.
