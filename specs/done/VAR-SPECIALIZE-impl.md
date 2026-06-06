# VAR-SPECIALIZE — Implementation plan

Companion to [`VAR-SPECIALIZE.md`](VAR-SPECIALIZE.md). The spec covers the design (the
generator spectrum, carrier/support merge, the four cases); this doc covers PR sequencing,
intermediate green-points, and the one decision that gates the variant-subset case.

## Status

**COMPLETE — S1–S5 ✅ (green, 299 tests).** All four cases done; VAR-UNIFY complete too. The
entire variant roadmap is landed. Sequence as executed:

> **S1 ✅ → S3 ✅ → VAR-UNIFY U4 + Phase 2 ✅ → S2 ✅ → S4 (a/b/c/d) ✅ → S5 ✅.**

Only deferral across the whole effort: **multi-level marginal-pinning** (a restriction nested
in a deeper include level isn't seen by the parent's `preserve_marginal` subdivision).

## Four cases, recapped (see spec for detail)

- **Case 2 — generator-domain specialisation** (PR S2). A child pins a parent open-domain field
  with `value:` / `one_of:`. Pure merge-model change; resolves the "Generator-plus-value"
  known limitation. (Merge already landed in S1; S2 adds the `one_of` generator + wiring.)
- **Case 3 — constraint-bearing variant carrier on a ref'd field** (PR S3; the **U4 unblocker**).
  A `ref` field carries a `variants:` value-distribution that specialises the inherited field
  per case, each case entering segmentation. The capability top-level `variants:` have and
  field variants can't — **the gate for VAR-UNIFY U4.**
- **Case 1 — variant-subset restriction** (PR S4). `one_of` keeps a subset of a parent union's
  cases (by value **or** name). Has the open Option A/B fork.
- **Case 4 — per-case specialisation** (PR S5). `constrain_cases` tightens named cases of a
  ref'd parent variant, non-restrictively. Reuses S1's merge ("a case is a field"); additive.

## Ordering rationale (interleaved)

1. **S1 — merge model** ✅. The shared foundation (spectrum, carrier/support `Merge`, no more
   `value + generator` conflict). Everything leans on it; clears a known limitation.
2. **S3 — case 3 (U4 unblocker)** — *next*. `ref` + `variants` → constraint-bearing lowering.
   Depends on S1 (the general case needs `value + generator` merge); does **not** need S2/S4.
3. **VAR-UNIFY U4 + Phase 2** — circle back the moment S3 lands (retire top-level `variants:`;
   then delete the cross-product machinery). See [`VAR-UNIFY-impl.md`](VAR-UNIFY-impl.md).
4. **S2 — `one_of` finite-set generator** (case 2 complete). Standalone-useful; independent of
   U4, so it returns after the U4 payoff. (Could equally precede S3 if preferred — low stakes.)
5. **S4 — case 1 (variant-subset)**. Needs the Option A/B decision first.
6. **S5 — case 4 (`constrain_cases`)**. Per-case tightening; builds on S1's merge + S4's
   case-addressing. Additive richness, blocks nothing.

## S4 decisions — settled (was the A/B fork)

All resolved (spec §Variant-subset, §Marginal preservation):
- **Option A vs B → A** (pure implementation detail; the atom-up engine makes A
  topology-complete, verified on a 3-level chain). B is a documented fallback.
- **Real spine is carrier propagation**, not A/B: a restricted parent variant is **lowered into
  the lattice as case sub-populations** (OQ1 partition — only restricted variants lowered).
  This also fixes the carrier-loss bug (a ref'd variant currently generates garbage).
- **Marginal preservation is solvable** — a balanced transportation problem; feasibility via the
  Gale–Hoffman cut condition; IPF preserves margins when feasible. **`p_c` free by default**
  (always feasible), opt-in to pin, cut-validated.

---

## PR S1 — the merge model (`constraints.rs`) — **DONE (green, 289 tests)**

Make `FieldConstraints::Merge` the carrier/support spectrum from spec §Generalised merge
semantics. Behaviour-changing but localised.

**As-built notes:**
- `FieldConstraints` gained `one_of: Option<Vec<YamlValue>>` (populated from `Field.one_of`
  in S2; `From<&Field>` sets `None` for now). `Merge` rewritten as a value-source spectrum
  (`Source` enum + `merge_source`): tightest compatible source wins (`value` ≺ `one_of` ≺
  `generator`), supports intersect, bounds intersect. `satisfiable` + `validate_field_constraints`
  no longer reject `value + generator`; the only numeric error is a constant `value` outside
  its range. Old `merge_equal` helper removed.
- **Behaviour change rippled to tests (all intended):** `tests/constraints_tests.rs` two
  satisfiable tests flipped (value+min within bounds, value+generator now satisfiable);
  `validate_tests.rs` `test_value_with_generator_errors` → `…_specialises` (passes),
  `test_value_with_min_max_errors` → `test_value_within_range_passes` + new
  `test_value_outside_range_errors` (fixture `value_below_range`). `segment.rs` DFS picks up
  the new merge for free (full suite green confirms no segmentation regressions).
- **CLAUDE.md** "Generator-plus-value should specialise, not conflict" known limitation
  **removed** (resolved).

- **`lib/constraints.rs`** — rewrite `Merge` per the matrix: `merge(generator, value) = value`;
  `merge(generator, one_of) = one_of`; `value`/`value` conflict iff differing; ranges
  intersect. Revise `satisfiable` and `validate_field_constraints` to **stop rejecting
  `value + generator`** (constraints.rs:31-71). Keep the `value` outside `min/max` check.
- **`lib/models.rs`** — add `one_of: Option<Vec<YamlValue>>` to `FieldConstraints` (the field
  on `Field` lands in S2 with the user-facing key).
- **`lib/segment.rs`** — no change: DFS conflict-pruning already calls `Merge`, so it picks up
  the new behaviour for free (add a test asserting it).
- **`CLAUDE.md`** — remove the "Generator-plus-value constraint should specialise, not
  conflict" known limitation.
- **Tests:** `Merge` table tests for every matrix row; a DFS-pruning test where a child
  `value` specialises a parent `generator` field (previously errored, now merges).

**Green-point:** parent `generator:` + child `value:` specialises instead of erroring; full
suite green.

## PR S2 — `one_of` finite-set generator (case 2 complete) — **DONE (green, 294 tests)**

**As-built notes:**
- `Field.one_of: Option<Vec<YamlValue>>` added; `FieldConstraints::from` now populates it
  (the S1 placeholder is gone). Both `resolve_refs` ref-field builders carry `merged.one_of`.
- **Generation reuses U5:** `one_of` is sugar for a uniform, value-only same-type variant —
  `generate_column_raw` synthesises ratio-less `FieldVariant`s and dispatches to
  `build_same_type_variant_column`. So standalone `one_of` *and* `one_of`-via-`apply_constraints`
  (the specialisation path) both work with no separate generator.
- `executor.rs::apply_constraints` gained a `one_of` arm → the merged constraint reaches the
  shared atom column, so `one_of` on a ref field restricts the parent's `generator` domain
  (merge: `generator + one_of → one_of`) end-to-end.
- `validate.rs`: `one_of` non-empty; `value` + `one_of` mutually exclusive; entries type-checked
  against the field type.
- Tests: standalone `one_of` (string + numeric, membership + coverage); `one_of` specialising a
  parent `generator: word` through the segment pipeline; validation (value+one_of, type mismatch).
  Docs: `one_of` row in `yaml-schema.mdx` + docgen.

<details><summary>Original S2 plan</summary>

- **`lib/models.rs`** — `one_of: Option<Vec<YamlValue>>` on `Field` (YAML key `one_of`).
- **`lib/validate.rs`** — `one_of` non-empty; `value` + `one_of` mutually exclusive; subset
  check when the parent field is a tagged union; accept + intersect with `range`.
- **`lib/rewrite.rs`** — `resolve_refs` propagates a child's `value`/`one_of` into the parent
  column's `FieldConstraints` via the new `Merge`.
- **`lib/generator.rs`** — `generate_column` honours `one_of` (uniform pick when no single
  `value`), numeric + string.
- **`lib/executor.rs`** — `apply_constraints` gains an `one_of` arm so it reaches the shared
  atom column (`generate_segment_atom_batch`).
- **`src/docgen.rs` / `reference/yaml-schema.mdx`** — document `one_of`.
- **Tests:** statistical — parent `generator: word`, child `one_of: [...]`, assert child rows
  drawn only from the set; unit — `one_of` numeric uniform.

**Green-point:** `one_of` works as a standalone finite-set generator and as a specialisation.

</details>

## PR S3 — case 3: `ref` + `variants` (the VAR-UNIFY U4 unblocker) — **DONE (green, 292 tests)**

Allow a `ref` field to carry a `variants:` value-distribution; lower it so each case-member
inherits the ref **and** pins the case value, entering segmentation. **This is the gate for
VAR-UNIFY U4.**

**As-built notes:**
- The whole change is in the *expansion* path (no new planner/merge machinery): a `ref` +
  `variants` field reuses the existing same-type field-variant pipeline. `collect_variant_paths`
  now collects any field with non-empty `variants` (not just `type: variant`) and captures the
  field's `refs`; `build_delta_field` stamps that ref onto each cross-product delta (so each
  delta is `{type: inferred, ref, value}` — exactly the shape the old top-level form wrote by
  hand); `stub_variant_fields` keeps the ref stub (clears `variants`) so `resolve_refs` still
  resolves the field. The resulting `dataset.variants` is **identical** to the pre-migration
  top-level form, so `lower_member_variants` lowers it unchanged.
- `validate.rs`: a `ref` field may now carry `variants` (`validate_case3_variants` — cases must
  be value-source-only, no object cases; usual distribution-sum rules).
- **Migration = regression proof:** `variant_pruned_by_segment` rewritten to the case-3 form;
  the existing pruning test stays green. Added `test_ref_variants_both_cases_survive` and
  `test_ref_variants_specialise_generator_parent` (the latter exercises S1's value-beats-
  generator merge through the case-3 path) + a validation test for the object-case error.
- Docs: `reference/yaml-schema.mdx` gained a `ref` + `variants` note.

**→ VAR-UNIFY U4 is now unblocked.** Next per the interleave: circle back to U4 + Phase 2.

- **`lib/validate.rs`** — allow `ref` + `variants` together (currently `ref` bans `type` and
  `variants` requires `type: variant`). New shape: a `ref` field whose `variants` cases supply
  only value-sources (`value`/`generator`/`range`/`one_of`) — *not* structural keys. Validate
  the cases are value-only and the distribution sums as usual.
- **`lib/expand_variants.rs`** — `collect_variant_paths` recognises a `ref` field bearing
  `variants:` (no `type: variant`) as a variant path so it is lowered, not stubbed away.
- **`lib/plan.rs`** — `lower_member_variants` carries the field's `ref` onto each case-member
  alongside the case's `value` (so the case-member is an inherited, value-pinned column).
  Confirm the lowered case-member enters the DFS conflict pruning against a sibling pinning the
  same ref'd column (it already does via `value` constraints — the ref just makes them the
  same column).
- **Migration as regression test:** rewrite `tests/fixtures/execute/variant_pruned_by_segment`
  from its top-level-variant form to the field-variant form (`category: ref p.category` +
  `variants: [{value: premium}, {value: basic}]`). The existing test (all `member.category ==
  "premium"`, `basic` pruned by the sibling) **must stay green** — that is the proof case 3
  reproduces the capability.
- **`src/docgen.rs` / docs** — document `ref` + `variants`.
- **Tests:** the migrated `variant_pruned_by_segment` (pruning preserved); a positive case
  where two cases both survive (no conflicting sibling); a general case with a parent
  `generator:` parent field (leans on S1's merge).

**Green-point:** a constraint-bearing variant on an inherited field works as a field variant —
**VAR-UNIFY U4 is now unblocked.**

## PR S4 — case 1: variant-subset restriction (`one_of` on a parent variant) — **DONE (S4a/b/c/d, green, 297 tests)**

**As-built (S4a + S4b + S4d):**
- **S4a carrier propagation** — `resolve_refs` now carries `variants` (the carrier) onto a ref'd
  field (both builders), so a ref'd variant keeps its cases and generates real values. *Root
  cause of the `three_level_chain` garbage was exactly this drop.*
- **S4b `one_of` restricts the carrier** — `build_same_type_variant_column` filters cases to
  those whose value ∈ `one_of` and **renormalises** the surviving ratios (sum<1 was dumping the
  tail mass on the last case — fixed). So `merge(Variant, one_of) = Variant[subset]`, not flat
  uniform. Works through the segment-atom path (`apply_constraints` already carries `one_of`).
- **S4d docs** — `concepts/variant-lowering.mdx` gained "Specialising a variant in a child"
  (the animals/cats example, renormalisation, and the free-by-default "parent mix shifts; ratios
  are a specialisation concern" note).
- Test: `test_three_level_variant_carrier_and_restriction` — valid cases at all 3 levels + cats
  restricted ~50/50. Verified end-to-end (`three_level_chain`: cats 135/115 birds/mice; pets &
  animals all-valid).

**S4c — marginal *pinning* (DONE, opt-in).** `preserve_marginal: true` on a parent variant pins
the declared case ratios as a global marginal. Implementation: after `plan_segments`,
`subdivide_for_pinned_variants` subdivides each segment by the variant's cases — **2-D IPF**
(segment row-totals × case marginals, structural zeros where a member restricted the subset),
largest-remainder rounded — and pins each sub-segment to one case via a singleton `one_of`
(reusing S4b generation). **Feasibility:** the Gale–Hoffman cut (both single-set directions) is
checked up front → precise infeasibility error. Verified: `pinned_marginal` fixture holds
animals at 250/250/250/250 while cats are birds/mice; `pinned_marginal_infeasible` (cats 0.6 ⊂
birds/mice vs pinned 0.5) errors. `preserve_marginal` validated as variant-only.
**Scope:** single-group (restrictions visible in the parent's own segments). A restriction
nested in a *deeper* include level isn't seen by the parent's subdivision — multi-level pinning
is the one remaining deferral.

<details><summary>Original S4 plan</summary>

**Decisions settled** (see spec §Variant-subset, §Marginal preservation): Option **A** (impl
detail); `p_c` **free by default**; restricted variants are **lowered into the lattice as case
sub-populations**; marginal preservation is the existing factoring/IPF. The spine — in order:

**S4a — carrier propagation (the actual bug fix; do first).** Today a variant's `variants`
carrier is dropped at the ref boundary, so a ref'd variant generates garbage (see the
`three_level_chain` fixture). Carry the carrier so a restricted parent variant is **lowered
into the lattice as case sub-populations** and generates real values.
- `lib/plan.rs` — when a parent variant is `ref`'d-and-restricted by a child (OQ1 rollup),
  lower it (cases → sub-populations / case-members with ratios `p_c`); unrestricted variants
  stay per-row (U5). Reuses the VAR-EXPAND `lower_member_variants` machinery.
- Regression target: the `three_level_chain` fixture's `animals`/`pets` `eats` must come out as
  valid cases (birds/mice/grass/fish), not random strings.

**S4b — `one_of` restriction = carrier/support merge + conflict pruning.**
- `lib/validate.rs` — `one_of` on a ref'd variant matches a parent case by **value or name**
  (names required for object/heterogeneous cases); strict-subset (full set warns).
- `lib/constraints.rs` — `merge(Variant[N], one_of[M]) = Variant[M]` (keep carrier, intersect
  support, renormalise ratios over the survivors) — *not* the flat `one_of`. The child's
  restriction prunes out-of-subset case sub-populations via the existing DFS conflict pruning.
- `lib/generator.rs` — generating a restricted variant draws from the surviving (renormalised)
  cases.

**S4c — parent marginals: free-by-default + cut-condition validation.**
- `p_c` unset ⇒ free (the factoring/IPF solves them; always feasible). `p_c` set (whole or
  partial) ⇒ opt-in; `lib/segment.rs`/`lib/validate.rs` check the **Gale–Hoffman cut
  condition** and emit a precise error on infeasibility ("cases {…} demand X% but only Y% of
  rows can carry them"). Within feasibility the existing IPF + largest-remainder preserves them.

**S4d — docs (approachable).** A concepts-page section (`variant-lowering.mdx` or a new
`concepts/variant-specialisation.mdx`) explaining, in plain terms with the `animals`/`cats`
example: restricting a parent variant in a child; "leave parent ratios unset and let children
specialise" as best practice; what setting parent ratios means (preserved when possible);
and the error you get when an over-pinned marginal is infeasible. Plus `yaml-schema.mdx` updates.

- **Tests:** `three_level_chain` valid-cases regression (S4a); `animals`/`cats` subset by value
  *and* by name; two children with overlapping-but-unequal subsets → intersection fires;
  statistical — free parent ratios skew correctly; fixed parent ratios are preserved (χ²);
  over-pinned marginal → validation error.

**Green-point:** a restricted parent variant generates valid cases; restriction honoured (value
or name); parent marginals free-by-default, preserved when set + feasible, clear error when not.

</details>

## PR S5 — case 4: per-case specialisation (`constrain_cases`) — **DONE (green, 299 tests)**

**As-built:**
- `CaseDelta { name, generator, value, range }` + `Field.constrain_cases: Vec<CaseDelta>`
  (models). `one_of`-on-a-case dropped as degenerate (a case is a single value-source).
- Flows through the **segment pipeline** like `one_of`: `FieldConstraints` gained
  `case_overrides: Vec<CaseDelta>` (populated by `From`, carried through `Merge` by concat,
  ignored by `satisfiable`/pruning). `apply_constraints` narrows the matching variant case's
  value-source in place (intersect range, override value/generator) — so the shared atom column
  reflects it; generation is unchanged (reads the narrowed carrier). `resolve_refs` carries
  `constrain_cases` onto the resolved member field so `lower_cover_field_constraints` picks it up.
- `validate`: `constrain_cases` requires a ref; each delta needs a `name`.
- Tests: `constrain_cases` fixture (child `high` case capped at 60 → all child scores ≤ 60,
  parent's high still > 60); validation (no-ref error). Docs: concepts section + yaml-schema +
  docgen.

<details><summary>Original S5 plan</summary>

Tighten named cases of a ref'd parent variant without dropping any. "A case is a field," so
this reuses S1's `Merge` — no new merge machinery.

- **`lib/models.rs`** — `constrain_cases: Vec<CaseDelta>` on `Field` (a `CaseDelta` is a
  parent case `name` + value-source/bounds deltas only).
- **`lib/validate.rs`** — valid only on a field that refs a `type: variant`; each `name` must
  match a parent case; entries carry value-source/bounds keys only; the per-case delta must
  merge satisfiably with that case's field; enforce the `(parent carrier × key)` table.
- **`lib/rewrite.rs` / `lib/plan.rs`** — route each delta to the matching lowered case-member
  (or `DenseUnion` case field) and merge it via `FieldConstraints::Merge`. Non-restrictive:
  unlisted cases pass through; case **ratios are untouched** (the merge-only-narrows invariant).
- **`src/docgen.rs` / docs** — document `constrain_cases` + the verbs/disambiguation table.
- **Tests:** narrow one case's `range`, assert that case's values are bounded while siblings
  are unchanged and **case ratios are preserved**; a delta that conflicts with the case's
  field → validation error; `constrain_cases` on a scalar ref → placement error.

**Green-point:** individual parent cases specialise by name without repeating the variant
structure; ratios preserved.

</details>

---

## Circle back: VAR-UNIFY U4 + Phase 2

Once **S3** lands (case 3), return to [`VAR-UNIFY-impl.md`](VAR-UNIFY-impl.md):

- **U4** — retire top-level `variants:` as user input (`#[serde(skip)]` + migration error),
  migrate the remaining top-level-variant fixtures to field variants (the four output-shape
  ones + `variant_pruned_by_segment`, already migrated as S3's regression test), delete the
  redundant `variant_*` validation fixtures (covered by `field_variant_*`), strip the
  top-level `variants:` docs / `VariantSchema` `TypeDoc`.
- **Phase 2 (U5–U7)** — per-row categorical for same-type field variants; delete the
  cross-product machinery (`plan_variant_steps` / `CombineVariantBatches` / `VariantSchema` /
  `SyntheticDataset.variants`). **Add the linked-dataset-with-variant fixture *first*** (green
  on the current machinery) as the regression guard before the U5/U6 refactor.

U4 only blocks on **S3**. Everything else — **S2** (`one_of` generator), **S4** (variant-subset),
**S5** (`constrain_cases`) — is independent of U4 and lands *after* the circle-back in the
interleaved sequence (`S1 ✅ → S3 → U4/Phase 2 → S2 → S4 → S5`). Order among S2/S4/S5 is
flexible; S5 builds on S4's case-addressing so it comes last.

## Dependencies

| Spec | Relationship |
|------|--------------|
| VAR-EXPAND (complete) | `lower_member_variants` / segmentation — case 3 extends it to carry a ref; cases 1/2 feed its conflict pruning |
| VAR-1 (complete) | Multi-type union substrate; per-case generator spectrum the carrier/support merge narrows |
| VAR-UNIFY U1–U3 (complete) | `flatten` output story (orthogonal); `discriminant_tag_column` (visible) is distinct from Option B's `_disc_` sentinel |
| VAR-UNIFY U4 / Phase 2 | **Blocked on S3** — retiring top-level variants needs case 3. Circle back after. |
| SEG-1 / SEG-ATOM-1 (complete) | DFS + IPF + `apply_constraints` atom path the new constraints flow through |
