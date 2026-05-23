use anyhow::{anyhow, bail, Result};
use arrow::array::{ArrayRef, Float64Array, ListArray, StringArray, StructArray, UInt32Array, new_empty_array};
use arrow::buffer::{OffsetBuffer, ScalarBuffer};
use arrow::compute::{concat, concat_batches, take};
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
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::constraints::FieldConstraints;
use crate::generator::{generate_column, sample_count};
use crate::schema::{field_to_arrow, schema_to_arrow};
use crate::models::{resolve_include, split_ref, CountSpec, Field, Format, Include, Range, Reducer, Schema, SyntheticDataset};
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
                let has_link_content = dataset.data.iter().any(|f| f.is_link_content());
                if *skip_emit && has_link_content {
                    // Scalar-only intermediate; AssembleNestedInclude adds list columns and emits.
                    computed.insert(path.clone(), batch);
                } else {
                    // Evaluate expressions for both normal emit and collect-target deferral
                    // (collect targets have skip_emit=true but no nested includes).
                    let batch = evaluate_expressions(batch, dataset.as_ref()).await?;
                    // Inject _pool_idx for junction datasets (link without include).
                    // The full batch (with _pool_idx) is kept in `computed` for CollectToPool;
                    // _pool_idx is stripped before emitting.
                    let batch = inject_pool_idx(&batch, path, dataset.as_ref(), &computed)?;
                    let output = filter_hidden_columns(
                        strip_pool_idx(batch.clone()),
                        &dataset.data,
                    ).await?;
                    computed.insert(path.clone(), batch);
                    if !*skip_emit {
                        emit_batch(output, &dataset.format, &dataset.output_file, &mut shared)?;
                    }
                    // When skip_emit is true (collect target): batch stored, file write deferred
                    // to the EmitDataset step that follows CollectToPool.
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
                inner_fields, include, cardinality,
                pool_slots_path,
            } => {
                execute_inner_flat(
                    flat_key, outer_path, list_field_name,
                    inner_fields, include, cardinality,
                    pool_slots_path,
                    &mut computed,
                )?;
            }
            ExecutionStep::AssembleNestedInclude { outer_path, dataset, flat_specs } => {
                execute_assemble_nested_include(
                    outer_path, dataset.as_ref(), flat_specs,
                    &mut computed, &mut shared,
                ).await?;
            }
            ExecutionStep::CollectToPool {
                source_path, source_field, pool_path, pool_field, group_by, reducer, default_val,
            } => {
                execute_collect_to_pool(
                    source_path, source_field, pool_path, pool_field, group_by, reducer, default_val,
                    &mut computed,
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
    // n_eligible_slots boundary correctly identifies pool-member rows.
    let mut pool_parent_batches: Vec<RecordBatch> = Vec::new();
    let mut nonpool_parent_batches: Vec<RecordBatch> = Vec::new();
    let mut sibling_buffers: HashMap<PathBuf, Vec<RecordBatch>> = HashMap::new();
    let mut slot_offset: usize = 0;

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
                    if let Some(ref card) = sib.cardinality {
                        // The precomputed batch has total_sib_rows rows, but
                        // grow_parent_from_children expects seg.rows canonical rows (one per
                        // parent slot). Generate a fresh canonical batch for assembly and an
                        // expanded batch tagged with _slot_idx so the lattice position is
                        // recorded in computed after all segments are processed.
                        let canonical = generate_fresh_batch(
                            &sib.dataset.data, seg.rows, &seg.field_constraints,
                        )?;
                        let expanded = generate_expanded_batch(
                            &sib.dataset.data, seg.rows, &seg.field_constraints, card, slot_offset,
                        )?;
                        sibling_buffers.entry(sib.path.clone()).or_default().push(expanded);
                        child_batches.push((sib, canonical));
                    } else {
                        // No cardinality: precomputed row count matches seg.rows.
                        let precomputed = computed[&sib.path].clone();
                        child_batches.push((sib, precomputed));
                    }
                } else {
                    // Canonical batch: one row per slot, used for parent assembly.
                    let canonical = generate_fresh_batch(
                        &sib.dataset.data, seg.rows, &seg.field_constraints,
                    )?;
                    if let Some(ref card) = sib.cardinality {
                        // Expanded batch: M_n rows per slot, tagged with _slot_idx for output.
                        let expanded = generate_expanded_batch(
                            &sib.dataset.data, seg.rows, &seg.field_constraints, card, slot_offset,
                        )?;
                        sibling_buffers.entry(sib.path.clone()).or_default().push(expanded);
                        child_batches.push((sib, canonical));
                    } else {
                        sibling_buffers.entry(sib.path.clone()).or_default().push(canonical.clone());
                        child_batches.push((sib, canonical));
                    }
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
        slot_offset += seg.rows;
    }

    // Pool-rows-first: pool members occupy the leading positions in the combined parent
    // batch so that GenerateInnerFlat's n_eligible_slots boundary selects them correctly.
    let parent_shuffled = if has_pool_siblings && !pool_parent_batches.is_empty() {
        combine_pool_first(pool_parent_batches, nonpool_parent_batches, &dataset.data, &dataset.name).await?
    } else {
        let mut all = pool_parent_batches;
        all.extend(nonpool_parent_batches);
        combine_and_shuffle(all, &dataset.data, &dataset.name).await?
    };

    let has_link_content = dataset.data.iter().any(|f| f.is_link_content());
    if skip_parent_emit && has_link_content {
        // Scalar-only intermediate; AssembleNestedInclude adds list columns, evaluates
        // expressions, and emits.
        computed.insert(path.to_path_buf(), parent_shuffled);
    } else {
        // For both normal emit and collect-target deferral (skip_parent_emit=true, no nested
        // includes): evaluate expressions now; file write is either done immediately or deferred
        // to the EmitDataset step that follows CollectToPool.
        let parent_shuffled = evaluate_expressions(parent_shuffled, dataset).await?;
        let parent_output = filter_hidden_columns(parent_shuffled.clone(), &dataset.data).await?;
        computed.insert(path.to_path_buf(), parent_shuffled);
        if !skip_parent_emit {
            emit_batch(parent_output, &dataset.format, &dataset.output_file, shared)?;
        }
    }
    parent_computed.insert(path.to_path_buf());

    for sib in siblings {
        // Pool siblings have no standalone output — skip entirely.
        if sib.is_pool {
            continue;
        }
        // Siblings that were themselves parents in a prior step are already emitted.
        // If the sibling had cardinality we accumulated an expanded+_slot_idx batch in
        // sibling_buffers during the segment loop — store it in computed so downstream
        // steps (collect bindings, pool sampling) see its lattice position.
        if parent_computed.contains(&sib.path) {
            if sib.cardinality.is_some() {
                let buffers = sibling_buffers.remove(&sib.path).unwrap_or_default();
                if !buffers.is_empty() {
                    let sib_schema = buffers[0].schema();
                    let tagged = concat_batches(&sib_schema, &buffers)?;
                    computed.insert(sib.path.clone(), tagged);
                }
            }
            continue;
        }
        let sib_shuffled = combine_and_shuffle(
            sibling_buffers.remove(&sib.path).unwrap_or_default(),
            &sib.dataset.data,
            &sib.dataset.name,
        ).await?;
        let sib_shuffled = evaluate_expressions(sib_shuffled, &sib.dataset).await?;
        // For junction link siblings: sample one pool row per row and prepend as _pool_idx.
        // The pool batch must already be in `computed` (DAG link-edge ordering guarantees this).
        let sib_shuffled = inject_pool_idx(&sib_shuffled, &sib.path, &sib.dataset, computed)?;
        let sib_output = filter_hidden_columns(
            strip_pool_idx(strip_slot_idx(sib_shuffled.clone())),
            &sib.dataset.data,
        ).await?;
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
        .filter(|f| f.expression.is_none() && !f.is_link_content())
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
// Nested include generation
// ---------------------------------------------------------------------------

/// Build the flat intermediate table for one nested include field.
///
/// Produces a `RecordBatch` with `_slot_idx: UInt32` (which outer row each
/// Generate the joint-atom flat for one nested-include list field.
///
/// Each row is one **atom**: a (outer-slot, pool-slot) pair. Column sources:
///   - pool-scoped refs: the pushed-down pool-slot solution for that atom's `_pool_idx`
///   - outer-scoped refs: the outer row value for that atom's `_slot_idx`
///   - plain fields: generated fresh per atom
///
/// All atoms sharing the same `_pool_idx` carry identical pool-scoped field values because
/// they draw from the same pre-solved pool-slot row. This is the push-down mechanism:
/// the pool node's field schema is resolved once per slot, then referenced by every atom
/// assigned to that slot.
fn execute_inner_flat(
    flat_key: &PathBuf,
    outer_path: &PathBuf,
    list_field_name: &str,
    inner_fields: &[Field],
    include: &Include,
    cardinality: &CountSpec,
    pool_slots_path: &PathBuf,
    computed: &mut HashMap<PathBuf, RecordBatch>,
) -> Result<()> {
    let outer_batch = computed.get(outer_path).ok_or_else(|| {
        anyhow!("inner flat '{list_field_name}': outer batch not yet computed")
    })?.clone();
    // pool_slots: one pre-solved row per pool slot. Pool-scoped refs in atom rows
    // are resolved by indexing into this batch at the atom's assigned _pool_idx.
    let pool_slots = computed.get(pool_slots_path).ok_or_else(|| {
        anyhow!("inner flat '{list_field_name}': pool-slot batch not yet computed")
    })?.clone();

    // --- Phase 1: assign each atom to an outer slot and a pool slot ---
    let n_eligible_slots = match include.ratio {
        Some(r) => ((r * pool_slots.num_rows() as f64).round() as usize)
            .min(pool_slots.num_rows()).max(1),
        None => pool_slots.num_rows(),
    };

    let n = outer_batch.num_rows();
    let counts: Vec<usize> = (0..n).map(|_| sample_count(cardinality)).collect();
    let total: usize = counts.iter().sum();

    let outer_idxs: Vec<u32> = counts.iter().enumerate()
        .flat_map(|(i, &c)| std::iter::repeat(i as u32).take(c))
        .collect();
    let slot_idx_arr: ArrayRef = Arc::new(UInt32Array::from(outer_idxs.clone()));
    // slot_assignments: which pool slot each atom row is assigned to (_pool_idx values).
    // The sampling mode is controlled by include.reinforcement:
    //   None / 1.0 → uniform with-replacement (existing behaviour)
    //   0.0        → Fisher-Yates without-replacement per outer row
    //   r > 1.0    → Polya-urn weighted re-selection per outer row
    let slot_assignments: UInt32Array = {
        let r = include.reinforcement;
        if r == Some(0.0) {
            // Without-replacement: draw M_n unique slots per outer row.
            counts.iter().flat_map(|&m_n| {
                sample_pool_without_replacement(n_eligible_slots, m_n)
            }).collect::<Vec<u32>>().into()
        } else if let Some(reinf) = r.filter(|&v| v > 1.0) {
            // Polya-urn: weighted re-selection per outer row.
            counts.iter().flat_map(|&m_n| {
                sample_pool_weighted(n_eligible_slots, m_n, reinf)
            }).collect::<Vec<u32>>().into()
        } else {
            // Uniform with-replacement (None or 1.0).
            (0..total)
                .map(|_| (0u64..n_eligible_slots as u64).fake::<u64>() as u32)
                .collect::<Vec<u32>>()
                .into()
        }
    };
    let pool_idx_arr: ArrayRef = Arc::new(slot_assignments.clone());
    let rep_indices: UInt32Array = outer_idxs.into();

    // --- Phase 2: build atom columns ---
    // Pool-scoped refs: apply the pre-solved pool-slot value for the assigned slot.
    // Outer-scoped refs: replicate the outer-row value for the slot index.
    // Plain fields: generate fresh per atom.
    let mut arrow_fields: Vec<ArrowField> = Vec::new();
    let mut columns: Vec<ArrayRef> = Vec::new();

    for field in inner_fields {
        let col: ArrayRef = if let Some(ref_str) = field.simple_ref() {
            let is_pool_scoped = split_ref(ref_str)
                .map(|(rp, _)| include.reference == rp)
                .unwrap_or(false);
            if is_pool_scoped {
                let (_, target_col) = split_ref(ref_str).unwrap();
                let idx = pool_slots.schema().index_of(target_col)
                    .map_err(|_| anyhow!("column '{target_col}' not found in pool-slot batch"))?;
                take(pool_slots.column(idx).as_ref(), &slot_assignments, None)?
            } else {
                let idx = outer_batch.schema().index_of(ref_str)
                    .map_err(|_| anyhow!("outer-scoped column '{ref_str}' not found in outer batch"))?;
                take(outer_batch.column(idx).as_ref(), &rep_indices, None)?
            }
        } else {
            generate_column(field, total, &[])?
        };
        arrow_fields.push(field_to_arrow(field));
        columns.push(col);
    }

    let data_batch = RecordBatch::try_new(Arc::new(ArrowSchema::new(arrow_fields)), columns)?;
    let with_pool = prepend_column(&data_batch, "_pool_idx", pool_idx_arr)?;
    let flat_batch = prepend_column(&with_pool, "_slot_idx", slot_idx_arr)?;
    computed.insert(flat_key.clone(), flat_batch);
    Ok(())
}

/// Fold the inner flat tables produced by `execute_inner_flat` back into the
/// outer batch as `ListArray` columns, then evaluate expressions and emit.
async fn execute_assemble_nested_include(
    outer_path: &PathBuf,
    dataset: &SyntheticDataset,
    flat_specs: &[(String, PathBuf, Option<String>)],
    computed: &mut HashMap<PathBuf, RecordBatch>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let mut batch = computed.get(outer_path).ok_or_else(|| {
        anyhow!("assemble nested include '{}': outer batch not yet computed", dataset.name)
    })?.clone();

    for (field_name, flat_key, project_col) in flat_specs {
        let inner = computed.get(flat_key).ok_or_else(|| {
            anyhow!("assemble nested include '{}': inner flat for '{field_name}' not yet computed", dataset.name)
        })?.clone();

        let outer_n = batch.num_rows();
        let outer_idx_col = inner.schema().index_of("_slot_idx")
            .map_err(|_| anyhow!("inner flat missing '_slot_idx' column"))?;
        let outer_idx_arr = inner.column(outer_idx_col)
            .as_any().downcast_ref::<UInt32Array>()
            .ok_or_else(|| anyhow!("_slot_idx is not UInt32"))?;

        let mut counts = vec![0usize; outer_n];
        for &idx in outer_idx_arr.values() {
            counts[idx as usize] += 1;
        }

        // Strip both sentinels: _slot_idx (slot grouping) and _pool_idx (pool sampling).
        let inner = strip_pool_idx(strip_slot_idx(inner));
        let offsets = OffsetBuffer::<i32>::from_lengths(counts.iter().copied());

        let list_col: ArrayRef = if let Some(col_name) = project_col {
            // Project: extract a single column and produce a scalar ListArray.
            let col_idx = inner.schema().index_of(col_name.as_str())
                .map_err(|_| anyhow!("project: column '{col_name}' not found in inner flat for '{field_name}'"))?;
            let col = inner.column(col_idx).clone();
            let item_field = Arc::new(ArrowField::new("item", col.data_type().clone(), true));
            Arc::new(ListArray::new(item_field, offsets, col, None))
        } else {
            // Normal: wrap all remaining columns in a StructArray, skipping hidden item fields.
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
    computed.insert(outer_path.clone(), batch);
    emit_batch(output, &dataset.format, &dataset.output_file, shared)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pool accumulation
// ---------------------------------------------------------------------------

/// Build an Arrow array of `n` rows all set to the given `YamlValue`, typed as `dtype`.
///
/// Used as the fallback column for scalar-reducer `CollectToPool`: pool rows with no
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

/// Accumulate values from a junction or inner-flat batch into a pool dataset's field.
///
/// Groups `source_batch[source_field]` by `source_batch[group_by]` (always `"_pool_idx"`),
/// aggregates using `reducer`, then replaces the `pool_field` column in `computed[pool_path]`.
///
/// - `Collect`   → `array_agg`: builds a `ListArray`; unmapped pool rows get an empty list.
/// - `Sum`       → `sum`: scalar Float64; unmapped pool rows keep their existing default.
/// - `Max`/`Min` → `max`/`min`: scalar; unmapped rows keep their existing default.
/// - `TakeFirst` → `first_value`: first value in an arbitrary within-group order; unmapped
///   rows keep their existing default.
async fn execute_collect_to_pool(
    source_path: &PathBuf,
    source_field: &str,
    pool_path: &PathBuf,
    pool_field: &str,
    group_by: &str,
    reducer: &Reducer,
    default_val: &YamlValue,
    computed: &mut HashMap<PathBuf, RecordBatch>,
) -> Result<()> {
    let source_batch = computed.get(source_path).ok_or_else(|| {
        anyhow!("CollectToPool: source batch '{}' not computed", source_path.display())
    })?.clone();
    let pool_batch = computed.get(pool_path).ok_or_else(|| {
        anyhow!("CollectToPool: pool batch '{}' not computed", pool_path.display())
    })?.clone();
    let pool_n = pool_batch.num_rows();

    // Locate the pool field upfront — needed for scalar reducers to keep existing defaults.
    let pool_col_idx = pool_batch.schema().index_of(pool_field)
        .map_err(|_| anyhow!("CollectToPool: field '{}' not found in pool batch", pool_field))?;
    let existing_pool_col = pool_batch.column(pool_col_idx).clone();

    // Build and run the DataFusion aggregate.
    let aggr_expr = match reducer {
        Reducer::Collect   => array_agg(col(source_field)).alias("__agg"),
        Reducer::Sum       => df_sum(col(source_field)).alias("__agg"),
        Reducer::Max       => df_max(col(source_field)).alias("__agg"),
        Reducer::Min       => df_min(col(source_field)).alias("__agg"),
        Reducer::TakeFirst => df_first_value(col(source_field), vec![]).alias("__agg"),
    };
    let ctx = SessionContext::new();
    ctx.register_batch("src", source_batch)?;
    let agg_batches = ctx.table("src").await?
        .aggregate(vec![col(group_by)], vec![aggr_expr])?.collect().await?;
    let agg_schema = agg_batches.first().map(|b| b.schema())
        .unwrap_or_else(|| Arc::new(ArrowSchema::empty()));
    let agg_batch = concat_batches(&agg_schema, &agg_batches)?;

    // Build pool_idx → agg_row index map.
    let group_col = agg_batch.schema().index_of(group_by)
        .ok()
        .and_then(|i| agg_batch.column(i).as_any().downcast_ref::<UInt32Array>().map(|a| a.values().to_vec()))
        .unwrap_or_default();
    let agg_values_col = agg_batch.schema().index_of("__agg")
        .map(|i| agg_batch.column(i).clone())
        .unwrap_or_else(|_| Arc::new(arrow::array::Int32Array::from(vec![] as Vec<i32>)));
    let mut idx_map: HashMap<u32, usize> = HashMap::new();
    for (row, &pool_idx) in group_col.iter().enumerate() {
        idx_map.insert(pool_idx, row);
    }

    // Build the replacement column.
    let new_col: ArrayRef = match reducer {
        Reducer::Collect => {
            // Collect: build a ListArray; unmapped pool rows get an empty list.
            let element_type = match agg_values_col.data_type() {
                DataType::List(f) => f.data_type().clone(),
                other => bail!("CollectToPool: expected List from array_agg, got {other:?}"),
            };
            let agg_list = agg_values_col.as_any().downcast_ref::<ListArray>()
                .ok_or_else(|| anyhow!("CollectToPool: __agg column is not a ListArray"))?;
            let item_field = Arc::new(ArrowField::new("item", element_type.clone(), true));
            let mut offsets: Vec<i32> = vec![0];
            let mut child_slices: Vec<ArrayRef> = Vec::new();
            for pool_row in 0..pool_n {
                if let Some(&agg_row) = idx_map.get(&(pool_row as u32)) {
                    let slice = agg_list.value(agg_row);
                    offsets.push(offsets.last().unwrap() + slice.len() as i32);
                    child_slices.push(slice);
                } else {
                    offsets.push(*offsets.last().unwrap());
                }
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
            // Scalar reducers (Sum, Max, Min, TakeFirst): mapped pool rows get the aggregated
            // value; unmapped pool rows get the field's declared `default_val`.
            //
            // Implementation: concatenate [agg_col, default_col] into a combined array, then
            // `take` with indices that point into agg_col for mapped rows and into default_col
            // for unmapped rows.
            let agg_n = agg_batch.num_rows();
            let take_indices: UInt32Array = (0..pool_n as u32).map(|pool_row| {
                idx_map.get(&pool_row)
                    .map(|&agg_row| agg_row as u32)
                    .unwrap_or(agg_n as u32 + pool_row)
            }).collect::<Vec<u32>>().into();
            let default_col = yaml_value_to_array(default_val, existing_pool_col.data_type(), pool_n);
            let combined = concat(&[agg_values_col.as_ref(), default_col.as_ref()])?;
            take(combined.as_ref(), &take_indices, None)?
        }
    };

    // Replace pool_field column in pool_batch.
    let mut fields: Vec<Arc<ArrowField>> = pool_batch.schema().fields().to_vec();
    let mut columns = pool_batch.columns().to_vec();
    fields[pool_col_idx] = Arc::new(ArrowField::new(pool_field, new_col.data_type().clone(), true));
    columns[pool_col_idx] = new_col;
    computed.insert(pool_path.clone(), RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), columns)?);
    Ok(())
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
/// this function produces the expanded output batch for the sibling's own output file.
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
// Pool sampling helpers
// ---------------------------------------------------------------------------

/// Sample `count` indices from `[0, pool_size)` without replacement (Fisher-Yates).
///
/// Panics if `count > pool_size` — callers must enforce the planning-time check.
fn sample_pool_without_replacement(pool_size: usize, count: usize) -> Vec<u32> {
    assert!(count <= pool_size, "sample_pool_without_replacement: count {count} > pool_size {pool_size}");
    let mut indices: Vec<u32> = (0..pool_size as u32).collect();
    for i in 0..count {
        let j = (i as u64..pool_size as u64).fake::<u64>() as usize;
        indices.swap(i, j);
    }
    indices[..count].to_vec()
}

/// Sample `count` indices from `[0, pool_size)` with Polya-urn weighting.
///
/// Each initially-uniform weight is multiplied by `reinforcement` after selection,
/// making previously-selected indices more likely to be selected again.
/// `reinforcement > 1.0` produces clumping; `reinforcement = 1.0` degenerates to uniform.
fn sample_pool_weighted(pool_size: usize, count: usize, reinforcement: f64) -> Vec<u32> {
    let mut weights: Vec<f64> = vec![1.0; pool_size];
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
/// Used to remove the sentinel before emitting sibling output while retaining it in `computed`.
fn strip_slot_idx(batch: RecordBatch) -> RecordBatch {
    strip_sentinel(batch, "_slot_idx")
}

/// Strip `_pool_idx` from a batch before emitting a junction dataset's output.
/// The full batch (including `_pool_idx`) is retained in `computed` for `CollectToPool`.
fn strip_pool_idx(batch: RecordBatch) -> RecordBatch {
    strip_sentinel(batch, "_pool_idx")
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

/// Inject `_pool_idx` into `batch` for any junction links in `dataset`.
///
/// For each junction link (a link not referenced by any `content.group`), samples one pool
/// row per batch row and prepends a `_pool_idx: UInt32` column. The pool batch must already
/// be in `computed` (the DAG link-edge from pool → junction guarantees this).
///
/// For Stage 4, only the first junction link is processed (multi-link deferred).
fn inject_pool_idx(
    batch: &RecordBatch,
    path: &Path,
    dataset: &SyntheticDataset,
    computed: &HashMap<PathBuf, RecordBatch>,
) -> Result<RecordBatch> {
    let list_link_refs: HashSet<&str> = dataset.data.iter()
        .filter_map(|f| f.content.as_ref()?.group.as_deref())
        .collect();
    for link in &dataset.links {
        if list_link_refs.contains(link.reference.as_str()) { continue; }
        let Some(pool_path) = resolve_include(path, &link.file) else { continue };
        let Some(pool_batch) = computed.get(&pool_path) else { continue };
        let n_pool = pool_batch.num_rows();
        let n_eligible = match link.ratio {
            Some(r) => ((r * n_pool as f64).round() as usize).clamp(1, n_pool),
            None    => n_pool,
        };
        let n_rows = batch.num_rows();
        let r = link.reinforcement;
        let assignments: Vec<u32> = if r == Some(0.0) {
            sample_pool_without_replacement(n_eligible, n_rows)
        } else if let Some(reinf) = r.filter(|&v| v > 1.0) {
            sample_pool_weighted(n_eligible, n_rows, reinf)
        } else {
            (0..n_rows)
                .map(|_| (0u64..n_eligible as u64).fake::<u64>() as u32)
                .collect()
        };
        let pool_idx_arr: ArrayRef = Arc::new(UInt32Array::from(assignments));
        return prepend_column(batch, "_pool_idx", pool_idx_arr);
    }
    Ok(batch.clone())
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
        .filter(|f| f.expression.is_none() && !f.is_link_content())
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
/// first so that GenerateInnerFlat's n_eligible_slots boundary correctly identifies them.
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
