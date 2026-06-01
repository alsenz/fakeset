# IMPORT: incorporating pre-existing data into a synthetic schema

## Motivation

Sometimes an organisation wants to generate referentially-consistent synthetic data that incorporates pre-existing rows — real market tick symbols, a hand-crafted product catalogue, a partial legacy synthetic dataset that teams already use for demos. `import` solves this by letting a dataset declare that its rows come from an external file rather than being generated from scratch.

The core challenge is fitting imported rows into the concept semi-lattice. When an imported dataset is included by other datasets, each child must consume a non-overlapping slice of the imported file proportional to its Bernoulli-factored row count. The **hash ring** mechanism handles this.

---

## YAML syntax

`import:` is a new top-level field on `SyntheticDataset`, parallel to `include:` and `links:`:

```yaml
name: stocks
import:
  file: data/tickers.parquet    # relative to the schema root
  ref: tickers                  # namespace for expression refs within this dataset
  fields: ["symbol", "name", "exchange"]   # or ["*"] to include all columns
  # exclude: ["internal_id"]    # suppress specific columns (useful with "*")
  ring:                         # optional; see §Hash ring partitioning
    start: 0.0
    end: 1.0

data:
  fields:
    - name: market_cap
      type: number
      range: {min: 1000000, max: 1000000000}
    - name: display_name
      type: string
      expression: "CONCAT(tickers.symbol, ' (', tickers.exchange, ')')"
```

**Field reference:**

| Field | Required | Description |
|-------|----------|-------------|
| `file` | yes | Path to the imported file, relative to the schema root. Supported: Parquet, CSV, JSON array, JSONL. |
| `ref` | yes | Reference namespace. Imported columns are accessible in `expression:` fields as `<ref>.<column>`. |
| `fields` | no | List of column names to project in. `["*"]` includes all columns. Defaults to all columns when absent. |
| `exclude` | no | Column names to suppress after projection (most useful with `"*"`). |
| `ring` | no | Hash ring bounds `[start, end)` over `[0.0, 1.0)`. Restricts which rows of the file are used. When absent and the dataset has no lower cover, the full file is used; when absent and a lower cover exists, the planner assigns ring bounds automatically (see §Hash ring partitioning). |

**Mutual exclusion:** `rows:` is invalid when `import:` is present — row count is determined by the imported file (filtered to the ring bounds).

---

## Additional synthetic fields

`data.fields` may declare new columns **not** present in the imported schema. These are generated synthetically per imported row and appended to the output alongside the imported columns.

Within the same dataset, `expression:` fields may freely reference imported columns via the `ref` namespace (e.g. `tickers.symbol`).

**Naming rule:** a `data.fields` entry whose name collides with an imported column name is a validation error.

---

## Hash ring partitioning

### Mechanism

Row `i` of the imported file maps to a position `h(i) ∈ [0.0, 1.0)` via a deterministic hash of its row index. A dataset with ring bounds `[a, b)` receives exactly the rows where `h(i) ∈ [a, b)`.

Because the hash is positional (not value-based), this works regardless of file content — no constraint conflicts can arise. This is precisely why value-based specialisation of imported fields is banned in children-by-inclusion (see §Specialisation restrictions): the ring gives no guarantee that the rows in a child's slice satisfy any particular field constraint.

### Planner assignment

Ring bounds are assigned at the **segment level**, after Bernoulli factoring. Each segment — whether a pure `{A only}`, pure `{B only}`, or overlap `{A ∩ B}` segment — receives a contiguous ring slice proportional to its IPF-normalised row count. The slices tile the parent's ring range without gaps or overlaps.

```
parent ring: [0.0, 1.0)

Segment         rows   ring slice
──────────────────────────────────────
{A only}         30    [0.00, 0.30)
{A ∩ B}          20    [0.30, 0.50)
{B only}         50    [0.50, 1.00)
```

Both A and B read rows from the `{A ∩ B}` slice; there is no ambiguity because each segment is an already non-overlapping unit. A's ring is the union of its contributing slices (`[0.00, 0.30)` ∪ `[0.30, 0.50)` = `[0.00, 0.50)`), and B's is the union of its own (`[0.30, 0.50)` ∪ `[0.50, 1.00)` = `[0.30, 1.00)`).

Variant factoring integrates naturally: the planner tiles ring slices across the full `(variant × segment)` matrix using the same proportional assignment. Each sub-population — whether it is a variant slice, a lower cover segment, or both — ends up with its own contiguous ring range.

### File loading

The imported file is read **once** at execution time and hash-indexed in memory. All children (and any `links:` users) access the pre-computed index; no re-reads occur.

If a ring slice resolves to zero rows (e.g. because the file is smaller than the partition requires), that segment is treated as ⊥ and dropped, consistent with how `plan_segments` prunes zero-weight segments.

---

## Specialisation restrictions

Children-by-inclusion of an imported dataset **may not**:

1. Declare a `data.fields` entry with the same name as an imported column.
2. Use `ref:` pointing at an imported column, either directly or transitively through an `expression:` field that depends on one.

**What children can do:** freely declare and specialise any `data.fields` entry that is synthetic (neither an imported column nor derived from one). Those fields propagate through the include lattice exactly as normal.

**Implementation:** each `Field` needs a boolean flag — `imported_taint: bool` — set to `true` for directly imported columns and for any `expression:` field in the same dataset whose expression AST references an imported column. `validate` checks children-by-inclusion and rejects any field ref targeting a tainted field.

**Links are not subject to this restriction.** A `links:` entry pointing at an imported dataset, or a dataset using an imported dataset as a `links:` target, may project imported columns via `content.fields` `ref:` entries as normal — those are data projections, not constraint specialisations, and they do not affect the ring partition.

---

## Execution pipeline integration

### New schema-load step

Before `validate`, a new pass reads the **header only** (column names and Arrow types) of each imported file and merges the imported schema into the dataset's schema. This is sufficient for `resolve_refs`, `validate`, and `schema_to_arrow` to operate on imported columns without reading file bodies.

### New `ExecutionStep` variant

A new step — `LoadImportedDataset` (or an extension of `GenerateDataset`) — handles imported datasets:

1. Read and hash-index the imported file (first access; subsequent steps reuse the in-memory index).
2. Filter to rows within the step's ring bounds.
3. Append synthetic `data.fields` columns (generated per-row).
4. Evaluate `expression:` fields.
5. Emit the output file.

The resulting batch is stored in `computed` under the dataset's canonical path, so downstream witness steps and lower cover assembly work without modification.

### DAG ordering

External files have no DAG node. No new edge types are needed — the imported file is simply read at the step that needs it.

---

## Validation rules

| Rule | Error |
|------|-------|
| `rows:` present alongside `import:` | Validation error |
| `data.fields` name collides with an imported column | Validation error |
| Child-by-inclusion declares a field with the same name as an imported column | Validation error |
| Child-by-inclusion uses `ref:` to an imported or import-derived field | Validation error |
| Imported file not found | Hard error |
| Imported file format not one of Parquet / CSV / JSON array / JSONL | Validation error |
| `ring.start >= ring.end` | Validation error |
| `ring` values outside `[0.0, 1.0)` | Validation error |

---

## Reproducibility and seed

The hash `h(i)` must be deterministic and produce a near-uniform distribution over `[0.0, 1.0)`. The specific function needs to be pinned — result stability across runs depends on it.

The ring seed is random by default and overridable via `--seed.ring <value>` on the CLI. The `seed.*` namespace leaves room for a parallel `--seed.generator` (or similar) to be added later without a breaking flag rename.

---

## Implementation plan

### Phase 1 — Data model (`lib/models.rs`)

Add two new structs and wire them onto `SyntheticDataset` and `Field`.

**New `RingBounds`:**
```rust
#[derive(Debug, Clone, Deserialize)]
pub struct RingBounds {
    pub start: f64,
    pub end: f64,
}
```

**New `ImportSpec`:**
```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ImportSpec {
    pub file: String,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub fields: Vec<String>,
    pub exclude: Option<Vec<String>>,
    pub ring: Option<RingBounds>,
}
```

**`SyntheticDataset`:** add `pub import: Option<ImportSpec>` alongside the existing `include` and `links` fields.

**`Field`:** add `#[serde(skip, default)] pub imported_taint: bool`. The `#[serde(skip)]` ensures it is never written into or read from YAML; it is set programmatically by Phase 2. `Field` already derives/uses `Default` (see `expressions.rs:57`), so no `Default` impl changes are needed.

**`Segment` (`lib/segment.rs`):** add `pub ring: Option<RingBounds>` for the per-segment ring slice assigned at plan time (Phase 4).

---

### Phase 2 — Import header loading (`lib/import.rs`, new module)

New public function, called immediately after `load_all_datasets`:

```rust
pub fn load_import_headers(
    datasets: &mut HashMap<PathBuf, SyntheticDataset>,
) -> Result<()>
```

For each dataset where `import` is `Some(spec)`:

1. Resolve `spec.file` relative to the dataset's own directory (same resolution as `resolve_include`).
2. Detect format from extension (`parquet` / `csv` / `json` / `jsonl`). Error if unrecognised.
3. Read only the **schema** (column names + Arrow types):
   - Parquet: `ParquetRecordBatchReaderBuilder::try_new(file)?.schema()`.
   - CSV: read the header row and infer types from the first data row.
   - JSON / JSONL: read the first object and infer types.
4. Apply `fields` / `exclude` projection to produce the visible column list.
5. For each visible column, construct a `Field { name, field_type: <inferred from Arrow type>, imported_taint: true, ..Default::default() }`.
6. **Prepend** these fields to `dataset.data`. Prepending keeps imported columns before synthetic ones in schema order, matching the YAML model (imported columns are the "base" row).
7. Validate that none of the existing `dataset.data` entries collide with an imported column name — bail if so (see validation rule: "naming rule" in the spec).

Also export a helper:
```rust
pub fn imported_column_names(dataset: &SyntheticDataset) -> HashSet<String>
```
Returns the set of column names where `field.imported_taint == true`. Used by validation and the planner.

Register `pub mod import;` in `lib/lib.rs`.

---

### Phase 3 — Pipeline wiring (`src/main.rs`, `lib/lib.rs`)

**`src/main.rs`:** call `load_import_headers` right after `load_all_datasets`, before `build_dag`:

```rust
let mut datasets = load_all_datasets(&cli.paths)?;
fakeset::import::load_import_headers(&mut datasets)?;  // ← new
let dag = build_dag(&datasets)?;
// ...
```

**`pull_down_expression_deps` (`lib/expressions.rs`):** `include_refs_containing` currently returns every field name found in an included dataset. It must be updated to **skip tainted fields** (`!f.imported_taint`). Without this, expression deps in a parent that reference imported columns would be incorrectly injected as hidden ref fields into children-by-inclusion.

**`expand_include_fields` (`lib/rewrite.rs`):** when expanding `include.fields: ["*"]` wildcard copies, **skip fields where `imported_taint == true`**. Imported columns should never be silently propagated to children through wildcard expansion; users who explicitly list a tainted column name in `include.fields` are caught by the validation in Phase 4.

---

### Phase 4 — Validation (`lib/validate.rs`)

Add import-specific checks inside `validate_dataset`, and a new helper called once from `validate` to check all child-by-inclusion relationships.

**Per-dataset checks (in `validate_dataset`):**

1. `rows:` + `import:` mutual exclusion — already present as a field after Phase 1; add the check alongside Rule 1.
2. `ring` bounds sanity: `ring.start >= ring.end`, or either value outside `[0.0, 1.0)`.
3. `variants:` + `import:` compatibility — permitted (handled by Phase 5), but note it for the planner so ring assignment covers the `(variant × segment)` matrix.

**Cross-dataset taint check (new helper `check_import_taint`):**

Called from `validate` after the per-dataset loop:

```rust
fn check_import_taint(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
) -> Result<()>
```

Algorithm:
1. For each dataset `P` with `import: Some(spec)`:
   a. Compute the **taint closure** of `P`:
      - Seed set: all field names where `field.imported_taint == true`.
      - Expand: any `expression:` field in `P.data` whose `extract_identifiers(expression)` intersects the seed set is also tainted.
      - Repeat until stable (one pass suffices because expressions can only reference earlier fields).
   b. For each dataset `C` that directly includes `P` (i.e. `resolve_include(c_path, include.file) == P_path`):
      - For each field in `C.data`:
        - If the field name is in the taint closure → bail (same-name declaration of imported column).
        - If the field has a `ref:` of the form `<include.reference>.<col>` where `col` is in the taint closure → bail.
      - If `C.include.fields` contains any name from the taint closure → bail (explicit field copy of an imported column).

The taint closure computation reuses `extract_identifiers` from `expressions.rs`.

---

### Phase 5 — Ring assignment (`lib/segment.rs`, `lib/plan.rs`)

**New function in `lib/segment.rs`:**

```rust
pub fn assign_ring_slices(
    segments: &mut [Segment],
    parent_ring: &RingBounds,
)
```

Tiles `parent_ring` across `segments` proportionally to `segment.rows`, writing `segment.ring = Some(RingBounds { start, end })` for each. Segments with `rows == 0` get a zero-width slice (which the executor skips as ⊥).

**`lib/plan.rs` — `build_plan` changes:**

When planning a dataset with `import: Some(_)`:

1. After `plan_segments` returns the segment list, call `assign_ring_slices(segments, parent_ring)` where `parent_ring` is the dataset's `import.ring` (defaulting to `RingBounds { start: 0.0, end: 1.0 }` if absent).
2. Store the ring-annotated segments in the `ExecutionStep` as normal — the ring is on the `Segment` itself, so no step signature changes.
3. For a no-lower-cover imported dataset (`segments` is a single trivial segment), `assign_ring_slices` sets that one segment's ring to the full `parent_ring`.

**`expand_variant_dataset` (`lib/plan.rs`):** update to copy `import` from the base dataset with the ring narrowed to the variant's assigned slice. Each variant gets a `RingBounds` covering its proportional share of the parent's ring, computed by `assign_ring_slices` before variant expansion. The per-variant `SyntheticDataset` carries `import: Some(ImportSpec { ring: Some(variant_ring), .. })` so the executor has the correct bounds without any extra plumbing.

**Row count for imported datasets:** `build_plan` currently resolves row counts via `dataset.rows` or ratio-derived counts. For imported datasets, the row count is determined at **execution time** after the ring filter. At plan time, use the file's total row count (read from the header in Phase 2 — store this on `ImportSpec` as `total_rows: usize` added during `load_import_headers`) multiplied by the ring fraction as a planning estimate.

---

### Phase 6 — Execution (`lib/executor.rs`)

The executor already processes `ExecutionStep` variants by matching on their enum arm. Rather than introducing new step variants, extend the existing `GenerateDataset`, `GenerateStagingNode`, `GenerateLowerCoverGroup`, and `GenerateStagingLowerCoverGroup` handlers to branch on `dataset.import.is_some()`. The ring bounds for the step are read from `segment.ring` (lower cover groups) or `dataset.import.ring` (simple datasets).

**New shared helper `lib/import.rs` (extending Phase 2):**

```rust
pub struct ImportIndex {
    /// All rows from the file, in file order.
    pub batch: RecordBatch,
    /// Per-row hash value h(i) ∈ [0.0, 1.0), computed once with the ring seed.
    pub hashes: Vec<f64>,
}

pub fn load_import_index(
    spec: &ImportSpec,
    dataset_dir: &Path,
    seed: u64,
) -> Result<ImportIndex>
```

`h(i)` is computed as `hash64(seed, i as u64) as f64 / u64::MAX as f64` using a fast non-cryptographic hash (e.g. [`rustc-hash`](https://crates.io/crates/rustc-hash) or a simple multiply-shift). The hash function and seed are the only inputs, making results fully reproducible given the same seed.

```rust
pub fn filter_ring(index: &ImportIndex, ring: &RingBounds) -> RecordBatch
```

Returns the subset of `index.batch` whose rows satisfy `h(i) ∈ [ring.start, ring.end)`.

**Cache:** `execute` receives a `HashMap<PathBuf, Arc<ImportIndex>>` cache (initially empty). Each imported file is loaded once on first access; subsequent steps retrieve the `Arc`.

**Execution path for imported datasets (within existing step handlers):**

```
if dataset.import.is_some():
    batch ← filter_ring(import_index, ring)
    // batch now contains only the imported columns
else:
    batch ← generate all data.fields columns as normal
// common path for both:
for each non-tainted data.field not yet in batch:
    batch ← append generated column
evaluate expression fields
filter hidden columns
emit / store in computed
```

**Zero-row ring slices:** if `filter_ring` returns an empty batch, skip the step (treat as ⊥), consistent with how `plan_segments` skips zero-row segments.

---

### Phase 7 — CLI seed flag (`src/main.rs`)

Add to the `Cli` struct:

```rust
/// Seed for the import hash ring. Random by default; set for reproducible import partitions.
#[arg(long = "seed.ring", value_name = "SEED")]
seed_ring: Option<u64>,
```

Resolve to a concrete `u64` early in `main`:

```rust
let ring_seed: u64 = cli.seed_ring.unwrap_or_else(rand::random);
```

Thread `ring_seed` through `build_plan` (for planning-time row-count estimates) and `execute` (for `load_import_index` calls).

---

### Phase 8 — Tests

**Unit tests (inline, `#[cfg(test)]`):**

| Location | Test |
|----------|------|
| `lib/import.rs` | `hash_row_index` is deterministic: same seed+index → same value |
| `lib/import.rs` | hash values are roughly uniform: χ² test over 10,000 indices vs. 10 buckets |
| `lib/import.rs` | `filter_ring` returns correct subset for a known ring |
| `lib/segment.rs` | `assign_ring_slices` tiles without gaps or overlaps; widths proportional to rows |
| `lib/segment.rs` | `assign_ring_slices` with a single segment → full parent ring |
| `lib/validate.rs` | `rows:` + `import:` → error |
| `lib/validate.rs` | child ref to imported column → error |
| `lib/validate.rs` | child ref to expression field derived from imported column → error |
| `lib/validate.rs` | child ref to synthetic `data.fields` column → ok |

**Integration tests (`tests/`):**

- Simple imported dataset (no lower cover, no variants): output row count equals file rows filtered to ring; output contains both imported and synthetic columns.
- Imported dataset with two lower cover members: union of children's rows = parent rows; the two children's row sets are disjoint (verified by checking hash values).
- Imported dataset used as a `links:` target: witness/assembly pipeline produces correct list columns from imported rows.
- Imported dataset with `variants:`: each variant gets a disjoint ring slice; union covers the parent ring.

**Statistical tests (`tests/statistical/`):**

Add a small CSV fixture (`tests/fixtures/tickers.csv`) with, say, 1,000 rows. A new `test_import.py` builds a schema with one imported dataset and two children, runs the binary, and checks:
- Total rows across children ≈ file size × ring fraction (binomial test).
- No row appears in both children's output (referential integrity check on a known unique column).

---

### Phase 9 — Documentation (`src/docgen.rs`, `docs/`)

- `src/docgen.rs`: add `ImportSpec` and `RingBounds` to the `TypeDoc` list; add `import:` to the `SyntheticDataset` field table.
- `docs/src/content/docs/reference/yaml-schema.mdx`: new `## import` section with field table and example.
- `docs/src/content/docs/reference/cli.mdx`: document `--seed.ring`.
- `docs/src/content/docs/concepts/execution-pipeline.mdx`: note the new `load_import_headers` stage and the executor's import branch.
