# DQ-1 — Data Quality

## Status

Complete — implemented in `lib/dq.rs`, `lib/models.rs`, `lib/plan.rs`, `lib/executor.rs`, `lib/validate.rs`.

## Motivation

Real-world datasets are messy. They contain duplicates, data-entry errors, missing values, and corrupted fields.

fakeset should be able to generate synthetically degraded data alongside clean data — either instead of, or in addition to, the clean baseline.

## YAML interface

### `output` field

`SyntheticDataset` gains an `output` field (replacing `output_file`) that accepts either:

- A plain string — shorthand for `output.file`.
- An `Output` block.

`Output` has two fields:

- `file` (required): string path, same semantics as the old `output_file`.
- `quality` (optional): a `DataQuality` stanza.

### Multiple output files

A dataset may also declare `outputs: [Output]` — a list of `Output` entries. `output` is syntactic sugar for a single-entry `outputs` list. This lets you produce both a clean file and a degraded file from the same generated batch:

```yaml
outputs:
  - file: clean/customers.parquet
  - file: dirty/customers.parquet
    quality:
      nulls: 0.05
      corruptions:
        character_deletion: 0.02
        truncation: 0.05
```

### Field-level quality

`Field` gains an optional `quality: DataQuality` field for per-field overrides. A field-level `quality` stanza is only valid when the dataset also declares an `output` (or `outputs`) block with a `quality` stanza; it is a validation error otherwise.

## `DataQuality` model

All probability values are **independent Bernoulli rates**: each eligible cell fires independently at the stated probability.

### Dataset-level only

| Field | Type | Description |
|-------|------|-------------|
| `duplication` | float 0–1 | Fraction of rows to duplicate. Each selected row is appended once as an exact copy. |
| `missing` | float 0–1 | Fraction of rows to delete (applied after duplication). |

### All levels (dataset or field)

| Field | Type | Description |
|-------|------|-------------|
| `nulls` | float 0–1 | Per-cell probability of replacing the value with null. |
| `default_rate` | float 0–1 | Per-cell probability of replacing the value with a type-appropriate default (see Default values). |
| `corruptions` | `Corruptions` | Sub-object controlling per-mode corruption probabilities (see below). |

### Field-level only

| Field | Type | Description |
|-------|------|-------------|
| `default_values` | list of values | Custom default values to draw from when `default_rate` fires. Must be compatible with the field type. |
| `defaults_mode` | `override \| extend` | Whether `default_values` replaces (`override`) or augments (`extend`) the built-in default set. Default: `extend`. |

## `Corruptions` sub-object

`corruptions` is a struct; each sub-field is an independent per-cell probability. Only modes applicable to the field's type are evaluated. Specifying an inapplicable mode at the field level is a validation error; at the dataset level, inapplicable modes are silently skipped for non-matching fields.

| Field | Type | Applies to | Description |
|-------|------|------------|-------------|
| `character_deletion` | float 0–1 | string | Delete one random character. |
| `character_insertion` | float 0–1 | string | Insert one random ASCII character at a random position. |
| `truncation` | float 0–1 | string | Truncate to a random prefix length. Models VARCHAR overflow, a particularly common source of real-world data corruption. |
| `encoding` | float 0–1 | string | Introduce an encoding corruption — re-encode a random substring through a lossy codepage (e.g. a latin-1 round-trip), producing mojibake. |
| `noise` | float 0–1 | number | Per-cell probability of adding Gaussian noise. Noise amplitude = `noise_scale × σ`, where σ is the column's empirical std dev over the clean batch. If the column is constant (σ = 0), `noise_scale` is used directly as the amplitude. This keeps perturbations proportional to the natural variation in the data. |
| `noise_scale` | float, default 1.0 | number | Multiplier on the column std dev for noise amplitude. Not a probability. |
| `day_shift` | float 0–1 | date, date_time | Per-cell probability of shifting the value by a random number of days drawn uniformly from `[−day_shift_max, +day_shift_max]`. |
| `day_shift_max` | int, default 30 | date, date_time | Maximum absolute shift in days. Not a probability. |

## Default values

When `default_rate` fires, the replacement is drawn uniformly from the field's active default set:

| Type | Built-in defaults |
|------|-------------------|
| string | `""`, `"N/A"`, `"NA"`, `"None"`, `"NULL"`, `"n/a"` |
| number | `0`, `0.0` |
| boolean | `false` |
| date | `1970-01-01`, `1900-01-01`, `9999-12-31` |
| date_time | `1970-01-01T00:00:00Z` |
| object, list | *(no-op — `default_rate` is silently ignored)* |

For generators with well-known natural defaults (e.g. `latitude`/`longitude` → `0.0`/`0.0`, `date`/`date_time` → epoch), those defaults are added to the built-in set.

`default_values` at the field level provides additional or replacement values; `defaults_mode` controls whether that list extends or replaces the built-in set.

## Order of operations

DQ transforms are applied in the following order after the clean batch is finalised, with each step feeding into the next:

1. **Duplication** — append duplicate rows.
2. **Missing** — delete rows.
3. **Nulls** — null individual cells.
4. **Defaults** — replace cells with default values.
5. **Corruptions** — apply corruption modes in declaration order.

## Interaction with the include lattice

None. Data quality is applied strictly to output files, after the full execution DAG has completed. The batch stored in `computed` — used for ref inheritance, witness generation, and `AccumulateToLinked` — is always the clean batch. A DQ-transformed output is never read back or used for any further computation.

Concretely:
- Row-count changes from `duplication` or `missing` do not affect the row counts of included or linked datasets.
- Shared output files (produced by `WriteSharedOutput`) always use the clean batch.
- A DQ output could conceptually run as a final sweep over all written files after the DAG completes; the design should keep this door open.

## Implementation

Data quality is a pure post-processing pass applied immediately before each file write. It does not touch planning, segmentation, ref resolution, or any execution step other than the final write.

Suggested structure: a new `lib/dq.rs` module exposing `apply_data_quality(batch: RecordBatch, quality: &DataQuality, schema: &Schema) -> Result<RecordBatch>`, called from the write path in `executor.rs`. Column statistics needed for `noise` (mean, std dev) can be computed over the clean batch with Arrow's `compute::aggregate` before the DQ pass begins.

---

## Implementation plan

### Approach

The entire write path already funnels through a single choke point: `emit_batch` pushes batches into the `shared` HashMap, and one `WriteSharedOutput` step per output file unions, shuffles, and writes the final file. The DQ pass slots in at `WriteSharedOutput`, after union-and-shuffle and before `write_output`. Multiple outputs per dataset are handled by pushing each batch to all output-file slots in `shared` and registering a separate `WriteSharedOutput` per output.

No changes to the generation pipeline, segmentation, ref resolution, or any lattice machinery.

---

### Step 1 — Models (`lib/models.rs`)

**New types:**

```rust
pub struct Output {
    pub file:    String,
    pub quality: Option<DataQuality>,
}

// Untagged: `output: customers.parquet`  or  `output: {file: ..., quality: ...}`
#[serde(untagged)]
pub enum OutputSpec {
    Shorthand(String),
    Block(Output),
}

pub struct DataQuality {
    // dataset-level only
    pub duplication:  Option<f64>,
    pub missing:      Option<f64>,
    // all levels
    pub nulls:        Option<f64>,
    pub default_rate: Option<f64>,
    pub corruptions:  Option<Corruptions>,
    // field-level only
    pub default_values: Option<Vec<serde_yaml::Value>>,
    pub defaults_mode:  Option<DefaultsMode>,
}

#[serde(rename_all = "lowercase")]
pub enum DefaultsMode { Override, Extend }

pub struct Corruptions {
    // string modes
    pub character_deletion:  Option<f64>,
    pub character_insertion: Option<f64>,
    pub truncation:          Option<f64>,
    pub encoding:            Option<f64>,
    // number modes
    pub noise:       Option<f64>,
    #[serde(default = "default_noise_scale")]   // 1.0
    pub noise_scale: f64,
    // date / date_time modes
    pub day_shift:     Option<f64>,
    #[serde(default = "default_day_shift_max")] // 30
    pub day_shift_max: i64,
}
```

**`SyntheticDataset` changes:**

- Replace `output_file: Option<String>` with `output: Option<OutputSpec>` and `outputs: Option<Vec<OutputSpec>>`.
- Add helper:

```rust
pub fn resolved_outputs(&self) -> Vec<Output>
```

Returns a flat, normalised list (interpreting `Shorthand(s)` as `Output { file: s, quality: None }`). If both `output` and `outputs` are set, `outputs` wins; if only `output` is set, return a single-element vec; if neither, return empty.

- `plan.rs` constructs synthetic `SyntheticDataset` values (staging, witness, lower-cover members) directly. All `output_file: None` call sites → `output: None, outputs: None`; `output_file: Some(key)` → `output: Some(OutputSpec::Shorthand(key)), outputs: None`.

**`Field` changes:**

Add `pub quality: Option<DataQuality>`.

---

### Step 2 — New module `lib/dq.rs`

Public entry point:

```rust
pub fn apply_data_quality(
    batch:   RecordBatch,
    quality: &DataQuality,
    schema:  &[Field],     // for field-level quality lookup and type info
) -> Result<RecordBatch>
```

Internal pipeline — each function returns a new `RecordBatch`:

| Function | Notes |
|----------|-------|
| `apply_duplication(batch, rate) -> RecordBatch` | Sample `round(rate × n)` rows uniformly; `concat_batches([original, sampled])`. |
| `apply_missing(batch, rate) -> RecordBatch` | Build a Bernoulli boolean keep-mask (1−rate per row); Arrow `filter(batch, mask)`. |
| `apply_column_transforms(batch, dataset_quality, schema) -> Result<RecordBatch>` | Iterate columns; merge dataset-level + field-level quality; apply nulls → defaults → corruptions per column. |

Column-level helpers (each takes a single `ArrayRef`, returns `ArrayRef`):

| Function | Applies to |
|----------|------------|
| `apply_nulls(col, rate)` | All types |
| `apply_defaults(col, field, rate, values, mode)` | All types with a defined default set |
| `apply_corruptions(col, field_type, corruptions, stddev)` | Dispatcher; calls applicable sub-functions below |
| `corrupt_char_deletion(col, rate)` | string |
| `corrupt_char_insertion(col, rate)` | string |
| `corrupt_truncation(col, rate)` | string |
| `corrupt_encoding(col, rate)` | string — re-encode a random substring through `windows_1252` (or latin-1), producing mojibake |
| `corrupt_noise(col, rate, noise_scale, stddev)` | number — `N(0, noise_scale × stddev)` per firing cell; if `stddev == 0` use `noise_scale` directly |
| `corrupt_day_shift(col, rate, day_shift_max)` | date, date_time — uniform shift in `[−max, +max]` days |

**Column std dev pre-computation:**

```rust
fn compute_stddevs(batch: &RecordBatch) -> HashMap<String, f64>
```

Compute once before the corruption pass using Arrow's `compute` functions. Pass into `apply_corruptions`. For integer columns, cast to `Float64` before computing.

**Dataset-level / field-level merge:**

```rust
fn effective_field_quality<'a>(
    field:     &'a Field,
    dataset_q: &'a DataQuality,
) -> EffectiveFieldQuality<'a>
```

Returns a view where field-level rates take precedence slot-by-slot. `default_values`/`defaults_mode` come only from the field; `duplication`/`missing` are already consumed at the batch level and are not part of the per-column pass.

---

### Step 3 — `lib/executor.rs`

**`emit_batch`** — new signature:

```rust
fn emit_batch(
    batch:   RecordBatch,
    outputs: &[Output],
    format:  &Format,
    shared:  &mut HashMap<String, (Format, Vec<RecordBatch>)>,
)
```

Pushes `batch.clone()` to each slot in `outputs`. Update all six call sites to pass `&dataset.resolved_outputs()` (or the member's equivalent).

**`WriteSharedOutput` handling:**

```rust
ExecutionStep::WriteSharedOutput { output_file, format, quality, schema } => {
    let combined = union_and_shuffle(...).await?;
    let final_batch = match quality {
        Some(q) => apply_data_quality(combined, q, &schema)?,
        None    => combined,
    };
    write_output(&final_batch, &output_file, &format, output_dir)?;
}
```

`schema` here is the `Vec<Field>` for the dataset that owns this output file — needed so `apply_data_quality` can look up field-level quality and type information.

---

### Step 4 — `lib/plan.rs`

**`shared_outputs`:** change type from `Vec<(String, Format)>` to `Vec<(String, Format, Option<DataQuality>, Vec<Field>)>`.

**`track_shared`:** iterate `dataset.resolved_outputs()`:

```rust
fn track_shared(
    dataset: &SyntheticDataset,
    outputs: &mut Vec<(String, Format, Option<DataQuality>, Vec<Field>)>,
    seen:    &mut HashSet<String>,
)
```

For each `Output { file, quality }`, push `(file, dataset.format.clone(), quality.clone(), dataset.data.clone())` if `file` not already seen.

**`WriteSharedOutput` variant:** add `quality: Option<DataQuality>` and `schema: Vec<Field>`.

**Synthetic dataset construction:** update the five `SyntheticDataset { output_file: ... }` literal sites (lines ~226, ~533, ~966, ~1066, and the `expand_variant_dataset` helper) to use `output`/`outputs` instead.

---

### Step 5 — `lib/validate.rs`

Add a `validate_data_quality` call at the end of the existing `validate` function. Checks:

| Rule | Error |
|------|-------|
| All probability fields in `[0.0, 1.0]` | `"quality field '{name}' must be between 0.0 and 1.0"` |
| `duplication` / `missing` on a field-level block | `"quality.{field} is only valid on an output block, not on a field"` |
| `default_values` / `defaults_mode` on a dataset-level block | `"quality.{field} is only valid on a field, not on the output block"` |
| Field-level `quality` with no output-level quality block | `"field '{name}' has a quality stanza but the dataset output block has no quality stanza"` |
| Inapplicable corruption mode at field level (e.g. `noise` on `type: string`) | `"corruptions.noise is not applicable to string fields"` |
| `default_values` entries incompatible with field type | `"quality.default_values entry is incompatible with field type {type}"` |

---

### Step 6 — `lib/lib.rs`

Add `pub mod dq;`.

---

### Step 7 — `src/docgen.rs`

Add `TypeDoc` entries for `Output`, `DataQuality`, `Corruptions`, and `DefaultsMode` following the existing pattern.

---

### Step 8 — Docs

| File | Change |
|------|--------|
| `docs/src/content/docs/reference/yaml-schema.mdx` | New `## Output`, `## DataQuality`, `## Corruptions` sections; update `SyntheticDataset` table (`output_file` → `output` / `outputs`) |
| `docs/src/content/docs/concepts/execution-pipeline.mdx` | Brief note: DQ is applied inside `WriteSharedOutput` after union-and-shuffle; the clean batch in `computed` is never touched |

---

### Step 9 — Tests

| Location | Coverage |
|----------|----------|
| `lib/dq.rs` unit tests | Each transform in isolation: duplication row count, missing row count, null rate ≈ expected (large N), default replacement, each corruption mode fires and produces the expected change, noise magnitude within expected range, date shift within `[−max, +max]` |
| `tests/` integration test | Schema with `outputs:` (clean + dirty); run generation; verify clean file is unmodified, dirty file has correct row count and a sample of expected degradation |
| `lib/validate.rs` unit tests | One test per new error case |
