# REFRAME-1 Stage 3 — Staging node as explicit execution step

Currently `skip_emit: bool` on `GenerateDataset` serves two distinct roles, and the step type
name gives no hint which role applies:

1. **Staging** (`has_list_link = true`): scalar batch stored in `computed`; no expression
   evaluation; no emit. Assembly deferred to `AssembleFromWitness`.
2. **Collect-target deferral** (`is_collect_target = true`, no list links): expressions
   evaluated; emit deferred to the `EmitDataset` step that follows `AccumulateToLinked`.

`GenerateLowerCoverGroup` has `skip_parent_emit: bool` for the same role-1 purpose.

Stage 3 separates these roles by introducing two new step variants and a shared executor
helper, so the step type is always self-documenting.

---

## `lib/plan.rs`

**New `ExecutionStep` variants**:

```rust
/// Staging node: generates scalar (non-list) fields only. No expression evaluation,
/// no emit. `AssembleFromWitness` adds list columns and emits.
GenerateStagingNode {
    path: PathBuf,
    dataset: Arc<SyntheticDataset>,
    rows: usize,
    prefills: Vec<InheritedField>,
},

/// Staging counterpart of `GenerateLowerCoverGroup`.
/// Parent has list-link fields; emit is deferred to `AssembleFromWitness`.
GenerateStagingLowerCoverGroup {
    parent_path: PathBuf,
    parent: Arc<SyntheticDataset>,
    segments: Vec<Segment>,
    members: Vec<LowerCoverMember>,
},
```

**Remove flags from existing variants**:
- `GenerateDataset`: remove `skip_emit: bool` field. The `defer_emit: bool` rename (for
  collect-target deferral) is the only remaining skip flag — rename the field to `defer_emit`
  to make the remaining purpose explicit.
- `GenerateLowerCoverGroup`: remove `skip_parent_emit: bool`. The step type now carries this
  information.

**`push_with_list_link_steps`** (was `push_with_nested_include`) — change signature to accept
two closures, one per case:

```rust
fn push_with_list_link_steps(
    steps: &mut Vec<ExecutionStep>,
    dataset: &SyntheticDataset,
    path: &Path,
    defer_emit: bool,             // collect-target deferral; only applies when !has_list_link
    all_datasets: &HashMap<PathBuf, SyntheticDataset>,
    make_staging: impl FnOnce() -> ExecutionStep,
    make_normal: impl FnOnce(bool) -> ExecutionStep,  // arg = defer_emit
) {
    if dataset.data.iter().any(|f| f.is_list_link()) {
        steps.push(make_staging());
        emit_witness_steps(dataset, path, all_datasets, steps);
    } else {
        steps.push(make_normal(defer_emit));
    }
}
```

**Call sites** (there are two, one for datasets and one for lower cover groups):

```rust
// Standalone dataset:
push_with_list_link_steps(
    &mut steps, dataset, path, is_collect_target, datasets,
    || ExecutionStep::GenerateStagingNode { path: p.clone(), dataset: d.clone(), rows, prefills: prefills.clone() },
    |defer| ExecutionStep::GenerateDataset { path: p, dataset: d, rows, prefills, defer_emit: defer },
);

// Lower cover group:
push_with_list_link_steps(
    &mut steps, dataset, path, /*defer_emit=*/false, datasets,
    || ExecutionStep::GenerateStagingLowerCoverGroup { parent_path: p.clone(), parent: d.clone(), segments: segs.clone(), members: members.clone() },
    |_| ExecutionStep::GenerateLowerCoverGroup { parent_path: p, parent: d, segments: segs, members },
);
```

(A lower cover group parent is never a standalone collect target; `defer_emit=false` here.)

Note: after Stage 3 implementation, `defer_emit` was also added to `GenerateLowerCoverGroup`
for the case where the parent is itself a collect target (linked dataset used as a lower cover
group parent). The `make_normal` closure was updated to `|defer|` accordingly.

---

## `lib/executor.rs`

**Shared helpers** — introduce two functions that both paths call:

```rust
/// Core logic for GenerateDataset (defer_emit=false or true) and GenerateStagingNode (is_staging=true).
async fn execute_dataset_core(
    is_staging: bool,
    defer_emit: bool,
    path: &Path,
    dataset: &SyntheticDataset,
    rows: usize,
    prefills: &[InheritedField],
    computed: &mut HashMap<PathBuf, RecordBatch>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let prefill_map = resolve_prefills(prefills, computed);
    let batch = generate_prefilled_batch(&dataset.data, rows, &prefill_map)?;
    if is_staging {
        // Scalar batch only. AssembleFromWitness adds list columns and emits.
        computed.insert(path.to_path_buf(), batch);
    } else {
        let batch = evaluate_expressions(batch, dataset).await?;
        let batch = inject_linked_idx(&batch, path, dataset, computed)?;
        let output = filter_hidden_columns(strip_linked_idx(batch.clone()), &dataset.data).await?;
        computed.insert(path.to_path_buf(), batch);
        if !defer_emit {
            emit_batch(output, &dataset.format, &dataset.output_file, shared)?;
        }
    }
    Ok(())
}
```

```rust
/// Core logic for GenerateLowerCoverGroup and GenerateStagingLowerCoverGroup.
async fn execute_lower_cover_group_core(
    is_staging: bool,
    defer_emit: bool,
    path: &Path,
    dataset: &SyntheticDataset,
    segments: &[Segment],
    members: &[LowerCoverMember],
    computed: &mut HashMap<PathBuf, RecordBatch>,
    parent_computed: &mut HashSet<PathBuf>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()>
```

(The function body is the existing `execute_sibling_group` body, renamed and parameterised by
`is_staging` and `defer_emit` instead of `skip_parent_emit`.)

**Match dispatch** — the four arms in `execute`:

```rust
ExecutionStep::GenerateStagingNode { path, dataset, rows, prefills } => {
    execute_dataset_core(true, false, path, dataset.as_ref(), *rows, prefills,
                         &mut computed, &mut shared).await?;
}
ExecutionStep::GenerateDataset { path, dataset, rows, prefills, defer_emit } => {
    execute_dataset_core(false, *defer_emit, path, dataset.as_ref(), *rows, prefills,
                         &mut computed, &mut shared).await?;
}
ExecutionStep::GenerateStagingLowerCoverGroup { parent_path, parent, segments, members } => {
    execute_lower_cover_group_core(true, false, parent_path, parent.as_ref(), segments, members,
                                   &mut computed, &mut parent_computed, &mut shared).await?;
}
ExecutionStep::GenerateLowerCoverGroup { parent_path, parent, segments, members, defer_emit } => {
    execute_lower_cover_group_core(false, *defer_emit, parent_path, parent.as_ref(), segments, members,
                                   &mut computed, &mut parent_computed, &mut shared).await?;
}
```

Delete `execute_sibling_group`; the body moves into `execute_lower_cover_group_core`.

---

## `src/main.rs`

Add `print_plan` arms for the two new variants:

```
"[{i}] staging node: {name} ({rows} rows)"
"[{i}] staging lower cover group: {name} (...)"
```

(Exact format matches the existing `GenerateDataset` and `GenerateLowerCoverGroup` arms
respectively, prefixed with "staging ".)

Remove `skip_emit` and `skip_parent_emit` from destructuring in existing arms (fields no
longer exist). `src/main.rs` uses `..` wildcards in all match arms, so no structural change
is needed there — only adding the two new arms.

---

## `tests/plan_tests.rs`

Three tests pattern-match on `skip_emit` / `skip_parent_emit` and must be updated:

| Test | Change needed |
|------|---------------|
| `list_link_dataset_decomposes_into_witness_and_assemble` | `GenerateDataset { skip_emit: false, .. }` → `GenerateDataset { .. }`; `GenerateLowerCoverGroup { skip_parent_emit: false, .. }` → `GenerateLowerCoverGroup { .. }`; `GenerateDataset { skip_emit: true, .. }` → `GenerateStagingNode { .. }` |
| `bernoulli_list_link_parent_has_skip_parent_emit` | `GenerateLowerCoverGroup { skip_parent_emit: true, .. }` → `GenerateStagingLowerCoverGroup { .. }`; rename function to `bernoulli_list_link_parent_produces_staging_lower_cover_group` |
| `list_link_collect_produces_correct_step_sequence` | `GenerateDataset { skip_emit: true, .. }` → `GenerateStagingNode { .. }`; `GenerateLowerCoverGroup { skip_parent_emit: true, .. }` → `GenerateStagingLowerCoverGroup { .. }`; update assertion messages accordingly |

---

## Verification

```bash
cargo check    # must pass cleanly
cargo test     # all tests pass; plan output now shows "staging node:" labels
```

Spot-check `--print-plan` output for a dataset with list links to confirm the plan printer
shows `staging node:` / `witness:` / `assemble from witness:` in sequence.

**Deliverable**: `cargo check` and `cargo test` both pass. Plan output explicitly labels
staging nodes and staging lower cover groups. `skip_emit` / `skip_parent_emit` flags are gone.
