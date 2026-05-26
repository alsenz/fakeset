# REFRAME-1 Stage 7 — Fixture renames, vocabulary cleanup, and final consistency pass

Pure naming/cleanup stage — no behaviour changes. Eliminates all surviving old vocabulary
(`pool`, `rich_list`, `link_content`, `pool_scoped`, `pool_path`, `sample_pool_*`) from
comments, variable names, function names, and fixture paths. Adds module-level `//!` doc
comments to all `lib/*.rs` files. Adds a `debug_assert!` in `build_plan` to make the
staging-before-witness ordering invariant machine-checkable.

*Implemented across `lib/validate.rs`, `lib/plan.rs`, `lib/executor.rs`, `lib/segment.rs`,
`lib/graph.rs`, `lib/lib.rs`, `lib/models.rs`, `lib/rewrite.rs`, `lib/expressions.rs`,
`lib/expand_variants.rs`, `lib/constraints.rs`, `lib/schema.rs`, `lib/generator.rs`,
`tests/executor_tests.rs`, `tests/validate_tests.rs`, `tests/plan_tests.rs`,
`tests/rewrite_tests.rs`, `tests/dag_tests.rs`, `CLAUDE.md`. All tests pass.*

---

## 1. Fixture directory renames

The `link_content*` names were already a Stage 2 rename of the original `rich_list*`
names. Stage 7 moves to the canonical REFRAME-1 vocabulary.

**`tests/fixtures/execute/`**:
- `link_content/` → `list_link/`
- `bernoulli_link_content/` → `bernoulli_list_link/`
- `link_content_plain/` → `list_link_flat/`

**`tests/fixtures/validation/`**:
- `link_content_expression_in_content/` → `list_link_expression_in_content/`
- `link_content_include_scoped_missing_field/` → `list_link_include_scoped_missing_field/`
- `link_content_include_scoped_with_type/` → `list_link_include_scoped_with_type/`
- `link_content_outer_scoped_missing_field/` → `list_link_outer_scoped_missing_field/`
- `link_content_outer_scoped_no_type/` → `list_link_outer_scoped_no_type/`

All fixture paths updated in `executor_tests.rs`, `plan_tests.rs`, `rewrite_tests.rs`,
`dag_tests.rs`, `validate_tests.rs`.

---

## 2. Old vocabulary cleanup — exact locations

**`lib/validate.rs`**:
- Lines 78/82: `"pool row"` → `"linked-dataset row"` (junction link error message and comment)
- Lines 394–402: `pool_scoped` variable → `linked_scoped`; error message `"pool-scoped \`ref\`"` → `"linked-scoped \`ref\`"`
- Line 614: `"pool field type"` → `"linked field type"`
- Function `validate_link_content` → `validate_list_link_content`

**`lib/plan.rs`**:
- Doc comment on `check_cardinality_feasibility` (renamed in Stage 6): "eligible pool slots/rows" → "eligible linked-dataset slots/rows"
- Error messages lines 305/311/346: "eligible pool size/rows" → "eligible linked-dataset size/rows"
- `pool_path` variable in `scan_collect_targets` → `linked_path`
- Doc comment on `resolve_collect_bind_target`: "pool dataset path" → "linked dataset path"

**`lib/executor.rs`**:
- `sample_pool_without_replacement(pool_size, count)` → `sample_linked_without_replacement(linked_n, count)`
- `sample_pool_weighted(pool_size, count, reinforcement)` → `sample_linked_weighted(linked_n, count, reinforcement)`
- Section comment "Pool sampling helpers" → "Linked-dataset sampling helpers"
- All four call sites updated accordingly

**`lib/segment.rs`**:
- `LowerCoverMember` doc comment: `"pool_size index"` → `"n_eligible boundary"`

**`tests/executor_tests.rs`**:
- `test_reinforcement_zero_no_duplicate_pool_rows` → `test_reinforcement_zero_no_duplicate_linked_rows`
- `test_junction_collect_to_pool` → `test_junction_collect_to_linked`
- Section comments: "collect-to-pool" → "collect-to-linked"

**`tests/validate_tests.rs`**:
- Five test functions renamed `test_link_content_*` → `test_list_link_*`
- Section comment "Rich list content validation" → "List-link content validation"
- Error message assertion `msg.contains("pool-scoped")` → `msg.contains("linked-scoped")`

**`tests/fixtures/validation/list_link_include_scoped_with_type/main.yaml`**:
- YAML comment `"pool-scoped ref"` → `"linked-scoped ref"`

---

## 3. Outer-ref ordering assertion

Added at end of `build_plan` in `lib/plan.rs` (debug builds only):

```rust
#[cfg(debug_assertions)]
{
    let mut seen_staging: HashSet<&PathBuf> = HashSet::new();
    for step in &steps {
        match step {
            ExecutionStep::GenerateStagingNode { path, .. } => {
                seen_staging.insert(path);
            }
            ExecutionStep::GenerateStagingLowerCoverGroup { parent_path, .. } => {
                seen_staging.insert(parent_path);
            }
            ExecutionStep::GenerateWitness { staging_path, .. } => {
                debug_assert!(
                    seen_staging.contains(staging_path),
                    "GenerateWitness for {staging_path:?} appears before its staging step — \
                     topo-sort invariant violated"
                );
            }
            _ => {}
        }
    }
}
```

Added to `lib/graph.rs` doc comment on `build_dag`: documents that the staging → witness
ordering dependency is satisfied by step ordering in the linear plan rather than a DAG
edge, and flags it as a target for a future DAG-aware scheduler.

---

## 4. Module-level `//!` doc comments

Added to all 13 `lib/*.rs` files (`lib.rs`, `models.rs`, `graph.rs`, `validate.rs`,
`expand_variants.rs`, `expressions.rs`, `rewrite.rs`, `constraints.rs`, `segment.rs`,
`plan.rs`, `schema.rs`, `generator.rs`, `executor.rs`). Each names the module's role
in one or two sentences using REFRAME-1 vocabulary.

---

## 5. `CLAUDE.md` update

Feature specs table: `specs/REFRAME-1.md` marked complete ("all stages implemented and merged").
