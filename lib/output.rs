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

use crate::arrow_util::downcast;
use crate::models::{Field, FlattenStrategy, Format, discriminant_tag_column};

/// Remove columns marked `hidden` from a batch and project the survivors into **canonical
/// YAML (`fields`) order** before writing output. Canonical ordering makes output
/// independent of *where* each column was computed — required by EXPR-RELOCATE, which
/// relocates expression evaluation to earlier pipeline points (so a relocated column would
/// otherwise shift its position in the batch). The full batch (including hidden columns) is
/// kept in `computed` for inherited field wiring; only this filtered, reordered batch is
/// written.
///
/// Declared, non-hidden fields lead in `fields` order; any leftover batch column with no
/// matching declared field (e.g. a carried sentinel) keeps its original relative position at
/// the end, preserving the prior inclusion set.
pub(crate) fn filter_hidden_columns(batch: RecordBatch, fields: &[Field]) -> Result<RecordBatch> {
    let schema = batch.schema();
    let hidden: HashSet<&str> = fields
        .iter()
        .filter(|f| f.hidden)
        .map(|f| f.name.as_str())
        .collect();

    let mut order: Vec<usize> = Vec::with_capacity(batch.num_columns());
    let mut taken = vec![false; batch.num_columns()];
    for f in fields {
        if f.hidden {
            continue;
        }
        if let Ok(i) = schema.index_of(&f.name) {
            if !taken[i] {
                order.push(i);
                taken[i] = true;
            }
        }
    }
    for i in 0..batch.num_columns() {
        if taken[i] || hidden.contains(schema.field(i).name().as_str()) {
            continue;
        }
        order.push(i);
    }
    Ok(batch.project(&order)?)
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
            let u =
                downcast::<UnionArray>(col.as_ref(), &format!("union column '{}'", field.name()))?;
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
            let s = downcast::<StructArray>(
                col.as_ref(),
                &format!("struct column '{}'", field.name()),
            )?;
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
            let u =
                downcast::<UnionArray>(col.as_ref(), &format!("flatten field '{}'", field.name()))?;
            flatten_union_to_columns(field.name(), u, union_fields, strategy)
        }
        // An object has no cases, so `strategy` doesn't apply — splice its children raw.
        DataType::Struct(struct_fields) => {
            let s = downcast::<StructArray>(
                col.as_ref(),
                &format!("flatten field '{}'", field.name()),
            )?;
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
                let s = downcast::<StructArray>(child.as_ref(), &format!("union case '{label}'"))?;
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

/// VAR-UNIFY PR U2 — `flatten`-aware output. Proves the write-time pull-up: an object
/// field's struct children and a union field's case fields are spliced to the row level,
/// and — the load-bearing assumption for "per-row keys" — the JSON writer omits null keys,
/// so a flattened union row carries only its active case's fields.
#[cfg(test)]
mod flatten_output {
    use super::*;
    use crate::models::FieldType;
    use arrow::array::Int32Array;
    use arrow::buffer::ScalarBuffer;
    use arrow::datatypes::UnionMode;

    fn flatten_field(name: &str) -> Field {
        Field {
            name: name.into(),
            field_type: Some(FieldType::Variant),
            flatten: true,
            ..Default::default()
        }
    }

    fn flatten_field_strategy(name: &str, strategy: FlattenStrategy) -> Field {
        Field {
            flatten_strategy: Some(strategy),
            ..flatten_field(name)
        }
    }

    /// 2-row batch: `id` + a `detail` union with two object cases that **share** a field
    /// name (`amount`) — the cross-case collision the `prefixed` strategy resolves.
    fn flatten_union_collision_batch() -> RecordBatch {
        let mk_struct = |v: i32| -> ArrayRef {
            Arc::new(StructArray::from(vec![(
                Arc::new(ArrowField::new("amount", DataType::Int32, true)),
                Arc::new(Int32Array::from(vec![v])) as ArrayRef,
            )]))
        };
        let alpha = mk_struct(10);
        let beta = mk_struct(20);
        let union_fields: UnionFields = [
            (
                0_i8,
                Arc::new(ArrowField::new("alpha", alpha.data_type().clone(), false)),
            ),
            (
                1_i8,
                Arc::new(ArrowField::new("beta", beta.data_type().clone(), false)),
            ),
        ]
        .into_iter()
        .collect();
        let union = UnionArray::try_new(
            union_fields.clone(),
            ScalarBuffer::<i8>::from(vec![0, 1]),
            Some(ScalarBuffer::<i32>::from(vec![0, 0])),
            vec![alpha, beta],
        )
        .expect("build union");
        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("id", DataType::Int32, false),
            ArrowField::new(
                "detail",
                DataType::Union(union_fields, UnionMode::Dense),
                false,
            ),
        ]));
        RecordBatch::try_new(
            schema,
            vec![Arc::new(Int32Array::from(vec![0, 1])), Arc::new(union)],
        )
        .expect("build batch")
    }

    fn col_names(batch: &RecordBatch) -> Vec<String> {
        batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }

    /// 2-row batch: `id` + a `detail` dense union with two **object** cases —
    /// row 0 = `alpha {a: Int32}`, row 1 = `beta {b: Utf8}`.
    fn flatten_union_batch() -> RecordBatch {
        let alpha_struct: ArrayRef = Arc::new(StructArray::from(vec![(
            Arc::new(ArrowField::new("a", DataType::Int32, true)),
            Arc::new(Int32Array::from(vec![10])) as ArrayRef,
        )]));
        let beta_struct: ArrayRef = Arc::new(StructArray::from(vec![(
            Arc::new(ArrowField::new("b", DataType::Utf8, true)),
            Arc::new(StringArray::from(vec!["x"])) as ArrayRef,
        )]));
        let union_fields: UnionFields = [
            (
                0_i8,
                Arc::new(ArrowField::new(
                    "alpha",
                    alpha_struct.data_type().clone(),
                    false,
                )),
            ),
            (
                1_i8,
                Arc::new(ArrowField::new(
                    "beta",
                    beta_struct.data_type().clone(),
                    false,
                )),
            ),
        ]
        .into_iter()
        .collect();
        let type_ids = ScalarBuffer::<i8>::from(vec![0, 1]);
        let offsets = ScalarBuffer::<i32>::from(vec![0, 0]);
        let union = UnionArray::try_new(
            union_fields.clone(),
            type_ids,
            Some(offsets),
            vec![alpha_struct, beta_struct],
        )
        .expect("build union");
        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("id", DataType::Int32, false),
            ArrowField::new(
                "detail",
                DataType::Union(union_fields, UnionMode::Dense),
                false,
            ),
        ]));
        RecordBatch::try_new(
            schema,
            vec![Arc::new(Int32Array::from(vec![0, 1])), Arc::new(union)],
        )
        .expect("build batch")
    }

    #[test]
    fn flatten_union_pulls_case_fields_to_top_level() {
        let out = prepare_output_batch(
            &flatten_union_batch(),
            &[flatten_field("detail")],
            &Format::Jsonl,
        )
        .unwrap();
        let sch = out.schema();
        let names: Vec<&str> = sch.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(
            names,
            vec!["id", "a", "b"],
            "case fields pulled up; `detail` gone"
        );
        let a = out.column_by_name("a").unwrap();
        let b = out.column_by_name("b").unwrap();
        assert!(a.is_valid(0) && !a.is_valid(1), "`a` only on the alpha row");
        assert!(!b.is_valid(0) && b.is_valid(1), "`b` only on the beta row");
    }

    /// The gate: the JSON writer must omit null keys so each row carries only its active
    /// case's fields. If this ever regressed, the spec's per-row-keys story would need a
    /// custom encoder.
    #[test]
    fn flatten_union_jsonl_emits_per_row_keys() {
        let out = prepare_output_batch(
            &flatten_union_batch(),
            &[flatten_field("detail")],
            &Format::Jsonl,
        )
        .unwrap();
        let mut buf = Vec::new();
        {
            let mut w = arrow::json::LineDelimitedWriter::new(&mut buf);
            w.write(&out).unwrap();
            w.finish().unwrap();
        }
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("\"a\"") && !lines[0].contains("\"b\""),
            "row 0 should carry only the alpha case's keys: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("\"b\"") && !lines[1].contains("\"a\""),
            "row 1 should carry only the beta case's keys: {}",
            lines[1]
        );
    }

    /// The Parquet superset path: flattened case fields become plain nullable top-level
    /// columns, written and read back through a real Parquet file.
    #[test]
    fn flatten_union_parquet_superset_round_trips() {
        let dir = std::env::temp_dir().join(format!("varunify_u2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_output(
            &flatten_union_batch(),
            "detail",
            &Format::Parquet,
            &dir,
            &[flatten_field("detail")],
        )
        .expect("flattened superset writes to parquet");

        let file = File::open(dir.join("detail.parquet")).unwrap();
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap();
        let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
        let _ = std::fs::remove_dir_all(&dir);

        let names: Vec<String> = batches[0]
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();
        assert_eq!(
            names,
            vec!["id", "a", "b"],
            "case fields are top-level columns"
        );
        let total: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn flatten_object_pulls_fields_to_top_level() {
        let id: ArrayRef = Arc::new(Int32Array::from(vec![0, 1]));
        let addr: ArrayRef = Arc::new(StructArray::from(vec![
            (
                Arc::new(ArrowField::new("street", DataType::Utf8, true)),
                Arc::new(StringArray::from(vec!["s1", "s2"])) as ArrayRef,
            ),
            (
                Arc::new(ArrowField::new("city", DataType::Utf8, true)),
                Arc::new(StringArray::from(vec!["c1", "c2"])) as ArrayRef,
            ),
        ]));
        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("id", DataType::Int32, false),
            ArrowField::new("addr", addr.data_type().clone(), true),
        ]));
        let batch = RecordBatch::try_new(schema, vec![id, addr]).unwrap();
        let out = prepare_output_batch(&batch, &[flatten_field("addr")], &Format::Jsonl).unwrap();
        let sch = out.schema();
        let names: Vec<&str> = sch.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, vec!["id", "street", "city"]);
    }

    /// `prefixed` namespaces colliding case fields by case label, so a Parquet superset of
    /// two cases that both carry `amount` becomes `alpha_amount` / `beta_amount`.
    #[test]
    fn flatten_union_prefixed_namespaces_colliding_fields() {
        let out = prepare_output_batch(
            &flatten_union_collision_batch(),
            &[flatten_field_strategy("detail", FlattenStrategy::Prefixed)],
            &Format::Parquet,
        )
        .unwrap();
        assert_eq!(col_names(&out), vec!["id", "alpha_amount", "beta_amount"]);
    }

    /// `discriminant` keeps the superset names and appends a `<field>_case` tag column naming
    /// the active case per row.
    #[test]
    fn flatten_union_discriminant_appends_case_tag() {
        let out = prepare_output_batch(
            &flatten_union_batch(),
            &[flatten_field_strategy(
                "detail",
                FlattenStrategy::Discriminant,
            )],
            &Format::Parquet,
        )
        .unwrap();
        assert_eq!(col_names(&out), vec!["id", "a", "b", "detail_case"]);
        let tag = out.column_by_name("detail_case").unwrap();
        let tag = tag.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(tag.value(0), "alpha");
        assert_eq!(tag.value(1), "beta");
    }

    /// JSON/JSONL ignore the strategy — per-row keys use raw names regardless of `prefixed`.
    #[test]
    fn flatten_union_jsonl_ignores_strategy() {
        let out = prepare_output_batch(
            &flatten_union_collision_batch(),
            &[flatten_field_strategy("detail", FlattenStrategy::Prefixed)],
            &Format::Jsonl,
        )
        .unwrap();
        // Raw (un-prefixed) names; the two `amount` columns coexist (one fires per row).
        assert_eq!(col_names(&out), vec!["id", "amount", "amount"]);
    }
}
