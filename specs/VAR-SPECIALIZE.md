# VAR-SPECIALIZE — Child specialisation of parent variant and generator fields

## Status

**Proposed — design exploration.** Builds on [`VAR-EXPAND`](done/VAR-EXPAND.md),
which is **complete**. Two as-built facts from VAR-EXPAND reshape this spec and must
be kept front of mind:

1. **No discriminant column is materialised.** A lower-cover member's `type: variant`
   field is lowered (`lower_member_variants`, `plan.rs`) into one **case-member** per
   case; mutual exclusion is enforced **structurally** in the DFS (an `ExclusionGroup`
   branches categorically — "no case" ∪ "exactly one case"). `ExclusionGroup.discriminant`
   is only a *label* (`_disc_<name>`) carried to the executor — it is never written to a
   batch. The earlier sketch's "`allowed_values` constraint on a real `_disc_<union>`
   column" therefore describes machinery that **does not exist yet**.
2. **A lowered case-member pins the parent field's *value*, not a discriminant.** Via
   `merge_variant_fields`, each case-member carries the variant's field overrides; when a
   case fixes the unioned field to a constant it surfaces as an ordinary `value`
   `FieldConstraint` (`lower_cover_field_constraints`) and already participates in the
   pairwise conflict pruning the DFS performs.

This splits VAR-SPECIALIZE into **two largely independent halves**:

- **Generator-domain specialisation (case 2 below)** — pure `FieldConstraints::Merge` /
  `satisfiable` / validation change. **No VAR-EXPAND machinery, no column, no DFS change.**
  Resolves the CLAUDE.md "Generator-plus-value constraint should specialise, not conflict"
  known limitation. Self-contained and shippable on its own.
- **Variant-subset specialisation (case 1 below)** — restricting a parent union to a
  subset of its cases. This is the half with a genuine **open design fork** (see
  §Variant-subset: how the planner sees the restriction): whether subset pruning can fall
  out of the same pairwise `value`/`allowed_values` merge that case-members already feed,
  or whether it forces materialising the discriminant column for the first time. That fork
  is the main thing to decide before an implementation plan.

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

Under VAR-EXPAND a lower-cover member's tagged union is **lowered** into cases that
are ordinary lower-cover members, mutually exclusive by **structural** DFS branching
(`caseᵢ ∧ caseⱼ = ⊥` because at most one case-member's bit is ever set). What
VAR-SPECIALIZE adds in each case:

- A child's **`value: "constant"`** specialisation (case 2) propagates as a
  `FieldConstraints` whose `value` pins the parent column. Lowered case-members whose
  pinned value `!=` the constant are pruned by *existing* pairwise conflict checking;
  pure-generator parent fields merge cleanly once `Merge` stops treating
  `value + generator` as a conflict (see §Generalised merge semantics). This half
  genuinely "needs no new mechanism."
- A child's **variant-subset restriction** (case 1) is the half that needs a real
  decision. The clean story — "it's an `allowed_values` constraint on a `_disc_<union>`
  column that conflict pruning sees for free" — assumed a materialised discriminant,
  which VAR-EXPAND deliberately did **not** build. Whether subset restriction can still
  be expressed purely as pairwise `value`/`allowed_values` merges between the child and
  each lowered case-member, or whether it forces materialising the discriminant, depends
  on a subtlety the original draft glossed over: **the union being restricted sits on the
  *parent*, but VAR-EXPAND only lowers *member* unions** (and explicitly skips members
  that are also parents). See §Variant-subset: how the planner sees the restriction.

For both halves, the IPF story is unchanged and favourable: because lowered cases are
ordinary members, **no separate Level-2 IPF pass and no IPF extension is needed** —
*vanilla* per-member `ipf_rescale_sparse` already restores each case's marginal and
redistributes mass pruned by specialisation. VAR-SPECIALIZE contributes only the
constraints that cause the pruning.

## Variant-subset: how the planner sees the restriction

This is the central open fork for case 1. Restating the scenario precisely against
as-built code: `animals.yaml` (the **parent**) declares `eats: type: variant
[birds, mice, grass, fish]`; `cats.yaml` (a **child**, i.e. a lower-cover member of
`animals`) wants `eats ∈ {birds, mice}`.

The catch: VAR-EXPAND's `lower_member_variants` lowers the union of a **lower-cover
member** into case-members. The union here lives on the **parent** `animals`, so at the
position where `cats` restricts it there may be **no `ExclusionGroup` and no
case-members at all** — the parent's `eats` is just a field generated by the
segment-atom pipeline. Two candidate designs:

- **Option A — pairwise merge, no column.** Lower the *parent's* union too (or recognise
  that, where the child co-segments with the parent's lowered cases, each case-member
  pins `eats = <case>` as a `value` constraint). The child carries
  `allowed_values: [birds, mice]` on its `eats` ref; `merge(value="grass",
  allowed_values=[birds,mice]) = None` prunes the out-of-subset joints via the existing
  conflict machinery. **No discriminant column is ever materialised.** Cost: parent-union
  lowering must run at the right lattice position, and we must confirm a child and the
  parent's lowered cases actually share a segment.
- **Option B — materialise the discriminant.** Give the union a real `_disc_<union>`
  `UInt32`/string column (stripped from output like `_slot_idx`), have each case-member
  pin it, and carry the child's restriction as an `allowed_values` constraint on that
  column. This is the original sketch; it is heavier (a new sentinel through the
  segment-atom pipeline) but decouples subset restriction from whether/where the union
  was lowered.

Picking between A and B — and resolving the parent-vs-member-position question that
drives it — is the prerequisite for an implementation plan for case 1. Case 2 does not
wait on this.

## Generalised merge semantics

The bigger change VAR-SPECIALIZE forces is to broaden
`FieldConstraints::Merge`. Current behaviour (`lib/constraints.rs:99-115`)
treats `value + generator` as unsatisfiable. The unifying model that dissolves
that:

**A field's value-source is a single spectrum of generators ordered by how much
of the domain they pin down — not a set of competing fields.** Having a `type`
already gives the field its *default generator* (random-for-the-type); everything
else is a *narrower generator over the same domain*:

- **type default** — widest (any value of the type)
- **explicit `generator:`** — a narrower domain (`email`, a gaussian, …)
- **`allowed_values:`** — a *finite-set generator* (uniform over a known set)
- **`value:`** — a **static (const) generator**: a singleton the planner knows
  ahead of time. `value` *is* a generator — the maximally specialised one.

Seen this way, a child specialising a field just **replaces the inherited
generator with a more specialised one further down the same spectrum** — never a
conflict. The old `value + generator` "conflict" was a category error: `value`
doesn't fight the generator, it *is* a tighter (static) generator that supersedes
it. Two specialisations conflict only when they pick *incompatible* points on the
spectrum — two different constants, disjoint sets, or a value outside an allowed
set.

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

The merge always **picks the tightest point on the spectrum**: `value` (static
generator) ≺ `allowed_values` (finite set) ≺ `generator` (domain) ≺ type default.

### Per-case generators (mirrors VAR-1)

The same spectrum applies *inside* a tagged union: a variant **case is a field**, so
it has a generator by construction (its type default, an explicit `generator:`, or a
`value:` static generator — the last being *terribly common*, especially for same-type
unions). Two consequences VAR-SPECIALIZE must plan for from the start:

- A child can **specialise a single case's generator** (e.g. restrict one case from an
  open generator to a `value`, or swap a case's gaussian for a tighter one) — the exact
  same merge, applied per case.
- This is the same machinery VAR-1 needs for **generator-bearing heterogeneous cases**
  (the salary two-gaussian shape). VAR-1 and VAR-SPECIALIZE's generator-domain half
  therefore share *one* `FieldConstraints::Merge` — they must be designed against it
  together, not as two separate notions of "specialise a generator."

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

3. **Bernoulli factoring (per VAR-EXPAND).** Each lowered case-member carries its
   own ref-bound `FieldConstraints` — a `value` pin on the unioned parent field
   (there is no discriminant pin; see §Status). If a case-member's constraints are
   incompatible with the merged constraints arriving from a sibling member on the
   same segment — e.g. a child's `allowed_values` restriction, or a sibling's
   `value` pin — that segment is `⊥` and is dropped. This is the *same* pairwise
   conflict pruning VAR-EXPAND already performs; specialisation just adds
   constraints to it. (Whether the parent union's cases are present to be pruned at
   the child's position is the Option A/B question in §Variant-subset.)

4. **IPF (vanilla — no extension).** Because lowered cases are ordinary members,
   the existing per-member `ipf_rescale_sparse` already restores each case's
   marginal and redistributes the mass pruned by a specialisation across the
   surviving cases — keeping each case's declared ratio globally honoured. The
   earlier drafts' "extend IPF to per-variant marginals" step is **obsolete**
   under VAR-EXPAND's synthesis: the marginals are per-member, and cases *are*
   members. VAR-SPECIALIZE owns no IPF change.

5. **Executor.** Each lowered case is generated per-member from its concrete
   schema; the merged constraints arrive in `seg.field_constraints` and are applied
   to the shared atom column via `apply_constraints` in `generate_segment_atom_batch`
   (`executor.rs`). One concrete change is required here: `apply_constraints` today
   handles `value` / `generator` / `min` / `max` only — it must learn an
   `allowed_values` arm, and `generate_column` must honour it (sample uniformly from
   the set when no single `value` is set). So "no executor changes" is **not** true —
   the generator/atom path gains `allowed_values` support. Under Option B it would also
   need to thread the `_disc_<union>` sentinel through the atom pipeline and strip it on
   emit.

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

Half-1 = generator-domain specialisation (self-contained); Half-2 = variant-subset
(gated on the Option A/B decision).

| File | Expected change | Half |
|------|----------------|------|
| `lib/constraints.rs` | New merge table per §Generalised merge semantics; revise `satisfiable` **and** `validate_field_constraints` (both currently reject `value + generator`, constraints.rs:31-71) | 1 |
| `lib/segment.rs` | DFS conflict-pruning already uses `Merge` — picks up new `value/generator` behaviour for free | 1 |
| `lib/models.rs` | Add `allowed_values: Option<Vec<YamlValue>>` to `Field` and `FieldConstraints` | 2 |
| `lib/validate.rs` | `allowed_values` checks per §Validation | 2 |
| `lib/rewrite.rs` | `resolve_refs` propagates child's `value`/`allowed_values` through the parent column's constraint map | 2 |
| `lib/generator.rs` | `generate_column` learns to honour `allowed_values` (pick uniformly from the set when no `value` is set) | 2 |
| `lib/executor.rs` | `apply_constraints` (executor.rs:2338) gains an `allowed_values` arm so it reaches the shared atom column; Option B additionally threads `_disc_<union>` through `generate_segment_atom_batch` and strips it on emit | 2 |
| `lib/plan.rs` | Option A only: lower the *parent's* union at the child's position (counterpart to `lower_member_variants`' member-only lowering) — resolves §open-question-2 | 2 |
| `src/docgen.rs` | Document `allowed_values` in the YAML schema | 2 |
| `docs/src/content/docs/reference/yaml-schema.mdx` | New YAML field entry | 2 |
| `CLAUDE.md` | Remove "Generator-plus-value constraint should specialise, not conflict" from Known limitations once Half-1 lands | 1 |

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

The first two are **load-bearing** — an implementation plan cannot start for the
variant-subset half until they are answered. The rest are smaller policy calls.

1. **Variant-subset mechanism: Option A (pairwise merge, no column) vs Option B
   (materialise `_disc_<union>`).** See §Variant-subset. This is the decision that
   shapes the whole case-1 implementation.

2. **Parent-vs-member union position.** VAR-EXPAND lowers a *lower-cover member's*
   union and **skips members that are also parents**. The fields a child restricts
   live on the *parent*. So: does the parent's union get lowered into case-members at
   the position the child co-segments, or not? If not, Option A is unavailable and
   Option B is forced. Confirming the exact planner position (and whether
   `lower_member_variants`' skip rule needs a counterpart for parent unions) is a
   prerequisite. This wasn't a question in the original draft because it assumed a
   pre-materialised discriminant.

3. **Split the deliverable?** The generator-domain half (case 2) is self-contained:
   the `Merge`/`satisfiable`/validation change plus the `allowed_values`
   generator support, with no dependency on the Option A/B decision. It also clears a
   standing CLAUDE.md known limitation. Strong candidate to land **first**, as its own
   PR, before the variant-subset half is even designed. (Reframes the original Q4.)

4. **Closed-enumeration generators — none exist today.** The merge model treats
   every generator as an **open domain**: a child's `value:`/`allowed_values:` selects
   within it and is never a domain conflict. The only real gates are **type**
   compatibility (already enforced by `Generator::valid_for`, models.rs:348) and
   numeric range. Surveying the `Generator` enum (models.rs:282), *no* variant is a
   closed enumeration — names, words, emails, uuids, currency/state codes are all
   open or "pin any valid member" sets — and `boolean` is a `FieldType`, not a
   generator. So there is nothing to denylist now; if a genuinely closed-set generator
   is ever added, revisit then. (Earlier drafts used a fictional
   `merge(generator=boolean, value=…)` example — removed.)

5. **`allowed_values` on numeric fields.** `value: [10, 20, 30]` in a
   `range: {min: 0, max: 100}` is reasonable. Sample uniformly from the set?
   Probably yes — same machinery as the variant-value case, over numeric values.

6. **Validating `allowed_values` against the parent's generator.** If parent has
   `generator: first_name` and child specialises `allowed_values: [1, 2, 3]`, we
   have a type mismatch but not a domain mismatch (we don't enumerate). Catch this at
   validation via the parent's `field_type`?

## Dependencies

| Spec | Reason |
|------|--------|
| VAR-EXPAND | Variant lowering (tagged unions → cases as members + discriminant) is the substrate; variant-subset specialisation is an `allowed_values` constraint on the union discriminant |
| SEG-1 (complete) | DFS + IPF machinery — conflict pruning carries the new merge semantics; vanilla per-member IPF rebalances pruned cases (no extension) |
| VAR-2 (complete) | Defines current Level-2 behaviour; replaced by lowering in VAR-EXPAND |
| SEG-ATOM-1 (complete) | `apply_constraints` in atom-column materialisation must honour the new `allowed_values` field |
