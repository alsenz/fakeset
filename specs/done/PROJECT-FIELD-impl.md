# PROJECT-FIELD — implementation plan

**Status: COMPLETE (PR1–PR2).** Both PRs landed together. Implementation notes vs the plan
below:

- **No separate `is_dotted` free helper** — the classifier lives as two methods on `ListContent`
  (`models.rs`): `is_bare_project()` (project set & no `.`) and `project_col()` (the `<field>`
  part for dotted, the whole identifier for bare). `plan.rs` now calls `content.project_col()`;
  the assembly column-pick (`executor.rs`) was *already* keyed on `project_col`, so PR2's
  executor path needed **no change** — the existing branch projects the named column out of
  `inner` for both forms (the bare field is generated into the witness/junction like any content
  field, then all-but-one are discarded).
- **`schema.rs::field_to_arrow`** gained a `is_bare_project()` arm → `List<projected scalar
  type>` (resolved from `content.fields`).
- **`validate_normalize`** (`validate.rs`) needed a one-line fix the plan didn't foresee: a bare
  `project` collapses to a *scalar* list, so it must take the scalar normalise path even though
  `content.fields` is populated (`.filter(|c| !c.is_bare_project())`). This is what makes
  `project` + `normalize` (no `field:`) compose.
- **Test harness:** the executor-test `run()` helper was missing `desugar_normalize` (the real
  CLI pipeline has it between `expand_field_variants` and `expand_include_fields`); added so the
  compose test exercises the real path.

Original plan follows.

---

Implementation plan for `specs/PROJECT-FIELD.md`. Read the spec first for the design,
semantics, and validation rules.

**Sequencing:** Built **second** of the three list/expression specs — **LIST-NORM →
PROJECT-FIELD → EXPR-RELOCATE**. Independent of LIST-NORM (they only compose). This spec
delivers the **projection half** now; its motivating *derivation* case (`holding.weight * 2`)
is realised once EXPR-RELOCATE later lifts the content-expression gate (`validate.rs:1148`).

The change is small: `content.project` already exists as `Option<String>` on `ListContent`
(`models.rs:880`); no new field. The work is (a) reinterpreting `project` by syntactic shape
and (b) a post-assembly column-pick. Two small, independently-testable PRs.

## PR1 — validation: allow bare `project` alongside `fields`

- **Classify `project`** by shape: **dotted** `<link_ref>.<field>` (existing — project
  straight from the linked dataset) vs **bare** `<identifier>` (new — project a field defined
  in `content.fields`). A tiny helper (`is_dotted` / split on `.`) used by both validation and
  assembly.
- **Relax `validate_project`** (`validate.rs:1575`): the `project` + `fields` mutual-exclusion
  rule (`validate.rs:1593`) now applies **only to the dotted form**. The bare form *requires*
  `fields`.
- **New rules for the bare form**:
  - the identifier must name a field present in `content.fields`;
  - that field must be **scalar** (number/string/bool) — not a struct/list.
- **Dotted form** keeps today's rules unchanged (ref part matches the link; mutually exclusive
  with `fields`).
- **Tests**: bare `project` + `fields` accepted; bare naming a missing field → error; bare
  naming a non-scalar field → error; dotted `project` + `fields` still rejected (unchanged).

*Ships nothing user-visible yet* (assembly still builds the struct) — but the validation gate
is correct and the classifier is in place.

## PR2 — assembly: project the sub-field to a scalar list

- In **`AssembleFromWitness`** (`executor.rs:1092`), after the per-item struct content has
  been folded into the `List<Struct>` column, if `project` is **bare**: pick the named
  sub-field out of the inner `StructArray` (`StructArray::column_by_name`) and rebuild the
  column as a `List<scalar>` **reusing the existing list offset buffer** —
  `ListArray::new(elem_field, offsets, projected_values, nulls)`. Pure Arrow; no new
  generation path. (All `content.fields` are still generated, then one is projected out — the
  others are discarded; a generate-only-deps optimisation is possible later but out of scope.)
- **Schema** (`schema.rs` / `field_to_arrow`): the field's Arrow type becomes `List<scalar>`
  instead of `List<Struct>` when a bare `project` is present.
- **Tests**: bare `project` over a `ref`-sourced content field → scalar `List<T>` of the
  correct element type and lengths; composes with `LIST-NORM` (`array_normalize`, no `field:`)
  → per-list sum equals `total`.

## Out of scope (post-EXPR-RELOCATE)

The **derivation half** — a *computed* content field to project, e.g. `holding.weight * 2` —
needs `expression:` inside `content.fields`, currently rejected at validation
(`validate.rs:1148`). That gate is lifted by the EXPR-RELOCATE content-expression unlock
(witness-step expressions); once it lands, the bare-`project` machinery here projects a
witness-computed field with no further change.
