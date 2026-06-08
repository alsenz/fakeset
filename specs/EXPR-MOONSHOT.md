# EXPR-MOONSHOT — all column generation as DataFusion projections (blue-sky stub)

**Status:** Future — **blue-sky stub.** North star, not scheduled. The eventual destination
that `specs/EXPR-RELOCATE.md` is the safe stepping stone toward. Deliberately minimal: this
records the *shape* of the idea and the inside/outside boundary, not an implementation plan.

## The idea, in one paragraph

Per-row column **value** production becomes a sequence of **DataFusion projections**. The
combinatorial / numeric **planning** stays *outside* DataFusion and produces, **per atom**, a
small **parameter set** (row count, range bounds, ratios, segment masks, …). DataFusion then
**expands** each atom to its rows via a projection that reads those parameters and uses
**volatile UDFs** for the actual random draws. Two layers, one clean seam: *parameters
outside, rows inside*.

## Two non-negotiable shape constraints

1. **Not one projection.** Atom materialisation can never be a single pass over all defined
   columns — there is always `staging → linked-data → assembly`, potentially **iterated**
   (multiple linked content lists), **scoped by YAML presentation order**. The moonshot is a
   *chain* of projections (one per scope) separated by witness/sampling **islands** — the
   exact materialisation-point theory of EXPR-RELOCATE, with each scope now a DataFusion
   projection.

2. **Not a full parameter frame.** We do **not** materialise an `N`-row parameter frame.
   Parameters are computed **per atom — which may be joint** (a segment is a member-
   *combination*, so one atom's parameters cover the overlapping members jointly). Those
   small parameter sets are injected as **scalar context** into the projection, and the many
   rows are produced *inside* DataFusion by `volatile UDF + expr`. This keeps a crisp
   inside/outside boundary **and** opens an incremental path to *liberate generator-argument
   parameterisation* later (a range bound that is a per-atom scalar today can become a
   per-row expression tomorrow, without re-architecting).

## The split — atom materialisation mechanisms

`Outside` = the parameter/planning layer (combinatorial, numeric, IO — not relational).
`Inside` = a DataFusion projection over a row-driver, reading per-atom parameters.
`Island` = a non-projection step *between* projections.

| Mechanism | Today (module) | Layer | Note |
|---|---|---|---|
| `rows:` / sampled / segment row counts | `plan.rs`, `segment.rs` | **Outside** | a per-atom parameter |
| Bernoulli prior · conflict pruning · IPF · largest-remainder | `segment.rs` | **Outside** | pure combinatorics; produces segment sizes + masks |
| Variant lowering · exclusion groups · categorical DFS | `plan.rs`, `segment.rs` | **Outside** | produces per-case absolute ratios + structure |
| Ring assignment / import row filter | `plan.rs`, `import.rs` | **Outside** | pre-filter / parameter |
| Distribution resolution | `models.rs` (`resolve_distributions`) | **Outside** | parameter |
| Locale stamping | `rewrite.rs` (`apply_global_locales`) | **Outside** | becomes a UDF arg |
| Import file load + taint | `import.rs`, `executor.rs` | **Outside → Inside** | load is IO; once loaded it is a table the projection reads |
| Plain generator column (name/bic/email/…) | `generator.rs` (`generate_column`) | **Inside** | **volatile** UDF, parameterised by `args` |
| Numeric range draw (`range`, `precision`) | `generator.rs` | **Inside** | volatile UDF; bounds = injected parameter |
| Date/datetime bounds (`after`/`before`) | `generator.rs` (`fake_date`) | **Inside** | volatile UDF; bounds = parameter |
| `one_of` / `value` / categorical draw | `generator.rs` (`draw_categorical`) | **Inside** | volatile UDF or `CASE`; weights = parameter |
| Same-type variant per-row draw | `generator.rs` (`build_same_type_variant_column`) | **Inside** | volatile categorical UDF; ratios = parameter |
| Heterogeneous union (`DenseUnion`) | `generator.rs` (`build_union_column`) | **Inside** | projection builds union/struct; `type_id` draw = volatile UDF |
| Object / struct field (recursive) | `generator.rs` | **Inside** | nested projection / `named_struct` |
| Ref / inherited (shared) column | `executor.rs` (`project_*_from_atom`, SEG-ATOM-1) | **Inside** | **ref = common-subexpression**; CSE dedups "author once, fan out" for free |
| Expression fields | `executor.rs` (`evaluate_expressions`) | **Inside** | already DataFusion; subsumes EXPR-RELOCATE |
| Witness draw (sample-with-replacement + dedup) | `executor.rs` (`execute_witness`) | **Island** | not a native projection; index-join + volatile sampling UDF, or stays bespoke |
| Assembly / list fold | `executor.rs` (`AssembleFromWitness`) | **Inside** | natural `group_by` + `array_agg` |
| Accumulate-to-linked (collect) | `executor.rs` (`AccumulateToLinked`) | **Inside** | aggregate/join back to the linked table |
| Shuffle / shared-output union | `executor.rs` (`union_and_shuffle`) | **Inside** | already DataFusion |
| Output encoding (flatten / unionize / hidden-filter) | `output.rs` | **Outside** | write-time deserialisation concern; stays out of the generation projection |

## Why the parameter/projection split (and not "UDF-everything")

- **Combinatorics stay where they belong.** Bernoulli/IPF/pruning/lowering are not relational
  — forcing them into DataFusion would be a category error. They produce parameters.
- **Volatile-UDF hazards are contained.** CSE-collapse and seeding live *only* in the row-
  expansion layer, not smeared across the whole system.
- **Incremental liberation.** Generator-argument parameterisation can be freed gradually:
  scalar-per-atom → per-row expression, one argument at a time, without a big-bang rewrite.

## Open questions / spikes (inherited from EXPR-RELOCATE's gates)

- Volatile UDF that is **not** CSE'd/constant-folded (two independent draws stay independent)
  **while** a genuine shared ref Expr **is** CSE'd — both behaviours at once.
- Seeding / reproducibility under partitioned, parallel execution.
- The `N`-row driver, and injecting per-atom parameters as **scalar context** into it.
- Witness **island** shape: index-join + sampling UDF vs. staying bespoke Arrow.
- Error attribution: DataFusion planning errors → "field X, line Y" diagnostics.
- Coupling to DataFusion-internal analysis APIs (`cp_solver`/`evaluate_bounds`) across upgrades.

## Migration — small, independently-testable steps (the whole point)

This must be a **strangler, never a big bang.** Each step swaps **one** mechanism (one table
row) from `Outside`-bespoke to `Inside`-DataFusion behind a flag, proves equivalence, then
flips the default and deletes the old path. The discipline at every step:

> Keep the old path. Add the new path behind a per-field/per-mechanism flag. Assert the new
> path matches the old — **hard invariants identical** (ranges, membership, referential
> integrity, cardinality), **soft invariants within α** (the statistical suite). Flip default.
> Delete old path. Only then take the next step.

The **statistical suite is the tripwire**: per CLAUDE.md invariant #7, the integration bugs
that matter are invisible to unit tests and surface only end-to-end on the examples. So every
step is gated on `cargo test` **and** `pytest` staying green.

A plausible low-risk ordering (each line is one independently-shippable, independently-testable
step):

1. **EXPR-RELOCATE** ships first (its own spec) — relocation + analysis adoption, *no* UDF
   rewrite. Establishes the seam and the DataFusion-analysis dependency.
2. **One generator → one volatile UDF**, behind a per-field flag, diff-tested for
   distributional equivalence against `generate_column` (start with a boring one, e.g. a plain
   numeric range). Proves the volatile-UDF/CSE/seeding spikes on real ground.
3. **Range / date / categorical draws** → UDF + per-atom **scalar parameter** injection.
4. **Refs → shared Expr (CSE)** within a single scope — verify "author once, fan out" matches
   the projection output byte-for-byte on hard invariants.
5. **Assembly fold → `group_by`/`array_agg`** — swap one list-fold, compare ListArrays.
6. **Witness island** considered last (or left bespoke indefinitely) — highest risk, least
   relational.

If any step's equivalence check fails or its spike (volatile-UDF independence, seeding,
parameter injection) doesn't come back clean, **stop there** — the parameter/projection seam
means the half-migrated system is still a coherent, shippable product, not a stuck rewrite.

## Relationship to other specs

`EXPR-RELOCATE` is the **stepping stone** (relocate expressions + adopt DataFusion *analysis*
for the merge algebra, *without* moving generation into DataFusion). `EXPR-MOONSHOT` is the
**unification** that adoption de-risks. The load-bearing insight is **ref = CSE**. As always,
when a case is unclear, the answer is the order-theoretic one: *author at the atom, accumulate
up at the subsequent node* — the moonshot only changes the *engine*, never the theory.
