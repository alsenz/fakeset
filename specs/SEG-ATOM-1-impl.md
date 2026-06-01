# SEG-ATOM-1 — Implementation plan

Companion to [`SEG-ATOM-1.md`](SEG-ATOM-1.md). The spec covers the design and
function-level contracts; this doc covers PR sequencing, intermediate test
green-points, and risks discovered during planning.

## Status

Planned.

## PR sequencing

Three PRs. The atom-batch swap (PR 2) is one atomic switchover — splitting it
leaves the code in a confusing transitional state with two parent-assembly paths.

| PR | Subject | Scope |
|----|---------|-------|
| 1 | Lock the bug as a Rust integration-test failure | New fixture + `#[ignore]`-marked test |
| 2 | The atom-batch swap | Spec Steps 1–6, single atomic change |
| 3 | Cleanup | Spec Steps 7 + 8 — remove `_BUG_REF` xfails, update CLAUDE.md |

### PR 1 — Lock the bug

A new fixture + `#[ignore]`-marked test (no native `#[xfail]` in Rust; ignored
test runs only via `cargo test -- --ignored` until the fix lands).

- `tests/fixtures/execute/overlap_shared_ref/` — minimal contracts / premiums /
  claims (3 YAML files; ~50 contract rows; both members ref `contract.contract_id`
  with overlap ratios chosen so the `{premiums, claims}` joint segment is large).
- `tests/executor_tests.rs::test_overlap_shared_ref_integrity` — asserts every
  `premiums.contract_id` and `claims.contract_id` value is present in
  `contracts.contract_id`. Marked `#[ignore]`. Today: fails with orphans.
  After PR 2: passes; `#[ignore]` removed.

Rationale: the Python statistical tests catch this via `_BUG_REF` xfail markers,
but they run in a separate suite and the xfail decorator is easy to overlook.
A Rust-level test makes the regression failure load-bearing in `cargo test`.

### PR 2 — The atom-batch swap

All of spec Steps 1–6 in one go. Order of edits inside the PR, top-down so the
file compiles incrementally:

1. **New helpers** (above existing functions in `lib/executor.rs`):
   - `pad_or_generate_tail(col, target_n, field, fc) -> Result<ArrayRef>` —
     for short precomputed columns.
   - `build_segment_atom_schema(parent_schema, members, seg_constraints) ->
     (Vec<Field>, HashMap<String,String>, HashMap<String,Vec<usize>>)` — returns
     the atom schema, parent-column → atom-column map, and
     parent-column → providing-member-indices map. **Use `Vec<Field>` (or
     `IndexMap`) for deterministic column order.**
   - `generate_segment_atom_batch(parent_schema, members, n_rows,
     seg_constraints, opt_import_batch, computed, parent_computed) ->
     Result<(RecordBatch, HashMap<String,String>)>` — column source priority:
     import → precomputed → fresh. **Precomputed branch only fires when
     `m.cardinality.is_none()`** (cardinality members' precomputed batches are
     at expanded shape, not per-slot).
   - `project_parent_columns_from_atom(...)` — Arrow column selection +
     `generate_column` for non-provided active parent fields.
   - `project_member_columns(...)` — composes ref-from-atom +
     non-ref-from-`generate_member_nonref_fields` with optional cardinality
     expansion via Arrow `take`.
   - `generate_remainder_parent_batch(parent_schema, n_rows, seg_constraints,
     opt_import_batch) -> Result<RecordBatch>` — extracted from the existing
     `seg.members.is_empty()` branch (no logic change).

2. **Refactor in place**: rename `generate_member_batch` →
   `generate_member_nonref_fields` and pass it a filtered field subset (or have
   it skip ref fields internally). VAR-2 logic stays put.

3. **Rewrite the `seg.members.is_empty() / else` block** in
   `execute_lower_cover_group_core` (currently `lib/executor.rs:503-559`) per
   the Step 5 sketch in the spec.

4. **Delete in this order** (after the rewrite compiles):
   - `generate_segment_member_batches`
   - `generate_member_expanded_batch`
   - `grow_parent_from_children`
   - `resolve_inherited_source_columns`
   - `prepend_row_index` if no remaining callers.

5. **Remove `#[ignore]`** from the PR-1 test.

6. Run `cargo test` (Rust, ~194 tests) and `pytest` (Python statistical, ~40
   tests). Insurance tests `test_{premium,claim}_{contract,customer}_id_refs`
   should now pass; statistical distribution tests should hold (no semantic
   change to non-ref generation).

### PR 3 — Cleanup

Spec Steps 7 + 8:

- Remove `_BUG_REF` xfail markers from `tests/statistical/test_insurance.py`.
- Update `CLAUDE.md`:
  - Delete the **BUG-REF** entry from *Known limitations*.
  - Update the *Module map* `executor.rs` line — replace references to
    `grow_parent_from_children` with the new function names.
  - Update the "Core architectural framing" section (line ≈148) and the
    description further down (line ≈150) that still reference
    `grow_parent_from_children`.
  - Update the *Complexity analysis* section's "historically high
    complexity" list (line ≈59).
  - Mark `specs/SEG-ATOM-1.md` as **Complete** in the feature specs table and
    move it to `specs/done/`.

## Risks discovered during planning

1. **Rule 2 inheritance regression.** Today's `resolve_inherited_source_columns`
   Rule 2 inherits a parent field from a member's same-name non-ref field. The
   new design only deduplicates via *ref* fields, so a member's same-name non-ref
   field is no longer inherited by the parent. After `expand_include_fields`
   runs, most "same-name" fields become explicit refs (Rule 1), so this is
   probably a no-op — but it's a real behaviour change. **Mitigation**: grep
   `tests/fixtures/execute/` for cases where a child declares a same-name non-ref
   field with no corresponding ref; audit each one.

2. **Precomputed + cardinality.** Handled by skipping the precomputed branch for
   cardinality members; the atom column gets freshly generated. Their
   previously-emitted batch (from when they were a parent) keeps its own ref
   values that won't match the new atom. This is the same divergence as today —
   not a regression but worth noting in case it surfaces in statistical tests.

3. **Short precomputed padding.** When `pre.num_rows() < n_rows` (stochastic
   rounding in the prior plan), today's LEFT JOIN silently fills with skeleton
   fresh values. New design needs `pad_or_generate_tail` — straightforward but
   easy to forget. Existing `mult1_grandchild` fixture should keep passing as
   the regression check.

4. **Empty `seg_constraints` case.** A member can ref `parent.X` with no further
   constraint (`ref:` only). Then `lower_cover_field_constraints` inserts an
   empty `FieldConstraints` for X. `apply_constraints` with an empty FC is a
   no-op clone — works fine, but worth a unit test alongside the atom-schema
   helpers.

5. **Atom column ordering / batch schema stability.** The atom schema must be
   deterministic across runs. Use `Vec<Field>` or `IndexMap` rather than
   `HashMap` for the schema list itself; `parent_col_map` and
   `providing_members` can remain `HashMap` since they're keyed-lookup-only.

## Estimated diff size

| PR | Approx LoC |
|----|------------|
| 1 | +80 (fixture + test) |
| 2 | -250 / +300, net ≈ +50 |
| 3 | ~30 across two files |
