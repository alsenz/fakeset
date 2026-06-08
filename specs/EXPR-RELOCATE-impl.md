# EXPR-RELOCATE — implementation plan

Implementation plan for `specs/EXPR-RELOCATE.md`. Read the spec first for the design — the
placement rule, the materialisation-point tables (include **and** link), the merge algebra,
and the `ref`+`expression` reframing.

**Sequencing:** Built **third** (last) of the list/expression specs — **LIST-NORM →
PROJECT-FIELD → EXPR-RELOCATE**. The first two are **done** (PROJECT-FIELD shipped its
projection half). This is the largest; it lifts the content-expression gate
(`validate.rs:1148`) that LIST-NORM rollups and PROJECT-FIELD's derivation half still wait on.

**Discipline (strangler, every step independently testable).** Keep the old behaviour; add the
new path; prove equivalence on `cargo test` **and** the `pytest` statistical suite (CLAUDE.md
invariant #7 — the integration bugs that matter surface only end-to-end); then advance. Each PR
below is a stop point: if its gate fails, the system is still coherent and shippable.

## PR1 — placement scheduler (pure relocation, no new capability)

Relocate `evaluate_expressions` from one terminal pass to **dependency-placed** per-point
invocations. **Value-preserving** — this PR adds *no* new capability and must not change any
output.

- **Classify each expression by its dependencies** → assign a materialisation point:
  include pipeline `staging-scalar → shared-atom → member-non-ref → assembly`; link pipeline
  `staging → staging-shared-atom → witness → assembly → collect` (per the spec tables). Reuse
  `validate_expression_order` (deps already point upward) — placement is the *latest* point
  among an expression's dependencies.
- **Split the call sites**: invoke `evaluate_expressions` (filtered to the expressions placed
  there) at each point instead of once at emit (`executor.rs:411/557/594/1233`).
- For this PR, list/assembled-dependent expressions stay at **assembly** (= today); only
  scalar-only expressions move earlier (staging). Nothing is forced earlier than its deps.
- **Tests**: existing suite + statistical suite green unchanged (value-preservation is the
  whole acceptance bar); plus a *staged-expr-as-outer-scoped-ref* test (an expression computed
  at staging is consumed by a `content` field `ref` — newly possible, but additive).

*Gate:* byte-identical example outputs (modulo shuffle) vs `main`.

## PR2 — computed atom column-source + type inference + lift the ban

The headline capability: an expression authors a **ref'd/shared** column at the atom.

- **Computed column source** in `generate_segment_atom_batch`: extend the source priority
  `import → precomputed → fresh` with a fourth, **computed**, evaluated with inputs limited to
  shared columns already materialised at that point, then projected up unchanged (covers the
  include atom and the staging lower-cover-group atom).
- **Static type inference** via DataFusion: build a `DFSchema` from the known input column
  types and read the expression's output `DataType` at plan time (`ExprSchemable::get_type` /
  the logical-plan output schema — spiked, works with no execution). Used to type the computed
  column and to drive type-merges.
- **Lift `validate.rs:887`** (`ref` + `expression` ban). Enforce instead **one value-source
  per shared column**: a share-set may carry at most one of `generator` / `value` /
  `expression` (`ref` is wiring, not a value-source). This yields the one-field "pin a ref'd
  field with an expr" form.
- **Constraint participation limited to type merges** this PR: type-mismatch prunes a segment;
  a computed shared column that would need a *value/range* merge is **rejected at validation**
  (deferred to PR3).
- **Tests**: pin-a-ref'd-field — parent + every member see *identical* computed values;
  type-mismatch → segment pruned; one-value-source violation → error; import-taint still
  blocks laundering an imported column through a computed shared column.

*Gate:* the project-up equivalence test + statistical suite green.

## PR3 — bound-merge algebra

- Wire DataFusion interval analysis (`physical_expr::analyze` / `cp_solver` /
  `PhysicalExpr::evaluate_bounds`) into `constraints.rs` merge for computed columns.
- Allow range/value merges **when bounds are derivable**; otherwise emit the validation error
  ("expression output bounds not statically determinable; cannot merge with range
  constraint") — never silently prune or silently allow.
- Const-fold literal-only expressions to a scalar (`ExprSimplifier`) → type *and* value known.
- **Tests**: derivable bounds intersect correctly; un-derivable bounds vs a range → error;
  const-fold yields a constant column.

*Gate:* no over-/under-pruning regressions; bound tests + statistical suite green.

## PR4 — content-expressions + edge-granular collect (the link unlock)

Lifts the gate LIST-NORM and PROJECT-FIELD wait on.

- **Lift `validate.rs:1148`** (expressions inside list-link `content.fields`). Place them per
  the link table: *linked-scoped-only* → **witness** (per-unique-linked-row); *outer-scoped-
  dependent* (e.g. `linked.raw_weight * outer.multiplier`) → **assembly** (per-edge, where the
  junction has outer-scoped refs already `take`-replicated, `executor.rs:~1150`).
- **Edge-granular `collect`** of such an expression: no new accumulation engine —
  `AccumulateToLinked` already expands the witness via `_staging_refs` to a per-edge junction,
  replicates content per draw, groups by `_linked_idx`, and `array_agg`s
  (`executor.rs:1271`+). The work is to make `collect` consume the **assembled junction that
  has the content-expression evaluated** (the assembly path already builds such a junction);
  the `[]float` list absorbs the per-edge multiplicity.
- **Tests**: per-edge content-expression value; `collect` into `[]float` matches hand-computed
  per-edge values; the worked-example schema (per-edge vs per-row) end-to-end; statistical.
- **Docs deliverable**: extend the per-edge worked example in
  `docs/concepts/list-links.mdx` (shipped-collect version already added) with the
  content-expression case; update `reference/yaml-schema.mdx` for content `expression:`.

*Gate:* statistical suite green; the per-edge worked example produces correct sums both
outer-side (list) and linked-side (`collect`).

## Spikes / risks

- **Static type inference** (`get_type`) — ✅ already spiked (DataFusion 53.1).
- **Interval analysis** (`evaluate_bounds`/`cp_solver`) — spike the API shape in 53.1 before
  PR3 (more internal than `get_type`; version-sensitive).
- **Error attribution** — DataFusion planning/coercion errors must map back to "field X, line
  Y"; add a translation layer rather than surfacing raw DataFusion errors.
- **DataFusion-internal coupling** — the analysis APIs are less stable than the query API;
  isolate their use behind a thin module so upgrades touch one place.

## Out of scope (per spec)

`range`-on-column (separate generator-engine track); multi-level project-up beyond the direct
parent; the big DataFusion-everything rewrite (`specs/EXPR-MOONSHOT.md`).
