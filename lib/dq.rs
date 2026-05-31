//! Data quality post-processing pass.
//!
//! `apply_data_quality` is called inside `WriteSharedOutput` after `union_and_shuffle`
//! and before `write_output`. It never touches the clean batch stored in `computed`.
//!
//! ## Order of operations
//!
//! 1. Duplication — append duplicate rows.
//! 2. Missing    — delete rows (Bernoulli keep-mask).
//! 3. Per-column: nulls → defaults → corruptions.
use anyhow::{Context, Result};
use arrow::array::{
    Array, ArrayRef, BooleanArray, Date32Array, Float64Array, StringArray,
    TimestampMicrosecondArray, TimestampMillisecondArray, UInt32Array, new_null_array,
};
use arrow::compute::kernels::zip::zip;
use arrow::compute::{cast, concat_batches, filter};
use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;
use fake::Fake;
use std::f64::consts::PI;
use std::sync::Arc;

use crate::models::{Corruptions, DataQuality, DefaultsMode, Field, FieldType};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Apply the `quality` stanza to `batch`, returning a new (potentially larger or
/// smaller) `RecordBatch`. `schema` is the originating dataset's `data` field list,
/// used to look up per-field quality overrides and type information.
pub fn apply_data_quality(
    batch: RecordBatch,
    quality: &DataQuality,
    schema: &[Field],
) -> Result<RecordBatch> {
    // Batch-level passes (order: dup → missing).
    let batch = if let Some(rate) = quality.duplication {
        apply_duplication(batch, rate)?
    } else {
        batch
    };
    let batch = if let Some(rate) = quality.missing {
        apply_missing(batch, rate)?
    } else {
        batch
    };

    // Pre-compute per-column std devs for noise corruption (single pass over clean batch).
    let stddevs = compute_stddevs(&batch);

    // Per-column transforms.
    apply_column_transforms(batch, quality, schema, &stddevs)
}

// ---------------------------------------------------------------------------
// Batch-level transforms
// ---------------------------------------------------------------------------

fn apply_duplication(batch: RecordBatch, rate: f64) -> Result<RecordBatch> {
    if rate <= 0.0 {
        return Ok(batch);
    }
    let n = batch.num_rows();
    let dup_count = (rate * n as f64).round() as usize;
    if dup_count == 0 {
        return Ok(batch);
    }

    let mut indices: Vec<u32> = (0..dup_count as u32)
        .map(|_| (0u32..n as u32).fake::<u32>())
        .collect();
    indices.sort_unstable();
    let index_array = UInt32Array::from(indices);
    let dup_columns: Vec<ArrayRef> = batch
        .columns()
        .iter()
        .map(|col| {
            arrow::compute::take(col.as_ref(), &index_array, None)
                .context("duplication: take failed")
        })
        .collect::<Result<_>>()?;
    let dup_batch = RecordBatch::try_new(batch.schema(), dup_columns)
        .context("duplication: record batch construction failed")?;
    concat_batches(&batch.schema(), &[batch, dup_batch]).context("duplication: concat failed")
}

fn apply_missing(batch: RecordBatch, rate: f64) -> Result<RecordBatch> {
    if rate <= 0.0 {
        return Ok(batch);
    }
    let n = batch.num_rows();
    let keep: Vec<bool> = (0..n)
        .map(|_| (0.0f64..1.0f64).fake::<f64>() >= rate)
        .collect();
    let mask = BooleanArray::from(keep);
    filter_batch(&batch, &mask)
}

fn filter_batch(batch: &RecordBatch, mask: &BooleanArray) -> Result<RecordBatch> {
    let cols: Vec<ArrayRef> = batch
        .columns()
        .iter()
        .map(|col| filter(col.as_ref(), mask).context("missing: filter failed"))
        .collect::<Result<_>>()?;
    RecordBatch::try_new(batch.schema(), cols).context("missing: record batch construction failed")
}

// ---------------------------------------------------------------------------
// Column-level transform dispatcher
// ---------------------------------------------------------------------------

fn apply_column_transforms(
    batch: RecordBatch,
    dataset_q: &DataQuality,
    schema: &[Field],
    stddevs: &std::collections::HashMap<String, f64>,
) -> Result<RecordBatch> {
    let n = batch.num_rows();
    let mut new_cols: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());

    for (col_idx, arrow_field) in batch.schema().fields().iter().enumerate() {
        let col = batch.column(col_idx).clone();
        let col_name = arrow_field.name();

        // Find the matching Field definition in the dataset schema.
        let field_def = schema.iter().find(|f| &f.name == col_name);
        let field_type = field_def.and_then(|f| f.field_type.as_ref());
        let field_q = field_def.and_then(|f| f.quality.as_ref());

        // Merge dataset-level + field-level rates: field-level wins slot-by-slot.
        let nulls_rate = field_q.and_then(|q| q.nulls).or(dataset_q.nulls);
        let defaults_rate = field_q
            .and_then(|q| q.default_rate)
            .or(dataset_q.default_rate);
        let corruptions = field_q
            .and_then(|q| q.corruptions.as_ref())
            .or(dataset_q.corruptions.as_ref());
        let default_values = field_q.and_then(|q| q.default_values.as_ref());
        let defaults_mode = field_q.and_then(|q| q.defaults_mode.as_ref());

        let col = if let Some(rate) = nulls_rate {
            apply_nulls(col, rate, n)
        } else {
            col
        };
        let col = if let Some(rate) = defaults_rate {
            let ft = field_type.unwrap_or(&FieldType::String);
            apply_defaults(col, ft, rate, n, default_values, defaults_mode)?
        } else {
            col
        };
        let col = if let Some(c) = corruptions {
            let ft = field_type.unwrap_or(&FieldType::String);
            let stddev = stddevs.get(col_name).copied().unwrap_or(0.0);
            apply_corruptions(col, ft, c, stddev, n)?
        } else {
            col
        };

        new_cols.push(col);
    }

    RecordBatch::try_new(batch.schema(), new_cols)
        .context("column transforms: batch rebuild failed")
}

// ---------------------------------------------------------------------------
// Nulls
// ---------------------------------------------------------------------------

fn apply_nulls(col: ArrayRef, rate: f64, n: usize) -> ArrayRef {
    if rate <= 0.0 {
        return col;
    }
    let null_col: ArrayRef = new_null_array(col.data_type(), n);
    // keep_mask: true → keep original value; false → use null.
    let keep_mask: BooleanArray = (0..n)
        .map(|_| Some((0.0f64..1.0f64).fake::<f64>() >= rate))
        .collect();
    // `zip` expects &dyn Datum; ArrayRef (= Arc<dyn Array>) implements Datum.
    zip(&keep_mask, &col, &null_col).unwrap_or(col)
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

fn apply_defaults(
    col: ArrayRef,
    ft: &FieldType,
    rate: f64,
    n: usize,
    custom_values: Option<&Vec<serde_yaml::Value>>,
    mode: Option<&DefaultsMode>,
) -> Result<ArrayRef> {
    if rate <= 0.0 {
        return Ok(col);
    }

    let active_defaults: Vec<DefaultValue> = build_default_set(ft, custom_values, mode);
    if active_defaults.is_empty() {
        return Ok(col);
    }

    let pool_len = active_defaults.len();
    let replacements: Vec<Option<usize>> = (0..n)
        .map(|_| {
            if (0.0f64..1.0f64).fake::<f64>() < rate {
                Some((0usize..pool_len).fake::<usize>())
            } else {
                None
            }
        })
        .collect();

    replace_with_defaults(col, &active_defaults, &replacements)
}

#[derive(Clone)]
enum DefaultValue {
    Str(String),
    F64(f64),
    Bool(bool),
    Date32(i32),
    TsUs(i64),
}

fn build_default_set(
    ft: &FieldType,
    custom_values: Option<&Vec<serde_yaml::Value>>,
    mode: Option<&DefaultsMode>,
) -> Vec<DefaultValue> {
    let builtin: Vec<DefaultValue> = match ft {
        FieldType::String => vec![
            DefaultValue::Str("".to_string()),
            DefaultValue::Str("N/A".to_string()),
            DefaultValue::Str("NA".to_string()),
            DefaultValue::Str("None".to_string()),
            DefaultValue::Str("NULL".to_string()),
            DefaultValue::Str("n/a".to_string()),
        ],
        FieldType::Number => vec![DefaultValue::F64(0.0)],
        FieldType::Boolean => vec![DefaultValue::Bool(false)],
        FieldType::Date => vec![
            DefaultValue::Date32(0),       // 1970-01-01
            DefaultValue::Date32(-25567),  // 1900-01-01
            DefaultValue::Date32(2932896), // 9999-12-31
        ],
        FieldType::DateTime => vec![DefaultValue::TsUs(0)], // 1970-01-01T00:00:00Z
        _ => vec![],
    };

    let custom: Vec<DefaultValue> = custom_values
        .map(|vals| vals.iter().filter_map(|v| yaml_to_default(v, ft)).collect())
        .unwrap_or_default();

    match mode {
        Some(DefaultsMode::Override) => custom,
        _ => {
            let mut merged = builtin;
            merged.extend(custom);
            merged
        }
    }
}

fn yaml_to_default(v: &serde_yaml::Value, ft: &FieldType) -> Option<DefaultValue> {
    match ft {
        FieldType::String => v
            .as_str()
            .map(|s| DefaultValue::Str(s.to_string()))
            .or_else(|| Some(DefaultValue::Str(format!("{v:?}")))),
        FieldType::Number => v.as_f64().map(DefaultValue::F64),
        FieldType::Boolean => v.as_bool().map(DefaultValue::Bool),
        _ => None,
    }
}

fn replace_with_defaults(
    col: ArrayRef,
    defaults: &[DefaultValue],
    replacements: &[Option<usize>],
) -> Result<ArrayRef> {
    match col.data_type() {
        DataType::Utf8 => {
            let strings: Vec<Option<&str>> =
                if let Some(a) = col.as_any().downcast_ref::<StringArray>() {
                    (0..a.len())
                        .map(|i| if a.is_null(i) { None } else { Some(a.value(i)) })
                        .collect()
                } else {
                    return Ok(col);
                };
            let result: StringArray = strings
                .iter()
                .enumerate()
                .map(|(i, orig)| {
                    if let Some(idx) = replacements[i] {
                        if let DefaultValue::Str(s) = &defaults[idx] {
                            return Some(s.as_str());
                        }
                    }
                    *orig
                })
                .collect();
            Ok(Arc::new(result))
        }
        DataType::Float64 => {
            let floats = col.as_any().downcast_ref::<Float64Array>();
            let result: Float64Array = (0..col.len())
                .map(|i| {
                    if let Some(idx) = replacements[i] {
                        if let DefaultValue::F64(v) = defaults[idx] {
                            return Some(v);
                        }
                    }
                    floats.and_then(|a| if a.is_null(i) { None } else { Some(a.value(i)) })
                })
                .collect();
            Ok(Arc::new(result))
        }
        DataType::Int32
        | DataType::Int64
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Float32
        | DataType::Int8
        | DataType::Int16
        | DataType::UInt8
        | DataType::UInt16 => {
            let float_col = cast(col.as_ref(), &DataType::Float64)
                .context("defaults: cast to float64 failed")?;
            let float_col = replace_with_defaults(float_col, defaults, replacements)?;
            cast(float_col.as_ref(), col.data_type()).context("defaults: cast back failed")
        }
        DataType::Boolean => {
            let bools = col.as_any().downcast_ref::<BooleanArray>();
            let result: BooleanArray = (0..col.len())
                .map(|i| {
                    if let Some(idx) = replacements[i] {
                        if let DefaultValue::Bool(b) = defaults[idx] {
                            return Some(b);
                        }
                    }
                    bools.and_then(|a| if a.is_null(i) { None } else { Some(a.value(i)) })
                })
                .collect();
            Ok(Arc::new(result))
        }
        DataType::Date32 => {
            let dates = col.as_any().downcast_ref::<Date32Array>();
            let result: Date32Array = (0..col.len())
                .map(|i| {
                    if let Some(idx) = replacements[i] {
                        if let DefaultValue::Date32(d) = defaults[idx] {
                            return Some(d);
                        }
                    }
                    dates.and_then(|a| if a.is_null(i) { None } else { Some(a.value(i)) })
                })
                .collect();
            Ok(Arc::new(result))
        }
        DataType::Timestamp(_, _) => {
            let result: TimestampMicrosecondArray = (0..col.len())
                .map(|i| {
                    if let Some(idx) = replacements[i] {
                        if let DefaultValue::TsUs(t) = defaults[idx] {
                            return Some(t);
                        }
                    }
                    // Leave original value unchanged; downcast attempt below
                    None::<i64>
                })
                .collect();
            // Merge: use replacement where fired, original elsewhere.
            let orig = col
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .or_else(|| None); // Handle millis case below
            let merged: TimestampMicrosecondArray = (0..col.len())
                .map(|i| {
                    if result.is_valid(i) {
                        Some(result.value(i))
                    } else {
                        orig.and_then(|a| if a.is_null(i) { None } else { Some(a.value(i)) })
                    }
                })
                .collect();
            Ok(Arc::new(merged))
        }
        _ => Ok(col),
    }
}

// ---------------------------------------------------------------------------
// Corruptions dispatcher
// ---------------------------------------------------------------------------

fn apply_corruptions(
    col: ArrayRef,
    ft: &FieldType,
    c: &Corruptions,
    stddev: f64,
    n: usize,
) -> Result<ArrayRef> {
    match ft {
        FieldType::String => {
            let col = if let Some(r) = c.character_deletion {
                corrupt_char_deletion(col, r, n)
            } else {
                col
            };
            let col = if let Some(r) = c.character_insertion {
                corrupt_char_insertion(col, r, n)
            } else {
                col
            };
            let col = if let Some(r) = c.truncation {
                corrupt_truncation(col, r, n)
            } else {
                col
            };
            let col = if let Some(r) = c.encoding {
                corrupt_encoding(col, r, n)
            } else {
                col
            };
            Ok(col)
        }
        FieldType::Number => {
            let col = if let Some(r) = c.noise {
                let amplitude = if stddev == 0.0 {
                    c.noise_scale
                } else {
                    c.noise_scale * stddev
                };
                corrupt_noise(col, r, amplitude, n)?
            } else {
                col
            };
            Ok(col)
        }
        FieldType::Date | FieldType::DateTime => {
            let col = if let Some(r) = c.day_shift {
                corrupt_day_shift(col, r, c.day_shift_max, n, ft)?
            } else {
                col
            };
            Ok(col)
        }
        _ => Ok(col),
    }
}

// ---------------------------------------------------------------------------
// String corruption helpers
// ---------------------------------------------------------------------------

fn corrupt_char_deletion(col: ArrayRef, rate: f64, n: usize) -> ArrayRef {
    let Some(arr) = col.as_any().downcast_ref::<StringArray>() else {
        return col;
    };
    let result: StringArray = (0..n)
        .map(|i| {
            if arr.is_null(i) {
                return None;
            }
            let s = arr.value(i);
            if s.is_empty() || (0.0f64..1.0f64).fake::<f64>() >= rate {
                return Some(s.to_string());
            }
            let chars: Vec<char> = s.chars().collect();
            let del_pos = (0usize..chars.len()).fake::<usize>();
            let result: String = chars
                .iter()
                .enumerate()
                .filter(|(idx, _)| *idx != del_pos)
                .map(|(_, c)| *c)
                .collect();
            Some(result)
        })
        .collect();
    Arc::new(result)
}

fn corrupt_char_insertion(col: ArrayRef, rate: f64, n: usize) -> ArrayRef {
    let Some(arr) = col.as_any().downcast_ref::<StringArray>() else {
        return col;
    };
    let result: StringArray = (0..n)
        .map(|i| {
            if arr.is_null(i) {
                return None;
            }
            let s = arr.value(i);
            if (0.0f64..1.0f64).fake::<f64>() >= rate {
                return Some(s.to_string());
            }
            let chars: Vec<char> = s.chars().collect();
            let ins_pos = (0usize..=chars.len()).fake::<usize>();
            let rnd_char = char::from((32u8..127u8).fake::<u8>());
            let mut result: String = chars[..ins_pos].iter().collect();
            result.push(rnd_char);
            result.extend(&chars[ins_pos..]);
            Some(result)
        })
        .collect();
    Arc::new(result)
}

fn corrupt_truncation(col: ArrayRef, rate: f64, n: usize) -> ArrayRef {
    let Some(arr) = col.as_any().downcast_ref::<StringArray>() else {
        return col;
    };
    let result: StringArray = (0..n)
        .map(|i| {
            if arr.is_null(i) {
                return None;
            }
            let s = arr.value(i);
            if s.is_empty() || (0.0f64..1.0f64).fake::<f64>() >= rate {
                return Some(s.to_string());
            }
            let char_count = s.chars().count();
            let trunc_len = (0usize..char_count).fake::<usize>();
            Some(s.chars().take(trunc_len).collect())
        })
        .collect();
    Arc::new(result)
}

fn corrupt_encoding(col: ArrayRef, rate: f64, n: usize) -> ArrayRef {
    let Some(arr) = col.as_any().downcast_ref::<StringArray>() else {
        return col;
    };
    let result: StringArray = (0..n)
        .map(|i| {
            if arr.is_null(i) {
                return None;
            }
            let s = arr.value(i);
            if s.is_empty() || (0.0f64..1.0f64).fake::<f64>() >= rate {
                return Some(s.to_string());
            }
            // Re-encode a random substring through windows-1252 (latin-1 subset), producing mojibake.
            let bytes = s.as_bytes();
            let len = bytes.len();
            if len == 0 {
                return Some(s.to_string());
            }
            let start = (0usize..len).fake::<usize>();
            let end = (start..=len).fake::<usize>();
            let mut corrupted = bytes.to_vec();
            // Round-trip each byte in [start, end) through latin-1: if > 127, mis-interpret as UTF-8.
            for b in &mut corrupted[start..end] {
                if *b > 127 {
                    // Replace with a visually-corrupt but valid-UTF-8 latin-1 surrogate
                    *b = 0xC0 | (*b >> 6); // produces a 2-byte lead without the continuation byte
                }
            }
            // If result is not valid UTF-8, fall back to replacement-character path.
            Some(String::from_utf8_lossy(&corrupted).into_owned())
        })
        .collect();
    Arc::new(result)
}

// ---------------------------------------------------------------------------
// Numeric corruption helpers
// ---------------------------------------------------------------------------

fn corrupt_noise(col: ArrayRef, rate: f64, amplitude: f64, n: usize) -> Result<ArrayRef> {
    // Cast to Float64, apply noise, cast back.
    let float_col =
        cast(col.as_ref(), &DataType::Float64).context("noise: cast to float64 failed")?;
    let arr = float_col
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| anyhow::anyhow!("noise: downcast to Float64Array failed"))?;

    let result: Float64Array = (0..n)
        .map(|i| {
            if arr.is_null(i) {
                return None;
            }
            let v = arr.value(i);
            if (0.0f64..1.0f64).fake::<f64>() >= rate {
                return Some(v);
            }
            // Box-Muller transform for N(0, amplitude).
            let u1 = (f64::EPSILON..1.0f64).fake::<f64>();
            let u2 = (0.0f64..1.0f64).fake::<f64>();
            let z = (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos();
            Some(v + amplitude * z)
        })
        .collect();

    if *col.data_type() == DataType::Float64 {
        return Ok(Arc::new(result));
    }
    cast(Arc::new(result).as_ref(), col.data_type()).context("noise: cast back failed")
}

// ---------------------------------------------------------------------------
// Date / date-time corruption helpers
// ---------------------------------------------------------------------------

fn corrupt_day_shift(
    col: ArrayRef,
    rate: f64,
    max_days: i64,
    n: usize,
    _ft: &FieldType,
) -> Result<ArrayRef> {
    if max_days == 0 {
        return Ok(col);
    }
    let range = -(max_days)..=(max_days);

    match col.data_type() {
        DataType::Date32 => {
            let arr = col
                .as_any()
                .downcast_ref::<Date32Array>()
                .ok_or_else(|| anyhow::anyhow!("day_shift: downcast Date32 failed"))?;
            let result: Date32Array = (0..n)
                .map(|i| {
                    if arr.is_null(i) {
                        return None;
                    }
                    if (0.0f64..1.0f64).fake::<f64>() >= rate {
                        return Some(arr.value(i));
                    }
                    let shift = (*range.start()..=*range.end()).fake::<i64>() as i32;
                    Some(arr.value(i) + shift)
                })
                .collect();
            Ok(Arc::new(result))
        }
        DataType::Timestamp(arrow::datatypes::TimeUnit::Microsecond, _) => {
            let arr = col
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .ok_or_else(|| anyhow::anyhow!("day_shift: downcast TimestampUs failed"))?;
            let us_per_day: i64 = 86_400 * 1_000_000;
            let result: TimestampMicrosecondArray = (0..n)
                .map(|i| {
                    if arr.is_null(i) {
                        return None;
                    }
                    if (0.0f64..1.0f64).fake::<f64>() >= rate {
                        return Some(arr.value(i));
                    }
                    let shift = (*range.start()..=*range.end()).fake::<i64>() * us_per_day;
                    Some(arr.value(i) + shift)
                })
                .collect();
            Ok(Arc::new(result))
        }
        DataType::Timestamp(arrow::datatypes::TimeUnit::Millisecond, _) => {
            let arr = col
                .as_any()
                .downcast_ref::<TimestampMillisecondArray>()
                .ok_or_else(|| anyhow::anyhow!("day_shift: downcast TimestampMs failed"))?;
            let ms_per_day: i64 = 86_400 * 1_000;
            let result: TimestampMillisecondArray = (0..n)
                .map(|i| {
                    if arr.is_null(i) {
                        return None;
                    }
                    if (0.0f64..1.0f64).fake::<f64>() >= rate {
                        return Some(arr.value(i));
                    }
                    let shift = (*range.start()..=*range.end()).fake::<i64>() * ms_per_day;
                    Some(arr.value(i) + shift)
                })
                .collect();
            Ok(Arc::new(result))
        }
        _ => Ok(col),
    }
}

// ---------------------------------------------------------------------------
// Column std dev computation (for noise amplitude)
// ---------------------------------------------------------------------------

pub(crate) fn compute_stddevs(batch: &RecordBatch) -> std::collections::HashMap<String, f64> {
    let mut map = std::collections::HashMap::new();
    for (i, field) in batch.schema().fields().iter().enumerate() {
        let col = batch.column(i);
        let float_col = match cast(col.as_ref(), &DataType::Float64) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Some(arr) = float_col.as_any().downcast_ref::<Float64Array>() else {
            continue;
        };
        let values: Vec<f64> = (0..arr.len())
            .filter(|&j| !arr.is_null(j))
            .map(|j| arr.value(j))
            .collect();
        if values.is_empty() {
            continue;
        }
        let n = values.len() as f64;
        let mean = values.iter().sum::<f64>() / n;
        let variance = values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        map.insert(field.name().clone(), variance.sqrt());
    }
    map
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::StringArray;
    use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    fn string_batch(vals: Vec<&str>) -> RecordBatch {
        let schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "name",
            DataType::Utf8,
            true,
        )]));
        let col: ArrayRef = Arc::new(StringArray::from(vals));
        RecordBatch::try_new(schema, vec![col]).unwrap()
    }

    fn float_batch(vals: Vec<f64>) -> RecordBatch {
        let schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "value",
            DataType::Float64,
            true,
        )]));
        let col: ArrayRef = Arc::new(Float64Array::from(vals));
        RecordBatch::try_new(schema, vec![col]).unwrap()
    }

    #[test]
    fn duplication_increases_row_count() {
        let batch = string_batch(vec!["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
        let result = apply_duplication(batch.clone(), 0.5).unwrap();
        assert!(result.num_rows() > batch.num_rows());
    }

    #[test]
    fn missing_reduces_row_count() {
        // Use a large batch so the Bernoulli rate reliably removes rows.
        let vals: Vec<&str> = (0..1000).map(|_| "x").collect();
        let batch = string_batch(vals);
        let result = apply_missing(batch.clone(), 0.5).unwrap();
        assert!(result.num_rows() < batch.num_rows());
    }

    #[test]
    fn null_rate_introduces_nulls() {
        let vals: Vec<&str> = (0..1000).map(|_| "hello").collect();
        let batch = string_batch(vals);
        let q = DataQuality {
            nulls: Some(0.5),
            duplication: None,
            missing: None,
            default_rate: None,
            corruptions: None,
            default_values: None,
            defaults_mode: None,
        };
        let result = apply_data_quality(batch, &q, &[]).unwrap();
        let col = result
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let null_count = (0..col.len()).filter(|&i| col.is_null(i)).count();
        assert!(null_count > 0, "expected nulls to be introduced");
    }

    #[test]
    fn truncation_shortens_strings() {
        let vals: Vec<&str> = (0..100).map(|_| "hello_world").collect();
        let batch = string_batch(vals);
        let q = DataQuality {
            corruptions: Some(Corruptions {
                truncation: Some(1.0), // fire on every cell
                character_deletion: None,
                character_insertion: None,
                encoding: None,
                noise: None,
                noise_scale: 1.0,
                day_shift: None,
                day_shift_max: 30,
            }),
            duplication: None,
            missing: None,
            nulls: None,
            default_rate: None,
            default_values: None,
            defaults_mode: None,
        };
        let result = apply_data_quality(
            batch,
            &q,
            &[Field {
                name: "name".to_string(),
                field_type: Some(FieldType::String),
                ..Default::default()
            }],
        )
        .unwrap();
        let col = result
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let shortened = (0..col.len())
            .filter(|&i| !col.is_null(i) && col.value(i).len() < "hello_world".len())
            .count();
        // With rate=1.0 many cells should be shortened (some may truncate to same length if random picks max)
        assert!(shortened > 0);
    }

    #[test]
    fn noise_changes_values() {
        let vals: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let batch = float_batch(vals.clone());
        let q = DataQuality {
            corruptions: Some(Corruptions {
                noise: Some(1.0),
                noise_scale: 1.0,
                day_shift: None,
                day_shift_max: 30,
                character_deletion: None,
                character_insertion: None,
                truncation: None,
                encoding: None,
            }),
            duplication: None,
            missing: None,
            nulls: None,
            default_rate: None,
            default_values: None,
            defaults_mode: None,
        };
        let result = apply_data_quality(
            batch,
            &q,
            &[Field {
                name: "value".to_string(),
                field_type: Some(FieldType::Number),
                ..Default::default()
            }],
        )
        .unwrap();
        let col = result
            .column(0)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let changed = (0..col.len())
            .filter(|&i| !col.is_null(i) && (col.value(i) - vals[i]).abs() > 1e-9)
            .count();
        assert!(changed > 0, "expected noise to change some values");
    }
}
