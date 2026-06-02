# VAR-EXPAND — Implementation plan

Companion to [`VAR-EXPAND.md`](VAR-EXPAND.md). The spec covers the design, the
terminology, and the function-level contracts; this doc covers PR sequencing,
intermediate test green-points, and risks discovered during planning. It also
folds in the two remaining "next steps" the spec deferred: the
[`VAR-LINKED-CONTENT`](VAR-LINKED-CONTENT.md) validation gate (PR 1) and the
celebratory `variant-lowering.mdx` concepts page (PR 4).

## Status

Planned. The planner-math is already de-risked: the **Q-prototype**
(`lib/segment.rs::mod var_expand_prototype`) proves `M₀ = 0` is preserved by the
real `ipf_rescale_sparse` and that interior IPF converges in ≤14 sweeps. That
test stays as the planner-math regression guard for the life of the feature.

## The one correctness invariant to hold throughout

Each lowered case's ratio is **absolute**: `case_ratio = r_M · vᵢ`, where `r_M` is
the member's include ratio and `vᵢ` is the case's within-union ratio. So a union's
exclusion-group "no case" categorical factor is `1 − Σ case_ratio = 1 − r_M`:

- `r_M = 1` (mandatory member) ⇒ "no case" factor is **0** — the illegal cell the
  categorical factor zeroes. This is the leak case.
- `r_M < 1` (optional member) ⇒ "no case" factor is `1 − r_M` > 0 — the legitimate
  "member absent" cell, *not* illegal.

`ipf_rescale_sparse` then restores each case-member's marginal to `r_M · vᵢ` with
no change to the function (cases are members). The Q-prototype only exercised
`r_M = 1`; PR 2 must add the `r_M < 1` case to the unit tests.

## PR sequencing

Five PRs. The lowering switchover (PR 3) is one atomic change — until the planner
emits cases-as-members, the executor still needs the VAR-2 generation-time path;
once it does, that path is dead. Splitting the switchover leaves two live variant
paths. PR 2 lands all the new machinery *dormant* (no caller produces groups, so
behaviour is byte-identical), so PR 3 is a focused flip.

| PR | Subject | Scope |
|----|---------|-------|
| 1 | Lock target behaviour + `VAR-LINKED-CONTENT` gate | New fixtures/tests (`#[ignore]` where they need lowering) + validation rejection of variants on linked content lists |
| 2 | Dormant machinery | `models.rs` types; `segment.rs` exclusion-group-aware DFS + categorical factor + discriminant constraint; `plan_segments` signature change (all callers pass `&[]`) — behaviour unchanged |
| 3 | The lowering switchover | `plan.rs` `lower_variant_unions`; executor discriminant handling; delete VAR-2 variant sub-distribution; remove `#[ignore]` |
| 4 | Docs + terminology adoption | `variant-lowering.mdx` (celebratory), CLAUDE.md glossary/module-map/sentinels, README, testing.mdx, docgen |
| 5 | Close-out | Mark specs complete, move to `specs/done/`, re-point VAR-SPECIALIZE |

**Revert safety (a convention worth keeping).** The dormant/flip split is chosen so
that exactly one commit changes behaviour. PRs 1–2 are behaviour-neutral (new
tests, rejection of an already-broken config, and machinery no caller invokes), and
PRs 4–5 are docs/moves. So **PR 3 is the only revertable behaviour commit**: if a
statistical regression surfaces post-merge, `git revert <PR3>` restores VAR-2
behaviour cleanly without unwinding the types or the DFS refactor. Keep PR 3 a
single squash-merge for this reason.

### PR 1 — Lock target behaviour + the VAR-LINKED-CONTENT gate

**Pre-check.** Grep `examples/` and `tests/fixtures/` for `type: variant` item
fields under a `links:`/`content:` stanza. If any exist, they are currently
mis-handled (Q4) — convert or remove them before adding the gate. Expectation:
none exist.

**Validation gate (`lib/validate.rs`).** Reject a `type: variant` field that
appears among the content (item) fields of a linked content list:

> variants on linked content lists are not yet supported (see VAR-LINKED-CONTENT)

- `tests/validate_tests.rs` (or inline) — a fixture/builder with a list-link whose
  item field is `type: variant`; assert the error fires. Lands green immediately.

**Lock-in tests for lowering (mostly `#[ignore]` until PR 3).**

- `tests/fixtures/execute/variant_lowering/` — a parent with a leaf member carrying
  a 2–3-case **mandatory** union, plus a sibling member chosen so a non-trivial
  joint segment exists. Tests in `tests/executor_tests.rs`:
  - `variant_case_membership` — every member row's case value is in the declared
    set. *(May already pass under VAR-2; if so, not ignored.)*
  - `mandatory_union_no_orphan` — no parent row that belongs to the mandatory
    member lacks a case value. **`#[ignore]` until PR 3** (VAR-2 can leave the
    proportional-rounding gap; lowering makes it exact).
  - `joint_case_segment_counts_are_ipf_exact` — per-`(case, segment)` row counts
    match the IPF expectation, not VAR-2's proportional split. **`#[ignore]` until
    PR 3.**
- Statistical (`tests/statistical/test_insurance.py`): the existing variant
  membership + distribution tests are the main regression guard (insurance's
  `premiums`/`claims` are leaf-member unions). Optionally add a joint-proportion
  assertion, `#[xfail]`-style skipped until PR 3.

### PR 2 — Dormant machinery

Everything compiles and the full suite stays green because no caller produces a
non-empty `ExclusionGroup` yet, and the categorical factor over zero groups equals
today's product-Bernoulli prior.

1. **`lib/models.rs`** — add:
   - `LoweredCase` (case schema + `case_ratio = r_M·vᵢ` + owning group id +
     discriminant index), or carry the equivalent on `LowerCoverMember`.
   - `ExclusionGroup { discriminant: String, members: Vec<usize>, ratios: Vec<f64>, mandatory: bool }`
     — `members` index into the lower cover's member vector; `ratios` are absolute
     `r_M·vᵢ`; `mandatory == (Σ ratios ≈ 1)`. **One group per union *field*.**
   - `discriminant_column(union_field: &str) -> String` → `_disc_<union>`, and a
     `DISCRIMINANT_PREFIX` const next to the other sentinels.

2. **`lib/segment.rs`** — refactor the DFS to be entry-based and add the factor:
   - `enumerate_segments_dfs` iterates a list of **entries**, where an entry is
     either a lone member or an `ExclusionGroup`. A group branches over
     `{no case} ∪ {pick case j}` (≤1 group member set per branch); a lone member
     branches include/exclude as today. Weights use `categorical_prior_factor`.
   - `categorical_prior_factor(group, chosen: Option<usize>) -> f64` — `ratios[j]`
     for a chosen case, `1 − Σ ratios` for none.
   - `discriminant_constraint(idx) -> FieldConstraints` — pins `_disc_<union>` to a
     case index (the `meet = ⊥` encoder); `allowed_values` form reserved for
     VAR-SPECIALIZE.
   - `plan_segments(parent_rows, members, groups: &[ExclusionGroup])` — **signature
     change**; `ipf_rescale_sparse` **unchanged**. With `groups == &[]` the
     entry list is all-lone-members → byte-identical to today.
   - Extend `mod var_expand_prototype` (or add unit tests) to cover `r_M < 1`
     (optional member: "no case" = `1 − r_M` is legitimate, not driven to 0).

3. **Callers** — `plan.rs::build_lower_cover_groups` and every test/fixture caller
   pass `&[]`. Trivial wrapping ripple.

Green-point: full `cargo test` + `pytest` unchanged from pre-PR counts.

### PR 3 — The lowering switchover (atomic)

1. **`lib/plan.rs` — `lower_variant_unions`.** The lowering pass. For every node
   with `variants:` (top-level *and* leaf member), produce one lowered case per
   union value (one **exclusion group per union field**; multiple union fields give
   `n + m` cases across two groups, **not** `n·m`). Inject the `_disc_<union>`
   parent column; give each case a `ref: parent._disc_<union>, value: <idx>` field
   so `lower_cover_field_constraints` extracts it for conflict pruning. Unify with
   `plan_variant_steps` so top-level and leaf-member unions lower through one path;
   emit `LoweredCase`s + `ExclusionGroup`s into the lower-cover group and thread the
   groups into `plan_segments`.

2. **`lib/executor.rs`:**
   - Discriminant sentinel: skip `_disc_<union>` in `build_segment_atom_schema`
     materialisation (or materialise-then-strip) and strip it in
     `filter_hidden_columns` — same lifecycle as `_slot_idx`.
   - **Output accumulation:** lowered cases of one union accumulate to the single
     original-member output path; the segment's discriminant selects which case
     schema `project_member_columns` generates. No `CombineVariantBatches`-style
     step for leaf-member cases.
   - **Cardinality:** the case is chosen once per slot and replicated across the
     `M_n` replicas (node-level property) — verify against current
     non-ref-under-cardinality handling.
   - **Delete** the variant filtering / proportional-split / concat inside
     `generate_member_nonref_fields` (cases now carry concrete schemas); it reduces
     to "generate this case's non-ref subset" or disappears.

3. **Remove `#[ignore]`** from the PR-1 lowering tests.

Green-point: `cargo test` (Rust) + `pytest` (statistical). Insurance variant
membership/distribution tests hold; the new no-orphan and IPF-exact joint-count
tests now pass.

### PR 4 — Docs + terminology adoption

Ships with PR 3's behaviour (per the CLAUDE.md "docs in the same change" rule);
keep it a separate PR only for review clarity, landing together.

- **`docs/src/content/docs/concepts/variant-lowering.mdx`** — "Variants as tagged
  unions." Explains, for users: a `type: variant` field *is* a tagged union; the
  planner *lowers* it into cases; the *discriminant* records the active case and
  makes the cases mutually exclusive (`meet = ⊥`); so variants ride the same
  Bernoulli-factoring machinery as everything else — no special path. Include the
  leak → categorical-prior insight as a short "why this is correct" aside
  (celebrate it). Add to the Concepts sidebar in `astro.config.mjs`.
- **`CLAUDE.md`** — glossary: *tagged union, lowering, lowered case, discriminant
  (tag), exclusion group, illegal mass, linked content list*. Add lowering as a
  named stage to the execution-pipeline list; add `lower_variant_unions` to the
  module map; add `_disc_<union>` to the sentinel conventions; delete the
  variant-handling "two paths" framing and the VAR-2 deferred-path description.
- **`README.md`** — one line: variants are tagged unions lowered into the lattice.
- **`docs/.../reference/testing.mdx`** — document the new invariants (case
  membership & exclusivity; mandatory-union no-orphan; IPF-exact joint counts).
- **`src/docgen.rs` / `reference/yaml-schema.mdx`** — no new YAML; note the
  tagged-union model in prose.

### PR 5 — Close-out

- Mark `specs/VAR-EXPAND.md` and `specs/VAR-EXPAND-impl.md` **Complete**, move to
  `specs/done/`; update the feature-spec table in `CLAUDE.md`.
- Move `mod var_expand_prototype` note from "Q-prototype" to a permanent
  "planner-math regression guard" mention.
- Re-point `specs/VAR-SPECIALIZE.md`: its substrate (lowering + discriminant) now
  exists; update its status from "depends on (unbuilt) VAR-EXPAND".
- Leave `specs/VAR-LINKED-CONTENT.md` as the open future stub (its gate now lives
  in `validate.rs`).

## Traceability matrix

Every design claim in the spec maps to a guarding test and the PR that makes it
true. (A convention to carry forward: a spec claim with no row here is a claim with
no test.)

| Invariant (spec claim) | Guarding test | Lands |
|------------------------|---------------|-------|
| Categorical prior gives `M₀ = 0`, preserved by IPF | `var_expand_prototype::categorical_prior_zeroes_illegal_mass_and_restores_marginals` | ✅ done |
| Interior IPF converges fast on stacked mandatory unions | `var_expand_prototype::stacked_mandatory_unions_with_conflicts_converge_fast` | ✅ done |
| Naive prior leaks (justifies the categorical factor) | `var_expand_prototype::naive_prior_leaks_under_low_iteration_cap` | ✅ done |
| Case ratios are absolute `r_M·vᵢ`; "no case" = `1−r_M` | new `r_M < 1` unit test in `mod var_expand_prototype` | PR 2 |
| Each case is in the declared value set | `executor_tests::variant_case_membership` | PR 1 (may pass pre-flip) |
| Mandatory union ⇒ no orphan (every member row has a case) | `executor_tests::mandatory_union_no_orphan` | PR 3 (ignored → on) |
| Per-`(case, segment)` counts are IPF-exact, not proportional | `executor_tests::joint_case_segment_counts_are_ipf_exact` | PR 3 (ignored → on) |
| One group per union field (cross-product via segments) | new two-union-field unit test (members `== n+m`) | PR 3 |
| Cases share one Arrow schema (else error) | new validation test | PR 3 |
| `_disc_<union>` never reaches output | `filter_hidden_columns` test | PR 3 |
| Variants on linked content lists rejected | `validate` rejection test | PR 1 |
| Top-level + member variant distributions unchanged | insurance / corporate-registry statistical suites | PR 3 |

## Risks discovered during planning

1. **Case ratios must be absolute (`r_M·vᵢ`).** The single most likely bug: using
   `vᵢ` directly for an *optional* member would make the union look mandatory and
   wrongly zero the legitimate "member absent" cell. Mitigation: the PR-2 `r_M < 1`
   unit test; assert the "no case" mass equals `1 − r_M`.

2. **One group per union field, not per combination.** A node with two union fields
   must lower to `n + m` cases / two groups, with the `∏` cross-product emerging as
   segments. Mitigation: a unit test with two union fields asserting member count
   `== n + m` and that all `n·m` value-combinations appear across segments.

3. **Heterogeneous case schemas.** Accumulating all cases to one output path
   assumes they share one Arrow schema (VAR-2 already relies on this for `concat`).
   A variant that *adds/removes* a field breaks it. Mitigation: validate that all
   cases of a union share an identical field set; error otherwise (closes the
   unguarded VAR-2 assumption noted in CLAUDE.md).

4. **Top-level union regression.** Unifying `plan_variant_steps` into
   `lower_variant_unions` must not regress top-level variant datasets. Mitigation:
   the corporate-registry / insurance examples exercise top-level + member variants;
   run both statistical suites. Consider keeping `plan_variant_steps` as a thin
   adapter initially and unifying in a follow-up if the diff gets risky.

5. **Discriminant entering import taint / segment-atom paths.** `_disc_<union>` is
   synthetic and must be skipped by the atom-schema builder and never treated as an
   imported/tainted column. Mitigation: a `filter_hidden_columns` test asserting it
   never reaches output; reuse the `_slot_idx` handling sites as the checklist.

6. **`MAX_FEASIBLE_SEGMENTS` interaction.** Lowering increases member count, but the
   discriminant prunes intra-union co-occurrence so feasible segments stay bounded.
   Confirm the cap message still makes sense and add a stacked-union test near the
   cap (the Q-prototype's enumeration is the reference).

## Estimated diff size

| PR | Approx LoC |
|----|------------|
| 1 | +120 (fixtures + tests + gate) |
| 2 | +250 (types + entry-DFS + factor), behaviour-neutral |
| 3 | −200 / +250, net ≈ +50 (lowering in, VAR-2 variant path out) |
| 4 | +200 (mdx page + glossary/docs prose) |
| 5 | ~40 across spec/table moves |
