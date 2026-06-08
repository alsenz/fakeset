# fakeset — Claude Code guide

## What this repo is

A declarative, DAG-structured synthetic dataset generator. Users write YAML schemas; fakeset generates Parquet/CSV/JSON/JSONL output. The core challenge is producing referentially consistent data across a graph of related datasets — solved by generating children (the more-constrained datasets) first, then assembling each parent's rows from those already-solved child rows.

## Build and test

```bash
cargo build                  # debug
cargo build --release        # release binary → target/release/fakeset
cargo test                   # all unit + integration tests (~301 tests)
cargo check                  # fast type-check without linking
```

Run an example:
```bash
cargo run --bin fakeset -- examples/corporate-registry --output ./output/corporate-registry
cargo run --bin fakeset -- examples/insurance --output ./output/insurance
```

Output goes to `./output/` (gitignored).

### Statistical regression tests

A Python pytest suite in `tests/statistical/` runs both examples end-to-end and checks
distributional correctness using polars and scipy:

```bash
# install Python deps (one-time)
pip install pytest polars scipy

# run all statistical tests (~74 tests)
pytest
```

`pytest.ini` at the repo root points pytest at `tests/statistical/`.

Two test files — `test_insurance.py` and `test_corporate_registry.py` — share session-scoped
fixtures in `conftest.py` (builds the release binary once, runs each example once, loads
Parquet files into DataFrames).

**Hard invariants** (always true): numeric range bounds, variant value membership, referential
integrity, expression formula correctness, list cardinality, lower-cover partition identity.

**Soft invariants** (statistical, α=0.01): include ratios (binomial test), variant frequency
distributions (chi-squared goodness-of-fit), numeric value distributions (KS test vs uniform).
Tests auto-skip when sample size is too small for the chosen statistic.

### Complexity analysis (for quality audits and simplicity refactors)

When auditing code quality or planning simplicity refactors, get a per-function complexity
signal with a quick scripted proxy — line span, a cyclomatic count (decision points:
`if`/`match`/`while`/`for`/`&&`/`||`/`?`/`=>`), and max brace-nesting depth. Combine all three:
length alone overstates *sequential* phase functions, and cyclomatic alone flags flat dispatch
matches (`fake_string`, a `Display::fmt`) that carry no real cognitive load — the nesting-depth
axis is what separates genuinely-tangled logic from long-but-flat code. Treat metrics as a guide,
not a hard threshold. (The external `rust-code-analysis-cli` is *not* used — it fails to compile
on current rustc; a one-off awk pass over `lib/*.rs` gives the same signal without adding repo
tooling.)

The 2026-06 audit found the complexity concentrated in `validate.rs` (`validate_field` 258L/cx85,
`validate_dataset` 271L, `validate_args` depth-6); these have since been **decomposed** into thin
orchestrators over per-concern `check_*`/`validate_*` helpers (no helper now exceeds ~120L and max
nesting dropped to 4). The executor's large functions (`execute`, `execute_lower_cover_group_core`,
`execute_accumulate_to_linked`) are *long but shallow* — sequential phase pipelines (low nesting,
low branch density), so their line count overstates their cognitive cost; decomposing them was
judged low-ROI and deliberately skipped. `plan_segments` is no longer a hotspot (the DFS was
extracted to `enumerate_segments_dfs` in SEG-1).

## Documentation site

The docs live in `docs/` and are built with [Astro Starlight](https://starlight.astro.build).

### Running and building

```bash
cd docs
pnpm run dev        # dev server at http://localhost:4321 with hot reload
pnpm run build      # production build → docs/dist/
pnpm run preview    # preview the production build locally
```

`pnpm run build` automatically runs `scripts/gen-schema.mjs` first (the `prebuild` hook), which executes `cargo run --bin docgen` to regenerate `docs/src/data/schema.json`. The `docgen` binary (`src/docgen.rs`) serialises all YAML-deserializable types to a hand-crafted JSON description.

### Page map

| File | Content |
|------|---------|
| `docs/src/content/docs/index.mdx` | Introduction / home page |
| `docs/src/content/docs/getting-started.mdx` | Installation, first schema, quick-start |
| `docs/src/content/docs/examples/corporate-registry.mdx` | Corporate-registry example walk-through |
| `docs/src/content/docs/examples/insurance.mdx` | Insurance example walk-through |
| `docs/src/content/docs/concepts/semi-lattice.mdx` | Concept semi-lattice model |
| `docs/src/content/docs/concepts/execution-pipeline.mdx` | 11-stage pipeline, ExecutionStep types, sentinel columns, import specialisation restrictions |
| `docs/src/content/docs/concepts/bernoulli-factoring.mdx` | Lower cover segmentation algorithm |
| `docs/src/content/docs/concepts/list-links.mdx` | Staging/witness/assembly pipeline, collect bindings |
| `docs/src/content/docs/reference/yaml-schema.mdx` | Complete YAML field reference (static MDX) |
| `docs/src/content/docs/reference/expressions.mdx` | Expression-language reference: DataFusion SQL scoping rules, function families, and the fakeset `array_normalize*` list UDFs / `normalize:` sugar |
| `docs/src/content/docs/reference/generators.mdx` | All generators grouped by category, locale matrix |
| `docs/src/content/docs/reference/cli.mdx` | CLI flags and examples |

The sidebar order and section labels are configured in `docs/astro.config.mjs`.

### Keeping docs current

When you add or change features, update the docs in the same PR:

- **New YAML field on an existing type** → update `src/docgen.rs` (add the field to the relevant `FieldDoc` list) AND update the corresponding table in `docs/src/content/docs/reference/yaml-schema.mdx`.
- **New top-level type** → add a `TypeDoc` entry in `src/docgen.rs` AND add a new `##` section with an example in `yaml-schema.mdx`.
- **New generator or locale** → update `docs/src/content/docs/reference/generators.mdx`.
- **New CLI flag** → update `docs/src/content/docs/reference/cli.mdx`.
- **New architectural concept or execution step** → add or extend a page under `docs/src/content/docs/concepts/`. Add it to the sidebar in `astro.config.mjs` if it's a new page.
- **New or updated example** → update the corresponding page under `docs/src/content/docs/examples/`.
- **Renamed terminology** → update the glossary in both this file and the relevant concepts page.

### Assets and styling

- Logo SVG: `docs/src/assets/logo.svg` (Hasse diagram in a cog shape; referenced by Starlight header and the intro hero).
- Custom CSS: `docs/src/styles/custom.css` — table column no-wrap rules, accent colour palette, hero image flip.
- **Starlight CSS variable quirk**: Starlight uses `:root` for dark-mode defaults and `:root[data-theme='light']` for light-mode overrides (opposite of the usual convention). To reliably override accent colours, use `html:root` for dark and `html:root[data-theme='light']` for light — this beats Starlight's specificity regardless of stylesheet bundle order.

## Glossary

These terms have precise meanings in this codebase — use them consistently.
The full theoretical framing is in `specs/done/REFRAME-1.md`.

| Term | Meaning |
|------|---------|
| **concept semi-lattice** | The partial order over all datasets where `A ≤ B` means "A is a more-constrained subset of B's population". Every pair of datasets with a common ancestor has a meet (greatest lower bound). |
| **element / node** | One member of the semi-lattice. "Node" is preferred when emphasising graph structure; "element" for order-theoretic properties. |
| **⊥ (bottom)** | The empty concept — the unsatisfiable constraint set. Bernoulli segments that prune to zero rows represent ⊥ and are dropped. |
| **atom** | An element that covers ⊥ directly — the most-constrained node in a component. Atoms are generated first. |
| **parent** (parent-by-inclusion) | A dataset that is *included by* another — the less-constrained, broader population. |
| **child** (child-by-inclusion) | A dataset that *includes* another — the more-constrained, narrower population. |
| **lower cover** | The set of elements that directly include a given parent element (formerly "siblings"). |
| **lower cover group** | A parent together with its lower cover; planned as a unit via Bernoulli factoring (formerly "sibling group"). |
| **segment** | One subset of a parent element's rows that belongs to a particular combination of lower cover members. |
| **tagged union** | A `type: variant` field — a node that is exactly one of N **cases** per row; the cases partition the population (their pairwise meet is ⊥). |
| **case** | One concrete alternative of a tagged union (one `variants:` entry), with its own value/generator and ratio. |
| **lowering** (variant lowering) | The planner-time rewrite (`lower_member_variants` in `plan.rs`) that replaces a lower-cover member's tagged union with one **case-member** per case (concrete schema, absolute ratio `r_M·vᵢ`, shared output) plus an `ExclusionGroup`. Only leaf members are lowered; members that are also parents are generated by their own step. Only **constraint-bearing** variants (case-3 `ref` + `variants`) reach lowering; plain same-type variants generate per-row (VAR-UNIFY Phase 2). |
| **exclusion group** | The set of case-members from one lowered union, factored as a single **categorical** DFS entry (`N+1` branches: no-case, or exactly one case) so mutual exclusion is *structural*. |
| **discriminant** (tag) | The conceptual case-identity of a tagged union. Exclusivity is enforced structurally in the DFS, so **no discriminant column is materialised today** — a materialised tag is reserved for future child-specialisation (VAR-SPECIALIZE). |
| **illegal mass** | Prior weight a categorical exclusion group would put on "no case of a *mandatory* union" — exactly 0 by construction (the `1 − Σvᵢ` factor), so a mandatory member never orphans a row. |
| **heterogeneous union** | A `type: variant` whose cases span more than one type — or carry different object schemas (VAR-1). Unlike a same-type variant (which generates per-row, or lowers via the lattice when constraint-bearing), it generates as an Arrow **`DenseUnion`** (`FieldType::Union` + `Field::union_cases`); the per-row `type_id` is its intrinsic discriminant. At write time `unionize_for_output` converts it to a **nullable-superset struct** (one nullable sub-field per case, exactly one populated per row). Parquet/JSON/JSONL only — CSV is gated at validation. |
| **staging node** | A virtual node that holds the scalar non-list fields of a source dataset while its witness and assembly nodes are being built. No output file. |
| **witness node** | An atom carrying the linked dataset's schema. One witness row per unique linked-row draw. A hidden `_staging_refs: List<UInt32>` column maps each witness row back to the staging source slots that drew it. |
| **assembly node** | A virtual node above the staging node that folds witness rows into list columns, evaluates expressions, and emits the final output. |
| **source slot** | One row of a staging batch, identified by `_slot_idx`. |
| **linked dataset** | The target of a `links:` stanza (formerly "pool dataset"). |
| **linked content list** | A `links:` list field whose per-item (`content:`) fields are drawn from a linked dataset. (≠ "linked list".) Variants on its item fields are not yet supported — see `specs/VAR-LINKED-CONTENT.md`. |
| **seed edge** | The execution edge from linked dataset atoms to the witness node — the draw that populates witness rows from the linked dataset. |
| **inherited field** | A column pre-populated from an already-computed child batch into the parent's batch, wiring up ref fields so they are never regenerated (formerly "prefill"). |
| **preceding** (preceding-by-execution) | Generated first. Atoms are always preceding. |
| **subsequent** (subsequent-by-execution) | Generated after. Parents and assembly nodes are always subsequent. |

## Core architectural framing

fakeset is built around a **concept semi-lattice**: a partial order where `A ≤ B` means "dataset A is a more-constrained subset of B's population". An `include:` stanza expresses constraint specialisation — not data dependency. A child is a narrower, more-constrained cut of its parent's population. A `links:` stanza introduces a *linked dataset* — a target from which list items are drawn per outer row, governed by the witness/assembly pipeline.

This framing is specified in full in `specs/done/REFRAME-1.md`. In brief: every dataset is a node in the semi-lattice; every pair of nodes with a shared ancestor has a **meet** (greatest lower bound); the most-constrained nodes — those covering ⊥ directly — are **atoms**, generated first.

The algorithm has two symmetric phases:

1. **Push down** — field definitions, type constraints, and ref bindings propagate *down* the lattice toward atoms. For linked datasets, the staging node pre-generates scalar fields before the witness (atom) is generated.

2. **Accumulate up** — generated atom values propagate *up* the lattice toward parents and linked nodes. For include relationships this is the segment-atom pipeline in `execute_lower_cover_group_core`: `generate_segment_atom_batch` materialises shared ref columns once per segment, then `project_parent_columns_from_atom` and `project_member_columns` fan those columns to the parent and each member. For collect bindings this is `AccumulateToLinked` — the symmetric operation that accumulates atom-level values back into linked-dataset fields.

The DAG is a hierarchy of ever-narrowing constraints, sorted topologically so atoms are always generated before their parents. Within each segment, fields that two or more members ref are deduplicated into a single atom column; the parent and every member that refs that field receive the same generated values, guaranteeing referential integrity structurally. Parent fields with no member-ref source are generated fresh by `project_parent_columns_from_atom`.

*Theoretical note:* all generator invocations could conceptually happen in parallel — the algorithm's serialisation is purely a scheduling constraint imposed by the inherited-field lattice. The interesting work is resolving which pre-solved values propagate to which nodes and in what order.

## Lower cover segmentation (Bernoulli factoring)

When two or more datasets include the same parent they form the parent's **lower cover** and their rows must be partitioned consistently. All lower cover members participate — including those with `ratio: 1.0` — because their field constraints must enter conflict pruning jointly. This is the exponential-explosion problem: N lower cover members → 2^N possible membership subsets.

`segment.rs::plan_segments` controls the explosion with three steps:

1. **Product-Bernoulli prior** — enumerate all 2^N subsets, weight each by the product of marginal (in/out) probabilities.
2. **Conflict pruning** — zero out any subset whose field constraints are mutually contradictory (e.g. two lower cover members both pinning `status` to different constants). Rows from zeroed subsets are redistributed to surviving subsets.
3. **IPF (Iterative Proportional Fitting)** — scale the surviving weights so declared marginals are exactly restored (skipped when nothing was pruned), then apply **largest-remainder (Hamilton) rounding** to integer row counts (unbiased + total-conserving).

Enumeration uses a branch-and-bound DFS over an entry plan. A plain member branches two ways — exclude (`× 1−ratio`) then include (`× ratio`, rejected if it conflicts with an already-included member). A **variant `ExclusionGroup`** (a lowered tagged union — see Variants below) branches categorically: *no case* (`× 1−Σvᵢ`, pruned for a mandatory union) or exactly one case (`× vᵢ`), so union cases are mutually exclusive by construction. The `MAX_FEASIBLE_SEGMENTS` cap (1,000,000 segments) is a K-based safety valve — it bounds the number of *surviving* feasible segments, not the raw 2^N enumeration space, so arbitrarily large lower-cover groups are handled as long as the feasible set stays small (segment masks are `FixedBitSet`, so there is no member-count ceiling). IPF and largest-remainder rounding operate on the sparse feasible set only.

**Variant lowering (VAR-EXPAND).** A `type: variant` field is a **tagged union**. A plain same-type variant generates **per-row** (categorical; VAR-UNIFY Phase 2). A **constraint-bearing** variant — a lower-cover member's case-3 `ref` + `variants` — is lowered by `lower_member_variants` into one case-member per case (concrete schema, absolute ratio `r_M·vᵢ`, shared output) plus an `ExclusionGroup`, so it rides the Bernoulli machinery above with no separate path. Members that are also parents are not lowered (generated by their own step). Mutual exclusion is structural in the DFS — no discriminant column is materialised. See `docs/.../concepts/variant-lowering.mdx`.

## Execution pipeline

Each run follows this fixed sequence:

```
load YAML files
  → load_import_headers        (read schema + row count from each import: file; prepend tainted fields)
  → build_dag          (petgraph DAG, topo-sort, cycle detection)
  → pull_down_expression_deps  (push hidden ref fields DOWN the lattice: inject expression deps declared only in an included parent)
  → validate           (structural checks, ref validity, constraint consistency, import taint checks)
  → expand_field_variants      (heterogeneous → DenseUnion; constraint-bearing ref+variants → mark `constraint_bearing` for the planner to lower; same-type → per-row marker)
  → expand_include_fields      (materialise `include.fields` wildcard copies as explicit ref fields)
  → resolve_refs       (push field types and merged constraints DOWN the lattice to child/ref targets)
  → apply_global_locales       (stamp locale onto locale-aware fields)
  → build_plan         (resolve row counts, lower cover groups, ring assignment, inherited-field wiring, collect targets → ExecutionPlan)
  → execute            (generate atoms leaf-first; accumulate values UP the lattice; write output files)
```

`build_plan` produces a flat list of `ExecutionStep` variants:

- `GenerateDataset` — dataset with no list links; generates, evaluates, and emits in one step.
- `GenerateStagingNode` — dataset with list links; generates scalar batch only, stores in `computed`, no expression evaluation, no emit.
- `GenerateLowerCoverGroup` — parent + lower cover planned together via Bernoulli factoring; parent emits directly when it has no list links.
- `GenerateStagingLowerCoverGroup` — staging counterpart of `GenerateLowerCoverGroup`; parent has list links, so emit is deferred to `AssembleFromWitness`.
- `GenerateWitness` — generates the witness batch (one row per source-slot × linked-row draw).
- `AssembleFromWitness` — folds witness batches into `ListArray` columns, evaluates expressions, emits the final output.
- `AccumulateToLinked` — accumulates atom-level values into linked-dataset fields (collect bindings); followed by `EmitDataset` for the updated linked dataset.
- `WriteSharedOutput` — union + shuffle all accumulated batches for a shared output file, write once.

## Module map

| Module | Responsibility |
|--------|---------------|
| `lib.rs` | Public API: `load_all_datasets`, YAML discovery |
| `models.rs` | All data types (`SyntheticDataset`, `Field`, `Include`, `ImportSpec`, `RingBounds`, `Schema`, `UnionCase`, …) and `FieldType` (incl. the internal `Union` marker — VAR-1). `SyntheticDataset` is `#[serde(deny_unknown_fields)]` (rejects typos + the retired top-level `variants:` at load). Internal `#[serde(skip)]` `Field` markers: `imported_taint` (IMPORT) and `constraint_bearing` (a `ref`+`variants` case-3 field, set by `expand_field_variants`, lowered by the planner). Also `resolve_distributions`, `eligible_linked_rows`, the list-link visitor, and lattice-traversal helpers. *Gotcha:* `Field` is not `PartialEq` (so `FieldType::Union` is a marker + `Field::union_cases`, not a data-carrying variant). |
| `graph.rs` | `build_dag` — petgraph DAG construction and topo-sort |
| `import.rs` | `load_import_headers` (schema + row-count header pass), `load_import_index` (full file load + hash array), `filter_ring` (ring-bounds row filter), `imported_column_names` |
| `list_norm.rs` | LIST-NORM. The `array_normalize`/`array_normalize_field` scalar **UDFs** (hand-rolled `ScalarUDFImpl` + `return_field_from_args` for value-dependent output type; integer path via `segment::largest_remainder`), `register_list_udfs` (called in `evaluate_expressions`), and the `desugar_normalize` pass (rewrites `normalize:` → hidden `<name>__norm_src` + injected `array_normalize*` expression, after `validate`). |
| `validate.rs` | Schema validation: structural rules, ref checks, expression ordering, import taint checks (`check_import_taint`), `normalize:` block checks (`validate_normalize`) |
| `expand_variants.rs` | Route `type: variant` fields (`expand_field_variants`). **Heterogeneous** → `lower_heterogeneous_unions` → `FieldType::Union` + `union_cases` (VAR-1). **Constraint-bearing** (`ref` + `variants`) → `finalize_variant_fields` sets the `constraint_bearing` marker (cases kept) so the planner lowers it — the cross-product itself lives in `plan.rs`, not here. **Same-type, no ref** → unified concrete type, cases kept for per-row generation (VAR-UNIFY Phase 2). `is_heterogeneous`/`infer_field_type`/`unified_variant_type`/`merge_delta_into` (shared, also used by `validate.rs`/`generator.rs`/`plan.rs`) |
| `expressions.rs` | `pull_down_expression_deps`, identifier extraction for validation |
| `rewrite.rs` | `resolve_refs` (ref chain resolution, constraint merging), `expand_include_fields` (wildcard field copying), `apply_global_locales`, `apply_locale_to_schema`. Both ref-field builders (`resolve_field`, `resolve_list_link_content_field`) construct the resolved field via the single `merged_ref_field` helper — *the* home for "ref resolution = inherit base type + merged value-source + propagated carrier (case-3 guard)"; callers add only `expression` or `refs` |
| `constraints.rs` | `FieldConstraints`, `Satisfiable`, `Merge`, `validate_field_constraints`. **Value-source spectrum** (VAR-SPECIALIZE): `generator`/`one_of`/`value` form one spectrum (tightest wins, supports intersect; `value`+`generator` is *not* a conflict). `one_of` = finite-set support selector; `case_overrides` (`Vec<CaseDelta>`) carries per-case `constrain_cases` deltas through merge (concat; ignored by pruning) |
| `segment.rs` | `plan_segments` — Bernoulli weights, conflict pruning, IPF, **largest-remainder rounding**. Entry-based DFS (`DfsEntry::{Lone, Group}`) where a variant **`ExclusionGroup`** branches categorically; `categorical_prior_factor`. `LowerCoverMember`/`ExclusionGroup`/`Segment` types; segment masks are `SegMask = FixedBitSet` (no member-count ceiling). `assign_ring_slices` — tiles parent ring across segments proportionally. |
| `plan.rs` | `build_plan` — row counts, lower cover groups, ring assignment, inherited-field wiring, collect targets → `ExecutionPlan` / `ExecutionStep`. **Owns variant lowering:** `collect_variant_paths` (finds `constraint_bearing` fields) + `build_local_combinations` (cross-product → `CaseCombination`s) + `build_delta_field` + `lower_member_variants` lower a leaf member's constraint-bearing variants into case-members + `ExclusionGroup`s (skips members that are also parents). `subdivide_for_pinned_variants` — VAR-SPECIALIZE S4c `preserve_marginal`: subdivides segments by a pinned variant's cases via 2-D IPF + Gale–Hoffman feasibility check. (Plain same-type field variants generate per-row — no planner step.) |
| `schema.rs` | `schema_to_arrow`, `field_to_arrow`, `parquet_datatype_to_arrow` — Arrow schema conversion. `FieldType::Union` → Arrow `DenseUnion` of the case types (VAR-1) |
| `generator.rs` | `generate_column`, `sample_count` — per-field fake data generation via fake-rs; handles `type: object` by recursively generating sub-fields into a `StructArray`. `fake_date`/`fake_datetime` handle `after`/`before` bounds; `fake_string` threads `args` to range-bearing generators (`Sentence`, `Paragraph`, `Password`, `Words`, `Sentences`, `Paragraphs`, `Geohash`, `NumberWithFormat`); `locale_fake_join!` macro handles generators that return `Vec<String>`. `build_union_column` builds a `DenseUnion` for `FieldType::Union` — each case generated through its own `Field` (VAR-1). `build_same_type_variant_column` generates a same-type variant **per-row** (categorical draw + `interleave`) when a field carries `variants:` (VAR-UNIFY Phase 2). Both share `draw_categorical` — the per-row categorical-draw kernel (resolve + renormalise-over-survivors → `(case_of_row, counts)`); they differ only in *assembly* (DenseUnion type_ids/offsets vs `interleave`). |
| `executor.rs` | `execute` — interprets the plan; staging node generation, witness generation, assembly, segment-atom pipeline (`generate_segment_atom_batch` + `project_parent_columns_from_atom` + `project_member_columns`), `AccumulateToLinked`. Import branch: loads `ImportIndex` on first access, applies ring filter, appends synthetic fields. All DataFusion and Arrow batch operations. (Write-time output encoding lives in `output.rs`.) |
| `output.rs` | The **write-time** encoding layer — turning a computed batch into bytes is a *deserialisation concern*, kept out of the generation core. `write_output` (Parquet/CSV/JSON/JSONL dispatch); `prepare_output_batch` applies `flatten` (`flatten_column`/`flatten_union_to_columns`) then converts any remaining `DenseUnion` to a portable nullable-superset struct via `unionize_for_output`/`union_to_portable` (VAR-1 — no Arrow writer serialises a union, ARROW-8817); `filter_hidden_columns` drops `hidden` fields before write. The `flatten`/union output tests live here (in `mod flatten_output`), co-located with the code they exercise. |

## DataFusion usage

DataFusion is used for query-engine operations, not as a storage layer:

- **`union_and_shuffle`** — `ctx.read_batch(combined).sort([random()])` for reproducibility-agnostic shuffles.
- **`evaluate_expressions`** — CTE chain in SQL evaluates expression fields in YAML order. Fresh `SessionContext::new()` per call; table registered as `"src"` so there is no registration lifecycle to manage.

(`filter_hidden_columns`, in `output.rs`, is pure Arrow `batch.project()` — no DataFusion.)

The segment-atom pipeline that replaced `grow_parent_from_children` is pure Arrow column selection plus `generate_column`, with no DataFusion involvement: `generate_segment_atom_batch` materialises shared ref columns once (resolving import-taint → precomputed-member → fresh per column), then `project_parent_columns_from_atom` and `project_member_columns` stitch the parent and member batches. `pad_or_generate_tail` absorbs a precomputed-member shape mismatch when a precomputed member batch is shorter than `seg.rows`.

Each function creates its own `SessionContext::new()` — there is no shared context threaded through the executor.

Anything expressible as a SQL string can also be constructed programmatically via DataFusion's `Expr` / `LogicalPlan` / `DataFrame` API. Prefer the programmatic API for new work: it is type-safe, composable, and avoids string-formatting bugs. SQL strings are acceptable for cases where the query structure is fixed and the readability benefit is clear (e.g. the CTE chain in `evaluate_expressions`).

## Invariants & guardrails (don't regress these)

Load-bearing theory points that each, at some point, were violated and caused real bugs. Check them before touching **variants, segmentation, or output** — and prefer aligning code *to* the theory (strong theory alignment is what keeps this codebase simple).

1. **Output is opt-in via `output`/`output_file` only — there is NO default-to-name.** A dataset with neither is generated but *not written* (a pool/intermediate). This is deliberate: the *absence* of `output_file` is precisely how you say "don't emit this" (linked pools, imports). `resolved_outputs()` returns `[]` accordingly. Never "helpfully" default output to the dataset name. (A whole example silently emitted only 1 of 5 files when datasets that had relied on the retired variant-step's auto-default lost it.)
2. **Segment field constraints are keyed by the *parent* field name and constrain only the shared, ref'd parent columns** — materialised once in `generate_segment_atom_batch` and projected to members. They must **not** touch a member's *own* non-ref fields. A member field that merely shares a name with a constrained parent field is independent (parent and child both having a `status` field is common!). `generate_member_nonref_fields` deliberately generates with *no* segment constraints; reintroducing them there re-creates a referential-correctness bug (the parent's restriction bleeds onto the child's unrelated same-named column).
3. **A field's value-source is one spectrum**: type-default ≻ `generator` ≻ `one_of` ≻ `value`. Merge keeps the tightest source and *intersects* supports. `value`+`generator` is **specialisation, not conflict**. `merge(Variant, one_of) = Variant[subset]` — the carrier (cases + ratios) survives, **renormalised** over the subset; it is *not* a flat uniform `one_of`. (Un-renormalised subset weights silently dump the tail mass on the last case.)
4. **Tagged-union exclusivity is structural in the DFS — no discriminant column is materialised.** Don't reintroduce a `_disc_` sentinel. The only visible case tag is `<field>_case`, and only for the `flatten` `discriminant` *output* strategy.
5. **`segment::largest_remainder` is the single rounding primitive** — Hamilton (largest-remainder): unbiased per cell *and* total-conserving. Use it; don't hand-roll `round()` (biased toward common cells → χ² failures) or per-row stochastic rounding (drifts totals, breaks precomputed-member reuse).
6. **`preserve_marginal` is free-by-default.** A variant's `ratio`s are within-population draw weights; a child restriction reshapes the parent mix *unless* `preserve_marginal: true` pins it (a balanced transportation problem — Gale–Hoffman feasibility + IPF; `plan::subdivide_for_pinned_variants`). Don't treat declared ratios as a global guarantee.
7. **Run the `pytest` statistical suite after touching variants/segmentation/output.** Both integration bugs this session (silent no-output; segment-constraint leak across a name collision) were invisible to the Rust unit/integration tests and surfaced only end-to-end on the examples. Statistical/example coverage is not optional polish — it is the only layer that exercises the full planner→generation→write pipeline.
8. **A constraint-bearing variant (`ref` + own `variants`, "case-3") must be identified in `expand_field_variants`, not later.** `resolve_refs` copies the *parent's* carrier onto a plain ref'd variant (S4a), so by plan-time a plain restricted ref and a case-3 field are structurally identical (`ref` + `variants`). The `#[serde(skip)] Field.constraint_bearing` marker (set in expand, where "did the user write `variants` here?" is still knowable, and preserved through `resolve_refs`) is what keeps them apart. Don't reintroduce a `refs.is_some() && !variants.is_empty()` test at plan-time — it will false-positive on every restricted ref.

## Key conventions

- **`Arc<SyntheticDataset>`** in `ExecutionStep` — datasets are shared by reference across steps; clone the Arc, not the dataset.
- **`output/` is gitignored** — generated data never lands in version control.
- **Doc comments welcome** — `///` comments documenting public functions, structs, and fields are encouraged. Inline comments should only explain non-obvious *why* (hidden constraints, invariants, workarounds) — not *what* the code does, which well-named identifiers already convey.
- **No `sql_safe_name`** — DataFusion column names are double-quoted in SQL strings (`"field_name"`), so arbitrary field names are safe without sanitisation.
- **`_slot_idx` sentinel** — a `UInt32` staging-node slot index present in all witness and child batches — which source slot each atom row belongs to. Used by `AssembleFromWitness` to fold witness rows into per-slot lists. Also used in top-level cardinality batches to record which parent-row slot each child row belongs to. Retained in `computed` for grandchild access; stripped from emitted output by `filter_hidden_columns`.
- **`_staging_refs` sentinel** — a `List<UInt32>` column in each witness batch. Entry i lists all staging-slot indices that drew witness row i (the many-to-one pairing from source slots to linked rows). Built in `execute_witness`; consumed and dropped by `AssembleFromWitness` during list folding.
- **`_linked_idx` sentinel** — a `UInt32` column in witness batches recording which linked-dataset row was drawn (index into the eligible linked batch). Persisted for `AccumulateToLinked` collect bindings.
- **Linked rows preceding staging rows** — when a dataset has witness-source lower cover members, the linked-dataset rows occupy the leading positions in the combined batch so `GenerateWitness`'s `n_eligible_slots` boundary correctly identifies eligible linked-dataset slots.


## Known limitations

**Heterogeneous (multi-type) variant fields → CSV** — a `type: variant` whose cases span more than one type (or different object schemas) lowers to an Arrow `DenseUnion` and is emitted as a nullable-superset struct (or a `flatten`ed superset). **CSV** can't represent the nested struct, so such datasets are rejected at validation — use `parquet`, `json`, or `jsonl`. (Configurable output encoding is now provided by VAR-UNIFY's `flatten` strategies, superseding the old VAR-1-OUTPUT-FLAG; a `flatten` of *all-scalar* cases to CSV is a possible future unblock.)

**Variant specialisation — two deferrals** (everything else in the variant roadmap is done):
- **Multi-level `preserve_marginal`** — pinning a parent variant's global marginal only sees restrictions in the parent's *own* lower-cover segments; a restriction nested in a deeper include level isn't compensated. Single-level (direct) pinning works.
- **`one_of` / `constrain_cases` by case *name* for heterogeneous (object) cases** — same-type carriers match cases by *value* today; name-matching for `DenseUnion`/object cases is a follow-on.

**Variants on linked content list items** — a `type: variant` among a list-link's `content:` fields is rejected at validation (see `specs/VAR-LINKED-CONTENT.md`).


## Feature specs

Full design specs and implementation plans live in `specs/`:

| File | Status |
|------|--------|
| `specs/done/MULT-1.md` | **Complete** — implemented and merged |
| `specs/done/MULT-2a.md` | **Complete** — implemented and merged |
| `specs/done/MULT-2.md` | **Complete** — implemented and merged |
| `specs/done/MULT-3.md` | **Complete** — implemented and merged |
| `specs/done/REFRAME-1.md` | **Complete** — planning stages and segment-atom execution (via SEG-ATOM-1) both implemented per spec |
| `specs/done/VAR-1.md` (+ `-impl.md`) | **Complete** — heterogeneous (multi-type / multi-object-schema) tagged unions. Marker `FieldType::Union` + `Field::union_cases: Vec<UnionCase>` (a case is a full `Field`; `FieldVariant` gained `fields:` for object cases). `lower_heterogeneous_unions` (`expand_variants.rs`) lowers het variants → `DenseUnion`; **`DenseUnion` is the internal representation** (DataFusion carries it — `type_id` is the discriminant, free); `unionize_for_output` (`output.rs`) converts to a **nullable-superset struct** at write (parquet/json/jsonl). **CSV** can't hold the nested struct → gated at validation (`is_heterogeneous` is the shared predicate; gate in `validate_dataset`). Configurable output encoding deferred → **VAR-1-OUTPUT-FLAG** (since subsumed by VAR-UNIFY's `flatten`). The substrate for VAR-SPECIALIZE's multi-type half |
| `specs/done/DQ-1.md` | **Complete** — post-write DQ layer implemented: nulls, defaults, corruptions (char deletion/insertion/truncation/encoding, noise, day shift), duplication, row deletion; field-level overrides; multiple output files per dataset |
| *(no spec file)* ARGS-1 | **Complete** — `args` map on `Field` for generator-specific parameters; `after`/`before` top-level date bounds (parallel to `range`); new generators `words`, `sentences`, `paragraphs`, `geohash`, `number_with_format`; boolean `ratio` via `args` |
| `specs/done/SEG-1.md` | **Complete** — branch-and-bound DFS replaces dense 2^N weight pass; N-based cap removed; `MAX_FEASIBLE_SEGMENTS` K-based cap; O(K·N) memory |
| `specs/done/VAR-2.md` | **Complete** — two-level variant factoring; `generate_member_nonref_fields` (formerly `generate_member_batch` pre-SEG-ATOM-1) applies Level 2 variant sub-distribution in the lower cover segment loop; BUG-VAR resolved |
| `specs/done/IMPORT.md` | **Complete** — `import:` stanza on `SyntheticDataset`; hash-ring partitioning (`assign_ring_slices`); `load_import_headers` / `load_import_index` / `filter_ring`; import taint validation; `--seed.ring` CLI flag |
| `specs/done/VAR-SPECIALIZE.md` (+ `-impl.md`) | **COMPLETE (S1–S5)** — child specialisation of parent fields. **Four cases:** (2) generator-domain (`value:`/`one_of:` overrides `generator:`); (3 — the VAR-UNIFY U4 unblocker) constraint-bearing variant carrier on a ref'd field (`ref` + `variants:` → each case lowers with the ref + value, entering segmentation); (1) variant-subset (`one_of:` restricts a parent union, carrier **renormalised**; `preserve_marginal` pins the parent marginal via 2-D IPF + cut-condition validation; multi-level pinning deferred); (4) per-case `constrain_cases` deltas. Unifying model: value-source is one **generator spectrum** (type-default ≻ `generator` ≻ `one_of` ≻ `value`), merge keeps the **richest carrier** + intersects supports (`merge(Variant, one_of) = Variant[subset] ≠ one_of`). Case 3 is required because `FieldVariant` has no `refs`, so top-level variants' constraint-bearing role (e.g. `variant_pruned_by_segment`) can't be expressed as a field variant — blocking U4. PR breakdown in `specs/done/VAR-SPECIALIZE-impl.md`; resolves the "Generator-plus-value should specialise, not conflict" known limitation |
| `specs/done/SEG-ATOM-1.md` | **Complete** — unified shared-ref atom batch per segment with column source priority (import → precomputed → fresh); non-ref columns generated per-member via the variant-aware path. Replaced `grow_parent_from_children` / `resolve_inherited_source_columns` / `generate_segment_member_batches` / `generate_member_expanded_batch`; refactored `generate_member_batch` to non-ref-only. Root-cause fix for BUG-REF; `_BUG_REF` xfail markers removed |
| `specs/done/VAR-EXPAND.md` (+ `-impl.md`) | **Complete** — tagged-union lowering. A `type: variant` field is a tagged union; `lower_member_variants` (`plan.rs`) lowers a leaf lower-cover member's union into one case-member per case + an `ExclusionGroup` that factors as a categorical DFS entry. Mutual exclusion is **structural** — no `_disc_` column is materialised (contrary to the original spec sketch; a discriminant column is reserved for VAR-SPECIALIZE). The "no case of a mandatory union" cell has prior 0 (illegal mass), so members never orphan. Three things made it work: `SegMask = FixedBitSet` (no member ceiling); **largest-remainder rounding** (unbiased + total-conserving, also retired the old `conflicting_constants` flake); and lowering **skips members that are also parents**. The original "fragmentation/IPF/NP-hard" worry was **wrong** (IPF isn't even run when nothing conflicts). Remaining: **scale** (∏ segment count) → conflict-graph factoring, a future optimisation. See `specs/done/VAR-EXPAND.md` §Implementation finding and `docs/.../concepts/variant-lowering.mdx` |
| `specs/done/VAR-UNIFY.md` (+ `-impl.md`) | **COMPLETE (U1–U6; U7 = docs)** — a serde-style **`flatten`** primitive (output-write-time only) on object/union fields [U1–U3: `flatten` + `flatten_strategy`, JSON per-row keys, Parquet superset/prefixed/discriminant]; **top-level dataset `variants:` retired as user input** [U4: rejected at validation; whole-row variation is a `type: variant` field]; **same-type field variants generate per-row** (categorical via `interleave`), not via cross-product [U5]; the top-level variant machinery (`plan_variant_steps` + `CombineVariantBatches`) **deleted** [U6]. **Cross-product consolidation (post-VAR-UNIFY cleanup):** `SyntheticDataset.variants` + `VariantSchema` + `build_local_combinations` + `merge_variant_fields` **removed** — the planner now owns the case-3 cross-product (reads `constraint_bearing` fields straight from `data` → `CaseCombination`); the retired top-level `variants:` is caught by `deny_unknown_fields` at load. `flatten` generalises `unionize_for_output`. **Subsumes VAR-1-OUTPUT-FLAG.** Does *not* touch segmentation. PR breakdown in `specs/done/VAR-UNIFY-impl.md` |
| `specs/VAR-LINKED-CONTENT.md` | **Future — stub** — variants on **linked content list** item fields (a witness-source member carrying a tagged union); deferred out of VAR-EXPAND (Q4) because of `n_eligible_slots` / `_staging_refs` interactions. Rejected at validation in v1 until designed |
| `specs/VAR-CASE-NAME.md` | **Future — designed, not built** — `one_of` / `constrain_cases` by case **name** for **heterogeneous** (multi-type / object) union carriers (same-type already matches by value/name). Carrier type chooses the selector. Load-bearing fix: `merged_ref_field` (rewrite.rs) currently drops `union_cases` onto a child ref — now a one-line fix in one place (both ref-field builders route through it). The last open variant-roadmap item |
| `specs/EXPR-RELOCATE.md` (+ `-impl.md`) | **Future — designed, not built; built 3rd** (after LIST-NORM, PROJECT-FIELD) — relocate expression evaluation from terminal emit into the materialisation pipeline, **placed by dependency** (earliest point where referenced fields exist), *so that* an expression can **author a ref'd/shared column** projected up the lattice ("pin a ref'd field with an expr") — gated by a const-expr **merge algebra** (type-inference always, via DataFusion `get_type`; range-bound analysis where derivable via `cp_solver`/`evaluate_bounds`, else reject). Staged (see `-impl.md`): placement scheduler → type-only computed atom column-source (+ lifts the `ref`+`expression` ban) → bound-merge → content-expressions + edge-granular `collect`. Placement is **uniform for `link` data** — `staging → (staging shared-atom) → witness → assembly → collect`, traced by the order-theory ("witness authors, assembly folds"). The bigger "all generators as DataFusion UDFs / whole pipeline as projections" rewrite is the **back-of-mind north star** (`specs/EXPR-MOONSHOT.md`); this is the analysis-adoption stepping stone. Related: `specs/done/LIST-NORM.md` rollups (**done**) + `specs/done/PROJECT-FIELD.md` projection half (**done**) — its `holding.weight * 2` derivation half waits on lifting the content-expression gate (`validate.rs`) |
| `specs/EXPR-MOONSHOT.md` | **Future — blue-sky stub** — the north star `specs/EXPR-RELOCATE.md` steps toward: all per-row column *value* production as a **chain of DataFusion projections** (`staging → linked → assembly`, iterated, scoped by YAML order). Combinatorial/numeric **planning kept outside** as small **per-atom parameters** (row counts, segment masks, range bounds, ratios) — *not* a full N-row frame — injected as scalar context; rows expanded **inside** by `volatile UDF + expr`. Centrepiece: a big inside/outside mechanism table. Load-bearing insight **ref = CSE** ("author once, fan out" free from the optimiser). **Strangler migration only** — one mechanism at a time, gated on `cargo test` + `pytest`; the parameter/projection seam keeps a half-migrated system shippable. The split (not "UDF-everything") is what enables incremental liberation of generator-arg parameterisation |
| `specs/done/LIST-NORM.md` (+ `-impl.md`) | **COMPLETE (PR1–PR3)** — normalise a numeric field within a list to a target sum: `normalize: {total, field?, into?, precision?}` on any list-producing field (`type: list` or list-valued `expression`). UBO stakes summing to exactly 100 the motivating case (corporate-registry `organisations.shareholders`, hard-invariant pytest); general (budgets, portfolio weights, vote shares). Desugars (`list_norm::desugar_normalize`, after `validate`) to a hidden `<name>__norm_src` + injected `array_normalize`/`array_normalize_field` expression — registered Arrow scalar **UDFs** in `lib/list_norm.rs` (hand-rolled `ScalarUDFImpl` + `return_field_from_args` for value-dependent output type; `Signature::variadic_any`). **`precision: 0` forces the integer exact-sum path via `segment::largest_remainder`** (invariant #5) — needed because a `type: number` field is always Arrow `Float64`. Output-write-time only; does **not** touch segmentation; placement-agnostic for EXPR-RELOCATE (pure desugar → expression + UDF, no bespoke hook). Composes with `array_cat` rollups (works on `List<Struct>` in DF 53.1) and `PROJECT-FIELD` (scalar-list path, no `field:`). Expression language now has its own docs page (`docs/.../reference/expressions.mdx`) |
| `specs/done/PROJECT-FIELD.md` (+ `-impl.md`) | **Complete (PR1–PR2)** — project a content field to a scalar list: `content.project` overloaded by shape — a **bare** `<identifier>` projects a scalar field from `content.fields` (composes with `fields:`); the dotted `<link_ref>.<field>` projects straight from the linked dataset (mutually exclusive with `fields:`, unchanged). Classifier = `ListContent::is_bare_project()`/`project_col()` (`models.rs`); the assembly column-pick (`executor.rs`) was already `project_col`-keyed → **no executor change** (the bare field is generated into the witness/junction like any content field, then all-but-one discarded); `field_to_arrow` returns `List<scalar>` for the bare form; `validate_normalize` takes the scalar path for a bare project so `project` + `normalize` (no `field:`) composes. Delivers the **projection half**; the `holding.weight * 2` derivation half still needs the content-expression gate lifted (`validate.rs`, via EXPR-RELOCATE) |

## Planned next steps

- **DF5** — make `execute_witness` async; use DataFusion to shuffle and limit the linked batch before Arrow-based with-replacement sampling.
- **DF4** — write output via DataFusion `DataSink`; needs care around single-file vs partitioned output.
- **T1** — unit tests for `generate_column` per field type.

## Future work (needs planning)
- **SERDE-YAML** — migrate off deprecated `serde_yaml = "0.9.34+deprecated"` to `serde_yaml_neo` (direct API-compatible fork); find-and-replace crate name in `Cargo.toml` and all `use serde_yaml::` imports
- **REL** — model relationships induced by nested lists
- **REPO** — allow definitions to be imported and included from remote GitHub repositories
- **CNV-1** — field and excluded (field) wildcards on includes to default-define fields from includes as refs
- **Variant roadmap — COMPLETE.** Interleaved sequence as executed: `S1` ✅ → `S3` ✅ → `VAR-UNIFY U4–U6` ✅ → `S2` ✅ → `S4` ✅ → `S5` ✅. Only deferral: multi-level marginal-pinning.
  - **VAR-UNIFY — COMPLETE (U1–U6).** `flatten` output primitive (+ `flatten_strategy`) [U1–U3]; top-level `variants:` retired [U4]; same-type field variants generate **per-row** [U5]; `plan_variant_steps` + `CombineVariantBatches` deleted [U6]. **Cross-product later consolidated** (post-VAR-UNIFY cleanup): `SyntheticDataset.variants`/`VariantSchema`/`build_local_combinations`/`merge_variant_fields` removed — the planner reads `constraint_bearing` case-3 fields directly. **Subsumes VAR-1-OUTPUT-FLAG.** See `specs/done/VAR-UNIFY-impl.md`.
  - **VAR-SPECIALIZE — COMPLETE (S1–S5).** Four cases: (2) generator-domain spectrum [S1 merge + S2 `one_of` generator], (3) `ref`+`variants` constraint-bearing variant [S3], (1) variant-subset (`one_of` restricts the inherited variant, carrier renormalised; `preserve_marginal` pins the parent marginal via 2-D IPF + cut-condition validation) [S4; multi-level pinning deferred], (4) per-case `constrain_cases` [S5]. **Three verbs:** `variants` introduces, `one_of` restricts, `constrain_cases` specialises. See `specs/done/VAR-SPECIALIZE-impl.md`.