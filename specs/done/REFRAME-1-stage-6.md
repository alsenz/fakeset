# REFRAME-1 Stage 6 — Cardinality validation against eligible linked-dataset size

`check_reinforcement_zero_feasibility` renamed to `check_cardinality_feasibility` and
extended to cover two failure classes:

1. **Empty linked dataset** (all reinforcement modes): n_eligible == 0 → bail with a
   clear message before any sampling is attempted.
2. **Without-replacement infeasibility** (reinforcement=0 only): Fixed(N) > n_eligible
   → bail; Uniform{min} > n_eligible → bail (new — previously only max was checked);
   Normal → bail (unchanged). Uniform max > n_eligible no longer errors — the runtime
   cap in `execute_witness` handles it silently.

`max_cardinality_bound` helper removed (now dead code).

*Implemented in `lib/plan.rs` (`check_cardinality_feasibility`), `lib/executor.rs`
(runtime Uniform max-cap in `execute_witness`). New fixtures:
`tests/fixtures/validation/card_fixed_pool_too_small/`,
`tests/fixtures/validation/card_uniform_min_too_large/`,
`tests/fixtures/execute/no_replacement_max_cap/`. New tests:
`card_fixed_pool_too_small_errors`, `card_uniform_min_too_large_errors` (plan_tests.rs),
`test_no_replacement_max_cap` (executor_tests.rs). All tests pass.*

---

## Problem

The existing `check_reinforcement_zero_feasibility` had two gaps:

1. No check for n_eligible == 0 under any reinforcement mode — `sample_linked_weighted`
   would panic if given an empty eligible pool.
2. For `Uniform{min, max}` with reinforcement=0, only `max > n_eligible` was checked.
   This was too strict: if max > n_eligible but min ≤ n_eligible the configuration is
   feasible (just capped). And the converse — min > n_eligible — was not caught at all.

---

## `lib/plan.rs` — rename and extend `check_reinforcement_zero_feasibility`

Rename to `check_cardinality_feasibility`. Same signature:

```rust
fn check_cardinality_feasibility(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    row_counts: &HashMap<PathBuf, usize>,
) -> Result<()>
```

Update call site in `build_plan`.

**Eligible linked-dataset size** (`n_eligible`) — computed at plan time as the
ratio-filtered linked-dataset row count:
```
(ratio * linked_rows).round().max(1).min(linked_rows)
```
Segment-constraint-filtered sizes are not available at plan time.

**For nested-include (list-link) fields**, three phases:

*Phase 1 — bail (all reinforcement modes):*
```rust
if n_eligible == 0 {
    anyhow::bail!(
        "dataset '{}' field '{}': linked dataset '{}' has 0 eligible rows \
         (after applying ratio); cannot draw any items",
        dataset.name, field.name, link.reference
    );
}
```

*Phase 2 — bail (reinforcement=0 only):*
```rust
if link.reinforcement == Some(0.0) {
    let cardinality = link.cardinality.clone().unwrap_or(CountSpec::Fixed(1));
    match &cardinality {
        CountSpec::Normal { .. } => anyhow::bail!("...Normal cardinality incompatible..."),
        CountSpec::Fixed(n) if *n > n_eligible => anyhow::bail!("...cardinality is {n}"),
        CountSpec::Uniform { min, .. } if *min > n_eligible => anyhow::bail!("...min is {min}"),
        _ => {}
    }
}
```

*Phase 3 — Uniform max-cap (reinforcement=0, no plan-time error):*
`Uniform { max }` where `max > n_eligible` does NOT bail — the runtime cap handles it.

**For junction links** — same Phase 1 empty-pool guard added. The existing
`junction_rows > n_eligible` check for reinforcement=0 is unchanged.

**`max_cardinality_bound` removed** — no longer needed after switching to a direct
`match &cardinality`.

---

## `lib/executor.rs` — runtime Uniform max-cap

In `execute_witness`, inside the `counts` generation loop, clamp the sampled count when
`reinforcement == Some(0.0)`:

```rust
let counts: Vec<usize> = if n_eligible == 0 {
    vec![0; n]
} else {
    (0..n).map(|_| {
        let m_n = sample_count(cardinality);
        // Clamp to n_eligible for without-replacement so Uniform max values that
        // exceed the (constraint-filtered) eligible size don't panic the sampler.
        if include.reinforcement == Some(0.0) { m_n.min(n_eligible) } else { m_n }
    }).collect()
};
```

---

## New test fixtures

**`tests/fixtures/validation/card_fixed_pool_too_small/`**
- `linked.yaml` — 2 rows
- `outer.yaml` — links linked with `cardinality: 5`, `reinforcement: 0`
- Expected planning error: Fixed(5) > n_eligible=2

**`tests/fixtures/validation/card_uniform_min_too_large/`**
- `linked.yaml` — 3 rows
- `outer.yaml` — links linked with `cardinality: {min: 5, max: 10}`, `reinforcement: 0`
- Expected planning error: min=5 > n_eligible=3

**`tests/fixtures/execute/no_replacement_max_cap/`**
- `linked.yaml` — 4 rows
- `outer.yaml` — 5 rows, links linked with `cardinality: {min:1, max:10}`, `reinforcement: 0`
- Expected: each outer row has 1–4 items (capped at n_eligible=4), no duplicate linked ids
  within a row. Verifies silent runtime cap instead of panic.

---

## New tests

**`plan_tests.rs`**
```rust
#[test]
fn card_fixed_pool_too_small_errors() {
    let err = plan_err_for("tests/fixtures/validation/card_fixed_pool_too_small");
    let msg = err.to_string();
    assert!(msg.contains("reinforcement") && msg.contains("eligible"), "...");
}

#[test]
fn card_uniform_min_too_large_errors() {
    let err = plan_err_for("tests/fixtures/validation/card_uniform_min_too_large");
    let msg = err.to_string();
    assert!(msg.contains("reinforcement") && msg.contains("min"), "...");
}
```

**`executor_tests.rs`**
```rust
#[tokio::test]
async fn test_no_replacement_max_cap() {
    let out = run("tests/fixtures/execute/no_replacement_max_cap").await;
    let rows = jsonl_rows(&out, "outer");
    assert_eq!(rows.len(), 5);
    for (i, row) in rows.iter().enumerate() {
        let items = row["items"].as_array()...;
        assert!(items.len() <= 4);   // capped at n_eligible=4
        assert!(!items.is_empty());  // min cardinality respected
        // no duplicate ids within a row (reinforcement=0)
    }
}
```
