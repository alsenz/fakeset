# VAR-CASE-NAME — `one_of` / `constrain_cases` by case *name* for heterogeneous unions

## Status

**Future — designed, not built.** The last open item from the variant roadmap
(see CLAUDE.md "Known limitations" → *Variant specialisation*). Same-type carriers
already match cases by **value** (`one_of`) and by **name** (`constrain_cases`); this
spec extends both verbs to **heterogeneous** (multi-type / object) carriers, which
are addressable only by case *name*.

## What this is

A child field that `ref`s a parent **heterogeneous** `type: variant` should be able to:

- **`one_of: [caseA, caseB]`** — restrict the inherited union to a subset of its cases,
  *by name*, with the surviving ratios **renormalised** over the survivors
  (`merge(Variant, one_of) = Variant[subset]`, exactly as the same-type path does today).
- **`constrain_cases: [{name: caseA, …}]`** — specialise a named case's scalar
  value-source (value / generator / range), leaving other cases untouched.

Same-type carriers select cases by the case's scalar **value**; a heterogeneous carrier
has no single scalar value per case (cases differ in *type*, and object cases have no
scalar at all), so the case **name** is the only stable selector. **The carrier type
decides which selector applies** — consistent with the "context decides" pattern used
elsewhere (VAR-UNIFY OQ1).

## The gap (current behaviour)

A heterogeneous variant lowers (in `expand_variants::lower_heterogeneous_unions`) to
`FieldType::Union` + `Field::union_cases: Vec<UnionCase>`, where each
`UnionCase.field.name` carries the case label (the `FieldVariant.name`, or `<field>_<i>`
positionally). Three places are unaware of `one_of`/`constrain_cases` for this carrier:

1. **`rewrite::merged_ref_field` drops the carrier.** The single ref-field constructor
   (both `resolve_field` and `resolve_list_link_content_field` route through it) sets
   `field_type: base.field_type.clone()` (so the child becomes `Union`) but copies only
   `variants`, never `union_cases` — `..Default::default()` leaves it empty. **A child
   ref onto a heterogeneous parent variant is therefore already broken**: the child
   inherits `type: Union` with *no cases*. This is the load-bearing fix; the other two
   are inert until the carrier actually reaches the child.

2. **`generator::build_union_column` ignores `field.one_of`.** It generates straight from
   `field.union_cases` with no name filter / renormalisation (contrast
   `build_same_type_variant_column`, which filters `field.variants` by value and
   renormalises).

3. **`executor::apply_constraints` only touches `field.variants`.** Its `case_overrides`
   loop matches `f.variants` by `name`; it never looks at `f.union_cases`, so a
   `constrain_cases` delta against a heterogeneous case is silently dropped.

## Design

### 1. Propagate the carrier (`rewrite.rs`)

A **one-line** addition in `merged_ref_field` (the single ref-field constructor) copies the
base/target union carrier onto a plain ref'd variant, mirroring the existing `variants` rule
(case-3 keeps its own carrier) — and both ref-field builders inherit it for free:

```rust
union_cases: if local.variants.is_empty() { base.union_cases.clone() } else { vec![] },
```

(Same guard as `variants:` — a case-3 field owns its cases and is lowered by the planner,
so it must not inherit the parent's union carrier.)

### 2. `one_of` filters the union carrier by name (`generator.rs`)

At the top of `build_union_column`, apply the same subset-and-renormalise the same-type
path uses, but **matched on `case.field.name`** instead of value:

```rust
let restricted: Vec<UnionCase>;
let cases: &[UnionCase] = if let Some(set) = &field.one_of {
    let names: HashSet<&str> = set.iter().filter_map(|v| v.as_str()).collect();
    restricted = field.union_cases.iter()
        .filter(|c| names.contains(c.field.name.as_str()))
        .cloned().collect();
    if restricted.is_empty() {
        bail!("union field '{}': `one_of` restricts to no declared cases (by name)", field.name);
    }
    &restricted
} else { &field.union_cases };
```

Ratios already renormalise downstream (`resolve_distributions` over the surviving
`cases`), so no extra normalisation step is needed — the cumulative draw is over the
survivors. `type_id i` stays consistent because it indexes the *post-filter* `cases`
slice, and the Arrow `UnionFields` are rebuilt from the same slice (already the case).

### 3. `constrain_cases` specialises a named union case (`executor.rs`)

Extend `apply_constraints`'s `case_overrides` loop to also match `f.union_cases` by
`uc.field.name == delta.name` and merge the delta into that case's `field`
(value / generator / range — the same fields the `f.variants` branch sets, applied to
`uc.field`). **Scope: scalar cases only.** A delta naming an **object** case is rejected
at validation (specialising a sub-field of an object case is a separate, larger feature —
it needs a sub-path selector and is explicitly out of scope here).

## Validation (`validate.rs`)

The ref-field branch of `validate_field` skips type-dependent checks (the type is
inherited), so name-string `one_of` entries already pass. Add, once the carrier is
resolvable (i.e. in the rewrite/post-resolve validation pass, where `union_cases` is
populated):

- **`one_of` name membership** — every entry must name a real case of the inherited
  union; unknown name → error listing the valid case names.
- **`constrain_cases` name membership** — every `delta.name` must name a real case.
- **object-case rejection** — a `constrain_cases` delta naming a case whose
  `union_case.field.field_type == Object` is rejected with a "specialising object-case
  sub-fields is not supported" message.

(Same-type carriers keep value-matching; the selector is chosen by `field_type == Union`.)

## Test plan

- **Unit** (`generator.rs`): `build_union_column` with `one_of` by name keeps only the
  named cases and the drawn distribution renormalises (count-based assertion over a large
  `n`).
- **Unit** (`constraints.rs`/`executor.rs`): `apply_constraints` applies a
  `constrain_cases` delta to the matching `union_case.field` and leaves others untouched.
- **Integration** (fixture): a child dataset refs a heterogeneous parent variant and
  restricts it `one_of: [name1, name2]`; assert the output union has only those two case
  sub-fields populated and referential integrity holds.
- **Integration** (negative): `one_of` / `constrain_cases` naming an unknown case →
  validation error; `constrain_cases` on an object case → validation error.
- **Statistical**: extend an example with a name-restricted heterogeneous variant and add
  a chi-squared check that the surviving cases carry the renormalised marginal.

## Dependencies

| Spec | Reason |
|------|--------|
| [`VAR-1`](done/VAR-1.md) | Provides the heterogeneous carrier (`FieldType::Union` + `union_cases`) this restricts |
| [`VAR-SPECIALIZE`](done/VAR-SPECIALIZE.md) | Provides the value-source spectrum, `one_of` (S4) and `constrain_cases` (S5) — this generalises both from value/same-type to name/heterogeneous |

## Non-goals

- Specialising **sub-fields of an object case** by dotted path (a larger feature; object
  cases are name-addressable only as a whole here, and only for selection, not sub-field
  specialisation).
- `preserve_marginal` interaction beyond what S4c already does — a name-restricted
  heterogeneous variant pins its marginal through the same single-level path; multi-level
  pinning remains the separate deferred item.
