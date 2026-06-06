# VAR-SPECIALIZE — Child specialisation of parent variant and generator fields

## Status

**Complete (S1–S5; as-built record in [`VAR-SPECIALIZE-impl.md`](VAR-SPECIALIZE-impl.md)).**
Builds on [`VAR-EXPAND`](VAR-EXPAND.md) and [`VAR-1`](VAR-1.md) (both **complete**) and
[`VAR-UNIFY`](VAR-UNIFY.md) (the **`flatten`** output primitive — **complete**). Only deferral:
multi-level `preserve_marginal` (see [`VAR-SPECIALIZE-impl.md`](VAR-SPECIALIZE-impl.md)).

**Sequencing (revised — Tom).** VAR-SPECIALIZE now lands **before VAR-UNIFY U4/Phase 2**,
because VAR-SPECIALIZE has acquired an **extra, load-bearing role: it is the prerequisite for
retiring top-level dataset `variants:`** (VAR-UNIFY U4). See §Extra role below — this is new
and reshapes the deliverables. After VAR-SPECIALIZE lands, we **circle back to VAR-UNIFY U4
(+ Phase 2)** to finish retiring top-level variants.

**As-built facts that reshape this spec (keep front of mind):**

1. **No discriminant column is materialised.** A lower-cover member's `type: variant`
   field is lowered (`lower_member_variants`, `plan.rs`) into one **case-member** per
   case; mutual exclusion is enforced **structurally** in the DFS (an `ExclusionGroup`
   branches categorically — "no case" ∪ "exactly one case"). `ExclusionGroup.discriminant`
   is only a *label* (`_disc_<name>`) carried to the executor — it is never written to a
   batch. The earlier sketch's "`one_of` constraint on a real `_disc_<union>`
   column" therefore describes machinery that **does not exist yet**.
2. **A lowered case-member pins the parent field's *value*, not a discriminant.** Via
   `merge_variant_fields`, each case-member carries the variant's field overrides; when a
   case fixes the unioned field to a constant it surfaces as an ordinary `value`
   `FieldConstraint` (`lower_cover_field_constraints`) and already participates in the
   pairwise conflict pruning the DFS performs.
3. **VAR-UNIFY U1–U3 are done.** `Field` now carries `flatten: bool` and
   `flatten_strategy: Option<FlattenStrategy>` (output-only). There is a **visible**
   `discriminant_tag_column(field) = "<field>_case"` helper (the flatten `discriminant`
   strategy) — *distinct* from the reserved `_disc_` sentinel an Option B would use. The
   `one_of` finite-set generator is **not** built yet (a VAR-SPECIALIZE deliverable).
   `flatten` is purely an output transform and **does not** touch segmentation — so it does
   *not* address the constraint-bearing capability in §Extra role.

This splits VAR-SPECIALIZE into **three deliverables** (the third is new):

- **Generator-domain specialisation (case 2 below)** — pure `FieldConstraints::Merge` /
  `satisfiable` / validation change. **No VAR-EXPAND machinery, no column, no DFS change.**
  Resolves the CLAUDE.md "Generator-plus-value constraint should specialise, not conflict"
  known limitation. Self-contained and shippable on its own.
- **Variant-subset specialisation (case 1 below)** — restricting a parent union to a
  subset of its cases. This is the half with a genuine **open design fork** (see
  §Variant-subset: how the planner sees the restriction): whether subset pruning can fall
  out of the same pairwise `value`/`one_of` merge that case-members already feed,
  or whether it forces materialising the discriminant column for the first time.
- **Constraint-bearing variant carrier on an inherited field (case 3 — NEW; the U4
  unblocker)** — allow a `ref` field to carry a `variants:` value-distribution that
  specialises the inherited field per case, each case entering lower-cover segmentation.
  This is the capability top-level `variants:` provide today and field variants **cannot**
  — so it must exist before top-level variants can be retired. See §Extra role.
- **Per-case specialisation (case 4 — NEW; `constrain_cases`)** — tighten *individual* cases
  of a ref'd parent variant by name (e.g. narrow one case's `range`), without dropping any.
  Reuses S1's field-merge ("a case is a field"); additive richness, blocks nothing. See
  §Per-case specialisation.

**Naming / verbs (decided).** Three keys, three verbs, never overlapping: `variants:`
*introduces* a variant; **`one_of`** *restricts* a ref'd variant to a subset of cases (by
value **or** case name; ratios preserved); **`constrain_cases`** *specialises* named cases
(non-restrictive). See the disambiguation table in §Restriction vs specialisation.

## What this is

Two related forms of specialisation that today are either rejected by the
validator or silently approximated by VAR-2:

1. **Variant-subset specialisation.** A child restricts a parent's tagged-union
   (`type: variant`) field to a subset of its cases.
   *Example:* `animals.yaml` declares `eats: type: variant [birds, mice, grass, fish]`.
   `cats.yaml` specialises `eats` to `[birds, mice]` — cats never eat grass or fish.
   Under VAR-EXPAND this is exactly an `one_of` constraint on the union's
   discriminant `_disc_eats` (see §Why it depends on VAR-EXPAND).

2. **Generator-domain specialisation.** A child constrains a parent's
   open-domain `generator:` field to a specific constant or to a finite
   `one_of` set drawn from the same domain. This is the case currently
   documented in CLAUDE.md as the "Generator-plus-value constraint should
   specialise, not conflict" known limitation.
   *Example:* `policies.yaml` declares `status: type: string, generator: word`.
   `fraudulent_policies.yaml` specialises `status` to `value: "cancelled"`.
   In segments containing the fraudulent-policies child, `status` is always
   `"cancelled"`; in other segments the random `word` generator still fires.

Both forms are *specialisations* — they narrow the parent's domain, not
contradict it. The planner today treats both as hard conflicts.

**Specialisation requires a `ref`.** A child field that carries `ref: parent.field` and a
tighter value-source is *specialising* (constraining) the parent's shared column — it enters
constraint merging and lower-cover conflict pruning. A child field with **no ref** is
*replacing* the parent's field: an independent column for that subpopulation, generated
fresh, with no identity promised back to the parent. This is the same opt-in-identity model
the inherited-field wiring already runs on, and it is what makes "is this field
constraint-bearing?" a cheap, explicit, per-field property (a one-pass graph rollup of
ref-to-parent fields) rather than a whole-graph inference. Consequence: conflict merging is
needed **only when ≥2 lower-cover members ref the *same* parent field**; one ref + one
replace generates both independently. (VAR-UNIFY OQ1 leans on exactly this rule to partition
pure value/shape variants from constraint-bearing ones.)

## Extra role: unblocking VAR-UNIFY U4 (case 3)

This role is **new** and is why VAR-SPECIALIZE now precedes VAR-UNIFY U4. It was discovered
implementing VAR-UNIFY: retiring top-level dataset `variants:` (U4) requires expressing every
top-level-variant usage as a field variant, and one usage class **cannot** be expressed today.

**The blocker (concrete).** The fixture `tests/fixtures/execute/variant_pruned_by_segment`
has a lower-cover member whose top-level variant *cases each carry a `ref`*:

```yaml
# member.yaml — includes parent (ratio 0.5); a sibling pins category=premium (ratio 1.0)
data:
  - { name: id, ref: p.id }
  - { name: category, ref: p.category }
variants:
  - { ratio: 0.5, data: [ { name: category, ref: p.category, value: "premium" } ] }
  - { ratio: 0.5, data: [ { name: category, ref: p.category, value: "basic" } ] }
```

The test asserts every `member.category` is `"premium"` — the `basic` case-member conflicts
with the sibling's `category=premium` pin and its segment is **pruned**. This works only
because the variant cases **ref the parent column** (`p.category`), making them the *same*
column for conflict pruning.

**Why field variants can't express it.** `FieldVariant` has **no `refs`** (and a `ref` field
bans `type`/`variants`). So there is no field-variant equivalent of "a variant whose cases
specialise an inherited (ref'd) field per case." Retiring top-level `variants:` would lose
this capability — hence VAR-SPECIALIZE must deliver it first.

**The capability to add — a Variant carrier on a ref'd field.** Allow a field to carry both a
`ref` (identity + segmentation participation) **and** a `variants:` value-distribution
(the specialisation). In carrier/support terms (§Carrier vs support), the child's value-source
for the inherited field is a **Variant carrier**: `merge(parent scalar/generator, child
Variant) = Variant` — the child supplies a richer carrier, and because it *refs*, the result
is **constraint-bearing** → each case lowers to a case-member that inherits the ref **and**
pins the case value, entering the existing DFS conflict pruning. Migrated form:

```yaml
- name: category
  ref: p.category            # identity → same column as the sibling, drives pruning
  variants:                  # Variant carrier specialising the inherited value per case
    - { value: premium, ratio: 0.5 }
    - { value: basic,   ratio: 0.5 }
```

This is the same `lower_member_variants` path VAR-EXPAND already runs; the new work is
(a) the validator allowing `ref` + `variants` together, (b) `lower_member_variants` carrying
the field's `ref` onto each case-member alongside the case value, and (c) recognising the
field as a variant despite the `ref` (it has no `type: variant`). **Mechanically this is the
constraint-bearing twin of case 1** (case 1 narrows an existing parent union; case 3
introduces a variant value-source on a previously-scalar inherited field) — both are "a ref'd
field whose child-side value-source is a Variant," so they share the lowering + conflict
machinery. Landing case 3 is the gate for VAR-UNIFY U4.

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
  decision. The clean story — "it's an `one_of` constraint on a `_disc_<union>`
  column that conflict pruning sees for free" — assumed a materialised discriminant,
  which VAR-EXPAND deliberately did **not** build. Whether subset restriction can still
  be expressed purely as pairwise `value`/`one_of` merges between the child and
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
  `one_of: [birds, mice]` on its `eats` ref; `merge(value="grass",
  one_of=[birds,mice]) = None` prunes the out-of-subset joints via the existing
  conflict machinery. **No discriminant column is ever materialised.** Cost: parent-union
  lowering must run at the right lattice position, and we must confirm a child and the
  parent's lowered cases actually share a segment.
- **Option B — materialise the discriminant.** Give the union a real `_disc_<union>`
  `UInt32`/string column (stripped from output like `_slot_idx`), have each case-member
  pin it, and carry the child's restriction as an `one_of` constraint on that
  column. This is the original sketch; it is heavier (a new sentinel through the
  segment-atom pipeline) but decouples subset restriction from whether/where the union
  was lowered.

**Resolved (Tom): A vs B is a pure implementation detail — go with A.** Same YAML, same
output (B's `_disc_` is a stripped sentinel; the *visible* tag is the separate `flatten`
`discriminant` strategy), same distribution. The earlier "A can't reach transitive
restriction" worry was wrong — it assumed top-down inheritance, but the engine pushes
constraints **down to atoms** and generates **up**, so a restriction reaches the deepest
atom at any include depth (verified: a 3-level `animals → pets → cats` chain restricting
`eats` at the leaf pins the `cats` atom correctly). A's only theoretical gap (B's
topology-agnosticism) doesn't exist under the atom model. So A is the lighter, correct choice;
B stays documented as a fallback we don't expect to need.

**The real work is carrier propagation, not A/B.** Today a variant field's **carrier** (its
`variants` cases) is dropped at the ref boundary — `FieldConstraints` (what flows through
`resolve_refs` and the segment-atom shared-column path) carries `generator`/`value`/`one_of`/
`min`/`max` but **not** `variants`. So a ref'd variant currently generates as a bare
unified-type field (garbage), and the carrier/support merge `merge(Variant, one_of) =
Variant[subset]` has nothing to act on. Fixing this — propagate the carrier so a restricted
variant is **lowered into the lattice as case sub-populations** — is S4's spine (see
§Marginal preservation), and it subsumes the A-vs-B question entirely.

## Marginal preservation and feasibility (variant-subset)

When a child restricts a parent variant to a subset, what happens to the **parent's** declared
case marginal? (`animals.eats` declared 25/25/25/25; `cats` — 25% of animals — eat only
birds/mice.) This is a real modelling question, and it turns out to be a clean, on-framework,
*solvable* one.

**It's a transportation problem.** After membership factoring the parent splits into atoms
`A_j` with fixed weights `w_j`; each atom allows a case-subset `S_j` (from the restrictions
pushed down to it). Seek per-atom case-mass `x_{j,c}` with row-sums `w_j`, column-sums = the
declared marginal `p_c`, `x_{j,c}=0` for `c∉S_j`, `x≥0`. That is exactly a **balanced bipartite
transportation problem** (atoms→cases). Consequences:

- **Feasibility is decidable**, with an explainable cut condition (Gale–Hoffman): feasible iff
  for every case-set `T`, `∑_{c∈T} p_c ≤ ∑_{j : S_j∩T≠∅} w_j`. Violations give a precise error
  ("cases {…} need X% of rows but only Y% can carry them under the restrictions").
- **Within feasibility, marginals are preserved exactly** (up to our existing largest-remainder
  rounding): IPF respecting the structural zeros converges to the max-entropy (I-projection)
  table, which — when the feasible set is non-empty — matches the margins. IPF fails to match
  *only* when infeasible, which we detect up front.

**Framework fit — it's the machinery we already run.** A parent variant's cases *partition the
population* (the glossary "tagged union: cases partition the population, pairwise meet ⊥"). So a
**restricted** parent variant is **lowered into the lattice as K case sub-populations** (ratios
`p_c`); a child's subset restriction becomes ordinary **conflict pruning**; and the existing
Bernoulli-factoring + IPF + largest-remainder restores *both* the membership and the variant
marginals in **one** solve — no new solver, no separate "Level-2 IPF". Lowering also fixes the
carrier-loss bug for free (the cases enter segmentation as real values). Cost is bounded by the
**OQ1 partition**: only a variant actually restricted by a child is lowered (paying segment
cost); an unrestricted variant stays **per-row** (VAR-UNIFY Phase 2).

**Default that stays in the feasible region — `p_c` free by default.** Best practice: *don't*
set parent variant ratios — leave them free and let restrictions specialise. Free case-mass is
slack the solver uses, so the common case is **always feasible** (a child can always be served
from free mass; conflicting children just prune to ⊥ and redistribute). Setting `p_c` is opt-in
over-constraint (then the cut condition is validated; infeasible ⇒ clear error). **Partial**
setting is allowed — pin some cases, leave others free — which widens the feasible region. We
promise exactly the contract we already promise for membership marginals: exact-when-feasible,
max-entropy, largest-remainder rounded — not a stronger guarantee.

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
- **`one_of:`** — a *finite-set generator* (uniform over a known set)
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
    pub one_of: Option<Vec<YamlValue>>,  // new — support selector (dual role: see below)
}
```

And rewrite `Merge`:

| LHS | RHS | Merged | Reason |
|-----|-----|--------|--------|
| `generator=g` | `generator=g` | `generator=g` | unchanged |
| `generator=g₁` | `generator=g₂`, `g₁≠g₂` | `None` | unchanged (genuine conflict) |
| `generator=g` | `value=v` | `value=v` | child specialises within the generator's domain |
| `generator=g` | `one_of={…}` | `one_of={…}` *(generator dropped)* | child restricts the generator's domain |
| `value=a` | `value=b`, `a≠b` | `None` | unchanged (genuine conflict) |
| `value=v` | `one_of=S` | `value=v` if `v∈S` else `None` | further restriction |
| `one_of=S₁` | `one_of=S₂` | `one_of=S₁∩S₂` (or `None` if empty) | intersection |
| `min`/`max` ranges | | intersected | unchanged |

The merge always **picks the tightest point on the spectrum**: `value` (static
generator) ≺ `one_of` (finite set) ≺ `generator` (domain) ≺ type default.

### Per-case specialisation — `constrain_cases` (case 4 / PR S5)

The same spectrum applies *inside* a tagged union: a variant **case is a field** (a
VAR-EXPAND case-member / VAR-1 `DenseUnion` child), so it has a value-source by construction
(type default, explicit `generator:`, or a `value:` static generator). Therefore
**specialising one case is just the S1 field-merge applied to that case's field — *as if it
weren't a variant.*** No new merge machinery: `constrain_cases` reuses
`FieldConstraints::Merge`. (This is the same `Merge` VAR-1 needs for generator-bearing
heterogeneous cases — one merge, designed once.)

`constrain_cases` is a **non-restrictive** per-case tightener on a field that refs a parent
`type: variant`. It is a list of structs, each addressing a parent case **by name** and
supplying only value-source deltas:

```yaml
- name: amount
  ref: claim.amount                # ref'd parent variant (cases: small, large, …)
  constrain_cases:
    - { name: large, range: { max: 100000 } }   # tighten only the `large` case
    # small, … pass through unchanged
```

Semantics:
- **Non-restrictive:** every parent case survives; only the named cases are narrowed. To
  *drop* cases, use `one_of` (below) — two distinct verbs, so there is no silent-drop
  footgun.
- Each entry merges into its case's field via S1; an unlisted case is unchanged.
- Only value-source/bounds keys are permitted per entry (`generator` / `value` / `range` /
  `one_of`) — **not** structural keys (`type`, `fields`, `content`, `ref`, nested `variants`).

**Invariant — merge only narrows.** S1's rules intersect supports, tighten bounds, or pin a
constant *within* the existing domain; **no rule shifts or re-weights.** So a per-case
specialisation can never skew one case's distribution to overlap another's (there is no
"set mean" constraint), and **case ratios are carried by the variant carrier and are
untouched by `constrain_cases`** — they renormalise only when `one_of` drops cases. This is
what makes "a case is just a field" sound.

### Restriction vs specialisation — the two verbs (and the disambiguation)

Restriction (drop cases) and specialisation (tighten cases) are **separate keys**:

- **`one_of`** *restricts* — keeps a subset of cases. On a ref'd variant it matches cases by
  their **value** *or their case `name`** (names are required for object/heterogeneous cases,
  which have no scalar value). Ratio-preserving (`merge(Variant[N], one_of[M]) = Variant[M]`).
- **`constrain_cases`** *specialises* — tightens named cases, drops none.

The keyword that **introduces** a variant is `variants:`; `one_of`/`constrain_cases`
**constrain** an existing (ref'd) one. The full disambiguation, by the ref target's carrier:

| ref target is… | `variants:` | `one_of:` | `constrain_cases:` |
|----------------|-------------|-----------|--------------------|
| a **scalar** field | **introduce** a variant value-distribution (case 3) | restrict the scalar's values (finite set) | — *(error: no cases)* |
| a **variant** field | — *(error: already a variant)* | **restrict** to a subset of cases (by value or name; ratios preserved) | **specialise** named cases (non-restrictive) |
| *(no ref — a fresh field)* | introduce an ordinary field variant | uniform finite-set generator | — *(error: no cases)* |

This table is the validation spec; the docs must state it so the three keys never read as
overlapping.

### Carrier vs support: `one_of` onto a Variant (the formalisation)

`one_of` has a **dual role** that must be formalised precisely so it doesn't read as
improvised. The split is between two facets of a value-source:

- **carrier** — its *structure*: a flat domain, or a `Variant` (a tagged union carrying
  per-case ratios *and* heterogeneous per-case generators/types, the VAR-1 substrate);
- **support** — the *set* of values/cases it can produce.

`one_of` (and `value`) constrain the **support** only — they are **support selectors**,
not carriers. Merge then obeys one rule: **intersect the supports, keep the richest
carrier.** A `Variant` is a richer carrier than a flat domain, so it survives the merge:

```
merge( Variant[cases N],  one_of[M] )  with M ⊆ N   :=   Variant[M]
```

i.e. the surviving M cases **bring their ratios (renormalised over M) and their
heterogeneous generators with them** — the result is still a tagged union, just
narrower. Critically:

```
merge( Variant, one_of )  ≠  one_of
```

Degrading a restricted variant to a bare `one_of` would throw away the ratios and the
per-case structure — that is the wrong answer. By contrast, with **no** richer carrier
present (a fresh field, or a plain `generator:` domain), `one_of` supplies its *own*
simple carrier:

```
merge( flat-domain, one_of[M] )  :=  one_of[M]   (uniform, value-only, no ratios)
```

So `one_of` standalone is the simpler thing (a uniform finite-set generator); `one_of`
*as a constraint on a variant* is a subset restriction that preserves the variant's
richer semantics. Same operator, different result — determined by the **other operand's
carrier**, never by `one_of` itself. The merge table above is the flat-carrier case;
this rule is the carrier-aware generalisation that subsumes it (a flat domain's "carrier"
is trivial, so "keep the richest carrier" reduces to the table).

> **Docs NB.** The reference must call this out explicitly: using `one_of` *on a field
> that refs/specialises a parent `type: variant`* specialises the variant and **carries
> the variant's ratios and heterogeneous types with it** (`merge(Variant, one_of) =
> Variant[subset]`), whereas `one_of` on a standalone field is just "generate one of
> these values" (uniform, no ratios, single type). Two roles, one keyword — spelled out
> so it never reads as a coincidence.

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
- `one_of.is_empty()` (after intersection): unsatisfiable.

`validate_field_constraints` in `lib/constraints.rs:31-55` also needs to
stop erroring on `value + generator` at YAML load time. It can still warn
or error on `value + min/max` where the numeric value falls outside the
range — that's a real user error.

## YAML syntax

### Variant-subset restriction (`one_of`)

The natural shape is on the child's ref field, listing the cases to keep — by **value** (for
scalar cases) or by **case name** (required for object/heterogeneous cases):

```yaml
# cats.yaml
include:
  file: animals.yaml
  ref: animal
data:
  - name: eats
    ref: animal.eats
    one_of: [birds, mice]    # restrict to this subset (ratios preserved, renormalised)
```

`one_of:` is a new YAML key (mapped to `Field::one_of: Option<Vec<YamlValue>>` in
`models.rs`). When the ref target is a tagged union it restricts the surviving cases
(`merge(Variant[N], one_of[M]) = Variant[M]`). Validation: every entry must be a declared
case of the parent union (matched by value or by `name`); the list must be non-empty; a full
set is a no-op (warn but allow).

### Per-case specialisation (`constrain_cases`)

Tighten specific parent cases without dropping any (see §Per-case specialisation):

```yaml
- name: amount
  ref: claim.amount
  constrain_cases:
    - { name: large, range: { max: 100000 } }   # narrow only the `large` case
```

`constrain_cases:` is a new YAML key on `Field` (a list of per-case delta structs). Each
entry names a parent case and supplies value-source/bounds deltas only; it merges into that
case's field via S1. Combine with `one_of` to restrict *and* tighten.

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

1. **Parse.** `Field::one_of` and `Field::value` are deserialised
   from YAML as today.

2. **Push-down (`resolve_refs`).** When the child's ref field carries a
   `value` or `one_of`, propagate that into the parent column's
   `FieldConstraints` via the existing merge pathway. The new
   `Merge::merge` impl resolves the value-with-generator and
   allowed-values-with-generator cases naturally.

3. **Bernoulli factoring (per VAR-EXPAND).** Each lowered case-member carries its
   own ref-bound `FieldConstraints` — a `value` pin on the unioned parent field
   (there is no discriminant pin; see §Status). If a case-member's constraints are
   incompatible with the merged constraints arriving from a sibling member on the
   same segment — e.g. a child's `one_of` restriction, or a sibling's
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
   `one_of` arm, and `generate_column` must honour it (sample uniformly from
   the set when no single `value` is set). So "no executor changes" is **not** true —
   the generator/atom path gains `one_of` support. Under Option B it would also
   need to thread the `_disc_<union>` sentinel through the atom pipeline and strip it on
   emit.

## Validation

Add to `lib/validate.rs`:

- `one_of` must be non-empty.
- If the parent field is a tagged union, every `one_of` entry must match a declared case
  (by **value** or by case **name**).
- If the parent field has only a `generator:` (no enumeration), accept any `one_of` — the
  user is asserting these values are reasonable outputs of that generator.
- `value` and `one_of` on the same field is an error (use `value` for a single constant;
  `one_of` for a multi-value set).
- `one_of` and `range` on the same field — accept and intersect.
- **`constrain_cases`** is valid only on a field that refs a `type: variant`; each entry's
  `name` must match a parent case; entries carry value-source/bounds keys only (no `type` /
  `fields` / `content` / `ref` / nested `variants`); the per-case delta must merge
  satisfiably with that case's field.
- The `(parent carrier × key)` table in §Restriction vs specialisation is the placement spec
  (e.g. `constrain_cases` on a scalar ref → error; `variants` on a variant ref → error).

## Files (preliminary)

Case 2 = generator-domain (self-contained); Case 1 = variant-subset (gated on the Option A/B
decision); **Case 3 = constraint-bearing variant carrier on a ref'd field (the VAR-UNIFY U4
unblocker; see §Extra role); Case 4 = per-case `constrain_cases` (additive).** The full PR
sequencing is in [`VAR-SPECIALIZE-impl.md`](VAR-SPECIALIZE-impl.md).

| File | Expected change | Case |
|------|----------------|------|
| `lib/constraints.rs` | New merge table per §Generalised merge semantics; revise `satisfiable` **and** `validate_field_constraints` (both currently reject `value + generator`, constraints.rs:31-71) | 2 |
| `lib/segment.rs` | DFS conflict-pruning already uses `Merge` — picks up new `value/generator` behaviour for free | 2 |
| `lib/models.rs` | Add `one_of: Option<Vec<YamlValue>>` to `Field` and `FieldConstraints`; **(case 4)** `constrain_cases: Vec<CaseDelta>` on `Field` | 1/2/4 |
| `lib/validate.rs` | `one_of` checks per §Validation (match by value **or** name); **(case 3)** allow `ref` + `variants` together; **(case 4)** `constrain_cases` placement + per-case checks; the `(parent carrier × key)` table | 1/2/3/4 |
| `lib/rewrite.rs` | `resolve_refs` propagates child's `value`/`one_of` through the parent column's constraint map; **(case 4)** routes per-case deltas to each case's field | 2/4 |
| `lib/generator.rs` | `generate_column` learns to honour `one_of` (pick uniformly when no `value`) | 2 |
| `lib/executor.rs` | `apply_constraints` (executor.rs:2338) gains an `one_of` arm; Option B additionally threads `_disc_<union>` through `generate_segment_atom_batch` and strips it on emit | 2 |
| `lib/plan.rs` | Option A only: lower the *parent's* union at the child's position. **(Case 3)** `lower_member_variants` carries the field's `ref` onto each case-member alongside the case value; recognise a `ref`+`variants` field as a variant to lower. **(Case 4)** merge each `constrain_cases` delta into the matching lowered case-member | 1/3/4 |
| `lib/expand_variants.rs` | **(Case 3)** `collect_variant_paths` recognises a `ref` field bearing `variants:` (no `type: variant`) as a variant path | 3 |
| `src/docgen.rs` | Document `one_of`; **(case 3)** `ref` + `variants`; **(case 4)** `constrain_cases` | 2/3/4 |
| `docs/src/content/docs/reference/yaml-schema.mdx` | New YAML field entries (`one_of`, `constrain_cases`) + the verbs/disambiguation table | 2/3/4 |
| `CLAUDE.md` | Remove "Generator-plus-value constraint should specialise, not conflict" from Known limitations once Case 2 lands | 2 |

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

The first two are now **resolved** (see §Variant-subset and §Marginal preservation). The rest
are smaller policy calls.

1. **Variant-subset mechanism: Option A vs B — RESOLVED: A (pure implementation detail).**
   Same YAML/output/distribution; the atom-up engine makes A topology-complete. B is a
   documented fallback we don't expect to need. The real work is **carrier propagation**
   (lower a restricted parent variant into the lattice as case sub-populations), which
   subsumes A/B.

2. **Parent-vs-member union position — RESOLVED.** The engine pushes constraints down to atoms
   and generates up, so the restriction reaches the deepest atom regardless of include depth
   (verified on a 3-level chain). Lowering a *restricted* parent variant into the lattice (the
   OQ1 partition: only restricted variants are lowered; unrestricted stay per-row) puts its
   cases into the same factoring as membership — no parent-vs-member position problem.

   **Marginal preservation is solvable (resolved).** It's a balanced transportation problem
   (atoms→cases): feasibility decidable via the Gale–Hoffman cut condition; within feasibility
   IPF preserves margins exactly (max-entropy). `p_c` are **free by default** (always feasible);
   setting them is opt-in and cut-validated. See §Marginal preservation.

3. **Split the deliverable?** The generator-domain half (case 2) is self-contained:
   the `Merge`/`satisfiable`/validation change plus the `one_of`
   generator support, with no dependency on the Option A/B decision. It also clears a
   standing CLAUDE.md known limitation. Strong candidate to land **first**, as its own
   PR, before the variant-subset half is even designed. (Reframes the original Q4.)

4. **Closed-enumeration generators — none exist today.** The merge model treats
   every generator as an **open domain**: a child's `value:`/`one_of:` selects
   within it and is never a domain conflict. The only real gates are **type**
   compatibility (already enforced by `Generator::valid_for`, models.rs:348) and
   numeric range. Surveying the `Generator` enum (models.rs:282), *no* variant is a
   closed enumeration — names, words, emails, uuids, currency/state codes are all
   open or "pin any valid member" sets — and `boolean` is a `FieldType`, not a
   generator. So there is nothing to denylist now; if a genuinely closed-set generator
   is ever added, revisit then. (Earlier drafts used a fictional
   `merge(generator=boolean, value=…)` example — removed.)

5. **`one_of` on numeric fields.** `value: [10, 20, 30]` in a
   `range: {min: 0, max: 100}` is reasonable. Sample uniformly from the set?
   Probably yes — same machinery as the variant-value case, over numeric values.

6. **Validating `one_of` against the parent's generator.** If parent has
   `generator: first_name` and child specialises `one_of: [1, 2, 3]`, we
   have a type mismatch but not a domain mismatch (we don't enumerate). Catch this at
   validation via the parent's `field_type`?

## Dependencies

| Spec | Reason |
|------|--------|
| VAR-UNIFY U1–U3 (**complete**) | The `flatten` output primitive (+ `flatten_strategy`) is done; it cleanly subsumes top-level variants' *output-shape* role. `flatten` is output-only and does **not** touch segmentation, so it does not provide case 3's constraint-bearing capability. |
| VAR-UNIFY U4 / Phase 2 (**blocked on this spec**) | **Sequencing reversed.** VAR-SPECIALIZE now lands *before* U4: U4 retires top-level `variants:`, which requires case 3 (constraint-bearing variant on a ref'd field; see §Extra role). After VAR-SPECIALIZE, **circle back to VAR-UNIFY U4 + Phase 2.** |
| VAR-EXPAND (complete) | Variant lowering (tagged unions → cases as members + discriminant) is the substrate; variant-subset specialisation is a `one_of` constraint on the union discriminant |
| VAR-1 (complete) | Multi-type / multi-object union substrate (`FieldType::Union`, `Field::union_cases`, `is_heterogeneous`, nullable-superset output). Object-case carriers and the per-case generator spectrum that the carrier/support merge narrows |
| SEG-1 (complete) | DFS + IPF machinery — conflict pruning carries the new merge semantics; vanilla per-member IPF rebalances pruned cases (no extension) |
| VAR-2 (complete) | Defines current Level-2 behaviour; replaced by lowering in VAR-EXPAND |
| SEG-ATOM-1 (complete) | `apply_constraints` in atom-column materialisation must honour the new `one_of` field |
