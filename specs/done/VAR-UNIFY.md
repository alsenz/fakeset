# VAR-UNIFY — `flatten`, and bringing top-level variants in line with field variants

## Status

**Complete (U1–U6; as-built record in [`VAR-UNIFY-impl.md`](VAR-UNIFY-impl.md)).** Builds on —
and *leans harder on* — [`VAR-1`](VAR-1.md) and [`VAR-EXPAND`](VAR-EXPAND.md), both **complete**.
Nothing about field-level variants changes conceptually: same tagged-union model, same
lowering, same `DenseUnion` internal representation, same per-row sampling, same VAR-EXPAND
segmentation. This spec adds one new primitive — **`flatten`** — and uses it to retire
**top-level dataset `variants:`** as a separate feature, expressing whole-row variation as
an ordinary `flatten`ed union field. It also **subsumes** `VAR-1-OUTPUT-FLAG` (the deferred
output-encoding work *is* `flatten`'s output story).

## The idea in one line

A field-level union is a tagged union over *one column*; a top-level dataset variant is a
tagged union over *the whole row*. They differ only in **output nesting** — and nesting is
a write-time concern. So add a serde-style **`flatten`** attribute that elides a field's
nesting at output, and a top-level variant becomes "a `flatten`ed union field at the top of
`data`." One mechanism, two nesting levels.

## Scope (read this first)

- **Field variants: unchanged.** `type: variant` on a field keeps working exactly as it
  does today (same-type → existing path; heterogeneous → `DenseUnion`, VAR-1). This spec
  does **not** rework them.
- **Top-level dataset `variants:`: retired as a user feature.** Whole-row variation is
  written as a `flatten`ed union field instead. (Pre-alpha, so removing the surface syntax
  is acceptable; §Migration.)
- **New: `flatten`** on `object` and `variant`(union) fields — a pure output-write-time
  transform.
- **Out of scope (a noted *Phase 2*):** retiring the *internal* cross-product machinery
  (`build_local_combinations` / `cross_product_variants` / `plan_variant_steps` /
  `CombineVariantBatches` / `VariantSchema`). Field-variant expansion still uses it, so it
  stays in Phase 1; §Phase 2 sketches how it could later be re-pointed for the deeper
  complexity cut.

## The `flatten` primitive

`flatten: true` on a `Field` of type `object` or `variant` is a **write-time** instruction:
elide this field's nesting and pull its sub-columns up into the parent level. It is the
direct analogue of serde's `#[serde(flatten)]`.

- **On an `object` field** — pull the struct's fields up into the parent (one-level elision).
- **On a `variant` (union) field** — `flatten` distributes as `flatten: true` on every
  **object case**: the variant's own name level is elided *and* each object case is flattened
  in turn, so the **active case's fields** land directly at the parent (the others null in
  the Parquet superset). Per row the variant *is* its active case, so this is just "a
  flattened object of the active case's type." For an object case it removes two name levels
  (variant name + case name), but as two composed *one-level* flattens — there is no
  recursive/deep flatten beyond cases (see §Open questions Q4). This is a slightly deeper
  generalisation of VAR-1's `unionize_for_output`, which builds `{case_name: {case_fields}}`
  (case-name wrapper kept); `flatten` elides that wrapper too, emitting the case fields
  **flat** at the parent. The case identity survives as a *tag*, never a wrapper.

**Crucially, `flatten` is output-only — the internal model is untouched.** The field keeps
its **name** (see §Names and refs); generation produces the normal nested batch; refs
resolve against the nested structure. Only the writer sees `flatten`. This is why the
encoding worries (Parquet superset hacks, etc.) collapse to a *deserialisation* concern at
the very end, not something the planner/executor reason about.

## Output encoding becomes a deserialisation concern

Because `flatten` runs only at write time, each format does the natural thing:

- **JSON / JSONL** — emit per-row keys: a flattened row is `{…common…, <active case's
  fields>}`, with only the active case's keys present. No superset, no nulls. The loose
  typing of JSON makes this the *obvious* encoding.
- **Parquet** — a flat schema can't vary per row, so pull-up produces the **nullable
  superset** (union of all case fields; one case populated per row). Collisions and width
  are handled by a configurable strategy in the `parquet` block, e.g.:
  - `superset` (default) — case fields side by side; populated set = the case tag.
  - `prefixed` — prefix pulled-up fields by case name to avoid collisions.
  - `type_suffixed` / `discriminant` — add a materialised case-tag column.
- **CSV** — flat by nature; works once the union is reduced to scalar superset columns
  (object cases still can't go to CSV — same nesting limit as object fields).

This is the same matrix `VAR-1-OUTPUT-FLAG` described; it is folded in here as the set of
`flatten` output strategies.

## Names and refs (a hard rule)

`flatten` changes *output shape only*, never identity. Therefore:

1. **A `flatten` field must have a name** — it is the addressable identity used by ref
   resolution, which runs **before** output writing. (Distinct from union *case* names,
   which VAR-1 allows to be positional; the *field* name is mandatory.)
2. **Refs are unaffected by `flatten`.** `ref: claims.claim_detail.theft.police_reference`
   resolves against the nested model regardless of whether `claim_detail` is flattened on
   the way out.
3. **A ref *into a union case* targets a conditionally-present field.** Because the case
   only fires for some rows, such a ref reads `null` on the others. The spec should state
   this explicitly; validation should at least warn ("reffing a sometimes-null union-case
   field").

## How top-level variation is written after this

There is no top-level `variants:`. A dataset that was N row-shapes becomes a `flatten`ed
union field, with common fields as ordinary siblings:

```yaml
name: claims
format: jsonl
data:
  - name: claim_id
    type: string
    generator: uuid
  - name: claim_detail        # flatten union — its case fields pull up to row level
    type: variant
    flatten: true
    variants:
      - name: property_damage
        type: object
        ratio: 0.3
        fields: [ … ]
      - name: theft
        type: object
        ratio: 0.2
        fields: [ … ]
```

This is *more* composable than top-level `variants:`: common fields are normal siblings (no
parallel `data:` vs `variants:` structure), and you can have several flatten fields, or nest
them. It reuses the field-variant pipeline wholesale.

## Why field variants don't change

Field-level variants already do the right thing and keep doing it:

- **Heterogeneous** field variant → `DenseUnion` (VAR-1). `flatten` simply gives it the
  option to emit flat instead of nested.
- **Same-type** field variant → its existing expansion path, untouched in Phase 1.
- **Constraint-bearing** variants (cases that pin ref fields and must enter lower-cover
  conflict pruning for referential integrity) → VAR-EXPAND's `lower_member_variants`,
  untouched. **`flatten` is an output concern and does not touch segmentation** — keep this
  distinction sharp: it unifies the *data-shape/output* story, not the *generation/
  consistency* story.

## What is removed (Phase 1) vs kept

**Removed:** the user-facing top-level `variants:` key on `SyntheticDataset` (parsing,
docs, examples), and its dedicated planning where it served *only* the user feature.

**Kept (Phase 1):** the internal cross-product machinery, because same-type field-variant
expansion still targets it. `flatten` + the union path cover the *output* unification; the
internal dedup is deferred.

## Phase 2 (optional, later) — the deeper dedup

Once Phase 1 lands, same-type field variants could be re-pointed off the cross-product:
a same-type variant is a **per-row categorical column** (sample a case per row, generate via
its spec) — which is exactly `build_union_column` specialised to homogeneous cases (emit the
shared type directly instead of a union). Doing so would retire `build_local_combinations`,
`cross_product_variants`, `plan_variant_steps`, `CombineVariantBatches`, and `VariantSchema`
— a real complexity cut. The catch is the **constraint-bearing** case: variants whose cases
enter lower-cover segmentation must stay on VAR-EXPAND's path, so Phase 2 must precisely
partition "pure value/shape variant → per-row" vs "constraint-bearing variant →
segmentation." That partition is the hard design question and the reason it is a separate
phase, not Phase 1.

## Validation

- `flatten` is valid only on `object` and `variant` fields (error otherwise).
- A `flatten` field must have a non-empty `name`.
- **Name-collision check:** pulling up fields must not collide with sibling field names (or
  with each other across union cases) unless a non-colliding `parquet` strategy
  (`prefixed`/…) is selected. For JSON this is per-row so only same-case collisions matter;
  for Parquet the superset makes cross-case collisions matter.
- Reffing into a union case → warn (conditionally-null target).
- CSV + a `flatten`ed *object-case* union → the existing nested-in-CSV error.

## Files (preliminary)

| File | Change |
|------|--------|
| `lib/models.rs` | `flatten: bool` on `Field` (serde default false); retire `SyntheticDataset.variants` *as a user input* (Phase 1 may keep the field as an internal artifact) |
| `lib/validate.rs` | `flatten` placement + name + collision checks; ref-into-case warning |
| `lib/executor.rs` | Generalise `unionize_for_output` → `flatten`-aware: pull-up vs nest, per output format; the `parquet` strategy switch |
| `lib/schema.rs` | Output schema for a flattened field (superset columns) — write-path only |
| `src/docgen.rs` / `docs/.../reference/yaml-schema.mdx` | Document `flatten` + the `parquet` strategies; remove top-level `variants:` |
| `docs/.../concepts/variant-lowering.mdx` | Add the flat-vs-nested model; "top-level variation = a flatten union field" |
| examples / fixtures | Convert any top-level `variants:` usage to a `flatten` field |

## Open questions / decisions

 1. **Phase 2 partition — resolved in principle (ref-driven).** The rule for "pure
   value/shape variant (→ per-row / categorical)" vs "constraint-bearing variant
   (→ segmentation)". Load-bearing for the deeper dedup; **not needed for Phase 1.**

   It *is* a whole-graph property (example C from the design chat: the same variant
   definition is pure or constraint-bearing depending on its lattice context) — but the
   simplification below makes that property **cheap to compute and explicit to declare**:

   - **Specialisation requires a `ref` to the parent.** A case that carries
     `ref: parent.field` (+ a tighter `value`/`one_of`/generator) is *constraining* a shared
     column → its membership must be reconciled against the parent and any sibling that also
     refs it → **segmentation**. A case that declares the field with **no ref** is
     *replacing* it — an independent column for that subpopulation → **per-row, generated
     fresh**. So the predicate is local-per-case: *does this case ref a parent field?*
   - **Detection = one-pass graph rollup.** Walk the lattice once, mark every field that
     carries a ref to a parent into a `HashMap`; a variant is constraint-bearing iff any case
     hits it. Requiring each constraining lower-cover member to carry its own ref is what
     makes the whole-graph property a cheap, explicit lookup.
   - **Conflict merging shrinks to "≥2 members ref the *same* parent field."** If one member
     refs and another replaces, generate both independently (pin the ref'd one to the parent,
     generate the replacing one fresh) — no merge. Only mutual refs need pruning / IPF.
   - **Trade-off (accepted):** replacement breaks row-subset identity for that field — the
     child's emitted column may differ from the parent's for what is logically the same row.
     This is consistent with fakeset's existing model: cross-dataset identity is **opt-in via
     `ref`**; absent a ref, nothing is promised. (Same principle the inherited-field wiring
     already runs on.)

   Still genuinely open: the **partial** variant (some cases ref, some replace) — split the
   ref'ing cases onto segmentation and sample the rest per-row within the leftover mass, vs
   route the whole variant to segmentation. Lean: **split**, since the predicate is now
   per-case.
2. **Default Parquet strategy — resolved: `superset`.** Case fields side by side (tag =
   the populated set); discriminant column opt-in. **Note:** Parquet has no native union type
   today (ARROW-8817 — the writer `unimplemented!`s), which is *why* `flatten`/superset is the
   Parquet story. If/when Parquet gains native `DenseUnion` write support, that should become
   a selectable strategy for **non-flattened** union fields (write the union as-is rather than
   the superset struct) — revisit then.
3. **Keep `SyntheticDataset.variants` internally? — resolved: remove outright.** The
   cross-product machinery Phase 1 retains is driven by *field*-variant expansion (on
   `Field`), **not** by `SyntheticDataset.variants` — so nothing internal needs the top-level
   field. Pre-alpha means no migration burden: remove the key rather than aliasing it to a
   flatten field. (A parse-time alias can be added later if a friendlier migration is ever
   wanted — but that's not a reason to keep the field now.)
4. **`flatten` depth — resolved: one level, distributed to cases.** `flatten` elides exactly
   **one** nesting level (one name) per application:
   - **`object` field** — pull its `fields` up one level. Flattens once.
   - **`variant` field** — `flatten` means *"`flatten: true` defaulted on every object
     case."* Per row the variant *is* its active case, so a flattened variant behaves like a
     flattened object of the active case's type: the variant's own name level is elided and
     each object case is itself flattened, landing the **case's fields** at the parent. For an
     object case this removes two name levels (variant + case), but each is an ordinary
     one-level flatten *composed* — there is **no recursive/deep flatten** beyond cases. Case
     identity becomes a *tag* (recoverable via the discriminant Parquet strategy, or implicit
     from which keys are present in JSON), never a wrapper.
   - **Scalar cases** under a flattened variant have no inner object to flatten, so each
     contributes a single (case-named, to avoid collisions) column. Degenerate path —
     whole-row variation is almost always over object cases.

   No "partial flatten" knob: flattening a variant always distributes to its object cases. A
   user who wants case-name wrappers simply doesn't flatten (the default VAR-1 nested
   nullable-superset keeps them).

## Dependencies

| Spec | Relationship |
|------|--------------|
| VAR-1 (complete) | Provides `FieldType::Union`, `union_cases`, `unionize_for_output` (which `flatten` generalises), per-row sampling, `FieldVariant.fields`/`name` |
| VAR-EXPAND (complete) | Provides member-variant lowering / segmentation — the *constraint-bearing* path `flatten` deliberately does not touch |
| VAR-1-OUTPUT-FLAG | **Subsumed** — the deferred output-encoding strategies are `flatten`'s output story |
| VAR-SPECIALIZE | **Scheduled after VAR-UNIFY.** Its generator/`one_of` spectrum is orthogonal to `flatten`, but sequencing VAR-UNIFY first lets VAR-SPECIALIZE specialise the *unified* variant model rather than the two-path version |
