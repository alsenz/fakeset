use anyhow::{anyhow, Result};
use arrow::array::{ArrayRef, ListArray, StructArray, UInt32Array};
use arrow::buffer::OffsetBuffer;
use arrow::compute::{concat_batches, take};
use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::prelude::SessionContext;
use fake::Fake;
use parquet::arrow::ArrowWriter;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::constraints::FieldConstraints;
use crate::generator::{field_to_arrow, generate_column, is_rich_list, sample_count, schema_to_arrow};
use crate::models::{split_ref, CountSpec, Field, Format, Include, Range, Schema, SyntheticDataset};
use crate::plan::{ExecutionPlan, ExecutionStep, PrefillSource};
use crate::segment::{Segment, Sibling};

/// Execute the plan produced by `plan::build_plan`, writing outputs to `output_dir`.
///
/// Each step is interpreted in order with no branching on dataset shape:
/// row counts, sibling segments, and prefill wiring are all pre-resolved in
/// the plan.
pub async fn execute(plan: &ExecutionPlan, output_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;

    let ctx = SessionContext::new();
    let mut computed: HashMap<PathBuf, RecordBatch> = HashMap::new();
    let mut shared: HashMap<String, (Format, Vec<RecordBatch>)> = HashMap::new();

    for step in &plan.steps {
        match step {
            ExecutionStep::GenerateDataset { path, dataset, rows, prefills, skip_emit } => {
                let prefill_map = resolve_prefills(prefills, &computed);
                let batch = generate_batch(&dataset.data, *rows, &prefill_map, &HashMap::new())?;
                if *skip_emit {
                    // Scalar-only intermediate; AssembleRichList adds list columns and emits.
                    computed.insert(path.clone(), batch);
                } else {
                    let batch = evaluate_expressions(batch, dataset, &ctx).await?;
                    let output = filter_hidden_columns(batch.clone(), &dataset.data)?;
                    computed.insert(path.clone(), batch);
                    emit_batch(output, &dataset.name, &dataset.format, dataset.skip,
                        &dataset.output_file, &mut shared, output_dir)?;
                }
            }
            ExecutionStep::GenerateSiblingGroup {
                parent_path, parent, segments, siblings, skip_parent_emit,
            } => {
                execute_sibling_group(
                    parent_path, parent, segments, siblings, *skip_parent_emit,
                    &ctx, &mut computed, &mut shared, output_dir,
                ).await?;
            }
            ExecutionStep::GenerateInnerFlat {
                flat_key, outer_path, list_field_name,
                inner_fields, includes, count,
                include_path, include_distribution,
            } => {
                execute_inner_flat(
                    flat_key, outer_path, list_field_name,
                    inner_fields, includes, count,
                    include_path, *include_distribution,
                    &mut computed,
                )?;
            }
            ExecutionStep::AssembleRichList { outer_path, dataset, flat_specs } => {
                execute_assemble_rich_list(
                    outer_path, dataset, flat_specs,
                    &ctx, &mut computed, &mut shared, output_dir,
                ).await?;
            }
            ExecutionStep::WriteSharedOutput { output_file, format } => {
                let Some((_, batches)) = shared.get(output_file) else { continue };
                if !batches.is_empty() {
                    let combined = union_and_shuffle(batches.clone(), output_file, &ctx).await?;
                    write_output(&combined, output_file, format, output_dir)?;
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Prefill resolution
// ---------------------------------------------------------------------------

fn resolve_prefills(
    prefills: &[PrefillSource],
    computed: &HashMap<PathBuf, RecordBatch>,
) -> HashMap<String, Vec<ArrayRef>> {
    let mut map: HashMap<String, Vec<ArrayRef>> = HashMap::new();
    for ps in prefills {
        let Some(batch) = computed.get(&ps.from_path) else { continue };
        let Ok(col_idx) = batch.schema().index_of(&ps.from_column) else { continue };
        map.entry(ps.into_column.clone())
            .or_default()
            .push(batch.column(col_idx).clone());
    }
    map
}

// ---------------------------------------------------------------------------
// Sibling group execution
// ---------------------------------------------------------------------------

async fn execute_sibling_group(
    path: &Path,
    dataset: &SyntheticDataset,
    segments: &[Segment],
    siblings: &[Sibling],
    skip_parent_emit: bool,
    ctx: &SessionContext,
    computed: &mut HashMap<PathBuf, RecordBatch>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
    output_dir: &Path,
) -> Result<()> {
    let mut parent_batches: Vec<RecordBatch> = Vec::new();
    let mut sibling_buffers: HashMap<PathBuf, Vec<RecordBatch>> = HashMap::new();

    for seg in segments {
        if seg.rows == 0 {
            continue;
        }
        let parent_seg =
            generate_batch(&dataset.data, seg.rows, &HashMap::new(), &seg.field_constraints)?;
        parent_batches.push(parent_seg.clone());

        for sib_path in &seg.siblings {
            let sib = siblings.iter().find(|s| s.path == *sib_path).unwrap();
            sibling_buffers
                .entry(sib_path.clone())
                .or_default()
                .push(generate_sibling_batch(sib, &parent_seg)?);
        }
    }

    let parent_shuffled =
        combine_and_shuffle(parent_batches, &dataset.data, &dataset.name, ctx).await?;
    if skip_parent_emit {
        // Scalar-only intermediate; AssembleRichList adds list columns, evaluates
        // expressions, and emits.
        computed.insert(path.to_path_buf(), parent_shuffled);
    } else {
        let parent_shuffled = evaluate_expressions(parent_shuffled, dataset, ctx).await?;
        let parent_output = filter_hidden_columns(parent_shuffled.clone(), &dataset.data)?;
        computed.insert(path.to_path_buf(), parent_shuffled);
        emit_batch(parent_output, &dataset.name, &dataset.format, dataset.skip,
            &dataset.output_file, shared, output_dir)?;
    }

    for sib in siblings {
        let sib_shuffled = combine_and_shuffle(
            sibling_buffers.remove(&sib.path).unwrap_or_default(),
            &sib.dataset.data,
            &sib.dataset.name,
            ctx,
        ).await?;
        let sib_shuffled = evaluate_expressions(sib_shuffled, &sib.dataset, ctx).await?;
        let sib_output = filter_hidden_columns(sib_shuffled.clone(), &sib.dataset.data)?;
        computed.insert(sib.path.clone(), sib_shuffled);
        emit_batch(sib_output, &sib.dataset.name, &sib.dataset.format, sib.dataset.skip,
            &sib.dataset.output_file, shared, output_dir)?;
    }

    Ok(())
}

/// Generate a sibling's batch for one segment.
/// Fields that ref back to the parent are projected from `parent_seg`;
/// all other fields are generated fresh.
fn generate_sibling_batch(sibling: &Sibling, parent_seg: &RecordBatch) -> Result<RecordBatch> {
    let prefix = format!("{}.", sibling.include_ref);
    let arrow_schema = Arc::new(schema_to_arrow(&sibling.dataset.data));
    let n = parent_seg.num_rows();

    let columns = sibling
        .dataset
        .data
        .iter()
        .filter(|field| field.expression.is_none() && !is_rich_list(field))
        .map(|field| {
            if let Some(ref ref_str) = field.ref_field {
                if let Some(parent_field) = ref_str.strip_prefix(&prefix) {
                    if let Ok(col_idx) = parent_seg.schema().index_of(parent_field) {
                        return Ok(parent_seg.column(col_idx).clone());
                    }
                }
            }
            generate_column(field, n, &[])
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(RecordBatch::try_new(arrow_schema, columns)?)
}

// ---------------------------------------------------------------------------
// Rich list generation
// ---------------------------------------------------------------------------

/// Build the flat intermediate table for one rich list field.
///
/// Produces a `RecordBatch` with `_outer_idx: UInt32` (which outer row each
/// item belongs to) plus one column per inner field, stored in `computed[flat_key]`.
/// Include-scoped refs are sampled from the include batch; outer-scoped refs are
/// replicated from the outer batch; plain fields are generated fresh.
fn execute_inner_flat(
    flat_key: &PathBuf,
    outer_path: &PathBuf,
    list_field_name: &str,
    inner_fields: &[Field],
    includes: &[Include],
    count: &CountSpec,
    include_path: &PathBuf,
    include_distribution: Option<f64>,
    computed: &mut HashMap<PathBuf, RecordBatch>,
) -> Result<()> {
    let outer_batch = computed.get(outer_path).ok_or_else(|| {
        anyhow!("inner flat '{list_field_name}': outer batch not yet computed")
    })?.clone();
    let inc_batch = computed.get(include_path).ok_or_else(|| {
        anyhow!("inner flat '{list_field_name}': include batch not yet computed")
    })?.clone();

    let inc_rows = inc_batch.num_rows();
    let pool_size = match include_distribution {
        Some(d) => ((d * inc_rows as f64).round() as usize).min(inc_rows).max(1),
        None => inc_rows,
    };

    let n = outer_batch.num_rows();
    let counts: Vec<usize> = (0..n).map(|_| sample_count(count)).collect();
    let total: usize = counts.iter().sum();

    let outer_idxs: Vec<u32> = counts.iter().enumerate()
        .flat_map(|(i, &c)| std::iter::repeat(i as u32).take(c))
        .collect();
    let outer_idx_arr: ArrayRef = Arc::new(UInt32Array::from(outer_idxs.clone()));
    let sampled_indices: UInt32Array = (0..total)
        .map(|_| (0u64..pool_size as u64).fake::<u64>() as u32)
        .collect::<Vec<u32>>()
        .into();
    let rep_indices: UInt32Array = outer_idxs.into();

    let mut arrow_fields = vec![ArrowField::new("_outer_idx", DataType::UInt32, false)];
    let mut columns: Vec<ArrayRef> = vec![outer_idx_arr];

    for field in inner_fields {
        let col: ArrayRef = if let Some(ref ref_str) = field.ref_field {
            let is_include_scoped = split_ref(ref_str)
                .and_then(|(rp, _)| includes.iter().find(|i| i.reference == rp))
                .is_some();
            if is_include_scoped {
                let (_, target_col) = split_ref(ref_str).unwrap();
                let idx = inc_batch.schema().index_of(target_col)
                    .map_err(|_| anyhow!("column '{target_col}' not found in include batch"))?;
                take(inc_batch.column(idx).as_ref(), &sampled_indices, None)?
            } else {
                let idx = outer_batch.schema().index_of(ref_str.as_str())
                    .map_err(|_| anyhow!("outer-scoped column '{ref_str}' not found in outer batch"))?;
                take(outer_batch.column(idx).as_ref(), &rep_indices, None)?
            }
        } else {
            generate_column(field, total, &[])?
        };
        arrow_fields.push(field_to_arrow(field));
        columns.push(col);
    }

    let flat_batch = RecordBatch::try_new(Arc::new(ArrowSchema::new(arrow_fields)), columns)?;
    computed.insert(flat_key.clone(), flat_batch);
    Ok(())
}

/// Fold the inner flat tables produced by `execute_inner_flat` back into the
/// outer batch as `ListArray` columns, then evaluate expressions and emit.
async fn execute_assemble_rich_list(
    outer_path: &PathBuf,
    dataset: &SyntheticDataset,
    flat_specs: &[(String, PathBuf)],
    ctx: &SessionContext,
    computed: &mut HashMap<PathBuf, RecordBatch>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
    output_dir: &Path,
) -> Result<()> {
    let mut batch = computed.get(outer_path).ok_or_else(|| {
        anyhow!("assemble rich list '{}': outer batch not yet computed", dataset.name)
    })?.clone();

    for (field_name, flat_key) in flat_specs {
        let inner = computed.get(flat_key).ok_or_else(|| {
            anyhow!("assemble rich list '{}': inner flat for '{field_name}' not yet computed", dataset.name)
        })?.clone();

        let outer_n = batch.num_rows();
        let outer_idx_col = inner.schema().index_of("_outer_idx")
            .map_err(|_| anyhow!("inner flat missing '_outer_idx' column"))?;
        let outer_idx_arr = inner.column(outer_idx_col)
            .as_any().downcast_ref::<UInt32Array>()
            .ok_or_else(|| anyhow!("_outer_idx is not UInt32"))?;

        let mut counts = vec![0usize; outer_n];
        for &idx in outer_idx_arr.values() {
            counts[idx as usize] += 1;
        }

        let (struct_fields, struct_cols): (Vec<_>, Vec<_>) = (0..inner.num_columns())
            .filter(|&ci| inner.schema().field(ci).name() != "_outer_idx")
            .map(|ci| (
                Arc::new(inner.schema().field(ci).as_ref().clone()),
                inner.column(ci).clone(),
            ))
            .unzip();

        let child: ArrayRef = Arc::new(StructArray::new(
            struct_fields.into_iter().collect(),
            struct_cols,
            None,
        ));
        let item_field = Arc::new(ArrowField::new("item", child.data_type().clone(), true));
        let offsets = OffsetBuffer::<i32>::from_lengths(counts.iter().copied());
        let list_col: ArrayRef = Arc::new(ListArray::new(item_field, offsets, child, None));

        let list_arrow_field = ArrowField::new(field_name.as_str(), list_col.data_type().clone(), true);
        batch = add_column(batch, list_arrow_field, list_col)?;
    }

    let batch = evaluate_expressions(batch, dataset, ctx).await?;
    let output = filter_hidden_columns(batch.clone(), &dataset.data)?;
    computed.insert(outer_path.clone(), batch);
    emit_batch(output, &dataset.name, &dataset.format, dataset.skip,
        &dataset.output_file, shared, output_dir)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Batch generation
// ---------------------------------------------------------------------------

/// Generate a batch for `schema`. Fields in `overrides` have their constraints
/// replaced before generation; fields in `prefills` prepend pre-computed values.
fn generate_batch(
    schema: &Schema,
    rows: usize,
    prefills: &HashMap<String, Vec<ArrayRef>>,
    overrides: &HashMap<String, FieldConstraints>,
) -> Result<RecordBatch> {
    let arrow_schema = Arc::new(schema_to_arrow(schema));
    let columns = schema
        .iter()
        .filter(|f| f.expression.is_none() && !is_rich_list(f))
        .map(|f| {
            let prefix = prefills.get(&f.name).map_or(&[] as &[ArrayRef], |v| v.as_slice());
            let effective = overrides.get(&f.name).map(|fc| apply_constraints(f, fc));
            generate_column(effective.as_ref().unwrap_or(f), rows, prefix)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RecordBatch::try_new(arrow_schema, columns)?)
}

/// Return a copy of `field` with any non-None constraint from `fc` applied.
fn apply_constraints(field: &Field, fc: &FieldConstraints) -> Field {
    let mut f = field.clone();
    if fc.value.is_some()     { f.value     = fc.value.clone(); }
    if fc.generator.is_some() { f.generator = fc.generator.clone(); }
    if fc.min.is_some() || fc.max.is_some() {
        let r = f.range.get_or_insert(Range::default());
        if fc.min.is_some() { r.min = fc.min; }
        if fc.max.is_some() { r.max = fc.max; }
    }
    f
}

// ---------------------------------------------------------------------------
// Shuffling and emission
// ---------------------------------------------------------------------------

async fn combine_and_shuffle(
    batches: Vec<RecordBatch>,
    schema: &Schema,
    name: &str,
    ctx: &SessionContext,
) -> Result<RecordBatch> {
    if batches.is_empty() {
        return generate_batch(schema, 0, &HashMap::new(), &HashMap::new());
    }
    union_and_shuffle(batches, name, ctx).await
}

fn sql_safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Load all batches into a single MemTable (one partition each), then shuffle with SQL.
async fn union_and_shuffle(
    batches: Vec<RecordBatch>,
    name: &str,
    ctx: &SessionContext,
) -> Result<RecordBatch> {
    let safe = sql_safe_name(name);
    let tname = format!("_shuffle_{safe}");
    let schema = batches.first()
        .ok_or_else(|| anyhow!("union_and_shuffle: no batches for '{name}'"))?.schema();
    let partitions: Vec<Vec<RecordBatch>> = batches.into_iter().map(|b| vec![b]).collect();
    let table = MemTable::try_new(schema, partitions)?;
    ctx.deregister_table(&tname).ok();
    ctx.register_table(&tname, Arc::new(table))?;
    let result_batches = ctx.sql(&format!("SELECT * FROM {tname} ORDER BY random()"))
        .await?.collect().await?;
    ctx.deregister_table(&tname).ok();
    let schema = result_batches.first()
        .map(|b| b.schema())
        .ok_or_else(|| anyhow!("union_and_shuffle: no output for '{name}'"))?;
    Ok(concat_batches(&schema, &result_batches)?)
}

fn emit_batch(
    batch: RecordBatch,
    name: &str,
    format: &Format,
    skip: bool,
    output_file: &Option<String>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
    output_dir: &Path,
) -> Result<()> {
    if skip {
        return Ok(());
    }
    if let Some(of) = output_file {
        shared
            .entry(of.clone())
            .or_insert_with(|| (format.clone(), Vec::new()))
            .1
            .push(batch);
    } else {
        write_output(&batch, name, format, output_dir)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Batch column helpers
// ---------------------------------------------------------------------------

fn add_column(batch: RecordBatch, field: ArrowField, col: ArrayRef) -> Result<RecordBatch> {
    let mut fields: Vec<Arc<ArrowField>> = batch.schema().fields().to_vec();
    fields.push(Arc::new(field));
    let mut columns = batch.columns().to_vec();
    columns.push(col);
    Ok(RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), columns)?)
}

/// Evaluate all expression fields against the batch, building a CTE chain in
/// YAML order so each step can reference expression columns defined above it.
/// Returns the original batch augmented with new expression columns appended.
async fn evaluate_expressions(
    batch: RecordBatch,
    dataset: &SyntheticDataset,
    ctx: &SessionContext,
) -> Result<RecordBatch> {
    let expr_fields: Vec<_> = dataset.data.iter()
        .filter(|f| f.expression.is_some())
        .collect();

    if expr_fields.is_empty() {
        return Ok(batch);
    }

    let safe_name = sql_safe_name(&dataset.name);
    let src = format!("_src_{safe_name}");

    ctx.deregister_table(&src).ok();
    ctx.register_batch(&src, batch)?;

    let mut ctes = Vec::new();
    let mut prev = src.clone();
    for (i, field) in expr_fields.iter().enumerate() {
        let step = format!("_expr_{safe_name}_{i}");
        let expr = field.expression.as_ref().unwrap();
        ctes.push(format!(
            "{step} AS (SELECT *, {expr} AS \"{fname}\" FROM {prev})",
            fname = field.name
        ));
        prev = step;
    }

    let sql = format!("WITH {} SELECT * FROM {prev}", ctes.join(", "));
    let df = ctx.sql(&sql).await?;
    let batches = df.collect().await?;

    ctx.deregister_table(&src).ok();

    let schema = batches.first()
        .map(|b| b.schema())
        .ok_or_else(|| anyhow!("expression evaluation returned no rows"))?;
    Ok(concat_batches(&schema, &batches)?)
}

/// Remove columns marked `hidden` from a batch before writing output.
/// The full batch (including hidden columns) is kept in `computed` for prefill
/// wiring; only the filtered batch is written to output.
fn filter_hidden_columns(batch: RecordBatch, fields: &[Field]) -> Result<RecordBatch> {
    let hidden: HashSet<&str> = fields.iter()
        .filter(|f| f.hidden)
        .map(|f| f.name.as_str())
        .collect();

    if hidden.is_empty() {
        return Ok(batch);
    }

    let schema = batch.schema();
    let (kept_fields, kept_cols): (Vec<_>, Vec<_>) = schema
        .fields()
        .iter()
        .zip(batch.columns())
        .filter(|(f, _)| !hidden.contains(f.name().as_str()))
        .map(|(f, c)| (f.clone(), c.clone()))
        .unzip();

    Ok(RecordBatch::try_new(Arc::new(ArrowSchema::new(kept_fields)), kept_cols)?)
}

// ---------------------------------------------------------------------------
// Output writing
// ---------------------------------------------------------------------------

fn write_output(batch: &RecordBatch, name: &str, format: &Format, output_dir: &Path) -> Result<()> {
    let ext = match format {
        Format::Parquet => "parquet",
        Format::Csv     => "csv",
        Format::Json    => "json",
        Format::Jsonl   => "jsonl",
    };
    let path = output_dir.join(format!("{name}.{ext}"));
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
