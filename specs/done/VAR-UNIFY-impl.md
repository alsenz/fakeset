# VAR-UNIFY — Implementation plan

Companion to [`VAR-UNIFY.md`](VAR-UNIFY.md). The spec covers the design (the `flatten`
primitive, retiring top-level `variants:`, the OQ resolutions); this doc covers PR
sequencing, intermediate green-points, and the two ordering constraints that shape it.

## Status

**COMPLETE (U1–U7), green at 290 tests.** Phase 1 (`flatten` + output unification + retire
top-level `variants:` as user input) and Phase 2 (same-type variants → per-row; delete the
top-level variant machinery) are both done. **Scope correction:** the cross-product
(`VariantSchema` / `SyntheticDataset.variants` / `build_local_combinations` /
`lower_member_variants`) is **kept** — case-3 (`ref` + `variants`, VAR-SPECIALIZE S3) rides it.
Only the **top-level** path (`plan_variant_steps` / `CombineVariantBatches` /
`expand_variant_dataset` / `variant_key` / `distribute_rows`) was deleted. Removing the cross-
product outright (OQ3) is a future cleanup (refactor `lower_member_variants` to read case-3
fields directly). Next on the roadmap: **VAR-SPECIALIZE S2 → S4 → S5.**

## Two ordering facts that drive the sequencing

1. **`SyntheticDataset.variants` is doing double duty.** It is *both* the user-facing
   top-level feature *and* the internal storage where `expand_field_variants` writes the
   **cross-product of same-type field variants** (`expand_variants.rs:40` —
   `dataset.variants = local_combos | cross_product_variants(...)`), which `plan_variant_steps`
   (`plan.rs:601`) then turns into N variant batches + a `CombineVariantBatches`. So you
   **cannot** delete the field (OQ3's "remove outright") in Phase 1 while keeping the
   cross-product. "Remove outright" is therefore a *Phase 2* outcome; Phase 1 removes only the
   YAML **key** and leaves the field as an internal artifact.

2. **The constraint-bearing path is already separate and stays untouched.** A *lower-cover
   member's* variant is lowered by VAR-EXPAND's `lower_member_variants` (`plan.rs`), **not** by
   `plan_variant_steps` (the latter early-returns for members:
   `plan.rs:232`). So `plan_variant_steps` + the cross-product is *exactly* the **top-level
   dataset variant** path. Phase 2 only re-points *that* path; member-variant lowering — and
   the whole OQ1 ref-driven segmentation story — is left exactly as VAR-EXPAND built it.
   (The OQ1 *optimisation* — don't lower a pure value/shape member variant — is correctness-
   neutral and explicitly **future**, see §Out of scope.)

Net: top-level variation becomes a `flatten`ed union **field**, which (being heterogeneous,
object-cased) is a VAR-1 `DenseUnion` — so it rides the field-variant pipeline wholesale and
needs no top-level machinery of its own.

## Scope

**In scope.**
- A `flatten: bool` primitive on `object` / `variant` fields — a write-time pull-up (§OQ4: one
  level; on a variant, distributed to object cases).
- The output-encoding story it subsumes from VAR-1-OUTPUT-FLAG: JSON per-row keys, Parquet
  superset (default) / prefixed / discriminant strategies.
- Retiring user-facing top-level `variants:` (migrate examples/fixtures/docs to a `flatten`
  field).
- **Phase 2:** per-row categorical generation for same-type field variants, and deletion of
  the cross-product machinery + `VariantSchema` + `SyntheticDataset.variants` +
  `plan_variant_steps` + `CombineVariantBatches`.

**Out of scope (future, not precluded).**
- The OQ1 **partial-variant** optimisation (some cases `ref`, some replace → split per-case)
  and "don't lower a pure value/shape *member* variant." Correctness-neutral; a scale
  optimisation on top of the existing lowering. Tracked as **VAR-UNIFY-OPT**.
- Anything in VAR-SPECIALIZE (`one_of`, generator-domain merge) — scheduled *after* this.

## The one thing to verify before PR U2

`flatten` JSON "per-row keys" relies on the Arrow JSON writer **omitting null keys**.
`arrow-json` has `WriterBuilder::with_explicit_nulls(bool)`, default `false` → null fields
are not emitted. So a superset struct with the inactive cases null serialises, per row, to
exactly the active case's keys. **First task of PR U2: a unit test asserting this** (a 2-row
batch, two object cases, assert each JSONL line carries only its active case's keys). If it
ever regressed, the fallback is to build a per-row-pruned encoder; not expected.

---

# Phase 1 — `flatten` + output unification + retire top-level `variants:`

## PR U1 — `flatten` field + validation (no writer change yet)

Parser + validation only; the writer still emits nested (a flatten field is a no-op at
output until U2). Behaviour-neutral for every existing schema.

- **`lib/models.rs`** — add `#[serde(default)] pub flatten: bool` to `Field`. Doc-comment:
  output-only pull-up; requires a name; valid on `object`/`variant`.
- **`lib/validate.rs`** — new checks in `validate_field`:
  - `flatten` only on `object` or `variant` (else error).
  - a `flatten` field must have a non-empty `name` (it is the ref identity; §Names and refs).
  - **name-collision check** (the static half): a flatten object's pulled-up field names, and a
    flatten union's case field names *across cases*, must not collide with sibling field names —
    unless a non-colliding Parquet strategy is selected (deferred to U3; for U1 the default
    `superset` means cross-case collisions are an error). For JSON-only datasets, only
    *same-case* collisions matter; gate by `format`.
  - ~~**ref-into-case warning**~~ **→ moved to U2.** Detecting "a `ref:` targets a field
    *inside a union case*" needs to resolve the ref path against the target dataset's case
    structure — that context lives in the rewrite/union machinery (U2), not the pre-expansion
    validator. Deferred to keep U1 to checks the validator can actually make.
- **`src/docgen.rs`** — add a `flatten` `FieldDoc` row; note it on `object`/`variant` in the
  `FieldType` enum doc.
- **`docs/.../reference/yaml-schema.mdx`** — document `flatten` on the `Field` table.
- **Tests** (`tests/validate_tests.rs` + fixtures): `flatten` on a scalar → error; `flatten`
  without a name → error; cross-case collision under default strategy → error; ref-into-case →
  warning.

**Green-point:** all existing tests pass unchanged; new validation tests pass; flatten parses
but does nothing at output yet.

## PR U2 — flatten-aware writer (superset default) — **DONE (green)**

Generalise the VAR-1 write-time conversion so a `flatten` field's sub-columns are spliced up
to the **batch top level** instead of left nested. This is the heart of the feature.

**As-built notes:**
- **Gate verified.** `flatten_union_jsonl_emits_per_row_keys` confirms the Arrow JSON writer
  omits null keys (default `explicit_nulls=false`) — per-row keys work with no custom encoder.
- `prepare_output_batch(batch, fields)` added to `executor.rs`: fast-path delegates to
  `unionize_for_output` when no flatten fields; otherwise iterates columns, splicing flatten
  fields (`flatten_column` → `flatten_union_to_columns` for unions, struct-children for
  objects) and passing the rest through `union_to_portable`. `write_output` gained a
  `&[Field]` param, wired from `WriteSharedOutput.schema` (the only caller).
- **No `schema.rs` change needed** — the output schema falls out of the spliced Arrow fields;
  no separate post-flatten schema helper was required.
- **Scope: nested flatten gated at validation.** Only top-level flatten is implemented; a
  flatten field inside an object now **errors** (`validate_flatten`, `prefix != ""`) rather
  than silently emitting nested. (Supersedes U1's nested cross-case collision check, which is
  now unreachable — kept for when nested flatten lands.) Lift the gate in a later PR if needed.
- Tests: 4 in `flatten_output` (union shape, JSON per-row-keys gate, Parquet superset
  round-trip, object pull-up) + end-to-end `test_flatten_pulls_fields_to_row_level`
  (`flatten_variant` fixture: union + object, both wrappers elided, one case per row) +
  `test_flatten_nested_not_supported_errors`. 273 tests green.

- **First: the JSON null-omission verification test** (above).
- **`lib/executor.rs`** — generalise `unionize_for_output` (currently union→nested-struct) into
  a **flatten-aware** pass run in `write_output`:
  - Non-flatten union field → unchanged (nested nullable-superset struct, VAR-1 behaviour).
  - **flatten object field** → drop the struct wrapper; splice its sub-`(field, array)` pairs
    into the batch's top-level field/column lists (one-level elision).
  - **flatten union field** → distribute to object cases (§OQ4): build the nullable superset of
    *case fields* (not case-named sub-structs — go one level deeper than `union_to_portable`
    does today: take the inner case-struct's children, null where the case didn't fire), and
    splice those up to the batch top level. Scalar cases → one case-named column each.
  - The conversion needs the *schema* (which fields are `flatten`) — it currently works off the
    Arrow batch alone. Thread the dataset `Schema` (or a precomputed `HashSet<flatten field
    names>`) into `write_output`/the conversion. `write_output` already receives enough to reach
    the dataset; if not, add a `&[Field]` parameter (the `WriteSharedOutput`/`EmitDataset` steps
    already carry the schema — `WriteSharedOutput.schema`).
- **`lib/schema.rs`** — a helper computing the *output* (post-flatten) Arrow schema for a field
  set: a flatten field contributes its pulled-up columns; everything else unchanged. Write-path
  only — the internal/generation schema is untouched.
- **Tests** (`tests/executor_tests.rs` + `tests/fixtures/execute/`):
  - flatten **object** field → JSONL has the sub-fields at row level; Parquet has them as
    top-level columns.
  - flatten **union** field → JSONL row carries only the active case's keys; Parquet has the
    case-field superset at row level, one case populated per row.
  - non-flatten union still emits the nested struct (VAR-1 regression guard).

**Green-point:** `flatten` works end-to-end for object and union fields; top-level `variants:`
still exists and is untouched.

## PR U3 — Parquet flatten strategies (`superset` / `prefixed` / `discriminant`) — **DONE (green)**

Make the Parquet collision/identity story configurable; folds in VAR-1-OUTPUT-FLAG.

**As-built notes:**
- **`lib/models.rs`** — new `FlattenStrategy` enum (`Superset` default / `Prefixed` /
  `Discriminant`) + `flatten_strategy: Option<FlattenStrategy>` on `Field`. **Not** folded into
  the `parquet` block (which is a per-field *type* override) — kept as its own field, clearer.
  New `discriminant_tag_column(field) = "<field>_case"` helper — a **visible** output column,
  deliberately *not* the reserved `_disc_` sentinel (that's stripped from output).
- **`lib/executor.rs`** — `prepare_output_batch`/`flatten_column`/`flatten_union_to_columns`
  now take the format + strategy. `Prefixed` namespaces object-case fields `<case>_<field>`;
  `Discriminant` appends the `<field>_case` tag (built from `type_ids`). **JSON/JSONL force
  `Superset`** (per-row keys; strategies are a flat-columnar concern).
- **`lib/validate.rs`** — collision checks run on the *effective* (strategy-resolved) names, so
  `prefixed` resolves cross-case collisions naturally; added a `discriminant` tag-collision
  check and a "`flatten_strategy` only on a flatten variant" placement check.
- **`src/docgen.rs` / `reference/yaml-schema.mdx`** — `flatten_strategy` field row +
  `FlattenStrategy` type section. (Concept-page prose deferred to U7 close-out.)
- **Tests (+5 → 278):** in-memory `prefixed`/`discriminant`/`jsonl-ignores-strategy`;
  validation `prefixed resolves collision` + `flatten_strategy on object errors`. Also
  confirmed JSON tolerates the duplicate same-named columns two cases produce (one fires per
  row → one key). fmt/clippy/docs all green.

**Note on CSV:** still gated for heterogeneous variants (Rule 0b). A `flatten` of *all-scalar*
cases is in principle CSV-writable (scalar superset), but unblocking that is left as future
work — the flatten strategies here target Parquet.

> **Mergeable with U2** if it keeps the diff readable; kept separate here so U2 ships the
> minimal working default first.

## PR U4 — retire top-level `variants:` as user input — **DONE (green, 290 tests)**

**As-built notes:**
- **Rejected at validation** rather than `#[serde(skip)]` + capture: validation runs *before*
  `expand_field_variants`, so a non-empty `dataset.variants` at that point can only have come
  from YAML — Rule 0 now `bail!`s with a migration message. Cleaner UX than a silent serde
  drop. `SyntheticDataset.variants` stays as the internal field-variant cross-product artifact
  (Phase 2 / U6 removes it + `VariantSchema`).
- **Migrated** the 4 output-shape fixtures to `type: variant` fields (`execute/variants`,
  `variant_sibling`, `variant_in_lower_cover`, `import_variants`) — behaviour-transparent
  (same post-expand `dataset.variants` → same plan/execute), so `plan_tests` + the execute
  tests passed untouched. `variant_pruned_by_segment` was already migrated in S3.
- **Deleted** the 3 redundant top-level distribution fixtures/tests (`variant_valid`,
  `variant_bad_sum`, `variant_all_set_wrong`; covered by `field_variant_*`); added
  `top_level_variants_retired` + `test_top_level_variants_rejected`.
- **Docs/docgen**: removed the dataset `variants` row + the `VariantSchema` type
  (docgen + `yaml-schema.mdx`); reworded the `variant` FieldType docs away from "global dataset
  variants". Examples already used field variants (no migration). Both examples smoke-tested.

---

### Original plan (for reference)

> **Blocker found during U4 (resolved by resequencing — Tom).** Top-level `variants:` carry
> *two* capabilities: (a) **output-shape** variation — fully subsumed by `flatten` (U1–U3,
> done); and (b) **constraint-bearing per-case specialisation of an inherited field** — a
> variant whose cases carry `ref: parent.field` + a value (e.g.
> `tests/fixtures/execute/variant_pruned_by_segment`, where a sibling's `category=premium` pin
> prunes the `basic` case). `FieldVariant` has **no `refs`**, so (b) has **no field-variant
> equivalent today** → top-level variants can't be fully retired yet.
>
> **Resolution: interleave VAR-SPECIALIZE with VAR-UNIFY.** U4 blocks only on VAR-SPECIALIZE
> **case 3 / PR S3** (`ref` + `variants` on a field; see
> [`VAR-SPECIALIZE-impl.md`](VAR-SPECIALIZE-impl.md)), which delivers capability (b).
> `variant_pruned_by_segment` is migrated *there* as S3's regression test. Interleaved order:
> **S1 ✅ → S3 → U4 (+ Phase 2) → S2 → S4 → S5.** So U4 resumes the moment S3 lands — *not*
> after all of VAR-SPECIALIZE; the rest (`one_of` generator, variant-subset, `constrain_cases`)
> follows the U4 payoff.

Remove the *feature*, keep the *internal field* (Phase 2 deletes the field).

- **`lib/models.rs`** — change `SyntheticDataset.variants` from a deserialised field to an
  **internal artifact**: `#[serde(skip)]` so YAML can no longer set it; it is now populated
  *only* by `expand_field_variants`. (Field-variant cross-product still lands here — that's
  Phase 2's to remove.)
- **`lib/validate.rs`** — if the raw YAML carried a top-level `variants:` key, error with a
  migration message pointing at `flatten`. (Because `#[serde(skip)]` silently drops unknown
  keys, detect via a `#[serde(deny_unknown_fields)]`-style guard or a pre-parse check — simplest
  is a lightweight scan in the loader, or a deprecated-alias capture field that errors if set.)
- **Migrate every top-level `variants:` user.** *Survey finding (U4):* only **8 fixtures** use
  true top-level (column-0) `variants:` — the **insurance examples do NOT** (their `variants:`
  are all field-level, so no example migration is needed). The 8:
  - **Output-shape / value (→ plain field variant; no ref):** `execute/variants/orders` (status),
    `execute/variant_sibling/source` (tier), `execute/variant_in_lower_cover/member` (code),
    `execute/import_variants/stocks` (tier). Each becomes a `type: variant` field; member ones
    lower via VAR-EXPAND. Assert the same row-count/distribution invariants.
  - **Constraint-bearing (per-case ref):** `execute/variant_pruned_by_segment/member` — **already
    migrated in VAR-SPECIALIZE PR S3** as its regression test (`ref` + `variants`). Nothing to do
    here beyond confirming it still passes.
  - **Redundant validation fixtures (delete):** `validation/{variant_valid,variant_bad_sum,
    variant_all_set_wrong}` — top-level-variant distribution-sum checks, already covered by the
    `field_variant_*` fixtures.
  - docs: `concepts/variant-lowering.mdx`, `reference/yaml-schema.mdx` (remove the top-level
    `variants:` section; `src/docgen.rs` drop the `variants` `TypeDoc`/field on
    `SyntheticDataset`). *(semi-lattice.mdx / examples/insurance.mdx use field variants — check
    but likely no change.)*
- **`tests/statistical/`** — update any assertions keyed on the old top-level-variant output
  shape (insurance suite).

**Green-point:** no YAML uses top-level `variants:`; the feature is gone; field variants
(same-type via cross-product, heterogeneous via DenseUnion, member via lowering) all unchanged.
This is a clean stopping point if Phase 2 is deferred.

---

# Phase 2 — retire the internal cross-product machinery — **DONE (U5+U6, green 290 tests)**

Goal: same-type field variants stop routing through `dataset.variants`, so the top-level
variant apparatus can be deleted.

> **Partial-deletion finding (during U5).** `case-3` (`ref` + `variants`, shipped in
> VAR-SPECIALIZE S3) populates `dataset.variants` via the cross-product and feeds
> `lower_member_variants`. So **`build_local_combinations` / `VariantSchema` /
> `SyntheticDataset.variants` / `lower_member_variants` cannot be deleted** — they're load-
> bearing for constraint-bearing variants. The achievable cut (and what U6 did): delete the
> **top-level** path — `plan_variant_steps` + `CombineVariantBatches` (+ `expand_variant_dataset`,
> `variant_key`, `distribute_rows`) — which a **probe** confirmed unreachable post-U5.

## PR U5 — per-row categorical generation for same-type field variants — **DONE (green)**

**As-built notes:**
- `generator.rs` — `build_same_type_variant_column`: per-row categorical draw (reusing
  `build_union_column`'s sampling) + `interleave` to scatter each case's bulk-generated values
  back to row order. `generate_column_raw` dispatches to it whenever a field has non-empty
  `variants` (before the type match).
- `expand_variants.rs` — `collect_variant_paths` now collects **only case-3** (`ref` +
  `variants`); `finalize_variant_fields` gives a same-type variant its unified concrete
  `field_type` while **keeping `variants`** (so schema/refs see a typed field, the generator
  sees the cases). New `unified_variant_type` helper. Dead `cross_product_variants` removed.
- **No `resolve_refs`/`schema.rs` change needed** (the unified-type-keeps-variants trick).
- Tests: `linked_with_variant` regression guard (linked dataset + variant → one batch);
  `expand_variants` unit tests rewritten to the per-row reality; `plan_tests` updated
  (one GenerateDataset, no `*__v*` steps). Fixtures that relied on the old auto-default output
  gained explicit `output_file`.

<details><summary>Original U5 plan</summary>

The cheap path you already proved in VAR-1 (`build_union_column`'s per-row sampling),
specialised to **homogeneous** cases (same-type → emit the shared Arrow type directly, not a
`DenseUnion`).

- **`lib/generator.rs`** — add a homogeneous-variant column builder: resolve case ratios →
  cumulative weights → per-row independent draw → generate each row through the chosen case's
  `Field` (honouring per-case `generator`/`value`/`range`/`locale`/`parquet`). This is
  `build_union_column` minus the union wrapper. Reuse its sampling exactly (same statistical
  fix).
- **`lib/expand_variants.rs`** — for a **top-level** (non-lower-cover-member) dataset, stop
  cross-producting same-type field variants into `dataset.variants`; instead leave the field as
  a (new, internal) "same-type variant" column the generator resolves per row. Two options:
  (a) keep a `FieldType::Variant` marker + the `variants` on the *field* and have
  `generate_column` dispatch to the homogeneous builder; (b) a lower-time rewrite analogous to
  `lower_heterogeneous_unions` but emitting a per-row categorical marker. **Prefer (a)** —
  least new state, mirrors how `FieldType::Union` dispatches.
- **OQ1 partition guard:** apply the homogeneous per-row path **only** to variants that do *not*
  enter segmentation. The decisive, cheap predicate (per §OQ1): the field is on a dataset that
  is a lower-cover member *and* a case `ref`s a parent → leave it on `lower_member_variants`
  (VAR-EXPAND). Otherwise → per-row. Implement the "carries a ref to a parent" rollup as a
  single pass producing a `HashSet<(dataset, field)>`; consult it at expansion time.
- **Tests:** statistical χ² on a same-type variant's case distribution (already covered for the
  union path — mirror for the homogeneous path); a same-type variant on a *linked* dataset now
  produces a single batch (no `CombineVariantBatches` needed — see U6); inherited fields into a
  formerly-variant parent now wire (the `plan_variant_steps` v1 limitation disappears).

**Green-point:** same-type field variants generate per row; `plan_variant_steps` is now
unreached for them. Verify by temporarily `panic!`-ing in `plan_variant_steps` and running the
suite — nothing should hit it (member lowering doesn't call it).

</details>

## PR U6 — delete the dead machinery — **DONE (green)**

Deleted (probe-confirmed unreachable post-U5): `plan_variant_steps`, `expand_variant_dataset`,
`variant_key`, `distribute_rows`, and the `ExecutionStep::CombineVariantBatches` variant + its
executor arm + the `main.rs` debug arm. The `build_plan` branch for a non-pure-member with
`dataset.variants` now `bail!`s (the unsupported member+parent case-3 edge). **Kept** (case-3
needs them): `build_local_combinations`, `cross_product`-into-`dataset.variants`,
`VariantSchema`, `SyntheticDataset.variants`, `lower_member_variants`, `merge_variant_fields`.

<details><summary>Original U6 plan</summary>

With U5 in, the cross-product path has no callers.

- **`lib/plan.rs`** — delete `plan_variant_steps`, the `ExecutionStep::CombineVariantBatches`
  variant, `variant_key`/`expand_variant_dataset` and the `build_plan` branch
  (`plan.rs:940`).
- **`lib/executor.rs`** — delete the `CombineVariantBatches` arm (`executor.rs:249`) and the
  `dataset.variants` merge in member generation (`executor.rs:1795`).
- **`lib/expand_variants.rs`** — delete `build_local_combinations`, `cross_product_variants`,
  and the `dataset.variants` assignment; `expand_field_variants` now only runs
  `lower_heterogeneous_unions` + the same-type per-row marking from U5.
- **`lib/models.rs`** — delete `VariantSchema` and `SyntheticDataset.variants` (now truly
  *removed outright*, satisfying OQ3).
- **`src/docgen.rs`** — drop `VariantSchema` `TypeDoc` (already removed from user docs in U4).
- **Tests:** delete cross-product-specific unit tests in `expand_variants.rs`
  (`build_local_combinations`/`cross_product_variants` coverage); keep the same-type behavioural
  tests (now exercising the per-row path).

**Green-point:** full suite green with the entire top-level/cross-product apparatus gone.
A complexity audit (scripted line/cyclomatic/nesting proxy over `lib/*.rs`) should show `plan.rs`/`executor.rs` shrink.

> **Note (as-built):** the U6 plan above over-reached — `VariantSchema` /
> `SyntheticDataset.variants` / `build_local_combinations` are **kept** (case-3 needs them), and
> `executor.rs:1795` (member `dataset.variants` merge) is part of the kept case-3 path. OQ3's
> "removed outright" is therefore **not** achieved; it would require refactoring `lower_member_variants`
> to read case-3 fields directly (a future cleanup, not blocking).

</details>

## PR U7 — docs + close-out — **DONE**

- **`CLAUDE.md`** — module map (`expand_variants.rs`, `plan.rs`, `executor.rs` rows lose the
  cross-product/`plan_variant_steps`/`CombineVariantBatches` mentions); glossary (`flatten`
  entry; "top-level variant" marked retired → "whole-row variation = a flatten union field");
  feature table (VAR-UNIFY → Complete; VAR-1-OUTPUT-FLAG → Subsumed); execution-pipeline step
  list (drop `CombineVariantBatches`).
- **`docs/.../concepts/`** — `variant-lowering.mdx` + `execution-pipeline.mdx` updated; add the
  flat-vs-nested model and the per-row-categorical vs lowering split.
- **Memory** — update `[[var-unify-flatten]]` to Complete; note VAR-UNIFY-OPT as the remaining
  optional follow-up.

---

## Risk register

| Risk | Where | Mitigation |
|------|-------|------------|
| JSON writer emits explicit nulls (breaks per-row keys) | U2 | Verification test first; `with_explicit_nulls(false)` is the default |
| `write_output` lacks the schema to know which fields are `flatten` | U2 | Thread `&[Field]`/flatten-name set via `WriteSharedOutput.schema` (already carried) |
| Constraint-bearing fixtures change output shape on migration | U4 | They migrate to member flatten fields → `lower_member_variants`; assert invariants, update shape expectations only |
| Removing `plan_variant_steps` breaks linked-dataset-with-variants | U5/U6 | Per-row path yields a single batch, so `CombineVariantBatches` is unnecessary by construction; add a linked+variant fixture |
| Inherited-fields-into-variant-parent (v1 unsupported) silently stays broken | U5 | Per-row path produces one stable batch → wire + test inherited fields into a formerly-variant parent |
| Hidden second source of `dataset.variants` | U6 | Pre-delete, `panic!` in `plan_variant_steps` and run suite to prove no callers |

## Dependencies

| Spec | Relationship |
|------|--------------|
| VAR-1 (complete) | `unionize_for_output` (generalised by `flatten`), per-row sampling (reused by U5), `union_cases`/`FieldVariant.fields`/`name` |
| VAR-EXPAND (complete) | `lower_member_variants` / segmentation — the constraint-bearing path Phase 2 leaves untouched; OQ1 routes ref'ing variants here |
| VAR-1-OUTPUT-FLAG | **Subsumed** — U2/U3 are its output-encoding story |
| VAR-SPECIALIZE | **Scheduled after** — specialises the unified model U1–U7 leaves behind |
| VAR-UNIFY-OPT (future) | OQ1 partial-variant split + skip-lowering pure value/shape member variants (scale optimisation) |
