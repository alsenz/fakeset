# EXPR-RELOCATE — evaluate expressions during materialisation, not at emit

**Status:** **COMPLETE** — implemented across PR1–PR4 (placement scheduler; computed atom
column-source + static type inference; bound-merge algebra; content-expressions + edge-granular
collect). A follow-on tidy-up pass aligned the code to theory (single per-edge collect path through
the **junction**; `expression` as the tightest point on the value-source spectrum with a rigid
support; `reconcile` the single constraint combinator). See `specs/EXPR-RELOCATE-impl.md` for the
as-built PR notes.

**Sequencing:** Third (last) of the list/expression specs — **LIST-NORM → PROJECT-FIELD →
EXPR-RELOCATE** (all three now done). It lifted the content-expression gate that LIST-NORM rollups
and **PROJECT-FIELD's derivation half** waited on — both now work.

## Goal

Move expression evaluation out of the terminal per-dataset emit pass and into the
materialisation pipeline, **placed by dependency**, *so that* an expression can **author a
ref'd / shared column** (projected up the lattice) — *as long as* the constraint merge
algebra can solve it. The headline new capability is **pinning a ref'd field with an
expression**.

Two conceptual capabilities (the strangler PR breakdown is in `specs/EXPR-RELOCATE-impl.md`):

1. **Expressions as a computed atom column-source, type-inference only** — unblocks
   "expression authors at the atom, ref propagates up" with essentially zero new algebra
   (type is free). Covers the UBO / rollup-shaped cases.
2. **The bound-merge algebra for constraint-bearing computed columns** — use the DataFusion
   parser/analyser for type inference and, where possible, range-bound analysis; otherwise
   reject.

## Background / framing

- **Today expressions are terminal.** `GenerateStagingNode` evaluates *no* expressions
  (scalar batch only); `evaluate_expressions` runs at emit points (`executor.rs:411`,
  `:557`, `:594`, `:1233`) over the fully-materialised batch; `generate_segment_atom_batch`
  authors shared columns from `import → precomputed → fresh`, then projects up
  (`project_parent_columns_from_atom`). So an expression result exists only *after*
  everything downstream of it is frozen → it can **never** be a ref source / projected up.

- **Parents don't author.** A shared field's value is authored once at the **atom** (the
  meet of a segment's member-combination), then pushed up; the parent is a passive
  recipient. An expression that authors a shared column must therefore evaluate at
  **shared-atom time, before projection** — never "at the parent".

- **Placement, not scope.** Relocation does **not** change ref scoping rules. The rule is:

  > Evaluate each expression at the **earliest materialisation point** where all the fields
  > it references already exist.

  An expression references the same fields it always could; it just computes as soon as they
  are ready. Relocation is **value-preserving** — a scalar-only expression computes the
  identical result at staging or at assembly (lists don't affect scalar columns).

- **`ref` + `expression` on one field becomes *allowed* — the existing ban is a
  current-pipeline artifact, not a semantic one.** The two keys play **orthogonal** roles:
  `ref` is **wiring** ("this is the shared column; tie it to the cover/parent column; the
  parent pulls it up") and `expression` is **authoring** ("compute the value at the atom").
  They compose because there is a *single* authoring point — the atom — fanned to the parent
  and every member: identity is preserved structurally (one column) and the value is the
  expression, so there is no *second* authority. The current ban (`validate.rs:887`) exists
  only because *today* expressions run at terminal emit, **after** the shared column has been
  projected, which would create a genuine second value; relocating evaluation to atom time
  removes that reason. This is the **natural** way to "pin a ref'd field with an expr":

  ```yaml
  # weighted_scores includes scores (parent)
  - name: score
    ref: scores.score           # wiring: this IS the shared score column
    expression: "base * weight" # authoring: computed at this member's atom
  ```

  Putting the `expression` on the **member** (where authoring happens) is *better* aligned
  with "parents don't author" than declaring it on the parent. The real invariant is **not**
  "ref xor expression" but **exactly one value-source per shared column**: `ref` is *not* a
  value-source (it is wiring); a share-set may carry at most one of
  `generator` / `value` / `expression`. The combo is gated by the **merge algebra** (type
  always; bounds-or-reject for constraint-bearing columns) and the existing import-taint rules
  — not a blanket ban.

## Design

### Placement scheduler

Materialisation points, in pipeline order:

```
staging-scalar  →  shared-atom  →  member-non-ref  →  assembly (list-fold)
```

Each expression is placed at the earliest point where all referenced fields are materialised:

| Expression depends only on… | Placed at | Payoff |
|---|---|---|
| scalar / staging fields | **staging** | result usable as an **outer-scoped ref** in list content + witness (`executor.rs:1134`) |
| shared-atom (ref'd) columns | **shared-atom** | can be **projected up** — the *pin-a-ref'd-field* case |
| a member's own non-ref field | **member emit** | = today |
| assembled list columns | **assembly** | = today; covers `array_cat`/`normalize` rollups |

Implementation: split the single `evaluate_expressions` call into per-point invocations,
each evaluating the expressions placed there.

### Computed column source

`generate_segment_atom_batch` gains a **fourth** column source:

```
import → precomputed → fresh → computed
```

A shared column declared with `expression:` is evaluated here, with inputs limited to
shared columns already materialised at this point, then projected up unchanged.

### Constraint merge algebra (the gate)

A computed shared column may be **constraint-bearing** — a member restricts it, or it
participates in segment conflict pruning. The algebra needs its **support**:

- **Type axis — always.** DataFusion infers the output `DataType` statically from the input
  column types (the logical-plan output schema; `ExprSchemable::get_type` underneath). Total
  — works for any expression, no execution. Type-mismatch merge = conflict = **prune**.
  Safe.
- **Bound axis — when derivable, else reject.** DataFusion's interval engine
  (`physical_expr::analyze` / `cp_solver` / `PhysicalExpr::evaluate_bounds`) propagates
  intervals through arithmetic and comparisons; returns **unbounded** for ops it can't reason
  about. Rule: a range/value merge is allowed **iff** bounds are derivable; otherwise emit a
  **validation error** ("expression output bounds not statically determinable; cannot merge
  with range constraint"). Never silently prune (drops valid rows) and never silently allow
  (corrupts a referential cell).
- **Const-folding.** Literal-only expressions fold to a scalar at plan time
  (`ExprSimplifier`/`ConstEvaluator`) → type *and* value known statically.
- **Strings are the easy case.** A string-typed output needs no interval; its merge is
  type-only (domain = strings; equality/membership at most).

**Discipline:** lean entirely on DataFusion's existing analysis. The moment we'd need to
write our own bound analysis is exactly the moment we emit the validation error — we do not
write half a compiler.

**Spike (DataFusion 53.1), confirmed — static inference without execution:**

```
qty * 2  -> Int64     qty * 1.5 -> Float64    qty + price -> Float64
concat(name,'-x') -> Utf8     qty > 5 -> Boolean     cast(qty AS VARCHAR) -> Utf8View
2 * 3 + 1 -> Int64   (optimised plan folded it to Int64(7))
```

### Linked content lists & `link` data — same theory, more points

A dataset with `links:` is **not a special case** — the placement rule is identical. Links
simply expand the set of materialisation points, because a link dataset is generated as
`GenerateStagingNode → GenerateWitness → AssembleFromWitness` (or the lower-cover-group
variants). Following the theory through, the points are:

```
staging-scalar  →  [staging shared-atom]  →  witness  →  assembly (list-fold) → [collect / AccumulateToLinked]
```

Mapping each step to its order-theoretic role (and to "parents don't author"):

| Step | Role | Expressions placed here can reference… | Payoff |
|---|---|---|---|
| **staging-scalar** (`GenerateStagingNode`) | push-down of the outer row's scalar fields | other staging-scalar fields | result usable as an **outer-scoped ref** in `content` *and* by witness generation (`executor.rs:1134`) |
| **staging shared-atom** (`GenerateStagingLowerCoverGroup`) | the meet, when the staging node is itself in a lower-cover group | shared/ref'd staging columns | **project-up** of an expression-authored shared column — *identical* to the include case; the segment-atom machinery is reused unchanged |
| **witness** (`GenerateWitness`) | the **atom** carrying the linked dataset's schema; authored once per unique linked-row draw | content fields + linked-scoped refs (per-unique-linked-row, matching plain content generation) | per-item derived content values; can feed assembly folds and, where present, `AccumulateToLinked` |
| **assembly** (`AssembleFromWitness`) | the **subsequent** node — folds witness rows into lists; *does not author*, exactly as a parent doesn't | assembled `ListArray` columns + everything above | rollups / `array_cat` / `normalize` (= today's emit-time behaviour, just relocated into the step it already lives in) |
| **collect** (`AccumulateToLinked`) | the symmetric **accumulate-up** for links | assembly-level / witness values bound by `collect` | expression-authored values accumulated back into a linked dataset's fields |

The same placement rule — *earliest point where all referenced fields exist* — selects the
step. Outer-scoped refs are the one cross-step subtlety, and it already resolves cleanly:
`AssembleFromWitness` pulls outer-scoped content refs from the **staging** batch
(`executor.rs:1134`), so an expression placed at **staging** is automatically in scope for a
`content` field that refs it — no new wiring.

**Honest gate to flag:** per-item *content* expressions (an `expression:` among a list-link's
`content.fields`) are currently rejected at validation (`validate.rs:1148`), as are content
variants (`VAR-LINKED-CONTENT`). The placement model **accommodates them by theory** — they
are simply expressions placed at the **witness** step — but lifting that validation gate is a
**scoped follow-on**, not part of this spec. EXPR-RELOCATE establishes the machinery; it does
not silently open the content-expression surface. (When it is opened, it also unblocks
**PROJECT-FIELD's derivation half** — the projection machinery already ships, so a witness-computed
`holding.weight * 2` only needs the content-expression to author it, then bare `project`
collapses it to a scalar list with no further change.)

**Edge-granular `collect` of a content-expression (the linked-side dual).** A per-item
content expression that mixes a *linked-scoped* term with an *outer-scoped* term — e.g.
`score = linked.raw_weight * outer.multiplier` — is a genuinely **per-edge** value, and you
may want it `collect`ed into a `[]float` on the linked dataset. This is **sound and needs no
new accumulation engine**, because the per-edge machinery already exists:

- `AccumulateToLinked` already operates at **edge granularity** — it expands the witness via
  `_staging_refs` to one row per (slot, linked-row) draw (content columns *replicated per
  draw*), groups by `_linked_idx`, and `array_agg`s for `collect` (`executor.rs:1271`+).
- Outer-scoped refs are already **`take`-replicated per-edge** into the junction at assembly
  (from the staging batch, `executor.rs:~1150`). So "copying the outer field down per edge" —
  the implementer's instinct — is *already done* for refs; it is the existing outer-scoped-ref
  mechanism, not new work.

So the per-edge expression is just **evaluated on the per-edge junction** (where `raw_weight`
is replicated-per-draw and `multiplier` is replicated-per-edge), and `collect`'s existing
`group_by(_linked_idx)` + `array_agg` gathers it into the `[]float`. The `[]float` *list*
absorbs the many-edges-per-linked-row multiplicity — which is exactly why this works where a
**scalar `ref`-pin does not** (per-edge value, per-row target, wrong direction). The only
genuinely new work is: (a) the content-expression gate (above), and (b) making the `collect`
path consume the **assembled junction that has the expression evaluated** (the assembly path
already builds such a junction with outer-scoped refs) — wiring/ordering, not a new engine.
This is the linked-side **dual** of a list rollup: the same per-edge value surfaced toward the
linked endpoint (`collect` → list on linked) instead of the outer endpoint (list on outer).

Staying with the theory: nothing here is new architecture. The witness is an atom, so it
authors; assembly is subsequent, so it folds, never authors; the staging lower-cover group is
just the include-case meet wearing a different hat. If a question arises about a link-shaped
case not covered above, the answer is the order-theoretic one — *author at the atom (staging
shared / witness), accumulate up at the subsequent node (assembly / collect)*.

## Phasing

See **`specs/EXPR-RELOCATE-impl.md`** — the strangler PR breakdown (PR1 placement scheduler /
PR2 computed atom column-source + type-inference + ban-lift / PR3 bound-merge algebra / PR4
content-expressions + edge-granular collect), with files, tests, spikes, and the go/no-go
gates.

## Validation

- **Placement feasibility:** every expression must have a valid placement (all deps
  materialised by some point) — already approximated by `validate_expression_order`
  (expressions reference fields defined above them).
- **Before bound-merge lands:** a computed *shared* column needing a value/range merge →
  error (deferred to the bound-merge phase).
- **Once bound-merge lands:** bounds not derivable for a needed merge → error.
- **`ref` + `expression` on one field** is **allowed** (the computed-source phase lifts
  `validate.rs:887`); the guard becomes **one value-source per shared column** — a share-set
  may carry at most one of `generator` / `value` / `expression`. A ref'd+expression field
  needing a *value/range* merge is rejected until bound-merge lands.

## Out of scope

- **`range`-on-column** (per-row parameterised generator bounds) — a separate
  generator-engine track; do not couple to it.
- **The big DataFusion rewrite** (all generators as UDFs, whole pipeline as DataFusion
  projections) — held in the **back of mind** as the likely eventual north star, *not* this
  spec. EXPR-RELOCATE is a deliberate stepping stone: it introduces DataFusion *analysis*
  (type + bounds) for the merge algebra **without** moving generation into DataFusion. If the
  analysis adoption proves clean here, it de-risks that larger direction later.
- **Multi-level project-up** beyond the direct parent — follow-on if needed.

## Test plan

- **Relocation value-preservation:** a scalar expression yields identical output whether
  placed at staging or assembly.
- **Staged expr as outer-scoped ref:** an expression computed at staging is usable as
  `ref: <expr_field>` inside list content; every item gets it.
- **Project-up:** a computed shared column is seen *identically* by the parent and every
  member that refs it.
- **Type-merge prune:** a computed column whose inferred type is incompatible with a member
  restriction → that segment is pruned.
- **Bound-merge:** derivable bounds intersect correctly; un-derivable bounds against a
  range → validation error.
- **Const-fold:** a literal-only expression yields a constant column.
- **Link — staging placement:** an expression over staging-scalar fields computes at staging
  and is consumed as an outer-scoped `ref` by a `content` field (witness/assembly trace).
- **Link — staging shared-atom project-up:** a staged expression-authored shared column in a
  `GenerateStagingLowerCoverGroup` is seen identically by parent + members (same assertion as
  the include case, on the staging side).
- **Link — collect:** an expression-authored value accumulates correctly via
  `AccumulateToLinked`.
- **Statistical suite:** a UBO / rollup example exercising the full
  staging→atom→witness→assembly path end-to-end.
