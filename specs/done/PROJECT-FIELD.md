# PROJECT-FIELD — project a *computed* content field to a scalar list

**Status:** **Complete (PR1–PR2)** — implemented and merged. The **projection half** is
shipped: bare `project` collapses a per-item struct (built from `content.fields`) to a scalar
list, composing with `fields:` and `normalize:`. The *derivation* half (`holding.weight * 2`)
still waits on EXPR-RELOCATE lifting the content-expression gate (`validate.rs`).

**Sequencing:** Second of the three list/expression specs — **LIST-NORM → PROJECT-FIELD →
EXPR-RELOCATE**. Independent of `LIST-NORM` (no dependency either way; they only *compose* —
`project` → `List<number>` → `normalize` with no `field:`). Built **before** EXPR-RELOCATE,
delivering the **projection half** now; its motivating *derivation* case (`holding.weight * 2`)
is realised once EXPR-RELOCATE *subsequently* lifts the content-expression gate
(`validate.rs:1148`). Implementation plan: `specs/PROJECT-FIELD-impl.md`.

## Problem

`content.project: "<link_ref>.<field>"` produces a scalar list (`List<number>` etc.)
but can only project a field **raw from the linked dataset**, and is **mutually
exclusive** with `content.fields` (`validate.rs:1577`). So there is no way to derive a
value per item (e.g. `holding.weight * 2`, a unit conversion, a ref + transform) and then
emit the list as bare scalars — you are forced to emit a `List<Struct{…}>` with a single
sub-field, the exact clunky shape `project` was introduced to avoid.

## Proposal

Allow `content.fields` **and** `content.project` together, and overload `project` on its
syntactic shape:

| `project:` value | Meaning | Status |
|---|---|---|
| `<link_ref>.<field>` (dotted) | project a field straight from the linked dataset | existing |
| `<identifier>` (bare, no dot) | project a field **defined in `content.fields`** | **new** |

```yaml
- name: scaled_weights
  type: list
  content:
    from: holding
    fields:
      - name: doubled
        ref: holding.weight        # (or any future computed/derived content field)
        # …a per-item derivation lands here…
    project: doubled               # collapse the struct → List<number> of `doubled`
```

The list element type becomes the projected field's scalar type instead of a struct.

## Semantics

Assemble the per-item struct from `content.fields` exactly as today, then **project the
single named sub-field out of the `StructArray`**, reusing the list's existing offset
buffer — a pure-Arrow `StructArray.column_by_name(p)` + `ListArray::new(elem_field,
offsets, projected_values, nulls)`. No new generation path; it is a post-assembly column
pick, the scalar-list twin of the existing linked-field projection.

## Validation

- Bare `project` identifier **must** name a field present in `content.fields`.
- The projected field must be **scalar** (number/string/bool) — not itself a struct/list.
- Dotted `project` keeps today's rule (must match the link ref; mutually exclusive with
  `fields`). Only the **bare** form composes with `fields`.

## Interactions / limitations

- **Most powerful once content fields can carry derivations.** Today content fields are
  `ref`/`value`/`generator`/`type` only — `expression` is banned inside list-link content
  (`validate.rs:1148`). So the headline `holding.weight * 2` case needs either
  expressions-in-content or a derivation primitive to be unlocked separately; **this spec
  delivers the projection half**, which is independently useful (project a generated or
  ref'd field as a scalar) and is the precondition for the arithmetic case.
- Composes with **LIST-NORM**: `project` first yields a `List<number>`, then
  `normalize: { total }` (no `field:`) rescales it — the scalar-list normalise path.

## Test plan

- Bare `project` over a `ref`-sourced content field → scalar `List<T>` of correct type/len.
- `project` naming a missing / non-scalar field → validation error.
- Dotted `project` + `fields` together → still rejected (unchanged).
- `project` + `normalize` (no `field:`) → per-list sum equals `total`.
