use anyhow::{anyhow, Result};
use arrow::array::{ArrayRef, ListArray, StructArray, UInt32Array};
use arrow::buffer::OffsetBuffer;
use arrow::compute::{concat_batches, take};
use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use fake::Fake;
use parquet::arrow::ArrowWriter;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::constraints::FieldConstraints;
use crate::generator::{generate_column, sample_count};
use crate::schema::{field_to_arrow, schema_to_arrow};
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

    let mut computed: HashMap<PathBuf, RecordBatch> = HashMap::new();
    // Tracks datasets that were generated *as a parent* in their own GenerateSiblingGroup step.
    // Only these are eligible for reuse when they appear as a sibling in a later step.
    // Datasets generated *as siblings* are not reusable across separate variant groups.
    let mut parent_computed: HashSet<PathBuf> = HashSet::new();
    let mut shared: HashMap<String, (Format, Vec<RecordBatch>)> = HashMap::new();

    for step in &plan.steps {
        match step {
            ExecutionStep::GenerateDataset { path, dataset, rows, prefills, skip_emit } => {
                let prefill_map = resolve_prefills(prefills, &computed);
                let batch = generate_prefilled_batch(&dataset.data, *rows, &prefill_map)?;
                if *skip_emit {
                    // Scalar-only intermediate; AssembleRichList adds list columns and emits.
                    computed.insert(path.clone(), batch);
                } else {
                    let batch = evaluate_expressions(batch, dataset.as_ref()).await?;
                    let output = filter_hidden_columns(batch.clone(), &dataset.data).await?;
                    computed.insert(path.clone(), batch);
                    emit_batch(output, &dataset.format, &dataset.output_file, &mut shared)?;
                }
            }
            ExecutionStep::GenerateSiblingGroup {
                parent_path, parent, segments, siblings, skip_parent_emit,
            } => {
                execute_sibling_group(
                    parent_path, parent.as_ref(), segments, siblings, *skip_parent_emit,
                    &mut computed, &mut parent_computed, &mut shared,
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
                    outer_path, dataset.as_ref(), flat_specs,
                    &mut computed, &mut shared,
                ).await?;
            }
            ExecutionStep::WriteSharedOutput { output_file, format } => {
                let Some((_, batches)) = shared.get(output_file) else { continue };
                let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
                if total_rows > 0 {
                    let combined = union_and_shuffle(batches.clone(), output_file).await?;
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
    computed: &mut HashMap<PathBuf, RecordBatch>,
    parent_computed: &mut HashSet<PathBuf>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let pool_sibling_paths: HashSet<PathBuf> = siblings.iter()
        .filter(|s| s.is_pool)
        .map(|s| s.path.clone())
        .collect();
    let has_pool_siblings = !pool_sibling_paths.is_empty();

    // pool_parent_batches: rows from segments that contain at least one pool sibling.
    // These are placed before the shuffled non-pool rows so that GenerateInnerFlat's
    // distribution-based pool_size index correctly selects pool members.
    let mut pool_parent_batches: Vec<RecordBatch> = Vec::new();
    let mut nonpool_parent_batches: Vec<RecordBatch> = Vec::new();
    let mut sibling_buffers: HashMap<PathBuf, Vec<RecordBatch>> = HashMap::new();

    for seg in segments {
        if seg.rows == 0 {
            continue;
        }

        let seg_has_pool = seg.siblings.iter().any(|sp| pool_sibling_paths.contains(sp));

        if seg.siblings.is_empty() {
            // Parent-only segment: no child rows to inherit — generate fresh.
            let parent_seg = generate_fresh_batch(&dataset.data, seg.rows, &seg.field_constraints)?;
            nonpool_parent_batches.push(parent_seg);
        } else {
            // Pool siblings contribute constraints to the segment but produce no standalone
            // batches. Separate them from real (flat) siblings before generating children.
            let real_sib_paths: Vec<&PathBuf> = seg.siblings.iter()
                .filter(|sp| !pool_sibling_paths.contains(*sp))
                .collect();

            // Children are preceding: generate each real sibling first, then grow the
            // parent outward from those already-solved rows (UNION ALL semantics).
            //
            // If a sibling was itself a parent with its own sibling group, it is already
            // in `computed` — use that batch directly rather than regenerating it, and
            // suppress re-emission below.
            let mut child_batches: Vec<(&Sibling, RecordBatch)> = Vec::new();
            for sib_path in &real_sib_paths {
                let sib = siblings.iter().find(|s| &s.path == *sib_path).unwrap();
                if parent_computed.contains(&sib.path) {
                    // Generated as a parent in its own prior step — reuse that batch.
                    let precomputed = computed[&sib.path].clone();
                    child_batches.push((sib, precomputed));
                } else {
                    // Apply segment constraints to the sibling so that co-varying fields
                    // (e.g. status) receive the same constrained value as the parent.
                    let child_batch = generate_fresh_batch(
                        &sib.dataset.data, seg.rows, &seg.field_constraints,
                    )?;
                    sibling_buffers.entry(sib.path.clone()).or_default().push(child_batch.clone());
                    child_batches.push((sib, child_batch));
                }
            }

            let parent_seg = if child_batches.is_empty() {
                // Pool-only segment: all siblings are pool siblings; no real children.
                generate_fresh_batch(&dataset.data, seg.rows, &seg.field_constraints)?
            } else {
                grow_parent_from_children(
                    &dataset.data, &child_batches, &seg.field_constraints,
                ).await?
            };

            if seg_has_pool {
                pool_parent_batches.push(parent_seg);
            } else {
                nonpool_parent_batches.push(parent_seg);
            }
        }
    }

    // Pool-rows-first: pool members occupy the leading positions in the combined parent
    // batch so that GenerateInnerFlat's distribution-based pool_size index selects them.
    let parent_shuffled = if has_pool_siblings && !pool_parent_batches.is_empty() {
        combine_pool_first(pool_parent_batches, nonpool_parent_batches, &dataset.data, &dataset.name).await?
    } else {
        let mut all = pool_parent_batches;
        all.extend(nonpool_parent_batches);
        combine_and_shuffle(all, &dataset.data, &dataset.name).await?
    };

    if skip_parent_emit {
        // Scalar-only intermediate; AssembleRichList adds list columns, evaluates
        // expressions, and emits.
        computed.insert(path.to_path_buf(), parent_shuffled);
    } else {
        let parent_shuffled = evaluate_expressions(parent_shuffled, dataset).await?;
        let parent_output = filter_hidden_columns(parent_shuffled.clone(), &dataset.data).await?;
        computed.insert(path.to_path_buf(), parent_shuffled);
        emit_batch(parent_output, &dataset.format, &dataset.output_file, shared)?;
    }
    parent_computed.insert(path.to_path_buf());

    for sib in siblings {
        // Pool siblings have no standalone output — skip entirely.
        if sib.is_pool {
            continue;
        }
        // Siblings that were themselves parents in a prior step are already emitted; skip.
        if parent_computed.contains(&sib.path) {
            continue;
        }
        let sib_shuffled = combine_and_shuffle(
            sibling_buffers.remove(&sib.path).unwrap_or_default(),
            &sib.dataset.data,
            &sib.dataset.name,
        ).await?;
        let sib_shuffled = evaluate_expressions(sib_shuffled, &sib.dataset).await?;
        let sib_output = filter_hidden_columns(sib_shuffled.clone(), &sib.dataset.data).await?;
        computed.insert(sib.path.clone(), sib_shuffled);
        emit_batch(sib_output, &sib.dataset.format, &sib.dataset.output_file, shared)?;
    }

    Ok(())
}

/// Grow the parent batch for one segment from the already-generated child rows.
///
/// Expressed as a DataFusion JOIN: a skeleton batch (fresh-generated rule-3 columns +
/// `_row_idx`) is LEFT-JOINed with each child batch (also prepended with `_row_idx`).
/// The SELECT clause names exactly which source each parent field comes from.
///
/// Per parent field, first match across children wins:
/// 1. Child has `ref: <include_ref>.<parent_field>` → child column aliased to parent name.
/// 2. Child has a field of the same name as the parent field (no cross-schema ref) →
///    child column taken directly.
/// 3. Neither → generated fresh into the skeleton and pulled from there.
async fn grow_parent_from_children(
    parent_schema: &Schema,
    child_batches: &[(&Sibling, RecordBatch)],
    field_constraints: &HashMap<String, FieldConstraints>,
) -> Result<RecordBatch> {
    let n = child_batches.first()
        .expect("grow_parent_from_children requires non-empty child_batches")
        .1.num_rows();

    // Map parent field name → (child alias "c0"/"c1"/…, child column name).
    // or_insert preserves first-child-wins semantics.
    let mut sources: HashMap<String, (String, String)> = HashMap::new();
    for (ci, (sib, child_batch)) in child_batches.iter().enumerate() {
        let alias = format!("c{ci}");
        let prefix = format!("{}.", sib.reference);
        for child_field in &sib.dataset.data {
            if child_batch.schema().index_of(&child_field.name).is_err() { continue; }
            // Rule 1: cross-schema ref — child's ref points back to a parent field by name.
            if let Some(ref_str) = &child_field.ref_field {
                if let Some(parent_col) = ref_str.strip_prefix(&prefix) {
                    sources.entry(parent_col.to_string())
                        .or_insert_with(|| (alias.clone(), child_field.name.clone()));
                    continue;
                }
            }
            // Rule 2: same-name field, not a cross-ref pointing elsewhere.
            let is_cross_ref = child_field.ref_field.as_ref()
                .map_or(false, |r| r.starts_with(&prefix));
            if !is_cross_ref && parent_schema.iter().any(|pf| pf.name == child_field.name) {
                sources.entry(child_field.name.clone())
                    .or_insert_with(|| (alias.clone(), child_field.name.clone()));
            }
        }
    }

    // Active parent fields (skip expressions and rich-list placeholders).
    let active: Vec<&Field> = parent_schema.iter()
        .filter(|f| f.expression.is_none() && !f.is_rich_list())
        .collect();

    // Build skeleton batch: _row_idx column + all rule-3 (fresh) columns.
    let idx: ArrayRef = Arc::new(UInt32Array::from_iter_values(0..n as u32));
    let mut skel_fields = vec![ArrowField::new("_row_idx", DataType::UInt32, false)];
    let mut skel_cols: Vec<ArrayRef> = vec![idx];
    for f in &active {
        if sources.contains_key(f.name.as_str()) { continue; }
        let effective = field_constraints.get(f.name.as_str()).map(|fc| apply_constraints(f, fc));
        skel_cols.push(generate_column(effective.as_ref().unwrap_or(f), n, &[])?);
        skel_fields.push(field_to_arrow(f));
    }
    let skel = RecordBatch::try_new(Arc::new(ArrowSchema::new(skel_fields)), skel_cols)?;

    // Register all batches in a fresh context.
    let ctx = SessionContext::new();
    ctx.register_batch("skel", skel)?;
    for (ci, (_, child_batch)) in child_batches.iter().enumerate() {
        ctx.register_batch(&format!("c{ci}"), prepend_row_index(child_batch)?)?;
    }

    // SELECT clause: one expression per active parent field.
    let select: String = active.iter().map(|f| {
        if let Some((alias, child_col)) = sources.get(f.name.as_str()) {
            format!(r#"{alias}."{child_col}" AS "{}""#, f.name)
        } else {
            format!(r#"skel."{0}" AS "{0}""#, f.name)
        }
    }).collect::<Vec<_>>().join(", ");

    // LEFT JOIN chain on row index gives positional correspondence.
    let joins: String = (0..child_batches.len())
        .map(|ci| format!("LEFT JOIN c{ci} ON skel._row_idx = c{ci}._row_idx"))
        .collect::<Vec<_>>()
        .join(" ");

    let sql = if select.is_empty() {
        "SELECT * FROM skel".to_string()
    } else {
        format!("SELECT {select} FROM skel {joins}")
    };

    let batches = ctx.sql(&sql).await?.collect().await?;
    let schema = batches.first().map(|b| b.schema())
        .unwrap_or_else(|| Arc::new(schema_to_arrow(parent_schema)));
    Ok(concat_batches(&schema, &batches)?)
}

/// Prepend a `_row_idx` column (0..n) to a batch so it can be JOIN-keyed positionally.
fn prepend_row_index(batch: &RecordBatch) -> Result<RecordBatch> {
    let idx: ArrayRef = Arc::new(UInt32Array::from_iter_values(0..batch.num_rows() as u32));
    let mut fields: Vec<Arc<ArrowField>> =
        vec![Arc::new(ArrowField::new("_row_idx", DataType::UInt32, false))];
    fields.extend(batch.schema().fields().iter().cloned());
    let mut cols: Vec<ArrayRef> = vec![idx];
    cols.extend(batch.columns().iter().cloned());
    Ok(RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), cols)?)
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
    computed: &mut HashMap<PathBuf, RecordBatch>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
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

    let batch = evaluate_expressions(batch, dataset).await?;
    let output = filter_hidden_columns(batch.clone(), &dataset.data).await?;
    computed.insert(outer_path.clone(), batch);
    emit_batch(output, &dataset.format, &dataset.output_file, shared)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Batch generation
// ---------------------------------------------------------------------------

fn generate_fresh_batch(
    schema: &Schema,
    rows: usize,
    overrides: &HashMap<String, FieldConstraints>,
) -> Result<RecordBatch> {
    generate_batch(schema, rows, &HashMap::new(), overrides)
}

fn generate_prefilled_batch(
    schema: &Schema,
    rows: usize,
    prefills: &HashMap<String, Vec<ArrayRef>>,
) -> Result<RecordBatch> {
    generate_batch(schema, rows, prefills, &HashMap::new())
}

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
        .filter(|f| f.expression.is_none() && !f.is_rich_list())
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

async fn combine_and_shuffle(batches: Vec<RecordBatch>, schema: &Schema, name: &str) -> Result<RecordBatch> {
    if batches.is_empty() {
        return generate_batch(schema, 0, &HashMap::new(), &HashMap::new());
    }
    union_and_shuffle(batches, name).await
}

/// Concatenate all batches and shuffle via DataFusion `ORDER BY random()`.
/// Zero-row inputs are returned immediately to avoid DataFusion empty-result issues.
async fn union_and_shuffle(batches: Vec<RecordBatch>, name: &str) -> Result<RecordBatch> {
    let arrow_schema = batches.first()
        .ok_or_else(|| anyhow!("union_and_shuffle: no batches for '{name}'"))?.schema();
    let combined = concat_batches(&arrow_schema, &batches)?;
    if combined.num_rows() == 0 {
        return Ok(combined);
    }
    let ctx = SessionContext::new();
    let df = ctx.read_batch(combined)?;
    let shuffled = df.sort(vec![datafusion::functions::expr_fn::random().sort(true, true)])?
        .collect().await?;
    let schema = shuffled.first().map(|b| b.schema()).unwrap_or(arrow_schema);
    Ok(concat_batches(&schema, &shuffled)?)
}

/// Prepend pool rows (unshuffled) before shuffled non-pool rows. Pool rows must appear
/// first so that GenerateInnerFlat's pool_size index correctly identifies them.
async fn combine_pool_first(
    pool_batches: Vec<RecordBatch>,
    nonpool_batches: Vec<RecordBatch>,
    schema: &Schema,
    name: &str,
) -> Result<RecordBatch> {
    let shuffled_nonpool = combine_and_shuffle(nonpool_batches, schema, name).await?;
    let arrow_schema = Arc::new(schema_to_arrow(schema));
    let pool_combined = concat_batches(&arrow_schema, &pool_batches)?;
    Ok(concat_batches(&pool_combined.schema(), &[pool_combined, shuffled_nonpool])?)
}

fn emit_batch(
    batch: RecordBatch,
    format: &Format,
    output_file: &Option<String>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let Some(of) = output_file else { return Ok(()) };
    shared
        .entry(of.clone())
        .or_insert_with(|| (format.clone(), Vec::new()))
        .1
        .push(batch);
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
) -> Result<RecordBatch> {
    let expr_fields: Vec<_> = dataset.data.iter()
        .filter(|f| f.expression.is_some())
        .collect();

    if expr_fields.is_empty() {
        return Ok(batch);
    }

    // Fresh context per call — table name "src" is stable and the context is dropped
    // at function exit, so there is no registration lifecycle to manage.
    let ctx = SessionContext::new();
    ctx.register_batch("src", batch)?;

    let mut ctes = Vec::new();
    let mut prev = "src".to_string();
    for (i, field) in expr_fields.iter().enumerate() {
        let step = format!("step{i}");
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

    let schema = batches.first()
        .map(|b| b.schema())
        .ok_or_else(|| anyhow!("expression evaluation returned no rows"))?;
    Ok(concat_batches(&schema, &batches)?)
}

/// Remove columns marked `hidden` from a batch before writing output.
/// The full batch (including hidden columns) is kept in `computed` for prefill
/// wiring; only the filtered batch is written to output.
async fn filter_hidden_columns(batch: RecordBatch, fields: &[Field]) -> Result<RecordBatch> {
    if !fields.iter().any(|f| f.hidden) {
        return Ok(batch);
    }

    let hidden: HashSet<&str> = fields.iter()
        .filter(|f| f.hidden)
        .map(|f| f.name.as_str())
        .collect();

    let ctx = SessionContext::new();
    let df = ctx.read_batch(batch)?;
    let visible: Vec<datafusion::prelude::Expr> = df.schema()
        .fields()
        .iter()
        .filter(|af| !hidden.contains(af.name().as_str()))
        .map(|af| datafusion::prelude::col(af.name()))
        .collect();

    let batches = df.select(visible)?.collect().await?;
    let schema = batches.first().map(|b| b.schema())
        .unwrap_or_else(|| Arc::new(ArrowSchema::empty()));
    Ok(concat_batches(&schema, &batches)?)
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
