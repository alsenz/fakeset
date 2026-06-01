# fakeset — Claude Code guide

## What this repo is

A declarative, DAG-structured synthetic dataset generator. Users write YAML schemas; fakeset generates Parquet/CSV/JSON/JSONL output. The core challenge is producing referentially consistent data across a graph of related datasets — solved by generating children (the more-constrained datasets) first, then assembling each parent's rows from those already-solved child rows.

## Build and test

```bash
cargo build                  # debug
cargo build --release        # release binary → target/release/fakeset
cargo test                   # all unit + integration tests (~194 tests)
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

# run all statistical tests (~40 tests)
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

When auditing code quality or planning simplicity refactors, use `rust-code-analysis-cli` to get per-function complexity metrics as a starting signal:

```bash
cargo install rust-code-analysis-cli  # one-time
rust-code-analysis-cli --metrics -p lib/ -l rust
```

The functions with historically high cognitive/cyclomatic complexity are `plan_segments` (`segment.rs`), `execute` and `grow_parent_from_children` (`executor.rs`), and `build_plan` (`plan.rs`). Use the output to identify where decomposition would have the highest impact — treat metrics as a guide, not a hard threshold.

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
| **staging node** | A virtual node that holds the scalar non-list fields of a source dataset while its witness and assembly nodes are being built. No output file. |
| **witness node** | An atom carrying the linked dataset's schema. One witness row per unique linked-row draw. A hidden `_staging_refs: List<UInt32>` column maps each witness row back to the staging source slots that drew it. |
| **assembly node** | A virtual node above the staging node that folds witness rows into list columns, evaluates expressions, and emits the final output. |
| **source slot** | One row of a staging batch, identified by `_slot_idx`. |
| **linked dataset** | The target of a `links:` stanza (formerly "pool dataset"). |
| **seed edge** | The execution edge from linked dataset atoms to the witness node — the draw that populates witness rows from the linked dataset. |
| **inherited field** | A column pre-populated from an already-computed child batch into the parent's batch, wiring up ref fields so they are never regenerated (formerly "prefill"). |
| **preceding** (preceding-by-execution) | Generated first. Atoms are always preceding. |
| **subsequent** (subsequent-by-execution) | Generated after. Parents and assembly nodes are always subsequent. |

## Core architectural framing

fakeset is built around a **concept semi-lattice**: a partial order where `A ≤ B` means "dataset A is a more-constrained subset of B's population". An `include:` stanza expresses constraint specialisation — not data dependency. A child is a narrower, more-constrained cut of its parent's population. A `links:` stanza introduces a *linked dataset* — a target from which list items are drawn per outer row, governed by the witness/assembly pipeline.

This framing is specified in full in `specs/done/REFRAME-1.md`. In brief: every dataset is a node in the semi-lattice; every pair of nodes with a shared ancestor has a **meet** (greatest lower bound); the most-constrained nodes — those covering ⊥ directly — are **atoms**, generated first.

The algorithm has two symmetric phases:

1. **Push down** — field definitions, type constraints, and ref bindings propagate *down* the lattice toward atoms. For linked datasets, the staging node pre-generates scalar fields before the witness (atom) is generated.

2. **Accumulate up** — generated atom values propagate *up* the lattice toward parents and linked nodes. For include relationships this is `grow_parent_from_children` (DataFusion LEFT JOIN on `_row_idx`). For collect bindings this is `AccumulateToLinked` — the symmetric operation that accumulates atom-level values back into linked-dataset fields.

The DAG is a hierarchy of ever-narrowing constraints, sorted topologically so atoms are always generated before their parents. When a parent field matches a child's field (by ref-wiring or same name), the child's column is *inherited* directly; fields with no child source are generated fresh. This logic lives in `executor.rs::grow_parent_from_children`.

*Theoretical note:* all generator invocations could conceptually happen in parallel — the algorithm's serialisation is purely a scheduling constraint imposed by the inherited-field lattice. The interesting work is resolving which pre-solved values propagate to which nodes and in what order.

## Lower cover segmentation (Bernoulli factoring)

When two or more datasets include the same parent they form the parent's **lower cover** and their rows must be partitioned consistently. All lower cover members participate — including those with `ratio: 1.0` — because their field constraints must enter conflict pruning jointly. This is the exponential-explosion problem: N lower cover members → 2^N possible membership subsets.

`segment.rs::plan_segments` controls the explosion with three steps:

1. **Product-Bernoulli prior** — enumerate all 2^N subsets, weight each by the product of marginal (in/out) probabilities.
2. **Conflict pruning** — zero out any subset whose field constraints are mutually contradictory (e.g. two lower cover members both pinning `status` to different constants). Rows from zeroed subsets are redistributed to surviving subsets.
3. **IPF (Iterative Proportional Fitting)** — scale the surviving weights so declared marginals are exactly restored, then apply Bernoulli rounding to integer row counts.

Enumeration uses a branch-and-bound DFS over the inclusion lattice. At each DFS step the algorithm tries excluding the current member (multiply weight by `1 − ratio`) then including it (multiply by `ratio`, reject immediately if the new member conflicts with any previously-included member). The `MAX_FEASIBLE_SEGMENTS` cap (1,000,000 segments) is a K-based safety valve — it bounds the number of *surviving* feasible segments, not the raw 2^N enumeration space, so arbitrarily large lower-cover groups are handled as long as the feasible set stays small. IPF and Bernoulli rounding operate on the sparse feasible set only.

## Execution pipeline

Each run follows this fixed sequence:

```
load YAML files
  → load_import_headers        (read schema + row count from each import: file; prepend tainted fields)
  → build_dag          (petgraph DAG, topo-sort, cycle detection)
  → pull_down_expression_deps  (push hidden ref fields DOWN the lattice: inject expression deps declared only in an included parent)
  → validate           (structural checks, ref validity, constraint consistency, import taint checks)
  → expand_field_variants      (variant fields → concrete global variants)
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
- `CombineVariantBatches` — after all N variant generation steps for a dataset with `variants:`, concatenates the N variant batches at the canonical path in `computed` so downstream witness steps can find a single linked batch regardless of how many variants the linked dataset has.

## Module map

| Module | Responsibility |
|--------|---------------|
| `lib.rs` | Public API: `load_all_datasets`, YAML discovery |
| `models.rs` | All data types (`SyntheticDataset`, `Field`, `Include`, `ImportSpec`, `RingBounds`, `Schema`, …). Also `resolve_distributions`, `eligible_linked_rows`, the list-link visitor, and lattice-traversal helpers |
| `graph.rs` | `build_dag` — petgraph DAG construction and topo-sort |
| `import.rs` | `load_import_headers` (schema + row-count header pass), `load_import_index` (full file load + hash array), `filter_ring` (ring-bounds row filter), `imported_column_names` |
| `validate.rs` | Schema validation: structural rules, ref checks, expression ordering, import taint checks (`check_import_taint`) |
| `expand_variants.rs` | Expand `type: variant` fields into concrete global `variants:` entries; stubs the original field with an inferred concrete type so downstream ref resolution can still find it |
| `expressions.rs` | `pull_down_expression_deps`, identifier extraction for validation |
| `rewrite.rs` | `resolve_refs` (ref chain resolution, constraint merging), `expand_include_fields` (wildcard field copying), `apply_global_locales`, `apply_locale_to_schema` |
| `constraints.rs` | `FieldConstraints`, `Satisfiable`, `Merge`, `validate_field_constraints` |
| `segment.rs` | `plan_segments` — Bernoulli weights, conflict pruning, IPF, rounding. `LowerCoverMember` and `Segment` types. `assign_ring_slices` — tiles parent ring across segments proportionally. |
| `plan.rs` | `build_plan` — row counts, lower cover groups, ring assignment, inherited-field wiring, collect targets → `ExecutionPlan` / `ExecutionStep` |
| `schema.rs` | `schema_to_arrow`, `field_to_arrow`, `parquet_datatype_to_arrow` — Arrow schema conversion |
| `generator.rs` | `generate_column`, `sample_count` — per-field fake data generation via fake-rs; handles `type: object` by recursively generating sub-fields into a `StructArray`. `fake_date`/`fake_datetime` handle `after`/`before` bounds; `fake_string` threads `args` to range-bearing generators (`Sentence`, `Paragraph`, `Password`, `Words`, `Sentences`, `Paragraphs`, `Geohash`, `NumberWithFormat`); `locale_fake_join!` macro handles generators that return `Vec<String>`. |
| `executor.rs` | `execute` — interprets the plan; staging node generation, witness generation, assembly, `grow_parent_from_children`, `AccumulateToLinked`. Import branch: loads `ImportIndex` on first access, applies ring filter, appends synthetic fields. All DataFusion and Arrow batch operations. |

## DataFusion usage

DataFusion is used for query-engine operations, not as a storage layer:

- **`union_and_shuffle`** — `ctx.read_batch(combined).sort([random()])` for reproducibility-agnostic shuffles.
- **`evaluate_expressions`** — CTE chain in SQL evaluates expression fields in YAML order. Fresh `SessionContext::new()` per call; table registered as `"src"` so there is no registration lifecycle to manage.
- **`filter_hidden_columns`** — `ctx.read_batch(batch).select(visible_cols)` to project out hidden fields.
- **`grow_parent_from_children`** — LEFT JOIN on `_row_idx` expressing parent-field inheritance: skeleton (rule-3 fresh columns) joined with indexed child batches; the SELECT clause names exactly which source each parent field comes from. Takes an explicit `n: usize` (= `seg.rows`) so the skeleton is always the planned size — a precomputed child batch that is shorter than `seg.rows` (due to stochastic rounding in its own segment plan) is handled gracefully by the LEFT JOIN producing fresh values for unmatched skeleton rows.

Each function creates its own `SessionContext::new()` — there is no shared context threaded through the executor.

Anything expressible as a SQL string can also be constructed programmatically via DataFusion's `Expr` / `LogicalPlan` / `DataFrame` API. Prefer the programmatic API for new work: it is type-safe, composable, and avoids string-formatting bugs. SQL strings are acceptable for cases where the query structure is fixed and the readability benefit is clear (e.g. the CTE chain in `evaluate_expressions`).

## Key conventions

- **`Arc<SyntheticDataset>`** in `ExecutionStep` — datasets are shared by reference across steps; clone the Arc, not the dataset.
- **`output/` is gitignored** — generated data never lands in version control.
- **Doc comments welcome** — `///` comments documenting public functions, structs, and fields are encouraged. Inline comments should only explain non-obvious *why* (hidden constraints, invariants, workarounds) — not *what* the code does, which well-named identifiers already convey.
- **No `sql_safe_name`** — DataFusion column names are double-quoted in SQL strings (`"field_name"`), so arbitrary field names are safe without sanitisation.
- **`_row_idx` sentinel** — a `UInt32` 0..n column prepended to batches for positional JOIN keying inside `grow_parent_from_children`; stripped from all outputs.
- **`_slot_idx` sentinel** — a `UInt32` staging-node slot index present in all witness and child batches — which source slot each atom row belongs to. Used by `AssembleFromWitness` to fold witness rows into per-slot lists. Also used in top-level cardinality batches to record which parent-row slot each child row belongs to. Retained in `computed` for grandchild access; stripped from emitted output by `filter_hidden_columns`.
- **`_staging_refs` sentinel** — a `List<UInt32>` column in each witness batch. Entry i lists all staging-slot indices that drew witness row i (the many-to-one pairing from source slots to linked rows). Built in `execute_witness`; consumed and dropped by `AssembleFromWitness` during list folding.
- **`_linked_idx` sentinel** — a `UInt32` column in witness batches recording which linked-dataset row was drawn (index into the eligible linked batch). Persisted for `AccumulateToLinked` collect bindings.
- **Linked rows preceding staging rows** — when a dataset has witness-source lower cover members, the linked-dataset rows occupy the leading positions in the combined batch so `GenerateWitness`'s `n_eligible_slots` boundary correctly identifies eligible linked-dataset slots.

## Known flaky test

`segment::tests::conflicting_constants_zeroed_and_redistributed` — Bernoulli rounding in `plan_segments` is stochastic; when run in parallel with the full suite it occasionally hits a rounding edge that produces 101 rows instead of 100. Passes reliably in isolation (`cargo test conflicting_constants`). Pre-existing; not introduced by recent changes.

## Known limitations

**Mixed-type variant fields** — `type: variant` fields whose choices span more than one type (e.g. one choice is a string, another is a number) are not currently rejected at validation time. Instead, `expand_field_variants` produces an untyped stub (`field_type = None`), which causes a runtime panic when `schema_to_arrow` or `generate_column_raw` hits the unresolved type. Workaround: ensure all choices in a `type: variant` field share the same type. See `specs/VAR-1.md` for the planned fix and the longer-term `type: any` encoding design.

**BUG-REF (first-child-wins) — overlap segment ref integrity** — In overlap segments where two lower-cover members both appear, `grow_parent_from_children` inherits shared fields (e.g. `contract_id`) from whichever child is first in `child_batches` (HashMap iteration order). The other child's rows were generated with different values for that field and will not match the parent. Affects `claims.contract_id` / `claims.customer_id` in the `{premiums ∩ claims}` overlap segment (~34% of contract rows). Non-deterministic. Documented by `_BUG_REF` xfail markers in `tests/statistical/test_insurance.py`.

**Generator-plus-value constraint should specialise, not conflict** — A child field with `ref: parent.field, value: "constant"` currently errors in `resolve_refs` if the parent field declares a `generator:`. The intended semantics are that `value:` is a specialisation overriding the generator, but the constraint merge treats them as conflicting. Workaround: omit `generator:` from any parent field that children will specialise with a constant `value:`. See `rewrite.rs::resolve_refs` / `constraints.rs::Merge` for the fix location.

## Feature specs

Full design specs and implementation plans live in `specs/`:

| File | Status |
|------|--------|
| `specs/done/MULT-1.md` | **Complete** — implemented and merged |
| `specs/done/MULT-2a.md` | **Complete** — implemented and merged |
| `specs/done/MULT-2.md` | **Complete** — implemented and merged |
| `specs/done/MULT-3.md` | **Complete** — implemented and merged |
| `specs/done/REFRAME-1.md` | **Partially complete** — planning stages correct; segment atom generation in executor not implemented per spec (see SEG-ATOM-1) |
| `specs/VAR-1.md` | **Planned** — Phase 1 (validation gate); Phase 2 (`type: any` encoding) pending design sign-off |
| `specs/done/DQ-1.md` | **Complete** — post-write DQ layer implemented: nulls, defaults, corruptions (char deletion/insertion/truncation/encoding, noise, day shift), duplication, row deletion; field-level overrides; multiple output files per dataset |
| *(no spec file)* ARGS-1 | **Complete** — `args` map on `Field` for generator-specific parameters; `after`/`before` top-level date bounds (parallel to `range`); new generators `words`, `sentences`, `paragraphs`, `geohash`, `number_with_format`; boolean `ratio` via `args` |
| `specs/done/SEG-1.md` | **Complete** — branch-and-bound DFS replaces dense 2^N weight pass; N-based cap removed; `MAX_FEASIBLE_SEGMENTS` K-based cap; O(K·N) memory |
| `specs/done/VAR-2.md` | **Complete** — two-level variant factoring; `generate_member_batch` applies Level 2 variant sub-distribution in the lower cover segment loop; BUG-VAR resolved |
| `specs/done/IMPORT.md` | **Complete** — `import:` stanza on `SyntheticDataset`; hash-ring partitioning (`assign_ring_slices`); `load_import_headers` / `load_import_index` / `filter_ring`; import taint validation; `--seed.ring` CLI flag |
| `specs/VAR-SPECIALIZE.md` | **Future** — child specialisation of parent variant fields; Level 2 IPF upgrade; needs design sign-off on YAML syntax and `FieldConstraints` set-value encoding; depends on VAR-2 |
| `specs/SEG-ATOM-1.md` | **Planned** — correct segment atom generation per REFRAME. Unified shared-ref atom batch per segment with column source priority (import → precomputed → fresh); non-ref columns generated per-member via the variant-aware path. Replaces `grow_parent_from_children` / `resolve_inherited_source_columns` / `generate_segment_member_batches` / `generate_member_expanded_batch`; refactors `generate_member_batch` to non-ref-only. Root-cause fix for BUG-REF; removes `_BUG_REF` xfail markers |

## Planned next steps

- **DF5** — make `execute_witness` async; use DataFusion to shuffle and limit the linked batch before Arrow-based with-replacement sampling.
- **DF4** — write output via DataFusion `DataSink`; needs care around single-file vs partitioned output.
- **T1** — unit tests for `generate_column` per field type.

## Future work (needs planning)
- **SERDE-YAML** — migrate off deprecated `serde_yaml = "0.9.34+deprecated"` to `serde_yaml_neo` (direct API-compatible fork); find-and-replace crate name in `Cargo.toml` and all `use serde_yaml::` imports
- **REL** — model relationships induced by nested lists
- **REPO** — allow definitions to be imported and included from remote GitHub repositories
- **CNV-1** — field and excluded (field) wildcards on includes to default-define fields from includes as refs
- **VAR-SPECIALIZE** — see `specs/VAR-SPECIALIZE.md`