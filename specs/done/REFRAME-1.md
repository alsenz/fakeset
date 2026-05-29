# Reframe: fakeset as a concept semi-lattice

> Terminology is drawn from order theory and partially ordered sets for the lattice structure,
> and from graph theory for the execution DAG. Bernoulli probability and IPF are standard
> references already in use elsewhere in the codebase.
>
> **Node vs element**: throughout this document, "node" and "element" are used interchangeably
> to refer to a member of the semi-lattice or DAG. "Node" is preferred when emphasising graph
> structure; "element" when emphasising order-theoretic properties.
>
> **Arrow convention (diagrams)**: arrows point from a more-constrained element toward a
> less-constrained element — i.e. from child toward parent, from ⊥ upward. This matches
> the execution-time direction for include edges. Execution edges that cross component
> boundaries (seed, outer ref, cardinality, collect) connect elements that are incomparable
> in the semi-lattice and may point in any direction.

## Worked example

The structural diagrams use the following four datasets.

```yaml
# animals.yaml
name: animals
rows: 100
data:
  - name: id
    type: string
    generator: uuid
  - name: weight_kg
    type: number
    generator: float
    min: 0.5
    max: 80.0
```

```yaml
# cats.yaml
name: cats
format: csv
output_file: cats
include:
  file: animals.yaml
  ref: animals
  ratio: 0.6
data:
  - name: environment
    type: variant
    variants:
      - value: indoor
        ratio: 0.5
      - value: outdoor
        ratio: 0.5
  - name: name
    type: string
    generator: first_name
```

```yaml
# dogs.yaml
name: dogs
format: csv
output_file: dogs
include:
  file: animals.yaml
  ref: animals
  ratio: 0.4
data:
  - name: breed
    type: string
    generator: word
```

```yaml
# owners.yaml
name: owners
format: jsonl
output_file: owners
rows: 20
links:
  - file: cats.yaml
    ref: cats
    cardinality: 2
data:
  - name: owner_id
    type: string
    generator: uuid
  - name: pets
    type: list
    content:
      from: cats
      fields:
        - name: cat_id
          refs:
            - cats.id
        - name: cat_name
          refs:
            - cats.name
```

This gives 100 animals (60 cats, 40 dogs) and 20 owners each with a `pets` list of 2 cats.

**Bernoulli factoring example** — the animals/cats/dogs datasets are mutually exclusive
(no animal is both a cat and a dog), so all cross-species Bernoulli segments are pruned.
The following pair illustrates genuine joint segments:

```yaml
# people.yaml — root, 100 rows
# employees.yaml — includes people, ratio: 0.7
# seniors.yaml   — includes people, ratio: 0.3
```

Being an employee and being a senior are independent, so all four Bernoulli segments of
`people` survive: `emp only` (~49), `emp & senior` (~21), `senior only` (~9),
`neither` (~21).

## High-level approach

The fakeset algorithm is correct and well-staged, but its conceptual framing is fragmented
across abstractions introduced incrementally: includes, content includes, pool siblings,
inner flats, prefills, collects. Each abstraction made local sense at the time but the
vocabulary doesn't immediately reveal the unified structure underneath.

Dataset definitions form a single **concept semi-lattice**: the partial order is constraint
specialisation — A ≤ B if A includes B (A is a more-constrained, narrower population).
The structure is a meet-semi-lattice: every pair of elements has a greatest lower bound
(meet, written `&`). Two elements with no include relationship (e.g. `animals` and
`owners`) meet at ⊥, the **bottom element** — the empty concept, representing a population
satisfying all constraints simultaneously, which is empty when constraints are contradictory.
With ⊥ well-defined for all pairs, the structure is one connected semi-lattice from the
outset. There is no single top element: the maximal named datasets are incomparable at the
top.

The algorithm builds an **execution DAG** from this semi-lattice by adding execution-dependency
edges for link relationships. These edges cross component boundaries and establish execution ordering between
elements that are incomparable in the semi-lattice. Topological sort of the DAG gives
execution order.

The algorithm has two symmetric phases:
1. **Push down** — field definitions, type constraints, and ref bindings propagate toward
   the most-constrained leaf nodes (atoms).
2. **Accumulate up** — generated values propagate upward toward parent nodes.

**Row count estimation** follows the same semi-lattice recursively: every non-maximal element
derives its row count from its parent — `parent.rows × ratio` for include children,
`source.rows × cardinality` for witness nodes. Row counts are resolved bottom-up
from declared values on root elements before generation begins.

**Atoms** are elements that cover ⊥ directly — the most-constrained nodes in the expanded
semi-lattice. Atoms are generated first; everything above them is assembled from their output.

## Planning

Planning transforms YAML definitions into an execution DAG via six steps.

### Step 1 — Build the initial semi-lattice

Read all YAML files. Each dataset becomes an element. The `include` stanza on a dataset
induces the partial order: if A includes B then A ≤ B (A is more constrained than B).

Named datasets that share no include relationships are incomparable elements whose meet
is ⊥. They are members of the same semi-lattice — ⊥ connects everything at the bottom.

**Row count at this step:** Each element's row count is declared via `rows:`, or derived
recursively as `parent.rows × ratio`, bottom-up from root declarations.

### Step 2 — Expand variants

Elements with `type: variant` fields are expanded. A dataset with N declared values on a
single variant field is replaced by N new elements at the same position in the semi-lattice
(same include edges; the original is removed). For multiple independent variant fields of
sizes n₁, n₂, …, the expansion is the cross-product ∏nᵢ.

Row fractions are proportional to variant `ratio` fields. This is a flat ∏nᵢ split — the
combinatorial expansion from Bernoulli factoring over lower-cover elements comes in Step 4.

In the worked example, `cats` expands to `cats/indoor` (30 rows) and `cats/outdoor`
(30 rows), both at the same semi-lattice position as the original.

### Step 3 — Decompose link relationships

A **link relationship** is declared via a `links:` stanza. The declaring dataset is the
**source dataset**; the stanza's target is the **linked dataset**. A link relationship
serves one or both purposes:

- **List generation**: a `content: {from: <ref>}` list field produces a list column in
  the source dataset, populated with rows drawn from the linked dataset.
- **Collect binding**: a `bind: X, reducer: <reducer>` ref binding accumulates values from
  generated cross-rows back into fields of the linked dataset. Supported reducers include
  `collect` (gather all values as a list), `max`, `min`, `sum`, and similar aggregates.

Each link relationship introduces three nodes:

**Staging node** (in the source dataset's include component): the source dataset is split.
The staging node generates all non-list fields of the source dataset and does not emit
output. It exists so that outer-scoped field values are materialised and indexed by
`_slot_idx` before the witness node reads them.

**Witness node**: an atom in the semi-lattice — it covers ⊥ directly and is incomparable
to all elements in both the source and linked include components. There are no lattice
ordering edges between it and those components; all cross-component connections are
execution edges added in Step 6. The witness node has the **linked dataset's schema**: its
fields are those of the linked dataset as drawn. It also carries one hidden column,
`_staging_refs`, which is a list of source-slot indices — recording which staging rows
drew this particular linked row. When the same linked row is drawn by multiple source
slots, all those slot indices accumulate in one `_staging_refs` list on a single witness
row, giving one witness row per unique linked-row draw with `_staging_refs` encoding the
full many-to-one pairing.

The witness node does not participate in Bernoulli factoring — it has no lattice ordering
edges and is incomparable to everything. It is the **staging node** that participates in
source-component Bernoulli factoring (Step 4), and this induces witness replication as a
consequence: each staging segment atom gets its own paired witness node via the outer-ref
edge. The (staging, witness) pair is the replicating unit. If owners factors into seniors
and non-seniors, the result is {seniors-staging + cats-in-seniors-owners witness} and
{non-seniors-staging + cats-in-non-seniors-owners witness}. Both witness nodes still cover
⊥ directly and remain incomparable to all elements in both include components. Per-segment
replication is a correctness requirement: a single unified witness node cannot apply
source-segment-specific constraints to linked-dataset selection (e.g. senior owners
drawing preferentially from indoor cats).

**Assembly node** (for link relationships with list fields): sits above the staging node
in the include ordering of the source component. After the witness batch is generated, it
groups witness rows by `_staging_refs` entry to reconstruct the per-source-slot pairings,
assembles list columns, evaluates expressions, and emits the source dataset's final output.
For collect-only links (no list field), the assembly node may be omitted; the staging node
becomes the full generation step.

The staging/witness/assembly ordering is necessary because the witness node sits between
them in execution order: the staging node must run first (outer-scoped refs), then the
witness node (draws from both staging and linked dataset atoms), then the assembly node
(groups by `_staging_refs`).

**Multiple links.** A source dataset may declare multiple `links:` stanzas. Each produces
an independent (staging atom, witness, assembly) triple drawing from its respective linked
dataset. The staging atom is shared across all link relationships on the same source
dataset — all witness nodes for that source read from the same staging batch via
`_staging_refs`. Witness node names must be unique; otherwise the three-node structure is
fully independent per link and there is no conflict between them.

> **Note:** witness nodes are added before Bernoulli factoring (Step 4) so that row-count
> estimation across components is correct from that point forward.

### Step 4 — Bernoulli factoring (joint distribution nodes)

Every node with two or more elements in its lower cover (the set of elements that directly
include it) is subject to joint distribution expansion:

1. **Enumerate segments**: form all 2^N subsets of the lower cover. Each subset is a
   segment — rows of the parent belonging to exactly those lower-cover elements and no
   others. Each segment becomes a new element of the semi-lattice.
2. **Product-Bernoulli weights**: assign each segment a prior weight equal to the product
   of marginal in/out probabilities for each lower-cover element.
3. **Constraint conflict pruning**: zero out any segment whose combined field constraints
   are mutually unsatisfiable (meet = ⊥). Redistribute the zeroed weight to surviving
   segments proportionally.
4. **IPF reweighting**: scale surviving weights so declared marginal ratios are exactly
   restored.
5. **Rounding**: for segments with a fractional row count, the correct approach is to
   include a single row with probability equal to the fractional weight (Bernoulli sampling
   at the segment level) rather than deterministic zeroing. Pruning zero-weight segments
   after this step is the primary mechanism keeping 2^N enumeration tractable.

After expansion, the segment nodes become the new lower cover of the parent. The process
is applied recursively.

In the people/employees/seniors example: lower cover {employees, seniors} produces four
segments; none are pruned. After IPF: employees marginal = 70, seniors marginal = 30.

In the animals example: lower cover {cats/indoor, cats/outdoor, dogs} produces 8 segments.
All cross-species and cross-variant combinations are pruned. Since ratios sum to 1.0, the
remainder is also zero. The three surviving segment nodes are identical to the original
lower-cover elements — in this degenerate case the lower-cover elements ARE the atoms.

### Step 5 — Mark atoms

An atom is a least element strictly greater than ⊥ — an element that covers ⊥ directly.
After Step 4, atoms are:
- Remainder segment nodes (parent rows not covered by any lower-cover element)
- Segment nodes covering exactly one combination of lower-cover constraints, including
  fully-combined joint nodes (A & B & C & …)
- Any lower-cover element whose only lower-cover element is ⊥ (the degenerate case)
- Witness nodes (they cover ⊥ by construction — Step 3)

At this stage — before the execution edges added in Step 6 — atoms within an include
component are independent of each other. Step 6 introduces cross-component seed edges
that constrain the achievable execution parallelism. A library-level DAG scheduler is the
preferred path for exploiting available parallelism.

### Step 6 — Add execution edges (DAG formation)

The semi-lattice captures constraint narrowing but not the execution dependencies
introduced by link relationships. Four types of execution edge are added:

**Seed edge**: from each atom of the linked dataset (the segment-level nodes produced by
Steps 4–5, not individual data rows) to the witness node. Ensures the full linked
batch is generated before witness generation begins. The witness node and the
linked dataset's atoms are in different include components and therefore incomparable in
the semi-lattice; the seed edge establishes execution ordering between them.

**Outer ref edge**: from the staging node to the witness node. Ensures the source
dataset's non-list fields are materialised before the witness node reads outer-scoped
ref values from them.

**Cardinality edge**: from the witness node to the assembly node. Ensures the
witness batch is complete before the assembly node groups and folds it.

**Collect edge**: from the witness node to the linked dataset's emit step. When
the link relationship includes collect bindings, ensures accumulation into linked-dataset
fields fires before the linked dataset writes its output.

Topological sort of the resulting DAG gives execution order.

**Link edges and the ordering relation.** Adding execution edges does not modify the
semi-lattice ordering — the partial order is defined entirely by include relationships.
The meet (⊓) therefore remains well-defined only for elements connected by include edges.
Two elements whose only connection is an execution edge have no meaningful meet beyond ⊥.
Seed and outer-ref edges may run in a direction that contradicts what the lattice ordering
would imply for execution; this is expected and correct — the execution DAG is a
superset of the lattice structure, not a subgraph of it.

## Diagrams

Diagrams 1–3 are **lattice diagrams**: they show the semi-lattice structure (include
ordering) with ⊥ at the bottom. Diagram 4 is a **DAG diagram**: it shows execution
dependencies and omits ⊥. Solid arrows are ordering (include) edges. Dashed arrows are
execution edges.

### Diagram 1 — Initial semi-lattice (after Step 1)

```mermaid
graph BT
    bot["⊥"]
    cats["cats (60)"] --> animals["animals (100)"]
    dogs["dogs (40)"] --> animals
    bot --> cats
    bot --> dogs
    bot --> owners["owners (20)"]
```

`cats`, `dogs`, and `owners` are atoms at this stage (each covers ⊥ directly). `animals`
and `owners` are incomparable; their meet is ⊥.

### Diagram 2 — After variant expansion and link decomposition (after Steps 2–3)

```mermaid
graph BT
    bot["⊥"]
    cats_in["cats/indoor (30)"] --> animals["animals (100)"]
    cats_out["cats/outdoor (30)"] --> animals
    dogs["dogs (40)"] --> animals
    bot --> cats_in
    bot --> cats_out
    bot --> dogs
    owners_staging["owners (staging)"] --> owners_asm["owners (assembly)"]
    bot --> owners_staging
    bot --> cats_foreign["cats-in-owners\n(witness)"]
    owners_staging -. "outer ref (Step 6)" .-> cats_foreign
    cats_in -. "seed (Step 6)" .-> cats_foreign
    cats_out -. "seed (Step 6)" .-> cats_foreign
    cats_foreign -. "cardinality (Step 6)" .-> owners_asm
```

`cats` is replaced by `cats/indoor` and `cats/outdoor`. `owners` splits into a staging
node and an assembly node. The witness node `cats-in-owners` is an atom (solid edge from
⊥), incomparable to all other elements — it has no solid lattice edges to the cats or
owners components. All cross-component connections are execution edges (dashed).

### Diagram 3A — After Bernoulli factoring: genuine joint segments with recursive factoring

This diagram extends the people/employees/seniors example by adding `remote` (includes
`employees`, ratio 0.4, 28 rows), illustrating how Bernoulli factoring applies recursively
when an intermediate node itself has a lower cover.

```mermaid
graph BT
    bot["⊥"]
    eo_rem["emp·remote\n(~20)"] --> remote["remote (28)"]
    eo_nrem["emp·non-remote\n(~29)"] --> employees["employees (70)"]
    es_rem["emp·senior·remote\n(~8)"] --> remote
    es_rem --> seniors["seniors (30)"]
    es_nrem["emp·senior·non-remote\n(~13)"] --> employees
    es_nrem --> seniors
    so["senior only (~9)"] --> seniors
    neit["neither (~21)"] --> people["people (100)"]
    remote --> employees
    employees --> people
    seniors --> people
    bot --> eo_rem
    bot --> eo_nrem
    bot --> es_rem
    bot --> es_nrem
    bot --> so
    bot --> neit
```

Six atoms cover ⊥ directly. Bernoulli factoring at the `people` level produces four
population segments ({employees, seniors}); factoring at the `employees` level splits those
into remote and non-remote sub-variants. `remote` is an intermediate node (covered by two
atoms); `employees` and `seniors` are intermediate nodes above them; `people` is the root.
The four emp-related atoms each carry the intersection of their people-level and
employees-level segment constraints. Bernoulli factoring is applied recursively bottom-up:
each node with a non-empty lower cover is expanded before its parent is considered.

### Diagram 3B — After Bernoulli factoring: degenerate case (animals example)

```mermaid
graph BT
    bot["⊥"]
    cats_in["cats/indoor (30)"] --> animals["animals (100)"]
    cats_out["cats/outdoor (30)"] --> animals
    dogs["dogs (40)"] --> animals
    bot --> cats_in
    bot --> cats_out
    bot --> dogs
```

In the animals example, all cross-species and cross-variant Bernoulli segments are pruned
(mutually exclusive field constraints), and ratios sum to 1.0 so the remainder is zero.
The surviving segments are identical to the original lower-cover elements: `cats/indoor`,
`cats/outdoor`, and `dogs` become their own atoms directly — the lower-cover elements ARE
the atoms. No intermediate segment nodes exist between them and ⊥. This degenerate case
is common when include siblings partition the parent population cleanly.

### Diagram 4 — Execution DAG (after Step 6, animals example)

```mermaid
graph BT
    cats_in["cats/indoor\n(atom)"] --> animals["animals"]
    cats_out["cats/outdoor\n(atom)"] --> animals
    dogs["dogs\n(atom)"] --> animals
    owners_staging["owners\n(staging atom)"] --> owners_asm["owners\n(assembly)"]
    cats_foreign["cats-in-owners\n(witness)"]
    owners_staging -->|"outer ref"| cats_foreign
    cats_in -->|"seed"| cats_foreign
    cats_out -->|"seed"| cats_foreign
    cats_foreign -->|"cardinality"| owners_asm
```

No ⊥ in DAG diagrams. `cats/indoor` and `cats/outdoor` both emit to the same `cats.csv`
output (a shared-output union step above both atoms, omitted here for clarity). The seed
edges and outer-ref edge establish execution ordering between elements that are
incomparable in the semi-lattice (different include components).

## Execution

Execution traverses the DAG in topological order.

### Phase 1 — Generate atoms (leaf-first)

For each atom node in topological order:
- **Segment atoms**: generate rows using field generators and local constraints. No inherited
  field values from above — field definitions were pushed down during planning. Staging atoms
  (source component) additionally carry `_slot_idx` to index source rows for the witness node.
- **Witness nodes**: for each unique linked row drawn (across all source slots), generate
  one witness row carrying the linked dataset's fields and a `_staging_refs` list of all
  source-slot indices that drew this linked row. Seed and outer-ref edges guarantee both
  the linked batch and the staging batch are available.

Collect bindings fire after the witness batch is complete (collect edge ensures ordering).

When a linked dataset itself has a `links:` stanza (chained links), it is also split into
staging/witness/assembly nodes. Topological sort handles the ordering naturally: innermost
linked atoms are generated first, their witness batches complete and collect bindings fire,
their assembly nodes emit — all before the outer witness node's seed edge is satisfied.

### Phase 2 — Accumulate upward (child-first, parent-last)

For each non-atom node in topological order:
- **Inherited-field accumulation**: LEFT JOIN on `_row_idx` from each child batch. Fields
  present in a child are inherited; remaining fields are generated fresh.
- **List assembly**: for assembly nodes, witness rows are grouped by `_staging_refs` entry
  to reconstruct per-source-slot pairings, then folded into list columns.
- **Expression evaluation**: after all inherited fields are present. Evaluation order follows YAML field declaration order, as established during planning's push-down phase.
- **Filter hidden fields**: strip `hidden: true` fields before emitting.
- **Emit**: write to output file (CSV/Parquet/JSONL/JSON).

Within a component, all atoms are generated before any accumulation. A component completes
both phases before any downstream component starts.

## Glossary

| Term | Definition |
|------|------------|
| **⊥ (bottom)** | Greatest lower bound of all elements: the empty concept. Unsatisfiable Bernoulli segments reduce to ⊥ and are pruned. |
| **Atom** | An element covering ⊥ directly: a least element strictly greater than ⊥. Atoms generate rows from scratch with no inherited field values. |
| **Assembly node** | Virtual node (Step 3) above the staging node. Groups witness rows by `_staging_refs` entry to reconstruct per-source-slot pairings, folds into list columns, evaluates expressions, and emits the source dataset's output. |
| **Cardinality edge** | Execution edge from a witness node to an assembly node. Ensures the witness batch is complete before list assembly. |
| **Collect edge** | Execution edge from a witness node to the linked dataset's emit step. Ensures collect/reduce accumulation fires before the linked dataset writes output. |
| **Inheritance** | A ≤ B if A's dataset definition includes B. A inherits B's field definitions. |
| **Linked dataset** | The target dataset in a `links:` stanza. Its generated rows are drawn from by the witness node. |
| **Lower cover** | The set of immediate predecessors of an element: the elements it directly includes. |
| **Meet (⊓ or &)** | Greatest lower bound. For lower-cover siblings, meet(A, B) is the most-constrained segment satisfying both sets of constraints. The formal underpinning of Bernoulli factoring. |
| **Outer ref edge** | Execution edge from the staging node to the witness node. Ensures the source dataset's non-list fields are materialised before witness generation. |
| **Remainder segment** | The segment for rows of a parent not covered by any lower-cover element. |
| **Seed edge** | Execution edge from each atom of the linked dataset to the witness node. Ensures the full linked batch is generated before witness generation begins. *(Proposed alternative: **draw edge** — emphasises that the witness draws rows from the linked dataset, rather than that the linked dataset seeds the witness. Either name is defensible; this document uses "seed edge" pending a final decision.)* |
| **Segment node** | A node created by Bernoulli factoring: represents rows belonging to exactly a specified subset of lower-cover elements. |
| **Source dataset** | The dataset declaring a `links:` stanza. |
| **Source slot** | One row of the staging batch, identified by its `_slot_idx` value. Each entry in a witness row's `_staging_refs` list is the `_slot_idx` of a source slot that drew that linked row. "Source slot" and "staging row" are interchangeable; "source slot" is preferred when emphasising the draw relationship. |
| **Staging node** | Virtual node (Step 3) created by splitting the source dataset. Generates non-list fields and does not emit. Provides outer-scoped field values (via `_slot_idx`) to the witness node. |
| **Witness node** | An atom (covers ⊥) created per link relationship. Incomparable to all elements in both the source and linked include components; connected to them only via execution edges. Has the **linked dataset's schema** plus a hidden `_staging_refs` list column recording which source slots drew each linked row. One witness row per unique linked-row draw. |

## Concept map

| YAML feature | Semi-lattice concept | Notes |
|---|---|---|
| `include: {file: X}` | Partial order: A ≤ B | One include per dataset |
| `ratio:` on include | Marginal probability for Bernoulli factoring | |
| `rows:` | Declared row count for root elements | |
| Multiple datasets sharing a parent | Lower cover of the parent node | |
| `type: variant` field | Horizontal expansion into ∏nᵢ elements | |
| `links:` stanza | Link relationship → staging + witness + assembly | |
| `content: {from: <ref>}` | List generation: `from:` names the `ref:` of the linked dataset; witness batch grouped by `_staging_refs`, folded by assembly node | |
| `bind: X, reducer: collect` | Collect edge; accumulates source-context field values from witness rows back into linked-dataset fields | |
| `content: {project: "<ref>.<field>"}` | Projection from witness batch to scalar list | |
| `hidden: true` | Participates in constraint resolution; stripped before emit | |
| Bernoulli segment enumeration | 2^N elements forming the new lower cover | |
| Constraint conflict pruning | Pruning segment nodes that reduce to ⊥ | |
| IPF reweighting | Restoring marginal ratios after pruning | |
| `_slot_idx` sentinel | Integer index of a source slot in the staging batch. Carried on each staging atom row. Each entry in a witness row's `_staging_refs` list is a `_slot_idx` value identifying the staging row that drew this linked row. Also present in the inner-flat execution artifact (derived by unnesting `_staging_refs`), where it pairs each atom row with a source slot. | |
| `_staging_refs` (witness hidden column) | List of source-slot indices that drew this linked row; the witness-level encoding of source-to-linked pairings | |
| `_linked_idx` (execution artifact, formerly `_pool_idx`) | Linked-row index in the inner-flat table; not present on witness rows | |
| Prefill / field inheritance | Accumulate upward: inherit child column into parent batch | |

## Terminological changes

The terms in the left column appear in the existing codebase and documentation. They are to be fully deprecated in favour of the proposed terms, alongside structural refactoring and renaming to bring the implementation into alignment with the new framing.

| Current term | Proposed term | Rationale |
|---|---|---|
| sibling | lower cover element | More precise. |
| sibling group | parent + lower cover | The unit of Bernoulli factoring. |
| sibling set | lower cover | Exact order-theoretic term. |
| nested include / rich list | list-link field | Names the YAML feature. |
| `content.group:` | `content.from:` | Names the linked dataset's `ref:` key; `from:` is more transparent about the draw direction. |
| inner flat | *(drop entirely)* | This term names a transient implementation detail — the junction table produced by unnesting `_staging_refs` inside `execute_witness`. In the new model it is an anonymous intermediate with no theory role and no external callers. It should be dropped as a named concept rather than renamed. |
| prefill | inherited field | Emphasises the accumulate-up direction. |
| pool sibling | witness node | The atom introduced in Step 3 with linked-dataset schema + `_staging_refs`. |
| pool / pool slot | linked batch / linked row | Removes implementation-specific naming. |
| `_pool_idx` | `_linked_idx` (execution artifact) | Present in the inner flat; not on witness rows. |
| scalar node / partial node | staging node | Stages outer data before witness generation. |
| include chain | include component | A maximal include-connected subgraph. |
| `includes` → `include` | ✓ done in MULT-1 | One include per dataset. |
| `distribution` → `ratio` | ✓ done in MULT-1 | Consistent with probability framing. |

## Existing alignment

Where the implementation already maps cleanly to the new framing, only renaming is needed. Symbol names that do not transparently align with the new framing will be renamed even where a conceptual mapping exists — for example, `GenerateInnerFlat` maps to "witness node execution" but the name is misleading and will become `GenerateWitness`. The goal is full terminological coherence between specification and implementation, not just a conceptual overlay.

| Current implementation | New concept | Notes |
|---|---|---|
| `build_dag` + topo sort | Semi-lattice construction + execution order | Already correct |
| `plan_segments` (Bernoulli, IPF, pruning) | Steps 4–5 | Core algorithm unchanged |
| `grow_parent_from_children` | Phase 2 prefill accumulation | Already a LEFT JOIN on `_row_idx` |
| `filter_hidden_columns` | Hidden field stripping | |
| `evaluate_expressions` | Expression evaluation in Phase 2 | |
| `_slot_idx` sentinel | Source-row index | Clean mapping |
| `_pool_idx` sentinel | `_linked_idx` in inner-flat execution artifact | Rename only; not present on witness rows |
| `CollectToPool` step | Collect edge semantics | |
| `GenerateDataset(skip_emit=true)` | Staging node | Rename needed |
| `AssembleNestedInclude` | Assembly node | Rename needed |
| `GenerateInnerFlat` | Witness node execution (inner-flat artifact) | Rename needed; witness schema refactor deferred |

## Gap analysis

**Naming gaps (low effort)**

In `plan.rs`:
- `ExecutionStep::GenerateInnerFlat` → `GenerateWitness`
- `ExecutionStep::AssembleNestedInclude` → `AssembleFromWitness`
- Helper `emit_nested_include_steps` → `emit_witness_steps`
- `Sibling` struct and `build_sibling_groups` function → lower-cover vocabulary (`LowerCoverElement`, `build_lower_cover_groups`)

In `executor.rs`:
- `execute_inner_flat` function → `execute_witness`
- `_pool_idx` column name → `_linked_idx` (execution artifact inside `execute_witness`; not present on witness rows in the target model)
- `pool_*` local variable names within the inner-flat path → `linked_*`
- `_outer_idx` / `outer_path` vocabulary where it refers to staging → `_staging_*`

In `segment.rs`:
- Internal variable names and comments use "sibling" and "sibling group" throughout → lower-cover vocabulary

In `graph.rs`:
- `content_includes: Vec<ContentInclude>` accumulation and surrounding comments use "pool sibling" vocabulary

In CLAUDE.md:
- Glossary entries: `sibling`, `sibling group`, `pool sibling`, `inner flat`, `prefill`, `pool / pool slot`, `_pool_idx` all carry old vocabulary
- "Core architectural tenet" section uses `pool partner`, `pool datasets`, `pool pre-generation`
- Execution pipeline step list names `GenerateInnerFlat` and `AssembleNestedInclude`
- Module map `executor.rs` description uses "inner flat", "pool slot", "pool pre-generation"

**Structural gaps (higher effort)**

- **One witness node per staging segment** (`plan.rs`: `emit_nested_include_steps`, `executor.rs`: `execute_inner_flat`): `build_plan` creates one `GenerateInnerFlat` step per list-link field regardless of how many staging segments the source dataset has. The correct model is one paired witness node per staging segment atom. Fixing this requires `build_plan` to enumerate staging segment atoms and emit one `GenerateWitness` step per (staging-segment, link-relationship) pair; `AssembleFromWitness` must then aggregate across N witness batches.

- **`_staging_refs` witness schema** (`executor.rs`: `execute_inner_flat`, `AssembleNestedInclude`): the current inner flat is a junction table — one row per (source-slot, linked-row) pair, carrying `_slot_idx` + `_linked_idx` + linked fields. The target witness schema is one row per unique linked row, with `_staging_refs` as a hidden list column of source-slot indices. Requires refactoring both `execute_inner_flat` (to produce the new schema) and `AssembleNestedInclude` (to unnest `_staging_refs` rather than read from a pre-unnested junction table).

- **Explicit outer-ref edge** (`plan.rs`, `executor.rs`): `execute_inner_flat` reads `computed[outer_path]` without a declared DAG edge. Making this an explicit outer-ref edge in `ExecutionStep` / the DAG would make the dependency visible to the topo-sort and enable lattice-aware scheduling.

- **Explicit staging/witness/assembly DAG nodes** (`plan.rs`): the three-node decomposition is currently represented as two ad hoc plan steps (`GenerateInnerFlat` + `AssembleNestedInclude`) without explicit node objects. An explicit node representation with typed edges (seed, outer-ref, cardinality, collect) would enable cleaner phase separation, correct per-segment replication, and lattice-aware expression evaluation.

- **Cardinality validation against eligible pool size** (`plan.rs`, new planning-phase check): cardinality constraints must be validated against the per-segment eligible pool size after Bernoulli factoring (Step 4). Rules: for fixed cardinality N, error if `eligible_pool_size < N`; for min-max cardinality, error if `eligible_pool_size < min`, and silently cap `max` at `eligible_pool_size` if `eligible_pool_size < max`. This check cannot run at YAML parse time — it requires the post-factoring per-segment pool sizes.

**Test/fixture gaps**

- No fixture for a source dataset with two distinct `links:` stanzas (two witness nodes keying into one staging batch). Closest existing: `hidden_collect_binding` covers collect on a single link; no test covers multiple independent link relationships on the same source.
- No test for witness-per-segment correctness: a source dataset whose staging node has two or more segments should produce a paired witness per segment.
- No test for chained links (a linked dataset itself has a `links:` stanza). Topological ordering should sequence innermost atoms first; this property is unverified by any existing fixture.
- Existing fixtures `rich_list`, `bernoulli_rich_list`, `rich_list_plain`, and `hidden_collect_binding` test the current junction-table inner-flat model. They will need updating when the `_staging_refs` witness schema refactor lands.

---

## Implementation Plan

### Overview

Seven stages, each independently mergeable. Early stages are documentation and
mechanical renames; later stages make structural changes to the planner and executor.
The constraint throughout: no old vocabulary is left in any file after each stage merges.

| Stage | Title | Files primarily affected | Risk | Status |
|-------|-------|--------------------------|------|--------|
| 1 | Documentation | `../../CLAUDE.md`, `..` | None | ✓ Complete |
| 2 | Naming pass | All `.rs`, `../../src/main.rs`, test strings | Low | ✓ Complete |
| 3 | Staging node as explicit step | `plan.rs`, `executor.rs`, `../../src/main.rs` | Low | ✓ Complete |
| 4 | `_staging_refs` witness schema | `executor.rs`, `plan.rs`, test fixtures | High | ✓ Complete |
| 5 | Per-segment witness correctness | `plan.rs`, `executor.rs`, new fixtures | High | Next |
| 6 | Cardinality validation | `plan.rs`, new test fixtures | Medium | |
| 7 | Outer-ref edge + final cleanup | `graph.rs`, `plan.rs`, fixture dirs, all | Low | |

---

### Stage 1 — Documentation ✓ Complete

No code changes. Updated `../../CLAUDE.md` with the full semi-lattice glossary (atom, lower cover,
lower cover group, staging node, witness node, assembly node, inherited field, etc.), rewrote
the "Core architectural framing" section, renamed "Sibling segmentation" to "Lower cover
segmentation (Bernoulli factoring)", and updated the execution pipeline step list and module
map. `../../README.md` received matching glossary additions and YAML example updates.

*Full plan: [`REFRAME-1-stage-1.md`](REFRAME-1-stage-1.md)*

---

### Stage 2 — Naming pass ✓ Complete

Pure renames across all modules, CLI, tests, and fixture YAML files. No algorithmic changes.
Every symbol, constant, CLI flag, doc comment, and printed string now uses the new vocabulary:
`LowerCoverMember` (was `Sibling`), `is_witness_source` (was `is_pool`), `GenerateWitness` /
`AssembleFromWitness` / `AccumulateToLinked` (was `GenerateInnerFlat` / `AssembleNestedInclude` /
`CollectToPool`), `InheritedField` (was `PrefillSource`), `--max-lower-cover` (was
`--max-siblings`), `content.from` (was `content.group`). 23 fixture YAML files had `group:`
renamed to `from:`.

*Full plan: [`REFRAME-1-stage-2.md`](REFRAME-1-stage-2.md)*

---

### Stage 3 — Staging node as explicit execution step ✓ Complete

Introduced `GenerateStagingNode` and `GenerateStagingLowerCoverGroup` as explicit step variants,
replacing the overloaded `skip_emit` / `skip_parent_emit` booleans on `GenerateDataset` and
`GenerateLowerCoverGroup`. The staging role (generate scalar fields only; defer list assembly)
and the collect-target deferral role (`defer_emit: bool`, emit after `AccumulateToLinked`) are
now self-documenting from the step type alone. Shared helpers `execute_dataset_core` and
`execute_lower_cover_group_core` serve both the staging and non-staging paths.

*Full plan: [`REFRAME-1-stage-3.md`](REFRAME-1-stage-3.md)*

---

### Stage 4 — `_staging_refs` witness schema ✓ Complete

The core structural change. The witness batch is now one row per **unique** linked-row draw,
with `_staging_refs: List<UInt32>` recording all source-slot indices that drew that linked row.
The old junction-table model (one row per (source-slot, linked-row) pair with `_slot_idx +
_linked_idx + inner content fields`) has been replaced.

*Implemented in `../../lib/executor.rs`: `execute_witness`, `unnest_staging_refs`,
`execute_assemble_from_witness`, `execute_accumulate_to_linked`. New fixture:
`../../tests/fixtures/execute/staging_refs_dedup`. New test:
`test_staging_refs_deduplicates_linked_rows`. All 174 tests pass.*

---

### Stage 5 — Per-segment witness correctness ✓ Complete

One `GenerateWitness` step per (staging segment, list-link field). Each witness covers a
contiguous slot range (`slot_start`/`slot_count`) and filters the linked batch to rows
matching that segment's field constraints. Staging batches concatenated in segment order
(no shuffle) to preserve slot indices. `AssembleFromWitness` unions all per-segment
witness batches before unnesting. Cumulative `Collect` reducer in `AccumulateToLinked`:
subsequent calls carry forward existing list items rather than replacing them.

*Implemented in `../../lib/plan.rs` (`emit_witness_steps`, `push_with_list_link_steps`,
`GenerateWitness`, `AssembleFromWitness`), `../../lib/executor.rs` (`execute_witness`,
`execute_lower_cover_group_core`, `execute_assemble_from_witness`,
`execute_accumulate_to_linked`). New fixture:
`../../tests/fixtures/execute/segmented_list_link`. New test:
`test_segmented_list_link_assembles_correctly`. All tests pass.*

*Full plan: [`REFRAME-1-stage-5.md`](REFRAME-1-stage-5.md)*
---

### Stage 5.5 — Cumulative scalar reducers for multi-segment staging nodes ✓ Complete

Scalar `AccumulateToLinked` reducers (Sum, Max, Min, TakeOne) are now cumulative across
Bernoulli segments. Subsequent calls combine element-wise (add/max/min for mapped rows;
existing value unchanged for unmapped rows) rather than overwriting with the default.
`TakeOne` (renamed from `TakeFirst`, backward-compatible via serde alias) keeps the first
segment's captured value unchanged on subsequent calls.

*Implemented in `../../lib/executor.rs` (`execute_accumulate_to_linked`,
`accumulate_scalar_cumulative`), `../../lib/models.rs` (`Reducer::TakeOne`), `../../lib/validate.rs`.
New fixture: `../../tests/fixtures/execute/segmented_scalar_reduce`. New test:
`test_segmented_scalar_sum_accumulates_correctly`. All tests pass.*

*Full plan: [`REFRAME-1-stage-5.5.md`](REFRAME-1-stage-5.5.md)*

---

### Stage 6 — Cardinality validation against eligible linked-dataset size ✓ Complete

`check_reinforcement_zero_feasibility` renamed to `check_cardinality_feasibility` and
extended to cover two failure classes: (1) empty linked dataset (all reinforcement modes —
bail before any sampling); (2) without-replacement infeasibility — Fixed(N) > n_eligible
bails, Uniform{min} > n_eligible bails (new check), Uniform{max} > n_eligible is handled
by a silent runtime cap in `execute_witness` rather than a plan-time error.
`max_cardinality_bound` helper removed.

*Implemented in `../../lib/plan.rs` (`check_cardinality_feasibility`), `../../lib/executor.rs`
(Uniform max-cap in `execute_witness`). New fixtures:
`../../tests/fixtures/validation/card_fixed_pool_too_small`,
`../../tests/fixtures/validation/card_uniform_min_too_large`,
`../../tests/fixtures/execute/no_replacement_max_cap`. New tests:
`card_fixed_pool_too_small_errors`, `card_uniform_min_too_large_errors` (plan_tests.rs),
`test_no_replacement_max_cap` (executor_tests.rs). All tests pass.*

*Full plan: [`REFRAME-1-stage-6.md`](REFRAME-1-stage-6.md)*

---

### Stage 7 — Fixture renames, vocabulary cleanup, and final consistency pass ✓ Complete

Pure naming/cleanup pass — no behaviour changes. Eliminated all surviving old vocabulary
(`pool_scoped`, `sample_pool_*`, `link_content`, `pool row`, `pool field type`, etc.)
from comments, variable names, function names, and fixture paths. Renamed three execute
fixture directories (`link_content` → `list_link`, `bernoulli_link_content` →
`bernoulli_list_link`, `link_content_plain` → `list_link_flat`) and five validation
fixture directories. Added `//!` doc comments to all 13 `lib/*.rs` files. Added a
`#[cfg(debug_assertions)]` assertion in `build_plan` verifying staging steps precede
witness steps. Documented the staging → witness ordering dependency in `graph.rs`.

*Implemented across all `lib/*.rs` modules, `../../tests/executor_tests.rs`,
`../../tests/validate_tests.rs`, `../../tests/plan_tests.rs`, `../../tests/rewrite_tests.rs`,
`../../tests/dag_tests.rs`, `../../CLAUDE.md`. All tests pass.*

*Full plan: [`REFRAME-1-stage-7.md`](REFRAME-1-stage-7.md)*

---

### Resolved design decisions

1. **`content.from` YAML key** (was `content.group`): renamed to `from:` — short and
   transparent ("draw list items from this linked dataset"). Applied throughout worked
   examples, Appendix A, and all fixture YAMLs in Stage 2. Existing `group:` YAML files
   will need migration; serde alias added during the transition window.

2. **`--max-lower-cover` CLI flag**: keep this name. The help string and any error messages
   that reference the limit will explain it in plain terms (e.g. "datasets that share a
   common parent").

3. **Staging step variants** (Stage 3): two distinct step variants —
   `GenerateStagingNode` and `GenerateStagingLowerCoverGroup` — both dispatching to a
   single shared executor function parameterised by `is_staging: bool`. Theory-transparent
   step names, zero code duplication. `GenerateLowerCoverGroup` (renamed from
   `GenerateSiblingGroup` in Stage 2) is the normal, emitting variant.

4. **`is_witness_source` on `LowerCoverMember`** (was `is_pool`): this flag marks a lower
   cover member whose rows will be sampled to generate the witness batch (arose from a
   list-link field rather than a top-level `include:`). `is_witness_source` is precise and
   theory-aligned.

---

## Appendix A — Link relationship decomposition, case by case

Step 3 produces the same three-node structure in all six cases. What varies is:

1. **Witness rows per source slot** — determined by `cardinality:` in the `links:` stanza.
2. **Collect bindings** — whether witness-row values accumulate back into linked-dataset fields.
3. **Assembly mode** — whether the assembly node reconstructs source rows from witness rows
   by lookup or expansion (cases 1–4) or by list fold (cases 5–6).

The witness node is always an atom (covers ⊥ directly) and is always incomparable to both
the source and linked components, regardless of case.

---

### Case 1 — Scalar join, 1:1

**Shape:** one source slot → one linked row (`cardinality: 1`). No collect binding.
Linked-dataset fields appear as scalars on the source output.

```yaml
# departments.yaml
name: departments
rows: 10
data:
  - name: dept_id
    type: string
    generator: uuid
  - name: dept_name
    type: string
    generator: word
```

```yaml
# employees.yaml
name: employees
rows: 100
links:
  - file: departments.yaml
    ref: dept
    cardinality: 1
data:
  - name: emp_id
    type: string
    generator: uuid
  - name: dept_id
    refs:
      - dept.dept_id
  - name: dept_name
    refs:
      - dept.dept_name
```

Decomposition:

| Node | Schema | Role |
|------|--------|------|
| employees — staging | `_slot_idx`, `emp_id` | Non-linked source fields; no emit. |
| dept-in-employees — witness (atom) | `dept_id`, `dept_name`; hidden: `_staging_refs` (list) | One row per unique dept drawn; `_staging_refs` lists the drawing employee slots. |
| employees — assembly | `emp_id`, `dept_id`, `dept_name` | Joins staging with witness via `_staging_refs`; emits one row per staging slot. |

Witness row count ≤ 10 (one row per unique department drawn; with 100 employees drawing
from 10 departments, all departments are typically represented). Assembly resolves each
employee's department by finding the witness row whose `_staging_refs` contains that
employee's `_slot_idx`.

Execution edges: seed from `departments` atoms → witness; outer-ref from staging → witness;
cardinality from witness → assembly.

---

### Case 2 — Flat expansion, 1:N (no collect)

**Shape:** one source slot → N linked rows (`cardinality: N`). No collect binding.
Without a list field, witness rows expand the source row count: the assembly node emits
one output row per witness row, producing a flat join table of (source-slot, linked-row)
pairs.

```yaml
# tags.yaml
name: tags
rows: 20
data:
  - name: tag_id
    type: string
    generator: uuid
  - name: tag_name
    type: string
    generator: word
```

```yaml
# articles.yaml
name: articles
rows: 50
links:
  - file: tags.yaml
    ref: tag
    cardinality: 3
data:
  - name: article_id
    type: string
    generator: uuid
  - name: tag_id
    refs:
      - tag.tag_id
  - name: tag_name
    refs:
      - tag.tag_name
```

Decomposition:

| Node | Schema | Role |
|------|--------|------|
| articles — staging | `_slot_idx`, `article_id` | Non-linked source fields; no emit. |
| tag-in-articles — witness (atom) | `tag_id`, `tag_name`; hidden: `_staging_refs` (list) | One row per unique tag drawn; `_staging_refs` lists all drawing article slots. |
| articles — assembly | `article_id`, `tag_id`, `tag_name` | Unnests `_staging_refs`; emits one row per (article-slot, tag) draw. |

Witness row count ≤ 20 (one row per unique tag drawn; with 150 draws from 20 tags, most
or all tags are typically represented). Assembly output = 150 rows, produced by unnesting
`_staging_refs` across all witness rows. No list folding.

> **Relationship to case 5.** Cases 2 and 5 share identical witness generation. The only
> difference is the assembly mode: case 2 expands (flat join), case 5 folds (list column).
> Adding a `content: {group: tag}` list field to `articles.yaml` and switching assembly to
> fold converts case 2 into case 5.

---

### Case 3 — Collect-only, N:1

**Shape:** N source slots each draw one linked row (`cardinality: 1`). Multiple source
slots may independently draw the same linked row. A collect binding aggregates values
from all N source slots that drew each linked row back into linked-dataset fields. No
list column on the source output.

```yaml
# accounts.yaml
name: accounts
rows: 10
data:
  - name: account_id
    type: string
    generator: uuid
  - name: transaction_count
    type: integer
    generator: constant
    value: 0
```

```yaml
# transactions.yaml
name: transactions
rows: 200
links:
  - file: accounts.yaml
    ref: account
    cardinality: 1
data:
  - name: txn_id
    type: string
    generator: uuid
  - name: account_id
    refs:
      - account.account_id
      - bind: account.transaction_count
        reducer: collect
```

Decomposition:

| Node | Schema | Role |
|------|--------|------|
| transactions — staging | `_slot_idx`, `txn_id` | Non-linked source fields; no emit. |
| account-in-transactions — witness (atom) | `account_id`; hidden: `_staging_refs` (list), collect ref | One row per unique account drawn; `_staging_refs` lists all drawing transaction slots. |
| transactions — assembly | `txn_id`, `account_id` | Joins staging with witness via `_staging_refs`; emits one row per staging slot. |
| accounts — emit | `account_id`, `transaction_count` | Receives collected values from witness via collect edge. |

Witness row count ≤ 10 (one row per unique account drawn; with 200 transactions drawing
from 10 accounts, all accounts are typically represented). Each witness row's `_staging_refs`
contains ~20 transaction-slot indices. The collect reducer processes `_staging_refs` to
populate `transaction_count`.

Execution edges: seed from `accounts` atoms → witness; outer-ref from staging → witness;
cardinality from witness → assembly; **collect edge from witness → accounts emit step**.

---

### Case 4 — Flat expansion with collect, N:M

**Shape:** one source slot → M linked rows (`cardinality: M`). N source slots exist.
A collect binding accumulates from all N×M witness rows into linked-dataset fields. No
list column. This is cases 2 and 3 combined.

```yaml
# products.yaml
name: products
rows: 20
data:
  - name: product_id
    type: string
    generator: uuid
  - name: basket_appearances
    type: integer
    generator: constant
    value: 0
```

```yaml
# baskets.yaml
name: baskets
rows: 50
links:
  - file: products.yaml
    ref: product
    cardinality: 3
data:
  - name: basket_id
    type: string
    generator: uuid
  - name: product_id
    refs:
      - product.product_id
      - bind: product.basket_appearances
        reducer: collect
```

Decomposition:

| Node | Schema | Role |
|------|--------|------|
| baskets — staging | `_slot_idx`, `basket_id` | Non-linked source fields; no emit. |
| product-in-baskets — witness (atom) | `product_id`; hidden: `_staging_refs` (list), collect ref | One row per unique product drawn; `_staging_refs` lists all drawing basket slots. |
| baskets — assembly | `basket_id`, `product_id` | Unnests `_staging_refs`; emits one row per (basket-slot, product) draw. |
| products — emit | `product_id`, `basket_appearances` | Receives collected values from witness via collect edge. |

Witness row count ≤ 20 (one row per unique product drawn from 150 draws). Each witness
row's `_staging_refs` lists on average ~7.5 basket-slot indices. The collect reducer
processes `_staging_refs` to populate `basket_appearances`. Assembly output = 150 rows.

> **Relationship to case 6.** Cases 4 and 6 share identical witness generation. Adding a
> `content: {from: product}` list field and switching assembly to fold converts case 4
> into case 6.

---

### Case 5 — List field, 1:N (no collect)

**Shape:** one source slot → N linked rows. A `content: {from: ...}` list field folds
the N witness rows per source slot into a list column. No collect binding.

This is the standard worked example (owners with pets):

```yaml
# owners.yaml
name: owners
format: jsonl
output_file: owners
rows: 20
links:
  - file: cats.yaml
    ref: cats
    cardinality: 2
data:
  - name: owner_id
    type: string
    generator: uuid
  - name: pets
    type: list
    content:
      from: cats
      fields:
        - name: cat_id
          refs:
            - cats.id
        - name: cat_name
          refs:
            - cats.name
```

Decomposition:

| Node | Schema | Role |
|------|--------|------|
| owners — staging | `_slot_idx`, `owner_id` | Non-linked source fields; no emit. |
| cats-in-owners — witness (atom) | `cat_id`, `cat_name`; hidden: `_staging_refs` (list) | One row per unique cat drawn; `_staging_refs` lists the drawing owner slots. |
| owners — assembly | `owner_id`, `pets: list<{cat_id, cat_name}>` | Groups witness rows by `_staging_refs` entry → list fold; emits. |

Witness row count ≤ 40 (one row per unique cat drawn from 40 draws across 20 owners).
Assembly groups by `_staging_refs` entry to produce 20 output rows, each with a `pets`
list of 2 items.

**Reduces to case 2 + list fold.** Witness generation is identical to case 2. The only
difference is the assembly node: instead of unnesting `_staging_refs` into flat rows, it
groups by `_staging_refs` entry and assembles a list column.

---

### Case 6 — List field with collect, N:M

**Shape:** one source slot → M linked rows (`cardinality: M`). N source slots exist.
A list field folds the M witness rows per source slot into a list column. A collect
binding also accumulates values from all N×M witness rows into linked-dataset fields.

```yaml
# pool.yaml (linked dataset)
name: pool
rows: 10
data:
  - name: pool_name
    type: string
    generator: word
  - name: seen_in
    type: string
    generator: constant
    value: ""
```

```yaml
# outer.yaml (source dataset)
name: outer
format: jsonl
output_file: outer
rows: 3
links:
  - file: pool.yaml
    ref: pool
    cardinality: 2
data:
  - name: outer_id
    type: string
    generator: uuid
  - name: items
    type: list
    content:
      from: pool
      fields:
        - name: pool_ref
          refs:
            - pool.pool_name
            - bind: pool.seen_in
              reducer: collect
          hidden: true
        - name: label
          type: string
          generator: word
```

Decomposition:

| Node | Schema | Role |
|------|--------|------|
| outer — staging | `_slot_idx`, `outer_id` | Non-linked source fields; no emit. |
| pool-in-outer — witness (atom) | `pool_name`, `label`; hidden: `_staging_refs` (list), collect ref | One row per unique pool row drawn; `_staging_refs` lists the drawing outer slots. |
| outer — assembly | `outer_id`, `items: list<{label}>` | Groups witness rows by `_staging_refs` entry → list fold; emits. |
| pool — emit | `pool_name`, `seen_in` | Receives collected values from witness via collect edge. |

Witness row count ≤ 6 (one row per unique pool row drawn from 6 draws; with only 6 draws
from 10 pool rows, expect ~5 unique rows drawn). Assembly groups by `_staging_refs` entry
to produce 3 output rows, each with an `items` list of 2 entries. The collect reducer
processes `_staging_refs` to populate `seen_in`.

**Reduces to case 4 + list fold.** Witness generation is identical to case 4. The only
difference is the assembly node: instead of unnesting `_staging_refs` into flat rows, it
groups by `_staging_refs` entry and assembles a list column.

---

### Summary

| Case | Cardinality | Collect | Assembly | Reduces to |
|------|-------------|---------|----------|------------|
| 1 | 1:1 | no | flat JOIN (1:1) | — |
| 2 | 1:N | no | flat expand | — |
| 3 | N:1 | yes | flat JOIN (1:1) | — |
| 4 | N:M | yes | flat expand | — |
| 5 | 1:N | no | list fold | Case 2 + fold |
| 6 | N:M | yes | list fold | Case 4 + fold |

The witness node is structurally identical in cases 2 and 5, and in cases 4 and 6.
Cases 5 and 6 always reduce to cases 1–4 with a list-fold step substituted in the
assembly node. The three-node structure (staging → witness → assembly) and the witness
node's position as an atom in the semi-lattice are invariant across all six cases.
