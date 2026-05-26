//! Execution engine. Interprets the `ExecutionPlan` produced by `plan::build_plan`:
//! generates staging nodes, witness nodes (atoms in the semi-lattice), and assembly
//! nodes; accumulates witness values back into linked datasets via `AccumulateToLinked`;
//! and writes output files.
//!
//! ## Sentinel column lifecycle
//!
//! Several hidden columns carry positional bookkeeping across steps. They are produced and
//! consumed as follows:
//!
//! | Column | Type | Produced by | Consumed by | Stripped by |
//! |--------|------|-------------|-------------|-------------|
//! | `_row_idx` | `UInt32` | `grow_parent_from_children` | same (JOIN key) | same (never leaves the function) |
//! | `_slot_idx` | `UInt32` | `execute_lower_cover_group_core` (member batches) | `AssembleFromWitness` (fold into lists) | `strip_slot_idx` before member emit |
//! | `_staging_refs` | `List<UInt32>` | `execute_witness` | `execute_assemble_from_witness` (fold) | stripped during assembly |
//! | `_linked_idx` | `UInt32` | `inject_linked_idx` (junction) / `execute_witness` | `execute_accumulate_to_linked` | `strip_linked_idx` before junction emit |
use anyhow::{anyhow, bail, Result};
use arrow::array::{Array, ArrayRef, Float64Array, ListArray, StringBuilder, StringArray, StructArray, UInt32Array, new_empty_array};
use arrow::buffer::{OffsetBuffer, ScalarBuffer};
use arrow::compute::{concat, concat_batches, sort_to_indices, take};
use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use serde_yaml::Value as YamlValue;
use datafusion::functions_aggregate::expr_fn::{
    array_agg,
    first_value as df_first_value,
    max as df_max,
    min as df_min,
    sum as df_sum,
};
use datafusion::prelude::{col, SessionContext};
use fake::Fake;
use parquet::arrow::ArrowWriter;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::constraints::FieldConstraints;
use crate::generator::{generate_column, sample_count};
use crate::schema::{field_to_arrow, schema_to_arrow};
use crate::models::{eligible_linked_rows, resolve_include, split_ref, CountSpec, Field, Format, Include, Range, Reducer, Schema, SyntheticDataset};
use crate::plan::{ExecutionPlan, ExecutionStep, InheritedField};
use crate::segment::{LowerCoverMember, Segment};

/// Execute the plan produced by `plan::build_plan`, writing outputs to `output_dir`.
///
/// Each step is interpreted in order with no branching on dataset shape:
/// row counts, lower cover segments, and inherited field wiring are all pre-resolved
/// in the plan.
pub async fn execute(plan: &ExecutionPlan, output_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;

    let mut computed: HashMap<PathBuf, RecordBatch> = HashMap::new();
    // Tracks datasets that were generated *as a parent* in their own GenerateLowerCoverGroup step.
    // Only these are eligible for reuse when they appear as a lower cover member in a later step.
    // Datasets generated *as lower cover members* are not reusable across separate variant groups.
    let mut parent_computed: HashSet<PathBuf> = HashSet::new();
    let mut shared: HashMap<String, (Format, Vec<RecordBatch>)> = HashMap::new();
    // Tracks which (linked_path, linked_field) pairs have had their first AccumulateToLinked call.
    // The first call always replaces generated values; subsequent calls accumulate cumulatively.
    let mut accumulated_fields: HashSet<(PathBuf, String)> = HashSet::new();

    for step in &plan.steps {
        match step {
            // --- Push-down phase: generate datasets and staging nodes (scalar fields only
            //     for nodes with list-links), fan rows to lower cover members. ---
            ExecutionStep::GenerateStagingNode { path, dataset, rows, inherited } => {
                execute_dataset_core(
                    true, false, path, dataset.as_ref(), *rows, inherited,
                    &mut computed, &mut shared,
                ).await?;
            }
            ExecutionStep::GenerateDataset { path, dataset, rows, inherited, defer_emit } => {
                execute_dataset_core(
                    false, *defer_emit, path, dataset.as_ref(), *rows, inherited,
                    &mut computed, &mut shared,
                ).await?;
            }
            ExecutionStep::GenerateStagingLowerCoverGroup { parent_path, parent, segments, members } => {
                execute_lower_cover_group_core(
                    true, false, parent_path, parent.as_ref(), segments, members,
                    &mut computed, &mut parent_computed, &mut shared,
                ).await?;
            }
            ExecutionStep::GenerateLowerCoverGroup { parent_path, parent, segments, members, defer_emit } => {
                execute_lower_cover_group_core(
                    false, *defer_emit, parent_path, parent.as_ref(), segments, members,
                    &mut computed, &mut parent_computed, &mut shared,
                ).await?;
            }
            ExecutionStep::GenerateWitness {
                witness_key, staging_path, list_field_name,
                inner_fields, include, cardinality,
                linked_path, slot_start, slot_count, segment_constraints,
            } => {
                execute_witness(
                    witness_key, staging_path, list_field_name,
                    inner_fields, include, cardinality,
                    linked_path, *slot_start, *slot_count, segment_constraints,
                    &mut computed,
                )?;
            }
            // --- Accumulate-up phase: fold witness rows into list columns, propagate
            //     collected values back to linked datasets, emit final output files. ---
            ExecutionStep::AssembleFromWitness { staging_path, dataset, witness_specs } => {
                execute_assemble_from_witness(
                    staging_path, dataset.as_ref(), witness_specs,
                    &mut computed, &mut shared,
                ).await?;
            }
            ExecutionStep::AccumulateToLinked {
                source_path, source_field, linked_path, linked_field, group_by, reducer, default_val,
            } => {
                execute_accumulate_to_linked(
                    source_path, source_field, linked_path, linked_field, group_by, reducer, default_val,
                    &mut computed,
                    &mut accumulated_fields,
                ).await?;
            }
            ExecutionStep::EmitDataset { path, dataset } => {
                execute_emit_dataset(path, dataset.as_ref(), &computed, &mut shared).await?;
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
// Inherited field resolution
// ---------------------------------------------------------------------------

fn resolve_inherited_fields(
    inherited: &[InheritedField],
    computed: &HashMap<PathBuf, RecordBatch>,
) -> HashMap<String, Vec<ArrayRef>> {
    let mut map: HashMap<String, Vec<ArrayRef>> = HashMap::new();
    for ps in inherited {
        let Some(batch) = computed.get(&ps.from_path) else { continue };
        let Ok(col_idx) = batch.schema().index_of(&ps.from_column) else { continue };
        map.entry(ps.into_column.clone())
            .or_default()
            .push(batch.column(col_idx).clone());
    }
    map
}

// ---------------------------------------------------------------------------
// Lower cover group execution
// ---------------------------------------------------------------------------

/// Shared core for `GenerateStagingNode` (`is_staging=true`) and `GenerateDataset`
/// (`is_staging=false`). When staging, stores the scalar batch in `computed` with no
/// expression evaluation or emit. When not staging, evaluates expressions, handles
/// `_linked_idx` injection for junction links, and either emits or defers based on
/// `defer_emit` (collect-target deferral).
async fn execute_dataset_core(
    is_staging: bool,
    defer_emit: bool,
    path: &Path,
    dataset: &SyntheticDataset,
    rows: usize,
    inherited: &[InheritedField],
    computed: &mut HashMap<PathBuf, RecordBatch>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let inherited_map = resolve_inherited_fields(inherited, computed);
    let batch = generate_with_inherited(&dataset.data, rows, &inherited_map)?;
    if is_staging {
        // Scalar-only intermediate; AssembleFromWitness adds list columns, evaluates
        // expressions, and emits.
        computed.insert(path.to_path_buf(), batch);
    } else {
        // Evaluate expressions for both normal emit and collect-target deferral.
        let batch = evaluate_expressions(batch, dataset).await?;
        // Inject _linked_idx for junction datasets (link without include).
        // The full batch (with _linked_idx) is kept in `computed` for AccumulateToLinked;
        // _linked_idx is stripped before emitting.
        let batch = inject_linked_idx(&batch, path, dataset, computed)?;
        let output = filter_hidden_columns(
            strip_linked_idx(batch.clone()),
            &dataset.data,
        ).await?;
        computed.insert(path.to_path_buf(), batch);
        if !defer_emit {
            emit_batch(output, &dataset.format, &dataset.output_file, shared)?;
        }
        // When defer_emit is true (collect target): batch stored, file write deferred
        // to the EmitDataset step that follows AccumulateToLinked.
    }
    Ok(())
}

/// Shared core for `GenerateStagingLowerCoverGroup` (`is_staging=true`) and
/// `GenerateLowerCoverGroup` (`is_staging=false`).
async fn execute_lower_cover_group_core(
    is_staging: bool,
    defer_emit: bool,
    path: &Path,
    dataset: &SyntheticDataset,
    segments: &[Segment],
    members: &[LowerCoverMember],
    computed: &mut HashMap<PathBuf, RecordBatch>,
    parent_computed: &mut HashSet<PathBuf>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let witness_source_paths: HashSet<PathBuf> = members.iter()
        .filter(|m| m.is_witness_source)
        .map(|m| m.path.clone())
        .collect();
    let has_witness_sources = !witness_source_paths.is_empty();

    // witness_source_parent_batches: rows from segments that contain at least one witness-source member.
    // These are placed before the shuffled non-witness-source rows so that GenerateWitness's
    // n_eligible_slots boundary correctly identifies linked-member rows.
    let mut witness_source_parent_batches: Vec<RecordBatch> = Vec::new();
    let mut non_witness_source_parent_batches: Vec<RecordBatch> = Vec::new();
    // For staging nodes: collect batches in segment declaration order (no shuffle).
    // Row order determines slot indices used by per-segment GenerateWitness.
    let mut ordered_staging_batches: Vec<RecordBatch> = Vec::new();
    let mut member_buffers: HashMap<PathBuf, Vec<RecordBatch>> = HashMap::new();
    let mut slot_offset: usize = 0;

    for seg in segments {
        if seg.rows == 0 {
            continue;
        }

        let seg_has_witness_source = seg.members.iter().any(|mp| witness_source_paths.contains(mp));

        if seg.members.is_empty() {
            // Parent-only segment: no child rows to inherit — generate fresh.
            let parent_seg = generate_fresh_batch(&dataset.data, seg.rows, &seg.field_constraints)?;
            if is_staging {
                ordered_staging_batches.push(parent_seg);
            } else {
                non_witness_source_parent_batches.push(parent_seg);
            }
        } else {
            // Witness-source members contribute constraints but produce no standalone batches.
            // Separate them from real (flat) members before generating children.
            let real_member_paths: Vec<&PathBuf> = seg.members.iter()
                .filter(|mp| !witness_source_paths.contains(*mp))
                .collect();

            // Children are preceding: generate each real member first, then grow the
            // parent outward from those already-solved rows (UNION ALL semantics).
            //
            // If a member was itself a parent with its own lower cover group, it is already
            // in `computed` — use that batch directly rather than regenerating it, and
            // suppress re-emission below.
            let mut child_batches: Vec<(&LowerCoverMember, RecordBatch)> = Vec::new();
            for member_path in &real_member_paths {
                let m = members.iter().find(|m| &m.path == *member_path).unwrap();
                if parent_computed.contains(&m.path) {
                    if let Some(ref card) = m.cardinality {
                        // The precomputed batch has total_member_rows rows, but
                        // grow_parent_from_children expects seg.rows canonical rows (one per
                        // parent slot). Generate a fresh canonical batch for assembly and an
                        // expanded batch tagged with _slot_idx so the lattice position is
                        // recorded in computed after all segments are processed.
                        let canonical = generate_fresh_batch(
                            &m.dataset.data, seg.rows, &seg.field_constraints,
                        )?;
                        let expanded = generate_expanded_batch(
                            &m.dataset.data, seg.rows, &seg.field_constraints, card, slot_offset,
                        )?;
                        member_buffers.entry(m.path.clone()).or_default().push(expanded);
                        child_batches.push((m, canonical));
                    } else {
                        // No cardinality: precomputed row count matches seg.rows.
                        let precomputed = computed[&m.path].clone();
                        child_batches.push((m, precomputed));
                    }
                } else {
                    // Canonical batch: one row per slot, used for parent assembly.
                    let canonical = generate_fresh_batch(
                        &m.dataset.data, seg.rows, &seg.field_constraints,
                    )?;
                    if let Some(ref card) = m.cardinality {
                        // Expanded batch: M_n rows per slot, tagged with _slot_idx for output.
                        let expanded = generate_expanded_batch(
                            &m.dataset.data, seg.rows, &seg.field_constraints, card, slot_offset,
                        )?;
                        member_buffers.entry(m.path.clone()).or_default().push(expanded);
                        child_batches.push((m, canonical));
                    } else {
                        member_buffers.entry(m.path.clone()).or_default().push(canonical.clone());
                        child_batches.push((m, canonical));
                    }
                }
            }

            let parent_seg = if child_batches.is_empty() {
                // Witness-source-only segment: all members are witness sources; no real children.
                generate_fresh_batch(&dataset.data, seg.rows, &seg.field_constraints)?
            } else {
                grow_parent_from_children(
                    &dataset.data, &child_batches, &seg.field_constraints,
                ).await?
            };

            if is_staging {
                ordered_staging_batches.push(parent_seg);
            } else if seg_has_witness_source {
                witness_source_parent_batches.push(parent_seg);
            } else {
                non_witness_source_parent_batches.push(parent_seg);
            }
        }
        slot_offset += seg.rows;
    }

    // For staging nodes: concatenate in segment declaration order (no shuffle).
    // For non-staging: witness-source rows first, then shuffled remainder.
    let parent_shuffled = if is_staging {
        // Row order determines slot indices used by per-segment GenerateWitness steps.
        let arrow_schema = ordered_staging_batches.first()
            .map(|b| b.schema())
            .unwrap_or_else(|| Arc::new(schema_to_arrow(&dataset.data)));
        concat_batches(&arrow_schema, &ordered_staging_batches)?
    } else if has_witness_sources && !witness_source_parent_batches.is_empty() {
        combine_witness_source_first(witness_source_parent_batches, non_witness_source_parent_batches, &dataset.data, &dataset.name).await?
    } else {
        let mut all = witness_source_parent_batches;
        all.extend(non_witness_source_parent_batches);
        combine_and_shuffle(all, &dataset.data, &dataset.name).await?
    };

    if is_staging {
        // Scalar-only intermediate; AssembleFromWitness adds list columns, evaluates
        // expressions, and emits.
        computed.insert(path.to_path_buf(), parent_shuffled);
    } else {
        let parent_shuffled = evaluate_expressions(parent_shuffled, dataset).await?;
        let parent_output = filter_hidden_columns(parent_shuffled.clone(), &dataset.data).await?;
        computed.insert(path.to_path_buf(), parent_shuffled);
        if !defer_emit {
            emit_batch(parent_output, &dataset.format, &dataset.output_file, shared)?;
        }
        // When defer_emit is true (collect target): batch stored, file write deferred
        // to the EmitDataset step that follows AccumulateToLinked.
    }
    parent_computed.insert(path.to_path_buf());

    for m in members {
        // Witness-source members have no standalone output — skip entirely.
        if m.is_witness_source {
            continue;
        }
        // Members that were themselves parents in a prior step are already emitted.
        // If the member had cardinality we accumulated an expanded+_slot_idx batch in
        // member_buffers during the segment loop — store it in computed so downstream
        // steps (collect bindings, linked sampling) see its lattice position.
        if parent_computed.contains(&m.path) {
            if m.cardinality.is_some() {
                let buffers = member_buffers.remove(&m.path).unwrap_or_default();
                if !buffers.is_empty() {
                    let m_schema = buffers[0].schema();
                    let tagged = concat_batches(&m_schema, &buffers)?;
                    computed.insert(m.path.clone(), tagged);
                }
            }
            continue;
        }
        let m_shuffled = combine_and_shuffle(
            member_buffers.remove(&m.path).unwrap_or_default(),
            &m.dataset.data,
            &m.dataset.name,
        ).await?;
        let m_shuffled = evaluate_expressions(m_shuffled, &m.dataset).await?;
        // For junction link members: sample one linked row per row and prepend as _linked_idx.
        // The linked batch must already be in `computed` (DAG link-edge ordering guarantees this).
        let m_shuffled = inject_linked_idx(&m_shuffled, &m.path, &m.dataset, computed)?;
        let m_output = filter_hidden_columns(
            strip_linked_idx(strip_slot_idx(m_shuffled.clone())),
            &m.dataset.data,
        ).await?;
        computed.insert(m.path.clone(), m_shuffled);
        emit_batch(m_output, &m.dataset.format, &m.dataset.output_file, shared)?;
    }

    Ok(())
}

/// Accumulate-up step for include relationships: inherit child column values into parent rows.
///
/// In semi-lattice terms, this is the upward propagation step — child (more-constrained)
/// values flow into the parent (less-constrained) batch so they are never re-generated.
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
    child_batches: &[(&LowerCoverMember, RecordBatch)],
    field_constraints: &HashMap<String, FieldConstraints>,
) -> Result<RecordBatch> {
    let n = child_batches.first()
        .expect("grow_parent_from_children requires non-empty child_batches")
        .1.num_rows();

    // Map parent field name → (child alias "c0"/"c1"/…, child column name).
    // or_insert preserves first-child-wins semantics.
    let mut sources: HashMap<String, (String, String)> = HashMap::new();
    for (ci, (m, child_batch)) in child_batches.iter().enumerate() {
        let alias = format!("c{ci}");
        let prefix = format!("{}.", m.reference);
        for child_field in &m.dataset.data {
            if child_batch.schema().index_of(&child_field.name).is_err() { continue; }
            // Rule 1: cross-schema ref — child's ref points back to a parent field by name.
            if let Some(ref_str) = child_field.simple_ref() {
                if let Some(parent_col) = ref_str.strip_prefix(prefix.as_str()) {
                    sources.entry(parent_col.to_string())
                        .or_insert_with(|| (alias.clone(), child_field.name.clone()));
                    continue;
                }
            }
            // Rule 2: same-name field, not a cross-ref pointing elsewhere.
            let is_cross_ref = child_field.simple_ref()
                .map_or(false, |r| r.starts_with(prefix.as_str()));
            if !is_cross_ref && parent_schema.iter().any(|pf| pf.name == child_field.name) {
                sources.entry(child_field.name.clone())
                    .or_insert_with(|| (alias.clone(), child_field.name.clone()));
            }
        }
    }

    // Active parent fields (skip expressions and nested-include placeholders).
    let active: Vec<&Field> = parent_schema.iter()
        .filter(|f| f.expression.is_none() && !f.is_list_link())
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
    prepend_column(batch, "_row_idx", idx)
}

fn prepend_column(batch: &RecordBatch, name: &str, col: ArrayRef) -> Result<RecordBatch> {
    let mut fields: Vec<Arc<ArrowField>> =
        vec![Arc::new(ArrowField::new(name, col.data_type().clone(), false))];
    fields.extend(batch.schema().fields().iter().cloned());
    let mut cols: Vec<ArrayRef> = vec![col];
    cols.extend(batch.columns().iter().cloned());
    Ok(RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), cols)?)
}

// ---------------------------------------------------------------------------
// Witness generation
// ---------------------------------------------------------------------------

/// Filter a batch to rows where every constrained field satisfies its `FieldConstraints`.
/// Returns `(filtered_batch, surviving_row_indices)` — the surviving indices are positions
/// in the original (unfiltered) batch, enabling callers to map back to original indices.
fn filter_batch_by_constraints(
    batch: &RecordBatch,
    constraints: &HashMap<String, FieldConstraints>,
) -> Result<(RecordBatch, Vec<u32>)> {
    if constraints.is_empty() {
        let surviving: Vec<u32> = (0..batch.num_rows() as u32).collect();
        return Ok((batch.clone(), surviving));
    }

    let mut mask: Vec<bool> = vec![true; batch.num_rows()];
    for (field_name, fc) in constraints {
        let Ok(col_idx) = batch.schema().index_of(field_name) else { continue };
        let col = batch.column(col_idx);
        for row in 0..batch.num_rows() {
            if mask[row] {
                mask[row] = row_satisfies_field_constraints(col, row, fc);
            }
        }
    }

    let surviving: Vec<u32> = (0..batch.num_rows() as u32)
        .filter(|&i| mask[i as usize])
        .collect();
    let indices = UInt32Array::from(surviving.clone());
    let fields: Vec<Arc<ArrowField>> = batch.schema().fields().to_vec();
    let cols: Vec<ArrayRef> = batch.columns().iter()
        .map(|c| take(c.as_ref(), &indices, None))
        .collect::<std::result::Result<_, _>>()?;
    let filtered = RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), cols)?;
    Ok((filtered, surviving))
}

/// Check whether the value at `row` in `col` satisfies the given `FieldConstraints`.
fn row_satisfies_field_constraints(col: &ArrayRef, row: usize, fc: &FieldConstraints) -> bool {
    use arrow::array::{BooleanArray, StringArray};

    if let Some(ref val) = fc.value {
        match val {
            YamlValue::String(expected) => {
                if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                    if !col.is_null(row) && arr.value(row) != expected.as_str() {
                        return false;
                    }
                }
            }
            YamlValue::Number(n) => {
                if let Some(expected) = n.as_f64() {
                    if let Some(arr) = col.as_any().downcast_ref::<Float64Array>() {
                        if !col.is_null(row) && (arr.value(row) - expected).abs() > 1e-9 {
                            return false;
                        }
                    }
                }
            }
            YamlValue::Bool(expected) => {
                if let Some(arr) = col.as_any().downcast_ref::<BooleanArray>() {
                    if !col.is_null(row) && arr.value(row) != *expected {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(arr) = col.as_any().downcast_ref::<Float64Array>() {
        let v = arr.value(row);
        if let Some(min) = fc.min {
            if v < min { return false; }
        }
        if let Some(max) = fc.max {
            if v > max { return false; }
        }
    }

    true
}

/// Deduplicate raw draw assignments into the `_linked_idx` / `_staging_refs` structure.
///
/// Given the flat draw results from Phase 1 of witness generation, maps each draw back
/// to its original eligible-linked index (`surviving_indices` reverses the constraint
/// filter), groups staging slots by linked row, and builds the `_staging_refs` ListArray
/// (one entry per unique linked row, containing the staging slot indices that drew it).
///
/// Returns `(unique_linked_idxs, staging_refs_array)`:
/// - `unique_linked_idxs`: sorted unique eligible-linked-batch row indices (keys for `take`)
/// - `staging_refs_array`: `List<UInt32>` — entry i = all staging slots that drew linked row i
fn build_witness_dedup(
    slot_assignments: &UInt32Array,
    staging_idxs: &[u32],
    surviving_indices: &[u32],
    total: usize,
) -> (Vec<u32>, ListArray) {
    // Map filtered index → original eligible_linked index → _linked_idx value,
    // accumulating staging slots per linked row. BTreeMap keeps deterministic order.
    let mut draw_map: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for k in 0..total {
        let original_linked_idx = surviving_indices[slot_assignments.value(k) as usize];
        draw_map
            .entry(original_linked_idx)
            .or_default()
            .push(staging_idxs[k]);
    }
    let unique_linked_idxs: Vec<u32> = draw_map.keys().copied().collect();

    let mut refs_offsets: Vec<i32> = vec![0];
    let mut refs_values: Vec<u32> = Vec::new();
    for &linked_idx in &unique_linked_idxs {
        refs_values.extend_from_slice(&draw_map[&linked_idx]);
        refs_offsets.push(refs_values.len() as i32);
    }
    let staging_refs_array = ListArray::new(
        Arc::new(ArrowField::new("item", DataType::UInt32, false)),
        OffsetBuffer::new(ScalarBuffer::from(refs_offsets)),
        Arc::new(UInt32Array::from(refs_values)),
        None,
    );
    (unique_linked_idxs, staging_refs_array)
}

/// Generate the witness batch for one list-link field.
///
/// One witness row per **unique** linked-row draw. Column sources:
///   - `_linked_idx` (hidden scalar): which linked-batch row this witness row represents
///   - `_staging_refs` (hidden list): all staging slot indices that drew this linked row
///   - linked-scoped refs: value taken from the linked batch (same for every draw)
///   - plain fields: generated once per unique linked row
///   - outer-scoped refs: **not stored**; resolved from the staging batch at assembly time
fn execute_witness(
    witness_key: &PathBuf,
    staging_path: &PathBuf,
    list_field_name: &str,
    inner_fields: &[Field],
    include: &Include,
    cardinality: &CountSpec,
    linked_path: &PathBuf,
    slot_start: usize,
    slot_count: usize,
    segment_constraints: &HashMap<String, FieldConstraints>,
    computed: &mut HashMap<PathBuf, RecordBatch>,
) -> Result<()> {
    if !computed.contains_key(staging_path) {
        return Err(anyhow!("witness '{list_field_name}': staging batch not yet computed"));
    }
    let linked_batch = computed.get(linked_path).ok_or_else(|| {
        anyhow!("witness '{list_field_name}': linked-slot batch not yet computed")
    })?.clone();

    // --- Phase 1: assign each draw to a staging slot and a linked slot ---
    // Eligible linked rows: apply ratio to determine the eligible prefix, then filter
    // by segment constraints to restrict to rows matching this segment's field constraints.
    let n_eligible_pre_filter = eligible_linked_rows(linked_batch.num_rows(), include.ratio);
    let eligible_linked = linked_batch.slice(0, n_eligible_pre_filter);
    let (filtered_linked, surviving_indices) =
        filter_batch_by_constraints(&eligible_linked, segment_constraints)?;
    let n_eligible = filtered_linked.num_rows();

    let n = slot_count;
    // If no linked rows survive constraint filtering, produce an empty witness (all slots
    // get empty lists). Otherwise sample normally.
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
    let total: usize = counts.iter().sum();

    // Staging slot indices are ABSOLUTE (relative to the full staging batch), not relative
    // to this segment — so AssembleFromWitness can fold them into the correct list positions.
    let staging_idxs: Vec<u32> = counts.iter().enumerate()
        .flat_map(|(i, &c)| std::iter::repeat((slot_start + i) as u32).take(c))
        .collect();
    // slot_assignments[k]: index into `filtered_linked` for draw k.
    // Sampling mode controlled by include.reinforcement:
    //   None / 1.0 → uniform with-replacement
    //   0.0        → Fisher-Yates without-replacement per staging row
    //   r > 1.0    → Polya-urn weighted re-selection per staging row
    let slot_assignments: UInt32Array = {
        let r = include.reinforcement;
        if r == Some(0.0) {
            counts.iter().flat_map(|&m_n| {
                sample_linked_without_replacement(n_eligible, m_n)
            }).collect::<Vec<u32>>().into()
        } else if let Some(reinf) = r.filter(|&v| v > 1.0) {
            counts.iter().flat_map(|&m_n| {
                sample_linked_weighted(n_eligible, m_n, reinf)
            }).collect::<Vec<u32>>().into()
        } else {
            (0..total)
                .map(|_| (0u64..n_eligible as u64).fake::<u64>() as u32)
                .collect::<Vec<u32>>()
                .into()
        }
    };

    // --- Phase 2: deduplicate draws by linked row ---
    let (unique_linked_idxs, staging_refs_array) =
        build_witness_dedup(&slot_assignments, &staging_idxs, &surviving_indices, total);
    let n_witness = unique_linked_idxs.len();
    let unique_linked_arr = UInt32Array::from(unique_linked_idxs.clone());

    // --- Phase 3: build witness columns (one value per unique linked row) ---
    // linked-scoped refs: take from eligible_linked by unique linked index.
    // outer-scoped refs: skip — resolved from staging at assembly time.
    // plain fields: generate once per unique linked row.
    let mut arrow_fields: Vec<ArrowField> = Vec::new();
    let mut columns: Vec<ArrayRef> = Vec::new();

    for field in inner_fields {
        let col: ArrayRef = if let Some(ref_str) = field.simple_ref() {
            let is_linked_scoped = split_ref(ref_str)
                .map(|(rp, _)| include.reference == rp)
                .unwrap_or(false);
            if is_linked_scoped {
                let (_, target_col) = split_ref(ref_str).unwrap();
                let idx = eligible_linked.schema().index_of(target_col)
                    .map_err(|_| anyhow!("column '{target_col}' not found in linked-slot batch"))?;
                take(eligible_linked.column(idx).as_ref(), &unique_linked_arr, None)?
            } else {
                // Outer-scoped: not stored in witness; skip.
                continue;
            }
        } else {
            generate_column(field, n_witness, &[])?
        };
        arrow_fields.push(field_to_arrow(field));
        columns.push(col);
    }

    // --- Phase 4: assemble witness batch ---
    let data_batch = RecordBatch::try_new(Arc::new(ArrowSchema::new(arrow_fields)), columns)?;
    let with_refs = prepend_column(&data_batch, "_staging_refs",
        Arc::new(staging_refs_array) as ArrayRef)?;
    let witness_batch = prepend_column(&with_refs, "_linked_idx",
        Arc::new(UInt32Array::from(unique_linked_idxs)) as ArrayRef)?;
    computed.insert(witness_key.clone(), witness_batch);
    Ok(())
}

/// Unnest the `_staging_refs` ListArray in a witness batch to produce a flat junction table:
/// one row per (staging-slot, linked-row) pair. Rows are sorted by `_slot_idx` — required for
/// the offset-based list-fold in `execute_assemble_from_witness`.
///
/// Returns `(junction, slot_idx_arr, witness_row_arr)` where:
///   - `junction`: `_slot_idx` + replicated content columns (no sentinels)
///   - `slot_idx_arr`: sorted staging slot indices (one per junction row)
///   - `witness_row_arr`: witness row index for each junction row (for outer-scoped lookup)
fn unnest_staging_refs(
    witness: &RecordBatch,
) -> Result<(RecordBatch, UInt32Array, UInt32Array)> {
    let refs_col_idx = witness.schema().index_of("_staging_refs")
        .map_err(|_| anyhow!("unnest_staging_refs: '_staging_refs' not found in witness"))?;
    let staging_refs = witness.column(refs_col_idx)
        .as_any().downcast_ref::<ListArray>()
        .ok_or_else(|| anyhow!("_staging_refs is not a ListArray"))?;

    let total: usize = (0..witness.num_rows())
        .map(|r| staging_refs.value(r).len())
        .sum();

    let mut slot_idxs: Vec<u32> = Vec::with_capacity(total);
    let mut witness_row_idxs: Vec<u32> = Vec::with_capacity(total);
    for wr in 0..witness.num_rows() {
        let refs_slice = staging_refs.value(wr);
        let refs_arr = refs_slice.as_any().downcast_ref::<UInt32Array>()
            .ok_or_else(|| anyhow!("_staging_refs list element is not UInt32"))?;
        for &slot in refs_arr.values() {
            slot_idxs.push(slot);
            witness_row_idxs.push(wr as u32);
        }
    }
    let slot_arr = UInt32Array::from(slot_idxs);
    let witness_row_arr = UInt32Array::from(witness_row_idxs);

    // Sort by slot_idx: required for the offset-based list-fold.
    let sort_order = sort_to_indices(&slot_arr, None, None)?;
    let slot_arr_sorted: UInt32Array = take(&slot_arr, &sort_order, None)?
        .as_any().downcast_ref::<UInt32Array>().unwrap().clone();
    let witness_row_arr_sorted: UInt32Array = take(&witness_row_arr, &sort_order, None)?
        .as_any().downcast_ref::<UInt32Array>().unwrap().clone();

    // Strip sentinels; replicate remaining witness columns per slot.
    let stripped = strip_sentinel(strip_sentinel(witness.clone(), "_linked_idx"), "_staging_refs");
    let mut fields = vec![ArrowField::new("_slot_idx", DataType::UInt32, false)];
    let mut cols: Vec<ArrayRef> = vec![Arc::new(slot_arr_sorted.clone())];
    for col_idx in 0..stripped.num_columns() {
        fields.push(stripped.schema().field(col_idx).as_ref().clone());
        cols.push(take(stripped.column(col_idx).as_ref(), &witness_row_arr_sorted, None)?);
    }
    let junction = RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), cols)?;
    Ok((junction, slot_arr_sorted, witness_row_arr_sorted))
}

/// Fold the witness batches produced by `execute_witness` back into the
/// staging batch as `ListArray` columns, then evaluate expressions and emit.
async fn execute_assemble_from_witness(
    staging_path: &PathBuf,
    dataset: &SyntheticDataset,
    witness_specs: &[(String, Vec<PathBuf>, Option<String>)],
    computed: &mut HashMap<PathBuf, RecordBatch>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let mut batch = computed.get(staging_path).ok_or_else(|| {
        anyhow!("assemble from witness '{}': staging batch not yet computed", dataset.name)
    })?.clone();

    for (field_name, witness_keys, project_col) in witness_specs {
        // Collect and union all per-segment witness batches for this field.
        let witness_batches: Vec<RecordBatch> = witness_keys.iter()
            .map(|key| computed.get(key).cloned().ok_or_else(|| {
                anyhow!("assemble from witness '{}': witness segment for '{field_name}' not computed", dataset.name)
            }))
            .collect::<Result<Vec<_>>>()?;
        let witness_schema = witness_batches.first()
            .ok_or_else(|| anyhow!("assemble from witness '{}': no witness batches for '{field_name}'", dataset.name))?
            .schema();
        let witness = concat_batches(&witness_schema, &witness_batches)?;

        let staging_n = batch.num_rows();

        // Unnest _staging_refs to reconstruct the anonymous junction table.
        let (mut junction, slot_arr_sorted, _witness_row_arr_sorted) =
            unnest_staging_refs(&witness)?;

        // Identify outer-scoped fields: defined in the content schema but absent from the
        // witness (because execute_witness skips them). Look them up from the staging batch.
        let content_field_defs = dataset.data.iter()
            .find(|f| &f.name == field_name)
            .and_then(|f| f.content.as_deref())
            .map(|c| c.item.fields.as_slice())
            .unwrap_or(&[]);
        let stripped_witness_cols: HashSet<String> = {
            let s = strip_sentinel(strip_sentinel(witness.clone(), "_linked_idx"), "_staging_refs");
            s.schema().fields().iter().map(|f| f.name().clone()).collect()
        };
        let staging = batch.clone();
        for cf in content_field_defs {
            if stripped_witness_cols.contains(&cf.name) { continue; }
            if let Some(ref_str) = cf.simple_ref() {
                // Bare ref (no qualifier) or qualified ref: resolve the column name in staging.
                let col_name = split_ref(ref_str).map(|(_, c)| c).unwrap_or(ref_str);
                let stg_idx = staging.schema().index_of(col_name)
                    .map_err(|_| anyhow!("outer-scoped column '{col_name}' not found in staging batch"))?;
                let col = take(staging.column(stg_idx).as_ref(), &slot_arr_sorted, None)?;
                let arrow_field = ArrowField::new(cf.name.as_str(), col.data_type().clone(), true);
                junction = add_column(junction, arrow_field, col)?;
            }
        }

        // Slot-grouped list fold: count rows per slot, build offsets, fold into ListArray.
        let slot_idx_arr = junction.column(junction.schema().index_of("_slot_idx")
            .map_err(|_| anyhow!("junction missing '_slot_idx'"))?)
            .as_any().downcast_ref::<UInt32Array>()
            .ok_or_else(|| anyhow!("_slot_idx is not UInt32"))?
            .clone();

        let mut counts = vec![0usize; staging_n];
        for &idx in slot_idx_arr.values() {
            counts[idx as usize] += 1;
        }

        let inner = strip_sentinel(junction, "_slot_idx");
        let offsets = OffsetBuffer::<i32>::from_lengths(counts.iter().copied());

        let list_col: ArrayRef = if let Some(col_name) = project_col {
            let col_idx = inner.schema().index_of(col_name.as_str())
                .map_err(|_| anyhow!("project: column '{col_name}' not found in witness for '{field_name}'"))?;
            let col = inner.column(col_idx).clone();
            let item_field = Arc::new(ArrowField::new("item", col.data_type().clone(), true));
            Arc::new(ListArray::new(item_field, offsets, col, None))
        } else {
            let hidden_item_cols: HashSet<&str> = dataset.data.iter()
                .find(|f| &f.name == field_name)
                .and_then(|f| f.content.as_deref())
                .map(|c| c.item.fields.iter()
                    .filter(|f| f.hidden)
                    .map(|f| f.name.as_str())
                    .collect())
                .unwrap_or_default();
            let (struct_fields, struct_cols): (Vec<_>, Vec<_>) = inner.schema().fields().iter()
                .zip(inner.columns())
                .filter(|(f, _)| !hidden_item_cols.contains(f.name().as_str()))
                .map(|(f, c)| (Arc::new(f.as_ref().clone()), c.clone()))
                .unzip();
            let child: ArrayRef = Arc::new(StructArray::new(
                struct_fields.into_iter().collect(),
                struct_cols,
                None,
            ));
            let item_field = Arc::new(ArrowField::new("item", child.data_type().clone(), true));
            Arc::new(ListArray::new(item_field, offsets, child, None))
        };

        let list_arrow_field = ArrowField::new(field_name.as_str(), list_col.data_type().clone(), true);
        batch = add_column(batch, list_arrow_field, list_col)?;
    }

    let batch = evaluate_expressions(batch, dataset).await?;
    let output = filter_hidden_columns(batch.clone(), &dataset.data).await?;
    computed.insert(staging_path.clone(), batch);
    emit_batch(output, &dataset.format, &dataset.output_file, shared)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Linked dataset accumulation
// ---------------------------------------------------------------------------

/// Build an Arrow array of `n` rows all set to the given `YamlValue`, typed as `dtype`.
///
/// Used as the fallback column for scalar-reducer `AccumulateToLinked`: linked rows with no
/// matching source rows receive this value (their declared `default:`).
fn yaml_value_to_array(val: &YamlValue, dtype: &DataType, n: usize) -> ArrayRef {
    match dtype {
        DataType::Float64 => {
            let v = match val {
                YamlValue::Number(num) => num.as_f64().unwrap_or(0.0),
                _ => 0.0,
            };
            Arc::new(Float64Array::from(vec![v; n]))
        }
        DataType::Utf8 => {
            let s = match val { YamlValue::String(s) => s.as_str(), _ => "" };
            Arc::new(StringArray::from(vec![s; n]))
        }
        _ => {
            // Fallback: null-filled array of the correct type and length.
            arrow::array::new_null_array(dtype, n)
        }
    }
}

/// Accumulate values from a junction or witness batch into a linked dataset's field.
///
/// Groups `source_batch[source_field]` by `source_batch[group_by]` (always `"_linked_idx"`),
/// aggregates using `reducer`, then replaces the `linked_field` column in `computed[linked_path]`.
///
/// - `Collect`   → `array_agg`: builds a `ListArray`; unmapped linked rows get an empty list.
/// - `Sum`       → `sum`: scalar Float64; unmapped linked rows keep their existing default.
/// - `Max`/`Min` → `max`/`min`: scalar; unmapped rows keep their existing default.
/// - `TakeFirst` → `first_value`: first value in an arbitrary within-group order; unmapped
///   rows keep their existing default.
async fn execute_accumulate_to_linked(
    source_path: &PathBuf,
    source_field: &str,
    linked_path: &PathBuf,
    linked_field: &str,
    group_by: &str,
    reducer: &Reducer,
    default_val: &YamlValue,
    computed: &mut HashMap<PathBuf, RecordBatch>,
    accumulated_fields: &mut HashSet<(PathBuf, String)>,
) -> Result<()> {
    let raw_source = computed.get(source_path).ok_or_else(|| {
        anyhow!("AccumulateToLinked: source batch '{}' not computed", source_path.display())
    })?.clone();

    // If the source is a Stage-4 witness batch (has `_staging_refs`), expand it to a flat
    // junction table: one row per (staging-slot, linked-row) draw. This restores the K entries
    // per linked row that the aggregation needs, since the witness carries only 1 row per unique
    // linked-row draw with `_staging_refs` encoding the back-references.
    let source_batch = if raw_source.schema().index_of("_staging_refs").is_ok() {
        let refs_col_idx = raw_source.schema().index_of("_staging_refs")?;
        let staging_refs = raw_source.column(refs_col_idx)
            .as_any().downcast_ref::<ListArray>()
            .ok_or_else(|| anyhow!("AccumulateToLinked: _staging_refs is not a ListArray"))?;
        let total: usize = (0..raw_source.num_rows())
            .map(|r| staging_refs.value(r).len())
            .sum();
        let mut witness_row_idxs: Vec<u32> = Vec::with_capacity(total);
        for wr in 0..raw_source.num_rows() {
            let n = staging_refs.value(wr).len();
            for _ in 0..n { witness_row_idxs.push(wr as u32); }
        }
        let witness_row_arr = UInt32Array::from(witness_row_idxs);
        // Strip _staging_refs; keep _linked_idx and content columns (replicated per draw).
        let stripped = strip_sentinel(raw_source, "_staging_refs");
        let fields: Vec<ArrowField> = stripped.schema().fields().iter()
            .map(|f| f.as_ref().clone())
            .collect();
        let cols: Vec<ArrayRef> = stripped.columns().iter()
            .map(|c| take(c.as_ref(), &witness_row_arr, None))
            .collect::<Result<_, _>>()?;
        RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), cols)?
    } else {
        raw_source
    };

    let linked_batch = computed.get(linked_path).ok_or_else(|| {
        anyhow!("AccumulateToLinked: linked batch '{}' not computed", linked_path.display())
    })?.clone();
    let linked_n = linked_batch.num_rows();

    // Locate the linked field upfront — needed for scalar reducers to keep existing defaults.
    let linked_col_idx = linked_batch.schema().index_of(linked_field)
        .map_err(|_| anyhow!("AccumulateToLinked: field '{}' not found in linked batch", linked_field))?;
    let existing_linked_col = linked_batch.column(linked_col_idx).clone();

    // Build and run the DataFusion aggregate.
    let aggr_expr = match reducer {
        Reducer::Collect   => array_agg(col(source_field)).alias("__agg"),
        Reducer::Sum       => df_sum(col(source_field)).alias("__agg"),
        Reducer::Max       => df_max(col(source_field)).alias("__agg"),
        Reducer::Min       => df_min(col(source_field)).alias("__agg"),
        Reducer::TakeOne   => df_first_value(col(source_field), vec![]).alias("__agg"),
    };
    let ctx = SessionContext::new();
    ctx.register_batch("src", source_batch)?;
    let agg_batches = ctx.table("src").await?
        .aggregate(vec![col(group_by)], vec![aggr_expr])?.collect().await?;
    let agg_schema = agg_batches.first().map(|b| b.schema())
        .unwrap_or_else(|| Arc::new(ArrowSchema::empty()));
    let agg_batch = concat_batches(&agg_schema, &agg_batches)?;

    // Build linked_idx → agg_row index map.
    let group_col = agg_batch.schema().index_of(group_by)
        .ok()
        .and_then(|i| agg_batch.column(i).as_any().downcast_ref::<UInt32Array>().map(|a| a.values().to_vec()))
        .unwrap_or_default();
    let agg_values_col = agg_batch.schema().index_of("__agg")
        .map(|i| agg_batch.column(i).clone())
        .unwrap_or_else(|_| Arc::new(arrow::array::Int32Array::from(vec![] as Vec<i32>)));
    let mut idx_map: HashMap<u32, usize> = HashMap::new();
    for (row, &linked_idx) in group_col.iter().enumerate() {
        idx_map.insert(linked_idx, row);
    }

    // Build the replacement column.
    let new_col: ArrayRef = match reducer {
        Reducer::Collect => {
            // Collect: build a ListArray. For the FIRST accumulation into a (linked_path,
            // linked_field) pair, discard any generator-produced initial values and use an
            // empty fallback for unmapped rows. For SUBSEQUENT calls (multi-segment staging),
            // carry forward the already-accumulated values so items from all segments are
            // combined rather than the later call overwriting the earlier one.
            let is_first = accumulated_fields.insert((linked_path.clone(), linked_field.to_string()));
            let element_type = match agg_values_col.data_type() {
                DataType::List(f) => f.data_type().clone(),
                other => bail!("AccumulateToLinked: expected List from array_agg, got {other:?}"),
            };
            let agg_list = agg_values_col.as_any().downcast_ref::<ListArray>()
                .ok_or_else(|| anyhow!("AccumulateToLinked: __agg column is not a ListArray"))?;
            // For subsequent calls, carry forward existing accumulated items.
            let existing_list = if !is_first {
                existing_linked_col.as_any().downcast_ref::<ListArray>()
            } else {
                None
            };
            let item_field = Arc::new(ArrowField::new("item", element_type.clone(), true));
            let mut offsets: Vec<i32> = vec![0];
            let mut child_slices: Vec<ArrayRef> = Vec::new();
            for linked_row in 0..linked_n {
                let mut row_len = 0i32;
                if let Some(existing) = existing_list {
                    let ex = existing.value(linked_row);
                    row_len += ex.len() as i32;
                    child_slices.push(ex);
                }
                if let Some(&agg_row) = idx_map.get(&(linked_row as u32)) {
                    let slice = agg_list.value(agg_row);
                    row_len += slice.len() as i32;
                    child_slices.push(slice);
                }
                offsets.push(offsets.last().unwrap() + row_len);
            }
            let child_array: ArrayRef = if child_slices.is_empty() {
                new_empty_array(&element_type)
            } else {
                let refs: Vec<&dyn arrow::array::Array> = child_slices.iter().map(|a| a.as_ref()).collect();
                concat(&refs)?
            };
            let offsets_buf = OffsetBuffer::<i32>::new(ScalarBuffer::from(offsets));
            Arc::new(ListArray::new(item_field, offsets_buf, child_array, None))
        }
        _ => {
            // Scalar reducers (Sum, Max, Min, TakeOne).
            // First accumulation: mapped rows get the aggregated value; unmapped rows get the
            // field's declared `default_val`.
            // Subsequent accumulations (multi-segment staging): commutative reducers (Sum/Max/Min)
            // combine element-wise with the existing value; TakeOne keeps the existing value
            // unchanged (= "take whichever segment captured it first").
            let is_first = accumulated_fields.insert((linked_path.clone(), linked_field.to_string()));
            if !is_first && matches!(reducer, Reducer::TakeOne) {
                existing_linked_col.clone()
            } else if !is_first {
                accumulate_scalar_cumulative(reducer, &existing_linked_col, &agg_values_col, &idx_map, linked_n)?
            } else {
                let agg_n = agg_batch.num_rows();
                let take_indices: UInt32Array = (0..linked_n as u32).map(|linked_row| {
                    idx_map.get(&linked_row)
                        .map(|&agg_row| agg_row as u32)
                        .unwrap_or(agg_n as u32 + linked_row)
                }).collect::<Vec<u32>>().into();
                let default_col = yaml_value_to_array(default_val, existing_linked_col.data_type(), linked_n);
                let combined = concat(&[agg_values_col.as_ref(), default_col.as_ref()])?;
                take(combined.as_ref(), &take_indices, None)?
            }
        }
    };

    // Replace linked_field column in linked_batch.
    let mut fields: Vec<Arc<ArrowField>> = linked_batch.schema().fields().to_vec();
    let mut columns = linked_batch.columns().to_vec();
    fields[linked_col_idx] = Arc::new(ArrowField::new(linked_field, new_col.data_type().clone(), true));
    columns[linked_col_idx] = new_col;
    computed.insert(linked_path.clone(), RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), columns)?);
    Ok(())
}

/// Element-wise combination of an existing linked-field column with new aggregated values,
/// for commutative scalar reducers (Sum, Max, Min) on a subsequent `AccumulateToLinked` call.
///
/// For each linked row: if that row has a new aggregated value (present in `idx_map`), combine
/// the existing value with the new one using `reducer`; otherwise keep the existing value
/// unchanged. This preserves contributions from earlier segments when the same linked field is
/// accumulated across multiple Bernoulli segments.
fn accumulate_scalar_cumulative(
    reducer: &Reducer,
    existing_col: &ArrayRef,
    agg_values_col: &ArrayRef,
    idx_map: &HashMap<u32, usize>,
    linked_n: usize,
) -> Result<ArrayRef> {
    match existing_col.data_type() {
        DataType::Float64 => {
            let existing = existing_col.as_any().downcast_ref::<Float64Array>()
                .ok_or_else(|| anyhow!("accumulate_scalar_cumulative: existing column is not Float64"))?;
            let agg = agg_values_col.as_any().downcast_ref::<Float64Array>()
                .ok_or_else(|| anyhow!("accumulate_scalar_cumulative: agg column is not Float64"))?;
            let result: Vec<f64> = (0..linked_n as u32).map(|linked_row| {
                let ev = if existing.is_null(linked_row as usize) { 0.0 } else { existing.value(linked_row as usize) };
                if let Some(&agg_row) = idx_map.get(&linked_row) {
                    let av = if agg.is_null(agg_row) { 0.0 } else { agg.value(agg_row) };
                    match reducer {
                        Reducer::Sum => ev + av,
                        Reducer::Max => f64::max(ev, av),
                        Reducer::Min => f64::min(ev, av),
                        _ => ev,
                    }
                } else {
                    ev
                }
            }).collect();
            Ok(Arc::new(Float64Array::from(result)))
        }
        DataType::Utf8 => {
            let existing = existing_col.as_any().downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow!("accumulate_scalar_cumulative: existing column is not Utf8"))?;
            let agg = agg_values_col.as_any().downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow!("accumulate_scalar_cumulative: agg column is not Utf8"))?;
            let mut builder = StringBuilder::new();
            for linked_row in 0..linked_n as u32 {
                let ev = if existing.is_null(linked_row as usize) { "" } else { existing.value(linked_row as usize) };
                if let Some(&agg_row) = idx_map.get(&linked_row) {
                    let av = if agg.is_null(agg_row) { "" } else { agg.value(agg_row) };
                    let chosen = match reducer {
                        Reducer::Max => if av > ev { av } else { ev },
                        Reducer::Min => if av < ev { av } else { ev },
                        _ => ev,
                    };
                    builder.append_value(chosen);
                } else {
                    builder.append_value(ev);
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        other => bail!("accumulate_scalar_cumulative: unsupported data type {other:?} for reducer"),
    }
}

/// Emit the batch at `path` from `computed` to an output file.
///
/// Applies `filter_hidden_columns` and then calls the normal emit path.
async fn execute_emit_dataset(
    path: &PathBuf,
    dataset: &SyntheticDataset,
    computed: &HashMap<PathBuf, RecordBatch>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let batch = computed.get(path).ok_or_else(|| {
        anyhow!("EmitDataset: batch at '{}' not computed", path.display())
    })?.clone();
    let output = filter_hidden_columns(batch, &dataset.data).await?;
    emit_batch(output, &dataset.format, &dataset.output_file, shared)
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

/// Generate M_n rows per parent-row slot, tagging each with `_slot_idx = slot_offset + i`.
/// The canonical (one-row-per-slot) batch is handled separately by `grow_parent_from_children`;
/// this function produces the expanded output batch for the lower cover member's output file.
fn generate_expanded_batch(
    fields: &Schema,
    slot_count: usize,
    constraints: &HashMap<String, FieldConstraints>,
    cardinality: &CountSpec,
    slot_offset: usize,
) -> Result<RecordBatch> {
    let mut slot_tags: Vec<u32> = Vec::new();
    let mut slot_batches: Vec<RecordBatch> = Vec::new();

    for i in 0..slot_count {
        let m_n = sample_count(cardinality).max(1);
        let batch = generate_fresh_batch(fields, m_n, constraints)?;
        let slot = (slot_offset + i) as u32;
        slot_tags.extend(std::iter::repeat(slot).take(m_n));
        slot_batches.push(batch);
    }

    let inner_schema = slot_batches.first().map(|b| b.schema())
        .unwrap_or_else(|| Arc::new(schema_to_arrow(fields)));
    let combined = concat_batches(&inner_schema, &slot_batches)?;
    let slot_col: ArrayRef = Arc::new(UInt32Array::from(slot_tags));
    prepend_column(&combined, "_slot_idx", slot_col)
}

// ---------------------------------------------------------------------------
// Linked-dataset sampling helpers
// ---------------------------------------------------------------------------

/// Sample `count` indices from `[0, linked_n)` without replacement (Fisher-Yates).
///
/// Panics if `count > linked_n` — callers must enforce the planning-time check.
fn sample_linked_without_replacement(linked_n: usize, count: usize) -> Vec<u32> {
    assert!(count <= linked_n, "sample_linked_without_replacement: count {count} > linked_n {linked_n}");
    let mut indices: Vec<u32> = (0..linked_n as u32).collect();
    for i in 0..count {
        let j = (i as u64..linked_n as u64).fake::<u64>() as usize;
        indices.swap(i, j);
    }
    indices[..count].to_vec()
}

/// Sample `count` indices from `[0, linked_n)` with Polya-urn weighting.
///
/// Each initially-uniform weight is multiplied by `reinforcement` after selection,
/// making previously-selected indices more likely to be selected again.
/// `reinforcement > 1.0` produces clumping; `reinforcement = 1.0` degenerates to uniform.
fn sample_linked_weighted(linked_n: usize, count: usize, reinforcement: f64) -> Vec<u32> {
    let mut weights: Vec<f64> = vec![1.0; linked_n];
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        let total: f64 = weights.iter().sum();
        let mut target = (0.0f64..total).fake::<f64>();
        let mut chosen = 0usize;
        for (i, &w) in weights.iter().enumerate() {
            if target < w { chosen = i; break; }
            target -= w;
        }
        result.push(chosen as u32);
        weights[chosen] *= reinforcement;
    }
    result
}

/// Strip `_slot_idx` from a batch, leaving data columns intact.
/// Used to remove the sentinel before emitting lower cover member output while retaining it in `computed`.
fn strip_slot_idx(batch: RecordBatch) -> RecordBatch {
    strip_sentinel(batch, "_slot_idx")
}

/// Strip `_linked_idx` from a batch before emitting a junction dataset's output.
/// The full batch (including `_linked_idx`) is retained in `computed` for `AccumulateToLinked`.
fn strip_linked_idx(batch: RecordBatch) -> RecordBatch {
    strip_sentinel(batch, "_linked_idx")
}

fn strip_sentinel(batch: RecordBatch, sentinel: &str) -> RecordBatch {
    let Ok(idx) = batch.schema().index_of(sentinel) else { return batch };
    let (fields, cols): (Vec<_>, Vec<_>) = batch.schema().fields().iter()
        .zip(batch.columns())
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(_, (f, c))| (f.clone(), c.clone()))
        .unzip();
    RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), cols)
        .expect("strip_sentinel: schema mismatch is impossible")
}

/// Inject `_linked_idx` into `batch` for any junction links in `dataset`.
///
/// For each junction link (a link not referenced by any `content.from`), samples one linked
/// row per batch row and prepends a `_linked_idx: UInt32` column. The linked batch must already
/// be in `computed` (the DAG link-edge from linked dataset → junction guarantees this).
///
/// Only the first junction link is processed (multi-link deferred).
fn inject_linked_idx(
    batch: &RecordBatch,
    path: &Path,
    dataset: &SyntheticDataset,
    computed: &HashMap<PathBuf, RecordBatch>,
) -> Result<RecordBatch> {
    let list_link_refs: HashSet<&str> = dataset.data.iter()
        .filter_map(|f| f.content.as_ref()?.from.as_deref())
        .collect();
    for link in &dataset.links {
        if list_link_refs.contains(link.reference.as_str()) { continue; }
        let Some(linked_path) = resolve_include(path, &link.file) else { continue };
        let Some(linked_batch) = computed.get(&linked_path) else { continue };
        let n_linked = linked_batch.num_rows();
        let n_eligible = eligible_linked_rows(n_linked, link.ratio);
        let n_rows = batch.num_rows();
        let r = link.reinforcement;
        let assignments: Vec<u32> = if r == Some(0.0) {
            sample_linked_without_replacement(n_eligible, n_rows)
        } else if let Some(reinf) = r.filter(|&v| v > 1.0) {
            sample_linked_weighted(n_eligible, n_rows, reinf)
        } else {
            (0..n_rows)
                .map(|_| (0u64..n_eligible as u64).fake::<u64>() as u32)
                .collect()
        };
        let linked_idx_arr: ArrayRef = Arc::new(UInt32Array::from(assignments));
        return prepend_column(batch, "_linked_idx", linked_idx_arr);
    }
    Ok(batch.clone())
}

fn generate_with_inherited(
    schema: &Schema,
    rows: usize,
    inherited: &HashMap<String, Vec<ArrayRef>>,
) -> Result<RecordBatch> {
    generate_batch(schema, rows, inherited, &HashMap::new())
}

/// Generate a batch for `schema`. Fields in `overrides` have their constraints
/// replaced before generation; fields in `inherited` prepend pre-computed values.
fn generate_batch(
    schema: &Schema,
    rows: usize,
    inherited: &HashMap<String, Vec<ArrayRef>>,
    overrides: &HashMap<String, FieldConstraints>,
) -> Result<RecordBatch> {
    let arrow_schema = Arc::new(schema_to_arrow(schema));
    let columns = schema
        .iter()
        .filter(|f| f.expression.is_none() && !f.is_list_link())
        .map(|f| {
            let prefix = inherited.get(&f.name).map_or(&[] as &[ArrayRef], |v| v.as_slice());
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

/// Prepend witness-source rows (unshuffled) before shuffled non-witness-source rows.
/// Witness-source rows must appear first so GenerateWitness's n_eligible_slots boundary
/// correctly identifies them.
async fn combine_witness_source_first(
    witness_source_batches: Vec<RecordBatch>,
    non_witness_source_batches: Vec<RecordBatch>,
    schema: &Schema,
    name: &str,
) -> Result<RecordBatch> {
    let shuffled_non_witness = combine_and_shuffle(non_witness_source_batches, schema, name).await?;
    let arrow_schema = Arc::new(schema_to_arrow(schema));
    let ws_combined = concat_batches(&arrow_schema, &witness_source_batches)?;
    Ok(concat_batches(&ws_combined.schema(), &[ws_combined, shuffled_non_witness])?)
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
/// The full batch (including hidden columns) is kept in `computed` for inherited
/// field wiring; only the filtered batch is written to output.
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
