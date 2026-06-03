# VAR-1 — Implementation plan

Companion to [`VAR-1.md`](VAR-1.md). The spec covers the design, the two-layer
encoding model, and the decisions; this doc covers PR sequencing, intermediate
test green-points, and the one up-front unknown that gates everything.

## Status

**Proposed.** Not started. Per the spec, the design is largely settled (DenseUnion
internally; tag intrinsic; write-time output encoding for portability). The single
empirical unknown — whether our Arrow/DataFusion stack carries a `DenseUnion` column
end-to-end — is resolved **first** by a spike, before any production code is written.

## Scope (what this plan delivers, and what it deliberately doesn't)

**In scope.** A heterogeneous (multi-type / multi-object-schema) `type: variant`
field **on a single dataset**, generated as an Arrow `DenseUnion` and carried
end-to-end to a writable output. Each case generates through *its own* generator
(type default, explicit, or a `value:` static generator).

**Out of scope (deferred to VAR-SPECIALIZE, but not precluded).** Cross-lattice
specialisation — a parent aggregating restrict-to-one children into a union column,
and the per-case generator/subset *restriction* merge. VAR-1 lands the substrate (the
union type + generation + encoding); VAR-SPECIALIZE consumes it. The one forward
contract VAR-1 must honour: a case is a full field spec whose generator is
specialisable, so the union representation stores concrete per-case `Field`s (not a
flattened blob) — see PR 2.

**Also out of scope.** The `one_of` finite-set generator (a VAR-SPECIALIZE / standalone
generator concern, not VAR-1).

## The one unknown that gates the plan

Does DataFusion carry a `DenseUnion` column, unmodified, through the three ops the
executor relies on — `union_and_shuffle` (`ORDER BY random()`),
`evaluate_expressions` (SQL CTE), `filter_hidden_columns` (`select`) — **and** can the
output writers (parquet / json / jsonl / csv) serialise it (or tell us which need a
conversion step)?

- **Expected outcome (likely):** sort/select/CTE carry unions fine; **Parquet** needs a
  write-time conversion to a portable shape; JSON/JSONL probably fine; CSV likely needs
  conversion. → proceed with **DenseUnion internal**, PR 4 owns output conversion.
- **Fallback outcome (less likely):** a DataFusion kernel drops or corrupts the union →
  switch the *internal* representation to a **nullable-superset struct** (plain struct,
  universally supported; tag readable from which sub-struct is populated). This is an
  *engine-support* decision, never a portability one. PRs 2–6 are then revised to build
  and carry the superset struct instead; the PR *shape* is unchanged.

This is why the spike is PR 1: the subsequent PRs assume the expected outcome, and we
review/adjust them only if the spike says otherwise (we likely will anyway).

## PR sequencing

Mirrors the VAR-EXPAND convention: land the type/schema/generation/constraints
machinery **dormant** (no caller emits a union, so behaviour is byte-identical), then
one **atomic switchover** flips `expand_variants` to emit the union representation. That
keeps exactly one revertable behaviour commit.

| PR | Subject | Scope |
|----|---------|-------|
| 0 | Phase 1 validation gate (independent) | Reject heterogeneous unions cleanly (panic→error). Lands anytime, in parallel; relaxed by PR 3 |
| 1 | **DataFusion + writer spike (decision gate)** | Prove (or disprove) DenseUnion carry-through; decide internal rep + which writers need conversion. Leaves a kept regression test |
| 2 | Dormant machinery | `models.rs` union representation; `schema.rs` `field_to_arrow` arm; `generator.rs` union-build arm; `constraints.rs` arms — all dead (expand still emits `None`) |
| 3 | **The switchover** (only behaviour commit) | `expand_variants` emits the union rep for heterogeneous cases instead of `None`; relax PR 0 gate; end-to-end fixture (JSON/JSONL) goes green |
| 4 | Output encoding | Write-time `DenseUnion` → portable shape per format (Parquet default = nullable-superset); ParquetConfig-adjacent flag; Parquet fixture green |
| 5 | Docs + docgen + close-out | yaml-schema / generators / concepts; `docgen`; feature-table flip; VAR-SPECIALIZE re-pointed (substrate exists) |

**Revert safety.** PR 0 rejects an already-broken config; PRs 1–2 are a throwaway spike
+ dead machinery; PR 3 is the single behaviour flip; PRs 4–5 are output/docs. So
`git revert <PR3>` cleanly restores "heterogeneous variants are gated" without unwinding
the types. Keep PR 3 a single squash-merge.

---

### PR 0 — Phase 1 validation gate (independent quick win)

Convert the silent panic (schema.rs:32 / generator.rs:161 on `field_type = None`) into a
diagnostic. New check in `lib/validate.rs`, after `build_dag`, before
`expand_field_variants`: for each `type: variant` field, infer each case's type
(`infer_field_type`, expand_variants.rs:164) and reject when two cases yield different
non-`None` types.

```
dataset 'foo': variant field 'bar' has inconsistent case types
  — case 0: String ("hello")
  — case 1: Number (range 1..10)
  All cases of a variant field must share the same type (multi-type unions: see VAR-1 Phase 2).
```

- `tests/validate_tests.rs` — fixture/builder with a mixed-type variant; assert the error.
- Lands green immediately; **no dependency on the spike.** PR 3 relaxes it.

### PR 1 — DataFusion + writer spike (decision gate)

A focused, ~throwaway probe (kept as one regression test). Build a small `RecordBatch`
with a `DenseUnion` column (e.g. cases `Utf8` + `Float64`, and one struct-case for the
object shape) and push it through, asserting the union survives:

1. `union_and_shuffle` (sort by `random()`) — row count + per-row case identity preserved.
2. `evaluate_expressions` — a trivial CTE/`select *` round-trips the union column.
3. `filter_hidden_columns` (`select` of visible cols) — union column projects.
4. Each writer: `parquet`, `json`, `jsonl`, `csv` — record which accept the union and
   which error (→ require conversion in PR 4).

**Deliverable:** a short findings note appended here + a kept test
(`tests/` or `lib/` `#[cfg(test)]`) named e.g. `denseunion_survives_datafusion_ops`.
**Decision recorded:** internal rep = DenseUnion (expected) or nullable-superset
(fallback); per-writer conversion matrix for PR 4.

**Checkpoint:** if fallback, pause and revise PRs 2–6 to the superset-struct rep before
continuing. Otherwise proceed.

### PR 2 — Dormant machinery

All additions are unreachable until PR 3 (expand still emits `None`), so behaviour is
byte-identical and the suite stays green.

- **`lib/models.rs`** — union representation holding **concrete per-case `Field`s**, e.g.
  `FieldType::Union(Vec<Field>)` (each case carries its own type/generator/value/range,
  preserving the per-case-generator forward contract). Helper to read the case list.
- **`lib/schema.rs`** — `field_to_arrow`: `Union` → `DataType::Union(UnionFields, Dense)`
  built from the case `Field`s' Arrow types.
- **`lib/generator.rs`** — `generate_column_raw` + `constant_column`: a `Union` arm that
  builds the `DenseUnion`: per-case row counts via `resolve_distributions(ratios)`; each
  case's child array via `generate_column` with that case's `Field` (so it uses *its own*
  generator, incl. a `value:` static generator); assemble `type_ids` + dense `offsets`.
- **`lib/constraints.rs`** — `Satisfiable` / `Merge` arms for the union representation
  (minimal: a union merges with an unconstrained/empty; deeper per-case
  generator-specialisation merge is VAR-SPECIALIZE's, designed against the *same* spectrum
  — left as a noted seam, not built here).

Unit tests exercise each arm in isolation (build a `Union` `Field` directly, assert Arrow
type and a generated `UnionArray`'s shape/case distribution) without going through
`expand_variants`.

### PR 3 — The switchover (the only behaviour commit)

- **`lib/expand_variants.rs`** — `infer_field_type` / `stub_variant_fields`: a
  heterogeneous case set resolves to the `Union` representation instead of `None`.
- **`lib/validate.rs`** — relax PR 0's gate to reject only *still-unsupported*
  heterogeneity (if any remains after Phase 2; ideally none — the gate becomes a
  safety net).
- **End-to-end fixture** (`tests/fixtures/…/variant_multitype/`) — a dataset with a
  heterogeneous variant (the supplier `form`: object-A vs object-B, and a scalar
  string-or-number case), output as **JSON/JSONL** (formats the spike showed serialise a
  union natively — Parquet waits for PR 4). Integration test asserts: it runs without
  panic, row count correct, each row carries a valid case, case distribution ≈ ratios.

This is the revertable commit.

### PR 4 — Output encoding (portability, write-time only)

- **Write/emit path** — convert `DenseUnion` → a portable shape **only at the very end**,
  per output format and per the spike's writer matrix. Parquet default: **nullable-superset
  struct** (∪ of case fields; each row fills its case's fields, rest null; the populated
  sub-struct *is* the readable tag). CSV: a sensible scalarisation (JSON-string cell or
  per-case columns). JSON/JSONL: native if the spike allowed it.
- **Flag** — a fit-for-purpose, ParquetConfig-adjacent write-time option selecting the
  portable shape (new field or widened `ParquetConfig`; the current `datatype`-only shape
  isn't quite it). Default chosen so no flag is needed for the common case.
- **Parquet fixture** — extend PR 3's fixture with a Parquet output; statistical/
  integration test reads it back and asserts case recoverability + distribution.

### PR 5 — Docs + docgen + close-out

- `docgen` (`src/docgen.rs`) + `docs/.../reference/yaml-schema.mdx` — the union type and
  the output-encoding flag.
- `docs/.../reference/generators.mdx` — each case generates via its own type/generator.
- `docs/.../concepts/` — extend the variant page (or a short companion) with the
  heterogeneous-case encoding + the two-layer (internal DenseUnion / output portable)
  model and the discriminant-from-encoding point.
- CLAUDE.md feature-table row → status updated; mark VAR-1 spec/impl complete and move to
  `specs/done/`; re-point VAR-SPECIALIZE (multi-type substrate now exists); known-limitation
  note for mixed-type variants removed/again-narrowed.

---

## Risks

- **DataFusion union support (the spike's job).** Mitigated by making it PR 1 with an
  explicit fallback (internal nullable-superset). Likely fine; cheap to confirm.
- **Per-row case assignment vs downstream shuffle.** Block-generating cases then relying
  on `union_and_shuffle` is fine for output, but verify any *pre-shuffle* consumer (e.g.
  expression eval referencing the union) sees correct per-row cases. Covered by PR 1 op 2.
- **Object-case field-name collisions** in the nullable-superset output (PR 4) — two
  cases with same-named sub-fields of different types. Namespace by case in the superset,
  or require disjoint names; decide in PR 4.
- **VAR-SPECIALIZE coupling.** The per-case generator-specialisation merge is left a
  noted seam (PR 2). Keep the union representation storing concrete per-case `Field`s so
  that seam stays cheap to fill.

## Forward contract for VAR-SPECIALIZE

- Union representation stores concrete per-case `Field`s (generators intact) → per-case
  generator specialisation is "merge into a case's `Field`," same `FieldConstraints`
  spectrum.
- A case-pinned ref resolves to the case's concrete type at `resolve_refs` (before
  factoring) — never a raw `DenseUnion`. Only *unrestricted* refs to a union are an open
  VAR-SPECIALIZE/P2 question.
- The `type_id` is the intrinsic discriminant — available for restrict-to-subset-of-≥2
  without materialising a separate sentinel.
