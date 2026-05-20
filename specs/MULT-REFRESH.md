# Multiplicity Semantics Refresh

The purpose of this document is to crystallise the semantics of includes into a unified, elegant design. Once agreed, this file is deleted and its content folded into MULT-{1,2,3}.md.

## What stays the same

- The fundamental architectural tenet: children by inclusion are generated first; data accumulates upward to parents. This never changes.
- Sibling segmentation models joint distributions by decomposing shared-parent siblings into joint slices. The algorithm (`plan_segments`, IPF) is unchanged.
- Nested include lists remain syntactic brevity for an implicit flat child dataset, but this document makes that equivalence structurally explicit.

---

## Three forms of include

Rather than an `includes:` array where every entry looks the same, includes take three structurally distinct forms. The driver/pool relationship is encoded in structure, not inferred at planning time.

### Form 1 — `include` (the driver)

Declares the primary parent-by-inclusion. Exactly one per dataset (or none). The driver is the entity whose population drives row count and field-constraint segmentation.

```yaml
include:
  file: individuals.yaml
  ref: individuals             # reference name for field lookups
  ratio: 0.1                  # fraction of driver population in this child
  cardinality: {min: 1, max: 6}   # optional: junction rows per driver row (defaults 1)
```

Fields:
- **`file`** — path to the included dataset (unchanged)
- **`ref`** — reference name (unchanged; optional when no fields are referenced)
- **`ratio`** — replaces `distribution`. Fraction of the driver parent's population that this child covers (Bernoulli probability for segmentation). Defaults to 1.0.
- **`cardinality`** — replaces top-level `multiplicity`. How many child/junction rows to generate per driver row. Defaults to 1. Accepts the same `CountSpec` forms: plain integer, `{min, max}`, `{mean, std_dev}`.

`ratio` can be inferred from a declared `rows` on the child dataset (`rows / parent.rows`). If stated `rows > ratio × parent.rows`, that is a planning error.

The driver's `cardinality` is the **hard constraint** on the junction: it unambiguously determines total rows (`driver.rows × ratio × E[cardinality]`). The pool side (see Form 2) is the soft side.

### Form 2 — `include.couple` (the pool)

Pairs the driver with a second dataset, creating an N:M junction. Lives nested inside `include`. For each junction row, exactly one pool row is sampled from the coupled dataset.

The `couple` stanza carries **exactly one** of three mutually exclusive fields — three different parameterisations of how pool rows are distributed across junction rows:

Option A — `ratio`: pool eligibility fraction (soft; total rows driven by `include.cardinality`):
```yaml
couple:
  file: organisations.yaml
  ref: organisations
  ratio: 1.0
```

Option B — `reinforcement`: sampling intensity (soft; total rows driven by `include.cardinality`):
```yaml
couple:
  file: organisations.yaml
  ref: organisations
  reinforcement: 0.5    # see §Reinforcement
```

Option C — `cardinality`: hard right-sided M (each pool row appears M times; determines total rows):
```yaml
couple:
  file: organisations.yaml
  ref: organisations
  cardinality: {min: 3, max: 15}
```

`couple` fields:
- **`file`**, **`ref`** — the pool dataset
- **`ratio`** — fraction of the pool eligible for sampling. Total rows driven by `include.cardinality`. Default 1.0.
- **`reinforcement`** — sampling intensity within the eligible set (see §Reinforcement). Total rows driven by `include.cardinality`.
- **`cardinality`** — hard right-sided M: each pool row appears M times across all junction rows. Total junction rows = `pool.rows × E[M]`. **Mutually exclusive with `include.cardinality`** — when `couple.cardinality` is set, `include.cardinality` must not be.

If none of the three is specified, `ratio: 1.0` is assumed (all pool rows eligible, uniform random).

**`couple.cardinality` validation:**
When the right-sided cardinality is the planning driver, the planner must verify the pool has sufficient rows:

```
total_junction_rows  = pool.rows × E[couple.cardinality]
driver_rows          = driver.rows × include.ratio

require: pool.rows × couple.cardinality.min  ≥  driver_rows   (each pool row appears at least min times)
require: total_junction_rows is achievable   (no stricter constraint without reinforcement=0)
```

For `reinforcement: 0` (without replacement within an outer row): `total_junction_rows ≤ eligible_pool_size`.

**Choosing which side carries the hard cardinality** reflects the business constraint you know precisely:
- "Each individual has 1–6 directorships" → `include.cardinality: {min: 1, max: 6}`, `couple.ratio: 1.0`
- "Each organisation has 3–15 directors" → `couple.cardinality: {min: 3, max: 15}`, no `include.cardinality`

### Form 3 — `content.include` (nested list coupling)

A `list` field with `content.include` is syntactic brevity for a nested junction. The outer row is the implicit driver (`cardinality: 1` per outer row); the included dataset is the pool. The planner generates an inner flat and folds it into a `ListArray`.

```yaml
- name: on_call_doctors
  type: list
  content:
    include:
      file: doctors.yaml
      ref: doctors
      cardinality: {min: 2, max: 5}    # items per outer row
      ratio: 0.33                       # OR: reinforcement: 0.5
    fields:
      - name: doctor_name
        type: string
        refs: doctors.full_name
```

`content.include` fields:
- **`file`**, **`ref`** — the pool dataset
- **`cardinality`** — how many items per outer row. Replaces both `count` and `multiplicity` on nested-include list fields. Same `CountSpec` forms.
- **`ratio`** or **`reinforcement`** — pool sampling; same semantics as `couple`

Vanilla `list` fields (no `content.include`) retain `count` for scalar item counts. This is the only remaining use of `count`.

The equivalence: `content.include` is exactly Form 2 where the outer row is the implicit driver (cardinality 1). The inner flat has `_slot_idx` tracking which outer row and `_pool_idx` tracking which pool row was sampled. When a `collect` binding appears inside `content.fields`, this equivalence is what enables the MULT-2 planning-time junction-table rewrite: the implicit anonymous child is made explicit, with the pool dataset assembled after the inner flat via `CollectToPool`.

---

## Reinforcement

`reinforcement` is a continuous sampling-intensity parameter for the pool side:

| Value | Behaviour |
|-------|-----------|
| `0` | Without-replacement: each eligible pool row sampled at most once per outer row |
| `1` | Uniform random with-replacement (current default) |
| `> 1` | Clumping / preferential attachment: some pool rows appear far more often |

`ratio` and `reinforcement` are dual. Given total junction rows and eligible pool size (`ratio × pool.rows`):

```
expected_appearances_per_pool_row = (total_rows / eligible_pool_size) × reinforcement
```

Specifying `reinforcement: 0` is a hard constraint: `total_rows ≤ eligible_pool_size`. Violation at planning time → error. This replaces the planned `without_replacement: true` boolean with a first-class continuous parameter that also enables clustering.

---

## Sibling segmentation — unchanged

Siblings form when two or more child datasets point to the same parent via their `include.file`. The planner detects these and runs the existing segmentation algorithm (`plan_segments`, IPF). `ratio` replaces `distribution`; algorithm unchanged.

**All siblings participate, including those with `ratio: 1.0`.** A child with ratio 1.0 says "my constraints apply to every row of the parent." Its Bernoulli probability is 1 — it is always present in every segment — but its field constraints must still enter conflict pruning jointly with its siblings' constraints. Excluding a ratio-1.0 child from segmentation would allow its constraints to silently override siblings' constraints via join order rather than correctly zeroing out the conflicting segment. `build_sibling_groups` therefore registers every child of a shared parent unconditionally; effective ratio is 1.0 when not declared.

A child with `include.couple` participates in its driver's sibling group based on `include.ratio`. The pool side (`couple`) is not a segmentation participant; it is sampled per junction row.

---

## Sentinel unification

The prior design had two names for the same concept:
- **`_outer_idx`** — which outer row each inner-flat item belongs to (nested list context)
- **`_slot_idx`** — which driver-row slot each multiplied sibling row belongs to (top-level cardinality context)

Under the unified model, both are **driver-parent slot indices**. They share one name: **`_slot_idx`**. `_outer_idx` is renamed to `_slot_idx` throughout — in `execute_inner_flat`, all produced batches, tests, and documentation. This is a pure rename with no execution-logic change.

Hidden columns in generated batches:
- **`_row_idx`** — positional JOIN key for `grow_parent_from_children` (unchanged)
- **`_slot_idx`** — driver-parent slot index; retained in `computed`, stripped from emitted output by `filter_hidden_columns`
- **`_pool_idx`** — which pool row was sampled; persisted for MULT-2's collect mechanism

---

## Data-quality note (future, out of scope for MULT-1/2/3)

Duplication-as-data-quality lives in a separate top-level `quality` stanza, not in the include machinery:

```yaml
quality:
  inflation: 0.05   # 5% duplicated rows introduced as post-processing
```

---

## YAML vocabulary mapping

| Old | New | Notes |
|-----|-----|-------|
| `includes: [...]` | `include: {...}` | Singular; coupled datasets go in `couple` |
| `distribution` | `ratio` | Same semantics, clearer name |
| `multiplicity` (top-level `includes:`) | `cardinality` on `include` | Identical semantics |
| `multiplicity` (nested list field) | `cardinality` on `content.include` | Identical semantics |
| `count` (nested list field) | `cardinality` on `content.include` | Was an alias; removed |
| `count` (vanilla list field) | `count` | Unchanged |
| `distribution` (nested list) | `ratio` on `content.include` | |
| `content.includes: [...]` | `content.include: {...}` | Singular pool per nested list |
| multiple top-level `includes:` entries | `include:` + `include.couple:` | Driver/pool explicitly encoded |
| `without_replacement: true` (future) | `reinforcement: 0` | Subsumed into continuous parameter |
| `_outer_idx` | `_slot_idx` | Unified sentinel; rename throughout |

---

## Complete examples

### Top-level cardinality only (MULT-1)

```yaml
# workers.yaml
include:
  file: companies.yaml
  ref: company
  ratio: 0.8
  cardinality: {min: 1, max: 5}

data:
  - name: name
    type: string
    generator: name
  - name: employer
    type: string
    ref: company.company_name
```

### N:M junction — directorships (MULT-2 Case 1)

Driver is individuals (we know each individual has 1–6 directorships):

```yaml
# directorships.yaml
include:
  file: individuals.yaml
  ref: individuals
  ratio: 0.1
  cardinality: {min: 1, max: 6}
  couple:
    file: organisations.yaml
    ref: organisations
    ratio: 1.0

data:
  - name: director
    type: string
    refs:
      - individuals.full_name
      - {bind: organisations.directors, reducer: collect}
  - name: company
    type: string
    ref: organisations.company_name
```

`organisations.directors` is a list field prefilled by the `CollectToPool` step after the directorships batch is generated.

### Nested list with collect — wards/doctors (MULT-2 Case 2)

`doctors.yaml`:
```yaml
data:
  - name: full_name
    type: string
    generator: name
  - name: on_call_list
    type: list
    content:
      type: string
    default: []
```

`wards.yaml`:
```yaml
data:
  - name: ward_name
    type: string
  - name: on_call_doctors
    type: list
    content:
      include:
        file: doctors.yaml
        ref: doctors
        cardinality: {min: 2, max: 5}
        ratio: 0.33
      fields:
        - name: doctor
          type: string
          refs: doctors.full_name
        - name: allocated_to
          type: string
          refs:
            - ward_name
            - {bind: doctors.on_call_list, reducer: collect}
```

The `allocated_to` field is structurally redundant in the output; MULT-3's `hidden: true` suppresses it. The planner synthesises a junction-table impl node (child of both wards and doctors), runs `CollectToPool` before assembling doctors, then folds the impl flat into `on_call_doctors`.

---

## Impact on MULT specs

Changes required in MULT-1, MULT-2, MULT-3:

### MULT-1

- Replace `distribution` → `ratio` throughout the spec and all YAML examples.
- Replace top-level `multiplicity` → `cardinality` on `include`.
- Replace nested-list `multiplicity`/`count` → `cardinality` on `content.include`.
- `content.includes: [...]` → `content.include: {...}` (singular, one pool per list).
- Rename `_outer_idx` → `_slot_idx` throughout spec, execution description, and tests.
- Implementation plan: Stage 1 models change is `distribution` → `ratio`, `multiplicity` → `cardinality`, and add `Couple` struct. Stage 2 updates `execute_inner_flat` to emit `_slot_idx` (renamed from `_outer_idx`) and persist `_pool_idx`. All else follows.

### MULT-2

- Rewrite the directorships example to use `include` + `include.couple` (no `includes:` array).
- Rewrite the wards/doctors example to use `content.include` (singular) with `cardinality` and `ratio`.
- Remove the old `refs.bind: organisations.directors` paragraph that assumed two separate `includes:` entries — the coupling is now explicit via `couple`.
- The `CollectToPool` step and `collect` reducer mechanism are unchanged.
- The junction-table rewrite for Case 2 is unchanged; the model just makes the implicit coupling explicit.
- Remove §8 (without-replacement sampling as a boolean) — absorbed into `reinforcement: 0`.
- Update the `_slot_idx`/`_pool_idx` descriptions to reflect the unified sentinel.

### MULT-3

- `include.fields` wildcard and `include.exclude` apply to `include` (and `include.couple`) — same semantics, updated field path.
- `content.include` (singular) replaces `content.includes` for `project_field` — cleaner since there is now only one pool.
- `hidden: true` is unchanged.