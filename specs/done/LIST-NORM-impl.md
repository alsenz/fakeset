# LIST-NORM — implementation plan

Implementation plan for `specs/LIST-NORM.md`. Read the spec first for the design, semantics,
UDF signatures, desugar, validation, and the **CTE-relocation constraint** (the
implementation must stay placement-agnostic so it does not block `specs/EXPR-RELOCATE.md`).

**Sequencing:** LIST-NORM is built **first** of the list-feature specs (LIST-NORM →
PROJECT-FIELD → EXPR-RELOCATE). It is independent of PROJECT-FIELD; they only compose.

Three small, independently-shippable, independently-testable PRs. Each is green on
`cargo test` before the next starts; PR3 adds the `pytest` end-to-end gate.

## PR1 — the UDFs (pure Arrow, no YAML surface)

The substance of the feature, usable immediately from raw `expression:` even before the
sugar exists.

- **Add `array_normalize` and `array_normalize_field`** (3-arg in-place + 4-arg `into`) as
  Arrow scalar UDFs. Each is an offset-window loop over the `ListArray` (and, for `_field`,
  the inner `StructArray` column).
- **Int-vs-float is a runtime decision inside the UDF**, read off the *output* Arrow type —
  `Float64` → `vᵢ/Σv·total`; integer → `segment::largest_remainder` (`lib/segment.rs:134`).
  In-place output type = the source sub-field's type. (See *Open decision* below for `into`.)
- **Register both on the `SessionContext`** in `evaluate_expressions` (`executor.rs:2435`) —
  registration only, *no* fixed pipeline position (keeps EXPR-RELOCATE unblocked).
- **Edge cases**: empty list → empty; `Σv == 0` → equal split (largest-remainder for int).
- **Tests** (unit, directly on the UDFs): float sum within tolerance; integer sum **exactly**
  `total`; empty; all-zero equal-split; `into` adds a sub-field while keeping the source;
  `array_cat`-then-normalise composition.

*Ships:* power users can already write the rollup+normalise recipe by hand.

## PR2 — the `normalize:` declarative sugar (desugar + validation + docs)

- **Models** (`models.rs`): add `normalize: Option<Normalize>` to `Field` where
  `struct Normalize { total: f64, field: Option<String>, into: Option<String> }`.
- **Desugar pass** (`expand_*` phase, before `resolve_refs`/`validate` so the injected
  expression is in scope for ordering): for each field carrying `normalize`, rename the
  list-producing field in place to a hidden `<name>__norm_src` (`hidden: true`) and inject an
  `expression:` field `<name>` immediately after, choosing the UDF arity from
  scalar-vs-struct (`field` present?) and in-place-vs-`into`. The rename-in-place +
  inject-after keeps `validate_expression_order` satisfied (src precedes the expression).
- **Validation** (`validate.rs`): the rules listed in the spec's *Validation* section —
  `total>0`; `field` required for struct elements / forbidden for scalar lists; `field`/`into`
  source must be a numeric sub-field; `into` must not collide; `normalize` on a non-list field
  errors.
- **Docs**: `FieldDoc` entry for `normalize` in `docgen.rs` + a row/example in
  `reference/yaml-schema.mdx`; mention the `array_normalize*` UDFs in `reference/generators`
  or an expressions reference.
- **Tests**: desugar produces the expected hidden src + expression field; the three shapes
  end-to-end on small schemas; each validation rule rejects.

## PR3 — UBO example + statistical hard-invariant

- Add (or extend) the UBO example using the headline `into:` form so `shareholders` carries
  `{name, shareholding, ownership_pc}`.
- **Statistical suite**: a hard-invariant check that every company's `ownership_pc` sums to
  exactly 100 (per CLAUDE.md invariant #7 — this is the layer that catches the integration
  bugs unit tests miss).

## Open decision (resolve in PR2)

The `into` case has no declared output field, so its precision is undefined by the current
"read off the target field's type" rule. Default: **`into` output inherits the source
field's precision**. If integer percentages from a *float* source amount are wanted (float
`shareholding` → integer `ownership_pc` summing to exactly 100), add an optional
`precision:` to the `normalize` block as a later, additive extension — out of scope for the
initial cut unless the UBO example needs it.
