# JOINT-REFRAME-1: Lean into the lattice framing

**Status:** Forward-looking inspiration spec — no implementation plan yet.

---

## Motivation

The current fakeset algorithm is correct and well-staged, but its conceptual framing is fragmented across several abstractions that were introduced incrementally: includes, content includes, pool siblings, inner flats, prefills, collects. Each abstraction made local sense at the time but the vocabulary doesn't immediately reveal the unified structure underneath.

That unified structure is a **lattice of joint atoms**. Every row fakeset produces is ultimately one atom — a tuple of values drawn from nodes at every level of the lattice. The algorithm's job is to:

1. **Push down** field definitions and constraints to the most-constrained lattice nodes (leaves/atoms).
2. **Generate** atoms satisfying those constraints.
3. **Accumulate up** generated values to parent and pool nodes via the prefill lattice.

This framing is already implicit in MULT-1's code after the naming nudges (`pool_slots_path`, `n_eligible_slots`, `slot_assignments`, `_pool_idx`). The goal of JOINT-REFRAME-1 is to ask: what would the code look like if we leaned into this framing end-to-end, without being constrained by the incremental history of how we got here?

This spec is deliberately open-ended and non-prescriptive. It is a thinking tool and a long-term design direction, not a work order.

---

## What the lattice framing unifies

### Include and link are the same structural concept

Both an `include:` and a `links:` entry are *constraint-narrowing edges* in the lattice. The difference is execution role:
- An **include edge** narrows the child; the child becomes the driver. Its rows become atom rows.
- A **link edge** narrows the pool; the pool contributes pre-solved slot values that atoms copy.

But both push field definitions (type, generator, constraints) downward and both contribute to atom values that eventually accumulate upward. Unifying them behind a single abstraction (`Link` with a role: `driver | pool`) may make the execution model more legible and extendable.

### Inner flat and junction dataset are the same thing

A `GenerateInnerFlat` batch and a junction dataset batch are both atom tables. Both carry `_slot_idx` (outer-node join key), `_pool_idx` (pool-node join key), and the fields that live at the atom level. The difference today is:
- The junction dataset is an explicit YAML file.
- The inner flat is synthesised implicitly from a `content: {group: ...}` list field.

MULT-2's translation principle (nested-include as syntactic sugar → junction atom table) closes this gap conceptually. JOINT-REFRAME-1 asks: should the execution engine treat them identically, with the planner responsible for translating nested-include syntax into the junction-atom form?

### Prefill and collect are symmetric operations

`grow_parent_from_children` is downward-accumulation (child values rise to the parent level). `CollectToPool` is upward-accumulation (atom values rise to the pool level). Both are JOIN operations: one on `_row_idx`, one on `_pool_idx`. The symmetry is already present in MULT-2's design. JOINT-REFRAME-1 asks: can we unify them behind a single `AccumulateUp(source, join_key, target, aggregation)` executor step — making the lattice traversal explicit rather than encoded in two differently-named functions?

### The sibling segmentation is a lattice constraint solver

`plan_segments` computes consistent row counts across a set of sibling nodes. In the lattice framing, this is: given the marginal constraints on each lattice node, find a joint distribution over atom memberships. The Bernoulli-product / conflict-pruning / IPF pipeline is one algorithm for this. The branch-and-bound enumeration (planned separately) is a more efficient algorithm for the same problem. Framing it explicitly as "joint-distribution estimation on the lattice" may help motivate future improvements.

---

## Potential design directions

These are questions, not decisions. Each deserves its own implementation spec when the time comes.

### 1. Unified `LatticeEdge` type

Replace `Include`, `Link`, and the implicit inner-flat relationship with a single `LatticeEdge`:

```
LatticeEdge {
  from: NodeRef,          // the more-constrained node (child / pool)
  to:   NodeRef,          // the less-constrained node (parent / outer / junction)
  role: Driver | Pool,    // how the from-node contributes to atoms
  ratio: Option<f64>,
  cardinality: Option<CountSpec>,
  reinforcement: Option<f64>,
}
```

The DAG becomes a true lattice graph whose edges carry enough metadata for both segmentation and execution planning. The `build_plan` pass traverses the lattice rather than ad-hoc inspecting per-field `content.group` references.

### 2. Unified atom table representation

Represent the planner's output as a set of named atom tables:

```
AtomTable {
  key: PathBuf,           // identity (flat_key or junction dataset path)
  outer_node: NodeRef,    // node contributing _slot_idx
  pool_node:  NodeRef,    // node contributing _pool_idx
  fields: Vec<Field>,     // atom-level fields
}
```

`build_plan` emits one `GenerateAtomTable` step per atom table, regardless of whether it originated from a junction YAML or a `content.group:` list. `AssembleNestedInclude` and `grow_parent_from_children` become the same `AccumulateUp` step parameterised on join key and aggregation style.

### 3. Explicit `AccumulateUp` execution step

Replace `grow_parent_from_children` (LEFT JOIN on `_row_idx`) and `CollectToPool` (group-aggregate on `_pool_idx`) with a single:

```
AccumulateUp {
  source: PathBuf,         // atom table or child batch
  target: PathBuf,         // parent or pool node
  join_key: String,        // "_row_idx" or "_pool_idx"
  fields: Vec<AccumulationSpec>,
}

AccumulationSpec {
  source_col: String,
  target_col: String,
  reducer: Reducer,        // TakeFirst (include case), Collect (collect case), etc.
}
```

This makes the upward-accumulation phase fully explicit and uniform, and makes it easy to reason about what data flows where.

### 4. Lattice-aware expression evaluation

Currently `evaluate_expressions` operates on the assembled parent batch. In the lattice framing, expressions that reference atom-level fields could be evaluated per-atom before assembly, enabling more flexible cross-node expressions (e.g. an outer field that aggregates an atom-level expression). This is currently impossible because the expression evaluator runs after `grow_parent_from_children`. Restructuring expression evaluation as a lattice-traversal pass — with expressions evaluated at the node where their inputs are available — would unlock this.

### 5. Lattice-aware segment enumeration

The current `plan_segments` operates on a flat list of siblings. In the lattice framing, the segmentation problem is a constraint over the joint distribution on a set of lattice nodes. A branch-and-bound traversal of the lattice (planned separately as `branch-and-bound segment enumeration`) would replace the 2^N dense enumeration. Stating the segmentation problem explicitly as "find a joint distribution consistent with all marginal constraints in the subgraph" makes the reduction to a standard optimisation problem clear.

---

## What this is NOT

- Not a rewrite. The existing algorithm is correct and the incremental path (MULT-2a → MULT-2 → MULT-3) is the right way to activate features.
- Not a blocker for any planned milestone. MULT-2a, MULT-2, and MULT-3 all proceed as designed.
- Not a guarantee that any of the above directions will be adopted. Some may be premature generalisation; implementation experience will reveal which unifications are load-bearing and which are cosmetic.

The value of this spec is to make the lattice framing explicit so that future design decisions — new spec files, executor changes, plan-step additions — can be evaluated against a coherent long-term picture rather than purely against local convenience.
