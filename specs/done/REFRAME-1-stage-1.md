# REFRAME-1 Stage 1 — Documentation

No code changes. Establishes the vocabulary reference that all later stages must match.
Once merged, no old vocabulary should remain in `CLAUDE.md` or `README.md`.

---

## Stage 1A — `CLAUDE.md`

**Glossary** — replace the entire table with:

| Term | Meaning |
|------|---------|
| **concept semi-lattice** | The partial order over all datasets where `A ≤ B` means "A is a more-constrained subset of B's population". Every pair of datasets with a common ancestor has a meet (greatest lower bound). |
| **element / node** | One member of the semi-lattice. "Node" is preferred when emphasising graph structure; "element" for order-theoretic properties. |
| **⊥ (bottom)** | The empty concept — the unsatisfiable constraint set. Bernoulli segments that prune to zero rows represent ⊥ and are dropped. |
| **atom** | An element that covers ⊥ directly — the most-constrained node in a component. Atoms are generated first. |
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

**"Core architectural tenet" section** — rename to "Core architectural framing" and replace body:

> fakeset is built around a **concept semi-lattice**: a partial order where `A ≤ B` means
> "dataset A is a more-constrained subset of B's population". An `include:` stanza expresses
> constraint specialisation — not data dependency. A child is a narrower, more-constrained cut
> of its parent's population. A `links:` stanza introduces a *linked dataset* — a target from
> which list items are drawn per outer row, governed by the witness/assembly pipeline.
>
> This framing is specified in full in `REFRAME-1.md`. In brief: every dataset is a node
> in the semi-lattice; every pair of nodes with a shared ancestor has a **meet** (greatest lower
> bound); the most-constrained nodes — those covering ⊥ directly — are **atoms**, generated first.
>
> The algorithm has two symmetric phases:
>
> 1. **Push down** — field definitions, type constraints, and ref bindings propagate *down* the
>    lattice toward atoms. For linked datasets, the staging node pre-generates scalar fields
>    before the witness (atom) is generated.
>
> 2. **Accumulate up** — generated atom values propagate *up* the lattice toward parents and
>    linked nodes. For include relationships this is `grow_parent_from_children` (DataFusion
>    LEFT JOIN on `_row_idx`). For collect bindings this is `AccumulateToLinked` — the symmetric
>    operation that accumulates atom-level values back into linked-dataset fields.
>
> When a parent field matches a child's field (by ref-wiring or same name), the child's column
> is *inherited* directly; fields with no child source are generated fresh. This logic lives in
> `executor.rs::grow_parent_from_children`.
>
> *Theoretical note:* all generator invocations could conceptually happen in parallel — the
> algorithm's serialisation is purely a scheduling constraint imposed by the inherited-field
> lattice. The interesting work is resolving which pre-solved values propagate to which nodes
> and in what order.

**"Sibling segmentation" section** — rename heading to "Lower cover segmentation (Bernoulli factoring)":
- "siblings" → "lower cover members"
- "sibling group" → "lower cover group"
- "`--max-siblings`" → "`--max-lower-cover`"
- "`plan_segments` controls the explosion with three steps" — body unchanged (already accurate)

**Execution pipeline step list** — update `build_plan` bullet list:

```
`build_plan` produces a flat list of `ExecutionStep` variants:

- `GenerateDataset` — dataset with no list links; generates, evaluates, and emits in one step.
- `GenerateStagingNode` — dataset with list links; generates scalar batch only, stores in
  `computed`, no expression evaluation, no emit.
- `GenerateLowerCoverGroup` — parent + lower cover planned together via Bernoulli factoring;
  parent emits directly from this step when it has no list links.
- `GenerateStagingLowerCoverGroup` — staging counterpart of `GenerateLowerCoverGroup`; parent
  has list links, so emit is deferred to `AssembleFromWitness`.
- `GenerateWitness` — generates the witness batch (one row per source-slot × linked-row draw).
- `AssembleFromWitness` — folds witness batches into `ListArray` columns, evaluates expressions,
  emits the final output.
- `AccumulateToLinked` — accumulates atom-level values into linked-dataset fields (collect
  bindings); followed by `EmitDataset` for the updated linked dataset.
- `WriteSharedOutput` — union + shuffle all accumulated batches for a shared output file,
  write once.
```

**Module map** — update descriptions for `segment.rs`, `plan.rs`, and `executor.rs`:

| `segment.rs` | `plan_segments` — Bernoulli weights, conflict pruning, IPF, rounding. `LowerCoverMember` and `Segment` types. |
| `plan.rs` | `build_plan` — row counts, lower cover groups, inherited-field wiring, collect targets → `ExecutionPlan` / `ExecutionStep` |
| `executor.rs` | `execute` — interprets the plan; staging node generation, witness generation, assembly, `grow_parent_from_children`, `AccumulateToLinked`. All DataFusion and Arrow batch operations. |

**Key conventions** — update sentinels:
- `_slot_idx`: "a `UInt32` staging-node slot index present in all witness and child batches — which source slot each atom row belongs to. Used by `AssembleFromWitness` to fold witness rows into per-slot lists. Also used in top-level cardinality batches for which parent-row slot each child row belongs to. Retained in `computed` for grandchild access; stripped from emitted output by `filter_hidden_columns`."
- `_pool_idx` entry → `_linked_idx`: "a `UInt32` column in witness batches recording which linked-dataset row was drawn (index into the eligible linked batch). Persisted for `AccumulateToLinked` collect bindings."
- Remove the "Pool rows come first" entry; replace with: "**Linked rows preceding staging rows** — when a dataset has witness-source lower cover members, the linked-dataset rows occupy the leading positions in the combined batch so `GenerateWitness`'s `n_eligible_slots` boundary correctly identifies eligible linked-dataset slots."

**Planned next steps** — update:
- `execute_inner_flat` → `execute_witness` (already captured in the step; just remove the now-incorrect function name)

---

## Stage 1B — `README.md`

**Glossary** — add/update rows:

| Term | Definition |
|---|---|
| **parent** (parent-by-inclusion) | A dataset that is *included by* another — the less-constrained, broader population. |
| **child** (child-by-inclusion) | A dataset that *includes* another — the more-constrained, narrower population. |
| **lower cover** | The set of datasets that directly include a given parent. Formerly called "siblings". |
| **lower cover group** | A parent together with its lower cover; planned as a unit via Bernoulli factoring. |
| **linked dataset** | The target of a `links:` stanza — the dataset whose rows are drawn as list items. |
| **staging node** | Internal node holding scalar non-list fields while list items are being assembled. |
| **witness node** | Atom node carrying the linked dataset's schema; one row per unique linked-row draw. |
| **preceding** (preceding-by-execution) | Generated first. Atoms are always preceding. |
| **subsequent** (subsequent-by-execution) | Generated later. Parents and assembly nodes are always subsequent. |

Remove the old sentence "The rule is: **parents are subsequent, children are preceding.**" and replace with "The rule is: **the most-constrained nodes (atoms) are generated first; parents and assembly nodes are assembled from them.**"

**YAML schema section** — the `events` field example currently shows the pre-MULT-1
`content: {include: {file: ..., ref: ..., ratio: ..., cardinality: ...}}` syntax. Replace it
and the surrounding `content.include` sibling-segmentation YAML block with the current format:

```yaml
links:
  - file: events.yaml
    ref: event
    cardinality: {min: 0, max: 3}   # items drawn per outer row

data:
  - name: events          # list-link field — items are structs drawn from the linked dataset
    type: list
    content:
      from: event         # draw items from the "event" linked dataset
      fields:
        - name: event_id
          refs: event.id  # sourced from the linked dataset
        - name: label
          type: string    # generated fresh per witness row
```

Also update the `ratio:` example that appears under "sibling segmentation":

```yaml
include:
  file: customers.yaml
  ratio: 0.05   # marginal row-membership probability (Bernoulli)
```
This is correct for top-level includes and needs no change; remove the `content: {include: ...}` block below it that no longer reflects the model.

**"Sibling segmentation" section** — rename heading to "Lower cover segmentation (Bernoulli factoring)":
- "siblings" → "lower cover members" / "lower cover"
- "sibling segmentation" → "Bernoulli factoring"
- Remove the sentence starting "A `content.include` pool sibling places qualifying rows…" (this concept no longer maps to the current model; the witness/assembly pipeline handles it)
