# fakeset

A declarative, DAG-structured synthetic dataset generator written in Rust.
Loosely inspired by [synth](https://github.com/getsynth/synth).

## Overview

You describe your datasets as YAML files.  Each file declares a name, an output
format, a schema (the shape of the rows), and optionally an *include* —
the broader dataset this one is a **constrained subset** of.  `fakeset` resolves
these constraint relationships into a directed acyclic graph (DAG), validates it
for cycles, and then executes the plan in topological order using [Apache DataFusion](https://datafusion.apache.org).

### Glossary

| Term | Definition |
|---|---|
| **parent** (parent-by-inclusion) | A dataset that is *included by* another — the less-constrained, broader population. |
| **child** (child-by-inclusion) | A dataset that *includes* another — the more-constrained, narrower population. |
| **atom** | The most-constrained node in a component — generated from scratch with no inherited values. Every lower-cover leaf and every witness node is an atom. Atoms are always preceding. |
| **lower cover** | The set of datasets that directly include a given parent. |
| **lower cover group** | A parent together with its lower cover; planned as a unit via Bernoulli factoring. |
| **linked dataset** | The target of a `links:` stanza — the dataset whose rows are drawn as list items. |
| **staging node** | Internal node holding scalar non-list fields while list items are being assembled. |
| **witness node** | Atom node carrying the linked dataset's schema; one row per unique linked-row draw. |
| **preceding** (preceding-by-execution) | Generated first. Atoms are always preceding. |
| **subsequent** (subsequent-by-execution) | Generated later. Parents and assembly nodes are always subsequent. |

The rule is: **the most-constrained nodes (atoms) are generated first; parents and assembly nodes are assembled from them.**

### Why a topologically sorted DAG?

An `include` is a *constraint specialisation*, not a data dependency.  A child
defines a more-constrained subset of its parent's population.  The hierarchy is
a hierarchy of ever-narrowing constraints, not a hierarchy of datasets.
Generation flows leaf-to-root through the constraint hierarchy: the most
constrained datasets (children, deepest leaves) are generated first, then each
parent's population is completed by expanding from those already-solved child
rows — so consistency across all outputs is guaranteed by construction rather
than enforced after the fact.

**Example — insurance fraud:**
- `fraudulent_policies.yaml` *includes* `policies.yaml` at a 5% ratio.
  It is a child: more constrained — policies that open and close very quickly (a
  known fraud signal).  It is generated first (preceding).
- `policies.yaml` is the parent: the broader, less constrained population.  Its
  output naturally contains the correctly proportioned fraudulent cohort because
  the constraints were solved at the child level first (subsequent).

When writing definition files, think of `include` as constraint specialisation:
"I am a more constrained subset of my parent's population."

### Lower cover segmentation (Bernoulli factoring)

When two or more datasets include the same parent they form the parent's
**lower cover**.  fakeset uses **Bernoulli factoring** to partition the
parent's rows: each lower cover member gets a segment whose size matches its
declared marginal `ratio`.  All lower cover members participate — even those
with `ratio: 1.0` — because their field constraints must enter conflict pruning
jointly.

Under the hood, `fakeset` starts from a product-independence prior (each
member's row membership is an independent Bernoulli trial), then applies
Iterative Proportional Fitting (IPF) to restore the declared marginals exactly.
This makes segmentation correct for both independent-overlap cases
(e.g. two optional flags) and mutually exclusive categorical cases (e.g.
small/medium/large company tiers whose fractions sum to 1).

```yaml
include:
  file: customers.yaml
  ratio: 0.05   # marginal row-membership probability (Bernoulli)
```

## YAML schema

```yaml
name: policies           # table name; also used as the output filename
format: parquet          # parquet | csv | json | jsonl
output: policies.parquet # output file (or use outputs: [] for multiple files)
rows: 1000               # explicit row count — omit when using ratio (mutually exclusive)

include:                 # parent dataset this file is a constrained subset of
  file: customers.yaml   # path relative to this file
  ref: customers         # name used to reference fields inside this dataset
  ratio: 0.3             # fraction of parent rows in this child; implies row count

data:                    # flat list of field definitions
  - name: id
    type: string
    generator: uuid
  - name: premium
    type: number
    range:
      min: 100
      max: 5000
  - name: inception_date
    type: date
    after: "2020-01-01"     # bounded date range
    before: "2025-12-31"
  - name: active
    type: boolean
    args: { ratio: 80 }     # 80% true; omit for 50/50
  - name: holder          # nested object
    type: object
    fields:
      - name: full_name
        type: string
  - name: tags            # simple list — items are plain scalars
    type: list
    count: {min: 1, max: 5}
    content:
      name: item
      type: string
      generator: word
  - name: events          # list-link field — items are structs drawn from the linked dataset
    type: list
    content:
      from: event         # draw items from the "event" linked dataset
      fields:             # struct fields for each list item
        - name: event_id
          refs: event.id  # sourced from the linked dataset
        - name: label
          type: string    # generated fresh per witness row

links:
  - file: events.yaml
    ref: event
    cardinality: {min: 0, max: 3}   # items drawn per outer row
    reinforcement: 0                 # 0 = without-replacement within each list; >1 = Pólya clumping
    overlap: 0                       # 0 = non-overlapping across outer rows; >1 = popularity bias
```

Supported field types: `number`, `boolean`, `string`, `object`, `list`, `date`, `date_time`, `variant`.

Fields also accept:
- `range: { min:, max: }` — inclusive bounds for `number` fields
- `after:` / `before:` — bounded date/datetime generation (ISO 8601 / RFC 3339)
- `args: { ... }` — generator-specific parameters: `min`/`max` (word/length count) for `sentence`, `paragraph`, `words`, `sentences`, `paragraphs`, `password`; `precision` for `geohash`; `format` for `number_with_format`; `ratio` (0–100) for `boolean`

## Dependencies

| Crate | Role |
|---|---|
| [apache-arrow](https://crates.io/crates/arrow) | In-memory columnar format for generated data |
| [datafusion](https://crates.io/crates/datafusion) | Query engine; holds registered datasets and drives execution |
| [fake-rs](https://crates.io/crates/fake) | Fake data generation for primitive field types |
| [parquet](https://crates.io/crates/parquet) | Parquet file writer |
| [petgraph](https://crates.io/crates/petgraph) | DAG construction, topological sort, and cycle detection |
| [clap](https://crates.io/crates/clap) | CLI argument parsing |
| [serde / serde_yaml](https://crates.io/crates/serde_yaml) | YAML deserialisation |
| [walkdir](https://crates.io/crates/walkdir) | Recursive YAML file discovery |
| [tokio](https://crates.io/crates/tokio) | Async runtime (required by DataFusion) |

## Documentation

Full documentation lives in the `docs/` directory and is built with [Astro Starlight](https://starlight.astro.build).

```bash
cd docs
pnpm install        # first time only
pnpm run dev        # dev server at http://localhost:4321
pnpm run build      # production build → docs/dist/
```

The YAML schema reference is generated from source via a `docgen` binary:

```bash
cd docs && pnpm run gen-schema   # regenerates docs/src/data/schema.json
```

This runs automatically as a prebuild step before `pnpm run build`.

## Building

```bash
cargo build --release
```

The binary is placed at `target/release/fakeset`.

## Testing

### Rust unit and integration tests

```bash
cargo test          # ~194 tests
```

### Statistical regression tests

A Python pytest suite exercises the two bundled examples end-to-end and verifies
statistical properties of the generated data using
[polars](https://pola.rs) and [scipy](https://scipy.org):

```bash
# install Python deps (one-time)
pip install pytest polars scipy

# run all statistical tests
pytest
```

The suite runs both examples, then checks:

| Check | Type |
|---|---|
| Numeric fields within declared `range` bounds | Hard invariant |
| Variant values restricted to declared set | Hard invariant |
| Referential integrity (`ref` fields point to valid rows) | Hard invariant |
| Expression results match formula (e.g. `net_payout = claim_amount - deductible`) | Hard invariant |
| List cardinality within declared `min`/`max` | Hard invariant |
| Mutually exclusive lower-cover segments partition parent exactly | Hard invariant |
| Include ratios match declared values (binomial test, α=0.01) | Statistical |
| Variant distributions match declared ratios (χ² test, α=0.01) | Statistical |
| Numeric distributions consistent with Uniform[min, max] (KS test, α=0.01) | Statistical |

Statistical tests use α=0.01 (1% false-positive rate per test) and skip automatically
when the sample is too small for the chosen test.

## Running

```bash
# Single file
fakeset path/to/dataset.yaml

# Directory (all .yaml / .yml files discovered recursively)
fakeset path/to/definitions/

# Mix of files and directories, custom output location
fakeset definitions/ extra.yaml --output ./generated

# Help
fakeset --help
```

Each dataset produces one or more output files under the output directory
(default: `./output`). Use `output:` for a single file or `outputs:` for
multiple (e.g. a clean copy and a synthetically degraded copy — see
[Data Quality](docs/src/content/docs/reference/yaml-schema.mdx#dataquality)).

## Examples

### corporate-registry

An 8-dataset DAG modelling a corporate registry: individuals, organisations (with an embedded `directors` list), SMEs, three mutually exclusive SME size tiers (micro/small/medium), directors, and grants. Exercises Bernoulli segmentation, list links with outer-scoped refs, and multi-level include chains.

```bash
cargo run --bin fakeset -- examples/corporate-registry --output ./output/corporate-registry
```

### insurance

A 5-dataset schema covering customers, policy products, contracts, claims, and premium payments. Exercises object fields (`address` struct), variant types, list links (contracts embedding linked policy objects), expression fields (`net_payout = claim_amount - deductible`), and two-level include chains.

```bash
cargo run --bin fakeset -- examples/insurance --output ./output/insurance
```