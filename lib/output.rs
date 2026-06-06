//! Output encoding — the **write-time** layer (VAR-1 + VAR-UNIFY). Turning a fully computed,
//! lattice-generated `RecordBatch` into bytes on disk is a *deserialisation concern*, kept
//! separate from generation: the internal model carries Arrow `DenseUnion` columns, and these
//! functions reshape them only as a batch is written.
//!
//! - [`filter_hidden_columns`] drops `hidden` fields before writing.
//! - [`prepare_output_batch`] applies `flatten` (pull a field's sub-columns up one level) and
//!   converts any remaining `DenseUnion` to a portable nullable-superset struct.
//! - [`write_output`] dispatches the prepared batch to the Parquet/CSV/JSON/JSONL writer.
use anyhow::{Result, anyhow};
use arrow::array::{Array, ArrayRef, StringArray, StructArray, UInt32Array, UnionArray};
use arrow::compute::take;
use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema, UnionFields};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use crate::models::{Field, FlattenStrategy, Format, discriminant_tag_column};

/// Remove columns marked `hidden` from a batch before writing output.
/// The full batch (including hidden columns) is kept in `computed` for inherited
/// field wiring; only the filtered batch is written to output.
pub(crate) fn filter_hidden_columns(batch: RecordBatch, fields: &[Field]) -> Result<RecordBatch> {
    if !fields.iter().any(|f| f.hidden) {
        return Ok(batch);
    }
    let hidden: HashSet<&str> = fields
        .iter()
        .filter(|f| f.hidden)
        .map(|f| f.name.as_str())
        .collect();
    let visible: Vec<usize> = (0..batch.num_columns())
        .filter(|&i| !hidden.contains(batch.schema().field(i).name().as_str()))
        .collect();
    Ok(batch.project(&visible)?)
}

/// True if `dt` is, or nests, an Arrow union.
fn contains_union(dt: &DataType) -> bool {
    match dt {
        DataType::Union(..) => true,
        DataType::Struct(fields) => fields.iter().any(|f| contains_union(f.data_type())),
        DataType::List(field) => contains_union(field.data_type()),
        _ => false,
    }
}

/// Convert every Arrow `DenseUnion` column (VAR-1) into a portable **nullable-superset
/// struct** for output: one nullable sub-field per case, each row populating only its
/// active case's sub-field (others null). The populated sub-field is the readable case tag.
/// No writer can serialise a union directly (parquet panics — ARROW-8817; json/jsonl/csv
/// fail), so this runs at the very end, in `write_output`. Recurses into struct columns
/// (a union nested in an object field). Batches with no union pass through untouched.
pub(crate) fn unionize_for_output(batch: &RecordBatch) -> Result<RecordBatch> {
    if !batch
        .columns()
        .iter()
        .any(|c| contains_union(c.data_type()))
    {
        return Ok(batch.clone());
    }
    let mut fields: Vec<Arc<ArrowField>> = Vec::with_capacity(batch.num_columns());
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for (f, col) in batch.schema().fields().iter().zip(batch.columns()) {
        let (nf, nc) = union_to_portable(f, col)?;
        fields.push(Arc::new(nf));
        columns.push(nc);
    }
    Ok(RecordBatch::try_new(
        Arc::new(ArrowSchema::new(fields)),
        columns,
    )?)
}

/// Recursively rewrite a `(field, array)` pair so any `DenseUnion` becomes a
/// nullable-superset struct. Structs recurse on their children; everything else is
/// returned unchanged.
fn union_to_portable(field: &ArrowField, col: &ArrayRef) -> Result<(ArrowField, ArrayRef)> {
    match col.data_type() {
        DataType::Union(union_fields, _) => {
            let u = col
                .as_any()
                .downcast_ref::<UnionArray>()
                .ok_or_else(|| anyhow!("union column '{}' is not a UnionArray", field.name()))?;
            let type_ids = u.type_ids();
            let offsets = u
                .offsets()
                .ok_or_else(|| anyhow!("union '{}' must be dense", field.name()))?;
            let n = type_ids.len();
            let mut sub_fields: Vec<Arc<ArrowField>> = Vec::new();
            let mut sub_arrays: Vec<ArrayRef> = Vec::new();
            for (tid, case_field) in union_fields.iter() {
                let child = u.child(tid);
                // Gather indices into `child`: this case's offset where the row is this
                // case, null otherwise — `take` maps a null index to a null output slot.
                let idx: UInt32Array = (0..n)
                    .map(|r| (type_ids[r] == tid).then_some(offsets[r] as u32))
                    .collect();
                let sub = take(child.as_ref(), &idx, None)?;
                // Defensive recursion (a case child is union-free today, but keep it total).
                let cf = ArrowField::new(case_field.name(), child.data_type().clone(), true);
                let (cf, sub) = union_to_portable(&cf, &sub)?;
                sub_fields.push(Arc::new(cf));
                sub_arrays.push(sub);
            }
            let struct_fields: arrow::datatypes::Fields = sub_fields.into();
            let arr = StructArray::new(struct_fields.clone(), sub_arrays, None);
            Ok((
                ArrowField::new(field.name(), DataType::Struct(struct_fields), true),
                Arc::new(arr),
            ))
        }
        DataType::Struct(_) => {
            let s = col
                .as_any()
                .downcast_ref::<StructArray>()
                .ok_or_else(|| anyhow!("struct column '{}' is not a StructArray", field.name()))?;
            let DataType::Struct(orig_fields) = field.data_type() else {
                unreachable!("matched Struct above")
            };
            let mut child_fields: Vec<Arc<ArrowField>> = Vec::new();
            let mut child_arrays: Vec<ArrayRef> = Vec::new();
            for (cf, carr) in orig_fields.iter().zip(s.columns()) {
                let (ncf, narr) = union_to_portable(cf, carr)?;
                child_fields.push(Arc::new(ncf));
                child_arrays.push(narr);
            }
            let fields: arrow::datatypes::Fields = child_fields.into();
            let arr = StructArray::new(fields.clone(), child_arrays, s.nulls().cloned());
            Ok((
                ArrowField::new(field.name(), DataType::Struct(fields), field.is_nullable()),
                Arc::new(arr),
            ))
        }
        _ => Ok((field.clone(), col.clone())),
    }
}

/// Prepare a batch for writing (VAR-UNIFY): pull up any top-level `flatten` field's
/// sub-columns to the row level, then convert any remaining `DenseUnion` columns to portable
/// nullable-superset structs (VAR-1). Columns not named by a `flatten` field go through
/// `union_to_portable` unchanged. Nested flatten is gated at validation, so only top-level
/// flatten fields are handled here.
pub(crate) fn prepare_output_batch(
    batch: &RecordBatch,
    fields: &[Field],
    format: &Format,
) -> Result<RecordBatch> {
    // JSON/JSONL emit per-row keys (null omission), so flat-columnar strategies don't apply:
    // force `superset` there. Flat formats apply the field's declared strategy.
    let json = matches!(format, Format::Json | Format::Jsonl);
    let flatten: HashMap<&str, FlattenStrategy> = fields
        .iter()
        .filter(|f| f.flatten)
        .map(|f| {
            let strategy = if json {
                FlattenStrategy::Superset
            } else {
                f.flatten_strategy.unwrap_or_default()
            };
            (f.name.as_str(), strategy)
        })
        .collect();

    // Fast path: no flatten fields → the plain VAR-1 union→struct conversion.
    if flatten.is_empty() {
        return unionize_for_output(batch);
    }

    let mut out_fields: Vec<Arc<ArrowField>> = Vec::with_capacity(batch.num_columns());
    let mut out_cols: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for (f, col) in batch.schema().fields().iter().zip(batch.columns()) {
        if let Some(&strategy) = flatten.get(f.name().as_str()) {
            for (nf, nc) in flatten_column(f, col, strategy)? {
                out_fields.push(Arc::new(nf));
                out_cols.push(nc);
            }
        } else {
            let (nf, nc) = union_to_portable(f, col)?;
            out_fields.push(Arc::new(nf));
            out_cols.push(nc);
        }
    }
    Ok(RecordBatch::try_new(
        Arc::new(ArrowSchema::new(out_fields)),
        out_cols,
    )?)
}

/// Pull a single `flatten` field's nesting up one level, returning the `(field, array)` pairs
/// to splice into the parent. An `object` (struct) yields its children; a `variant` (union)
/// distributes flatten to its cases per `strategy`, yielding the case fields as a nullable
/// superset (object case → its fields; scalar case → one case-named column). For Parquet/CSV
/// every row is present with one case populated; for JSON/JSONL null keys are omitted, so each
/// row carries only the active case's keys.
fn flatten_column(
    field: &ArrowField,
    col: &ArrayRef,
    strategy: FlattenStrategy,
) -> Result<Vec<(ArrowField, ArrayRef)>> {
    match col.data_type() {
        DataType::Union(union_fields, _) => {
            let u = col
                .as_any()
                .downcast_ref::<UnionArray>()
                .ok_or_else(|| anyhow!("flatten field '{}' is not a UnionArray", field.name()))?;
            flatten_union_to_columns(field.name(), u, union_fields, strategy)
        }
        // An object has no cases, so `strategy` doesn't apply — splice its children raw.
        DataType::Struct(struct_fields) => {
            let s = col
                .as_any()
                .downcast_ref::<StructArray>()
                .ok_or_else(|| anyhow!("flatten field '{}' is not a StructArray", field.name()))?;
            struct_fields
                .iter()
                .zip(s.columns())
                .map(|(cf, carr)| union_to_portable(cf.as_ref(), carr))
                .collect()
        }
        other => Err(anyhow!(
            "field '{}': cannot flatten a {other:?} column (expected object or variant)",
            field.name()
        )),
    }
}

/// Build the nullable-superset columns for a flattened union per `strategy`: each case's
/// fields (object case) or a single case-named column (scalar case), each null on rows where
/// that case did not fire (`take` with a null index → null slot). `Prefixed` namespaces
/// object-case field names by case label; `Discriminant` appends a `<field>_case` tag column.
fn flatten_union_to_columns(
    field_name: &str,
    u: &UnionArray,
    union_fields: &UnionFields,
    strategy: FlattenStrategy,
) -> Result<Vec<(ArrowField, ArrayRef)>> {
    let type_ids = u.type_ids();
    let offsets = u
        .offsets()
        .ok_or_else(|| anyhow!("flatten union '{field_name}' must be dense"))?;
    let n = type_ids.len();
    let mut out = Vec::new();
    for (tid, case_field) in union_fields.iter() {
        let label = case_field.name();
        let child = u.child(tid);
        // Indices into this case's child: its offset where the row is this case, null
        // otherwise — so each pulled-up column is null on rows where the case didn't fire.
        let idx: UInt32Array = (0..n)
            .map(|r| (type_ids[r] == tid).then_some(offsets[r] as u32))
            .collect();
        match child.data_type() {
            DataType::Struct(case_struct_fields) => {
                let s = child
                    .as_any()
                    .downcast_ref::<StructArray>()
                    .ok_or_else(|| anyhow!("union case '{label}' is not a struct"))?;
                for (cf, carr) in case_struct_fields.iter().zip(s.columns()) {
                    let taken = take(carr.as_ref(), &idx, None)?;
                    let name = if strategy == FlattenStrategy::Prefixed {
                        format!("{label}_{}", cf.name())
                    } else {
                        cf.name().to_string()
                    };
                    // Defensive: lower any union nested inside a case field.
                    let nested = ArrowField::new(&name, cf.data_type().clone(), true);
                    out.push(union_to_portable(&nested, &taken)?);
                }
            }
            other => {
                let taken = take(child.as_ref(), &idx, None)?;
                out.push((ArrowField::new(label, other.clone(), true), taken));
            }
        }
    }

    // `Discriminant`: append a visible `<field>_case` column naming the active case per row.
    if strategy == FlattenStrategy::Discriminant {
        let id_to_label: HashMap<i8, &str> = union_fields
            .iter()
            .map(|(tid, f)| (tid, f.name().as_str()))
            .collect();
        let tag: StringArray = (0..n)
            .map(|r| id_to_label.get(&type_ids[r]).copied())
            .collect();
        out.push((
            ArrowField::new(discriminant_tag_column(field_name), DataType::Utf8, true),
            Arc::new(tag),
        ));
    }
    Ok(out)
}

pub(crate) fn write_output(
    batch: &RecordBatch,
    name: &str,
    format: &Format,
    output_dir: &Path,
    fields: &[Field],
) -> Result<()> {
    // VAR-UNIFY: pull up flatten fields; VAR-1: lower any union column to a portable
    // nullable-superset struct. Both happen at write time only.
    let converted = prepare_output_batch(batch, fields, format)?;
    let batch = &converted;
    let ext = match format {
        Format::Parquet => "parquet",
        Format::Csv => "csv",
        Format::Json => "json",
        Format::Jsonl => "jsonl",
    };
    // If name already ends with the correct extension (e.g. from an explicit `file:` path),
    // use it as-is; otherwise append the extension (legacy `output_file:` name convention).
    let path = if std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        == Some(ext)
    {
        output_dir.join(name)
    } else {
        output_dir.join(format!("{name}.{ext}"))
    };
    let file = File::create(&path)?;

    match format {
        Format::Parquet => {
            let mut writer = ArrowWriter::try_new(file, batch.schema(), None)?;
            writer.write(batch)?;
            writer.close()?;
        }
        Format::Csv => {
            let mut writer = arrow::csv::WriterBuilder::new()
                .with_header(true)
                .build(file);
            writer.write(batch)?;
        }
        Format::Json => {
            let mut writer = arrow::json::ArrayWriter::new(file);
            writer.write_batches(&[batch])?;
            writer.finish()?;
        }
        Format::Jsonl => {
            let mut writer = arrow::json::LineDelimitedWriter::new(file);
            writer.write_batches(&[batch])?;
            writer.finish()?;
        }
    }

    println!("  wrote {}", path.display());
    Ok(())
}
