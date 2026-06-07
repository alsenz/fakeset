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
//! | `_slot_idx` | `UInt32` | `execute_lower_cover_group_core` (member batches) | `AssembleFromWitness` (fold into lists) | `strip_slot_idx` before member emit |
//! | `_staging_refs` | `List<UInt32>` | `execute_witness` | `execute_assemble_from_witness` (fold) | stripped during assembly |
//! | `_linked_idx` | `UInt32` | `inject_linked_idx` (junction) / `execute_witness` | `execute_accumulate_to_linked` | `strip_linked_idx` before junction emit |
use anyhow::{Result, anyhow, bail};
use arrow::array::{
    Array, ArrayRef, Float64Array, ListArray, StringArray, StringBuilder, StructArray, UInt32Array,
    new_empty_array,
};
use arrow::buffer::{OffsetBuffer, ScalarBuffer};
use arrow::compute::{concat, concat_batches, sort_to_indices, take};
use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};
use arrow::record_batch::{RecordBatch, RecordBatchOptions};
use datafusion::functions_aggregate::expr_fn::{
    array_agg, first_value as df_first_value, max as df_max, min as df_min, sum as df_sum,
};
use datafusion::prelude::{SessionContext, col};
use fake::Fake;
use serde_yaml::Value as YamlValue;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::arrow_util::{downcast, take_as};
use crate::constraints::FieldConstraints;
use crate::dq::apply_data_quality;
use crate::generator::{generate_column, sample_count};
use crate::import::{ImportIndex, filter_ring, load_import_index, resolve_import_path};
use crate::models::{
    CountSpec, Field, Format, Include, Range, Reducer, RingBounds, Schema, SeedConfig,
    SyntheticDataset, eligible_linked_rows, resolve_include, split_ref,
};
use crate::output::{filter_hidden_columns, write_output};
use crate::plan::{ExecutionPlan, ExecutionStep, InheritedField};
use crate::schema::{field_to_arrow, schema_to_arrow};
use crate::segment::{LowerCoverMember, Segment};

/// Execute the plan produced by `plan::build_plan`, writing outputs to `output_dir`.
///
/// Each step is interpreted in order with no branching on dataset shape:
/// row counts, lower cover segments, and inherited field wiring are all pre-resolved
/// in the plan.
pub async fn execute(
    plan: &ExecutionPlan,
    output_dir: &Path,
    seed_config: &SeedConfig,
) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;

    // Cache of fully-loaded import files, shared across all steps.
    let mut import_cache: HashMap<PathBuf, Arc<ImportIndex>> = HashMap::new();
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
            ExecutionStep::GenerateStagingNode {
                path,
                dataset,
                rows,
                inherited,
            } => {
                execute_dataset_core(
                    true,
                    false,
                    path,
                    dataset.as_ref(),
                    *rows,
                    inherited,
                    seed_config,
                    &mut import_cache,
                    &mut computed,
                    &mut shared,
                )
                .await?;
            }
            ExecutionStep::GenerateDataset {
                path,
                dataset,
                rows,
                inherited,
                defer_emit,
            } => {
                execute_dataset_core(
                    false,
                    *defer_emit,
                    path,
                    dataset.as_ref(),
                    *rows,
                    inherited,
                    seed_config,
                    &mut import_cache,
                    &mut computed,
                    &mut shared,
                )
                .await?;
            }
            ExecutionStep::GenerateStagingLowerCoverGroup {
                parent_path,
                parent,
                segments,
                members,
            } => {
                execute_lower_cover_group_core(
                    true,
                    false,
                    parent_path,
                    parent.as_ref(),
                    segments,
                    members,
                    seed_config,
                    &mut import_cache,
                    &mut computed,
                    &mut parent_computed,
                    &mut shared,
                )
                .await?;
            }
            ExecutionStep::GenerateLowerCoverGroup {
                parent_path,
                parent,
                segments,
                members,
                defer_emit,
            } => {
                execute_lower_cover_group_core(
                    false,
                    *defer_emit,
                    parent_path,
                    parent.as_ref(),
                    segments,
                    members,
                    seed_config,
                    &mut import_cache,
                    &mut computed,
                    &mut parent_computed,
                    &mut shared,
                )
                .await?;
            }
            ExecutionStep::GenerateWitness {
                witness_key,
                staging_path,
                list_field_name,
                inner_fields,
                include,
                cardinality,
                linked_path,
                slot_start,
                slot_count,
                segment_constraints,
                shard_q,
            } => {
                execute_witness(
                    witness_key,
                    staging_path,
                    list_field_name,
                    inner_fields,
                    include,
                    cardinality,
                    linked_path,
                    *slot_start,
                    *slot_count,
                    segment_constraints,
                    *shard_q,
                    &mut computed,
                )?;
            }
            // --- Accumulate-up phase: fold witness rows into list columns, propagate
            //     collected values back to linked datasets, emit final output files. ---
            ExecutionStep::AssembleFromWitness {
                staging_path,
                dataset,
                witness_specs,
            } => {
                execute_assemble_from_witness(
                    staging_path,
                    dataset.as_ref(),
                    witness_specs,
                    &mut computed,
                    &mut shared,
                )
                .await?;
            }
            ExecutionStep::AccumulateToLinked {
                source_path,
                source_field,
                linked_path,
                linked_field,
                group_by,
                reducer,
                default_val,
            } => {
                execute_accumulate_to_linked(
                    source_path,
                    source_field,
                    linked_path,
                    linked_field,
                    group_by,
                    reducer,
                    default_val,
                    &mut computed,
                    &mut accumulated_fields,
                )
                .await?;
            }
            ExecutionStep::EmitDataset { path, dataset } => {
                execute_emit_dataset(path, dataset.as_ref(), &computed, &mut shared).await?;
            }
            ExecutionStep::WriteSharedOutput {
                output_file,
                format,
                quality,
                schema,
            } => {
                let Some((_, batches)) = shared.get(output_file) else {
                    continue;
                };
                let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
                if total_rows > 0 {
                    let combined = union_and_shuffle(batches.clone(), output_file).await?;
                    let final_batch = match quality {
                        Some(q) => apply_data_quality(combined, q, schema)?,
                        None => combined,
                    };
                    write_output(&final_batch, output_file, format, output_dir, schema)?;
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
        let Some(batch) = computed.get(&ps.from_path) else {
            continue;
        };
        let Ok(col_idx) = batch.schema().index_of(&ps.from_column) else {
            continue;
        };
        map.entry(ps.into_column.clone())
            .or_default()
            .push(batch.column(col_idx).clone());
    }
    map
}

// ---------------------------------------------------------------------------
// Lower cover group execution
// ---------------------------------------------------------------------------

/// Accumulates per-segment batches across the lower cover group loop, tracking the
/// state needed for final parent-batch assembly and member emission.
///
/// The witness-source / non-witness-source split preserves the n_eligible_slots boundary
/// invariant: witness-source rows must precede non-witness-source rows in the assembled
/// parent batch so `GenerateWitness` correctly identifies the eligible linked-dataset slots.
struct SegmentBatchAccumulator {
    /// Parent-batch rows from segments containing at least one witness-source member.
    /// Placed first in the final batch to satisfy the n_eligible_slots boundary invariant.
    witness_source_parent_batches: Vec<RecordBatch>,
    non_witness_source_parent_batches: Vec<RecordBatch>,
    /// For staging nodes: parent rows in segment-declaration order (determines slot indices).
    ordered_staging_batches: Vec<RecordBatch>,
    /// Expanded or canonical member batches keyed by member path, for later emission.
    member_buffers: HashMap<PathBuf, Vec<RecordBatch>>,
    /// Running slot offset across segments; advanced by each segment's row count.
    slot_offset: usize,
}

impl SegmentBatchAccumulator {
    fn new() -> Self {
        Self {
            witness_source_parent_batches: Vec::new(),
            non_witness_source_parent_batches: Vec::new(),
            ordered_staging_batches: Vec::new(),
            member_buffers: HashMap::new(),
            slot_offset: 0,
        }
    }

    fn push_parent_batch(
        &mut self,
        batch: RecordBatch,
        is_staging: bool,
        seg_has_witness_source: bool,
    ) {
        if is_staging {
            self.ordered_staging_batches.push(batch);
        } else if seg_has_witness_source {
            self.witness_source_parent_batches.push(batch);
        } else {
            self.non_witness_source_parent_batches.push(batch);
        }
    }

    fn push_member_batch(&mut self, path: PathBuf, batch: RecordBatch) {
        self.member_buffers.entry(path).or_default().push(batch);
    }

    fn advance_slot_offset(&mut self, n_rows: usize) {
        self.slot_offset += n_rows;
    }

    /// Combine all accumulated segment batches into the final parent batch and return
    /// member buffers for subsequent member emission.
    async fn finalise(
        self,
        is_staging: bool,
        has_witness_sources: bool,
        schema: &Schema,
        name: &str,
    ) -> Result<(RecordBatch, HashMap<PathBuf, Vec<RecordBatch>>)> {
        let parent_batch = if is_staging {
            // Row order determines slot indices — concatenate in declaration order, no shuffle.
            let arrow_schema = self
                .ordered_staging_batches
                .first()
                .map(|b| b.schema())
                .unwrap_or_else(|| Arc::new(schema_to_arrow(schema)));
            concat_batches(&arrow_schema, &self.ordered_staging_batches)?
        } else if has_witness_sources && !self.witness_source_parent_batches.is_empty() {
            // Witness-source rows first (unshuffled), then shuffled remainder.
            combine_witness_source_first(
                self.witness_source_parent_batches,
                self.non_witness_source_parent_batches,
                schema,
                name,
            )
            .await?
        } else {
            let mut all = self.witness_source_parent_batches;
            all.extend(self.non_witness_source_parent_batches);
            combine_and_shuffle(all, schema, name).await?
        };
        Ok((parent_batch, self.member_buffers))
    }
}

/// Shared core for `GenerateStagingNode` (`is_staging=true`) and `GenerateDataset`
/// (`is_staging=false`). When staging, stores the scalar batch in `computed` with no
/// expression evaluation or emit. When not staging, evaluates expressions, handles
/// `_linked_idx` injection for junction links, and either emits or defers based on
/// `defer_emit` (collect-target deferral).
#[allow(clippy::too_many_arguments)]
async fn execute_dataset_core(
    is_staging: bool,
    defer_emit: bool,
    path: &Path,
    dataset: &SyntheticDataset,
    rows: usize,
    inherited: &[InheritedField],
    seed_config: &SeedConfig,
    import_cache: &mut HashMap<PathBuf, Arc<ImportIndex>>,
    computed: &mut HashMap<PathBuf, RecordBatch>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let inherited_map = resolve_inherited_fields(inherited, computed);
    let batch = if let Some(spec) = &dataset.import {
        let ring = spec.ring.clone().unwrap_or(RingBounds {
            start: 0.0,
            end: 1.0,
        });
        let idx = get_or_load_import(path, spec, seed_config.ring, import_cache)?;
        let import_batch = filter_ring(&idx, &ring)?;
        generate_batch_with_import(
            &dataset.data,
            &inherited_map,
            &HashMap::new(),
            &import_batch,
        )?
    } else {
        generate_batch(&dataset.data, rows, &inherited_map, &HashMap::new())?
    };
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
        let output =
            filter_hidden_columns(strip_sentinel(batch.clone(), "_linked_idx"), &dataset.data)?;
        computed.insert(path.to_path_buf(), batch);
        if !defer_emit {
            emit_batch(output, dataset, shared)?;
        }
        // When defer_emit is true (collect target): batch stored, file write deferred
        // to the EmitDataset step that follows AccumulateToLinked.
    }
    Ok(())
}

/// Shared core for `GenerateStagingLowerCoverGroup` (`is_staging=true`) and
/// `GenerateLowerCoverGroup` (`is_staging=false`).
#[allow(clippy::too_many_arguments)]
async fn execute_lower_cover_group_core(
    is_staging: bool,
    defer_emit: bool,
    path: &Path,
    dataset: &SyntheticDataset,
    segments: &[Segment],
    members: &[LowerCoverMember],
    seed_config: &SeedConfig,
    import_cache: &mut HashMap<PathBuf, Arc<ImportIndex>>,
    computed: &mut HashMap<PathBuf, RecordBatch>,
    parent_computed: &mut HashSet<PathBuf>,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    let witness_source_paths: HashSet<PathBuf> = members
        .iter()
        .filter(|m| m.is_witness_source)
        .map(|m| m.path.clone())
        .collect();
    let has_witness_sources = !witness_source_paths.is_empty();

    let mut acc = SegmentBatchAccumulator::new();

    for seg in segments {
        if seg.rows == 0 {
            continue;
        }

        // For imported parents, load this segment's ring slice now so we know the
        // actual row count (hash distribution may differ slightly from seg.rows).
        let (n_rows, opt_import_batch) = if let Some(spec) = &dataset.import {
            let ring = seg.ring.clone().unwrap_or(RingBounds {
                start: 0.0,
                end: 1.0,
            });
            let idx = get_or_load_import(path, spec, seed_config.ring, import_cache)?;
            let ib = filter_ring(&idx, &ring)?;
            if ib.num_rows() == 0 {
                acc.advance_slot_offset(seg.rows);
                continue; // ring slice is ⊥ — skip segment
            }
            (ib.num_rows(), Some(ib))
        } else {
            (seg.rows, None)
        };

        let seg_has_witness_source = seg
            .members
            .iter()
            .any(|mp| witness_source_paths.contains(mp));

        let parent_seg = if seg.members.is_empty() {
            generate_remainder_parent_batch(
                &dataset.data,
                n_rows,
                &seg.field_constraints,
                opt_import_batch.as_ref(),
            )?
        } else {
            let real_members: Vec<&LowerCoverMember> = seg
                .members
                .iter()
                .filter_map(|mp| {
                    if witness_source_paths.contains(mp) {
                        return None;
                    }
                    members.iter().find(|m| &m.path == mp)
                })
                .collect();

            if real_members.is_empty() {
                // Witness-source-only segment: parent is generated as a remainder.
                generate_remainder_parent_batch(
                    &dataset.data,
                    n_rows,
                    &seg.field_constraints,
                    opt_import_batch.as_ref(),
                )?
            } else {
                let atom_batch = generate_segment_atom_batch(
                    &dataset.data,
                    &real_members,
                    n_rows,
                    &seg.field_constraints,
                    opt_import_batch.as_ref(),
                    computed,
                    parent_computed,
                )?;

                // Project each member from the atom batch (order does not matter —
                // the atom is already final).
                for m in &real_members {
                    let pre = parent_computed
                        .contains(&m.path)
                        .then(|| computed.get(&m.path))
                        .flatten();
                    project_member_columns(&atom_batch, m, acc.slot_offset, n_rows, pre, &mut acc)?;
                }

                project_parent_columns_from_atom(
                    &dataset.data,
                    &atom_batch,
                    n_rows,
                    &seg.field_constraints,
                    opt_import_batch.as_ref(),
                )?
            }
        };

        acc.push_parent_batch(parent_seg, is_staging, seg_has_witness_source);
        acc.advance_slot_offset(n_rows);
    }

    let (parent_shuffled, mut member_buffers) = acc
        .finalise(
            is_staging,
            has_witness_sources,
            &dataset.data,
            &dataset.name,
        )
        .await?;

    if is_staging {
        // Scalar-only intermediate; AssembleFromWitness adds list columns, evaluates
        // expressions, and emits.
        computed.insert(path.to_path_buf(), parent_shuffled);
    } else {
        let parent_shuffled = evaluate_expressions(parent_shuffled, dataset).await?;
        let parent_output = filter_hidden_columns(parent_shuffled.clone(), &dataset.data)?;
        computed.insert(path.to_path_buf(), parent_shuffled);
        if !defer_emit {
            emit_batch(parent_output, dataset, shared)?;
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
        )
        .await?;
        let m_shuffled = evaluate_expressions(m_shuffled, &m.dataset).await?;
        // For junction link members: sample one linked row per row and prepend as _linked_idx.
        // The linked batch must already be in `computed` (DAG link-edge ordering guarantees this).
        let m_shuffled = inject_linked_idx(&m_shuffled, &m.path, &m.dataset, computed)?;
        let m_output = filter_hidden_columns(
            strip_sentinel(
                strip_sentinel(m_shuffled.clone(), "_slot_idx"),
                "_linked_idx",
            ),
            &m.dataset.data,
        )?;
        computed.insert(m.path.clone(), m_shuffled);
        emit_batch(m_output, &m.dataset, shared)?;
    }

    Ok(())
}

fn prepend_column(batch: &RecordBatch, name: &str, col: ArrayRef) -> Result<RecordBatch> {
    let mut fields: Vec<Arc<ArrowField>> = vec![Arc::new(ArrowField::new(
        name,
        col.data_type().clone(),
        false,
    ))];
    fields.extend(batch.schema().fields().iter().cloned());
    let mut cols: Vec<ArrayRef> = vec![col];
    cols.extend(batch.columns().iter().cloned());
    Ok(RecordBatch::try_new(
        Arc::new(ArrowSchema::new(fields)),
        cols,
    )?)
}

// ---------------------------------------------------------------------------
// Witness generation
// ---------------------------------------------------------------------------

/// Draw strategy for `overlap: 0` — each staging slot owns an exclusive index-contiguous
/// shard of the eligible linked rows. `shard_q` rows per shard, pre-computed by the planner.
///
/// Returns `(slot_assignments, staging_idxs, surviving_indices)` where `surviving_indices`
/// is the identity map (slot_assignments are already absolute pre-filter indices).
#[allow(clippy::too_many_arguments)]
fn draw_exclusive_shards(
    slot_start: usize,
    slot_count: usize,
    eligible_linked: &RecordBatch,
    n_eligible_pre_filter: usize,
    shard_q: usize,
    cardinality: &CountSpec,
    segment_constraints: &HashMap<String, FieldConstraints>,
    include: &Include,
) -> Result<(UInt32Array, Vec<u32>, Vec<u32>)> {
    let mut all_assignments: Vec<u32> = Vec::new();
    let mut all_staging: Vec<u32> = Vec::new();
    for i in 0..slot_count {
        let abs_slot = slot_start + i;
        let shard_start = abs_slot * shard_q;
        let shard_len = shard_q.min(n_eligible_pre_filter.saturating_sub(shard_start));
        if shard_len == 0 {
            continue;
        }
        let shard = eligible_linked.slice(shard_start, shard_len);
        let (filtered_shard, surviving_shard) =
            filter_batch_by_constraints(&shard, segment_constraints)?;
        let n_shard = filtered_shard.num_rows();
        if n_shard == 0 {
            continue;
        }
        let m_n = sample_count(cardinality);
        let m_n = if include.reinforcement == Some(0.0) {
            m_n.min(n_shard)
        } else {
            m_n
        };
        let reinf = include.reinforcement.unwrap_or(1.0);
        let local_samples = if include.reinforcement == Some(0.0) {
            sample_linked_without_replacement(n_shard, m_n)
        } else if reinf > 1.0 {
            sample_linked_weighted(n_shard, m_n, reinf)
        } else {
            (0..m_n)
                .map(|_| (0u64..n_shard as u64).fake::<u64>() as u32)
                .collect()
        };
        for local_idx in local_samples {
            let abs_idx = shard_start as u32 + surviving_shard[local_idx as usize];
            all_assignments.push(abs_idx);
            all_staging.push(abs_slot as u32);
        }
    }
    // Identity surviving_indices: assignments are already absolute pre-filter indices.
    let identity: Vec<u32> = (0..n_eligible_pre_filter as u32).collect();
    Ok((UInt32Array::from(all_assignments), all_staging, identity))
}

/// Draw strategy for the default overlap mode (absent or ≥ 1) — all staging slots share
/// one pre-filtered set of eligible linked rows. Sub-modes:
/// - `reinforcement: 0`  → Fisher-Yates without-replacement per slot
/// - `overlap > 1`       → power-law initial weights + optional Pólya urn
/// - default             → uniform with-replacement
///
/// The caller is responsible for filtering `eligible_linked` to `filtered_linked` and
/// supplying the corresponding `surviving` index map before calling this function.
///
/// Returns `(slot_assignments, staging_idxs, surviving_indices)`.
fn draw_shared_linked(
    slot_start: usize,
    slot_count: usize,
    filtered_linked: &RecordBatch,
    surviving: Vec<u32>,
    cardinality: &CountSpec,
    include: &Include,
) -> (UInt32Array, Vec<u32>, Vec<u32>) {
    let n_eligible = filtered_linked.num_rows();
    let counts: Vec<usize> = if n_eligible == 0 {
        vec![0; slot_count]
    } else {
        (0..slot_count)
            .map(|_| {
                let m_n = sample_count(cardinality);
                if include.reinforcement == Some(0.0) {
                    m_n.min(n_eligible)
                } else {
                    m_n
                }
            })
            .collect()
    };
    let total: usize = counts.iter().sum();
    let s_idxs: Vec<u32> = counts
        .iter()
        .enumerate()
        .flat_map(|(i, &c)| std::iter::repeat_n((slot_start + i) as u32, c))
        .collect();
    // Sampling mode:
    //   reinforcement:0               → Fisher-Yates without-replacement per slot
    //   overlap>1 (with/without reinf) → power-law initial weights + optional Pólya urn
    //   default                       → uniform with-replacement
    let s_assignments: UInt32Array = if n_eligible == 0 {
        UInt32Array::from(Vec::<u32>::new())
    } else {
        let r = include.reinforcement;
        let ov = include.overlap;
        if r == Some(0.0) {
            counts
                .iter()
                .flat_map(|&m_n| sample_linked_without_replacement(n_eligible, m_n))
                .collect::<Vec<u32>>()
                .into()
        } else if let Some(ov_val) = ov.filter(|&v| v > 1.0) {
            // Power-law initial weights: row j has weight (n_eligible - j)^(overlap - 1).
            // Row 0 is most popular; relies on union_and_shuffle having randomised the batch.
            let reinf = r.unwrap_or(1.0);
            let initial_weights: Vec<f64> = (0..n_eligible)
                .map(|j| ((n_eligible - j) as f64).powf(ov_val - 1.0))
                .collect();
            counts
                .iter()
                .flat_map(|&m_n| sample_with_polya(initial_weights.clone(), m_n, reinf))
                .collect::<Vec<u32>>()
                .into()
        } else if let Some(reinf) = r.filter(|&v| v > 1.0) {
            counts
                .iter()
                .flat_map(|&m_n| sample_linked_weighted(n_eligible, m_n, reinf))
                .collect::<Vec<u32>>()
                .into()
        } else {
            (0..total)
                .map(|_| (0u64..n_eligible as u64).fake::<u64>() as u32)
                .collect::<Vec<u32>>()
                .into()
        }
    };
    (s_assignments, s_idxs, surviving)
}

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
        let Ok(col_idx) = batch.schema().index_of(field_name) else {
            continue;
        };
        let col = batch.column(col_idx);
        for (row, m) in mask.iter_mut().enumerate() {
            if *m {
                *m = row_satisfies_field_constraints(col, row, fc);
            }
        }
    }

    let surviving: Vec<u32> = (0..batch.num_rows() as u32)
        .filter(|&i| mask[i as usize])
        .collect();
    let indices = UInt32Array::from(surviving.clone());
    let fields: Vec<Arc<ArrowField>> = batch.schema().fields().to_vec();
    let cols: Vec<ArrayRef> = batch
        .columns()
        .iter()
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
                if let Some(arr) = col.as_any().downcast_ref::<StringArray>()
                    && !col.is_null(row)
                    && arr.value(row) != expected.as_str()
                {
                    return false;
                }
            }
            YamlValue::Number(n) => {
                if let Some(expected) = n.as_f64()
                    && let Some(arr) = col.as_any().downcast_ref::<Float64Array>()
                    && !col.is_null(row)
                    && (arr.value(row) - expected).abs() > 1e-9
                {
                    return false;
                }
            }
            YamlValue::Bool(expected) => {
                if let Some(arr) = col.as_any().downcast_ref::<BooleanArray>()
                    && !col.is_null(row)
                    && arr.value(row) != *expected
                {
                    return false;
                }
            }
            _ => {}
        }
    }

    if let Some(arr) = col.as_any().downcast_ref::<Float64Array>() {
        let v = arr.value(row);
        if let Some(min) = fc.min
            && v < min
        {
            return false;
        }
        if let Some(max) = fc.max
            && v > max
        {
            return false;
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
#[allow(clippy::too_many_arguments)]
fn execute_witness(
    witness_key: &Path,
    staging_path: &Path,
    list_field_name: &str,
    inner_fields: &[Field],
    include: &Include,
    cardinality: &CountSpec,
    linked_path: &PathBuf,
    slot_start: usize,
    slot_count: usize,
    segment_constraints: &HashMap<String, FieldConstraints>,
    shard_q: Option<usize>,
    computed: &mut HashMap<PathBuf, RecordBatch>,
) -> Result<()> {
    if !computed.contains_key(staging_path) {
        return Err(anyhow!(
            "witness '{list_field_name}': staging batch not yet computed"
        ));
    }
    let linked_batch = computed
        .get(linked_path)
        .ok_or_else(|| anyhow!("witness '{list_field_name}': linked-slot batch not yet computed"))?
        .clone();

    // --- Phase 1: assign each draw to a staging slot and a linked slot ---
    // Eligible linked rows: apply ratio to determine the eligible prefix.
    let n_eligible_pre_filter = eligible_linked_rows(linked_batch.num_rows(), include.ratio);
    let eligible_linked = linked_batch.slice(0, n_eligible_pre_filter);

    // Two sampling strategies depending on overlap mode:
    //
    // overlap:0 — each staging slot draws from an exclusive shard of the pre-filter eligible
    //   linked rows (`draw_exclusive_shards`). Shards are index-contiguous; surviving_indices is the
    //   identity map so Phases 2–4 are unchanged.
    //
    // default (overlap absent/≥1) — filter the full eligible set once, then all staging
    //   slots draw against that shared filtered view (`draw_shared_linked`). Sub-modes for
    //   reinforcement and power-law overlap are handled inside the function.
    let (slot_assignments, staging_idxs, surviving_indices) = if include.overlap == Some(0.0) {
        draw_exclusive_shards(
            slot_start,
            slot_count,
            &eligible_linked,
            n_eligible_pre_filter,
            shard_q.unwrap_or(0),
            cardinality,
            segment_constraints,
            include,
        )?
    } else {
        let (filtered_linked, surviving) =
            filter_batch_by_constraints(&eligible_linked, segment_constraints)?;
        draw_shared_linked(
            slot_start,
            slot_count,
            &filtered_linked,
            surviving,
            cardinality,
            include,
        )
    };
    let total = slot_assignments.len();

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
                let idx = eligible_linked
                    .schema()
                    .index_of(target_col)
                    .map_err(|_| anyhow!("column '{target_col}' not found in linked-slot batch"))?;
                take(
                    eligible_linked.column(idx).as_ref(),
                    &unique_linked_arr,
                    None,
                )?
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
    let with_refs = prepend_column(
        &data_batch,
        "_staging_refs",
        Arc::new(staging_refs_array) as ArrayRef,
    )?;
    let witness_batch = prepend_column(
        &with_refs,
        "_linked_idx",
        Arc::new(UInt32Array::from(unique_linked_idxs)) as ArrayRef,
    )?;
    computed.insert(witness_key.to_path_buf(), witness_batch);
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
fn unnest_staging_refs(witness: &RecordBatch) -> Result<(RecordBatch, UInt32Array, UInt32Array)> {
    let refs_col_idx = witness
        .schema()
        .index_of("_staging_refs")
        .map_err(|_| anyhow!("unnest_staging_refs: '_staging_refs' not found in witness"))?;
    let staging_refs =
        downcast::<ListArray>(witness.column(refs_col_idx).as_ref(), "_staging_refs")?;

    let total: usize = (0..witness.num_rows())
        .map(|r| staging_refs.value(r).len())
        .sum();

    let mut slot_idxs: Vec<u32> = Vec::with_capacity(total);
    let mut witness_row_idxs: Vec<u32> = Vec::with_capacity(total);
    for wr in 0..witness.num_rows() {
        let refs_slice = staging_refs.value(wr);
        let refs_arr = downcast::<UInt32Array>(refs_slice.as_ref(), "_staging_refs list element")?;
        for &slot in refs_arr.values() {
            slot_idxs.push(slot);
            witness_row_idxs.push(wr as u32);
        }
    }
    let slot_arr = UInt32Array::from(slot_idxs);
    let witness_row_arr = UInt32Array::from(witness_row_idxs);

    // Sort by slot_idx: required for the offset-based list-fold.
    let sort_order = sort_to_indices(&slot_arr, None, None)?;
    let slot_arr_sorted: UInt32Array = take_as(&slot_arr, &sort_order)?;
    let witness_row_arr_sorted: UInt32Array = take_as(&witness_row_arr, &sort_order)?;

    // Strip sentinels; replicate remaining witness columns per slot.
    let stripped = strip_sentinel(
        strip_sentinel(witness.clone(), "_linked_idx"),
        "_staging_refs",
    );
    let mut fields = vec![ArrowField::new("_slot_idx", DataType::UInt32, false)];
    let mut cols: Vec<ArrayRef> = vec![Arc::new(slot_arr_sorted.clone())];
    for col_idx in 0..stripped.num_columns() {
        fields.push(stripped.schema().field(col_idx).as_ref().clone());
        cols.push(take(
            stripped.column(col_idx).as_ref(),
            &witness_row_arr_sorted,
            None,
        )?);
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
    let mut batch = computed
        .get(staging_path)
        .ok_or_else(|| {
            anyhow!(
                "assemble from witness '{}': staging batch not yet computed",
                dataset.name
            )
        })?
        .clone();

    for (field_name, witness_keys, project_col) in witness_specs {
        // Collect and union all per-segment witness batches for this field.
        let witness_batches: Vec<RecordBatch> = witness_keys.iter()
            .map(|key| computed.get(key).cloned().ok_or_else(|| {
                anyhow!("assemble from witness '{}': witness segment for '{field_name}' not computed", dataset.name)
            }))
            .collect::<Result<Vec<_>>>()?;
        let witness_schema = witness_batches
            .first()
            .ok_or_else(|| {
                anyhow!(
                    "assemble from witness '{}': no witness batches for '{field_name}'",
                    dataset.name
                )
            })?
            .schema();
        let witness = concat_batches(&witness_schema, &witness_batches)?;

        let staging_n = batch.num_rows();

        // Unnest _staging_refs to reconstruct the anonymous junction table.
        let (mut junction, slot_arr_sorted, _witness_row_arr_sorted) =
            unnest_staging_refs(&witness)?;

        // Identify outer-scoped fields: defined in the content schema but absent from the
        // witness (because execute_witness skips them). Look them up from the staging batch.
        let content_field_defs = dataset
            .data
            .iter()
            .find(|f| &f.name == field_name)
            .and_then(|f| f.content.as_deref())
            .map(|c| c.item.fields.as_slice())
            .unwrap_or(&[]);
        let witness_schema = witness.schema();
        let stripped_witness_cols: HashSet<&str> = witness_schema
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .filter(|&n| n != "_linked_idx" && n != "_staging_refs")
            .collect();
        let staging = batch.clone();
        for cf in content_field_defs {
            if stripped_witness_cols.contains(cf.name.as_str()) {
                continue;
            }
            if let Some(ref_str) = cf.simple_ref() {
                // Bare ref (no qualifier) or qualified ref: resolve the column name in staging.
                let col_name = split_ref(ref_str).map(|(_, c)| c).unwrap_or(ref_str);
                let stg_idx = staging.schema().index_of(col_name).map_err(|_| {
                    anyhow!("outer-scoped column '{col_name}' not found in staging batch")
                })?;
                let col = take(staging.column(stg_idx).as_ref(), &slot_arr_sorted, None)?;
                let arrow_field = ArrowField::new(cf.name.as_str(), col.data_type().clone(), true);
                junction = add_column(junction, arrow_field, col)?;
            }
        }

        // Slot-grouped list fold: count rows per slot, build offsets, fold into ListArray.
        let slot_idx_arr = downcast::<UInt32Array>(
            junction
                .column(
                    junction
                        .schema()
                        .index_of("_slot_idx")
                        .map_err(|_| anyhow!("junction missing '_slot_idx'"))?,
                )
                .as_ref(),
            "_slot_idx",
        )?
        .clone();

        let mut counts = vec![0usize; staging_n];
        for &idx in slot_idx_arr.values() {
            counts[idx as usize] += 1;
        }

        let inner = strip_sentinel(junction, "_slot_idx");
        let offsets = OffsetBuffer::<i32>::from_lengths(counts.iter().copied());

        let list_col: ArrayRef = if let Some(col_name) = project_col {
            let col_idx = inner.schema().index_of(col_name.as_str()).map_err(|_| {
                anyhow!("project: column '{col_name}' not found in witness for '{field_name}'")
            })?;
            let col = inner.column(col_idx).clone();
            let item_field = Arc::new(ArrowField::new("item", col.data_type().clone(), true));
            Arc::new(ListArray::new(item_field, offsets, col, None))
        } else {
            let hidden_item_cols: HashSet<&str> = dataset
                .data
                .iter()
                .find(|f| &f.name == field_name)
                .and_then(|f| f.content.as_deref())
                .map(|c| {
                    c.item
                        .fields
                        .iter()
                        .filter(|f| f.hidden)
                        .map(|f| f.name.as_str())
                        .collect()
                })
                .unwrap_or_default();
            let (struct_fields, struct_cols): (Vec<_>, Vec<_>) = inner
                .schema()
                .fields()
                .iter()
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

        let list_arrow_field =
            ArrowField::new(field_name.as_str(), list_col.data_type().clone(), true);
        batch = add_column(batch, list_arrow_field, list_col)?;
    }

    let batch = evaluate_expressions(batch, dataset).await?;
    let output = filter_hidden_columns(batch.clone(), &dataset.data)?;
    computed.insert(staging_path.clone(), batch);
    emit_batch(output, dataset, shared)?;
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
            let s = match val {
                YamlValue::String(s) => s.as_str(),
                _ => "",
            };
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
#[allow(clippy::too_many_arguments)]
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
    let raw_source = computed
        .get(source_path)
        .ok_or_else(|| {
            anyhow!(
                "AccumulateToLinked: source batch '{}' not computed",
                source_path.display()
            )
        })?
        .clone();

    // If the source is a Stage-4 witness batch (has `_staging_refs`), expand it to a flat
    // junction table: one row per (staging-slot, linked-row) draw. This restores the K entries
    // per linked row that the aggregation needs, since the witness carries only 1 row per unique
    // linked-row draw with `_staging_refs` encoding the back-references.
    let source_batch = if raw_source.schema().index_of("_staging_refs").is_ok() {
        let refs_col_idx = raw_source.schema().index_of("_staging_refs")?;
        let staging_refs = downcast::<ListArray>(
            raw_source.column(refs_col_idx).as_ref(),
            "AccumulateToLinked: _staging_refs",
        )?;
        let total: usize = (0..raw_source.num_rows())
            .map(|r| staging_refs.value(r).len())
            .sum();
        let mut witness_row_idxs: Vec<u32> = Vec::with_capacity(total);
        for wr in 0..raw_source.num_rows() {
            let n = staging_refs.value(wr).len();
            for _ in 0..n {
                witness_row_idxs.push(wr as u32);
            }
        }
        let witness_row_arr = UInt32Array::from(witness_row_idxs);
        // Strip _staging_refs; keep _linked_idx and content columns (replicated per draw).
        let stripped = strip_sentinel(raw_source, "_staging_refs");
        let fields: Vec<ArrowField> = stripped
            .schema()
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        let cols: Vec<ArrayRef> = stripped
            .columns()
            .iter()
            .map(|c| take(c.as_ref(), &witness_row_arr, None))
            .collect::<Result<_, _>>()?;
        RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), cols)?
    } else {
        raw_source
    };

    let linked_batch = computed
        .get(linked_path)
        .ok_or_else(|| {
            anyhow!(
                "AccumulateToLinked: linked batch '{}' not computed",
                linked_path.display()
            )
        })?
        .clone();
    let linked_n = linked_batch.num_rows();

    // Locate the linked field upfront — needed for scalar reducers to keep existing defaults.
    let linked_col_idx = linked_batch.schema().index_of(linked_field).map_err(|_| {
        anyhow!(
            "AccumulateToLinked: field '{}' not found in linked batch",
            linked_field
        )
    })?;
    let existing_linked_col = linked_batch.column(linked_col_idx).clone();

    // Build and run the DataFusion aggregate.
    let aggr_expr = match reducer {
        Reducer::Collect => array_agg(col(source_field)).alias("__agg"),
        Reducer::Sum => df_sum(col(source_field)).alias("__agg"),
        Reducer::Max => df_max(col(source_field)).alias("__agg"),
        Reducer::Min => df_min(col(source_field)).alias("__agg"),
        Reducer::TakeOne => df_first_value(col(source_field), vec![]).alias("__agg"),
    };
    let ctx = SessionContext::new();
    ctx.register_batch("src", source_batch)?;
    let agg_batches = ctx
        .table("src")
        .await?
        .aggregate(vec![col(group_by)], vec![aggr_expr])?
        .collect()
        .await?;
    let agg_schema = agg_batches
        .first()
        .map(|b| b.schema())
        .unwrap_or_else(|| Arc::new(ArrowSchema::empty()));
    let agg_batch = concat_batches(&agg_schema, &agg_batches)?;

    // Build linked_idx → agg_row index map.
    let group_col = agg_batch
        .schema()
        .index_of(group_by)
        .ok()
        .and_then(|i| {
            agg_batch
                .column(i)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .map(|a| a.values().to_vec())
        })
        .unwrap_or_default();
    let agg_values_col = agg_batch
        .schema()
        .index_of("__agg")
        .map(|i| agg_batch.column(i).clone())
        .unwrap_or_else(|_| Arc::new(arrow::array::Int32Array::from(vec![] as Vec<i32>)));
    let mut idx_map: HashMap<u32, usize> = HashMap::new();
    for (row, &linked_idx) in group_col.iter().enumerate() {
        idx_map.insert(linked_idx, row);
    }

    // Build the replacement column.
    // For the FIRST accumulation into a (linked_path, linked_field) pair, discard any
    // generator-produced initial values. For SUBSEQUENT calls (multi-segment staging),
    // carry forward already-accumulated values so all segments combine rather than overwrite.
    let is_first = accumulated_fields.insert((linked_path.clone(), linked_field.to_string()));
    let new_col: ArrayRef = match reducer {
        Reducer::Collect => {
            let element_type = match agg_values_col.data_type() {
                DataType::List(f) => f.data_type().clone(),
                other => bail!("AccumulateToLinked: expected List from array_agg, got {other:?}"),
            };
            let agg_list =
                downcast::<ListArray>(agg_values_col.as_ref(), "AccumulateToLinked: __agg column")?;
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
                let refs: Vec<&dyn arrow::array::Array> =
                    child_slices.iter().map(|a| a.as_ref()).collect();
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
            if !is_first && matches!(reducer, Reducer::TakeOne) {
                existing_linked_col.clone()
            } else if !is_first {
                accumulate_scalar_cumulative(
                    reducer,
                    &existing_linked_col,
                    &agg_values_col,
                    &idx_map,
                    linked_n,
                )?
            } else {
                let agg_n = agg_batch.num_rows();
                let take_indices: UInt32Array = (0..linked_n as u32)
                    .map(|linked_row| {
                        idx_map
                            .get(&linked_row)
                            .map(|&agg_row| agg_row as u32)
                            .unwrap_or(agg_n as u32 + linked_row)
                    })
                    .collect::<Vec<u32>>()
                    .into();
                let default_col =
                    yaml_value_to_array(default_val, existing_linked_col.data_type(), linked_n);
                let combined = concat(&[agg_values_col.as_ref(), default_col.as_ref()])?;
                take(combined.as_ref(), &take_indices, None)?
            }
        }
    };

    // Replace linked_field column in linked_batch.
    let mut fields: Vec<Arc<ArrowField>> = linked_batch.schema().fields().to_vec();
    let mut columns = linked_batch.columns().to_vec();
    fields[linked_col_idx] = Arc::new(ArrowField::new(
        linked_field,
        new_col.data_type().clone(),
        true,
    ));
    columns[linked_col_idx] = new_col;
    computed.insert(
        linked_path.clone(),
        RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), columns)?,
    );
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
            let existing = downcast::<Float64Array>(
                existing_col.as_ref(),
                "accumulate_scalar_cumulative: existing column",
            )?;
            let agg = downcast::<Float64Array>(
                agg_values_col.as_ref(),
                "accumulate_scalar_cumulative: agg column",
            )?;
            let result: Vec<f64> = (0..linked_n as u32)
                .map(|linked_row| {
                    let ev = if existing.is_null(linked_row as usize) {
                        0.0
                    } else {
                        existing.value(linked_row as usize)
                    };
                    if let Some(&agg_row) = idx_map.get(&linked_row) {
                        let av = if agg.is_null(agg_row) {
                            0.0
                        } else {
                            agg.value(agg_row)
                        };
                        match reducer {
                            Reducer::Sum => ev + av,
                            Reducer::Max => f64::max(ev, av),
                            Reducer::Min => f64::min(ev, av),
                            _ => ev,
                        }
                    } else {
                        ev
                    }
                })
                .collect();
            Ok(Arc::new(Float64Array::from(result)))
        }
        DataType::Utf8 => {
            let existing = downcast::<StringArray>(
                existing_col.as_ref(),
                "accumulate_scalar_cumulative: existing column",
            )?;
            let agg = downcast::<StringArray>(
                agg_values_col.as_ref(),
                "accumulate_scalar_cumulative: agg column",
            )?;
            let mut builder = StringBuilder::new();
            for linked_row in 0..linked_n as u32 {
                let ev = if existing.is_null(linked_row as usize) {
                    ""
                } else {
                    existing.value(linked_row as usize)
                };
                if let Some(&agg_row) = idx_map.get(&linked_row) {
                    let av = if agg.is_null(agg_row) {
                        ""
                    } else {
                        agg.value(agg_row)
                    };
                    let chosen = match reducer {
                        Reducer::Max => {
                            if av > ev {
                                av
                            } else {
                                ev
                            }
                        }
                        Reducer::Min => {
                            if av < ev {
                                av
                            } else {
                                ev
                            }
                        }
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
    let batch = computed
        .get(path)
        .ok_or_else(|| anyhow!("EmitDataset: batch at '{}' not computed", path.display()))?
        .clone();
    let output = filter_hidden_columns(batch, &dataset.data)?;
    emit_batch(output, dataset, shared)
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

/// Generate a batch for an imported dataset:
/// - Tainted fields are taken directly from `import_batch` (ring-filtered file rows).
/// - Non-tainted, non-expression, non-list-link fields are generated synthetically.
///
/// Row count is `import_batch.num_rows()`; `inherited` and `overrides` apply only to
/// synthetic fields.
fn generate_batch_with_import(
    schema: &Schema,
    inherited: &HashMap<String, Vec<ArrayRef>>,
    overrides: &HashMap<String, FieldConstraints>,
    import_batch: &RecordBatch,
) -> Result<RecordBatch> {
    let n = import_batch.num_rows();
    let arrow_schema = Arc::new(schema_to_arrow(schema));
    let columns = schema
        .iter()
        .filter(|f| f.expression.is_none() && !f.is_list_link())
        .map(|f| -> Result<ArrayRef> {
            if f.imported_taint {
                let col_idx = import_batch.schema().index_of(&f.name).map_err(|_| {
                    anyhow!("imported column '{}' not found in import batch", f.name)
                })?;
                Ok(import_batch.column(col_idx).clone())
            } else {
                let prefix = inherited
                    .get(&f.name)
                    .map_or(&[] as &[ArrayRef], |v| v.as_slice());
                let effective = overrides.get(&f.name).map(|fc| apply_constraints(f, fc));
                generate_column(effective.as_ref().unwrap_or(f), n, prefix)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RecordBatch::try_new(arrow_schema, columns)?)
}

/// Generate the parent batch for a remainder segment (no members) — extracted from
/// the legacy `seg.members.is_empty()` branch so all three segment shapes
/// (remainder, singleton atom, joint atom) call into the same per-segment dispatcher.
fn generate_remainder_parent_batch(
    parent_schema: &Schema,
    n_rows: usize,
    seg_constraints: &HashMap<String, FieldConstraints>,
    opt_import_batch: Option<&RecordBatch>,
) -> Result<RecordBatch> {
    if let Some(ib) = opt_import_batch {
        generate_batch_with_import(parent_schema, &HashMap::new(), seg_constraints, ib)
    } else {
        generate_fresh_batch(parent_schema, n_rows, seg_constraints)
    }
}

// ---------------------------------------------------------------------------
// Segment atom batch generation (SEG-ATOM-1)
//
// The unified atom batch contains *only* the parent-ref columns shared across
// one or more real (non-witness-source) members in a segment. Member-specific
// non-ref columns are generated inside `project_member_columns`; the parent's
// non-shared fields are generated inside `project_parent_columns_from_atom`.
// Referential integrity for shared ref columns is therefore structural: every
// member and the parent draw from the same atom column for a given segment slot.
// ---------------------------------------------------------------------------

/// Build the unified shared-ref schema for a segment atom.
///
/// Walks `parent_schema` in declaration order so the atom-batch column ordering is
/// deterministic. A parent field becomes an atom column iff at least one real
/// member declares a `ref:` pointing at it. Per-segment constraint overrides from
/// `seg_constraints` are applied to each entry.
///
/// Returns `(atom_fields, providing_members)` where `providing_members[X]` is the
/// ordered list of member indices (into `members`) that ref parent column `X`.
fn build_segment_atom_schema(
    parent_schema: &Schema,
    members: &[&LowerCoverMember],
    seg_constraints: &HashMap<String, FieldConstraints>,
) -> (Vec<Field>, HashMap<String, Vec<usize>>) {
    let per_member_refs: Vec<HashMap<String, String>> = members
        .iter()
        .map(|m| member_ref_to_parent_map(m))
        .collect();

    let mut atom_fields: Vec<Field> = Vec::new();
    let mut providing_members: HashMap<String, Vec<usize>> = HashMap::new();
    for pf in parent_schema {
        if pf.expression.is_some() || pf.is_list_link() {
            continue;
        }
        let providers: Vec<usize> = per_member_refs
            .iter()
            .enumerate()
            .filter(|(_, refs)| refs.values().any(|p| p == &pf.name))
            .map(|(i, _)| i)
            .collect();
        if providers.is_empty() {
            continue;
        }
        let entry = seg_constraints
            .get(&pf.name)
            .map(|fc| apply_constraints(pf, fc))
            .unwrap_or_else(|| pf.clone());
        atom_fields.push(entry);
        providing_members.insert(pf.name.clone(), providers);
    }
    (atom_fields, providing_members)
}

/// Effective output field list for a member. A lower-cover member reaching the executor is already
/// lowered into concrete case-members (constraint-bearing variants resolved in the planner), so
/// its `data` is the final column list.
fn member_effective_fields(m: &LowerCoverMember) -> Vec<Field> {
    m.dataset.data.clone()
}

/// Map a member's local field name → parent field name, for every field whose
/// `ref:` points at a column of the member's include parent (matched by
/// `member.reference`).
fn member_ref_to_parent_map(m: &LowerCoverMember) -> HashMap<String, String> {
    let prefix = format!("{}.", m.reference);
    let mut map = HashMap::new();
    for f in &m.dataset.data {
        if let Some(rs) = f.simple_ref()
            && let Some(parent_name) = rs.strip_prefix(prefix.as_str())
        {
            map.insert(f.name.clone(), parent_name.to_string());
        }
    }
    map
}

/// Generate the unified atom batch — `n_rows` rows, one column per shared
/// parent-ref entry. Column source priority per entry:
/// 1. Import taint: take from `opt_import_batch` if parent's field is tainted.
/// 2. Precomputed member: take from `computed[member.path]` when the first
///    providing member (in declaration order) is in `parent_computed` AND has no
///    cardinality (cardinality precomputed batches are at expanded shape).
/// 3. Fresh: generate via the atom-schema entry (which already has `seg_constraints`
///    applied).
///
/// When `members` provide no parent-ref columns, returns a zero-column batch with
/// `n_rows` rows (callers that index columns will simply produce empty selections).
fn generate_segment_atom_batch(
    parent_schema: &Schema,
    members: &[&LowerCoverMember],
    n_rows: usize,
    seg_constraints: &HashMap<String, FieldConstraints>,
    opt_import_batch: Option<&RecordBatch>,
    computed: &HashMap<PathBuf, RecordBatch>,
    parent_computed: &HashSet<PathBuf>,
) -> Result<RecordBatch> {
    let (atom_fields, providing_members) =
        build_segment_atom_schema(parent_schema, members, seg_constraints);

    if atom_fields.is_empty() {
        let opts = RecordBatchOptions::new().with_row_count(Some(n_rows));
        return Ok(RecordBatch::try_new_with_options(
            Arc::new(ArrowSchema::empty()),
            vec![],
            &opts,
        )?);
    }

    let arrow_fields: Vec<ArrowField> = atom_fields.iter().map(field_to_arrow).collect();
    let arrow_schema = Arc::new(ArrowSchema::new(arrow_fields));
    let mut cols: Vec<ArrayRef> = Vec::with_capacity(atom_fields.len());

    for atom_f in &atom_fields {
        let parent_f = parent_schema
            .iter()
            .find(|f| f.name == atom_f.name)
            .expect("atom field name is taken from parent_schema");

        // (1) Import taint
        if parent_f.imported_taint
            && let Some(ib) = opt_import_batch
            && let Ok(idx) = ib.schema().index_of(&atom_f.name)
        {
            cols.push(ib.column(idx).clone());
            continue;
        }

        // (2) Precomputed member (skip cardinality members — their batches are at
        //     expanded shape, not one-row-per-slot).
        let providers = providing_members
            .get(&atom_f.name)
            .expect("providing_members populated for every atom field");
        let mut taken: Option<ArrayRef> = None;
        for &mi in providers {
            let m = members[mi];
            if m.cardinality.is_some() || !parent_computed.contains(&m.path) {
                continue;
            }
            let Some(pre) = computed.get(&m.path) else {
                continue;
            };
            let prefix = format!("{}.", m.reference);
            let local_name = m.dataset.data.iter().find_map(|f| {
                f.simple_ref()
                    .and_then(|rs| rs.strip_prefix(prefix.as_str()))
                    .filter(|p| *p == atom_f.name.as_str())
                    .map(|_| f.name.as_str())
            });
            let Some(local_name) = local_name else {
                continue;
            };
            let Ok(col_idx) = pre.schema().index_of(local_name) else {
                continue;
            };
            taken = Some(pad_or_generate_tail(
                atom_f,
                pre.column(col_idx).clone(),
                n_rows,
            )?);
            break;
        }
        if let Some(col) = taken {
            cols.push(col);
            continue;
        }

        // (3) Fresh generate.
        cols.push(generate_column(atom_f, n_rows, &[])?);
    }

    Ok(RecordBatch::try_new(arrow_schema, cols)?)
}

/// Adjust a precomputed column to length `target_n`:
/// - Equal: return as-is.
/// - Longer: truncate via `take[0..target_n]`.
/// - Shorter: generate `target_n - len` fresh values from `field` and concatenate.
///
/// The short case matches the stochastic-rounding tolerance previously provided
/// by the LEFT JOIN in the pre-SEG-ATOM-1 parent assembly.
fn pad_or_generate_tail(field: &Field, base: ArrayRef, target_n: usize) -> Result<ArrayRef> {
    let base_n = base.len();
    if base_n == target_n {
        return Ok(base);
    }
    if base_n > target_n {
        let indices = UInt32Array::from_iter_values(0..target_n as u32);
        return Ok(take(base.as_ref(), &indices, None)?);
    }
    let tail_n = target_n - base_n;
    let tail = generate_column(field, tail_n, &[])?;
    Ok(concat(&[base.as_ref(), tail.as_ref()])?)
}

/// Build the parent batch for this segment from the unified atom batch.
///
/// For each active parent field (no expression, not a list-link): take the column
/// from `atom_batch` if it's a shared ref column; else from `opt_import_batch` if
/// tainted; else generate fresh under `seg_constraints[X]`.
fn project_parent_columns_from_atom(
    parent_schema: &Schema,
    atom_batch: &RecordBatch,
    n_rows: usize,
    seg_constraints: &HashMap<String, FieldConstraints>,
    opt_import_batch: Option<&RecordBatch>,
) -> Result<RecordBatch> {
    let active: Vec<&Field> = parent_schema
        .iter()
        .filter(|f| f.expression.is_none() && !f.is_list_link())
        .collect();
    let arrow_schema = Arc::new(ArrowSchema::new(
        active.iter().map(|f| field_to_arrow(f)).collect::<Vec<_>>(),
    ));
    let mut cols: Vec<ArrayRef> = Vec::with_capacity(active.len());
    for f in &active {
        let col = if let Ok(idx) = atom_batch.schema().index_of(&f.name) {
            atom_batch.column(idx).clone()
        } else if f.imported_taint
            && let Some(ib) = opt_import_batch
            && let Ok(idx) = ib.schema().index_of(&f.name)
        {
            ib.column(idx).clone()
        } else {
            let effective = seg_constraints
                .get(&f.name)
                .map(|fc| apply_constraints(f, fc));
            generate_column(effective.as_ref().unwrap_or(f), n_rows, &[])?
        };
        cols.push(col);
    }
    Ok(RecordBatch::try_new(arrow_schema, cols)?)
}

/// Build this member's output batch by composing two sources:
/// (a) ref columns projected from the unified atom batch, looked up by the parent
///     column name each ref points at, and
/// (b) non-ref columns generated via `generate_member_nonref_fields` (preserving
///     VAR-2 sub-distribution).
///
/// **Precomputed without cardinality**: use the precomputed batch directly (it was
/// already emitted by the prior plan step); the post-loop in
/// `execute_lower_cover_group_core` skips re-emission for `parent_computed` paths.
///
/// **Cardinality**: each slot expands to `m_n = sample_count(card).max(1)` rows.
/// Ref columns are replicated via Arrow `take` over per-row indices; non-ref
/// columns are freshly generated per slot via the variant-aware path and
/// concatenated. `_slot_idx = slot_offset + i` is prepended.
fn project_member_columns(
    atom_batch: &RecordBatch,
    m: &LowerCoverMember,
    slot_offset: usize,
    n_rows: usize,
    precomputed: Option<&RecordBatch>,
    acc: &mut SegmentBatchAccumulator,
) -> Result<()> {
    // (a) Precomputed, no cardinality: reuse the prior step's batch verbatim.
    if m.cardinality.is_none()
        && let Some(pre) = precomputed
    {
        acc.push_member_batch(m.path.clone(), pre.clone());
        return Ok(());
    }

    let prefix = format!("{}.", m.reference);

    if let Some(card) = &m.cardinality {
        // Sample m_n per slot; build per-row slot_idx and the source-row indices
        // used to replicate atom rows.
        let mut row_indices: Vec<u32> = Vec::new();
        let mut slot_tags: Vec<u32> = Vec::new();
        let mut nonref_slot_batches: Vec<RecordBatch> = Vec::new();
        let mut emitted_any_nonref = false;
        for i in 0..n_rows {
            let m_n = sample_count(card).max(1);
            row_indices.extend(std::iter::repeat_n(i as u32, m_n));
            slot_tags.extend(std::iter::repeat_n((slot_offset + i) as u32, m_n));
            let nonref = generate_member_nonref_fields(m, m_n)?;
            if nonref.num_columns() > 0 {
                emitted_any_nonref = true;
            }
            nonref_slot_batches.push(nonref);
        }
        let nonref_combined = if emitted_any_nonref {
            let s = nonref_slot_batches
                .iter()
                .find(|b| b.num_columns() > 0)
                .map(|b| b.schema())
                .expect("emitted_any_nonref implies at least one batch with columns");
            Some(concat_batches(&s, &nonref_slot_batches)?)
        } else {
            None
        };

        let take_indices = UInt32Array::from(row_indices);
        let mut out_fields: Vec<ArrowField> =
            vec![ArrowField::new("_slot_idx", DataType::UInt32, false)];
        let mut out_cols: Vec<ArrayRef> = vec![Arc::new(UInt32Array::from(slot_tags))];
        for f in &member_effective_fields(m) {
            if f.expression.is_some() || f.is_list_link() {
                continue;
            }
            let col = if let Some(parent_name) = f
                .simple_ref()
                .and_then(|rs| rs.strip_prefix(prefix.as_str()))
            {
                let atom_idx = atom_batch.schema().index_of(parent_name).map_err(|_| {
                    anyhow!(
                        "atom batch missing shared ref column '{parent_name}' for member '{}'",
                        m.dataset.name
                    )
                })?;
                take(atom_batch.column(atom_idx), &take_indices, None)?
            } else if let Some(comb) = &nonref_combined {
                let idx = comb.schema().index_of(&f.name).map_err(|_| {
                    anyhow!(
                        "nonref batch missing '{}' for member '{}'",
                        f.name,
                        m.dataset.name
                    )
                })?;
                comb.column(idx).clone()
            } else {
                generate_column(f, take_indices.len(), &[])?
            };
            out_cols.push(col);
            out_fields.push(field_to_arrow(f));
        }
        let batch = RecordBatch::try_new(Arc::new(ArrowSchema::new(out_fields)), out_cols)?;
        acc.push_member_batch(m.path.clone(), batch);
        return Ok(());
    }

    // No cardinality, not precomputed: one row per slot.
    let nonref_batch = {
        let nonref = generate_member_nonref_fields(m, n_rows)?;
        (nonref.num_columns() > 0).then_some(nonref)
    };
    let mut out_fields: Vec<ArrowField> = Vec::new();
    let mut out_cols: Vec<ArrayRef> = Vec::new();
    for f in &member_effective_fields(m) {
        if f.expression.is_some() || f.is_list_link() {
            continue;
        }
        let col = if let Some(parent_name) = f
            .simple_ref()
            .and_then(|rs| rs.strip_prefix(prefix.as_str()))
        {
            let atom_idx = atom_batch.schema().index_of(parent_name).map_err(|_| {
                anyhow!(
                    "atom batch missing shared ref column '{parent_name}' for member '{}'",
                    m.dataset.name
                )
            })?;
            atom_batch.column(atom_idx).clone()
        } else if let Some(b) = &nonref_batch {
            let idx = b.schema().index_of(&f.name).map_err(|_| {
                anyhow!(
                    "nonref batch missing '{}' for member '{}'",
                    f.name,
                    m.dataset.name
                )
            })?;
            b.column(idx).clone()
        } else {
            generate_column(f, n_rows, &[])?
        };
        out_cols.push(col);
        out_fields.push(field_to_arrow(f));
    }
    let batch = RecordBatch::try_new(Arc::new(ArrowSchema::new(out_fields)), out_cols)?;
    acc.push_member_batch(m.path.clone(), batch);
    Ok(())
}

/// Get a cached `ImportIndex` or load it from disk if this is the first access.
fn get_or_load_import(
    dataset_path: &Path,
    spec: &crate::models::ImportSpec,
    ring_seed: u64,
    cache: &mut HashMap<PathBuf, Arc<ImportIndex>>,
) -> Result<Arc<ImportIndex>> {
    let import_path = resolve_import_path(dataset_path, &spec.file)?;
    if let Some(idx) = cache.get(&import_path) {
        return Ok(Arc::clone(idx));
    }
    let idx = load_import_index(spec, dataset_path, ring_seed)?;
    cache.insert(import_path, Arc::clone(&idx));
    Ok(idx)
}

/// Generate a member's **non-ref** field columns only, applying Level 2 variant
/// sub-distribution when the member has `variants:`. Ref fields (whose values flow
/// from the segment atom batch) are excluded.
///
/// Without variants: delegates to `generate_fresh_batch` over the non-ref subset.
/// With variants: distributes `rows` across compatible variant schemas
/// proportionally, generates one sub-batch per surviving variant (restricted to its
/// non-ref subset), and concatenates them.
///
/// Variant compatibility against `seg_constraints` is checked using the **full**
/// variant schema (including its ref fields) so that variants whose ref-bound
/// values conflict with the segment are correctly pruned.
fn generate_member_nonref_fields(m: &LowerCoverMember, rows: usize) -> Result<RecordBatch> {
    // After VAR-EXPAND lowering, a member carries a concrete schema with no `variants:`
    // (tagged unions are lowered into separate case-members in the planner), so this
    // generates exactly the member's non-ref fields. Ref columns come from the shared
    // segment-atom batch; expression and list-link fields are handled elsewhere.
    //
    // Segment field constraints are deliberately NOT applied here: they are keyed by *parent*
    // field name and describe the shared (ref'd) parent columns, which are materialised in the
    // segment-atom batch and projected in. A member's *own* non-ref field is independent — even
    // if it happens to share a name with a constrained parent field (e.g. both a parent and a
    // child have a `status` field), the parent's restriction must not bleed onto it.
    let prefix = format!("{}.", m.reference);
    let is_nonref = |f: &Field| {
        f.simple_ref()
            .and_then(|rs| rs.strip_prefix(prefix.as_str()))
            .is_none()
    };
    let nonref_base: Vec<Field> = m
        .dataset
        .data
        .iter()
        .filter(|f| is_nonref(f) && f.expression.is_none() && !f.is_list_link())
        .cloned()
        .collect();

    if nonref_base.is_empty() {
        let opts = RecordBatchOptions::new().with_row_count(Some(rows));
        return Ok(RecordBatch::try_new_with_options(
            Arc::new(ArrowSchema::empty()),
            vec![],
            &opts,
        )?);
    }

    generate_fresh_batch(&nonref_base, rows, &HashMap::new())
}

// ---------------------------------------------------------------------------
// Linked-dataset sampling helpers
// ---------------------------------------------------------------------------

/// Sample `count` indices from `[0, linked_n)` without replacement (Fisher-Yates).
///
/// Panics if `count > linked_n` — callers must enforce the planning-time check.
fn sample_linked_without_replacement(linked_n: usize, count: usize) -> Vec<u32> {
    assert!(
        count <= linked_n,
        "sample_linked_without_replacement: count {count} > linked_n {linked_n}"
    );
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
    sample_with_polya(vec![1.0; linked_n], count, reinforcement)
}

/// Weighted random sampling with Pólya-urn updates.
///
/// `initial_weights` seeds the probability distribution; after each draw the chosen
/// index's weight is multiplied by `reinforcement`. `reinforcement = 1.0` gives static
/// categorical sampling with no urn dynamics.
fn sample_with_polya(mut weights: Vec<f64>, count: usize, reinforcement: f64) -> Vec<u32> {
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        let total: f64 = weights.iter().sum();
        let mut target = (0.0f64..total).fake::<f64>();
        let mut chosen = 0usize;
        for (i, &w) in weights.iter().enumerate() {
            if target < w {
                chosen = i;
                break;
            }
            target -= w;
        }
        result.push(chosen as u32);
        weights[chosen] *= reinforcement;
    }
    result
}

fn strip_sentinel(batch: RecordBatch, sentinel: &str) -> RecordBatch {
    let Ok(idx) = batch.schema().index_of(sentinel) else {
        return batch;
    };
    let (fields, cols): (Vec<_>, Vec<_>) = batch
        .schema()
        .fields()
        .iter()
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
    let list_link_refs: HashSet<&str> = dataset
        .data
        .iter()
        .filter_map(|f| f.content.as_ref()?.from.as_deref())
        .collect();
    for link in &dataset.links {
        if list_link_refs.contains(link.reference.as_str()) {
            continue;
        }
        let Some(linked_path) = resolve_include(path, &link.file) else {
            continue;
        };
        let Some(linked_batch) = computed.get(&linked_path) else {
            continue;
        };
        let n_linked = linked_batch.num_rows();
        let n_eligible = eligible_linked_rows(n_linked, link.ratio);
        let n_rows = batch.num_rows();
        let r = link.reinforcement;
        let ov = link.overlap;
        let assignments: Vec<u32> = if ov == Some(0.0) {
            // overlap:0 for junction: one exclusive linked row per junction row (partition of size 1).
            // This degenerates to without-replacement across the eligible linked rows.
            sample_linked_without_replacement(n_eligible, n_rows)
        } else if r == Some(0.0) {
            sample_linked_without_replacement(n_eligible, n_rows)
        } else if let Some(ov_val) = ov.filter(|&v| v > 1.0) {
            // Power-law single draw per junction row.
            let reinf = r.unwrap_or(1.0);
            let initial_weights: Vec<f64> = (0..n_eligible)
                .map(|j| ((n_eligible - j) as f64).powf(ov_val - 1.0))
                .collect();
            (0..n_rows)
                .map(|_| sample_with_polya(initial_weights.clone(), 1, reinf)[0])
                .collect()
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
            let prefix = inherited
                .get(&f.name)
                .map_or(&[] as &[ArrayRef], |v| v.as_slice());
            let effective = overrides.get(&f.name).map(|fc| apply_constraints(f, fc));
            generate_column(effective.as_ref().unwrap_or(f), rows, prefix)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RecordBatch::try_new(arrow_schema, columns)?)
}

/// Return a copy of `field` with any non-None constraint from `fc` applied.
fn apply_constraints(field: &Field, fc: &FieldConstraints) -> Field {
    let mut f = field.clone();
    if fc.value.is_some() {
        f.value = fc.value.clone();
    }
    if fc.one_of.is_some() {
        f.one_of = fc.one_of.clone();
    }
    if fc.generator.is_some() {
        f.generator = fc.generator.clone();
    }
    if fc.min.is_some() || fc.max.is_some() {
        let r = f.range.get_or_insert(Range::default());
        if fc.min.is_some() {
            r.min = fc.min;
        }
        if fc.max.is_some() {
            r.max = fc.max;
        }
    }
    // VAR-SPECIALIZE S5: per-case carrier specialisations — narrow the matching variant case's
    // value-source in place (non-restrictive; unnamed cases untouched).
    for delta in &fc.case_overrides {
        if let Some(case) = f
            .variants
            .iter_mut()
            .find(|c| c.name.as_deref() == Some(&delta.name))
        {
            if delta.value.is_some() {
                case.value = delta.value.clone();
            }
            if delta.generator.is_some() {
                case.generator = delta.generator.clone();
            }
            if let Some(dr) = &delta.range {
                let r = case.range.get_or_insert(Range::default());
                if let Some(mn) = dr.min {
                    r.min = Some(r.min.map_or(mn, |e| e.max(mn)));
                }
                if let Some(mx) = dr.max {
                    r.max = Some(r.max.map_or(mx, |e| e.min(mx)));
                }
            }
        }
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
) -> Result<RecordBatch> {
    if batches.is_empty() {
        return generate_batch(schema, 0, &HashMap::new(), &HashMap::new());
    }
    union_and_shuffle(batches, name).await
}

/// Concatenate all batches and shuffle via DataFusion `ORDER BY random()`.
/// Zero-row inputs are returned immediately to avoid DataFusion empty-result issues.
async fn union_and_shuffle(batches: Vec<RecordBatch>, name: &str) -> Result<RecordBatch> {
    let arrow_schema = batches
        .first()
        .ok_or_else(|| anyhow!("union_and_shuffle: no batches for '{name}'"))?
        .schema();
    let combined = concat_batches(&arrow_schema, &batches)?;
    if combined.num_rows() == 0 {
        return Ok(combined);
    }
    let ctx = SessionContext::new();
    let df = ctx.read_batch(combined)?;
    let shuffled = df
        .sort(vec![
            datafusion::functions::expr_fn::random().sort(true, true),
        ])?
        .collect()
        .await?;
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
    let shuffled_non_witness =
        combine_and_shuffle(non_witness_source_batches, schema, name).await?;
    let arrow_schema = Arc::new(schema_to_arrow(schema));
    let ws_combined = concat_batches(&arrow_schema, &witness_source_batches)?;
    Ok(concat_batches(
        &ws_combined.schema(),
        &[ws_combined, shuffled_non_witness],
    )?)
}

fn emit_batch(
    batch: RecordBatch,
    dataset: &SyntheticDataset,
    shared: &mut HashMap<String, (Format, Vec<RecordBatch>)>,
) -> Result<()> {
    for output in dataset.resolved_outputs() {
        shared
            .entry(output.file.clone())
            .or_insert_with(|| (dataset.format.clone(), Vec::new()))
            .1
            .push(batch.clone());
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
    Ok(RecordBatch::try_new(
        Arc::new(ArrowSchema::new(fields)),
        columns,
    )?)
}

/// Evaluate all expression fields against the batch, building a CTE chain in
/// YAML order so each step can reference expression columns defined above it.
/// Returns the original batch augmented with new expression columns appended.
async fn evaluate_expressions(
    batch: RecordBatch,
    dataset: &SyntheticDataset,
) -> Result<RecordBatch> {
    let expr_fields: Vec<_> = dataset
        .data
        .iter()
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

    let schema = batches
        .first()
        .map(|b| b.schema())
        .ok_or_else(|| anyhow!("expression evaluation returned no rows"))?;
    Ok(concat_batches(&schema, &batches)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FieldType;
    use arrow::array::StringArray;
    use serde_yaml::Value as YamlValue;

    /// Build a string Field with a constant `value:` — `generate_column` then emits the
    /// constant for every requested row, making the tail-generation deterministic.
    fn constant_field(name: &str, value: &str) -> Field {
        Field {
            name: name.to_string(),
            field_type: Some(FieldType::String),
            value: Some(YamlValue::String(value.to_string())),
            ..Default::default()
        }
    }

    fn string_col(values: &[&str]) -> ArrayRef {
        Arc::new(StringArray::from(
            values.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        ))
    }

    fn read_strings(arr: &ArrayRef) -> Vec<String> {
        let sa = arr.as_any().downcast_ref::<StringArray>().unwrap();
        (0..sa.len()).map(|i| sa.value(i).to_string()).collect()
    }

    #[test]
    fn pad_or_generate_tail_returns_base_when_lengths_match() {
        let field = constant_field("c", "pad");
        let base = string_col(&["a", "b", "c"]);
        let out = pad_or_generate_tail(&field, base, 3).unwrap();
        assert_eq!(read_strings(&out), vec!["a", "b", "c"]);
    }

    #[test]
    fn pad_or_generate_tail_truncates_when_base_is_longer() {
        let field = constant_field("c", "pad");
        let base = string_col(&["a", "b", "c", "d", "e"]);
        let out = pad_or_generate_tail(&field, base, 3).unwrap();
        // Truncated via take to exactly the first `target_n` rows.
        assert_eq!(read_strings(&out), vec!["a", "b", "c"]);
    }

    #[test]
    fn pad_or_generate_tail_pads_when_base_is_shorter() {
        // Mirrors the stochastic-rounding tolerance for precomputed members in
        // `generate_segment_atom_batch`: the precomputed batch may be a row or two
        // short of `seg.rows`, so the tail is freshly generated and concatenated.
        let field = constant_field("c", "pad");
        let base = string_col(&["a", "b"]);
        let out = pad_or_generate_tail(&field, base, 5).unwrap();
        // First two rows from the precomputed batch; the remaining three are the
        // freshly generated constant from the field definition.
        assert_eq!(read_strings(&out), vec!["a", "b", "pad", "pad", "pad"]);
    }
}

/// VAR-1 PR 1 — DataFusion + writer spike (the decision gate; see `specs/VAR-1-impl.md`).
///
/// Proves that an Arrow `DenseUnion` column — the proposed internal representation for a
/// heterogeneous tagged union — survives the three DataFusion operations the executor
/// relies on (`union_and_shuffle`, `evaluate_expressions`, `filter_hidden_columns`), and
/// records which output writers accept it. A green run here means the internal-rep
/// decision stands (DenseUnion); a failure routes us to the documented fallback
/// (internal nullable-superset struct). These tests are kept as the regression guard.
#[cfg(test)]
mod denseunion_spike {
    use super::*;
    use crate::models::{Field, Format, SyntheticDataset};
    use crate::output::unionize_for_output;
    use arrow::array::{Float64Array, Int32Array, StringArray, StructArray, UnionArray};
    use arrow::buffer::ScalarBuffer;
    use arrow::datatypes::{UnionFields, UnionMode};
    use std::collections::BTreeMap;

    /// A 6-row batch: an `id` Int32 column + a dense union `u` with three case types
    /// (Utf8, Float64, Struct{a:Int32}) — two rows each, covering the scalar-mixed and
    /// object-schema cases at once.
    fn union_batch() -> RecordBatch {
        let union_fields: UnionFields = [
            (0_i8, Arc::new(ArrowField::new("s", DataType::Utf8, false))),
            (
                1_i8,
                Arc::new(ArrowField::new("n", DataType::Float64, false)),
            ),
            (
                2_i8,
                Arc::new(ArrowField::new(
                    "o",
                    DataType::Struct(
                        vec![Arc::new(ArrowField::new("a", DataType::Int32, false))].into(),
                    ),
                    false,
                )),
            ),
        ]
        .into_iter()
        .collect();

        let strings: ArrayRef = Arc::new(StringArray::from(vec!["a", "b"]));
        let numbers: ArrayRef = Arc::new(Float64Array::from(vec![1.0, 2.0]));
        let structs: ArrayRef = Arc::new(StructArray::from(vec![(
            Arc::new(ArrowField::new("a", DataType::Int32, false)),
            Arc::new(Int32Array::from(vec![10, 20])) as ArrayRef,
        )]));

        // Dense union: type_ids pick the case per slot; offsets index into that child.
        let type_ids = ScalarBuffer::<i8>::from(vec![0, 1, 2, 0, 1, 2]);
        let offsets = ScalarBuffer::<i32>::from(vec![0, 0, 0, 1, 1, 1]);
        let union = UnionArray::try_new(
            union_fields.clone(),
            type_ids,
            Some(offsets),
            vec![strings, numbers, structs],
        )
        .expect("build dense union");

        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("id", DataType::Int32, false),
            ArrowField::new("u", DataType::Union(union_fields, UnionMode::Dense), false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![0, 1, 2, 3, 4, 5])),
                Arc::new(union),
            ],
        )
        .expect("build batch")
    }

    fn union_col(batch: &RecordBatch) -> UnionArray {
        batch
            .column_by_name("u")
            .expect("u column present")
            .as_any()
            .downcast_ref::<UnionArray>()
            .expect("u is a UnionArray")
            .clone()
    }

    /// Per-case (type_id) row-count histogram — the integrity invariant a shuffle must preserve.
    fn type_id_histogram(u: &UnionArray) -> BTreeMap<i8, usize> {
        let mut h = BTreeMap::new();
        for &t in u.type_ids().iter() {
            *h.entry(t).or_insert(0) += 1;
        }
        h
    }

    #[tokio::test]
    async fn denseunion_survives_union_and_shuffle() {
        let batch = union_batch();
        let before = type_id_histogram(&union_col(&batch));
        let out = union_and_shuffle(vec![batch], "spike")
            .await
            .expect("DataFusion sort must carry a DenseUnion column");
        assert_eq!(out.num_rows(), 6, "shuffle must preserve row count");
        let after = type_id_histogram(&union_col(&out));
        assert_eq!(before, after, "shuffle must preserve per-case row counts");
    }

    #[tokio::test]
    async fn denseunion_survives_evaluate_expressions() {
        let dataset = SyntheticDataset {
            name: "spike".into(),
            format: Format::Json,
            rows: None,
            output: None,
            outputs: vec![],
            locale: None,
            include: None,
            import: None,
            links: vec![],
            data: vec![Field {
                name: "id_plus".into(),
                expression: Some("id + 1".into()),
                ..Default::default()
            }],
        };
        let out = evaluate_expressions(union_batch(), &dataset)
            .await
            .expect("DataFusion SELECT * must carry a DenseUnion column through a CTE");
        assert_eq!(out.num_rows(), 6);
        assert!(
            out.column_by_name("u").is_some(),
            "union column survives the CTE"
        );
        assert!(
            out.column_by_name("id_plus").is_some(),
            "expression column added"
        );
    }

    #[test]
    fn denseunion_survives_filter_hidden_columns() {
        let fields = vec![
            Field {
                name: "id".into(),
                hidden: true,
                ..Default::default()
            },
            Field {
                name: "u".into(),
                hidden: false,
                ..Default::default()
            },
        ];
        let out = filter_hidden_columns(union_batch(), &fields).expect("project a union column");
        assert!(out.column_by_name("id").is_none(), "hidden column dropped");
        assert!(out.column_by_name("u").is_some(), "union column kept");
        assert_eq!(out.num_rows(), 6);
    }

    /// PR 4: the union → nullable-superset struct conversion has the right shape —
    /// one sub-field per case, and **exactly one** sub-field non-null per row (so the
    /// populated sub-field is an unambiguous case tag).
    #[test]
    fn unionize_for_output_is_nullable_superset() {
        let portable = unionize_for_output(&union_batch()).unwrap();
        let s = portable
            .column_by_name("u")
            .unwrap()
            .as_any()
            .downcast_ref::<StructArray>()
            .expect("union column lowered to a struct");
        assert_eq!(s.num_columns(), 3, "one sub-field per case");
        assert_eq!(s.len(), 6);
        for r in 0..s.len() {
            let non_null = (0..s.num_columns())
                .filter(|&c| s.column(c).is_valid(r))
                .count();
            assert_eq!(non_null, 1, "row {r} must populate exactly one case");
        }
    }

    /// PR 4 guard: `write_output` now lowers a union to a portable nullable-superset
    /// struct before writing, so the struct-capable writers (parquet/json/jsonl) succeed.
    /// CSV can't represent nested types (a pre-existing limitation that also applies to
    /// object fields), so it remains unsupported — asserted here so the boundary is
    /// explicit. (Raw arrow writers still reject a union directly — ARROW-8817 — which is
    /// exactly why `unionize_for_output` exists.)
    #[test]
    fn write_output_handles_union_via_conversion() {
        let batch = union_batch();
        let dir = std::env::temp_dir().join(format!("var1_pr4_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut results: Vec<(Format, bool)> = Vec::new();
        for format in [Format::Parquet, Format::Json, Format::Jsonl, Format::Csv] {
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                write_output(&batch, "u", &format, &dir, &[]).is_ok()
            }))
            .unwrap_or(false);
            results.push((format, ok));
        }
        std::panic::set_hook(prev_hook);
        let _ = std::fs::remove_dir_all(&dir);

        for (format, ok) in &results {
            println!(
                "write_output {format:?}: {}",
                if *ok { "ok" } else { "unsupported" }
            );
        }
        for (format, ok) in results {
            // Struct-capable formats succeed via conversion; CSV (flat) does not.
            let expected = !matches!(format, Format::Csv);
            assert_eq!(
                ok, expected,
                "write_output {format:?}: support changed — revisit the conversion / CSV note"
            );
        }
    }
}
