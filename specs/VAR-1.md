# VAR-1 — Mixed-type variant fields and `any`-type encoding

## Status

Planned — Phase 1 (validation gate) is a quick fix; Phase 2 (`type: any`) needs
design sign-off before implementation.

## Background

`type: variant` lets a field take one of N discrete shapes, each with its own value,
generator, and optionally its own type. The common case is same-type choices (e.g.
all string constants). The rarer case is heterogeneous choices — a string in one
variant, a number in another.

After `expand_field_variants`, the original `type: variant` field is replaced by a
typed stub in `data`:

- **Same-type choices** — stub gets `field_type = <shared type>`, `value = None`.
  `resolve_refs`, `schema_to_arrow`, and generation all see a concrete type and
  proceed normally.
- **Mixed-type choices** — `stub_variant_fields` leaves `field_type = None` (it
  can't unify the types). `schema_to_arrow` and `generate_column_raw` both call
  `.expect("field_type unresolved")` — **runtime panic**, no user-facing error.

There is currently no validation gate that catches this before execution.

## Problem scope

### P1 — Mixed-type variant field causes a runtime panic (not a clean error)

Any schema with a variant field whose choices span two or more types produces an
opaque thread-panic instead of a diagnostic. Affects:

- Direct generation of the dataset that owns the variant field.
- Any child `ref:` to that field (the ref inherits `field_type = None` via
  `resolve_refs`).

### P2 — Same-type variants can't be meaningfully `ref:`-d by a child

Even when the stub is correctly typed, the stub's `value` is `None` — the variant
choices are expressed as global `variants:` entries, not on the stub itself. A child
that refs the field gets the correct Arrow type but generates a fresh random value
(not one of the variant choices). This is a semantic gap: the user expects the child
to see the same constrained value the parent carries in each variant context.

P2 is a deeper design question about constraint propagation direction (children are
generated before parents in the lattice) and is **out of scope for this spec**. It
is noted here for completeness; VAR-1 addresses P1 only.

## Proposed solution

### Phase 1 — Validation gate (reject mixed-type variants)

Add a check in `lib/validate.rs` that runs after `build_dag` but before
`expand_field_variants`. For every `type: variant` field, infer the type of each
choice (using the same logic as `infer_field_type` in `expand_variants.rs`) and
reject the schema if any two choices produce different non-None types.

Error message shape:
```
dataset 'foo': variant field 'bar' has inconsistent choice types
  — choice 0: String ("hello")
  — choice 1: Number (range 1..10)
  All choices in a variant field must share the same type.
```

This is conservative but safe: it converts a silent panic into a clear error and
unblocks users immediately. Most real schemas don't need mixed-type variants.

**Files:**
- `lib/validate.rs` — new check in the field-validation walk
- `tests/validate_tests.rs` — test for the new error

### Phase 2 — `type: any` encoding (if genuine mixed-type use cases emerge)

If users need heterogeneous variant values, introduce a first-class `type: any`
field type. The "any" encoding stores every value as its JSON representation in a
`Utf8` Arrow column. This is the pragmatic choice given Arrow/Parquet constraints:

- Arrow's `DenseUnion` is not supported by most Parquet readers.
- A nullable-struct-with-type-code approach works but makes the output schema
  unusable without custom post-processing.
- JSON-string is universally readable and round-trips without information loss
  (numbers, booleans, strings, and nulls all have distinct JSON representations).

Behaviour under Phase 2:
- `type: any` (explicit) or a mixed-type `type: variant` (implicit upgrade) →
  `FieldType::Any`
- `schema_to_arrow` / `field_to_arrow`: `FieldType::Any` → `DataType::Utf8`
- `generate_column_raw`: if `field.value` is set, JSON-serialize it; otherwise
  generate `"null"` (or a configurable fallback)
- `constant_column`: add `FieldType::Any` arm — `serde_json::to_string(val)?`
- `ref:` to an `any` field: child receives `Utf8`; the ref constraint carries the
  JSON-string value if the parent variant pins it

**Files (Phase 2):**

| File | Change |
|------|--------|
| `lib/models.rs` | Add `FieldType::Any` |
| `lib/expand_variants.rs` | `infer_field_type`: mixed-type combo → `Some(FieldType::Any)`; `stub_variant_fields`: unified_type for mixed → `Some(FieldType::Any)` |
| `lib/schema.rs` | `field_to_arrow`: `Any` → `DataType::Utf8` |
| `lib/generator.rs` | `generate_column_raw`: `Any` arm; `constant_column`: `Any` arm (JSON-serialize) |
| `lib/constraints.rs` | `Satisfiable` / `Merge` for `Any` |
| `src/docgen.rs` | Document `any` type in `FieldDoc` |
| `docs/src/content/docs/reference/yaml-schema.mdx` | `any` type entry |
| `docs/src/content/docs/reference/generators.mdx` | Note `any` has no generators (value only) |

## Decision needed for Phase 2

Before implementing Phase 2, decide:

1. **Explicit opt-in vs implicit upgrade** — should a mixed-type `type: variant`
   automatically emit as `any`, or should the user be required to declare
   `type: any` explicitly and have the validator reject mixed-type `type: variant`
   as before?
2. **Generator support** — `type: any` with no `value` is meaningless (what would a
   random "any" look like?). Phase 2 likely means `type: any` is value-only (no
   generator), validated accordingly.
3. **`ref:` semantics** — a child that refs an `any`-typed field gets `Utf8`. Is
   that acceptable, or should type-aware casting be supported?
