# VAR-1 — Heterogeneous (multi-type) tagged unions and the case-type encoding

## Status

**Refreshed (design exploration).** This spec predates [`VAR-EXPAND`](done/VAR-EXPAND.md)
(now **complete**) and was written in the VAR-2 vocabulary; it has been brought up to
date. Two things changed the landscape:

1. **VAR-EXPAND landed lowering.** A `type: variant` field is a **tagged union**;
   `lower_member_variants` (`plan.rs`) lowers a lower-cover member's union into one
   **case-member** per case, with structural mutual exclusion in the DFS. VAR-1's old
   references to "VAR-2 Level 2 factoring" are obsolete — that path was *replaced* by
   lowering. VAR-EXPAND deliberately handles only **same-type** unions.
2. **VAR-1 is now on the critical path.** The most natural specialisation use cases
   (see [`VAR-SPECIALIZE`](VAR-SPECIALIZE.md)) are **multi-type** — e.g. a `form` field
   that is a `high_risk_form` object in one case and a `standard_form` object in
   another, with sub-populations specialising it. Those cannot be expressed until a
   union's column can hold cases of *different types*. So VAR-1 is the **substrate**
   that VAR-SPECIALIZE's multi-type half depends on — it should be designed *before*
   that half, not after.

**Two tracks, very different cost:**

- **Phase 1 (validation gate)** — cheap, independent, ready now: turn the current
  silent panic on a mixed-type union into a clean error. Worth landing immediately as
  a stopgap regardless of Phase 2.
- **Phase 2 (case-type encoding)** — the real design work. The old "encoding fork" is
  largely **resolved** by splitting it into two layers: **`DenseUnion` internally**
  (semantic match; its `type_id` *is* the discriminant, for free) and a **write-time
  output encoding** for portability (Parquet etc.), applied only at emit. What remains
  open is narrow: verify DataFusion carries union columns, and decide the portable output
  shape/flag (§Phase 2).

## Background

A `type: variant` field is a **tagged union**: per row it is exactly one of N **cases**.
Cases may be:

- **Same-type** (all string constants; or two gaussians over `number`) — VAR-EXPAND
  lowers these into case-members and generates them with no special encoding, because
  the column has one concrete Arrow type.
- **Heterogeneous (multi-type)** — a string in one case, a number in another; or, the
  important case, **a different object schema per case**. There is no single Arrow type
  for the column, and this is what VAR-1 must encode.

After `expand_field_variants`, the original `type: variant` field is replaced by a typed
stub in `data` (`stub_variant_fields`, expand_variants.rs:232; per-case type via
`infer_field_type`, :164):

- **Same-type cases** — stub gets `field_type = <shared type>`. `resolve_refs`,
  `schema_to_arrow`, and generation all see a concrete type and proceed.
- **Heterogeneous cases** — `stub_variant_fields` cannot unify the types and leaves
  `field_type = None`. `schema_to_arrow` (schema.rs:32) and `generate_column_raw`
  (generator.rs:161) then hit `.expect("field_type unresolved …")` — a **runtime
  panic**, with no user-facing error.

## Problem scope

### P1 — Heterogeneous union causes a runtime panic, not a clean error

A union whose cases span ≥2 types produces an opaque thread-panic instead of a
diagnostic. Affects both direct generation of the owning dataset and any child `ref:`
to the field (the ref inherits `field_type = None` via `resolve_refs`). Under
VAR-EXPAND, lowering a heterogeneous member union hits the same `None`-type panic during
schema construction — lowering does not rescue it. Phase 1 closes this gap.

### P2 — A case's value isn't visible to a cross-group `ref:` target

A dataset B doing `ref: A.field` where `A.field` is a tagged union sees the *stub* —
correct Arrow type (for same-type unions) but no case context — so B generates fresh
values rather than one of the declared cases.

**Reframed since VAR-EXPAND.** *Within* a lower-cover group this is a non-issue: cases
are lowered to members, so the case context is structural. The remaining gap is purely
**cross-group** refs. And the principled fix is now clear from the VAR-SPECIALIZE
discussion: giving a downstream dataset case context is a **specialisation / lattice**
concern, not a ref-propagation hack. A mixture over a population is a partition into
sub-populations (children); a downstream dataset that needs a specific case should
specialise the field (VAR-SPECIALIZE) or model the cohort as a lattice node. VAR-1 does
**not** solve P2 — it documents it and defers the semantics to VAR-SPECIALIZE.

## Division of labour (so the three specs stop overlapping)

| Spec | Owns | Status |
|------|------|--------|
| **VAR-EXPAND** | Lowering a **same-type** tagged union into case-members; structural mutual exclusion; vanilla per-member IPF; *no materialised tag* | **Done** |
| **VAR-1** (this) | **Heterogeneous** (multi-type / multi-object-schema) cases — the **column type encoding** that lets one union column hold differently-typed cases; the panic gate | Phase 1 ready; Phase 2 open |
| **VAR-SPECIALIZE** | **Child specialisation** — `value`/generator-domain merge (Half 1, independent); variant-subset restriction (Half 2, needs VAR-EXPAND for same-type, **VAR-1 for multi-type**) | Proposed |

The clean mental model: **VAR-EXPAND chooses *which* case fires; VAR-1 decides *how a
column stores* differently-typed cases; VAR-SPECIALIZE lets a child *narrow* the set of
cases.**

## Phase 1 — Validation gate (reject heterogeneous unions)

Add a check in `lib/validate.rs` after `build_dag`, before `expand_field_variants`. For
each `type: variant` field, infer each case's type (reuse `infer_field_type`) and reject
when two cases produce different non-`None` types.

```
dataset 'foo': variant field 'bar' has inconsistent case types
  — case 0: String ("hello")
  — case 1: Number (range 1..10)
  All cases of a variant field must share the same type (multi-type unions: see VAR-1 Phase 2).
```

Conservative but safe: converts a silent panic into a clear error and unblocks users.
Phase 2 *relaxes* this gate for the encodings it supports.

**Files:** `lib/validate.rs` (new check); `tests/validate_tests.rs` (error test).

## Phase 2 — The case-type encoding (the real decision)

The old draft pre-committed to a single JSON-string encoding and dismissed the
alternatives — chiefly on **Parquet portability**. That was the wrong axis to decide on.
Portability is an *output-serialisation* concern at the very end of the run; it must not
dictate the representation the planner and executor work in. Splitting the decision into
two layers dissolves most of the difficulty:

### Two layers, decided separately

1. **Internal representation** (planning + execution). What matters here is semantic fit
   and that *our* Arrow/DataFusion stack can carry it — not what downstream Parquet
   readers accept. On both counts the natural choice is **Arrow `DenseUnion`**: it is
   the native sum type, an exact match for a tagged union, and its per-slot `type_id`
   **is the discriminant, for free** — precisely what VAR-SPECIALIZE's
   restrict-to-subset-of-≥2 needs and what VAR-EXPAND chose not to materialise. We carry
   `DenseUnion` through generation, segmentation, and execution unchanged.

2. **Output encoding** (the final write step only). At emit, convert `DenseUnion` into
   whatever the target format wants — for Parquet, the widely-accepted portable shape
   (a nullable-superset struct, or a `{type_id, value…}` struct, or JSON-string).
   **This is the only place Parquet portability is a consideration**, and it is a
   per-format, post-processing transform — *not* a property baked into the internal type.
   The **intent** matches the existing **`parquet: ParquetConfig`** field (models.rs:134),
   whose own doc already gestures at *"forcing a consistent type across variant choices
   that would otherwise produce mixed types"* — but its current shape (a single
   `datatype` override) isn't quite the right flag for "post-process this union into a
   portable representation." VAR-1 adds a fit-for-purpose, write-time
   "lower-the-union-into-X" option (ParquetConfig-adjacent — new field or a widened
   `ParquetConfig`), applied **only at the very end**.

### The one genuine internal caveat

The thing to verify before committing to `DenseUnion` internally is **DataFusion/Arrow
operation support for union columns** — the ops fakeset actually leans on:
`union_and_shuffle` (sort by `random()`), `evaluate_expressions` (SQL CTE), and
`filter_hidden_columns` (select). If any of those can't carry a `DenseUnion` column
through, the fallback is to use a **nullable-superset struct** *internally* too (plain
struct, universally supported, tag readable from which sub-struct is populated) — but
that is an *internal-stack* limitation, decided on what our engine supports, **not** on
Parquet reader portability. JSON-string is demoted to one possible *output* encoding; it
is never the internal representation (it would throw away the type tag the executor needs).

### Likely shape (to confirm)

- **Internal:** one representation — `DenseUnion` — across the whole pipeline (modulo the
  DataFusion check above). This makes VAR-SPECIALIZE restrict-to-one trivial (each child
  adopts one case's concrete schema; the parent's union reconciles them) and
  restrict-to-subset expressible (the `type_id` is the tag).
- **Output:** a pluggable, write-time conversion to a portable shape per format, defaulting
  to a nullable-superset struct for Parquet, controlled via `ParquetConfig`.

Note this dissolves the old value-only `FieldType::Any`: a `DenseUnion` of concretely-typed
cases needs no opaque "any" type — each case keeps its real type and **generates through
its own generator** (the type's default, or an explicit/specialised one). JSON-string
survives only as an *output* encoding, never as the internal type.

Mechanics:
- A first-class multi-type / multi-object **union representation** in `models.rs`
  (mapping to `DenseUnion`), holding the concrete per-case field specs.
- `infer_field_type` / `stub_variant_fields`: heterogeneous combo → the union
  representation instead of `None`.
- `schema_to_arrow` / `field_to_arrow`: emit the `DenseUnion` of the case types.
- `generate_column_raw` + `constant_column`: build the union array (child arrays +
  `type_id` per row from the chosen case).
- `constraints.rs`: `Satisfiable` / `Merge` for the new representation.
- **Write step only:** `DenseUnion` → portable output shape per `ParquetConfig`.

**Files (Phase 2):**

| File | Change |
|------|--------|
| `lib/models.rs` | Multi-type **union representation** (→ `DenseUnion`) holding concrete per-case field specs; write-time output-encoding flag (ParquetConfig-adjacent) |
| `lib/expand_variants.rs` | `infer_field_type` / `stub_variant_fields`: heterogeneous → the union representation (not `None`) |
| `lib/schema.rs` | `field_to_arrow`: emit `DenseUnion` of the case types |
| `lib/generator.rs` | `generate_column_raw` + `constant_column`: build the union array — **each case generated through its own generator** (type default or specialised) + per-row `type_id` |
| `lib/constraints.rs` | `Satisfiable` / `Merge` for the new representation; per-case generator-specialisation merge (with VAR-SPECIALIZE) |
| `lib/executor.rs` | Carry `DenseUnion` through the DataFusion ops (verify support); parent-column reconciliation across child cases |
| *write/emit path* | **Only at the very end:** convert `DenseUnion` → portable output shape per format/flag |
| `src/docgen.rs` | Document the new type(s) |
| `docs/.../reference/yaml-schema.mdx` | New type entry |
| `docs/.../reference/generators.mdx` | Note: each variant case generates via its own type/generator (incl. type defaults), specialisable per case |

## Decisions needed for Phase 2

The two-layer split (DenseUnion internal + write-time output encoding) **resolves the
old #1 and #2 together**: the encoding fork stops being a fork (one internal type), and
the tag stops being a separate decision (the `type_id` *is* the tag, intrinsic to the
representation). What's left:

1. **[Largely settled] Internal representation = `DenseUnion`; tag is intrinsic.** The
   only live sub-question is the engine check: **does DataFusion carry union columns**
   through `union_and_shuffle` / `evaluate_expressions` / `filter_hidden_columns`? If
   not, fall back to an *internal* nullable-superset (an engine-support decision, never a
   portability one). *(Whether the tag is surfaced as a user-facing column is an output /
   VAR-SPECIALIZE concern, not a VAR-1 internal decision — that's why old #2 folds in.)*
2. **Output encoding(s).** The portable write-time shape per format (nullable-superset
   default for Parquet; CSV/JSON serialise naturally) and the shape of the
   ParquetConfig-adjacent flag that selects it.
3. **Explicit `type: any` vs implicit upgrade.** Auto-encode a heterogeneous
   `type: variant`, or require an explicit declaration and keep the Phase 1 gate firing
   otherwise?
4. **Generator support — not optional; a case is generative by construction.** A variant
   case is a full field spec, so *by having a `type` it already has a generator* — the
   type's default (random string, random number, …), overridable by an explicit
   `generator:`/`range:`, or by a `value:` — which is itself just the **static (const)
   generator**, the maximally specialised point on the same spectrum (see
   VAR-SPECIALIZE §Generalised merge semantics), and *terribly common* in same-type
   unions. There is no "value-only" case to validate for, and the
   old "random-any is meaningless" worry was an artefact of the JSON-blob `any` model: a
   `DenseUnion` of concretely-typed cases generates each case through *its own* generator,
   normally. So VAR-1 must support **generator-bearing cases from the start** — and,
   crucially, **generator *specialisation* of cases** (a case carrying a narrowed
   generator; a child specialising a case's generator) is in scope from day one, not a
   later add-on. This is the salary-mixture shape (two gaussians) at the heterogeneous
   level, and it ties VAR-1 directly to VAR-SPECIALIZE's generator-domain half — they
   must be designed coherently. *(This also further argues against a value-only
   `FieldType::Any` as the internal model — see below.)*
5. **`ref:` semantics — mostly answered by case-pinning.** A ref that **pins a case**
   (the restrict-to-one / specialisation path) is **type-aware by construction**: the case
   is declared in YAML, so `resolve_refs` (which runs *before* factoring) resolves the ref
   to that case's *concrete* type — it never sees a raw `DenseUnion`, and no runtime cast
   is needed. The union is a parent-level concern only. So the genuinely open part is
   narrow: what does an **unrestricted** ref to a union receive (a sibling/parent/
   downstream that refs the field without selecting a case)? That, and only that, is the
   P2 shape — hand over the whole union, require a case selection, or defer to
   VAR-SPECIALIZE.

## Dependencies

| Spec | Relationship |
|------|--------------|
| VAR-EXPAND (done) | Provides lowering; VAR-1 is its **heterogeneous-case counterpart** (lowering still applies; VAR-1 adds the column encoding lowering can't supply) |
| VAR-SPECIALIZE | **Downstream consumer.** Its multi-type half needs VAR-1's encoding; the encoding choice should be made with its tag/restriction needs in view |
| VAR-1 Phase 1 | Independent prerequisite-by-safety: stop the panic before Phase 2 lands |
