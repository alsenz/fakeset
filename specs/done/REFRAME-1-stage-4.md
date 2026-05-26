# REFRAME-1 Stage 4 — `_staging_refs` witness schema

The core structural change. The witness batch is now one row per **unique** linked-row draw,
with `_staging_refs: List<UInt32>` recording all source-slot indices that drew that linked row.
The old junction-table model (one row per (source-slot, linked-row) pair with `_slot_idx +
_linked_idx + inner content fields`) has been replaced.

*Implemented in `lib/executor.rs`: `execute_witness`, `unnest_staging_refs`,
`execute_assemble_from_witness`, `execute_accumulate_to_linked`. New fixture:
`tests/fixtures/execute/staging_refs_dedup/`. New test:
`test_staging_refs_deduplicates_linked_rows`. All 174 tests pass.*

---

## Current vs target witness schema

**Before** (junction table, `total` rows = Σ cardinalities):

```
_slot_idx:  UInt32            — which staging slot made this draw
_linked_idx: UInt32           — which linked batch row was drawn
<content fields>              — one value per draw (linked-scoped, outer-scoped, or plain)
```

**After** (one row per unique linked-row draw, ≤ `linked_batch.len()` rows):

```
_linked_idx: UInt32           — which linked batch row this witness row represents (hidden)
_staging_refs: List<UInt32>   — all staging slot indices that drew this linked row (hidden)
<linked-scoped content fields>  — value taken from linked batch (same for every draw)
<plain content fields>          — generated once per unique linked row
```

Outer-scoped content fields (those whose `simple_ref()` resolves in the staging batch rather
than the linked batch) are **not stored in the witness**. They are looked up from the staging
batch at assembly time using the per-slot index recovered by unnesting `_staging_refs`.

---

## `lib/executor.rs` — `execute_witness`

**Phase 1 — sampling** (unchanged logic):

```rust
// n_eligible_slots, counts, slot_assignments, staging_idxs — all unchanged.
let total = counts.iter().sum::<usize>();
```

**Phase 2 — deduplication: group by linked row**:

```rust
// Build linked_idx → Vec<slot_idx> using a BTreeMap so order is deterministic.
let mut draw_map: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
for k in 0..total {
    draw_map
        .entry(slot_assignments.value(k))
        .or_default()
        .push(staging_idxs[k]);
}
let unique_linked_idxs: Vec<u32> = draw_map.keys().copied().collect();
let n_witness = unique_linked_idxs.len();

// Build _staging_refs ListArray.
let mut refs_offsets: Vec<i32> = vec![0];
let mut refs_values: Vec<u32> = Vec::new();
for &linked_idx in &unique_linked_idxs {
    let slots = &draw_map[&linked_idx];
    refs_values.extend_from_slice(slots);
    refs_offsets.push(refs_values.len() as i32);
}
let staging_refs_array = ListArray::new(
    Arc::new(ArrowField::new("item", DataType::UInt32, false)),
    OffsetBuffer::new(ScalarBuffer::from(refs_offsets)),
    Arc::new(UInt32Array::from(refs_values)),
    None,
);
let unique_linked_arr = UInt32Array::from(unique_linked_idxs.clone());
```

**Phase 3 — build witness columns** (one value per unique linked row):

- linked-scoped: `take(linked_batch.column(idx), &unique_linked_arr)`
- outer-scoped: `continue` (not stored in witness)
- plain: `generate_column(field, n_witness, &[])`

**Phase 4 — assemble witness batch**:

```rust
let with_refs = prepend_column(&data_batch, "_staging_refs", Arc::new(staging_refs_array))?;
let witness_batch = prepend_column(&with_refs, "_linked_idx",
    Arc::new(UInt32Array::from(unique_linked_idxs)))?;
computed.insert(witness_key.clone(), witness_batch);
```

---

## `lib/executor.rs` — `unnest_staging_refs` (new helper)

Expands a witness batch back to an anonymous junction table: one row per (staging-slot,
linked-row) pair. Sorts by `_slot_idx` (required for the offset-based list-fold in assembly).

```rust
fn unnest_staging_refs(
    witness: &RecordBatch,
) -> Result<(RecordBatch, UInt32Array, UInt32Array)>
// Returns: (junction, slot_arr_sorted, witness_row_arr_sorted)
// junction columns: _slot_idx + replicated linked-scoped/plain witness columns (no sentinels)
```

Implementation:
1. Read `_staging_refs` ListArray
2. For each witness row `wr`, push each `slot` from `_staging_refs[wr]` into `slot_idxs` and `wr` into `witness_row_idxs`
3. `sort_to_indices(&slot_arr)` to sort by slot
4. Strip `_linked_idx` and `_staging_refs`; prepend `_slot_idx`; `take` remaining cols by `witness_row_arr_sorted`

---

## `lib/executor.rs` — `execute_assemble_from_witness`

For each `(field_name, witness_key, project_col)`:

1. Call `unnest_staging_refs(&witness)` → `(junction, slot_arr_sorted, _)`
2. Identify outer-scoped fields: content fields absent from the stripped witness columns
3. For each outer-scoped field: `take(staging.column(stg_idx), &slot_arr_sorted)` → `add_column(junction, …)`
4. Read `_slot_idx` from junction, count per slot, `strip_sentinel(junction, "_slot_idx")`
5. Existing project_col / struct-fold logic unchanged

Removes the old `strip_linked_idx(strip_slot_idx(inner))` calls.

---

## `lib/executor.rs` — `execute_accumulate_to_linked`

Pre-step added at top: if source batch has `_staging_refs`, expand to junction table before aggregating.
Uses a sort-free inline expansion (DataFusion handles grouping order):

```rust
let source_batch = if source_batch.schema().index_of("_staging_refs").is_ok() {
    // Replicate each witness row N times (N = len of its _staging_refs list).
    // strip _staging_refs; keep _linked_idx (replicated → correct for grouping).
    let stripped = strip_sentinel(raw_source, "_staging_refs");
    // take(stripped.columns(), witness_row_arr) for each column
    RecordBatch::try_new(schema, cols)?
} else {
    raw_source
};
// Existing DataFusion aggregate unchanged: group by "_linked_idx".
```

---

## `lib/plan.rs`

No changes. `GenerateWitness.inner_fields` still carries all content field definitions
(including outer-scoped); `execute_witness` skips outer-scoped at runtime.

---

## Tests

New test: `test_staging_refs_deduplicates_linked_rows`
New fixture: `tests/fixtures/execute/staging_refs_dedup/`

- `linked.yaml`: 1 row, `item_name: word`, `drawn_by: list<string>, default: []`
- `source.yaml`: 3 rows, `links: [{file: linked.yaml, ref: linked_item, cardinality: 1, reinforcement: 0}]`
  - list field `items` with `name: linked_item.item_name` (linked-scoped) + collect → `linked_item.drawn_by`

Assertions:
1. `source` has 3 rows, each with exactly 1 item
2. All items' `name` equals the single linked row's `item_name`
3. `linked.drawn_by` has exactly 3 entries (witness had 1 row, unnested to 3 junction rows)

**Known behavioural change**: plain (generator-based) content fields now carry one value per
unique linked-row draw. If two staging slots draw the same linked row, both list entries share
the same plain-field value. No existing test asserts per-slot uniqueness of plain fields.
