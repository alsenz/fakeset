# fakeset — Claude Code guide

## What this repo is

A declarative, DAG-structured synthetic dataset generator. Users write YAML schemas; fakeset generates Parquet/CSV/JSON/JSONL output. The core challenge is producing referentially consistent data across a graph of related datasets — solved by generating children (the more-constrained datasets) first, then assembling each parent's rows from those already-solved child rows.

## Build and test

```bash
cargo build                  # debug
cargo build --release        # release binary → target/release/fakeset
cargo test                   # all unit + integration tests (~142 tests)
cargo check                  # fast type-check without linking
```

Run the corporate-registry example:
```bash
cargo run -- examples/corporate-registry --output ./output/corporate-registry
```

Output goes to `./output/` (gitignored).

## Glossary

These terms have precise meanings in this codebase — use them consistently.

| Term | Meaning |
|------|---------|
| **parent** (parent-by-inclusion) | A dataset that is *included by* another — the less-constrained, broader population. |
| **child** (child-by-inclusion) | A dataset that *includes* another — the more-constrained, narrower population. |
| **sibling** | Two or more datasets that share a common parent-by-inclusion. |
| **preceding** (preceding-by-execution) | Generated first. Children are always preceding. |
| **subsequent** (subsequent-by-execution) | Generated after. Parents are always subsequent. |
| **sibling group** | A parent together with all its siblings; planned as a unit via segmentation. |
| **segment** | One subset of a parent's rows that belongs to a particular combination of siblings. |
| **pool sibling** | A sibling arising from a nested-include `content: {includes: [...]}` field — contributes constraints to the parent's segments but produces no standalone output file. |
| **nested include** | A `list` field whose items are structs drawn from an included dataset (as opposed to a simple scalar list). |
| **inner flat** | The intermediate flat `RecordBatch` (with `_outer_idx`) produced for one nested include field before assembly into `ListArray` columns. |
| **prefill** | A column pre-populated from an already-computed child batch into the parent's batch, wiring up ref fields so they are never regenerated. |

## Core architectural tenet

**Children are generated first; parents are expanded from them.**

An `include` is a *constraint specialisation*, not a data dependency. A child is a more-constrained subset of its parent's population. The DAG is a hierarchy of ever-narrowing constraints, sorted topologically so the deepest leaves (most constrained) are always generated before their parents. Each parent's rows are then assembled from those already-solved child rows — inheriting constrained columns directly and generating fresh values for the rest — so consistency is guaranteed by construction, not enforced after the fact.

When a parent field matches a child's field (by ref-wiring or same name), the child's column is inherited directly. Fields with no child source are generated fresh. This logic lives in `executor.rs::grow_parent_from_children`, which expresses it as a DataFusion LEFT JOIN on `_row_idx`.

## Sibling segmentation

When two or more siblings each declare a `distribution` on a common parent, their rows must be partitioned consistently. This is the exponential-explosion problem: N siblings → 2^N possible membership subsets.

`segment.rs::plan_segments` controls the explosion with three steps:

1. **Product-Bernoulli prior** — enumerate all 2^N subsets, weight each by the product of marginal (in/out) probabilities.
2. **Conflict pruning** — zero out any subset whose field constraints are mutually contradictory (e.g. two siblings both pinning `status` to different constants). Rows from zeroed subsets are redistributed to surviving subsets.
3. **IPF (Iterative Proportional Fitting)** — scale the surviving weights so declared marginals are exactly restored, then apply Bernoulli rounding to integer row counts.

The default cap is 16 siblings per group (65,536 subsets); override with `--max-siblings`. Raising it costs RAM quadratically.

## Execution pipeline

Each run follows this fixed sequence:

```
load YAML files
  → build_dag          (petgraph DAG, topo-sort, cycle detection)
  → pull_down_expression_deps  (inject hidden ref fields for expression variables declared only in an included parent)
  → validate           (structural checks, ref validity, constraint consistency)
  → expand_field_variants      (variant fields → concrete global variants)
  → resolve_refs       (copy field_type/schema from ref targets; merge constraints)
  → apply_global_locales       (stamp locale onto locale-aware fields)
  → build_plan         (resolve row counts, sibling groups, prefill wiring → ExecutionPlan)
  → execute            (interpret steps in order, write output files)
```

`build_plan` produces a flat list of `ExecutionStep` variants:

- `GenerateDataset` — simple dataset, no siblings.
- `GenerateSiblingGroup` — parent + siblings planned together via segmentation.
- `GenerateInnerFlat` — flat intermediate for one nested include field.
- `AssembleNestedInclude` — fold inner-flat batches into `ListArray` columns, evaluate expressions, emit.
- `WriteSharedOutput` — union + shuffle all accumulated batches for a shared output file, write once.

## Module map

| Module | Responsibility |
|--------|---------------|
| `lib.rs` | Public API: `load_all_datasets`, YAML discovery |
| `models.rs` | All data types (`SyntheticDataset`, `Field`, `Include`, `Schema`, …). Also `resolve_distributions`, `for_each_content_include` |
| `graph.rs` | `build_dag` — petgraph DAG construction and topo-sort |
| `validate.rs` | Schema validation: structural rules, ref checks, expression ordering |
| `expand_variants.rs` | Expand `type: variant` fields into concrete global `variants:` entries |
| `expressions.rs` | `pull_down_expression_deps`, identifier extraction for validation |
| `rewrite.rs` | `resolve_refs` (ref chain resolution, constraint merging), `apply_global_locales`, `apply_locale_to_schema` |
| `constraints.rs` | `FieldConstraints`, `Satisfiable`, `Merge`, `validate_field_constraints` |
| `segment.rs` | `plan_segments` — Bernoulli weights, conflict pruning, IPF, rounding |
| `plan.rs` | `build_plan` — row counts, sibling groups, `ExecutionPlan` / `ExecutionStep` |
| `schema.rs` | `schema_to_arrow`, `field_to_arrow`, `parquet_datatype_to_arrow` — Arrow schema conversion |
| `generator.rs` | `generate_column`, `sample_count` — per-field fake data generation via fake-rs |
| `executor.rs` | `execute` — interprets the plan; all DataFusion and Arrow batch operations |

## DataFusion usage

DataFusion is used for query-engine operations, not as a storage layer:

- **`union_and_shuffle`** — `ctx.read_batch(combined).sort([random()])` for reproducibility-agnostic shuffles.
- **`evaluate_expressions`** — CTE chain in SQL evaluates expression fields in YAML order. Fresh `SessionContext::new()` per call; table registered as `"src"` so there is no registration lifecycle to manage.
- **`filter_hidden_columns`** — `ctx.read_batch(batch).select(visible_cols)` to project out hidden fields.
- **`grow_parent_from_children`** — LEFT JOIN on `_row_idx` expressing parent-field inheritance: skeleton (rule-3 fresh columns) joined with indexed child batches; the SELECT clause names exactly which source each parent field comes from.

Each function creates its own `SessionContext::new()` — there is no shared context threaded through the executor.

Anything expressible as a SQL string can also be constructed programmatically via DataFusion's `Expr` / `LogicalPlan` / `DataFrame` API. Prefer the programmatic API for new work: it is type-safe, composable, and avoids string-formatting bugs. SQL strings are acceptable for cases where the query structure is fixed and the readability benefit is clear (e.g. the CTE chain in `evaluate_expressions`).

## Key conventions

- **`Arc<SyntheticDataset>`** in `ExecutionStep` — datasets are shared by reference across steps; clone the Arc, not the dataset.
- **`output/` is gitignored** — generated data never lands in version control.
- **Doc comments welcome** — `///` comments documenting public functions, structs, and fields are encouraged. Inline comments should only explain non-obvious *why* (hidden constraints, invariants, workarounds) — not *what* the code does, which well-named identifiers already convey.
- **No `sql_safe_name`** — DataFusion column names are double-quoted in SQL strings (`"field_name"`), so arbitrary field names are safe without sanitisation.
- **`_row_idx` sentinel** — a `UInt32` 0..n column prepended to batches for positional JOIN keying inside `grow_parent_from_children`; stripped from all outputs.
- **Pool rows come first** — when a parent has pool siblings, their rows occupy the leading positions in the combined batch so `GenerateInnerFlat`'s `pool_size` index correctly identifies eligible rows.

## Known flaky test

`segment::tests::conflicting_constants_zeroed_and_redistributed` — Bernoulli rounding in `plan_segments` is stochastic; when run in parallel with the full suite it occasionally hits a rounding edge that produces 101 rows instead of 100. Passes reliably in isolation (`cargo test conflicting_constants`). Pre-existing; not introduced by recent changes.

## Planned next steps (from memory)

- **DF5** — make `execute_inner_flat` async; use DataFusion to shuffle and limit the pool batch before Arrow-based with-replacement sampling.
- **DF4** — write output via DataFusion `DataSink`; needs care around single-file vs partitioned output.
- **T1** — unit tests for `generate_column` per field type.
- **branch-and-bound segment enumeration** — replace the dense 2^N weight pass in `plan_segments` with an O(K·N) lattice traversal for large sibling groups.

## Future work (needs planning)

- **REL** - model relationships induced by nested lists
- **REL-1** - allow nested includes to ref outer fields *and* included fields and ensure they are consistent (the same) in the final outputs
- **REPO** - allow definitions to be imported and included from remote github repositories
- **IMPORT** - allow imports from pre-existing files and database connections
- **CONVENIENCE (CNV-1)** - field and excluded (field) wildcards on includes to default define fields from includes as refs
- **COMPARISONS (CMP-1)** - comparison with synth library
- **CMP-2** - comparison with Synthetic Data Vault (SDV) (python library)
- **EX-1** an insurance dataset example
- **DATA-QUALITY (DQ)** - final execution stage post-processing the generated output to fake data quality issues-  default field values, corruptions, null fields, typos, inconsistent id keys, formatting errors etc.