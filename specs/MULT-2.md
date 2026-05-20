# MULT-2: Cross-include reducers and full cardinality hierarchy

Extends MULT-1 to handle fields that ref across two includes with differing cardinalities, and propagates `_slot_idx` fully through multi-level inclusion hierarchies. Also activates the `include.couple` and `content.include.reinforcement` fields introduced (but validated out) in MULT-1.

_Note_: The fundamental architectural tenet that children by inclusion are generated first, and data subsequently accumulates towards parents in segments, never changes.

## New YAML fields

- **`Field.value`**: constants extended to support arbitrary JSON strings or arbitrary YAML. Enables constant values for object-type fields. Additional `fields` constraints can be given alongside an object `value` field; validators check alignment.
- **`Field.default`**: on synthetic datasets — use this value unless the field is prefilled. Supports the same YAML-value forms as `value`, with validators checking alignment with the field definition.
- **`Field.ref`** renamed to **`Field.refs`**, supporting multiple reference bindings. `ref` remains an alias (backward-compat) for a single `refs` entry.
- **`refs[].reducer`**: how values are assembled when the ref'd include has a different cardinality.
- **`refs[].bind`**: used with `reducer: collect` to name the target field on the coupled or outer include.

## Cases

### Case 1 — N:M junction via `include.couple`

When two datasets are in an N:M relationship, the child dataset declares a primary driver (`include`) and a coupled partner (`include.couple`). Each child row pairs one driver slot with one sampled coupled row. `collect` reducer bindings wire values back to the coupled dataset's fields after the junction batch is generated.

*Example*:

`individuals.yaml` (paraphrased):
```yaml
name: individuals
rows: 1000
data:
  - name: full_name
    type: string
    generator: name
```

`organisations.yaml` (paraphrased):
```yaml
name: organisations
rows: 10
data:
  - name: company_name
    type: string
    generator: company_name
  - name: directors
    type: list
    content:
      type: string
    default: []
```

`directorships.yaml`:
```yaml
name: directorships
include:
  file: individuals.yaml
  ref: individuals
  ratio: 0.1
  cardinality: {min: 1, max: 6}
  couple:
    file: organisations.yaml
    ref: organisations
    ratio: 1.0
data:
  - name: company
    type: string
    ref: organisations.company_name
  - name: director
    type: string
    refs:
      - individuals.full_name
      - {bind: organisations.directors, reducer: collect}
```

`include.cardinality: {min: 1, max: 6}` is the hard constraint — each individual generates 1–6 directorship rows. The `collect` reducer on `director` accumulates the directors list back into `organisations.directors` after all directorships are generated.

### Case 2 — Nested-include collect binding

Within a nested-include list, you might want a field on the *pool* dataset to accumulate values from each outer row that drew it. The canonical example: each doctor should know which wards they are assigned to, but the assignment data lives in wards, not in doctors.

*Example*:

`doctors.yaml`:
```yaml
name: doctors
rows: 30
data:
  - name: full_name
    type: string
    generator: name
  - name: title
    type: string
    value: "Dr"
  - name: on_call_list
    type: list
    content:
      type: string
    default: []
```

`wards.yaml`:
```yaml
name: wards
rows: 8
data:
  - name: ward_name
    type: string
  - name: on_call_doctors
    type: list
    content:
      include:
        file: doctors.yaml
        ref: doctors
        cardinality: {min: 2, max: 5}
        ratio: 0.33
      fields:
        - name: doctor
          type: string
          refs: doctors.full_name
        - name: allocated_to
          type: string
          refs:
            - ward_name
            - {bind: doctors.on_call_list, reducer: collect}
```

Here, `doctors.on_call_list` is populated with the ward names each doctor is assigned to — a collect aggregation that runs *after* the inner flat (the assignments) is generated. Because the pool (doctors) must be assembled from the inner flat rather than before it, the planner rewrites this at planning time into a junction-table form (see Mechanism — Case 2).

*Note 1*
This example might be more natural with a string list of on_call_doctors rather than an object list. MULT-3 introduces `project_field` which projects a single field from the nested include, returning a simple-type list.

*Note 2*
`on_call_doctors[].allocated_to` is structurally redundant in the output — its only purpose is to wire `ward_name` into the collect binding on `doctors.on_call_list`. MULT-3's `hidden` flag suppresses it from the final output without removing the binding.

## Reducers

When a field refs a column from an include that has a different cardinality than another include on the same dataset, the M values must be reduced to one. Planned reducers:

- `take-first` — deterministic default (already implicit in MULT-1 parent assembly)
- `sum`, `max`, `min` — scalar aggregation
- `collect` — gather values into a list; validators must ensure the ref'd type is a list whose element type matches the referee's type

Reducer is declared on the field, not the include.

## `reinforcement` — activating without-replacement sampling

`include.couple.reinforcement` and `content.include.reinforcement` are model fields added in MULT-1 but validated as errors until MULT-2. MULT-2 activates them:

- `reinforcement: 0` — without-replacement: each pool row sampled at most once per outer row's list. Fisher-Yates draw of `M_n` items from the eligible pool slice. Error if `total_rows > eligible_pool_size`.
- `reinforcement: 1` — uniform random with-replacement (existing behaviour).
- `reinforcement: > 1` — clumping: preferential re-selection.

`reinforcement` and `ratio` are dual: `expected_appearances = (total_rows / eligible_pool_size) × reinforcement`. Specifying either one determines the other given the planned row counts.

## Mechanism

### Case 1 — top-level N:M with collect

Case 1 builds directly on MULT-1's slot expansion. The driver (`individuals`, `ratio: 0.1, cardinality: 1–6`) determines total rows; the couple (`organisations`, `ratio: 1.0`) is sampled once per junction row. Each row carries `_slot_idx` from the driver and `_pool_idx` from the couple.

The `collect` reducer on `director` means: after all directorship rows are generated, group them by their `_pool_idx` (organisations slot), accumulate `director` values per group into a list, and prefill `organisations.directors`. Concretely:

1. **Generate directorships** (child-first). Each row has `director` (= `individuals.full_name` for that slot) and `company` (= `organisations.company_name` for that couple slot).
2. **Collect phase** (new step, before parent assembly). Group rows by `_pool_idx`. Accumulate `director` values per group. Prefill `organisations.directors`.
3. **Assemble organisations** using `grow_parent_from_children`, which now finds `directors` already prefilled.

The DAG gets a new edge type: a `collect` binding on field F of child C referencing pool field P creates a dependency `C → pool-dataset(P)`, asserting the pool dataset must be assembled *after* C. This is the inverse of the usual direction and must be explicitly accepted by the DAG validator (it is not a cycle — the pool is distinct from C's driver parents).

### Case 2 — nested-include collect: planning-time junction-table rewrite

Case 2 is more complex because the `collect` binding targets a pool dataset's field from inside `content.fields`. The executor cannot simply post-process the nested list — the pool must be assembled from the inner flat.

The solution is a **planning-time rewrite**: the planner synthesises a virtual impl node representing the junction table. No files are written to disk.

**Rewrite steps (performed by the planner, not the user):**

1. **Strip the nested-include list field** from `wards.yaml`. Replace with a plain `list` field (`default: []`). This becomes `wards` in the rewritten plan.
2. **Synthesise the impl node**. Schema = `content.fields` of the stripped field. Includes:
   - Pool include (`doctors`, `ratio: 0.33, cardinality: min 2`) — sampling constraint side.
   - Outer include (`wards`, `ratio: 1, cardinality: 1`) — outer driver side (determines impl row count).
3. **Execute the impl node** as a child of both `wards` and `doctors`. Hidden index columns: `_slot_idx` (which ward row) and `_pool_idx` (which doctor pool row).
4. **Assemble doctors** from impl. Group impl rows by `_pool_idx`; collect `allocated_to` values per doctor into `on_call_list`. Standard `grow_parent_from_children` with collect-prefilled `on_call_list`.
5. **Assemble wards** from impl. Group impl rows by `_slot_idx`; fold into `on_call_doctors` nested list. Standard `AssembleNestedInclude`.

**Direction is structurally encoded — no inference needed.** The outer driver is always the field's parent dataset (wards); the pool is always the `content.include` dataset (doctors). The `collect` binding syntactically names its target field (`doctors.on_call_list`), giving the planner everything it needs to synthesise the impl node.

Validation required: the collect-binding target must be a list field whose element type matches the binding field's type, and the pool dataset must declare a `default` value for rows that never appear in any inner flat row.

## Full `_slot_idx` hierarchy propagation

A grandchild dataset including both a multiplied intermediate and its original parent must see a consistent join key across the hierarchy. Requires `_slot_idx` to be carried as a hidden prefill through all levels.

## Open questions

- **Dual sibling-group membership** (Case 2): the impl node is a child of both the pool dataset (doctors) and the outer dataset (wards). If either is also included by a third dataset, the impl node enters that parent's sibling group, coupling the two groups' segmentation. Proposed v1 constraint: for Case 2, the pool dataset must not be jointly segmented with any other sibling. If joint segmentation of the pool is required, the user must express the relationship via Case 1 (an explicit junction dataset). This is a reasonable v1 restriction.

---

## Implementation Plan (high-level)

Intentionally coarse — enough to sequence work and confirm MULT-1 doesn't snooker MULT-2.
Detailed per-stage breakdown deferred until MULT-1 is merged.

### MULT-1 deliverables that MULT-2 depends on

| MULT-1 output | Why MULT-2 needs it |
|---------------|---------------------|
| `_pool_idx` hidden column in inner flat | Join key for `CollectToPool` — locates which pool row each inner flat row sampled |
| `_slot_idx` unified sentinel | Join key for cross-hierarchy propagation and Case 1 grouping |
| `Include.couple: Option<Couple>` model field | MULT-2 activates couple execution |
| `ContentInclude.reinforcement: Option<f64>` model field | MULT-2 activates reinforcement sampling |
| `expected_cardinality()` helper in `plan.rs` | Reused for multi-include row count estimation |

### 1. New YAML fields — models and deserialization

`lib/models.rs`:
- `Field.ref_field: Option<String>` → `Field.refs: RefsSpec`:
  ```rust
  pub enum RefsSpec {
      Single(String),
      Multi(Vec<RefBinding>),
  }
  pub struct RefBinding {
      pub target:  Option<String>,
      pub bind:    Option<String>,
      pub reducer: Option<Reducer>,
  }
  pub enum Reducer { TakeFirst, Sum, Max, Min, Collect }
  ```
- `Field.default: Option<serde_yaml::Value>` — for pool dataset fields with a collect reducer
- Existing `ref:` key deserializes as `refs:` of length 1 via `#[serde(alias = "ref")]`

### 2. Activate `couple` and pool sampling parameters

Remove the MULT-1 validation errors for `include.couple`, `couple.reinforcement`, `couple.cardinality`, and `content.include.reinforcement`. Add execution paths:

- **`couple` execution** — when the combined-parent batch is built, include the pool dataset's rows; sample one pool row per junction row using the specified sampling mode.
- **Sampling modes** (mutually exclusive on `couple` and `content.include`):
  - `ratio` — sample uniformly from the eligible fraction of pool rows
  - `reinforcement: 0` → Fisher-Yates (without replacement); `reinforcement: 1` → uniform random; `reinforcement > 1` → weighted re-selection
  - `cardinality` (right-sided M) — each pool row must appear M times; total junction rows = `pool.rows × E[M]`; `include.cardinality` must not also be set

**Validation additions:**
- `couple` stanza: at most one of `ratio`, `reinforcement`, `cardinality` — error if more than one set.
- `couple.cardinality` and `include.cardinality` both set → error.
- `couple.cardinality` set: validate `pool.rows × cardinality.min ≥ driver.rows × include.ratio` (pool big enough for minimum appearances).
- `reinforcement: 0` with `total_junction_rows > eligible_pool_size` → planning error.

### 3. Validation extensions

- Each `RefBinding` with `reducer: collect` must target a list field whose element type matches the binding field's type.
- Pool datasets referenced by a collect binding must declare `default:` on the target field.
- Case 2 v1 restriction: if a collect binding is in `content.fields`, the pool dataset must not be jointly segmented with any other sibling.
- `data.default` must be type-compatible with the field definition.
- `reinforcement: 0` with `total_rows > eligible_pool_size` → planning error.

### 4. DAG edges for collect bindings

`lib/graph.rs` — during `build_dag`, scan for collect bindings and add a dependency edge `child → pool_dataset` for each. Validate that this does not create a cycle. This edge asserts the pool dataset must be assembled *after* the child — the inverse of the usual direction — and must be explicitly accepted by the cycle-detection logic.

### 5. Case 1 — top-level couple with collect

**New `ExecutionStep::CollectToPool`:**
```rust
CollectToPool {
    source_path:  PathBuf,
    source_field: String,
    pool_path:    PathBuf,
    pool_field:   String,
    group_by:     String,    // _slot_idx or _pool_idx
}
```

**Plan:** detect collect bindings on top-level fields; insert `CollectToPool` step immediately before the pool dataset's assembly step.

**Executor `execute_collect_to_pool`:** GROUP BY `group_by` on the source batch, `array_agg(source_field)`, write result as a prefill for `pool_field`. Fill gaps (pool rows with no source rows) with the decoded `default` value. Uses DataFusion programmatic API.

### 6. Case 2 — nested-include collect: planning-time junction-table rewrite

**Plan:** detect nested-include list fields with a collect binding in `content.fields`. For each, perform the junction-table rewrite at plan time (no user-facing files written):

1. Replace the nested-include list field on the outer dataset with a plain `list` field with `default: []`.
2. Synthesise a virtual impl dataset (schema = `content.fields`; include = pool include + outer as driver).
3. Emit `GenerateInnerFlat` for the impl node (outer = wards driver, pool = doctors).
4. Emit `CollectToPool` (source = impl flat, group_by = `_pool_idx`, prefill pool field).
5. Emit `AssembleNestedInclude` (fold impl flat into the outer's list column using `_slot_idx`).

**Executor:** `execute_inner_flat` gains an optional `pool_batch: Option<RecordBatch>` parameter. When `Some`, sample from that batch instead of the positional `pool_size` convention. When `None`, existing behaviour is preserved.

### 7. `Field.default` for pool fields

When `CollectToPool` runs, pool rows that appear in zero inner flat rows receive the `default` value rather than `null`. Decoded from `serde_yaml::Value` at execution time.

### 8. Full `_slot_idx` hierarchy propagation

For grandchild-of-multiplied relationships: detect in `build_plan` and wire `_slot_idx` as a hidden `PrefillSource` from the multiplied intermediate into the grandchild's batch. Grandchild reducers use `_slot_idx` for cross-hierarchy grouping.

### 9. Scalar reducers (`sum`, `max`, `min`, `take-first`)

Extend `CollectToPool` (or add a variant) to support DataFusion aggregate expressions for scalar reducers. `take-first` is the explicit no-op default and requires no aggregation.

### 10. Tests

- End-to-end directorships (Case 1): `organisations.directors` populated correctly.
- End-to-end wards/doctors (Case 2): `doctors.on_call_list` correct; wards `on_call_doctors` list correct.
- `Field.default` used for pool rows with zero assignments.
- `reinforcement: 0`: no duplicate pool row within one outer row's list.
- Scalar reducers: sum, max, min of nested values aggregated correctly.

### Key sequencing constraint

`CollectToPool` (items 5–6) requires `_pool_idx` from MULT-1's inner flat and `Couple` execution from item 2. Do not implement `CollectToPool` before MULT-1 is merged and `couple` execution is activated. Everything else (new YAML fields, validation, DAG edges, `Field.default` model) can be scaffolded alongside MULT-1 if desired.