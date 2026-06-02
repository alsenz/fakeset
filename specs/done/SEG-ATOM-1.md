# SEG-ATOM-1 — Correct segment atom generation per REFRAME

## Status

Planned.

## Context and motivation

This spec describes the root-cause fix for what was previously labelled **BUG-REF**
(documented in CLAUDE.md and marked with `_BUG_REF` xfail decorators in
`tests/statistical/test_insurance.py`).

The REFRAME specification (`specs/done/REFRAME-1.md`) defines the concept semi-lattice
and gives a precise execution model. Bernoulli factoring (Step 4 of planning) produces
segment nodes from every lower cover group. Step 5 then marks atoms:

> An atom is a least element strictly greater than ⊥. After Step 4, atoms are:
> — segment nodes covering exactly one combination of lower-cover constraints,
> **including fully-combined joint nodes (A & B & C & …)**

Phase 1 of execution states:

> **Segment atoms**: generate rows using field generators and local constraints.

This is unambiguous: the joint segment `{premiums, claims}` is a single atom, and
atoms generate rows as a unit. The implementation has never honoured this. Instead,
`execute_lower_cover_group_core` loops over members within a segment, calls
`generate_segment_member_batches` for each independently, then assembles the parent
via a DataFusion LEFT JOIN (`grow_parent_from_children`). This is a pre-REFRAME design
that was carried forward intact when REFRAME was written, rather than being replaced.

The explicit instruction during the REFRAME session was to rewrite from scratch without
being influenced by the old code. That instruction was not followed for this execution
path. The subsequent `_BUG_REF` xfail markers were added to park the known failure
rather than fix the root cause.

### What goes wrong

In the `{premiums, claims}` overlap segment each member's `contract_id` (and
`customer_id`) is generated independently:

```
generate_segment_member_batches(premiums) → premiums.contract_id = [a1, a2, a3, ...]
generate_segment_member_batches(claims)   → claims.contract_id   = [b1, b2, b3, ...]

grow_parent_from_children:
  resolve_inherited_source_columns: premiums wins (first-child-wins via or_insert_with)
  contracts.contract_id = [a1, a2, a3, ...]

emit premiums → contract_id = [a1, a2, a3, ...]   ✓
emit claims   → contract_id = [b1, b2, b3, ...]   ✗  (orphan refs)
```

The same breakage occurs in any segment — even a singleton — where a single member
has two fields that both ref the same parent field, since `generate_segment_member_batches`
generates each field independently.

## The correct design

### Segment atom batch

For any segment with one or more members, execution must:

1. **Build a unified shared-ref schema** by collecting every field across all members
   whose `ref:` points at a parent column. When two or more members ref the same
   parent column `X`, they are deduplicated into a single entry governed by the
   merged constraint already present in `seg.field_constraints["X"]`. Member-specific
   non-ref fields are **not** included in this schema (see *Member-specific columns*).

2. **Generate ONE unified atom batch** — one column per shared parent-ref entry,
   `n_rows` rows. Column source priority per entry (see *Column source priority*):
   import taint → precomputed member → fresh generate under the merged constraint.

3. **Fan out to each output**:
   - **Parent** (contracts): for each active parent field, take the column from the
     atom batch if any member provided it; otherwise generate fresh (or take from
     the import batch if tainted).
   - **Each member** (premiums, claims, …): take the shared ref columns from the
     atom batch (looked up by the parent column name each member's ref points to);
     generate every non-ref column separately via the member's own schema (preserving
     variant and cardinality behaviour).

All three datasets receive the **same** `contract_id` values for a given segment
row. Referential integrity is structurally guaranteed.

### Example: `{premiums, claims}` joint segment

Members and their fields (after `resolve_refs`):

| Member | Field | Ref |
|--------|-------|-----|
| premiums | `contract_id` | `contract.contract_id` |
| premiums | `customer_id` | `contract.customer_id` |
| premiums | `amount` | _(none)_ |
| claims | `contract_id` | `contract.contract_id` — **shared** |
| claims | `customer_id` | `contract.customer_id` — **shared** |
| claims | `claim_date` | _(none)_ |

Unified atom schema (shared ref columns only):

```
contract_id   ← seg.field_constraints["contract_id"] (merged from both members)
customer_id   ← seg.field_constraints["customer_id"] (merged from both members)
```

Projections from the one generated batch:

| Dataset | Columns taken from atom batch | Freshly generated |
|---------|-------------------------------|-------------------|
| contracts | `contract_id`, `customer_id` | all other contracts fields |
| premiums | `contract_id`, `customer_id` | `amount` and other premiums-specific fields (variant-aware) |
| claims | `contract_id`, `customer_id` | `claim_date` and other claims-specific fields (variant-aware) |

### Atom column naming

Shared ref fields are named after the **parent field name** in the unified schema
(e.g. `contract_id`), regardless of the local name any member uses. Each member maps
its local field name → parent field name via `ref:` to look up the correct atom
column during projection.

### Member-specific columns

Non-ref fields belong to a single member and are never placed in the atom batch.
`project_member_columns` generates them directly from the member's own schema —
preserving the existing variant filter (VAR-2) and cardinality expansion paths
without modification. This eliminates two otherwise-awkward cases:

- Two members declaring a non-ref field with the same local name (no namespacing
  required because the atom batch never sees them).
- Cardinality members whose non-ref columns are M_n rows per slot rather than one
  (the atom batch stays uniformly `n_rows` long).

The atom batch therefore contains **only** shared parent-ref columns. Singleton and
joint segments share the same shape — the joint case simply has more dedup
opportunities.

### Variant-aware member generation

When a member has `variants:` (VAR-2), variant sub-distribution applies only to the
member's non-ref fields. Ref fields are constrained at the lower-cover level and
already carry merged constraints in `seg.field_constraints`. Since non-ref columns
are now generated entirely inside `project_member_columns`, the existing variant
logic survives in place: `generate_member_batch` is refactored to generate the
**non-ref subset** of a member's schema (filter compatible variants against
`seg.field_constraints`, renormalise surviving ratios, distribute rows). It is then
called by `project_member_columns`.

This preserves VAR-2 behaviour exactly — Level 2 filtering, ratio renormalisation,
and per-slot variant draws under cardinality — without the atom batch having to
know anything about variants.

### Precomputed members

A lower-cover member may have already been generated by a prior plan step (the
grandparent case currently handled by the `precomputed: Some(pre)` branch in
`generate_segment_member_batches`). Its already-generated columns become the source
of truth for the atom batch:

- For each shared ref column X provided by a precomputed member, the atom column for
  X is **taken directly from that member's precomputed batch**. If the precomputed
  batch has fewer than `n_rows` rows (stochastic rounding in its own plan), the
  remaining rows are filled by fresh generation — the same tolerance the current
  LEFT JOIN provides.
- Variant logic is skipped for precomputed members (their values are already final).

If two precomputed members in the same segment provide the same shared ref column
with divergent values, the atom takes the first by member-order. This preserves
today's behaviour; resolving truly-conflicting precomputed values is out of scope
for this spec.

### Column source priority

When `generate_segment_atom_batch` materialises each shared ref column X, it applies
this priority:

1. **Import taint** — if the parent's field `X` has `imported_taint: true`, the
   column is taken from `opt_import_batch[X]`. Import-pinned values dominate.
2. **Precomputed member** — else, if any member providing X is precomputed (already
   in `computed` via `parent_computed`), the column is taken from that member's batch
   (positional copy; padded with fresh generation if shorter than `n_rows`).
3. **Fresh generate** — else, the column is generated synthetically under
   `seg.field_constraints[X]` (or the parent's field definition with no override).

This priority is checked on the parent's field metadata, not on members' ref fields,
since the import-taint flag lives on the parent's schema.

### Cardinality members

When a member has `include: { cardinality: N }`, the member output has M_n rows per
parent slot, not one. The atom batch still has `n_rows` rows (one per slot).
Expansion happens inside `project_member_columns`:

- For each slot `i` in `0..n_rows`: sample `m_n` from the cardinality distribution.
- **Ref columns**: take row `i` from the atom batch and replicate it `m_n` times via
  Arrow `take` (all M_n output rows share the same `contract_id`).
- **Non-ref columns**: generate `m_n` fresh values per slot via the variant-aware
  path above. Each claim row gets its own `claim_amount`, `claim_date`, etc.,
  distributed across surviving variants exactly as today.
- Prepend `_slot_idx = slot_offset + i` to each output row.

This preserves cardinality + VAR-2 behaviour for non-ref fields while structurally
fixing the ref-field side.

### Degenerate cases

| Segment type | Members | Behaviour |
|---|---|---|
| Remainder | `[]` | Generate parent batch fresh from parent schema + `seg.field_constraints`. No atom batch needed. |
| Singleton | `[M]` | Atom batch = M's ref columns only. No dedup needed; same algorithm. |
| Joint | `[M1, M2, …]` | Full atom batch with cross-member ref dedup. The fix. |

### Witness-source members

Witness-source members (`is_witness_source = true`) contribute field constraints to
`seg.field_constraints` but do not appear in the unified schema and produce no
standalone output. No change to their handling.

## Implementation plan

All changes are in `lib/executor.rs` unless noted. PR sequencing, intermediate
test green-points, and risks are in the companion doc
[`SEG-ATOM-1-impl.md`](SEG-ATOM-1-impl.md).

---

### Step 1 — Add `build_segment_atom_schema`

```rust
/// Build the unified shared-ref schema for a segment atom from all real
/// (non-witness-source) members. Only fields whose `ref:` points at a parent column
/// participate; member-specific non-ref fields are excluded entirely (generated later
/// by `project_member_columns`).
///
/// Returns:
/// - `atom_schema`: the deduplicated shared parent-ref field list, named after the
///   parent column each ref points at.
/// - `parent_col_map`: maps each parent field name → atom column name (currently the
///   same string; the map exists so callers don't depend on the naming convention).
/// - `providing_members`: maps each parent column name → the indices of all real
///   members that ref it. Used by `generate_segment_atom_batch` to apply the
///   precomputed-member rule (Column source priority §2).
fn build_segment_atom_schema(
    parent_schema: &Schema,
    members: &[&LowerCoverMember],
    seg_constraints: &HashMap<String, FieldConstraints>,
) -> (Schema, HashMap<String, String>, HashMap<String, Vec<usize>>)
```

Walk all members' fields. For each field with `ref: {member.reference}.X`:
- If `X` not yet in `parent_col_map`: add a column to `atom_schema` whose
  `Field` is **derived from the parent's** declaration of `X`, with
  `seg_constraints.get("X")` applied as the override (so type/imported_taint/etc.
  come from the parent, while the constraint comes from the segment). Record
  `parent_col_map[X] = "X"`.
- Append the member index to `providing_members[X]`.
- Else (member-specific non-ref field): ignore — handled in `project_member_columns`.

Use `lower_cover_field_constraints` logic from `segment.rs` to identify parent-ref
fields — same `ref_str.strip_prefix(&format!("{}.", member.reference))` pattern
already used in `resolve_inherited_source_columns`.

---

### Step 2 — Add `generate_segment_atom_batch`

```rust
/// Generate the unified atom batch — `n_rows` rows, one column per shared parent-ref
/// entry — applying the column source priority defined in the design section.
fn generate_segment_atom_batch(
    parent_schema: &Schema,
    members: &[&LowerCoverMember],
    n_rows: usize,
    seg_constraints: &HashMap<String, FieldConstraints>,
    opt_import_batch: Option<&RecordBatch>,
    computed: &HashMap<PathBuf, RecordBatch>,
    parent_computed: &HashSet<PathBuf>,
) -> Result<(RecordBatch, HashMap<String, String>)>
```

1. `(atom_schema, parent_col_map, providing_members) = build_segment_atom_schema(...)`.
2. For each entry `X` in `atom_schema`, materialise the column in priority order:
   - **Import taint**: if `parent_schema[X].imported_taint == true` and
     `opt_import_batch` contains `X`, take that column.
   - **Precomputed member**: else, scan `providing_members[X]` in order. For the first
     member whose path is in `parent_computed`, take `X` from `computed[member.path]`
     (positional copy; pad with fresh-generated values if the precomputed batch is
     shorter than `n_rows`). Note: the precomputed column is keyed by the
     member's local field name → the parent's `X`, mapped via the member's `ref:`.
   - **Fresh generate**: else, call `generate_column` on the atom_schema entry
     (which already has `seg_constraints[X]` applied).
3. Return `(atom_batch, parent_col_map)`.

Witness-source members are filtered out before this is called (their contribution is
already baked into `seg.field_constraints` by `plan_segments`).

---

### Step 3 — Add `project_parent_columns_from_atom`

```rust
/// Build the parent batch for this segment from the unified atom batch.
///
/// For each active parent field (no expression, not a list link):
/// - If `parent_col_map` contains the field name: take that column from `atom_batch`.
/// - Otherwise: generate fresh (using `seg_constraints` override if present, or
///   `opt_import_batch` for tainted fields).
///
/// Replaces the skeleton + DataFusion LEFT JOIN in `grow_parent_from_children` for
/// the multi-member case, and unifies the parent-assembly path for all segment types.
fn project_parent_columns_from_atom(
    atom_batch: &RecordBatch,
    parent_schema: &Schema,
    n_rows: usize,
    seg_constraints: &HashMap<String, FieldConstraints>,
    opt_import_batch: Option<&RecordBatch>,
    parent_col_map: &HashMap<String, String>,
) -> Result<RecordBatch>
```

No DataFusion, no JOIN. Pure Arrow column selection + `generate_column` for the
non-provided fields. This is simpler and faster than the current skeleton + LEFT JOIN.

---

### Step 4 — Add `project_member_columns`

```rust
/// Build this member's output batch by composing two sources:
/// (a) ref columns projected from the unified atom batch (looked up via
///     `parent_col_map[parent_field_name]` for each of the member's ref fields), and
/// (b) non-ref columns generated from the member's own schema via the variant-aware
///     path (`generate_member_nonref_fields` — the refactored `generate_member_batch`).
///
/// When `m.cardinality` is set: each slot expands to `m_n` rows. Ref columns are
/// replicated with Arrow `take`; non-ref columns are freshly generated per slot via
/// the variant-aware path, then concatenated. `_slot_idx = slot_offset + i` is
/// prepended.
///
/// When no cardinality: ref + non-ref columns are stitched side-by-side at one row
/// per slot. The result IS the member's output batch.
fn project_member_columns(
    atom_batch: &RecordBatch,
    m: &LowerCoverMember,
    member_idx: usize,
    slot_offset: usize,
    n_rows: usize,
    parent_col_map: &HashMap<String, String>,
    seg_constraints: &HashMap<String, FieldConstraints>,
    precomputed: Option<&RecordBatch>,
    acc: &mut SegmentBatchAccumulator,
) -> Result<()>
```

Notes:
- **Precomputed members**: when `precomputed` is `Some(pre)` and `m.cardinality` is
  `None`, take the entire member batch from `pre` directly (matching the current
  `generate_segment_member_batches` precomputed branch). With cardinality, regenerate
  as today — the precomputed batch is at the wrong shape.
- **Variant-aware non-ref**: `generate_member_nonref_fields(m, m_n, seg_constraints)`
  is the refactored `generate_member_batch`, restricted to the member's non-ref
  fields. With no variants, it falls through to a `generate_fresh_batch` over those
  fields. With variants, it preserves VAR-2 Level 2 filtering and renormalisation.
- **Ref column projection**: walk `m.dataset.data` for fields with `ref:
  {m.reference}.X`. For each, look up the atom column via `parent_col_map[X]`. With
  cardinality, replicate via Arrow `take(atom_col, repeat_indices)`. Without
  cardinality, the column is used directly.

---

### Step 5 — Rewrite the segment loop in `execute_lower_cover_group_core`

Replace the existing member loop (which calls `generate_segment_member_batches ×
N` then `grow_parent_from_children`) with:

```rust
let parent_seg = if seg.members.is_empty() {
    // Remainder segment — parent batch only, no members.
    generate_remainder_parent_batch(
        &dataset.data, n_rows, &seg.field_constraints, opt_import_batch.as_ref(),
    )?
} else {
    let real_members: Vec<&LowerCoverMember> = seg
        .members
        .iter()
        .filter(|mp| !witness_source_paths.contains(*mp))
        .map(|mp| members.iter().find(|m| &m.path == mp).unwrap())
        .collect();

    let (atom_batch, parent_col_map) = generate_segment_atom_batch(
        &dataset.data,
        &real_members,
        n_rows,
        &seg.field_constraints,
        opt_import_batch.as_ref(),
        computed,
        parent_computed,
    )?;

    // Project members from the atom batch (order doesn't matter — atom is already final).
    for (i, m) in real_members.iter().enumerate() {
        let pre = parent_computed
            .contains(&m.path)
            .then(|| computed.get(&m.path))
            .flatten();
        project_member_columns(
            &atom_batch, m, i, acc.slot_offset, n_rows,
            &parent_col_map, &seg.field_constraints, pre, &mut acc,
        )?;
    }

    // Assemble parent from the unified atom batch.
    project_parent_columns_from_atom(
        &atom_batch, &dataset.data, n_rows,
        &seg.field_constraints, opt_import_batch.as_ref(), &parent_col_map,
    )?
};

acc.push_parent_batch(parent_seg, is_staging, seg_has_witness_source);
acc.advance_slot_offset(n_rows);
```

The `generate_remainder_parent_batch` helper (rename/extract from the existing
`generate_fresh_batch` / `generate_batch_with_import` call for the `seg.members.is_empty()`
branch) makes the three cases symmetrical: remainder, singleton, joint.

---

### Step 6 — Remove or refactor pre-REFRAME functions

| Function | Action |
|---|---|
| `generate_segment_member_batches` | **Delete.** Its precomputed/cardinality fan-out is now inside `project_member_columns`. |
| `generate_member_expanded_batch` | **Delete.** Cardinality expansion logic moves into `project_member_columns`. |
| `generate_member_batch` | **Refactor → `generate_member_nonref_fields`**. Same variant-aware Level 2 logic (filter against `seg_constraints`, renormalise ratios, distribute rows), but restricted to the member's non-ref subset and called from `project_member_columns`. |
| `grow_parent_from_children` | **Delete.** Its only caller (the multi-child JOIN path) is replaced by `generate_segment_atom_batch` + `project_parent_columns_from_atom`. |
| `resolve_inherited_source_columns` | **Delete.** Its purpose (resolving which child provides which parent column) is subsumed by `build_segment_atom_schema`'s `parent_col_map` + `providing_members`. |

---

### Step 7 — Remove BUG-REF xfail markers

In `tests/statistical/test_insurance.py`:

- Remove `@_BUG_REF` from all four affected tests:
  - `test_premium_contract_id_refs`
  - `test_premium_customer_id_refs`
  - `test_claim_contract_id_refs`
  - `test_claim_customer_id_refs`
- Delete the `_BUG_REF` marker definition and the comment block explaining it
  (lines 18–32).
- Run `pytest` and confirm all four pass.

---

### Step 8 — Update CLAUDE.md

- Remove the **BUG-REF** entry from the *Known limitations* section.
- Update the *Module map* executor.rs row to reflect the new function names
  (`generate_segment_atom_batch`, `project_parent_columns_from_atom`,
  `project_member_columns`).
- Remove the **(formerly "prefill")** note from *inherited field* in the glossary
  if it is already absent; confirm BUG-REF mention is gone from the Known limitations.
- Add `SEG-ATOM-1` to the feature specs table as **Complete** once merged.

---

## Invariants

- **Referential integrity** (the fix): all datasets whose fields ref the same parent
  column receive the same generated value within a segment. No orphan refs.
- **Row counts**: each segment still contributes exactly `n_rows` to the parent and
  exactly `n_rows` (or `Σ m_n` for cardinality members) to each member output.
- **Constraint satisfaction**: `seg.field_constraints` continues to govern the merged
  constraints for parent-ref fields; per-member non-ref fields use member schemas.
- **Column source priority**: shared atom columns resolve in the order import taint →
  precomputed member → fresh generate. Import-taint and precomputed checks operate
  on parent-field metadata and the `parent_computed` set respectively; never on
  member-field metadata.
- **Variant preservation** (VAR-2): per-member variant filtering, ratio
  renormalisation, and Level 2 sub-distribution behave exactly as today, now
  invoked from `project_member_columns` over the member's non-ref subset.
- **Precomputed members**: when a member was generated by a prior plan step, its
  columns are used verbatim — never regenerated. If a precomputed member provides
  a shared ref column, that column flows into the atom batch directly (with
  fresh-generated padding only when the precomputed batch is short of `n_rows`).
- **Import taint**: tainted fields still come from the import batch at all
  projection sites; the import-taint check precedes the precomputed-member check.
- **Witness-source members**: unchanged — they contribute to `seg.field_constraints`
  only and are filtered from `real_members` before atom batch generation.
- **Cardinality**: M_n expansion preserved; ref columns replicated (same value per slot),
  non-ref columns freshly generated per replica via the variant-aware path.

## Non-goals

- Variant-level field merging across member variants (VAR-SPECIALIZE) — out of scope.
- Resolving conflicting precomputed values when two precomputed members in the same
  segment provide divergent values for a shared ref column — the new design preserves
  today's first-wins behaviour and does not introduce a new failure mode here.
- Any change to `segment.rs` (Bernoulli factoring, IPF, conflict pruning) — correct as-is.
- Any change to the staging / witness / assembly pipeline — atoms in this spec are
  segment atoms only; witness atoms remain untouched (REFRAME Stage 4 / Stage 5).
