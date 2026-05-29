# VAR-2 — Two-level variant factoring (BUG-VAR fix)

## Status

Complete — implemented and merged.

## Background

**BUG-VAR** (documented in the testing reference and CLAUDE.md Known Limitations):
when a dataset with `type: variant` fields is a lower cover member of another
dataset, the variant-specific values are never applied.

Root cause: `expand_field_variants` rewrites each `type: variant` field into:
1. A stub entry in `dataset.data` — carries the inferred concrete type but has no
   `value`, no `generator`, and no `range`.
2. One `VariantSchema` entry in `dataset.variants` for each choice — each carries the
   concrete field delta (value/generator/range) in `variants[i].data`.

When `execute_lower_cover_group_core` generates rows for a lower cover member `M`,
it calls `generate_fresh_batch(&m.dataset.data, seg.rows, &seg.field_constraints)`.
This uses the stub schema.  The stub's variant fields have no value/generator, so
`generate_column_raw` falls through to random string generation — producing garbage
instead of the declared variant values.

The fix is two-level factoring:

- **Level 1 (outer)**: `plan_segments` produces segments over the *base* member set
  (unchanged — same as today).
- **Level 2 (inner)**: within each segment, members that have `variants:` are
  sub-distributed across their concrete variant schemas before generation.

## Dependency on SEG-1

VAR-2 itself uses direct proportional distribution at Level 2 — no IPF is needed
because within a single member's variants, only feasibility pruning is required
(surviving variants' ratios are simply renormalised, not fitted against external
marginals).

However, when VAR-SPECIALIZE is implemented, a variant's specialised constraints may
conflict with sibling constraints arriving from Level 1 joint segments.  For example:
variant V0 specialises `contract_type = "premium"` but a sibling lower cover member
already constrains `contract_type = "basic"` in a particular joint segment — V0 is
incompatible with that segment.  At that point, Level 2 will need its own
branch-and-bound + IPF pass over surviving variants, redistributing the pruned weight
correctly across the feasible set — the same machinery as `plan_segments`.  SEG-1
must therefore precede VAR-2 so that machinery is correct and cap-free before Level 2
grows to depend on it.

Additionally: schemas will grow beyond the old 16-member cap regardless of variant
factoring, so removing the dense 2^N enumeration in SEG-1 first is the right
sequencing.

## Algorithm

### Level 2 inner variant sub-distribution

For a member `M` with `!m.dataset.variants.is_empty()`, replace the single
`generate_fresh_batch` call with:

```
1. Build concrete variant schemas:
   For each VariantSchema Vᵢ in m.dataset.variants:
     variant_schema[i] = merge_variant_fields(&m.dataset.data, &Vᵢ.data)
   This replaces stub fields with the variant's concrete value/generator/range.
   The concrete value is baked INTO the field's `value` property — it is not
   passed as an override.  `generate_fresh_batch` picks it up automatically.

2. Build variant_constraints: Vec<HashMap<String, FieldConstraints>>
   For each variant_schema[i]: construct a temporary LowerCoverMember with
   dataset.data = variant_schema[i] and reference = m.reference, then call
   lower_cover_field_constraints on it.
   These are the parent-field constraints this variant's ref fields would impose.
   (For BUG-VAR as-is, variant fields are not refs, so these maps are empty.
   They are populated when VAR-SPECIALIZE adds ref-backed variant constraints.)

3. Filter compatible variants:
   For each Vᵢ: discard if constraints_conflict(
       &variant_constraints[i], &seg.field_constraints)
   This is a no-op for the current BUG-VAR case; it will prune correctly once
   VAR-SPECIALIZE propagates specialisation constraints into variant ref fields.

4. If no variants survive → fall back to generate_fresh_batch(&m.dataset.data, ...)
   (all variants are incompatible with this segment; generate unconstrained)

5. Collect surviving variants' ratios (preserving None for free-split entries).
   Call resolve_distributions on just the surviving subset to get normalised ratios.

6. Call distribute_rows(seg.rows, &surviving_ratios) → per-variant row counts rᵢ.
   Skip any variant where rᵢ == 0 (avoid zero-row generate calls and empty batches
   that can confuse concat_batches schema inference).

7. For each surviving variant Vᵢ with rᵢ > 0:
   merged_constraints_Vᵢ = try_merge_incremental(
       seg.field_constraints.clone(), &variant_constraints[i])
   (returns None only if conflict — impossible here since we filtered in step 3)
   generate_fresh_batch(&variant_schema[i], rᵢ, &merged_constraints_Vᵢ.unwrap())

8. concat_batches(sub-batches) → canonical batch for M in this segment.
   All sub-batches must have identical Arrow schemas (same field names, types, order).
   This holds as long as variants only override values/generators on existing fields —
   the only supported case.  A variant that adds a new field would break concat; that
   is not currently validated, but the assumption holds for all declared use cases.
```

The same substitution applies in both paths inside the segment loop:
- The `!parent_computed.contains(&m.path)` path (lines ~306–320 in executor.rs) —
  generates canonical + optional expanded batch.
- The `parent_computed.contains(&m.path) && m.cardinality.is_some()` path
  (lines ~291–298) — also generates fresh canonical + expanded batches and must
  respect variant sub-distribution.

For the **expanded batch** (cardinality > 1 per slot), the same Level 2 logic applies:
loop over compatible variants, generate per-variant expanded sub-batches proportionally,
then concatenate.

### Forward compatibility with VAR-SPECIALIZE

VAR-SPECIALIZE (documented in CLAUDE.md Future Work) will allow a child dataset to
restrict a parent's variant field to a subset of variant choices.  When that lands,
the constraint will arrive via a ref field whose `FieldConstraints` pins the variant
field's value to one of the child's allowed values.

Steps 2–3 are already architecturally correct for this case: `constraints_conflict`
in step 3 will naturally discard incompatible variants when their ref field constraint
conflicts with the specialisation constraint propagated from the child.  The only
addition needed at that point is upgrading step 5–6 from direct proportional
distribution to a branch-and-bound + IPF pass (see Dependency on SEG-1 above).

## Extraction: `generate_member_batch`

To avoid duplicating the Level 2 logic for canonical vs expanded batches, extract a
helper:

```rust
fn generate_member_batch(
    m: &LowerCoverMember,
    rows: usize,
    seg_constraints: &HashMap<String, FieldConstraints>,
) -> Result<RecordBatch>
```

Returns a batch of `rows` rows for member `M`, applying Level 2 variant
sub-distribution when `!m.dataset.variants.is_empty()`.  Called in place of
`generate_fresh_batch(&m.dataset.data, rows, seg_constraints)` at both call sites
in `execute_lower_cover_group_core`.

The expanded-batch analogue is `generate_member_expanded_batch`, replacing the
`generate_expanded_batch` call.

## Visibility fixes required before implementation

Two helpers used by `generate_member_batch` are currently private to `lib/plan.rs`
and must be made visible to `lib/executor.rs` before implementation:

| Helper | Current | Fix |
|--------|---------|-----|
| `distribute_rows` | `fn` (private, `plan.rs`) | `pub(crate)` |
| `merge_variant_fields` | `fn` (private, `plan.rs`) | `pub(crate)` — or inline the 5-line body in `executor.rs` directly (it wraps `expand_variants::merge_delta_into` which is already `pub(crate)`) |

`lower_cover_field_constraints` and `constraints_conflict` are both `pub(crate)` in
`segment.rs` — already accessible from `executor.rs`.  `resolve_distributions` is
`pub` in `models.rs`.  `try_merge_incremental` needs to be made `pub(crate)` in
`segment.rs` (currently private).

## Files

| File | Change |
|------|--------|
| `lib/plan.rs` | `pub(crate)` on `distribute_rows` and `merge_variant_fields` |
| `lib/segment.rs` | `pub(crate)` on `try_merge_incremental` |
| `lib/executor.rs` | Add `generate_member_batch` and `generate_member_expanded_batch` helpers; replace `generate_fresh_batch(&m.dataset.data, …)` and `generate_expanded_batch(&m.dataset.data, …)` calls in `execute_lower_cover_group_core` with the new helpers |
| `tests/statistical/test_insurance.py` | Remove `@_BUG_VAR` marker from the five affected tests after implementation |

## Test plan

### Statistical tests (existing xfails flip to passing)

The five `@_BUG_VAR`-marked tests in `test_insurance.py` should pass once the fix
is applied:

- Variant value membership: `billing_period ∈ {monthly, quarterly, annual}` for all
  premiums rows
- Variant value membership: `payment_method ∈ {direct_debit, card, bank_transfer}`
  for all premiums rows
- Chi-squared distribution: `billing_period` ratios match declared 0.5/0.3/0.2
- `claim_type` and `claims.status` variant membership and distributions

### New unit test: `test_variant_lower_cover_member_applies_variant_values`

Add to `tests/executor_tests.rs`:

- Schema: parent P with 100 rows; single lower cover member M at ratio 0.6
- M has two variants: V0 sets a string field to `"alpha"` (50%), V1 sets it to `"beta"` (50%)
- Assert the generated M batch has ~30 rows with `"alpha"` and ~30 with `"beta"`
- Assert no randomly generated strings appear in the variant field column

### New unit test: `test_variant_incompatible_with_segment_constraint_is_pruned`

Add to `tests/executor_tests.rs`:

- Parent P with two lower cover members: sibling S (constrains `status = "active"`)
  and member M with two variants: V0 pins `status = "active"`, V1 pins `status = "lapsed"`
- In the segment where both S and M appear, V1 is incompatible with `status = "active"`
- Assert Level 2 generates only V0 rows for M in that segment
- Assert output batch has `status = "active"` throughout for M's rows
