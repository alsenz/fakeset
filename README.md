# fakeset

A declarative, DAG-structured synthetic dataset generator written in Rust.
Loosely inspired by [synth](https://github.com/getsynth/synth).

## Overview

You describe your datasets as YAML files.  Each file declares a name, an output
format, a schema (the shape of the rows), and optionally a list of *includes* —
other dataset files that this one depends on.  `fakeset` resolves the includes
into a directed acyclic graph (DAG), validates it for cycles, and then executes
the plan in dependency order using [Apache DataFusion](https://datafusion.apache.org).

### Glossary

| Term | Definition |
|---|---|
| **parent** (parent-by-inclusion) | A dataset that is *included by* another — the less-constrained, broader population. |
| **child** (child-by-inclusion) | A dataset that *includes* another — the more-constrained, narrower population. |
| **sibling** | Two datasets that share a common parent-by-inclusion. |
| **preceding** (preceding-by-execution) | Generated first by the executor. Children are always preceding. |
| **subsequent** (subsequent-by-execution) | Generated later by the executor. Parents are always subsequent. |

The rule is: **parents are subsequent, children are preceding.**  Siblings may execute in parallel unless another inclusion path creates a dependency between them.

### Why a topologically sorted DAG?

An `include` is a *constraint specialisation*, not a data dependency.  A child
defines a more-constrained subset of its parent's population.  The hierarchy is
a hierarchy of ever-narrowing constraints, not a hierarchy of datasets.  Rows
are grown iteratively from the leaves of the constraint graph: the most
constrained datasets (children, deepest leaves) are generated first, and each
level is grown outward from those already-solved populations, so consistency
across all outputs is guaranteed by construction rather than enforced after the
fact.

**Example — insurance fraud:**
- `fraudulent_policies.yaml` *includes* `policies.yaml` at a 5% distribution.
  It is a child: more constrained — policies that open and close very quickly (a
  known fraud signal).  It is generated first (preceding).
- `policies.yaml` is the parent: the broader, less constrained population.  Its
  output naturally contains the correctly proportioned fraudulent cohort because
  the constraints were solved at the child level first (subsequent).

When writing definition files, think of `includes` as constraint specialisation:
"I am a more constrained subset of my parent's population."

Datasets marked `skip: true` are treated as intermediates: their data is
generated and made available to dependants, but no output file is written.

### Sibling segmentation

When two or more datasets include the same parent and each declares a
`distribution`, they become *siblings* of that parent.  The executor uses
**sibling segmentation** to partition the parent's rows: each sibling gets a
segment whose size matches its declared marginal membership probability.

Under the hood, `fakeset` starts from a product-independence prior (each
sibling's membership is an independent Bernoulli trial), then applies
Iterative Proportional Fitting (IPF) to restore the declared marginals exactly.
This makes sibling segmentation correct for both independent-overlap cases
(e.g. two optional flags) and mutually exclusive categorical cases (e.g.
small/medium/large company tiers whose fractions sum to 1).

```
distribution: 0.05   # on top-level includes: marginal row-membership probability
distribution: 0.5    # on content includes:   fraction of the include pool to sample from
```

The two uses of `distribution` are syntactically identical but semantically
distinct: the top-level form drives IPF-based row partitioning; the content
form limits the sampling pool for inner list items.

## YAML schema

```yaml
name: policies           # table name; also used as the output filename
format: parquet          # parquet | csv
rows: 1000               # default 100
skip: false              # set true to suppress output for intermediate datasets

includes:                # datasets this file depends on
  - file: customers.yaml # path relative to this file
    ref: customers       # name used to reference it inside this dataset
    distribution: 0.3    # optional: fraction of parent rows (for sibling segmentation)

data:                    # flat list of field definitions
  - name: id
    type: string
    generator: uuid
  - name: premium
    type: number
    range:
      min: 100
      max: 5000
  - name: active
    type: boolean
  - name: holder          # nested object
    type: object
    fields:
      - name: full_name
        type: string
  - name: tags            # simple list — items are plain scalars
    type: list
    count: {min: 1, max: 5}
    content:
      type: string
  - name: events          # rich list — items are structs drawn from an included dataset
    type: list
    count: {min: 0, max: 3}
    content:
      includes:
        - file: events.yaml
          ref: event
      fields:             # struct fields for each list item
        - name: event_id
          ref: event.id   # sourced from the included dataset
        - name: label
          type: string    # generated fresh per item
```

Supported field types: `number`, `boolean`, `string`, `object`, `list`.

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

## Building

```bash
cargo build --release
```

The binary is placed at `target/release/fakeset`.

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

Each non-skipped dataset produces one output file under the output directory
(default: `./output`), named `<dataset-name>.<format>`.

## Examples

### corporate-registry

A three-dataset DAG modelling officers, organisations, and SMEs.

```bash
cargo run -- examples/corporate-registry --output ./output/corporate-registry
```

See [`examples/corporate-registry/README.md`](examples/corporate-registry/README.md) for details.