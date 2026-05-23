# MULT-1: Include cardinality

## Unified concept

Two parameters jointly control how rows from an included dataset appear in the output:

- **`ratio`** (replaces `distribution`): the fraction of the included dataset's unique rows that contribute. Defaults to 1.0.
- **`cardinality`** (new): how many times each contributing row appears. Defaults to 1.

Together: `total output rows ≈ ratio × parent_rows × E[cardinality]`. For example, `ratio: 0.3, cardinality: 3` yields a dataset roughly 90% the size of the parent, drawn from 30% of its unique rows, each appearing three times.

Both parameters apply consistently across both include contexts:

| Context | `ratio` | `cardinality` |
|---------|---------|---------------|
| Top-level `include:` on a dataset | fraction of driver parent rows in this child (Bernoulli; drives segmentation) | how many child rows per driver-parent slot |
| Nested list `content.include:` on a `list` field | fraction of the pool eligible for sampling | how many items drawn per outer row |

`count` is retained but restricted to **vanilla lists** (list fields with no `content.include`). Using `count` on a nested-include list becomes a validation error.

_Note_: The fundamental architectural tenet that children by inclusion are generated first, and data subsequently accumulates towards parents in segments, never changes.

## Conceptual model

### Nested include lists

Each list item is a **new generated row** whose include-scoped ref fields sample their values from an already-computed pool, outer-scoped refs replicate from the enclosing outer row, and plain fields are generated fresh. `cardinality` controls how many items are produced per outer row.

Concretely, for each outer row:

- A **pool** of `ratio × include.rows` already-computed rows is eligible for sampling (`ratio` defaults to 1.0).
- `M_n = sample(cardinality)` new item rows are assembled, each independently sampling one pool row for its include-scoped ref fields.
- The same pool row may be sampled more than once (with-replacement by default). Controlling sampling intensity via `reinforcement` is deferred to MULT-2; MULT-1 adds the model field and validates it as unsupported (see §Reinforcement below).

This is the existing `execute_inner_flat` mechanism. MULT-1 renames `count` → `cardinality` on `content.include` to reflect the shared concept; no core execution logic changes for nested includes. The key addition is renaming `_outer_idx` → `_slot_idx` in produced batches (see §Sentinel unification below).

### Top-level includes with cardinality

When `cardinality` is set on a top-level `include`:

1. **Segmentation is unchanged.** `ratio` continues to determine which parent-row slots belong to this child via Bernoulli membership. The segmentation algorithm in `plan_segments` is not affected. All children of a shared parent participate in segmentation unconditionally — including those with `ratio: 1.0`. A ratio-1.0 child's field constraints must enter conflict pruning jointly with its siblings'; excluding it would allow those constraints to silently win via join order rather than correctly zeroing out the conflicting segment.

2. **Slot expansion.** For each parent-row slot assigned to this child, `M_n = sample(cardinality)` child rows are generated. All M_n rows carry the same values for ref-wired fields (pinned by the segment's field constraints); each has independently generated values for fresh (non-ref) fields.

3. **Hidden `_slot_idx` column.** Each child row is tagged with a `_slot_idx` value (0..parent_rows-1) identifying its parent-row slot. This is the join key used when assembling the parent.

4. **Parent assembly.** `grow_parent_from_children` groups child rows by `_slot_idx`. Within each group, ref-wired fields are identical by construction — no value conflict. Fresh child fields are child-scoped and do not propagate to the parent. Assembly is therefore always trivial in V1.

5. **Children of multiplied datasets.** If a further dataset includes the multiplied child, it sees the full expanded row set naturally: `plan_row_counts` computes the grandchild's rows using the expanded count (`ratio × multiplied_parent_rows` where `multiplied_parent_rows = grandparent_rows × E[cardinality]`). No architectural change is required. Propagating `_slot_idx` into grandchild batches for cross-hierarchy ref resolution is deferred to MULT-2 §8; that is a follow-on execution change, not an architectural constraint on MULT-1.

### Sentinel unification

The prior design had two names for the same concept:
- **`_outer_idx`** — which outer row each inner-flat item belongs to (nested list context)
- **`_slot_idx`** — which parent-row slot each multiplied child row belongs to (top-level cardinality context)

Both are **driver-parent slot indices**. MULT-1 renames `_outer_idx` → `_slot_idx` throughout — in `execute_inner_flat`, all produced batches, and all tests. This is a pure rename with no execution-logic change, and eliminates a source of conceptual confusion for MULT-2.

### Reinforcement

`reinforcement` is a continuous sampling-intensity parameter for pool selection, shared by `content.include` and `include.couple`. MULT-1 adds its model fields; execution is deferred to MULT-2.

| Value | Behaviour |
|-------|-----------|
| `0` | Without-replacement: each eligible pool row sampled at most once per outer row |
| `1` | Uniform random with-replacement (current default) |
| `> 1` | Clumping: preferential re-selection of the same pool rows |

`ratio` and `reinforcement` are dual: `expected_appearances = (total_rows / eligible_pool_size) × reinforcement`. `reinforcement: 0` is a hard constraint — planning error if `total_rows > eligible_pool_size`.

## Reference reduction

Within a single slot group, all M_n child rows carry identical ref-wired field values by construction, and fresh child fields do not propagate to the parent. **No value conflict arises during parent assembly in MULT-1.**

The non-trivial reduction case — a field referencing across two includes with different cardinalities — is deferred to MULT-2. The `_slot_idx` and `_pool_idx` hidden columns are the mechanisms that will make configurable reducers (sum, max, collect-into-list) implementable without revisiting the core architecture.

## Requirements

### YAML

#### Structural rename: `includes:` → `include:`

The top-level `includes: [...]` array is replaced by a singular `include: {...}` object. When a dataset needs a second parent (N:M junction), it uses `include.couple:` (supported from MULT-2; model field added in MULT-1 Stage 1, validated as an error until MULT-2).

```yaml
# before
includes:
  - file: people.yaml
    ref: person
    distribution: 0.7

# after
include:
  file: people.yaml
  ref: person
  ratio: 0.7
```

Serde: `ratio` carries `#[serde(alias = "distribution")]` for a one-release migration window.

#### Nested include lists — `content.include` (singular) with `cardinality`

`cardinality` moves from the list field itself onto `content.include`. `content.includes:` (plural array) becomes `content.include:` (singular struct).

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
  content:
    include:
      file: people.yaml
      ref: person
      ratio: 0.5
      cardinality: {min: 1, max: 4}
```

#### Top-level include with cardinality

```yaml
include:
  file: individuals.yaml
  ref: individual
  ratio: 0.3
  cardinality: {min: 1, max: 3}
```

When `cardinality` is absent the current 1:1 behaviour is preserved. Accepts the same `CountSpec` forms: plain integer, `{min, max}`, or `{mean, std_dev}`.

#### `include.couple` — model only (MULT-2)

The `couple` stanza is parsed and validated in MULT-1 (as an unsupported error) but not executed until MULT-2. It carries exactly one of three mutually exclusive sampling parameters:

Option A — `ratio` (pool eligibility fraction):
```yaml
include:
  file: individuals.yaml
  ref: individuals
  cardinality: {min: 1, max: 6}
  couple:
    file: organisations.yaml
    ref: organisations
    ratio: 1.0
```

Option B — `reinforcement` (sampling intensity):
```yaml
  couple:
    file: organisations.yaml
    ref: organisations
    reinforcement: 0.5
```

Option C — `cardinality` (right-sided M; each pool row appears M times; mutually exclusive with `include.cardinality`):
```yaml
  couple:
    file: organisations.yaml
    ref: organisations
    cardinality: {min: 3, max: 15}
```

If none is specified, `ratio: 1.0` is assumed.

### Models (`models.rs`)

**`Include`** — rename and add fields:
```rust
pub struct Include {
    pub file: String,
    pub reference: String,
    #[serde(alias = "distribution")]
    pub ratio: Option<f64>,              // renamed from distribution
    pub cardinality: Option<CountSpec>,  // new; was going to be multiplicity
    pub couple: Option<Couple>,          // new; validated as unsupported until MULT-2
}
```

**New `Couple` struct** (model only; not executed until MULT-2):
```rust
pub struct Couple {
    pub file: String,
    pub reference: String,
    // Exactly one of the three may be set (validated as error until MULT-2):
    pub ratio: Option<f64>,
    pub reinforcement: Option<f64>,
    pub cardinality: Option<CountSpec>,  // right-sided M; mutually exclusive with include.cardinality
}
```

**`ListContent`** — replace `includes: Vec<Include>` with singular:
```rust
pub struct ListContent {
    pub include: Option<ContentInclude>,  // renamed + made singular
    // ... existing scalar item fields ...
}
```

**New `ContentInclude` struct**:
```rust
pub struct ContentInclude {
    pub file: String,
    pub reference: String,
    pub cardinality: Option<CountSpec>,  // how many items per outer row
    pub ratio: Option<f64>,              // pool coverage fraction
    pub reinforcement: Option<f64>,      // sampling intensity (MULT-2; validated as unsupported until then)
}
```

`Field` no longer needs a `multiplicity` field — cardinality now lives on `ContentInclude`. The existing `count: Option<CountSpec>` on `Field` is retained for vanilla lists only.

### Plan (`plan.rs`)

- **Nested includes**: read `content.include.cardinality` instead of `field.count` when building `ExecutionStep::GenerateInnerFlat`. Rename `GenerateInnerFlat.count` field → `cardinality`.
- **Top-level cardinality**: for each segment assigned to a child with `include.cardinality`, record the per-slot draw count so the executor knows how many rows to generate per slot. The child dataset must not declare an explicit `rows`.

### Execution (`executor.rs`)

- **`execute_inner_flat`**: read `content.include.cardinality`; rename `_outer_idx` → `_slot_idx` in all produced `RecordBatch` schemas; persist `sampled_indices[i]` as a hidden `_pool_idx` UInt32 column (forward-compatibility for MULT-2's collect mechanism).
- **Top-level cardinality**: in the segment generation loop, generate `M_n` child rows per parent-row slot, tagging each with `_slot_idx`. Pass `_slot_idx` as a hidden prefill to grandchild datasets. In `grow_parent_from_children`, group child rows by `_slot_idx` and take one representative per group.

### Fixture YAML updates

All fixture and example YAMLs must be updated. Changes per file: `includes:` → `include:`, `distribution:` → `ratio:`, `content.includes:` → `content.include:`, and `count:` on the list field moves to `cardinality:` inside `content.include`.

| File | Fields affected |
|------|----------------|
| `tests/fixtures/execute/rich_list/events.yaml` | `attendees` (count + content.includes) |
| `tests/fixtures/execute/bernoulli_rich_list/events.yaml` | `picks` (count + distribution + content.includes) |
| `tests/fixtures/execute/rich_list_plain/records.yaml` | `entries` (count + content.includes) |
| `../../tests/fixtures/execute/count_normal/outer.yaml` | `samples` (count + content.includes) |
| `../../examples/corporate-registry/organisations.yaml` | `directors` (count + content.includes) |

Also update all top-level `includes:` arrays to `include:` objects throughout the fixture set.

### Validation

- `count` on a `list` field with `content.include` → error: use `cardinality` on `content.include` instead.
- `cardinality` on a `list` field directly (not via `content.include`) → error.
- `content.include` and `content.includes` both present → error.
- `include.couple` present → error: "not yet supported; coming in MULT-2".
- `content.include.reinforcement` present → error: "not yet supported; coming in MULT-2".
- `include.cardinality.min` < 1 → error.
- Uniform form: `min > max` → error.
- Top-level `include.cardinality` set and explicit `rows` on the same dataset → error.

### Tests

**Nested include rename (regression):**
- All existing nested-include executor and plan tests pass with the new YAML shape.
- A `count` field on a nested-include list now produces a clear validation error.
- Inner flat batches contain a `_pool_idx` UInt32 column (0-based pool-row indices).
- Inner flat batches use `_slot_idx`, not `_outer_idx`.

**Top-level cardinality — behaviour:**
- Fixed: `cardinality: 2` → child output has exactly `2 × parent.rows` rows.
- Uniform: `cardinality: {min: 1, max: 3}` → child row count in `[parent.rows, 3 × parent.rows]`.
- Ref-field consistency: across all M rows for a given slot, ref-wired fields carry identical values.
- Fresh fields vary independently across the M rows for the same slot.
- Combined: `ratio: 0.5, cardinality: 2` → child row count ≈ `parent.rows`.
- Child-of-multiplied: a dataset including a multiplied child sees the full expanded row set and generates correctly against it.

**Validation:**
- `count` on a nested-include list → error.
- `cardinality` directly on a list field → error.
- `include.couple` present → error.
- `cardinality.min: 0` → error.
- Explicit `rows` + top-level `include.cardinality` → error.

## Vocabulary mapping

Full table of renamed and restructured YAML fields introduced by MULT-1:

| Old | New | Notes |
|-----|-----|-------|
| `includes: [...]` | `include: {...}` | Singular; coupled datasets go in `couple` (MULT-2) |
| `distribution` | `ratio` | Same semantics, clearer name; `alias = "distribution"` for migration |
| `multiplicity` (top-level) | `cardinality` on `include` | Identical semantics |
| `multiplicity` / `count` (nested list) | `cardinality` on `content.include` | Was on field; moves into `content.include` |
| `count` (vanilla list, no include) | `count` | Unchanged |
| `content.includes: [...]` | `content.include: {...}` | Singular pool per list field |
| `_outer_idx` | `_slot_idx` | Unified sentinel rename; no execution-logic change |

## Future (MULT-2)

- **N:M junction via `include.couple`**: `couple` model field added in MULT-1 but validated as an error until MULT-2 implements execution. `CollectToPool` execution step is the mechanism.
- **Cross-include reducers**: when a field refs across two includes with different cardinalities, a configurable reducer (sum, max, min, collect-into-list) resolves the value. `_slot_idx` and `_pool_idx` are the join keys.
- **`reinforcement` sampling**: the `ratio`/`reinforcement` dual on `content.include` and `couple`; without-replacement sampling (`reinforcement: 0`) deferred to MULT-2.
- **Full `_slot_idx` hierarchy propagation**: enabling a grandchild dataset to interact with both a multiplied intermediate and its parent in a single consistent hierarchy.

---

## Implementation Plan

Staged plan — each stage is a `cargo check` pass; stages marked **tests green** pass the full `cargo test` suite. Complete stages in order.

### Pre-reading: what matters in the current code

| Location | Relevant detail |
|----------|----------------|
| `models.rs` — `Include` | Has `distribution: Option<f64>`. Rename → `ratio`; add `cardinality`, `couple`. |
| `models.rs` — `ListContent` | Has `includes: Vec<Include>`. Replace → `include: Option<ContentInclude>`. |
| `plan.rs` — `GenerateInnerFlat` | Has `count: CountSpec` field — rename to `cardinality`. |
| `plan.rs` — `emit_nested_include_steps` | Reads `field.count` for `GenerateInnerFlat` — change to `content.include.cardinality`. |
| `plan.rs` — `build_sibling_groups` | Registers siblings when `include.distribution.is_some()` — change to register ALL children of a shared parent (effective ratio 1.0 when not declared). |
| `plan.rs` — `plan_row_counts` | Derives child row count from distribution × parent rows — extend to multiply by E[cardinality] when set. |
| `segment.rs` — `Sibling` | Has `distribution: f64`. Rename → `ratio`; add `cardinality: Option<CountSpec>`. |
| `executor.rs` — `execute_inner_flat` | Uses `_outer_idx` sentinel — rename to `_slot_idx`; persist `_pool_idx`. |
| `executor.rs` — `execute_sibling_group` | Generates flat batch per segment — extend for per-slot expansion when `sib.cardinality.is_some()`. |

### Stage 1 — Models (~30 lines, zero test change) ✓ DONE

`../../lib/models.rs`:

Rename `distribution` → `ratio` with serde alias (serde alias preserves old fixture deserialization during the transition):
```rust
#[serde(alias = "distribution")]
pub ratio: Option<f64>,
```

Add to `Include`:
```rust
pub cardinality: Option<CountSpec>,
pub couple: Option<Couple>,
```

New `Couple` struct (model only; deserialization; no execution):
```rust
#[derive(Debug, Deserialize)]
pub struct Couple {
    pub file: String,
    #[serde(default, alias = "ref")]
    pub reference: String,
    // Exactly one of the three may be set (validated as error until MULT-2):
    pub ratio: Option<f64>,
    pub reinforcement: Option<f64>,
    pub cardinality: Option<CountSpec>,  // right-sided M; mutually exclusive with include.cardinality
}
```

Replace `ListContent.includes: Vec<Include>` with:
```rust
pub include: Option<ContentInclude>,
```

New `ContentInclude` struct:
```rust
#[derive(Debug, Deserialize)]
pub struct ContentInclude {
    pub file: String,
    #[serde(default, alias = "ref")]
    pub reference: String,
    pub cardinality: Option<CountSpec>,
    pub ratio: Option<f64>,
    pub reinforcement: Option<f64>,
}
```

`cargo check` passes. No tests change — old fixture YAMLs still parse via serde aliases; new fields default to `None`.

### Stage 2 — Nested include rename + `_pool_idx` + `_slot_idx` ✓ DONE

**`../../lib/plan.rs`**

`GenerateInnerFlat` variant — rename field:
```rust
// before
count: CountSpec,
// after
cardinality: CountSpec,
```

`emit_nested_include_steps` — read `content.include.cardinality`:
```rust
// before
count: field.count.as_ref().cloned().unwrap_or(CountSpec::Fixed(1)),
// after
cardinality: field.content.as_ref()
    .and_then(|c| c.include.as_ref())
    .and_then(|ci| ci.cardinality.as_ref())
    .cloned()
    .unwrap_or(CountSpec::Fixed(1)),
```

**`../../lib/executor.rs`**

`execute_inner_flat` — rename `_outer_idx` → `_slot_idx` throughout:
```rust
// before
ArrowField::new("_outer_idx", DataType::UInt32, false),
// after
ArrowField::new("_slot_idx",  DataType::UInt32, false),
```

Add `_pool_idx` hidden column immediately after constructing `sampled_indices`:
```rust
// After:
let sampled_indices: UInt32Array = ...;

// Add:
let pool_idx_col: ArrayRef = Arc::new(sampled_indices.clone());

// Extend arrow_fields / columns (after _slot_idx):
let mut arrow_fields = vec![
    ArrowField::new("_slot_idx", DataType::UInt32, false),
    ArrowField::new("_pool_idx", DataType::UInt32, false),   // ← new
];
let mut columns: Vec<ArrayRef> = vec![slot_idx_arr, pool_idx_col];   // ← new entry
```

`_pool_idx` is not in `dataset.data`, so `filter_hidden_columns` strips it from all outputs automatically. It is retained in `computed` batches for MULT-2's collect mechanism.

**`../../src/main.rs`** — update the `GenerateInnerFlat` display/print match arm to destructure `cardinality` instead of `count`.

`cargo check` passes. Existing tests **will fail** until Stage 3 updates the fixture YAMLs.

### Stage 3 — Fixture YAML updates ✓ DONE

For each file in the table above, update:
1. `count:` on the list field → remove it (cardinality moves to `content.include`)
2. `content.includes:` array → `content.include:` object
3. `distribution:` → `ratio:` on the include
4. Move `count` value to `cardinality:` inside `content.include`

Also update any top-level `includes: [...]` arrays to `include: {...}` objects (with `#[serde(alias = "distribution")]` the serde alias covers the transition for `distribution`/`ratio`).

**Tests green.**

### Stage 4 — Validation ✓ DONE

Add to the per-field validation loop in `../../lib/validate.rs`:

```
if field.type == List && field.content.include.is_some():
    field.count.is_some() → error: "use `cardinality` on `content.include`, not `count` on the field"

if field.type == List && field.content.include.is_none():
    field.content.include.cardinality.is_some() → error (can't happen structurally)
```

Add dataset-level rules:

```
include.couple.is_some() → error: "`couple` not yet supported; coming in MULT-2"
content.include.reinforcement.is_some() → error: "`reinforcement` not yet supported; coming in MULT-2"

for each include with cardinality set:
    dataset.rows.is_some()             → error: "`rows` cannot be set when `include.cardinality` is present"
    Uniform cardinality: min > max     → error
    Uniform cardinality: min < 1       → error
    Fixed  cardinality: n < 1          → error
```

Add to `../../tests/validate_tests.rs`:
- `count` on a nested-include list → error containing `"cardinality"`
- `include.couple` present → error containing `"MULT-2"`
- `cardinality: {min: 0, max: 3}` → error
- `rows` + top-level `include.cardinality` → error

**Tests green.**

### Stage 5 — Top-level cardinality: plan ✓ DONE

**`../../lib/segment.rs`** — `Sibling`: rename `distribution: f64` → `ratio: f64`; add `pub cardinality: Option<CountSpec>`

**`../../lib/plan.rs`** — `build_sibling_groups`: register ALL children of a shared parent, regardless of whether `ratio` or `cardinality` is set. A child with no explicit `ratio` has effective ratio 1.0 — its Bernoulli probability is 1, meaning it is always present in every segment. This is not a no-op: its field constraints must enter conflict pruning so that e.g. a ratio-1.0 child pinning `status: "active"` and a ratio-0.6 sibling pinning `status: "inactive"` correctly zero out the {both present} segment rather than silently resolving via join order.

```rust
for include in dataset.include.iter() {   // singular
    let Some(parent_path) = resolve_include(outer_path, &include.file) else { continue };
    // Register unconditionally — even ratio 1.0 children must participate in conflict
    // pruning so their field constraints are applied jointly with sibling constraints.
    groups.entry(parent_path).or_default().push(Sibling {
        path: outer_path.clone(),
        dataset: dataset.clone(),
        ratio: include.ratio.unwrap_or(1.0),
        reference: include.reference.clone(),
        is_pool: false,
        cardinality: include.cardinality.clone(),
    });
}
```

**`../../lib/plan.rs`** — `collect_pool_siblings`: applies the same fix. Currently the closure guards on `let Some(d) = inc.distribution else { return }`, which skips pools without an explicit distribution. The same "all children participate" principle applies: a pool with no explicit `ratio` has effective ratio 1.0, and its field constraints must still enter conflict pruning. Remove the guard; default ratio to `inc.ratio.unwrap_or(1.0)`.

**`../../lib/plan.rs`** — `plan_row_counts`: add helper and apply when `include.cardinality` is set:

```rust
fn expected_cardinality(spec: &CountSpec) -> f64 {
    match spec {
        CountSpec::Fixed(n)             => *n as f64,
        CountSpec::Uniform { min, max } => (*min + *max) as f64 / 2.0,
        CountSpec::Normal  { mean, .. } => *mean,
    }
}
// In rows_from_includes, when include.cardinality is Some:
let rows = (base_rows as f64 * expected_cardinality(card)).round() as usize;
```

`cargo check` passes. No test regressions — `cardinality: None` is the default for all existing siblings.

### Stage 6 — Top-level cardinality: execution ✓ DONE

**`../../lib/executor.rs`** — new helper `generate_expanded_batch`:

Generates M_n rows per parent-row slot, tagging each with `_slot_idx = slot_offset + slot_i`. Returns the full expanded batch. A separate canonical batch (one-row-per-slot) is still passed to `grow_parent_from_children` — that function is unchanged.

```rust
fn generate_expanded_batch(
    fields: &Schema,
    slot_count: usize,
    constraints: &HashMap<String, FieldConstraints>,
    cardinality: &CountSpec,
    slot_offset: usize,
) -> Result<RecordBatch> {
    let mut slot_tags: Vec<u32> = Vec::new();
    let mut slot_batches: Vec<RecordBatch> = Vec::new();

    for i in 0..slot_count {
        let m_n = sample_count(cardinality).max(1);
        let batch = generate_fresh_batch(fields, m_n, constraints)?;
        let slot = (slot_offset + i) as u32;
        slot_tags.extend(std::iter::repeat(slot).take(m_n));
        slot_batches.push(batch);
    }

    let inner_schema = slot_batches.first().map(|b| b.schema())
        .unwrap_or_else(|| Arc::new(schema_to_arrow(fields)));
    let combined = concat_batches(&inner_schema, &slot_batches)?;
    let slot_col: ArrayRef = Arc::new(UInt32Array::from(slot_tags));
    let mut ext_fields = vec![Arc::new(ArrowField::new("_slot_idx", DataType::UInt32, false))];
    ext_fields.extend(combined.schema().fields().iter().cloned());
    let mut cols = vec![slot_col];
    cols.extend(combined.columns().iter().cloned());
    Ok(RecordBatch::try_new(Arc::new(ArrowSchema::new(ext_fields)), cols)?)
}
```

**`execute_sibling_group`** — add `slot_offset: usize = 0` before the segment loop. For each sibling with `cardinality`, generate both the canonical batch (parent assembly) and the expanded batch (output):

```rust
} else {
    let canonical = generate_fresh_batch(&sib.dataset.data, seg.rows, &seg.field_constraints)?;

    if let Some(ref card) = sib.cardinality {
        let expanded = generate_expanded_batch(
            &sib.dataset.data, seg.rows, &seg.field_constraints, card, slot_offset,
        )?;
        sibling_buffers.entry(sib.path.clone()).or_default().push(expanded);
        child_batches.push((sib, canonical));
    } else {
        sibling_buffers.entry(sib.path.clone()).or_default().push(canonical.clone());
        child_batches.push((sib, canonical));
    }
}
slot_offset += seg.rows;
```

**`_slot_idx` in `computed`:** the existing end-of-step code stores the shuffled batch in `computed` and the filtered (output) batch in the emit queue. `filter_hidden_columns` removes `_slot_idx` from emitted output (it is not in `dataset.data`) while `computed` retains it for grandchild access.

### Stage 7 — Tests ✓ DONE

**New test fixtures** under `../../tests/fixtures/execute`:

- `mult1_fixed/` — parent (10 rows) + child with `cardinality: 2`. Assert child has exactly 20 rows.
- `mult1_range/` — `cardinality: {min: 1, max: 3}`. Assert child row count in `[10, 30]`.
- `mult1_ratio_card/` — `ratio: 0.5, cardinality: 2`. Assert child row count ≈ 10.
- `mult1_grandchild/` — grandparent → multiplied parent → child. Assert grandchild sees all expanded parent rows.

**New executor tests** (`../../tests/executor_tests.rs`):
- `test_mult1_fixed_row_count` — exactly 2 × parent.rows
- `test_mult1_range_row_bounds` — within [parent.rows, 3 × parent.rows]
- `test_mult1_ref_field_consistency` — same ref value across all M rows for a slot
- `test_mult1_fresh_field_varies` — fresh field differs across M rows in a slot
- `test_mult1_combined_ratio_card` — ratio × cardinality ≈ parent.rows
- `test_mult1_grandchild_sees_full_batch` — grandchild row count uses full expanded parent
- `test_inner_flat_slot_idx` — inner flat batches use `_slot_idx`, not `_outer_idx`
- `test_inner_flat_pool_idx` — inner flat batches contain `_pool_idx` UInt32 column

**Tests green.**

### Summary: all touched files

| File | Change |
|------|--------|
| `../../lib/models.rs` | Rename `distribution` → `ratio` (alias); add `cardinality`, `couple` to `Include`; replace `ListContent.includes` → `include: ContentInclude`; new `Couple`, `ContentInclude` structs |
| `../../lib/segment.rs` | Rename `distribution` → `ratio`; add `cardinality` to `Sibling` |
| `../../lib/plan.rs` | Rename `GenerateInnerFlat.count` → `cardinality`; update `build_sibling_groups` and `plan_row_counts` |
| `../../lib/validate.rs` | 6 new validation rules |
| `../../lib/executor.rs` | `_outer_idx` → `_slot_idx` rename; `+_pool_idx` in inner flat; `+generate_expanded_batch`; extend `execute_sibling_group` |
| `../../src/main.rs` | Match arm rename only |
| 5+ fixture YAMLs | YAML shape update (includes → include, count → cardinality on content.include) |
| `../../tests/executor_tests.rs` | 8 new tests |
| `../../tests/validate_tests.rs` | 4 new tests |

Estimated net new code: ~130 lines across `executor.rs` and `plan.rs`; ~40 lines in `validate.rs`.
No existing function is rewritten — all changes are additive or renaming.