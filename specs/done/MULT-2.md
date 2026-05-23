# MULT-2: Cross-include reducers and full cardinality hierarchy

Extends MULT-1 (and the structural redesign from MULT-2a) to handle fields that ref across two includes with differing cardinalities, propagates `_slot_idx` fully through multi-level inclusion hierarchies, and activates junction link execution (`links:` entries without a `group:` reference) with collect reducer support.

_Note_: The fundamental architectural tenet that children by inclusion **and by linking** are always executed first — and that data subsequently accumulates towards parents and pool nodes — never changes. Links are constraint specialisations just as includes are; pool datasets are assembled from their atoms, not generated in isolation.

**Prerequisites:** MULT-1 and MULT-2a complete.

## New YAML fields

- **`Field.value`**: constants extended to support arbitrary JSON strings or arbitrary YAML. Enables constant values for object-type fields. Additional `fields` constraints can be given alongside an object `value` field; validators check alignment.
- **`Field.default`**: on synthetic datasets — use this value unless the field is prefilled. Supports the same YAML-value forms as `value`, with validators checking alignment with the field definition.
- **`Field.ref`** renamed to **`Field.refs`**, supporting multiple reference bindings. `ref` remains an alias (backward-compat) for a single `refs` entry.
- **`refs[].reducer`**: how values are assembled when the ref'd include has a different cardinality.
- **`refs[].bind`**: used with `reducer: collect` to name the target field on the linked or outer include.

## Cases

### Case 1 — N:M junction via `links`

When two datasets are in an N:M relationship, the junction dataset declares a primary driver (`include`) and one or more pool partners (`links`). Each junction row pairs one driver slot with one sampled row from each link. `collect` reducer bindings wire values back to a linked dataset's fields after the junction batch is generated.

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
links:
  - file: organisations.yaml
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
links:
  - file: doctors.yaml
    ref: doctors
    cardinality: {min: 2, max: 5}
    ratio: 0.33
data:
  - name: ward_name
    type: string
  - name: on_call_doctors
    type: list
    content:
      group: doctors
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

Here, `doctors.on_call_list` is populated with the ward names each doctor is assigned to — a collect aggregation that runs *after* the inner flat (the assignments) is generated.

*Note 1*
`on_call_doctors[].allocated_to` is structurally redundant in the output — its only purpose is to wire `ward_name` into the collect binding on `doctors.on_call_list`. MULT-3's `hidden` flag (already implemented in MULT-1, test coverage added in MULT-3) suppresses it from the final output without removing the binding.

*Note 2*
A simpler scalar list of doctor names is achievable once MULT-3's `project: doctors.full_name` feature is implemented. In MULT-2 the struct list form is sufficient for correctness testing.

## Reducers

When a field refs a column from a link that has a different cardinality than the driver, the M values must be reduced to one. Planned reducers:

- `take-first` — deterministic default (already implicit in MULT-1 parent assembly)
- `sum`, `max`, `min` — scalar aggregation
- `collect` — gather values into a list; validators must ensure the ref'd type is a list whose element type matches the referee's type

Reducer is declared on the field, not the link.

## `reinforcement` — activating without-replacement sampling

`links[i].reinforcement` is a model field added in MULT-1 but validated as an error until MULT-2. MULT-2 activates it:

- `reinforcement: 0` — without-replacement: each pool row sampled at most once per outer row's list. Fisher-Yates draw of `M_n` items from the eligible pool slice. Error if `total_rows > eligible_pool_size`.
- `reinforcement: 1` — uniform random with-replacement (existing behaviour).
- `reinforcement: > 1` — clumping: preferential re-selection.

`reinforcement` and `ratio` are dual: `expected_appearances = (total_rows / eligible_pool_size) × reinforcement`. Specifying either determines the other given the planned row counts.

## Mechanism

### Case 1 — top-level N:M with collect

Case 1 is the lattice accumulation model in its purest form. Directorships are the atoms — each is a (driver-slot, pool-slot) pair. Pool pre-generation (`organisations`) materialises the pushed-down pool-slot constraint solution before atoms are generated; the atoms then carry pool-scoped field values by indexing into that pre-solved batch via `_pool_idx`. After atoms are generated, values accumulate upward to the pool node (`organisations.directors`) via `CollectToPool` — the symmetric operation to `grow_parent_from_children`.

The driver (`individuals`, `ratio: 0.1, cardinality: 1–6`) determines total atom rows; the link (`organisations`, `ratio: 1.0`) is sampled once per atom using `_pool_idx` as the join key. Concretely:

1. **Generate organisations** (pool, child-first; no includes). Batch lands in `computed` without `directors` filled. Output file emission is *deferred* (see collect pre-scan in Stage 3).
2. **Generate directorships** (child of driver `individuals`). Each row has `director` (= `individuals.full_name` for that slot) and `company` (= `organisations.company_name` for that link slot, via `_pool_idx`).
3. **Collect phase** (`CollectToPool` step). Group directorship rows by `_pool_idx`. Accumulate `director` values per group. Prefill `organisations.directors` in `computed`.
4. **Emit organisations** (`EmitDataset` step). Read the now-updated `computed[organisations]`, apply `filter_hidden_columns`, and write the output file.
5. **Assemble individuals** from directorships using `grow_parent_from_children` — unaffected by the collect step.

Emission ordering is a **plan-layer** concern, not a DAG-topology change. The DAG already places `organisations` before `directorships` via normal topological sort (`organisations` has no `include:`, so it is a leaf). `build_plan` pre-scans for collect targets and defers their emit; no inverse edges are added and no cycle-detector changes are needed.

### Case 2 — nested-include collect

Case 2 applies the same lattice accumulation mechanism as Case 1, but atoms are generated by `GenerateInnerFlat` rather than a junction dataset step. The inner flat already contains both `_slot_idx` (which outer/ward atom group) and `_pool_idx` (which pool/doctor slot) from MULT-1 — each inner flat row is already a (ward-slot, doctor-slot) atom. `CollectToPool` is inserted between `GenerateInnerFlat` and `AssembleNestedInclude` to accumulate atom-level values upward to the pool node before the outer is assembled.

The only additions are:

1. **Mark the pool dataset** (`doctors`) with `skip_emit: true` in its `GenerateDataset` step.
2. **After `GenerateInnerFlat`:** emit `CollectToPool` — groups by `_pool_idx`, accumulates the collect-bound field values into the pool field.
3. **Then `EmitDataset[doctors]`** — writes the updated pool batch.
4. **Then `AssembleNestedInclude`** — folds the inner flat into the outer's list column using `_slot_idx`, then emits the outer (`wards`). Unchanged.

No "virtual impl dataset" synthesis is needed. `GenerateInnerFlat` already carries all sentinel columns; only the planning step order changes.

## Full `_slot_idx` hierarchy propagation

A grandchild dataset including both a multiplied intermediate and its original parent must see a consistent join key across the hierarchy. Requires `_slot_idx` to be carried as a hidden prefill through all levels.

## Open questions

- **Dual sibling-group membership** (Case 2): the inner flat is effectively a child of both the pool dataset (doctors) and the outer dataset (wards). If either is also included by a third dataset, the inner flat enters that parent's sibling group, coupling the two groups' segmentation. Proposed v1 constraint: for Case 2, the pool dataset must not be jointly segmented with any other sibling. If joint segmentation is required, the user must express it as an explicit top-level junction dataset (Case 1). This is a reasonable v1 restriction.

---

## MULT-1 and MULT-2a deliverables used by MULT-2

| Deliverable | Source | Why MULT-2 needs it |
|-------------|--------|---------------------|
| `_pool_idx` in inner flat | MULT-1 | Join key for `CollectToPool` — locates which pool row each inner flat row sampled |
| `_slot_idx` unified sentinel | MULT-1 | Join key for cross-hierarchy propagation and Case 1 grouping |
| `Include.reinforcement: Option<f64>` model field | MULT-2a | MULT-2 activates reinforcement sampling (field promoted to `Include` in MULT-2a from `Couple`/`ContentInclude`) |
| `expected_cardinality()` helper | MULT-1 | Reused for multi-include row count estimation |
| `SyntheticDataset.links: Vec<Include>` | MULT-2a | MULT-2 activates junction link execution |
| `ListContent.group: Option<String>` | MULT-2a | MULT-2 activates nested-include collect via group-referenced links |

---

## Implementation Plan

### Lessons carried forward from MULT-1

1. **Model-first isolation.** Stage 1 must produce a `cargo check` with zero errors and zero test failures before any semantic work starts. Serde aliases make this possible without fixture YAML changes.
2. **Validation errors for unactivated features drop in the same stage as the execution, never earlier.** Junction links are validated as errors in MULT-2a until Stage 4 activates them. Scalar reducers similarly deferred until Stage 7.
3. **Watch plan tests after any sibling-registration change.** Any change touching `build_sibling_groups` or DAG ordering needs plan test re-runs immediately.
4. **DataFusion programmatic API for all new executor code.** Use `DataFrame.aggregate()`, not raw SQL strings, for `CollectToPool`. See CLAUDE.md.
5. **Strip-sentinel helpers are no-ops when the column is absent.** `strip_slot_idx` follows this contract. New helpers (`strip_pool_idx` if needed) should do the same.
6. **`cargo check` after every stage; `cargo test` before moving on.**
7. **Staged verification order:** Model → Validation → Plan → Executor → Tests. Never jump stages.

---

### Stage 1 — `refs` model migration ✓ DONE

**Context:** The biggest single change in MULT-2. `Field.ref_field: Option<String>` becomes `Field.refs: Option<RefsSpec>` to support multi-ref bindings (the collect reducer target). This touches every module that reads `ref_field`. The serde alias means no fixture YAML changes — zero test failures after this stage.

**`../../lib/models.rs`:**

Add new types:
```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RefsSpec {
    Single(String),
    Multi(Vec<RefEntry>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RefEntry {
    Simple(String),
    Rich(RefBinding),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefBinding {
    pub target:  Option<String>,   // ref target ("include_ref.field")
    pub bind:    Option<String>,   // collect target ("pool_ref.field")
    pub reducer: Option<Reducer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reducer { TakeFirst, Sum, Max, Min, Collect }
```

Rename the field and change its type in `Field`:
```rust
// Before: #[serde(rename = "ref")] pub ref_field: Option<String>,
#[serde(alias = "ref")]
pub refs: Option<RefsSpec>,
```

Add accessors on `Field`:
```rust
/// Returns the simple ref string — the first non-bind target from `refs`, if any.
pub fn simple_ref(&self) -> Option<&str> {
    match &self.refs {
        Some(RefsSpec::Single(s)) => Some(s.as_str()),
        Some(RefsSpec::Multi(entries)) => entries.iter().find_map(|e| match e {
            RefEntry::Simple(s)      => Some(s.as_str()),
            RefEntry::Rich(b)        => b.target.as_deref(),
        }),
        None => None,
    }
}

/// Returns all collect bindings declared on this field.
pub fn collect_bindings(&self) -> Vec<&RefBinding> {
    match &self.refs {
        Some(RefsSpec::Multi(entries)) => entries.iter().filter_map(|e| match e {
            RefEntry::Rich(b) if matches!(b.reducer, Some(Reducer::Collect)) => Some(b),
            _ => None,
        }).collect(),
        _ => vec![],
    }
}
```

**All other modules** (`validate.rs`, `rewrite.rs`, `executor.rs`, `plan.rs`, `expressions.rs`, `graph.rs`): replace every `field.ref_field.as_deref()` / `field.ref_field.as_ref()` / `&field.ref_field` with `field.simple_ref()`. This is a mechanical find-replace — one commit.

**`../../lib/rewrite.rs` — multi-ref handling:** `resolve_field` currently uses `ref_field` to find the type source. With multi-ref, `RefEntry::Rich` entries that carry only `{bind, reducer}` (no `target`) are planning annotations, not type sources — they must be skipped. Add `field.type_source_ref()` (or extend `simple_ref()`) that exclusively returns the type-sourcing ref, and use it in `resolve_field` / `resolve_to_base`. The `bind` targets in collect bindings are resolved during planning (Stage 3), not the rewrite pass.

**`../../lib/rewrite.rs` — link-scoped ref resolution:** `resolve_field` and `resolve_to_base` currently only search `dataset.include` for the named ref. In MULT-2, a field may ref into a link (e.g. `ref: organisations.company_name` where `organisations` is a link, not the driver include). Extend both functions to search `dataset.links` when `dataset.include` does not match the ref part. The resolved path and included dataset are then looked up the same way.

**Verification:** `cargo check` (zero errors), `cargo test` (all tests pass — serde alias handles YAML backward-compat).

---

### Stage 2 — `Field.default` model + validation extensions ✓ DONE

**Context:** Adds the `default:` field and the new validation rules for refs, default type, and junction link structural constraints.

**`../../lib/models.rs`:**
```rust
// On Field:
pub default: Option<serde_yaml::Value>,
```

**`../../lib/validate.rs`:**

New rules (validate per field, per dataset):

- `refs` with `reducer: collect`: the `bind` target must resolve to a list field on the pool dataset whose element type matches this field's type. Error if the target field is not a list or types are incompatible.
- `refs` with `reducer: collect`: the pool field named by `bind` must declare `default:`. Error otherwise.
- `Field.default` type-compatibility check against the field's declared (or resolved) type. A `default` on a `number` field that is a YAML string is an error; a default on a `list` field must be a YAML sequence.
- Junction link with `cardinality` set → error (cardinality is not meaningful for junction links, which sample exactly one pool row per junction row).
- `validate_ref_target`: extend to also search `dataset.links` when `dataset.include` does not match the ref part. Error message: "no include or link with ref '{}' in this dataset". This is needed so that top-level ref fields pointing into link datasets (e.g. `ref: organisations.company_name`) pass validation.
- Case 2 v1 restriction: if a collect binding appears in a list field's `content.fields`, the pool dataset must not be jointly segmented with any other sibling. Detect at planning time and error: "nested-include collect is not supported when the pool dataset is jointly segmented; use a top-level junction dataset instead."

New validation fixture directories and tests for each rule.

**Verification:** All new validation tests pass; all existing tests pass.

---

### Stage 3 — Collect target pre-scan in `build_plan` ✓ DONE

**Context:** The `collect` reducer requires pool dataset *emission* to be deferred until after the junction batch is generated — but it does not require any change to the DAG or generation ordering. The DAG already places pool datasets (which have no `include:` of their own) before junction datasets via topological sort. `build_plan` pre-scans for collect targets so it can suppress their normal emit and insert `CollectToPool` + `EmitDataset` steps at the right point. The architectural tenet is fully preserved: generation order follows the standard topo-sort; only emission is deferred.

**`../../lib/plan.rs` — pre-scan in `build_plan`:**

Before emitting any steps, walk all datasets and collect the set of collect-target paths:
```
let collect_targets: HashSet<PathBuf> = for each dataset D:
  for each field F in D.data (recurse into content.item.fields):
    for each RefBinding B with reducer: Collect:
      parse split_ref(B.bind) → (pool_ref, pool_field)
      look up pool dataset path: for junction links (Case 1) match pool_ref against D.links[i].reference;
                                 for nested-include links (Case 2) match pool_ref against the link
                                 resolved from content.group
      insert pool dataset path into collect_targets
```

`resolve_collect_bind(dataset, bind_str, all_datasets) -> Option<(PathBuf, String)>` — helper implementing the lookup above. Uses `split_ref` for the ref/field split, then matches the ref part against `dataset.links` entries.

When emitting generation steps (`GenerateDataset`, `GenerateSiblingGroup`): if the dataset's path is in `collect_targets`, set `skip_emit: true`. The batch still lands in `computed` as usual; only the file write is suppressed.

After the junction dataset's step, insert:
1. `CollectToPool` — aggregates the collect-bound field from the junction batch into the pool field.
2. `EmitDataset` — reads the now-updated `computed[pool_path]`, applies `filter_hidden_columns`, and writes the output file.

No changes to `../../lib/graph.rs` or the cycle detector. No new edge types.

**`../../lib/graph.rs` — validation only:** Add a check that a collect bind target (pool dataset) is not also an ancestor of the junction dataset in the normal include graph (which would be a genuine cycle). This is not achievable with well-formed YAML but warrants a clear error.

**Verification:** `cargo test dag_tests`. Add a plan test confirming a dataset with a collect binding produces the correct step sequence: `GenerateDataset[pool, skip_emit=true]` → junction step → `CollectToPool` → `EmitDataset[pool]`.

---

### Stage 4 — Junction link execution and `CollectToPool` (Case 1) ✓ DONE

**Context:** Activates junction link execution end-to-end, including the collect phase. Removes the MULT-2a validation error for junction links without a `group:`. Introduces `ExecutionStep::CollectToPool` (pool-node accumulation from atom values — the upward-accumulation dual of `grow_parent_from_children`) and `ExecutionStep::EmitDataset`. End goal: the directorships example works.

**Execution model for Case 1:**
1. Pool dataset (`organisations`) is generated as a plain dataset (no includes, `rows: 10`). Its batch lands in `computed` without `directors` filled. `skip_emit: true` set by Stage 3 pre-scan.
2. Junction dataset (`directorships`) is generated as a child of its driver (`individuals`). During generation, the executor samples one pool row per junction row using `_pool_idx` as the join key. Pool sampling uses the link's `ratio` for the eligible fraction.
3. `CollectToPool` runs: groups by `_pool_idx`, accumulates the collect-bound field's values, writes the result back into `computed[pool_path]` (replacing the column with the field's `default` scalar for any pool row with zero junction rows).
4. `EmitDataset[pool]` writes the now-updated pool batch.

**`../../lib/plan.rs`:**
- Remove the MULT-2a junction link validation error.
- New `ExecutionStep` variants:
  ```rust
  CollectToPool {
      source_path:  PathBuf,   // junction batch key in computed
      source_field: String,    // field to aggregate
      pool_path:    PathBuf,   // pool dataset to update in computed
      pool_field:   String,    // field to populate
      group_by:     String,    // "_pool_idx" (single link; indexed form deferred to multi-link work)
      reducer:      Reducer,   // Collect for MULT-2; scalar reducers in Stage 7
  }
  EmitDataset {
      path:    PathBuf,
      dataset: Arc<SyntheticDataset>,
  }
  ```
- `build_plan`: when a dataset has junction links, wire `_pool_idx` sampling into its generation step. The pool dataset path is resolved from `dataset.links[i].file` via `resolve_include`. Pool size is derived from `links[i].ratio` against the pool batch row count (same formula as `execute_inner_flat`).

**`../../lib/executor.rs`:**
- **`_pool_idx` in junction sampling** — in the junction dataset's batch generation inside `execute_sibling_group`, after generating each segment's rows, sample one pool row per row: for each row draw `(0u64..n_eligible_slots as u64).fake::<u64>() as u32` (same pattern as `execute_inner_flat`; `n_eligible_slots` is computed from `links[i].ratio × pool_rows`). Collect into a `UInt32Array` and prepend as `_pool_idx` using `prepend_column`. The pool batch `computed[link_path]` must already be present (DAG ordering guarantees this).
- **`execute_collect_to_pool`**: DataFusion programmatic API — `ctx.read_batch(source_batch)?.aggregate(group_exprs, aggr_exprs)?`. Group col: `col(group_by)`. Aggr col: `array_agg(col(source_field))` for `Collect`. Result is a (group_key, aggregated_value) batch. Merge into `computed[pool_path]`: LEFT JOIN pool batch with aggregate result on pool row index; fill unmatched rows with the decoded `Field.default` scalar.
- **`execute_emit_dataset`**: read `computed[path]`, apply `filter_hidden_columns`, call `emit_batch`. Mirrors the tail of the parent-emit path in `execute_sibling_group`.
- **`Field.default` decoding**: `serde_yaml::Value` → Arrow scalar. Handle `null` / absent `default` → Arrow null scalar for the column's type.

**Verification:** `cargo test`. Add executor test fixture `directorships/` asserting `organisations.directors` is correctly populated, and that an organisation with zero directorships gets `default: []`.

---

### Stage 5 — Case 2 — nested-include collect ✓ DONE

**Context:** Activates collect bindings inside list fields' `content.fields`. No "virtual impl dataset" synthesis — `GenerateInnerFlat` already produces atom rows carrying `_slot_idx` (outer-node join key) and `_pool_idx` (pool-node join key) from MULT-1. The nested-include case is syntactic sugar translated at plan time into the same lattice accumulation model as Case 1. The only additions are: inserting `CollectToPool` + `EmitDataset` between `GenerateInnerFlat` and `AssembleNestedInclude` (upward pool-node accumulation before the outer node is assembled), and deferring the pool dataset's output file.

**`../../lib/plan.rs` — `emit_nested_include_steps`:**

When a list field's `content.fields` contains a collect binding:
1. The pool dataset (`doctors`) is marked `skip_emit: true` (by the Stage 3 pre-scan — no additional change here).
2. Emit steps in order:
   - `GenerateInnerFlat` — unchanged; produces inner flat with `_slot_idx` + `_pool_idx` + content fields.
   - `CollectToPool` — `source_path` = the inner flat key, `group_by` = `"_pool_idx"`, targets the pool field. Same step type as Stage 4.
   - `EmitDataset[pool]` — emit the now-updated pool batch.
   - `AssembleNestedInclude` — unchanged; folds inner flat into the outer's list column using `_slot_idx`, then emits the outer.

**Bind target resolution:** `bind: doctors.on_call_list` — the ref part (`doctors`) resolves to the pool dataset via the link matched by `content.group`. Pool dataset path is already present in the `GenerateInnerFlat` step's `pool_slots_path`. Use `resolve_collect_bind` helper from Stage 3.

**Case 2 v1 restriction:** if the pool dataset is jointly segmented with another sibling, error at planning time: "nested-include collect is not supported when the pool dataset is jointly segmented; use a top-level junction dataset instead."

**No changes to `execute_inner_flat`** — the inner flat already carries `_pool_idx` from MULT-1.

**Verification:** `cargo test`. Add executor test fixture `wards_doctors/` asserting `doctors.on_call_list` is populated correctly via `_pool_idx` grouping and `wards.on_call_doctors` nested list is assembled correctly via `_slot_idx` grouping.

---

### Stage 6 — Reinforcement sampling ✓ DONE

**Context:** Activates `links[i].reinforcement`. Removes the MULT-1 validation error for this field. Implements the three sampling modes in `execute_inner_flat` and the junction link sampling path.

**`../../lib/executor.rs` — `execute_inner_flat`:**

- `reinforcement: None` or `reinforcement: 1.0` → existing uniform with-replacement (no change).
- `reinforcement: 0.0` → Fisher-Yates without-replacement draw of `M_n` items from the eligible pool slice. Requires `M_n ≤ pool_size`; planning-time error if not guaranteed.
- `reinforcement: r > 1.0` → weighted re-selection. Exact algorithm TBD at implementation time (alias method or cumulative-weight sampling).

Same three modes applied to the junction link pool sampling path in `execute_sibling_group`.

**`../../lib/validate.rs`:** Remove the MULT-2a validation error for `links[i].reinforcement` (currently fired in `validate_rich_content` for list links; junction links were already a validation error so reinforcement was never reached for them). Add planning-time error for `reinforcement: 0` when `expected_junction_rows > expected_pool_size`.

**Verification:** `cargo test`. Add test asserting `reinforcement: 0` produces no duplicate pool rows per outer row.

---

### Stage 7 — Scalar reducers ✓ DONE

**Context:** Extends `CollectToPool` / `execute_collect_to_pool` to support `sum`, `max`, `min`, and `take-first` in addition to `collect`.

**`../../lib/executor.rs` — `execute_collect_to_pool`:**

Match on `reducer`:
- `Collect`   → `array_agg(source_field)` (existing from Stage 4)
- `Sum`       → `sum(source_field)` — element type must match field type
- `Max`/`Min` → `max(source_field)` / `min(source_field)`
- `TakeFirst` → `first_value(source_field ORDER BY _row_idx)` (or take the first row per group from the ordered source batch)

All use DataFusion's programmatic aggregate API.

**`../../lib/validate.rs`:** For each scalar reducer, validate that the source field's type is compatible with the aggregate (e.g. `sum` requires numeric). `collect` element-type match is checked in Stage 2 — ensure it is still in place.

**Verification:** `cargo test`. Add fixtures and tests for sum/max/min reducing a numeric field from a junction batch into the pool.

---

### Stage 8 — `_slot_idx` hierarchy propagation ✓ DONE

**Context:** A grandchild dataset that includes a multiplied intermediate (cardinality > 1 child) must see a consistent `_slot_idx` join key across hierarchy levels so its refs and reducers can group correctly.

**`../../lib/plan.rs`:**

Detect grandchild-of-multiplied patterns: dataset D includes P, and P itself has `cardinality` set (so P's batch has `_slot_idx`). Wire `_slot_idx` from `computed[P_path]` as a hidden `PrefillSource` into D's batch.

**`../../lib/executor.rs`:** No new execution logic — `PrefillSource` wiring is already handled by `resolve_prefills` + `generate_prefilled_batch`. The `_slot_idx` column is already retained in `computed` entries (only stripped from emitted output).

**Verification:** `cargo test`. Add executor test verifying a grandchild of a multiplied intermediate sees the correct `_slot_idx` values and that cross-hierarchy collect bindings group correctly.

---

### Stage 9 — Tests and cleanup ✓ DONE

Complete test coverage for all MULT-2 features:

- **Case 1 end-to-end** (`directorships/`): `organisations.directors` populated from junction rows. Default used for organisations with zero directorships.
- **Case 2 end-to-end** (`wards_doctors/`): `doctors.on_call_list` populated from inner flat via `_pool_idx`. `wards.on_call_doctors` nested list assembled via `_slot_idx`. `hidden: true` on `allocated_to` excludes it from output (binding still fires).
- **`reinforcement: 0`**: no duplicate pool row within one outer row's list.
- **Scalar reducers**: sum / max / min reducing a numeric field into the pool.
- **`_slot_idx` hierarchy**: grandchild of a multiplied parent carries the correct slot key.
- **Validation errors**: one test per new validation rule from Stage 2.

Update `MULT-2.md` to mark each stage `✓ DONE` as completed.
