# REFRAME-1 Stage 5.5 — Cumulative scalar reducers for multi-segment staging nodes

Scalar `AccumulateToLinked` reducers (Sum, Max, Min, TakeOne) are now cumulative across
Bernoulli segments. Subsequent calls combine element-wise (add/max/min for mapped rows;
existing value unchanged for unmapped rows) rather than overwriting with the default.
`TakeOne` (renamed from `TakeFirst`, backward-compatible via serde alias) keeps the first
segment's captured value unchanged on subsequent calls. New `accumulate_scalar_cumulative`
helper in `executor.rs` handles `Float64` and `Utf8` linked-field types.

*Implemented in `lib/executor.rs` (`execute_accumulate_to_linked`,
`accumulate_scalar_cumulative`), `lib/models.rs` (`Reducer::TakeOne`),
`lib/validate.rs`. New fixture: `tests/fixtures/execute/segmented_scalar_reduce/`.
New test: `test_segmented_scalar_sum_accumulates_correctly`. All tests pass.*

---

## Problem

When a staging node has a lower cover (two or more Bernoulli segments), `AccumulateToLinked`
is called once per segment per collect binding. Before Stage 5.5, only `Reducer::Collect`
was cumulative — scalar reducers (Sum, Max, Min, TakeFirst) would overwrite the linked
field on each subsequent call, discarding contributions from earlier segments for linked
rows that appeared in only one segment.

The broken scalar path:
```rust
// BROKEN: second segment overwrites first segment's results.
let take_indices: UInt32Array = (0..linked_n as u32).map(|linked_row| {
    idx_map.get(&linked_row)
        .map(|&agg_row| agg_row as u32)
        .unwrap_or(agg_n as u32 + linked_row)  // unmapped → reset to default!
}).collect::<Vec<u32>>().into();
```

For a linked row that was mapped in segment 1 but not in segment 2, the second call
resets its value to `default_val` (e.g. 0.0), discarding the first segment's contribution.

Conservation failure: with 10 plays × cardinality 2 × score 5 = expected 100 total, the
broken code produces less than 100 (because some linked rows lose their first-segment
contributions when not drawn in the second segment).

---

## `TakeFirst` → `TakeOne` rename

`Reducer::TakeFirst` is renamed to `Reducer::TakeOne` in `models.rs`:
```rust
pub enum Reducer {
    #[serde(alias = "take_first")]
    TakeOne,
    Sum,
    Max,
    Min,
    Collect,
}
```

The `#[serde(alias = "take_first")]` ensures all existing YAML files using `reducer: take_first`
continue to work. The rename reflects that the reducer keeps whichever segment's value was
captured first (from whichever segment happens to run first) rather than guaranteeing a
specific deterministic ordering across all rows.

`lib/validate.rs` updated: `Reducer::TakeFirst` → `Reducer::TakeOne` in the match arm.
`lib/executor.rs` updated: `Reducer::TakeFirst` → `Reducer::TakeOne` in the `aggr_expr` match.

---

## `lib/executor.rs` — cumulative scalar reducers

**First-vs-subsequent tracking** — same `accumulated_fields: HashSet<(PathBuf, String)>`
used by Collect. `HashSet::insert` returns `true` on first insertion (= first call per
field), `false` on subsequent calls.

**Updated `_ =>` branch in `execute_accumulate_to_linked`**:
```rust
_ => {
    // Scalar reducers (Sum, Max, Min, TakeOne).
    // First accumulation: mapped rows get the aggregated value; unmapped rows get default_val.
    // Subsequent accumulations (multi-segment staging):
    //   TakeOne → keep existing value unchanged (= whichever segment captured it first).
    //   Sum/Max/Min → combine element-wise with existing via accumulate_scalar_cumulative.
    let is_first = accumulated_fields.insert((linked_path.clone(), linked_field.to_string()));
    if !is_first && matches!(reducer, Reducer::TakeOne) {
        existing_linked_col.clone()
    } else if !is_first {
        accumulate_scalar_cumulative(reducer, &existing_linked_col, &agg_values_col, &idx_map, linked_n)?
    } else {
        let agg_n = agg_batch.num_rows();
        let take_indices: UInt32Array = (0..linked_n as u32).map(|linked_row| {
            idx_map.get(&linked_row)
                .map(|&agg_row| agg_row as u32)
                .unwrap_or(agg_n as u32 + linked_row)
        }).collect::<Vec<u32>>().into();
        let default_col = yaml_value_to_array(default_val, existing_linked_col.data_type(), linked_n);
        let combined = concat(&[agg_values_col.as_ref(), default_col.as_ref()])?;
        take(combined.as_ref(), &take_indices, None)?
    }
}
```

**New `accumulate_scalar_cumulative` helper**:

For each linked row:
- If that row has a new aggregated value (`idx_map` contains it): combine existing + new.
- Otherwise: keep existing unchanged (no reset to default).

Supports `Float64` and `Utf8` data types. For `Float64`:
```rust
match reducer {
    Reducer::Sum => ev + av,
    Reducer::Max => f64::max(ev, av),
    Reducer::Min => f64::min(ev, av),
    _ => ev,  // TakeOne handled before this call
}
```

For `Utf8` (Max/Min by lexicographic order), uses `StringBuilder`. Null values treated as
empty string / 0.0 for combination purposes.

**New Arrow imports**: `Array` trait (for `.is_null()` method) and `StringBuilder`.

---

## New fixture: `tests/fixtures/execute/segmented_scalar_reduce/`

**`game.yaml`** (linked dataset, 3 rows):
```yaml
name: game
format: jsonl
output_file: game
rows: 3
data:
  - name: total
    type: number
    default: 0
```

**`plays.yaml`** (staging node with list-link to game, has lower cover via child_a/child_b):
```yaml
name: plays
format: jsonl
output_file: plays
rows: 10
links:
  - file: game.yaml
    ref: game
    cardinality: 2
data:
  - name: tag
    type: string
  - name: played_games
    type: list
    content:
      from: game
      fields:
        - name: score
          type: number
          value: 5
          refs:
            - bind: game.total
              reducer: sum
```

**`child_a.yaml`** (lower cover, ratio: 0.4):
```yaml
name: child_a
format: jsonl
output_file: child_a
include:
  file: plays.yaml
  ref: plays
  ratio: 0.4
data:
  - name: tag
    ref: plays.tag
    value: "A"
```

**`child_b.yaml`** (lower cover, ratio: 0.6):
```yaml
name: child_b
format: jsonl
output_file: child_b
include:
  file: plays.yaml
  ref: plays
  ratio: 0.6
data:
  - name: tag
    ref: plays.tag
    value: "B"
```

**Conservation law**: 10 plays × cardinality 2 × score 5 = 100 total draws, each
contributing score=5 to exactly one game row. `sum(game.total) = 100.0` always, regardless
of which game rows are drawn. Without the fix, the second segment's call resets game rows
not drawn in segment 2 to 0, breaking the conservation law.

---

## New test: `test_segmented_scalar_sum_accumulates_correctly`

```rust
#[tokio::test]
async fn test_segmented_scalar_sum_accumulates_correctly() {
    let out = run("tests/fixtures/execute/segmented_scalar_reduce").await;

    let games = jsonl_rows(&out, "game");
    assert_eq!(games.len(), 3, "game should have 3 rows");

    // Conservation law: every draw contributes 5 to some game row.
    let grand_total: f64 = games.iter()
        .map(|g| g["total"].as_f64().expect("game.total should be a number"))
        .sum();
    assert!(
        (grand_total - 100.0).abs() < 0.001,
        "sum(game.total) should equal 10 plays × 2 cardinality × score 5 = 100; got {grand_total}"
    );
}
```
