# REFRAME-1 Stage 5 — Per-segment witness correctness

One `GenerateWitness` step per (staging segment, list-link field). Each witness covers a
contiguous slot range (`slot_start`/`slot_count`) and filters the linked batch to rows
matching that segment's field constraints. Staging batches concatenated in segment order
(no shuffle) to preserve slot indices. `AssembleFromWitness` unions all per-segment
witness batches before unnesting. Cumulative `Collect` reducer in `AccumulateToLinked`:
subsequent calls carry forward existing list items rather than replacing them.

*Implemented in `lib/plan.rs` (`emit_witness_steps`, `push_with_list_link_steps`,
`GenerateWitness`, `AssembleFromWitness`), `lib/executor.rs` (`execute_witness`,
`execute_lower_cover_group_core`, `execute_assemble_from_witness`,
`execute_accumulate_to_linked`). New fixture:
`tests/fixtures/execute/segmented_list_link/`. New test:
`test_segmented_list_link_assembles_correctly`. All tests pass.*

---

## Background

When the staging node has a lower cover group (from Bernoulli factoring), each segment
carries a `field_constraints: HashMap<String, FieldConstraints>` that describes that
segment's population. The per-segment witness must sample from a *filtered* slice of the
linked batch — only those linked rows whose field values satisfy the segment's constraints.

Field constraints in a segment come from `lower_cover_field_constraints` applied to each
lower cover member. If member B pins `kind: B`, the segment `{B}` has `{kind: {value: B}}`.
When this constraint is passed to `execute_witness`, rows of the linked dataset where
`kind ≠ B` are excluded from sampling — regardless of their position in the linked batch.

**Slot identification** — each per-segment witness covers a contiguous range of staging
slots `slot_start..slot_start+slot_count`. Making this well-defined requires that staging
batches are concatenated in *segment order* with no shuffle. Currently staging batches are
shuffled (via `combine_and_shuffle`). This shuffle is safe to remove for staging batches:
staging batches are never emitted directly; their row order does not affect output
statistical properties.

**`_linked_idx` semantics are preserved** — after constraint filtering the sampled row
indices are mapped back to positions in the full eligible linked batch (first
`n_eligible_slots` rows of `computed[linked_path]`), so AccumulateToLinked's GROUP BY
`_linked_idx` continues to work without change.

**Collect bindings across segments** — when a list-link field has collect bindings,
AccumulateToLinked is emitted once per segment. The executor must *merge* each segment's
contributions into the linked batch instead of replacing the previous result. For
`Reducer::Collect` this means concatenating the new list with the existing one. Stage 5
implements this for `Collect`; scalar reducers (Sum, Max, Min, TakeOne) with multi-segment
collect are handled in Stage 5.5.

---

## `lib/plan.rs`

**Imports** — add:
```rust
use crate::constraints::FieldConstraints;
use crate::segment::Segment;  // already imported via use crate::segment::{plan_segments, LowerCoverMember, Segment}
```

**`GenerateWitness` variant** — add three fields:
```rust
GenerateWitness {
    witness_key: PathBuf,
    staging_path: PathBuf,
    list_field_name: String,
    inner_fields: Vec<Field>,
    include: Include,
    cardinality: CountSpec,
    linked_path: PathBuf,
    // NEW:
    /// Index of the first staging slot this witness covers (0 for non-segmented nodes).
    slot_start: usize,
    /// Number of staging slots this witness covers (= total staging rows for non-segmented).
    slot_count: usize,
    /// Constraints from this segment applied as a filter to the linked batch before sampling.
    /// Empty map = no filtering (all eligible linked rows are candidates).
    segment_constraints: HashMap<String, FieldConstraints>,
},
```

**`AssembleFromWitness.witness_specs`** — change inner type from `PathBuf` to `Vec<PathBuf>`:
```rust
AssembleFromWitness {
    staging_path: PathBuf,
    dataset: Arc<SyntheticDataset>,
    /// `(list_field_name, witness_keys, project_col)` — one witness key per staging segment.
    /// Assembly unions all per-segment witnesses before unnesting and folding.
    witness_specs: Vec<(String, Vec<PathBuf>, Option<String>)>,
},
```

**`witness_key_seg` function** — new, alongside the existing `witness_key`:
```rust
fn witness_key_seg(staging_path: &Path, field_name: &str, seg_idx: usize) -> PathBuf {
    internal_path(staging_path, &format!("{field_name}___witness_{seg_idx}"))
}
```

The old `witness_key` function (without `seg_idx`) is no longer called; remove it or keep
it as dead code (the compiler will warn — remove it).

**`emit_witness_steps` signature** — add `segments: &[Segment]`:
```rust
fn emit_witness_steps(
    dataset: &SyntheticDataset,
    path: &Path,
    all_datasets: &HashMap<PathBuf, SyntheticDataset>,
    segments: &[Segment],   // NEW: one entry per Bernoulli segment; non-empty
    steps: &mut Vec<ExecutionStep>,
)
```

**`emit_witness_steps` body** — replace the existing loop with a two-level loop (fields
outer, segments inner):

```rust
fn emit_witness_steps(...) {
    let mut witness_specs: Vec<(String, Vec<PathBuf>, Option<String>)> = Vec::new();

    for field in &dataset.data {
        let Some(content) = &field.content else { continue };
        let Some(ref from_ref) = content.from else { continue };
        let Some(link) = dataset.links.iter().find(|l| l.reference == *from_ref) else { continue };
        let Some(linked_path) = resolve_include(path, &link.file) else { continue };
        let cardinality = link.cardinality.clone().unwrap_or(CountSpec::Fixed(1));

        let mut slot_offset: usize = 0;
        let mut seg_witness_keys: Vec<PathBuf> = Vec::new();
        let mut has_collect = false;
        let mut collect_linked_path = linked_path.clone();  // for EmitDataset later

        for (seg_idx, seg) in segments.iter().enumerate() {
            let wkey = witness_key_seg(path, &field.name, seg_idx);
            steps.push(ExecutionStep::GenerateWitness {
                witness_key: wkey.clone(),
                staging_path: path.to_path_buf(),
                list_field_name: field.name.clone(),
                inner_fields: content.item.fields.clone(),
                include: link.clone(),
                cardinality: cardinality.clone(),
                linked_path: linked_path.clone(),
                slot_start: slot_offset,
                slot_count: seg.rows,
                segment_constraints: seg.field_constraints.clone(),
            });
            seg_witness_keys.push(wkey.clone());

            // Collect bindings: AccumulateToLinked per segment (cumulative in executor).
            for cf in &content.item.fields {
                for binding in cf.collect_bindings() {
                    let Some(bind) = binding.bind.as_deref() else { continue };
                    let Some((_, linked_field)) = split_ref(bind) else { continue };
                    let lf_name = linked_field.to_string();
                    let def = linked_field_default(&linked_path, &lf_name, all_datasets);
                    steps.push(ExecutionStep::AccumulateToLinked {
                        source_path:  wkey.clone(),
                        source_field: cf.name.clone(),
                        linked_path:  linked_path.clone(),
                        linked_field: lf_name,
                        group_by:     "_linked_idx".to_string(),
                        reducer:      binding.reducer.clone().unwrap_or(Reducer::Collect),
                        default_val:  def,
                    });
                    has_collect = true;
                    collect_linked_path = linked_path.clone();
                }
            }

            slot_offset += seg.rows;
        }

        // EmitDataset once after all per-segment AccumulateToLinked steps for this field.
        if has_collect {
            if let Some(linked_ds) = all_datasets.get(&collect_linked_path) {
                steps.push(ExecutionStep::EmitDataset {
                    path:    collect_linked_path.clone(),
                    dataset: Arc::new(linked_ds.clone()),
                });
            }
        }

        let project_col = content.project.as_ref()
            .and_then(|p| split_ref(p))
            .map(|(_, f)| f.to_string());
        witness_specs.push((field.name.clone(), seg_witness_keys, project_col));
    }

    if !witness_specs.is_empty() {
        steps.push(ExecutionStep::AssembleFromWitness {
            staging_path: path.to_path_buf(),
            dataset: Arc::new(dataset.clone()),
            witness_specs,
        });
    }
}
```

Note: this is a straight replacement of the previous body. The only non-obvious
change is that the `has_collect` / `EmitDataset` logic now tracks across segments and
emits the `EmitDataset` once (after all segments' `AccumulateToLinked` steps for a field).

**`push_with_list_link_steps` signature** — add `segments: &[Segment]`:
```rust
fn push_with_list_link_steps(
    steps: &mut Vec<ExecutionStep>,
    dataset: &SyntheticDataset,
    path: &Path,
    defer_emit: bool,
    all_datasets: &HashMap<PathBuf, SyntheticDataset>,
    segments: &[Segment],          // NEW
    make_staging: impl FnOnce() -> ExecutionStep,
    make_normal: impl FnOnce(bool) -> ExecutionStep,
) {
    if dataset.data.iter().any(|f| f.is_list_link()) {
        steps.push(make_staging());
        emit_witness_steps(dataset, path, all_datasets, segments, steps);
    } else {
        steps.push(make_normal(defer_emit));
    }
}
```

**Call sites of `push_with_list_link_steps`** — there are four call sites in `build_plan`.
Each must now provide a `segments` slice:

*1 & 2 — lower cover group (standalone + variant):* these already have `segments` in scope
from the preceding `plan_segments(...)` call. Pass `&segments`.

*3 & 4 — standalone staging node (no lower cover, standalone + variant):* these have no
segments. Construct a synthetic single-segment covering all staging rows with no
constraints:
```rust
let single_seg = vec![Segment {
    members: vec![],
    rows,
    field_constraints: HashMap::new(),
}];
push_with_list_link_steps(..., &single_seg, ...);
```
(For variant expansions the row count is `variant_rows`.)

---

## `lib/executor.rs`

**New helper — `filter_batch_by_constraints`**

```rust
/// Filter `batch` to rows where every constrained field satisfies the given `FieldConstraints`.
/// Returns `(filtered_batch, surviving_row_indices)` where `surviving_row_indices[i]` is the
/// row in `batch` that corresponds to row i of `filtered_batch`.
/// Fields named in `constraints` but absent from `batch` are silently ignored.
fn filter_batch_by_constraints(
    batch: &RecordBatch,
    constraints: &HashMap<String, FieldConstraints>,
) -> Result<(RecordBatch, Vec<u32>)> {
    if constraints.is_empty() {
        let indices: Vec<u32> = (0..batch.num_rows() as u32).collect();
        return Ok((batch.clone(), indices));
    }
    let mut keep = vec![true; batch.num_rows()];
    for (field_name, fc) in constraints {
        let Ok(col_idx) = batch.schema().index_of(field_name) else { continue };
        let col = batch.column(col_idx);
        for row in 0..batch.num_rows() {
            if !row_satisfies_field_constraints(col, row, fc) {
                keep[row] = false;
            }
        }
    }
    let surviving: Vec<u32> = (0..batch.num_rows() as u32).filter(|&i| keep[i as usize]).collect();
    if surviving.len() == batch.num_rows() {
        return Ok((batch.clone(), surviving));
    }
    let idx_arr = UInt32Array::from(surviving.clone());
    let filtered_cols: Vec<ArrayRef> = batch.columns().iter()
        .map(|c| take(c.as_ref(), &idx_arr, None).map_err(anyhow::Error::from))
        .collect::<Result<_>>()?;
    let filtered = RecordBatch::try_new(batch.schema(), filtered_cols)?;
    Ok((filtered, surviving))
}

/// Return true if `col[row]` satisfies the `FieldConstraints` fc.
fn row_satisfies_field_constraints(col: &ArrayRef, row: usize, fc: &FieldConstraints) -> bool {
    use arrow::array::{StringArray, Float64Array, BooleanArray};
    // value constraint (equality check)
    if let Some(ref val) = fc.value {
        match (col.data_type(), val) {
            (DataType::Utf8, YamlValue::String(s)) => {
                if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                    if !arr.is_null(row) && arr.value(row) != s.as_str() { return false; }
                }
            }
            (DataType::Float64, YamlValue::Number(n)) => {
                if let Some(arr) = col.as_any().downcast_ref::<Float64Array>() {
                    let v = n.as_f64().unwrap_or(0.0);
                    if !arr.is_null(row) && (arr.value(row) - v).abs() > 1e-9 { return false; }
                }
            }
            (DataType::Boolean, YamlValue::Bool(b)) => {
                if let Some(arr) = col.as_any().downcast_ref::<BooleanArray>() {
                    if !arr.is_null(row) && arr.value(row) != *b { return false; }
                }
            }
            _ => {}  // unsupported type combination: no filtering
        }
    }
    // min/max constraints (numeric only)
    if fc.min.is_some() || fc.max.is_some() {
        if let Some(arr) = col.as_any().downcast_ref::<Float64Array>() {
            let v = arr.value(row);
            if let Some(min) = fc.min { if v < min { return false; } }
            if let Some(max) = fc.max { if v > max { return false; } }
        }
    }
    true
}
```

Add `YamlValue` to imports if not already present (it is already in scope via
`use serde_yaml::Value as YamlValue;`).

**`execute_lower_cover_group_core` — staging batch assembly (no shuffle)**

In the loop that appends to `witness_source_parent_batches` / `non_witness_source_parent_batches`,
replace the two-bucket split with a single ordered vec:

```rust
// BEFORE (current):
let mut witness_source_parent_batches: Vec<RecordBatch> = Vec::new();
let mut non_witness_source_parent_batches: Vec<RecordBatch> = Vec::new();
// ... in loop:
if seg_has_witness_source {
    witness_source_parent_batches.push(parent_seg);
} else {
    non_witness_source_parent_batches.push(parent_seg);
}
// ... after loop:
let parent_shuffled = if has_witness_sources && !witness_source_parent_batches.is_empty() {
    combine_witness_source_first(ws, non_ws, schema, name).await?
} else {
    let mut all = witness_source_parent_batches;
    all.extend(non_witness_source_parent_batches);
    combine_and_shuffle(all, schema, name).await?
};
```

```rust
// AFTER (Stage 5 for is_staging=true):
let mut ordered_parent_batches: Vec<RecordBatch> = Vec::new();  // staging path: all in segment order
// KEEP the old two-bucket split for non-staging (is_staging=false):
let mut witness_source_parent_batches: Vec<RecordBatch> = Vec::new();
let mut non_witness_source_parent_batches: Vec<RecordBatch> = Vec::new();

// ... in loop (replace the bucket push):
if is_staging {
    ordered_parent_batches.push(parent_seg);
} else if seg_has_witness_source {
    witness_source_parent_batches.push(parent_seg);
} else {
    non_witness_source_parent_batches.push(parent_seg);
}

// ... after loop:
let parent_assembled: RecordBatch = if is_staging {
    // Staging batch: segments in declaration order, no shuffle.
    // Slot ranges in GenerateWitness.slot_start/slot_count are computed from this order.
    let arrow_schema = Arc::new(schema_to_arrow(&dataset.data));
    concat_batches(&arrow_schema, &ordered_parent_batches)?
} else if has_witness_sources && !witness_source_parent_batches.is_empty() {
    combine_witness_source_first(
        witness_source_parent_batches, non_witness_source_parent_batches,
        &dataset.data, &dataset.name,
    ).await?
} else {
    let mut all = witness_source_parent_batches;
    all.extend(non_witness_source_parent_batches);
    combine_and_shuffle(all, &dataset.data, &dataset.name).await?
};
```

Then replace `parent_shuffled` with `parent_assembled` in the `if is_staging` block below.

If the ordered batches are empty (all segments zero rows), produce an empty batch:
```rust
if ordered_parent_batches.is_empty() {
    let arrow_schema = Arc::new(schema_to_arrow(&dataset.data));
    RecordBatch::new_empty(arrow_schema)
} else {
    let arrow_schema = ordered_parent_batches[0].schema();
    concat_batches(&arrow_schema, &ordered_parent_batches)?
}
```

**`execute_witness` — signature and body changes**

New parameters: `slot_start: usize, slot_count: usize, segment_constraints: &HashMap<String, FieldConstraints>`

Replace Phase 1 as follows:

```rust
// Phase 1: determine eligible linked rows (after ratio + constraint filtering)
let n_eligible_pre_filter = match include.ratio {
    Some(r) => ((r * linked_batch.num_rows() as f64).round() as usize)
        .min(linked_batch.num_rows()).max(1),
    None => linked_batch.num_rows(),
};
// Slice to eligible rows (eligible rows are placed first by combine_witness_source_first).
let eligible_linked = linked_batch.slice(0, n_eligible_pre_filter);
// Further filter by segment constraints.
let (filtered_eligible, surviving_idxs) =
    filter_batch_by_constraints(&eligible_linked, segment_constraints)?;
let n_eligible = filtered_eligible.num_rows().max(1);

// Sample for staging slots slot_start..slot_start+slot_count only.
let n = slot_count;
let counts: Vec<usize> = (0..n).map(|_| sample_count(cardinality)).collect();
let total: usize = counts.iter().sum();

// staging_idxs: the actual slot indices in the full staging batch.
let staging_idxs: Vec<u32> = counts.iter().enumerate()
    .flat_map(|(i, &c)| std::iter::repeat((slot_start + i) as u32).take(c))
    .collect();

// slot_assignments: indices into filtered_eligible (0..n_eligible).
let slot_assignments_filtered: UInt32Array = {
    let r = include.reinforcement;
    if r == Some(0.0) {
        counts.iter().flat_map(|&m_n| sample_pool_without_replacement(n_eligible, m_n))
            .collect::<Vec<u32>>().into()
    } else if let Some(reinf) = r.filter(|&v| v > 1.0) {
        counts.iter().flat_map(|&m_n| sample_pool_weighted(n_eligible, m_n, reinf))
            .collect::<Vec<u32>>().into()
    } else {
        (0..total).map(|_| (0u64..n_eligible as u64).fake::<u64>() as u32)
            .collect::<Vec<u32>>().into()
    }
};

// Map filtered-eligible indices back to eligible-linked indices (= linked_batch indices).
let slot_assignments: UInt32Array = slot_assignments_filtered.iter()
    .map(|opt| opt.map(|v| surviving_idxs[v as usize]))
    .collect::<Vec<_>>()
    .into();
```

Phase 2 (dedup) and Phase 4 (assembly) are unchanged. Phase 3 changes: replace
`linked_batch.column(idx)` with `eligible_linked.column(idx)` for linked-scoped refs —
because `unique_linked_idxs` are now indices into `eligible_linked` (not into the full
`linked_batch`):

```rust
// Phase 3 (linked-scoped refs):
take(eligible_linked.column(idx).as_ref(), &unique_linked_arr, None)?
```

**`execute_witness` match arm** — add destructuring for new fields:
```rust
ExecutionStep::GenerateWitness {
    witness_key, staging_path, list_field_name, inner_fields,
    include, cardinality, linked_path,
    slot_start, slot_count, segment_constraints,  // NEW
} => {
    execute_witness(
        witness_key, staging_path, list_field_name, inner_fields,
        include, cardinality, linked_path,
        *slot_start, *slot_count, segment_constraints,
        &mut computed,
    )?;
}
```

**`execute_assemble_from_witness` — union per-segment witnesses**

Change signature: `witness_specs: &[(String, Vec<PathBuf>, Option<String>)]`

At the top of the per-field loop, union all per-segment witnesses before proceeding:

```rust
for (field_name, witness_keys, project_col) in witness_specs {
    // Union all per-segment witness batches into one combined witness.
    let combined_schema = {
        let first_key = witness_keys.first().ok_or_else(|| anyhow!("no witnesses for '{field_name}'"))?;
        computed.get(first_key).ok_or_else(|| anyhow!("witness '{field_name}' not computed"))?.schema()
    };
    let witness_batches: Vec<RecordBatch> = witness_keys.iter()
        .map(|wk| computed.get(wk)
            .ok_or_else(|| anyhow!("witness '{}' for '{field_name}' not computed", wk.display()))
            .cloned())
        .collect::<Result<_>>()?;
    let witness = concat_batches(&combined_schema, &witness_batches)?;

    // Proceed with existing unnest_staging_refs / outer-scoped field resolution / fold logic
    // using `witness` (the combined batch) — no further changes.
    ...
}
```

**`execute_accumulate_to_linked` — cumulative Collect**

In the `Reducer::Collect` branch, change the "unmapped rows get empty list" logic to
"unmapped rows keep their existing value":

```rust
Reducer::Collect => {
    let existing_list = existing_linked_col.as_any().downcast_ref::<ListArray>()
        .ok_or_else(|| anyhow!("AccumulateToLinked: existing field is not a ListArray for Collect"))?;
    // ...
    for linked_row in 0..linked_n {
        if let Some(&agg_row) = idx_map.get(&(linked_row as u32)) {
            // New contributions from this segment.
            let new_items = agg_list.value(agg_row);
            // Concat with existing items (which may be the result of a previous segment's call).
            let existing_items = existing_list.value(linked_row);
            let total_len = existing_items.len() + new_items.len();
            offsets.push(offsets.last().unwrap() + total_len as i32);
            if existing_items.len() > 0 { child_slices.push(existing_items); }
            if new_items.len() > 0 { child_slices.push(new_items); }
        } else {
            // No new contributions: carry forward existing list unchanged.
            let existing_items = existing_list.value(linked_row);
            offsets.push(offsets.last().unwrap() + existing_items.len() as i32);
            if existing_items.len() > 0 { child_slices.push(existing_items); }
        }
    }
    // Build child_array and ListArray from child_slices as before.
}
```

This is backward-compatible: on the first call, `existing_linked_col` holds the initial
value from the staged computation. For collect bindings, this initial value is `[]`
(from `default: []`), so concat(`[]`, new_items) = new_items. On subsequent calls
(second segment), concat(previous_list, new_items) extends the list correctly.

---

## `src/main.rs`

**`print_plan` — `GenerateWitness` arm**: add slot range and constraints info:
```
"[{i}] witness: {name} slots {slot_start}..{slot_start+slot_count} ({n_constraints} constraints)"
```

**`print_plan` — `AssembleFromWitness` arm**: show multiple witness keys per field:
```
"[{i}] assemble from witness: {name}"
"  {field_name}: [{n} witnesses]"
```
(Exact format at implementer's discretion; must compile cleanly.)

**Destructuring** — update match arms for `GenerateWitness` (add new fields) and
`AssembleFromWitness` (change `witness_specs` element type). Both arms use `..` wildcards,
so only new field names need destructuring if used in the printed output.

---

## Tests

**New fixture: `tests/fixtures/execute/segmented_list_link/`**

`linked.yaml`:
```yaml
name: linked
format: jsonl
output_file: linked
rows: 10
data:
  - name: label
    type: string
    generator: word
  - name: kind
    type: string
    generator: word
```

`source.yaml`:
```yaml
name: source
format: jsonl
output_file: source
rows: 10
links:
  - file: linked.yaml
    ref: linked_item
    cardinality: 1
data:
  - name: items
    type: list
    content:
      from: linked_item
      fields:
        - name: item_label
          ref: linked_item.label
```

`child_a.yaml`:
```yaml
name: child_a
format: jsonl
output_file: child_a
include:
  file: source.yaml
  ref: source
  ratio: 0.4
data:
  - name: tag
    type: string
    value: A
```

`child_b.yaml`:
```yaml
name: child_b
format: jsonl
output_file: child_b
include:
  file: source.yaml
  ref: source
  ratio: 0.6
data:
  - name: tag
    type: string
    value: B
```

Bernoulli factoring of `source` with lower cover `{child_a (0.4), child_b (0.6)}` produces
up to four segments (child_a-only, child_b-only, both, neither). The non-zero segments each
get their own `GenerateWitness` for the `items` field.

**New test `test_segmented_list_link_assembles_correctly`**:

```rust
#[tokio::test]
async fn test_segmented_list_link_assembles_correctly() {
    let out = run("tests/fixtures/execute/segmented_list_link").await;

    let source = jsonl_rows(&out, "source");
    assert_eq!(source.len(), 10, "source should have 10 rows");

    for row in &source {
        let items = row["items"].as_array().expect("items should be an array");
        assert_eq!(items.len(), 1, "each source row should have exactly 1 item");
        let label = items[0]["item_label"].as_str().expect("item_label should be a string");
        assert!(!label.is_empty(), "item_label should be non-empty");
    }

    // child_a and child_b outputs exist with combined row count = 10.
    let child_a = jsonl_rows(&out, "child_a");
    let child_b = jsonl_rows(&out, "child_b");
    assert_eq!(child_a.len() + child_b.len(), 10, "children should partition all 10 source rows");
}
```

**Plan tests** — `tests/plan_tests.rs` has `list_link_dataset_decomposes_into_witness_and_assemble`
which counts `GenerateWitness` steps. With Bernoulli factoring, the count depends on the
number of non-zero segments (stochastic). Update the assertion to check that the count is
*at least 1* per list-link field:

```rust
let witness_count = steps.iter().filter(|s| matches!(s, ExecutionStep::GenerateWitness { .. })).count();
assert!(witness_count >= 1, "expected at least one GenerateWitness step");
```

(Or filter to the specific list-link field and assert `>= 1`.)

Also update `AssembleFromWitness` assertion: `witness_specs` now contains `Vec<PathBuf>` per
field instead of `PathBuf`. Change assertions accordingly.

---

## Verification

```bash
cargo check   # must pass cleanly
cargo test    # all tests pass; new test passes
```

Spot-check `--print-plan` for a dataset with list links and a lower cover group to confirm
multiple witness steps appear (one per non-zero segment) and AssembleFromWitness shows the
union of witness keys.
