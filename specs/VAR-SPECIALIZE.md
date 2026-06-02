# VAR-SPECIALIZE — Child specialisation of parent variant and generator fields

## Status

**Proposed — design exploration.** Depends on [`VAR-EXPAND`](VAR-EXPAND.md);
this spec assumes variants are already **lowered** into the lattice per
VAR-EXPAND — each `type: variant` field is a **tagged union** lowered into
**cases** that are ordinary lower-cover members, with a **discriminant**
sentinel (`_disc_<union>`) enforcing one case per row.

## What this is

Two related forms of specialisation that today are either rejected by the
validator or silently approximated by VAR-2:

1. **Variant-subset specialisation.** A child restricts a parent's tagged-union
   (`type: variant`) field to a subset of its cases.
   *Example:* `animals.yaml` declares `eats: type: variant [birds, mice, grass, fish]`.
   `cats.yaml` specialises `eats` to `[birds, mice]` — cats never eat grass or fish.
   Under VAR-EXPAND this is exactly an `allowed_values` constraint on the union's
   discriminant `_disc_eats` (see §Why it depends on VAR-EXPAND).

2. **Generator-domain specialisation.** A child constrains a parent's
   open-domain `generator:` field to a specific constant or to a finite
   `allowed_values` set drawn from the same domain. This is the case currently
   documented in CLAUDE.md as the "Generator-plus-value constraint should
   specialise, not conflict" known limitation.
   *Example:* `policies.yaml` declares `status: type: string, generator: word`.
   `fraudulent_policies.yaml` specialises `status` to `value: "cancelled"`.
   In segments containing the fraudulent-policies child, `status` is always
   `"cancelled"`; in other segments the random `word` generator still fires.

Both forms are *specialisations* — they narrow the parent's domain, not
contradict it. The planner today treats both as hard conflicts.

## Why it depends on VAR-EXPAND

In the current (pre-VAR-EXPAND) architecture, variant handling lives partly in
the planner (top-level dataset variants via `plan_variant_steps`) and partly
in `generate_member_nonref_fields` (lower-cover-member variants via VAR-2's
Level-2 sub-distribution). The VAR-SPECIALIZE drafts that predate VAR-EXPAND
had to add a *third* Level-2 IPF pass to handle specialisation correctly when
sibling members in a joint segment further constrain a variant set.

Under VAR-EXPAND every tagged union is **lowered** into cases that are ordinary
lower-cover members, and the union's **discriminant** makes the cases mutually
exclusive (`caseᵢ ∧ caseⱼ = ⊥`). Specialisation then needs no new mechanism — it
is just a constraint that prunes cases during ordinary Bernoulli factoring:

- A child's **variant-subset restriction** is an `allowed_values` constraint on
  the union's discriminant `_disc_<union>`. During factoring, any segment that
  pairs the child with an out-of-subset case pins the discriminant to two
  incompatible values → `⊥` → dropped. (Pinning a single value, as in
  VAR-EXPAND's per-case discriminant, is just the singleton case of the same
  `allowed_values` machinery — the two specs share it.)
- A child's **`value: "constant"`** specialisation propagates as a
  `FieldConstraints` whose `value` pins the parent column. Lowered cases whose
  ref-bound value `!=` constant are pruned; pure-generator parent fields merge
  cleanly (see §Generalised merge semantics).

Both forms reduce to "narrow the parent's allowed set, then let factoring + IPF
do the work". **No separate Level-2 IPF pass is needed — and, under VAR-EXPAND's
synthesis, no IPF extension either:** because lowered cases are ordinary members,
*vanilla* per-member `ipf_rescale_sparse` already restores each case's marginal
and redistributes mass pruned by specialisation. VAR-SPECIALIZE contributes only
the constraints that cause the pruning.

## Generalised merge semantics

The bigger change VAR-SPECIALIZE forces is to broaden
`FieldConstraints::Merge`. Current behaviour (`lib/constraints.rs:99-115`)
treats `value + generator` as unsatisfiable. The right model:

A parent `generator:` declares an **open domain** of values the field could
produce. A child specialising the same field with a constant or an
`allowed_values` set is *selecting a subset* of that domain — by definition
compatible.

Concretely, extend `FieldConstraints`:

```rust
pub struct FieldConstraints {
    pub generator: Option<Generator>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub value: Option<YamlValue>,
    pub allowed_values: Option<Vec<YamlValue>>,  // new — variant-subset restriction
}
```

And rewrite `Merge`:

| LHS | RHS | Merged | Reason |
|-----|-----|--------|--------|
| `generator=g` | `generator=g` | `generator=g` | unchanged |
| `generator=g₁` | `generator=g₂`, `g₁≠g₂` | `None` | unchanged (genuine conflict) |
| `generator=g` | `value=v` | `value=v` | child specialises within the generator's domain |
| `generator=g` | `allowed_values={…}` | `allowed_values={…}` *(generator dropped)* | child restricts the generator's domain |
| `value=a` | `value=b`, `a≠b` | `None` | unchanged (genuine conflict) |
| `value=v` | `allowed_values=S` | `value=v` if `v∈S` else `None` | further restriction |
| `allowed_values=S₁` | `allowed_values=S₂` | `allowed_values=S₁∩S₂` (or `None` if empty) | intersection |
| `min`/`max` ranges | | intersected | unchanged |

A `value` (constant) is always strictly tighter than a `generator` or a
non-empty `allowed_values` set. The merge picks the tightest of the inputs.

**Notable non-goal:** we deliberately do *not* implement per-generator
"is `v` in this domain?" feasibility checks. Generators are treated as
open domains. A user who specialises `generator: number range:{min:0,max:1}`
with `value: 100` will produce `100` for that child's segment; nothing
flags this as inconsistent because we don't know what values the parent's
distribution "would have" produced. The exception is bounded numeric
constraints, which we already intersect via `min`/`max`.

## Open `Satisfiable` revision

`FieldConstraints::satisfiable` currently flags `value && (generator || min || max)`
as unsatisfiable. Under the new merge semantics this rejects a legitimate
case. Tighten it to:

- `value && (min || max)`: still unsatisfiable if `value` is numeric and
  outside the range, satisfiable otherwise. Or, more simply: drop the
  generator clause and keep the min/max clause as a soft check via existing
  validation.
- `allowed_values.is_empty()` (after intersection): unsatisfiable.

`validate_field_constraints` in `lib/constraints.rs:31-55` also needs to
stop erroring on `value + generator` at YAML load time. It can still warn
or error on `value + min/max` where the numeric value falls outside the
range — that's a real user error.

## YAML syntax

### Variant-subset specialisation

The natural shape is on the child's ref field, listing the allowed subset:

```yaml
# cats.yaml
include:
  file: animals.yaml
  ref: animal
data:
  - name: eats
    ref: animal.eats
    allowed_values: [birds, mice]    # restrict to this subset
```

`allowed_values:` is a new YAML key (mapped to `Field::allowed_values:
Option<Vec<YamlValue>>` in `models.rs`). When the ref target is a tagged union,
it lowers to an `allowed_values` constraint on that union's discriminant
`_disc_<union>`. Validation: every entry must be a declared case of the parent
union; the list must be non-empty; it must be a strict subset (full set is a
no-op — warn but allow).

### Generator-domain specialisation

Already supported by the existing `value:` key — once `FieldConstraints::Merge`
allows it. No YAML change needed.

```yaml
# fraudulent_policies.yaml
include:
  file: policies.yaml
  ref: policy
data:
  - name: status
    ref: policy.status
    value: "cancelled"   # specialises parent's `generator: word`
```

## How specialisation flows through the planner

1. **Parse.** `Field::allowed_values` and `Field::value` are deserialised
   from YAML as today.

2. **Push-down (`resolve_refs`).** When the child's ref field carries a
   `value` or `allowed_values`, propagate that into the parent column's
   `FieldConstraints` via the existing merge pathway. The new
   `Merge::merge` impl resolves the value-with-generator and
   allowed-values-with-generator cases naturally.

3. **Bernoulli factoring (per VAR-EXPAND).** Each lowered case is an ordinary
   member carrying its own ref-bound `FieldConstraints` (including its
   discriminant pin). If a case's constraints are incompatible with the merged
   constraints arriving from a sibling member on the same segment — e.g. a
   child's `allowed_values` discriminant restriction, or a sibling's `value`
   pin — that segment is `⊥` and is dropped. This is the *same* conflict
   pruning VAR-EXPAND already performs; specialisation just adds constraints to
   it.

4. **IPF (vanilla — no extension).** Because lowered cases are ordinary members,
   the existing per-member `ipf_rescale_sparse` already restores each case's
   marginal and redistributes the mass pruned by a specialisation across the
   surviving cases — keeping each case's declared ratio globally honoured. The
   earlier drafts' "extend IPF to per-variant marginals" step is **obsolete**
   under VAR-EXPAND's synthesis: the marginals are per-member, and cases *are*
   members. VAR-SPECIALIZE owns no IPF change.

5. **Executor.** No changes beyond VAR-EXPAND's. Each lowered case is generated
   per-member from its concrete schema; the merged `value` / `allowed_values`
   arrive in `seg.field_constraints` and are applied to the shared atom column
   via `apply_constraints` in `generate_segment_atom_batch`.

## Validation

Add to `lib/validate.rs`:

- `allowed_values` must be non-empty.
- If the parent field is a tagged union, `allowed_values` must be a subset
  of the parent union's declared cases.
- If the parent field has only a `generator:` (no enumeration), accept any
  `allowed_values` — the user is asserting these values are reasonable
  outputs of that generator.
- `value` and `allowed_values` on the same field is an error (use `value`
  if you want a single constant; `allowed_values` for a multi-value set).
- `allowed_values` and `range` on the same field — accept and intersect
  (numeric variant sets within a bounded range).

## Files (preliminary)

| File | Expected change |
|------|----------------|
| `lib/models.rs` | Add `allowed_values: Option<Vec<YamlValue>>` to `Field`; propagate to `FieldConstraints` |
| `lib/constraints.rs` | New merge table per §Generalised merge semantics; revise `satisfiable`; drop `value + generator` rejection from `validate_field_constraints` |
| `lib/validate.rs` | `allowed_values` checks per §Validation |
| `lib/segment.rs` | DFS conflict-pruning already uses `Merge` — picks up new behaviour for free |
| `lib/rewrite.rs` | `resolve_refs` propagates child's `value`/`allowed_values` through the parent column's constraint map |
| `lib/generator.rs` | `generate_column` learns to honour `allowed_values` (pick uniformly from the set when no `value` is set) |
| `src/docgen.rs` | Document `allowed_values` in the YAML schema |
| `docs/src/content/docs/reference/yaml-schema.mdx` | New YAML field entry |
| `CLAUDE.md` | Remove "Generator-plus-value constraint should specialise, not conflict" from Known limitations once the merge change lands |

## Test plan

Statistical:

- New fixture: parent with `generator: word`; one child specialising
  `value: "alpha"`. Assert parent rows include "alpha" rows where the child
  is in the lower cover, random words elsewhere.
- New fixture: parent variant set of 4 choices; child specialising to a
  2-choice subset. Assert child rows draw only from the subset; parent's
  marginals across all 4 variants still match (statistical, α=0.001).
- New fixture: joint segment of two children specialising overlapping but
  non-equal subsets of a parent variant set. Assert their intersection is
  what fires in the joint segment.

Unit / integration:

- `Merge` table tests covering every row of the matrix in §Generalised
  merge semantics.
- DFS pruning test: a variant-set member with one variant specialised to a
  value that conflicts with a sibling member's pin; the conflicting branch
  must be pruned from `feasible`.

## Open questions

1. **Closed-enumeration generators.** `boolean` is a closed enumeration
   (`true` / `false`). Should `merge(generator=boolean, value="hello")`
   be a conflict? Likely yes — but we deliberately keep this list small
   (probably just `boolean` and any future closed-set generators). All
   other generators are treated as open domains.

2. **`allowed_values` on numeric fields.** `value: [10, 20, 30]` in a
   `range: {min: 0, max: 100}` is reasonable. Should the executor sample
   uniformly from the set? Probably yes — same machinery as the variant-
   value case, just over numeric values.

3. **Validating `allowed_values` against the parent's generator.** If
   parent has `generator: first_name` and child specialises
   `allowed_values: [1, 2, 3]`, we have a type mismatch but not a domain
   mismatch (since we don't enumerate). Catch this at validation via the
   parent's `field_type`?

4. **Ordering with VAR-EXPAND.** This spec assumes VAR-EXPAND (variant
   lowering) is implemented first. Could VAR-SPECIALIZE land without it if
   only the generator-domain part is implemented (no variant-subset)?
   Possibly — the merge table change is self-contained. The variant-subset
   half genuinely needs lowering: there is no discriminant to constrain
   until the tagged union has been lowered into cases.

## Dependencies

| Spec | Reason |
|------|--------|
| VAR-EXPAND | Variant lowering (tagged unions → cases as members + discriminant) is the substrate; variant-subset specialisation is an `allowed_values` constraint on the union discriminant |
| SEG-1 (complete) | DFS + IPF machinery — conflict pruning carries the new merge semantics; vanilla per-member IPF rebalances pruned cases (no extension) |
| VAR-2 (complete) | Defines current Level-2 behaviour; replaced by lowering in VAR-EXPAND |
| SEG-ATOM-1 (complete) | `apply_constraints` in atom-column materialisation must honour the new `allowed_values` field |
