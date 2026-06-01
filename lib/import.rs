//! Import header loading and runtime data loading.
//!
//! `load_import_headers` runs at schema-load time: it reads only the schema and row count
//! of each imported file and merges tainted columns into the dataset's `data`.
//!
//! `load_import_index` runs at execution time: it loads the full file into memory and
//! pre-computes per-row hash values for ring-based partitioning.
//!
//! `filter_ring` applies a `RingBounds` mask to an `ImportIndex` and returns a
//! `RecordBatch` containing only the matching rows.
use anyhow::{Result, anyhow, bail};
use arrow::array::{ArrayRef, BooleanArray};
use arrow::compute::{concat_batches, filter as arrow_filter};
use arrow::datatypes::{DataType, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::models::{Field, FieldType, Format, ImportSpec, RingBounds, SyntheticDataset};

// ---------------------------------------------------------------------------
// Runtime import index
// ---------------------------------------------------------------------------

/// A fully-loaded import file with pre-computed per-row hash values.
///
/// Constructed once per file per run by `load_import_index`; shared via `Arc`
/// across all execution steps that reference the same import file.
pub struct ImportIndex {
    /// All rows from the file (after column projection), in file order.
    pub batch: RecordBatch,
    /// Per-row hash value `h(i) ∈ [0.0, 1.0)`, computed with the ring seed.
    pub hashes: Vec<f64>,
}

/// Load a full import file into memory and pre-compute per-row hash values.
///
/// `dataset_path` is the canonical path of the dataset YAML file; the import
/// `spec.file` is resolved relative to its parent directory.
pub fn load_import_index(
    spec: &ImportSpec,
    dataset_path: &Path,
    ring_seed: u64,
) -> Result<Arc<ImportIndex>> {
    let import_path = resolve_import_path(dataset_path, &spec.file)?;
    let format = detect_format(&import_path)?;
    let batch = load_full_batch(&import_path, format, spec)?;
    let n = batch.num_rows();
    let hashes: Vec<f64> = (0..n).map(|i| hash_row(ring_seed, i)).collect();
    Ok(Arc::new(ImportIndex { batch, hashes }))
}

/// Filter an `ImportIndex` to rows whose hash falls in `[ring.start, ring.end)`.
pub fn filter_ring(index: &ImportIndex, ring: &RingBounds) -> Result<RecordBatch> {
    let mask: BooleanArray = index
        .hashes
        .iter()
        .map(|&h| Some(h >= ring.start && h < ring.end))
        .collect();
    let filtered: Vec<ArrayRef> = index
        .batch
        .columns()
        .iter()
        .map(|col| arrow_filter(col.as_ref(), &mask).map_err(|e| anyhow!("ring filter: {e}")))
        .collect::<Result<_>>()?;
    RecordBatch::try_new(index.batch.schema(), filtered)
        .map_err(|e| anyhow!("ring filter RecordBatch: {e}"))
}

/// Deterministic per-row hash using splitmix64.
/// Maps `(seed, row_index)` → `[0.0, 1.0)` with near-uniform distribution.
fn hash_row(seed: u64, row_index: usize) -> f64 {
    let mut z = seed ^ (row_index as u64).wrapping_mul(0x9e3779b97f4a7c15);
    z = z.wrapping_add(0x9e3779b97f4a7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^= z >> 31;
    // Map to [0.0, 1.0) using 53 bits of mantissa precision.
    (z >> 11) as f64 * (1.0f64 / (1u64 << 53) as f64)
}

// ---------------------------------------------------------------------------
// Full-file data loaders (called at execution time by load_import_index)
// ---------------------------------------------------------------------------

fn load_full_batch(path: &Path, format: Format, spec: &ImportSpec) -> Result<RecordBatch> {
    let raw = match format {
        Format::Parquet => load_parquet_full(path)?,
        Format::Csv => load_csv_full(path)?,
        Format::Json => load_json_full(path)?,
        Format::Jsonl => load_jsonl_full(path)?,
    };
    project_batch(raw, spec)
}

fn load_parquet_full(path: &Path) -> Result<RecordBatch> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file = File::open(path).map_err(|e| anyhow!("open '{}': {e}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| anyhow!("Parquet open '{}': {e}", path.display()))?;
    let schema = builder.schema().clone();
    let reader = builder
        .build()
        .map_err(|e| anyhow!("Parquet build '{}': {e}", path.display()))?;
    let batches: Vec<RecordBatch> = reader
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("Parquet read '{}': {e}", path.display()))?;
    if batches.is_empty() {
        return Ok(RecordBatch::new_empty(schema));
    }
    concat_batches(&schema, &batches).map_err(|e| anyhow!("Parquet concat: {e}"))
}

fn load_csv_full(path: &Path) -> Result<RecordBatch> {
    use arrow::csv::reader::Format as CsvFormat;
    let (schema_inner, _) = CsvFormat::default()
        .with_header(true)
        .infer_schema(&mut File::open(path)?, Some(200))
        .map_err(|e| anyhow!("CSV schema '{}': {e}", path.display()))?;
    let schema = Arc::new(schema_inner);
    let reader = arrow::csv::ReaderBuilder::new(Arc::clone(&schema))
        .with_header(true)
        .build(File::open(path)?)
        .map_err(|e| anyhow!("CSV build '{}': {e}", path.display()))?;
    let batches: Vec<RecordBatch> = reader
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("CSV read '{}': {e}", path.display()))?;
    if batches.is_empty() {
        return Ok(RecordBatch::new_empty(schema));
    }
    concat_batches(&schema, &batches).map_err(|e| anyhow!("CSV concat: {e}"))
}

fn load_json_full(path: &Path) -> Result<RecordBatch> {
    let content =
        std::fs::read_to_string(path).map_err(|e| anyhow!("JSON read '{}': {e}", path.display()))?;
    let arr: Vec<serde_json::Value> = serde_json::from_str(&content)
        .map_err(|e| anyhow!("JSON parse '{}': {e}", path.display()))?;
    if arr.is_empty() {
        bail!("JSON import file '{}' is empty", path.display());
    }
    // Serialise array elements as JSONL so the arrow JSON reader can consume them.
    let jsonl: Vec<u8> = arr
        .iter()
        .flat_map(|v| {
            let mut b = serde_json::to_vec(v).unwrap_or_default();
            b.push(b'\n');
            b
        })
        .collect();
    load_jsonl_bytes(&jsonl, path)
}

fn load_jsonl_full(path: &Path) -> Result<RecordBatch> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("JSONL read '{}': {e}", path.display()))?;
    load_jsonl_bytes(content.as_bytes(), path)
}

fn load_jsonl_bytes(bytes: &[u8], path: &Path) -> Result<RecordBatch> {
    use arrow::json::reader::ReaderBuilder as JsonReaderBuilder;
    let (schema, _) = arrow::json::reader::infer_json_schema(std::io::Cursor::new(bytes), None)
        .map_err(|e| anyhow!("JSONL schema '{}': {e}", path.display()))?;
    let schema_ref = Arc::new(schema);
    let reader = JsonReaderBuilder::new(Arc::clone(&schema_ref))
        .build(std::io::Cursor::new(bytes))
        .map_err(|e| anyhow!("JSONL build '{}': {e}", path.display()))?;
    let batches: Vec<RecordBatch> = reader
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("JSONL read '{}': {e}", path.display()))?;
    if batches.is_empty() {
        return Ok(RecordBatch::new_empty(schema_ref));
    }
    let schema_ref = batches[0].schema();
    concat_batches(&schema_ref, &batches).map_err(|e| anyhow!("JSONL concat: {e}"))
}

/// Project a `RecordBatch` to the columns specified by `spec.fields` / `spec.exclude`.
fn project_batch(batch: RecordBatch, spec: &ImportSpec) -> Result<RecordBatch> {
    let all = spec.fields.is_empty() || (spec.fields.len() == 1 && spec.fields[0] == "*");
    let exclude: HashSet<&str> = spec
        .exclude
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| s.as_str())
        .collect();
    let include: HashSet<&str> = if all {
        HashSet::new()
    } else {
        spec.fields.iter().map(|s| s.as_str()).collect()
    };

    let schema = batch.schema();
    let keep: Vec<usize> = schema
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            let n = f.name().as_str();
            !exclude.contains(n) && (all || include.contains(n))
        })
        .map(|(i, _)| i)
        .collect();

    if keep.len() == schema.fields().len() {
        return Ok(batch);
    }
    let new_schema = Arc::new(
        schema
            .project(&keep)
            .map_err(|e| anyhow!("column projection: {e}"))?,
    );
    let new_cols: Vec<ArrayRef> = keep.iter().map(|&i| batch.column(i).clone()).collect();
    RecordBatch::try_new(new_schema, new_cols).map_err(|e| anyhow!("projected batch: {e}"))
}

/// Read import file headers for all datasets that carry an `import:` stanza.
///
/// For each such dataset:
/// 1. Resolves and validates the import file path.
/// 2. Reads the file schema (column names + Arrow types) without loading data rows.
/// 3. Records `total_rows` on `ImportSpec` for planning estimates.
/// 4. Validates that no `data.fields` entry collides with an imported column name.
/// 5. Prepends the imported columns (as `imported_taint = true` fields) to `dataset.data`.
pub fn load_import_headers(
    datasets: &mut HashMap<PathBuf, SyntheticDataset>,
) -> Result<()> {
    let paths: Vec<PathBuf> = datasets.keys().cloned().collect();
    for path in &paths {
        // Clone the spec to release the immutable borrow before we mutate the dataset.
        let Some(spec) = datasets[path].import.clone() else {
            continue;
        };

        let import_path = resolve_import_path(path, &spec.file)?;
        let format = detect_format(&import_path)?;
        let (arrow_schema, total_rows) = read_schema_and_count(&import_path, format)?;
        let imported_fields =
            project_columns(&arrow_schema, &spec.fields, spec.exclude.as_deref())?;

        let dataset = datasets.get_mut(path).unwrap();

        // Validate no name collisions between imported columns and declared synthetic fields.
        for imported in &imported_fields {
            if dataset.data.iter().any(|f| f.name == imported.name) {
                bail!(
                    "dataset '{}': `data.fields` entry '{}' collides with imported column name; \
                     rename the synthetic field or exclude the imported column",
                    dataset.name,
                    imported.name
                );
            }
        }

        // Record total_rows for the planner.
        if let Some(ref mut s) = dataset.import {
            s.total_rows = total_rows;
        }

        // Prepend imported fields before synthetic fields so Arrow schema order matches
        // the conceptual model: imported columns are the "base" row.
        let mut new_data = imported_fields;
        new_data.extend(std::mem::take(&mut dataset.data));
        dataset.data = new_data;
    }
    Ok(())
}

/// Returns the set of column names that are import-tainted in `dataset`.
/// A field is tainted if it was injected by `load_import_headers` or if it is an
/// `expression:` field whose expression AST transitively references a tainted column.
/// (The expression-derived closure is computed separately by the validator.)
pub fn imported_column_names(dataset: &SyntheticDataset) -> std::collections::HashSet<String> {
    dataset
        .data
        .iter()
        .filter(|f| f.imported_taint)
        .map(|f| f.name.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

pub(crate) fn resolve_import_path(dataset_path: &Path, file: &str) -> Result<PathBuf> {
    dataset_path
        .parent()
        .unwrap_or(Path::new(""))
        .join(file)
        .canonicalize()
        .map_err(|e| anyhow!("import file '{}' not found (from '{}'): {e}", file, dataset_path.display()))
}

fn detect_format(path: &Path) -> Result<Format> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("parquet") => Ok(Format::Parquet),
        Some("csv") => Ok(Format::Csv),
        Some("json") => Ok(Format::Json),
        Some("jsonl") | Some("ndjson") => Ok(Format::Jsonl),
        ext => bail!(
            "unsupported import file format '{}' ({}); expected parquet, csv, json, or jsonl",
            ext.unwrap_or("(no extension)"),
            path.display()
        ),
    }
}

fn read_schema_and_count(path: &Path, format: Format) -> Result<(ArrowSchema, usize)> {
    match format {
        Format::Parquet => read_parquet_schema(path),
        Format::Csv => read_csv_schema(path),
        Format::Json => read_json_schema(path),
        Format::Jsonl => read_jsonl_schema(path),
    }
}

fn read_parquet_schema(path: &Path) -> Result<(ArrowSchema, usize)> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file = File::open(path)
        .map_err(|e| anyhow!("could not open import file '{}': {e}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| anyhow!("could not read Parquet file '{}': {e}", path.display()))?;
    let schema = builder.schema().as_ref().clone();
    let total_rows = builder
        .metadata()
        .file_metadata()
        .num_rows()
        .try_into()
        .unwrap_or(0);
    Ok((schema, total_rows))
}

fn read_csv_schema(path: &Path) -> Result<(ArrowSchema, usize)> {
    use arrow::csv::reader::Format;
    let mut file = File::open(path)
        .map_err(|e| anyhow!("could not open import file '{}': {e}", path.display()))?;
    let format = Format::default().with_header(true);
    let (schema, _) = format
        .infer_schema(&mut file, Some(200))
        .map_err(|e| anyhow!("CSV schema inference failed for '{}': {e}", path.display()))?;

    // Count data rows (seek back, skip header line).
    file.seek(SeekFrom::Start(0))?;
    let total_rows = BufReader::new(&mut file)
        .lines()
        .skip(1)
        .filter(|l| l.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false))
        .count();

    Ok((schema.as_ref().clone(), total_rows))
}

fn read_json_schema(path: &Path) -> Result<(ArrowSchema, usize)> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("could not read import file '{}': {e}", path.display()))?;
    let arr: Vec<serde_json::Value> = serde_json::from_str(&content)
        .map_err(|e| anyhow!("could not parse JSON import file '{}': {e}", path.display()))?;
    let total_rows = arr.len();
    let first = arr
        .first()
        .ok_or_else(|| anyhow!("JSON import file '{}' is empty", path.display()))?;
    let schema = infer_schema_from_json_object(first, path)?;
    Ok((schema, total_rows))
}

fn read_jsonl_schema(path: &Path) -> Result<(ArrowSchema, usize)> {
    let file = File::open(path)
        .map_err(|e| anyhow!("could not open import file '{}': {e}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;
    let trimmed = first_line.trim();
    if trimmed.is_empty() {
        bail!("JSONL import file '{}' is empty", path.display());
    }
    let first: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| anyhow!("could not parse first line of JSONL '{}': {e}", path.display()))?;

    // Count remaining non-empty lines.
    let rest = reader
        .lines()
        .filter(|l| l.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false))
        .count();
    let total_rows = 1 + rest;

    let schema = infer_schema_from_json_object(&first, path)?;
    Ok((schema, total_rows))
}

/// Build an Arrow schema from the keys of a JSON object, inferring types from values.
fn infer_schema_from_json_object(value: &serde_json::Value, path: &Path) -> Result<ArrowSchema> {
    let obj = value.as_object().ok_or_else(|| {
        anyhow!(
            "import file '{}': expected a JSON object at the top level of each record",
            path.display()
        )
    })?;
    let fields: Vec<arrow::datatypes::Field> = obj
        .iter()
        .map(|(k, v)| {
            let dt = json_value_to_arrow_type(v);
            arrow::datatypes::Field::new(k, dt, true)
        })
        .collect();
    Ok(ArrowSchema::new(fields))
}

fn json_value_to_arrow_type(v: &serde_json::Value) -> DataType {
    match v {
        serde_json::Value::Bool(_) => DataType::Boolean,
        serde_json::Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                DataType::Int64
            } else {
                DataType::Float64
            }
        }
        serde_json::Value::Object(obj) => {
            let sub: Vec<arrow::datatypes::Field> = obj
                .iter()
                .map(|(k, v)| arrow::datatypes::Field::new(k, json_value_to_arrow_type(v), true))
                .collect();
            DataType::Struct(sub.into())
        }
        serde_json::Value::Array(arr) => {
            let item_dt = arr
                .first()
                .map(json_value_to_arrow_type)
                .unwrap_or(DataType::Utf8);
            DataType::List(std::sync::Arc::new(
                arrow::datatypes::Field::new("item", item_dt, true),
            ))
        }
        _ => DataType::Utf8,
    }
}

/// Project an Arrow schema to only the columns specified by `fields` / `exclude`,
/// and convert each Arrow field to a tainted `crate::models::Field`.
fn project_columns(
    schema: &ArrowSchema,
    fields: &[String],
    exclude: Option<&[String]>,
) -> Result<Vec<Field>> {
    let exclude_set: std::collections::HashSet<&str> = exclude
        .unwrap_or(&[])
        .iter()
        .map(|s| s.as_str())
        .collect();

    let all = fields.is_empty() || (fields.len() == 1 && fields[0] == "*");
    let include_set: std::collections::HashSet<&str> = if all {
        std::collections::HashSet::new()
    } else {
        fields.iter().map(|s| s.as_str()).collect()
    };

    let mut result = Vec::new();
    for arrow_field in schema.fields() {
        let name = arrow_field.name().as_str();
        if exclude_set.contains(name) {
            continue;
        }
        if !all && !include_set.contains(name) {
            continue;
        }
        result.push(Field {
            name: name.to_string(),
            field_type: Some(arrow_datatype_to_field_type(arrow_field.data_type())),
            imported_taint: true,
            ..Default::default()
        });
    }

    // If the user listed explicit columns, warn about any that weren't found.
    if !all {
        for requested in fields {
            if requested != "*"
                && !exclude_set.contains(requested.as_str())
                && !result.iter().any(|f| &f.name == requested)
            {
                // Return an error rather than silently ignoring a mistyped column name.
                bail!(
                    "import column '{}' not found in file schema (available: {})",
                    requested,
                    schema
                        .fields()
                        .iter()
                        .map(|f| f.name().as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }

    Ok(result)
}

fn arrow_datatype_to_field_type(dt: &DataType) -> FieldType {
    match dt {
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Float16
        | DataType::Float32
        | DataType::Float64 => FieldType::Number,
        DataType::Boolean => FieldType::Boolean,
        DataType::Date32 | DataType::Date64 => FieldType::Date,
        DataType::Timestamp(_, _) => FieldType::DateTime,
        DataType::Struct(_) => FieldType::Object,
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(_, _) => {
            FieldType::List
        }
        // Utf8, LargeUtf8, Binary, and any other type → String
        _ => FieldType::String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, StringArray};
    use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};

    // ── hash_row ─────────────────────────────────────────────────────────────

    #[test]
    fn hash_row_is_deterministic() {
        // Same (seed, index) must always produce the same value.
        let h = hash_row(0, 0);
        assert_eq!(h, hash_row(0, 0));
        assert_eq!(hash_row(42, 100), hash_row(42, 100));
        assert_eq!(hash_row(u64::MAX, 9999), hash_row(u64::MAX, 9999));
    }

    #[test]
    fn hash_row_is_in_unit_interval() {
        for i in 0..1000_usize {
            let h = hash_row(12345, i);
            assert!(h >= 0.0 && h < 1.0, "hash_row({i}) = {h} outside [0, 1)");
        }
    }

    #[test]
    fn hash_row_different_seeds_produce_different_values() {
        // Two different seeds should almost never produce the same hash for the same index.
        let h1 = hash_row(1, 0);
        let h2 = hash_row(2, 0);
        assert_ne!(h1, h2, "different seeds should produce different hashes");
    }

    #[test]
    fn hash_row_different_indices_produce_different_values() {
        let h0 = hash_row(42, 0);
        let h1 = hash_row(42, 1);
        assert_ne!(h0, h1, "consecutive indices should produce different hashes");
    }

    #[test]
    fn hash_row_is_near_uniform() {
        // χ² bucket test: 10,000 rows, 10 equal-width buckets over [0, 1).
        // A perfect uniform distribution gives 1,000 per bucket.
        // We pass if every bucket has between 800 and 1,200 (80–120% of expected).
        const N: usize = 10_000;
        const BUCKETS: usize = 10;
        let mut counts = [0usize; BUCKETS];
        for i in 0..N {
            let h = hash_row(99, i);
            let bucket = (h * BUCKETS as f64) as usize;
            counts[bucket.min(BUCKETS - 1)] += 1;
        }
        for (b, &c) in counts.iter().enumerate() {
            assert!(
                c >= 800 && c <= 1200,
                "bucket {b} has {c} entries — too far from the expected 1000 (seed 99)"
            );
        }
    }

    // ── filter_ring ───────────────────────────────────────────────────────────

    fn make_index(symbols: &[&str], hashes: Vec<f64>) -> ImportIndex {
        assert_eq!(symbols.len(), hashes.len());
        let arr: StringArray = symbols.iter().map(|&s| Some(s)).collect();
        let schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "symbol",
            DataType::Utf8,
            true,
        )]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(arr) as ArrayRef]).expect("batch");
        ImportIndex { batch, hashes }
    }

    #[test]
    fn filter_ring_returns_matching_rows() {
        // Three rows with known hashes.
        let index = make_index(
            &["AAPL", "MSFT", "GOOG"],
            vec![0.1, 0.5, 0.9],
        );
        let ring = RingBounds { start: 0.4, end: 0.8 };
        let result = filter_ring(&index, &ring).expect("filter_ring");

        assert_eq!(result.num_rows(), 1, "only MSFT (hash=0.5) falls in [0.4, 0.8)");
        let col = result
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("string array");
        assert_eq!(col.value(0), "MSFT");
    }

    #[test]
    fn filter_ring_excludes_exact_end_boundary() {
        // Hash at exactly end=0.5 must be excluded (half-open [start, end)).
        let index = make_index(&["A", "B"], vec![0.3, 0.5]);
        let ring = RingBounds { start: 0.0, end: 0.5 };
        let result = filter_ring(&index, &ring).expect("filter_ring");
        assert_eq!(result.num_rows(), 1, "only A (hash=0.3) falls in [0.0, 0.5)");
    }

    #[test]
    fn filter_ring_full_range_returns_all_rows() {
        let index = make_index(&["A", "B", "C"], vec![0.1, 0.5, 0.9]);
        let ring = RingBounds { start: 0.0, end: 1.0 };
        let result = filter_ring(&index, &ring).expect("filter_ring");
        assert_eq!(result.num_rows(), 3, "full [0.0, 1.0) should include all rows");
    }

    #[test]
    fn filter_ring_empty_range_returns_no_rows() {
        let index = make_index(&["A", "B"], vec![0.2, 0.8]);
        // [0.4, 0.6) contains neither 0.2 nor 0.8.
        let ring = RingBounds { start: 0.4, end: 0.6 };
        let result = filter_ring(&index, &ring).expect("filter_ring");
        assert_eq!(result.num_rows(), 0);
    }

    #[test]
    fn filter_ring_preserves_column_values() {
        // Two columns — verify both are projected correctly after filtering.
        let symbols: StringArray = ["X", "Y", "Z"].iter().map(|&s| Some(s)).collect();
        let scores: Float64Array = [1.0f64, 2.0, 3.0].into_iter().map(Some).collect();
        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("symbol", DataType::Utf8, true),
            ArrowField::new("score", DataType::Float64, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(symbols) as ArrayRef,
                Arc::new(scores) as ArrayRef,
            ],
        )
        .expect("batch");
        let index = ImportIndex {
            batch,
            hashes: vec![0.1, 0.5, 0.9],
        };
        // Keep only X (0.1) and Z (0.9), skipping Y (0.5 is outside [0.7, 1.0) too).
        let ring = RingBounds { start: 0.0, end: 0.3 };
        let result = filter_ring(&index, &ring).expect("filter_ring");
        assert_eq!(result.num_rows(), 1);
        let sym = result
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let sc = result
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(sym.value(0), "X");
        assert!((sc.value(0) - 1.0).abs() < 1e-9);
    }
}
