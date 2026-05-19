# MULT-2: Cross-include reducers and full multiplicity hierarchy

Extends MULT-1 to handle fields that ref across two includes with differing multiplicities, and propagates `_slot_idx` fully through multi-level inclusion hierarchies.

## Multiple include refs



## Reducers

When a field in a dataset refs a column from an include that has a different multiplicity than another include on the same dataset, the M values for that field must be reduced to one. Planned reducers:

- `take-first` — deterministic default (already implicit in MULT-1 parent assembly)
- `sum`, `max`, `min` — scalar aggregation
- `collect` — gather values into a list (direct prerequisite for REL)

Reducer is declared on the field, not the include.

## Full `_slot_idx` hierarchy propagation

A grandchild dataset including both a multiplied intermediate and its original parent must see a consistent join key across the hierarchy. Requires `_slot_idx` to be carried as a hidden prefill through all levels, not just one hop.

## Without-replacement sampling for nested includes

An option on nested-include `multiplicity` to enforce uniqueness of pool rows within a single outer row's list.
