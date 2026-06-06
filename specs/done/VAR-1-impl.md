# VAR-1 — Implementation plan

Companion to [`VAR-1.md`](VAR-1.md). The spec covers the design, the two-layer
encoding model, and the decisions; this doc covers PR sequencing, intermediate
test green-points, and the one up-front unknown that gates everything.

## Status

**Complete — PR 0–5 done (green, ~263 tests; docs build 14 pages).** Heterogeneous
(multi-type / multi-object-schema) variants work end-to-end. Remaining optional follow-up:
`VAR-1-OUTPUT-FLAG` (configurable output encoding + CSV unblock).

<details><summary>Progress history</summary>

**In progress — PR 0, PR 1, PR 2 done (green).** PR 0 landed the Phase 1 validation gate
(mixed-type variant panic → clean error). PR 1 (the spike) **resolved the one empirical
unknown**: `DenseUnion` carries cleanly through all three DataFusion ops, so the internal
representation is confirmed — *no* fallback needed. It also found that **no output writer
serialises a union** (Parquet panics, ARROW-8817; JSON/JSONL/CSV fail), which makes PR 4's
write-time conversion mandatory for every format and means PR 3 proves generation
in-memory. PR 2 landed the dormant machinery (marker `FieldType::Union` +
`Field::union_cases`; schema + generator arms), behaviour-neutral. PR 3 (+ 3.5) landed the
`lower_heterogeneous_unions` pass — **scalar and object-schema** cases → `DenseUnion`
(`FieldVariant.fields` added; gate unified on `is_heterogeneous`), proven in-memory,
behaviour-neutral. **PR 4** landed the user-facing enablement: `unionize_for_output`
converts unions → nullable-superset structs at write (parquet/json/jsonl; CSV gated at
validation), blanket gate removed, end-to-end to disk. Het variants now work. Next: PR 5
(docs + docgen + close-out); the configurable output-shape flag is deferred. Full suite
green (263 tests).

</details>

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

**RESOLVED by PR 1 (spike complete, green).** Findings:

- ✅ **DataFusion carries `DenseUnion` cleanly** through all three ops — `union_and_shuffle`
  (sort by `random()`), `evaluate_expressions` (CTE `SELECT *`), and `filter_hidden_columns`
  (`project`). Row count and per-case (`type_id`) histogram are preserved across the
  shuffle. → **Internal representation = `DenseUnion` is confirmed; the nullable-superset
  fallback is NOT needed.**
- ⚠️ **No output writer emits a `DenseUnion` natively** (arrow/parquet 58.3): Parquet
  *panics* (ARROW-8817 — parquet has no Arrow-union mapping); JSON, JSONL, and CSV also
  fail. → **PR 4's write-time conversion is required for *every* output format, not just
  Parquet.** This is a stronger result than the original "Parquet only" guess and it
  **reorders the plan** (see PR 3 / PR 4 below): a union dataset cannot complete a
  full `execute`-to-disk run until PR 4 lands, so PR 3 proves generation **in-memory**.

Guard kept: `lib/executor.rs::mod denseunion_spike` (4 tests) — the DataFusion-carry
assertions and the writer matrix (which flags us if an arrow upgrade changes writer
support).

The fallback path (had a DataFusion kernel dropped the union): switch the *internal*
representation to a **nullable-superset struct**. Not triggered — recorded for posterity.

## PR sequencing

Mirrors the VAR-EXPAND convention: land the type/schema/generation/constraints
machinery **dormant** (no caller emits a union, so behaviour is byte-identical), then
one **atomic switchover** flips `expand_variants` to emit the union representation. That
keeps exactly one revertable behaviour commit.

| PR | Subject | Scope |
|----|---------|-------|
| 0 ✅ | Phase 1 validation gate (independent) | Reject heterogeneous unions cleanly (panic→error). Landed; **relaxed by PR 4** (not PR 3 — see sequencing note) |
| 1 ✅ | **DataFusion + writer spike (decision gate)** | **Done.** DenseUnion carry-through proven; internal rep = DenseUnion confirmed; *all* writers need conversion (see findings above). Kept guard: `denseunion_spike` |
| 2 ✅ | Dormant machinery | **Done.** Marker `FieldType::Union` + `Field::union_cases: Vec<UnionCase>` (Field isn't `PartialEq`, so no data-carrying enum); `schema.rs` + `generator.rs` `Union` arms; `constraints.rs` needed no change. All dead (expand still emits `None`) |
| 3 (+3.5) ✅ | **Lowering switchover** (gate retained) | **Done.** `lower_heterogeneous_unions` lowers het variants → `FieldType::Union` + `union_cases`; **scalar *and* object cases** (`FieldVariant.fields`); gate retained, unified on `is_heterogeneous`. Proven in-memory. Behaviour-neutral for real runs |
| 4 ✅ | Output encoding + **gate relax** (user-facing behaviour commit) | **Done.** `unionize_for_output` in `write_output` → nullable-superset struct (parquet/json/jsonl); CSV gated at validation (flat, like object fields); blanket gate removed; end-to-end JSONL fixture green. Configurable output-shape flag **deferred** |
| 5 ✅ | Docs + docgen + close-out | **Done.** `docgen` + `schema.json`; concepts/yaml-schema MDX (docs build green, 14 pages); CLAUDE.md (limitation rewrite, feature row → Complete, `VAR-1-OUTPUT-FLAG` future); README; specs moved to `done/`; memory |

**Revert safety.** PR 0 rejects an already-broken config; PRs 1–2 are a kept spike +
dead machinery; **PR 3** adds the lowering pass but stays behaviour-neutral (the gate
still blocks het variants in real runs); **PR 4** is the user-facing behaviour commit
(output conversion + gate relax, atomic). So `git revert <PR4>` re-gates het variants and
removes output conversion cleanly; `git revert <PR3>` removes the lowering pass.

> **Sequencing note (from the PR 1 finding).** Because *no* writer serialises a union, the
> gate relaxation **moves from PR 3 to PR 4**: relaxing before output conversion exists
> would turn the clean validation error into a `write_output` panic. So PR 3 lands the
> lowering pass but keeps the gate and is proven by **in-memory** assertions (case
> distribution, `type_id`s) via direct `expand_field_variants` + `generate_column` calls;
> PR 4 pairs the all-format output conversion with the gate relaxation as the atomic
> user-facing enablement. This is the "adjust subsequent steps after the spike" we
> anticipated.

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

### PR 1 — DataFusion + writer spike (decision gate) ✅ DONE

Landed as `lib/executor.rs::mod denseunion_spike` (4 kept tests). Builds a 6-row batch
with a dense union column (cases `Utf8` + `Float64` + `Struct{a:Int32}`, two rows each)
and exercises the real executor functions:

1. `union_and_shuffle` — ✅ row count + per-case `type_id` histogram preserved across the sort.
2. `evaluate_expressions` — ✅ the union survives a CTE `SELECT *`; the expression column is added.
3. `filter_hidden_columns` — ✅ the union column projects.
4. writer matrix (`parquet` / `json` / `jsonl` / `csv`) — probed under `catch_unwind`
   (parquet *panics* rather than `Err`-ing).

**Findings:**

- **Internal rep = `DenseUnion` confirmed.** All three DataFusion ops carry it cleanly;
  the nullable-superset fallback is not needed.
- **No writer serialises a union natively** — Parquet panics (ARROW-8817); JSON/JSONL/CSV
  fail too. So **PR 4 conversion is required for every format**, and PR 3 proves
  generation in-memory (a full `execute`-to-disk run can't complete until PR 4).
  - *Version check (don't wait for an upstream fix):* we are already on the **latest**
    arrow/parquet **58.3.0** and datafusion **53.1.0** (`cargo info`), and the latest
    parquet still has `DataType::Union(_,_) => unimplemented!("See ARROW-8817.")`. Parquet
    has **no native union type**, so this is a format limitation arrow-rs deliberately
    punts on — not a lagging bug. The PR 4 conversion is the correct design, not a stopgap.

The `denseunion_writer_matrix` test asserts the current (all-unsupported) matrix, so it
flags us if an arrow upgrade changes writer support.

### PR 2 — Dormant machinery ✅ DONE

All additions are unreachable until PR 3 (expand still emits `None` / the PR 0 gate still
fires), so behaviour is byte-identical and the suite stays green (lib tests 104 → 106).

**As-built representation (deviation from the sketch).** The plan floated
`FieldType::Union(Vec<Field>)` (data-carrying). That's not viable: `FieldType` derives
`PartialEq` (used widely via `==`/`matches!`), but `Field` does **not** derive `PartialEq`,
so `Vec<Field>` inside the enum would force `Field: PartialEq` — a large, risky ripple.
Instead, mirroring the existing `Variant` + `variants` idiom:

- **`lib/models.rs`** — a **marker** `FieldType::Union` (unit variant, `#[serde(skip)]` —
  internal-only, never YAML) + `Field::union_cases: Vec<UnionCase>` (`#[serde(skip)]`).
  New `pub struct UnionCase { field: Field, ratio: Option<f64> }` — a case is a *full*
  `Field` (so it can carry a nested object schema and its own generator/value/type — the
  per-case-generator forward contract) plus its row share. `Display for FieldType` gets a
  `union` arm.
- **`lib/schema.rs`** — `field_to_arrow`: `Union` → `DataType::Union(UnionFields, Dense)`
  built from the case `Field`s' Arrow types (`type_id i` ↔ case `i`).
- **`lib/generator.rs`** — `generate_column_raw` gains `FieldType::Union => build_union_column`;
  `build_union_column` draws each row's case **independently** from the declared ratios
  (per-row categorical sampling — *not* a largest-remainder block split, which would
  over-represent the biggest case once the segment/witness pipeline fragments generation
  into small batches; see §Risks), generates each case through **its own** `Field` (so a
  `value:` case is the static generator), and assembles `type_ids` + dense `offsets`.
  `constant_column` gets a `Union` bail arm.
- **`lib/validate.rs`** — the `default`-compat match gains a `Variant | Union` arm
  (unreachable at validate time — kept exhaustive).
- **`lib/constraints.rs`** — **no change.** `FieldConstraints` reads `generator/range/value`,
  not `field_type`, so there's no `FieldType` match to extend. The per-case
  generator-specialisation merge is VAR-SPECIALIZE's (same spectrum) — a noted seam, not
  built here.

Unit tests (`generator.rs::union_tests`) build a `Union` `Field` directly (no
`expand_variants`): assert `field_to_arrow` yields a Dense union of 2 case types, and that
`generate_column` produces a `UnionArray` with the expected per-case (`type_id`) split.

### PR 3 (+ 3.5) — The lowering switchover (gate retained; proven in-memory) ✅ DONE

`lower_heterogeneous_unions` + `is_heterogeneous` + `build_union_case_field` in
`expand_variants.rs`; gate retained but **unified on `is_heterogeneous`** (one definition
of "becomes a union", shared with `validate.rs`). **PR 3.5 (object cases) folded in** per
the sequencing decision: `FieldVariant` gained `fields:`, so object-schema cases (the
supplier `form`) lower to a union of struct cases — the headline case now works through
generation. Full suite green (lib 106 → 111), behaviour-neutral (gate still blocks
het/object variants in real runs until PR 4).

Tests (`expand_variants::tests`): scalar het → `FieldType::Union`; lowered union →
`DenseUnion` 5/5 split; **object-schema variant → union of struct cases, generates a
`DenseUnion`**; `is_heterogeneous` flags object + mixed-scalar, passes same-scalar;
homogeneous variant still → global variants (regression).

**Key correctness point (object heterogeneity):** two object cases are both
`FieldType::Object` but may carry *different schemas*, which `FieldType` equality can't
see. So `is_heterogeneous` treats **any object case** as a union (rather than comparing
schemas) — and the validator shares that exact predicate, so the gate rejects precisely
what lowering produces.

- **`lib/expand_variants.rs`** — a new `lower_heterogeneous_unions` pass runs first in
  `expand_field_variants`: it converts each **heterogeneous** variant field (≥2 distinct
  non-`None` case types) into `field_type = Some(FieldType::Union)` + populated
  `union_cases` (one `UnionCase { field, ratio }` per choice), and clears `variants`. The
  existing same-type path (`collect_variant_paths` → cross-product → typed stub) then sees
  only **homogeneous** variants, unchanged.
- **`lib/validate.rs`** — **gate NOT relaxed here** (correction from the PR 1 finding).
  Since *no* writer serialises a union yet, relaxing the gate before PR 4 would turn the
  current clean validation error into a *write-time panic* for any real run. The gate
  stays until PR 4 ships output conversion; so PR 3 is behaviour-neutral for real runs and
  is proven by tests that call `expand_field_variants` + `generate_column` directly
  (bypassing `validate`).
- **Scope: scalar *and* object cases (PR 3.5 folded in).** `FieldVariant` gained a
  `#[serde(default)] fields: Vec<Field>` member, so an object case carries its own nested
  schema; `build_union_case_field` copies it; `infer_field_type` recognises an object case
  from `fields` even without explicit `type: object`. The supplier `form` (object-A vs
  object-B) now lowers to a union of struct cases and generates.
- **In-memory tests** — a dataset with a scalar-heterogeneous variant; assert (a) after
  `expand_field_variants` the field is `FieldType::Union` with `union_cases` of the right
  arity/ratios/types, and (b) `generate_column` yields a `DenseUnion` with the expected
  per-case (`type_id`) split. The full `execute`-to-disk path waits for PR 4.

Revertable: `git revert` of this PR removes the lowering pass; the gate (still present)
keeps het variants rejected.

### PR 4 — Output encoding + gate relaxation (the user-facing enablement) ✅ DONE

The behaviour commit: heterogeneous variants now run end-to-end to disk.

- **Write/emit path (`lib/executor.rs`)** — `unionize_for_output` runs at the very end,
  inside `write_output`, converting every `DenseUnion` column → a **nullable-superset
  struct** (`union_to_portable`): one nullable sub-field per case, each row populating only
  its active case (others null via `take` with null indices); the populated sub-field is
  the readable case tag. Recurses into struct columns (union nested in an object). Batches
  with no union pass through untouched (`contains_union` fast path).
- **Format support** — **Parquet / JSON / JSONL** work (struct is portable). **CSV is
  not supported** for heterogeneous variants: CSV is flat and can't hold a nested struct —
  the *same* limitation object fields already have. Rather than fail at write, this is a
  clean **validation** error (below).
- **`lib/validate.rs`** — blanket gate removed from `validate_field`; replaced by a
  **CSV-only** gate in `validate_dataset` (`first_heterogeneous_variant` walks fields incl.
  objects): a het variant with `format: csv` errors with a "use parquet/json/jsonl"
  message; struct-capable formats validate fine.
- **Flag — deferred.** v1 ships the single default (nullable-superset). A configurable,
  ParquetConfig-adjacent output-shape selector (e.g. JSON-string, or a materialised tag
  column) is a future add; not needed for the common case. *(Noted as the remaining
  optional item.)*
- **Tests** — `executor::denseunion_spike`: conversion shape (one sub-field/case, exactly
  one non-null per row) + `write_output` succeeds for parquet/json/jsonl and not CSV;
  `executor_tests::test_multitype_variant_writes_union_as_superset_struct`: full pipeline
  on a fixture with a scalar **and** an object het variant → JSONL, asserts 200 rows, both
  object schemas + the scalar case present, every row carries the union columns as nested
  objects; `validate_tests`: CSV errors + JSON passes.

Full suite green (263 tests). CSV scalarisation (JSON-string) is folded into the deferred
flag work if a CSV use case ever needs it.

### PR 5 — Docs + docgen + close-out ✅ DONE

- `src/docgen.rs` — `FieldVariant` doc gains `fields` (object cases) and a heterogeneous-
  union note; `FieldType::variant` enum doc updated. `schema.json` regenerated; docs site
  builds (14 pages).
- `docs/.../concepts/variant-lowering.mdx` — new **Heterogeneous unions (multi-type cases)**
  section: `DenseUnion` internal, nullable-superset output, CSV limitation, object-case
  `fields:` example.
- `docs/.../reference/yaml-schema.mdx` — variant-field note + `FieldVariant.fields` row +
  case-vs-variant wording.
- CLAUDE.md — "Mixed-type variant" known-limitation rewritten to the CSV-only limitation;
  VAR-1 feature row → **Complete** (`specs/done/`); VAR-SPECIALIZE row re-pointed (`one_of`,
  spectrum, VAR-1 complete); `VAR-1-OUTPUT-FLAG` added to Future work; test count ~263.
- README — variant paragraph + test count. Memory: `var1-heterogeneous-unions`.
- Specs moved to `specs/done/` (`git mv`); VAR-1 Status → Complete; VAR-SPECIALIZE
  Dependencies note VAR-1 complete.

---

## Risks

- **DataFusion union support (the spike's job).** Mitigated by making it PR 1 with an
  explicit fallback (internal nullable-superset). Likely fine; cheap to confirm.
- **Per-call case distribution (finding — fixed).** The first cut block-split each
  generate call by largest-remainder (`distribute_rows`). But generation is fragmented
  (per segment / witness slot), and largest-remainder hands every small batch's leftover
  to the biggest-ratio case → gross over-representation (the insurance example showed
  property_damage at ~65% vs declared 30%, χ²≈144). Fix: **per-row independent
  categorical sampling** in `build_union_column` — unbiased regardless of how generation
  is fragmented (the same reason `boolean` ratio samples per row). The case-split unit
  tests became tolerance-based accordingly.
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
