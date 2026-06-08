# LIST-NORM — normalise a numeric field within a list to a target sum

**Status:** **COMPLETE** — implemented and merged. All three PRs shipped (UDFs → `normalize:`
sugar → UBO example + statistical invariant); `cargo test` + `pytest` green.

## Implementation finding

Built as designed, with two notes the design didn't fully pin down:

- **`create_udf` is insufficient** — its return type is fixed, but ours depends on the input
  list's element type, the arity (in-place vs `into`), *and* `precision`. The UDFs are hand-rolled
  `ScalarUDFImpl`s (`lib/list_norm.rs`) using `return_field_from_args` (DataFusion 53.1) to read
  the literal `'src'`/`'dst'`/precision scalar args at plan time and compute the exact output
  `Field`. `Signature::variadic_any` carries the variable arity; the impl does its own arg
  type-classification (Utf8 = field name, integer = precision), which is what makes the optional
  `dst`/`precision` trailing args unambiguous.
- **The `precision` open decision was resolved by *adding* `normalize.precision`** (not just
  inheriting the source type). A `type: number` field is always Arrow `Float64` (precision is a
  post-gen rounding, the type stays float), so without an explicit `precision: 0` the headline
  "exactly 100" UBO percentages would have been floats. `precision: 0` forces the integer
  largest-remainder path; `> 0`/absent inherits the source type. The whole pipeline placement is
  unchanged: `normalize:` desugars (in `desugar_normalize`, after `validate`) to a hidden
  `<name>__norm_src` + an injected `array_normalize*` expression, so it rides the existing
  `evaluate_expressions` CTE chain and stays placement-agnostic for EXPR-RELOCATE.

---

**Original design (Future — designed, not built). Well-contained; reuses an existing primitive.**

**Sequencing:** First of the three list/expression specs — **LIST-NORM → PROJECT-FIELD →
EXPR-RELOCATE**. Independent of PROJECT-FIELD (they only compose). Implementation plan:
`specs/LIST-NORM-impl.md`.

## Problem

A list field can carry a numeric quantity per item — ownership stakes, portfolio weights,
budget line-items, vote shares — that should **sum to a known total within each list**
(100, 1.0, a budget). fakeset has no mechanism for this: a generated numeric content field
draws each value independently, so per-list sums are arbitrary. The motivating case is UBO
shareholdings: a company's shareholder stakes should sum to 100%.

There is no within-list normalisation primitive today. Expressions are per-row and the
generation path has no notion of "rescale these sibling list items so they sum to N".

## Proposal

A **general list operation**, `normalize:`, attachable to any field that produces a list —
a `type: list` field or an `expression` field that returns a list. It rescales a numeric
target so each list sums to `total`. Underneath it desugars to a registered Arrow scalar
**UDF**; the declarative `normalize:` key is sugar over that UDF.

### Declarative surface

```yaml
normalize:
  total: 100        # required — the per-list sum target (int or float)
  field: stake      # optional — numeric sub-field for List<Struct>; omit for List<number>
  into: ownership_pc # optional — write result to a NEW sub-field; absent = overwrite `field`
```

Three shapes, one operation:

**1. List of structs, derived alongside the raw (the recommended/headline form):**

```yaml
- name: shareholders
  type: list
  normalize: { field: shareholding, into: ownership_pc, total: 100 }
  content:
    from: subsidiary
    fields:
      - { name: name, ref: subsidiary.company_name }
      - { name: shareholding, type: number, range: { min: 1000, max: 5_000_000 } }
```

Each item becomes `{name, shareholding, ownership_pc}` — raw absolute holding *and* its
within-list percentage.

**2. List of structs, in place (no `into:`):** rescales `field` itself.

```yaml
- name: shareholders
  type: list
  normalize: { field: stake, total: 100 }
  content: { from: subsidiary, fields: [ {name: name, ...}, {name: stake, type: number, precision: 0} ] }
```

**3. List of bare numbers (no `field:`):**

```yaml
- name: portfolio_weights
  type: list
  normalize: { total: 1.0 }
  content: { from: holding, project: holding.weight }   # scalar list — see PROJECT-FIELD
```

The presence/absence of `field:` falls out of the list shape (struct vs scalar), mapping
onto the two UDF arities below — the user never restates it.

## Semantics

Per outer row, per list window:

- **float target** (target numeric has `precision > 0`): `vᵢ ← vᵢ / Σv · total`.
- **integer target** (`precision: 0`): compute float weights, then
  `segment::largest_remainder(weights, total)` (`lib/segment.rs:134`) → integers summing to
  **exactly** `total`. This reuses the codebase's single rounding primitive (Hamilton /
  largest-remainder: unbiased per cell *and* total-conserving — CLAUDE.md invariant #5);
  do **not** hand-roll `round()`.
- **empty list** → empty (no-op).
- **Σv == 0** → equal split (`total / n` per item, largest-remainder for the integer case).

The integer-vs-float choice is read off the **target field's declared type/precision**, so
it is never specified in YAML.

## The UDFs (the real primitive)

Registered as scalar UDFs in the `evaluate_expressions` `SessionContext`
(`executor.rs:2435`), so they are also usable directly in any `expression:`:

```
array_normalize(list_of_number, total)                 -- List<number>  → rescaled List<number>
array_normalize_field(list_of_struct, 'src', total)    -- overwrite sub-field `src`
array_normalize_field(list_of_struct, 'src', total, 'dst') -- append/write sub-field `dst`
```

Each is a ~30-line closure over the `ListArray` offsets (and, for the `_field` variant, the
inner `StructArray` columns). Pure Arrow; no DataFusion query construction.

## Desugar

The declarative `normalize:` is rewritten at expand-time into the UDF call, uniform across
all three shapes:

1. Rename the list-producing field to a hidden `<name>__norm_src` (`hidden: true`).
2. Inject an `expression:` field `<name>` calling the matching UDF arity:
   - scalar list → `array_normalize(<name>__norm_src, total)`
   - struct, in place → `array_normalize_field(<name>__norm_src, 'field', total)`
   - struct, into → `array_normalize_field(<name>__norm_src, 'field', total, 'into')`

Same "lower to existing primitives" tactic as variant lowering — no new executor stage.

## Pipeline placement

Normalisation runs in the **expression CTE chain** (`evaluate_expressions`,
`executor.rs:2419`), which for list-link datasets is evaluated **after**
`AssembleFromWitness` has folded the lists into `ListArray` columns (`executor.rs:1092` →
fold, `:1233` → evaluate). Because the chain is YAML-ordered, a list *rollup* and its
normalisation compose as two steps — e.g. concat two homogeneous shareholder lists then
normalise the combined stake:

```yaml
- { name: shareholders_raw, expression: "array_cat(individual_shareholders, corporate_shareholders)", hidden: true }
- { name: shareholders,     expression: "array_normalize_field(shareholders_raw, 'stake', 100)" }
```

(`array_cat` on `List<Struct>` is confirmed working in the pinned DataFusion 53.1 — spiked.)

> **⚠️ The CTE chain is being relocated — do not block it.** `specs/EXPR-RELOCATE.md`
> moves `evaluate_expressions` out of terminal emit into **dependency-placed**
> materialisation. LIST-NORM must stay **placement-agnostic**: it makes this true *by
> construction* because it **desugars to ordinary `expression:` fields** and adds **no
> bespoke post-assembly hook**. Hard rules for the implementer:
> - **Do not** implement normalisation as a hardcoded step tied to the current emit call
>   site (`executor.rs:1233`) or anywhere that assumes "expressions run last".
> - **Do** keep it purely as *desugar → expression field* + *registered UDF*. The injected
>   expression depends on assembled `ListArray` columns, so EXPR-RELOCATE's placement rule
>   will naturally route it to the **assembly** point — no LIST-NORM change required when the
>   relocation lands. (Register the UDFs once on the `SessionContext`, not at a fixed
>   pipeline position.)

## Validation

- `normalize.total` required and > 0.
- `normalize.field` required when the list element is a struct; forbidden for a scalar list.
- `field` (and `into`'s source) must name a **numeric** sub-field of the element struct.
- `into`, when set, must not collide with an existing sub-field unless overwrite is intended
  (decision: reject collision; `into` is for *new* fields, omit it to overwrite `field`).
- Attaching `normalize:` to a non-list field is an error.

## Interactions

- **PROJECT-FIELD** — `project` yields a `List<number>`; `normalize` (no `field:`) then
  rescales it. The scalar-list path.
- **UBO example** — the headline consumer: per-company stakes summing to exactly 100, with
  raw holding retained via `into:`.
- **Generality** — budgets, portfolio weights, vote shares, survey percentages, Dirichlet-
  style splits. Not a UBO-specific feature.
- **Shape vs sum** — normalisation fixes the *sum*, not the *distribution shape*. Uniform
  raw draws rescale to roughly-equal splits; realistic ownership concentration (one
  majority holder) needs a skewed numeric draw, a separate generator-distribution concern.
- **Linked content lists — per-edge, post-assembly, and *not* ref-pinnable.** Normalising a
  linked content list is written as an **outer field below the list stanza**, so the desugared
  UDF/CTE necessarily runs **post-assembly** — self-documenting, since it can only reference
  the assembled `ListArray`. A normalised content field is **per-edge** (its denominator is its
  own list), so it **cannot be `ref`-pinned to the linked dataset**: that target is per-row,
  the direction is wrong (ref pulls *from* linked; the value is authored *after* the draw), and
  it would be a second value-source. Resolutions: **(a)** `into:` keeps the linked-drawn raw
  alongside the derived %; **(b)** the genuinely *pinned* case is modelled by making the
  relationship a first-class **edge dataset** and group-normalising there (see **REL** —
  list-window normalisation and group-by-key normalisation are the same UDF; the nested list
  and the flat edge table are duals). A per-edge value *can* be carried to the linked side via
  **`collect`** into a `[]float` list — `collect` is accumulate-*up* (the right direction) and
  the list type absorbs the many-edges-per-linked-row multiplicity; it is the **linked-side
  dual of the content list**.

## Test plan

- Float target: each list sums to `total` within fp tolerance.
- Integer target: each list sums to **exactly** `total` (largest-remainder).
- `into:` retains the raw field and adds the normalised one; in-place overwrites.
- Scalar list (`array_normalize`) and struct list (`array_normalize_field`) both covered.
- Empty list → empty; all-zero list → equal split.
- Statistical suite: add a UBO check that per-company stake sums == 100 (hard invariant).
- Compose with `array_cat` rollup → combined list normalises correctly.

## Implementation plan

See **`specs/LIST-NORM-impl.md`** — three small, independently-testable PRs (UDFs → `normalize:`
sugar → UBO example + statistical invariant), plus the `into`-precision open decision.
