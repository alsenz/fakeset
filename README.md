# fakeset

A declarative, DAG-structured synthetic dataset generator written in Rust.
Loosely inspired by [synth](https://github.com/getsynth/synth).

## Overview

You describe your datasets as YAML files.  Each file declares a name, an output
format, a schema (the shape of the rows), and optionally an *include* —
another dataset file that this one depends on.  `fakeset` resolves the includes
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

### Sibling segmentation

When two or more datasets include the same parent they become *siblings* of
that parent.  The executor uses **sibling segmentation** to partition the
parent's rows: each sibling gets a segment whose size matches its declared
marginal `ratio`.  All siblings participate — even those with `ratio: 1.0`,
whose field constraints must enter conflict pruning jointly with their siblings'.

Under the hood, `fakeset` starts from a product-independence prior (each
sibling's membership is an independent Bernoulli trial), then applies
Iterative Proportional Fitting (IPF) to restore the declared marginals exactly.
This makes sibling segmentation correct for both independent-overlap cases
(e.g. two optional flags) and mutually exclusive categorical cases (e.g.
small/medium/large company tiers whose fractions sum to 1).

```yaml
include:
  file: customers.yaml
  ratio: 0.05   # marginal row-membership probability (Bernoulli)
```

```yaml
content:
  include:
    file: events.yaml
    ratio: 0.5       # fraction of the include pool eligible for sampling
    cardinality: {min: 1, max: 4}   # items drawn per outer row
```

The distinction is in what the segment contributes: a top-level sibling writes
its rows as a standalone output file; a `content.include` pool sibling places
qualifying rows at the front of the parent batch for list-item sampling but
produces no output file of its own.

## YAML schema

```yaml
name: policies           # table name; also used as the output filename
format: parquet          # parquet | csv | json | jsonl
output_file: policies    # override output filename (default: name)
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
      name: item
      type: string
      generator: word
  - name: events          # nested include — items are structs drawn from an included dataset
    type: list
    content:
      include:
        file: events.yaml
        ref: event
        ratio: 0.5                  # fraction of event rows eligible for sampling
        cardinality: {min: 0, max: 3}   # items drawn per outer row
      fields:                       # struct fields for each list item
        - name: event_id
          refs: event.id            # sourced from the included dataset
        - name: label
          type: string              # generated fresh per item
```

Supported field types: `number`, `boolean`, `string`, `object`, `list`, `date`, `date_time`, `variant`.

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

Each dataset with an `output_file` (or name, as default) produces one output
file under the output directory (default: `./output`), named
`<output_file>.<format>`.

## Examples

### corporate-registry

A three-dataset DAG modelling officers, organisations, and SMEs.

```bash
cargo run -- examples/corporate-registry --output ./output/corporate-registry
```

See [`examples/corporate-registry/README.md`](examples/corporate-registry/README.md) for details.