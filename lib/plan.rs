use anyhow::Result;
use petgraph::visit::Topo;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use serde_yaml::Value as YamlValue;

use crate::segment::{plan_segments, LowerCoverMember, Segment};
use crate::graph::DatasetGraph;
use crate::models::{expected_cardinality, for_each_list_link, resolve_distributions, resolve_include, split_ref, CountSpec, Field, Format, Include, Locale, Reducer, RefBinding, Schema, SyntheticDataset, VariantSchema};
use crate::rewrite::apply_locale_to_schema;

const DEFAULT_ROWS: usize = 100;

/// Wires a pre-generated column from an already-computed batch into a field of
/// the dataset being generated. Produced from `ref_field` strings at plan time;
/// consumed by the executor so ref columns are never re-generated (inherited field).
#[derive(Debug, Clone)]
pub struct InheritedField {
    /// Canonical path of the already-generated dataset.
    pub from_path: PathBuf,
    /// Column name in that dataset's batch.
    pub from_column: String,
    /// Column name in the target dataset to fill.
    pub into_column: String,
}

/// A single unit of execution work.
#[derive(Debug)]
pub enum ExecutionStep {
    /// Generate one dataset, optionally pre-filling ref columns from already-computed batches.
    /// Evaluates expressions and emits immediately, unless `defer_emit` is true (collect-target
    /// deferral: expressions are evaluated but the file write is deferred to `EmitDataset`).
    GenerateDataset {
        path: PathBuf,
        dataset: Arc<SyntheticDataset>,
        rows: usize,
        prefills: Vec<InheritedField>,
        defer_emit: bool,
    },
    /// Staging node: generates scalar (non-list) fields only. No expression evaluation, no emit.
    /// `AssembleFromWitness` adds list columns, evaluates expressions, and emits.
    GenerateStagingNode {
        path: PathBuf,
        dataset: Arc<SyntheticDataset>,
        rows: usize,
        prefills: Vec<InheritedField>,
    },
    /// Generate a segmented parent and fan row segments out to lower cover members.
    /// Evaluates parent expressions and emits immediately, unless `defer_emit` is true
    /// (collect-target deferral: file write deferred to `EmitDataset`).
    GenerateLowerCoverGroup {
        parent_path: PathBuf,
        parent: Arc<SyntheticDataset>,
        segments: Vec<Segment>,
        members: Vec<LowerCoverMember>,
        defer_emit: bool,
    },
    /// Staging counterpart of `GenerateLowerCoverGroup`.
    /// Parent has list-link fields; expressions and emit are deferred to `AssembleFromWitness`.
    GenerateStagingLowerCoverGroup {
        parent_path: PathBuf,
        parent: Arc<SyntheticDataset>,
        segments: Vec<Segment>,
        members: Vec<LowerCoverMember>,
    },
    /// Generate the witness batch for one list-link field.
    ///
    /// Each row of the resulting batch is one **atom**: a single (staging-slot, linked-row) pair.
    /// The batch is stored in `computed[witness_key]` and contains:
    ///   - `_slot_idx: UInt32` — which staging slot this atom belongs to
    ///   - `_linked_idx: UInt32` — which linked-dataset row this atom was assigned to
    ///   - one column per `inner_fields` field, resolved as follows:
    ///       - linked-dataset refs: the pushed-down linked-row solution for `_linked_idx`
    ///       - outer-scoped refs: the staging row value for `_slot_idx`
    ///       - plain fields: generated fresh per atom
    ///
    /// `linked_path` is the path of the pre-solved linked-dataset batch (one row per eligible
    /// linked row). Atoms sharing the same `_linked_idx` carry identical linked-dataset values.
    GenerateWitness {
        witness_key: PathBuf,
        staging_path: PathBuf,
        list_field_name: String,
        inner_fields: Vec<Field>,
        include: Include,
        cardinality: CountSpec,
        linked_path: PathBuf,
    },
    /// Assemble list-link columns into the staging batch and emit.
    ///
    /// Reads the scalar staging batch and each witness batch from `computed`, builds one
    /// `ListArray` per spec, appends them to the staging batch, evaluates expressions,
    /// filters hidden columns, and writes output.
    AssembleFromWitness {
        staging_path: PathBuf,
        dataset: Arc<SyntheticDataset>,
        /// `(list_field_name, witness_key, project_col)` — `project_col` is `Some(col_name)`
        /// when `content.project` is set, causing scalar-list assembly for that field.
        witness_specs: Vec<(String, PathBuf, Option<String>)>,
    },
    /// Accumulate values from a source batch into a linked dataset's field in `computed`.
    ///
    /// Groups source rows by `group_by` (always `"_linked_idx"`), aggregates the
    /// `source_field` column using `reducer`, and writes the result into the linked batch's
    /// `linked_field` column. Linked rows with no matching source rows receive the linked
    /// field's `default` value.
    AccumulateToLinked {
        source_path:   PathBuf,
        source_field:  String,
        linked_path:   PathBuf,
        linked_field:  String,
        group_by:      String,
        reducer:       Reducer,
        /// Declared `default:` from the linked field YAML, used as the fallback value for
        /// linked rows that have no matching source rows (scalar reducers only).
        /// For `Collect` the empty-list is built explicitly; this field is ignored.
        default_val:   serde_yaml::Value,
    },
    /// Emit the batch at `path` from `computed` to an output file.
    ///
    /// Used after `AccumulateToLinked` to write the now-updated linked batch. Applies
    /// `filter_hidden_columns` and calls the normal emit path.
    EmitDataset {
        path:    PathBuf,
        dataset: Arc<SyntheticDataset>,
    },
    /// Flush a shared output file: union + shuffle all accumulated batches, write once.
    WriteSharedOutput {
        output_file: String,
        format: Format,
    },
}

/// A fully-resolved, ordered list of steps for the executor to interpret linearly.
pub struct ExecutionPlan {
    pub steps: Vec<ExecutionStep>,
}

// ---------------------------------------------------------------------------
// Variant helpers
// ---------------------------------------------------------------------------

fn internal_path(base: &Path, label: &str) -> PathBuf {
    let stem = base.file_stem().unwrap_or_default().to_string_lossy();
    base.with_file_name(format!("{stem}___{label}.internal"))
}

fn variant_key(path: &Path, i: usize) -> PathBuf {
    internal_path(path, &format!("variant_{i}"))
}


/// Split `parent_rows` into `dists.len()` integer counts that sum exactly to `parent_rows`.
/// Uses largest-remainder (Hamilton) rounding.
fn distribute_rows(parent_rows: usize, dists: &[f64]) -> Vec<usize> {
    let raw: Vec<f64> = dists.iter().map(|d| d * parent_rows as f64).collect();
    let mut counts: Vec<usize> = raw.iter().map(|r| r.floor() as usize).collect();
    let remainder = parent_rows - counts.iter().sum::<usize>();
    let mut fracs: Vec<(usize, f64)> = raw.iter().enumerate()
        .map(|(i, r)| (i, r - r.floor()))
        .collect();
    fracs.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for k in 0..remainder { counts[fracs[k].0] += 1; }
    counts
}

/// Merge base schema with variant schema: variant fields override same-named base fields.
/// Object fields are deep-merged (sub-fields are individually overridden rather than
/// replacing the entire object), matching the semantics of boolean factoring.
fn merge_variant_fields(base: &Schema, variant_data: &Schema) -> Schema {
    use crate::expand_variants::merge_delta_into;
    let mut result = base.clone();
    for vfield in variant_data {
        merge_delta_into(&mut result, vfield.clone());
    }
    result
}

/// Produce the concrete `SyntheticDataset` for one variant.
///
/// Base fields are merged with the variant's own data (variant wins on name collisions).
/// The effective locale (variant > dataset) is stamped onto any unstamped variant fields.
/// `output_key` is used as `output_file` so all variants accumulate into the same shared
/// output and are shuffled together by `WriteSharedOutput`.
fn expand_variant_dataset(
    base: &SyntheticDataset,
    variant: &VariantSchema,
    variant_index: usize,
    rows: usize,
    output_key: &str,
) -> SyntheticDataset {
    let effective_locale: Option<Locale> = variant.locale.clone().or_else(|| base.locale.clone());

    // Stamp locale onto variant-specific fields (base fields were already stamped by
    // apply_global_locales; variant fields are new and need the same treatment).
    let mut variant_fields = variant.data.clone();
    if let Some(ref loc) = effective_locale {
        apply_locale_to_schema(&mut variant_fields, loc);
    }

    SyntheticDataset {
        name: format!("{}__v{}", base.name, variant_index),
        format: base.format.clone(),
        locale: effective_locale,
        rows: Some(rows),
        output_file: Some(output_key.to_string()),
        include: base.include.clone(),
        links: base.links.clone(),
        data: merge_variant_fields(&base.data, &variant_fields),
        variants: vec![],
    }
}

/// Validate Case 2 v1 restriction: a pool dataset targeted by a nested-include collect binding
/// must not be jointly segmented with another (non-witness-source) lower cover member. When it is, the correct
/// approach is a top-level junction dataset (Case 1).
fn check_collect_segmentation_restrictions(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    lower_cover_groups: &HashMap<PathBuf, Vec<LowerCoverMember>>,
) -> Result<()> {
    for (path, dataset) in datasets {
        for field in &dataset.data {
            let Some(content) = &field.content else { continue };
            let Some(from_ref) = &content.from else { continue };
            let has_collect = content.item.fields.iter().any(|cf| !cf.collect_bindings().is_empty());
            if !has_collect { continue; }
            let Some(link) = dataset.links.iter().find(|l| l.reference == *from_ref) else { continue };
            let Some(linked_path) = resolve_include(path, &link.file) else { continue };
            if let Some(members) = lower_cover_groups.get(&linked_path) {
                if members.iter().any(|m| !m.is_witness_source) {
                    anyhow::bail!(
                        "dataset '{}': list-link collect on field '{}' is not supported \
                         when the linked dataset is jointly segmented with another lower cover member; \
                         use a top-level junction dataset instead",
                        dataset.name, field.name
                    );
                }
            }
        }
    }
    Ok(())
}

/// Return the maximum number of items a `CountSpec` can produce, or `None` if unbounded.
fn max_cardinality_bound(spec: &CountSpec) -> Option<usize> {
    match spec {
        CountSpec::Fixed(n)            => Some(*n),
        CountSpec::Uniform { max, .. } => Some(*max),
        CountSpec::Normal  { .. }      => None,
    }
}

/// Planning-time feasibility check for `reinforcement: 0` (without-replacement sampling).
///
/// Without-replacement requires that the number of items drawn per outer row (`M_n`) never
/// exceeds the number of eligible pool slots (`n_eligible`). Because counts are stochastic,
/// we check the maximum possible `M_n`:
///
/// - For nested-include fields: `max_cardinality_bound(cardinality) ≤ n_eligible_slots`.
/// - For junction links: `junction_rows ≤ n_eligible_pool_rows`.
///
/// `Normal` cardinality has no finite upper bound, so without-replacement is disallowed for
/// it unless the pool is unbounded (impossible in practice). We reject it as a planning error.
fn check_reinforcement_zero_feasibility(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    row_counts: &HashMap<PathBuf, usize>,
) -> Result<()> {
    for (path, dataset) in datasets {
        // Nested-include fields with reinforcement: 0.
        for field in &dataset.data {
            let Some(content) = &field.content else { continue };
            let Some(from_ref) = &content.from else { continue };
            let Some(link) = dataset.links.iter().find(|l| l.reference == *from_ref) else { continue };
            if link.reinforcement != Some(0.0) { continue; }
            let Some(linked_path) = resolve_include(path, &link.file) else { continue };
            let n_eligible = {
                let linked_rows = *row_counts.get(&linked_path).unwrap_or(&0);
                match link.ratio {
                    Some(r) => ((r * linked_rows as f64).round() as usize).max(1).min(linked_rows),
                    None    => linked_rows,
                }
            };
            let cardinality = link.cardinality.clone().unwrap_or(CountSpec::Fixed(1));
            match max_cardinality_bound(&cardinality) {
                None => anyhow::bail!(
                    "dataset '{}' field '{}': `reinforcement: 0` (without-replacement) \
                     is not compatible with `Normal` cardinality — the count is unbounded",
                    dataset.name, field.name
                ),
                Some(max_m) if max_m > n_eligible => anyhow::bail!(
                    "dataset '{}' field '{}': `reinforcement: 0` requires cardinality ≤ \
                     eligible pool size ({n_eligible}), but max cardinality is {max_m}",
                    dataset.name, field.name
                ),
                _ => {}
            }
        }

        // Junction links with reinforcement: 0.
        let list_link_refs: HashSet<&str> = dataset.data.iter()
            .filter_map(|f| f.content.as_ref()?.from.as_deref())
            .collect();
        for link in &dataset.links {
            if list_link_refs.contains(link.reference.as_str()) { continue; }
            if link.reinforcement != Some(0.0) { continue; }
            let Some(linked_path) = resolve_include(path, &link.file) else { continue };
            let n_eligible = {
                let linked_rows = *row_counts.get(&linked_path).unwrap_or(&0);
                match link.ratio {
                    Some(r) => ((r * linked_rows as f64).round() as usize).max(1).min(linked_rows),
                    None    => linked_rows,
                }
            };
            let junction_rows = *row_counts.get(path).unwrap_or(&0);
            if junction_rows > n_eligible {
                anyhow::bail!(
                    "dataset '{}': `reinforcement: 0` on link '{}' requires junction rows \
                     ({junction_rows}) ≤ eligible pool rows ({n_eligible})",
                    dataset.name, link.reference
                );
            }
        }
    }
    Ok(())
}

/// Walk all datasets and return the set of linked dataset paths that are collect targets.
///
/// A dataset is a collect target when any field (top-level or inside a list-link
/// content block) carries a `reducer: collect` binding whose `bind` target resolves to
/// a field in that dataset.
fn scan_collect_targets(datasets: &HashMap<PathBuf, SyntheticDataset>) -> HashSet<PathBuf> {
    let mut targets = HashSet::new();
    for (path, dataset) in datasets {
        for field in &dataset.data {
            // Top-level collect bindings (Case 1 — junction datasets, activated in Stage 4).
            for binding in field.collect_bindings() {
                if let Some(linked_path) = resolve_collect_bind_target(path, dataset, binding) {
                    targets.insert(linked_path);
                }
            }
            // List-link content field collect bindings (Case 2).
            if let Some(content) = &field.content {
                if content.from.is_some() {
                    for cf in &content.item.fields {
                        for binding in cf.collect_bindings() {
                            if let Some(pool_path) = resolve_collect_bind_target(path, dataset, binding) {
                                targets.insert(pool_path);
                            }
                        }
                    }
                }
            }
        }
    }
    targets
}

/// Resolve the pool dataset path for a single collect binding.
fn resolve_collect_bind_target(
    dataset_path: &Path,
    dataset: &SyntheticDataset,
    binding: &RefBinding,
) -> Option<PathBuf> {
    let bind = binding.bind.as_deref()?;
    let (linked_ref, _) = split_ref(bind)?;
    let link = dataset.links.iter().find(|l| l.reference == linked_ref)?;
    resolve_include(dataset_path, &link.file)
}

/// Look up the declared `default:` for `field_name` in the linked dataset at `linked_path`.
/// Returns `YamlValue::Null` when no default is declared (e.g. for Collect targets
/// where the fallback is an empty list built explicitly by `execute_accumulate_to_linked`).
fn linked_field_default(
    linked_path: &Path,
    field_name: &str,
    datasets: &HashMap<PathBuf, SyntheticDataset>,
) -> YamlValue {
    datasets.get(linked_path)
        .and_then(|ds| ds.data.iter().find(|f| f.name == field_name))
        .and_then(|f| f.default.clone())
        .unwrap_or(YamlValue::Null)
}

/// Build the execution plan from the resolved dataset map and its DAG.
///
/// All row counts, lower cover segments, and inherited field wiring are resolved here.
/// The executor receives a flat list of steps with no branching on dataset shape.
pub fn build_plan(
    dag: &DatasetGraph,
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    max_lower_cover: usize,
) -> Result<ExecutionPlan> {
    let row_counts = plan_row_counts(datasets);
    let lower_cover_groups = build_lower_cover_groups(datasets);
    let lower_cover_set: HashSet<PathBuf> = lower_cover_groups
        .values()
        .flat_map(|members| members.iter().map(|m| m.path.clone()))
        .collect();
    let collect_targets = scan_collect_targets(datasets);
    check_collect_segmentation_restrictions(datasets, &lower_cover_groups)?;
    check_reinforcement_zero_feasibility(datasets, &row_counts)?;

    let mut topo = Topo::new(&dag.graph);
    let mut steps: Vec<ExecutionStep> = Vec::new();
    let mut shared_outputs: Vec<(String, Format)> = Vec::new();
    let mut seen_shared: HashSet<String> = HashSet::new();

    while let Some(idx) = topo.next(&dag.graph) {
        let path = &dag.graph[idx];
        let Some(dataset) = datasets.get(path) else {
            continue;
        };

        // Pure lower cover members (no own lower cover group) are generated inside their parent's step.
        // Datasets that are *both* a member and a parent need their own step so their
        // children are generated first and the result is available when the outer parent runs.
        if lower_cover_set.contains(path) && !lower_cover_groups.contains_key(path) {
            track_shared(dataset, &mut shared_outputs, &mut seen_shared);
            continue;
        }

        // Variant expansion: replace this dataset with N concrete variants.
        // Each variant writes to the same output_file so WriteSharedOutput shuffles them.
        // Note: inherited fields into a variant parent are not wired in v1 — they require
        // a single stable batch to pull columns from; variants produce N separate batches.
        if !dataset.variants.is_empty() {
            let output_key = dataset.output_file.clone().unwrap_or_else(|| dataset.name.clone());
            let variant_dists: Vec<Option<f64>> = dataset.variants.iter().map(|v| v.ratio).collect();
            let dists = resolve_distributions(&variant_dists);
            let row_counts_v = distribute_rows(row_counts[path], &dists);

            for (i, (variant, &variant_rows)) in dataset.variants.iter().zip(row_counts_v.iter()).enumerate() {
                let virtual_path = variant_key(path, i);
                let concrete = expand_variant_dataset(dataset, variant, i, variant_rows, &output_key);

                if let Some(members) = lower_cover_groups.get(path) {
                    // Each flat lower cover member accumulates rows from N variant groups; ensure it has
                    // an output_file so WriteSharedOutput fires once for the combined output.
                    // Witness-source members (is_witness_source=true) have no standalone output.
                    let members_with_output: Vec<LowerCoverMember> = members.iter().map(|m| {
                        let mut s = m.clone();
                        if s.dataset.output_file.is_none() && !s.is_witness_source {
                            s.dataset.output_file = Some(m.dataset.name.clone());
                        }
                        s
                    }).collect();
                    let segments = plan_segments(variant_rows, &members_with_output, max_lower_cover)?;
                    for m in &members_with_output {
                        track_shared(&m.dataset, &mut shared_outputs, &mut seen_shared);
                    }
                    track_shared(&concrete, &mut shared_outputs, &mut seen_shared);
                    let vpath = virtual_path.clone();
                    let c = Arc::new(concrete.clone());
                    let (s_vpath, s_c) = (vpath.clone(), c.clone());
                    let (s_segs, s_mbrs) = (segments.clone(), members_with_output.clone());
                    push_with_list_link_steps(&mut steps, &concrete, &virtual_path, false, datasets,
                        || ExecutionStep::GenerateStagingLowerCoverGroup {
                            parent_path: s_vpath, parent: s_c,
                            segments: s_segs, members: s_mbrs,
                        },
                        |defer| ExecutionStep::GenerateLowerCoverGroup {
                            parent_path: vpath, parent: c,
                            segments, members: members_with_output, defer_emit: defer,
                        },
                    );
                } else {
                    track_shared(&concrete, &mut shared_outputs, &mut seen_shared);
                    let vpath = virtual_path.clone();
                    let c = Arc::new(concrete.clone());
                    let (s_vpath, s_c) = (vpath.clone(), c.clone());
                    push_with_list_link_steps(&mut steps, &concrete, &virtual_path, false, datasets,
                        || ExecutionStep::GenerateStagingNode {
                            path: s_vpath, dataset: s_c,
                            rows: variant_rows, prefills: vec![],
                        },
                        |_| ExecutionStep::GenerateDataset {
                            path: vpath, dataset: c,
                            rows: variant_rows, prefills: vec![], defer_emit: false,
                        },
                    );
                }
            }
            continue;
        }

        if let Some(members) = lower_cover_groups.get(path) {
            let segments = plan_segments(row_counts[path], members, max_lower_cover)?;
            for m in members.iter() {
                track_shared(&m.dataset, &mut shared_outputs, &mut seen_shared);
            }
            track_shared(dataset, &mut shared_outputs, &mut seen_shared);
            let p = path.clone();
            let d = Arc::new(dataset.clone());
            let mbrs = members.clone();
            let is_collect_target = collect_targets.contains(path);
            let (s_p, s_d) = (p.clone(), d.clone());
            let (s_segs, s_mbrs) = (segments.clone(), mbrs.clone());
            push_with_list_link_steps(&mut steps, dataset, path, is_collect_target, datasets,
                || ExecutionStep::GenerateStagingLowerCoverGroup {
                    parent_path: s_p, parent: s_d,
                    segments: s_segs, members: s_mbrs,
                },
                |defer| ExecutionStep::GenerateLowerCoverGroup {
                    parent_path: p, parent: d,
                    segments, members: mbrs, defer_emit: defer,
                },
            );
            // Junction link members: emit AccumulateToLinked + EmitDataset after the group step.
            for m in members {
                if m.is_witness_source { continue; }
                emit_top_level_collect_steps(&m.dataset, &m.path, datasets, &mut steps);
            }
            continue;
        }

        track_shared(dataset, &mut shared_outputs, &mut seen_shared);
        let p = path.clone();
        let d = Arc::new(dataset.clone());
        let prefills = compute_prefills(path, datasets, &lower_cover_set);
        let rows = row_counts[path];
        let is_collect_target = collect_targets.contains(path);
        let (s_p, s_d) = (p.clone(), d.clone());
        let s_prefills = prefills.clone();
        push_with_list_link_steps(&mut steps, dataset, path, is_collect_target, datasets,
            || ExecutionStep::GenerateStagingNode {
                path: s_p, dataset: s_d, rows, prefills: s_prefills,
            },
            |defer| ExecutionStep::GenerateDataset {
                path: p, dataset: d, rows, prefills, defer_emit: defer,
            },
        );
        emit_top_level_collect_steps(dataset, path, datasets, &mut steps);
    }

    for (output_file, format) in shared_outputs {
        steps.push(ExecutionStep::WriteSharedOutput { output_file, format });
    }

    Ok(ExecutionPlan { steps })
}

fn witness_key(staging_path: &Path, field_name: &str) -> PathBuf {
    internal_path(staging_path, &format!("{field_name}___witness"))
}

/// Push a step plus any follow-on witness steps if `dataset` has list-link fields.
///
/// When the dataset has list-link fields, `make_staging()` is called and the resulting
/// staging step is pushed, followed by all witness/assemble steps. When the dataset has no
/// list-link fields, `make_normal(defer_emit)` is called instead (for collect-target
/// deferral, `defer_emit` is passed through; for normal datasets it is `false`).
fn push_with_list_link_steps(
    steps: &mut Vec<ExecutionStep>,
    dataset: &SyntheticDataset,
    path: &Path,
    defer_emit: bool,
    all_datasets: &HashMap<PathBuf, SyntheticDataset>,
    make_staging: impl FnOnce() -> ExecutionStep,
    make_normal: impl FnOnce(bool) -> ExecutionStep,
) {
    if dataset.data.iter().any(|f| f.is_list_link()) {
        steps.push(make_staging());
        emit_witness_steps(dataset, path, all_datasets, steps);
    } else {
        steps.push(make_normal(defer_emit));
    }
}

fn emit_witness_steps(
    dataset: &SyntheticDataset,
    path: &Path,
    all_datasets: &HashMap<PathBuf, SyntheticDataset>,
    steps: &mut Vec<ExecutionStep>,
) {
    let mut witness_specs: Vec<(String, PathBuf, Option<String>)> = Vec::new();
    for field in &dataset.data {
        let Some(content) = &field.content else { continue };
        let Some(ref from_ref) = content.from else { continue };
        let Some(link) = dataset.links.iter().find(|l| l.reference == *from_ref) else { continue };
        let Some(linked_path) = resolve_include(path, &link.file) else { continue };
        let witness_key = witness_key(path, &field.name);
        let cardinality = link.cardinality.clone().unwrap_or(CountSpec::Fixed(1));
        steps.push(ExecutionStep::GenerateWitness {
            witness_key: witness_key.clone(),
            staging_path: path.to_path_buf(),
            list_field_name: field.name.clone(),
            inner_fields: content.item.fields.clone(),
            include: link.clone(),
            cardinality,
            linked_path: linked_path.clone(),
        });
        let project_col = content.project.as_ref()
            .and_then(|p| split_ref(p))
            .map(|(_, f)| f.to_string());
        witness_specs.push((field.name.clone(), witness_key.clone(), project_col));

        // Collect bindings in content fields: insert AccumulateToLinked + EmitDataset
        // between GenerateWitness and AssembleFromWitness so linked-node values
        // accumulate upward before the outer dataset is assembled (Case 2).
        // Pass 1: emit all AccumulateToLinked steps; Pass 2: emit EmitDataset once after all.
        let mut has_collect = false;
        for cf in &content.item.fields {
            for binding in cf.collect_bindings() {
                let Some(bind) = binding.bind.as_deref() else { continue };
                let Some((_, linked_field)) = split_ref(bind) else { continue };
                let lf_name = linked_field.to_string();
                let def = linked_field_default(&linked_path, &lf_name, all_datasets);
                steps.push(ExecutionStep::AccumulateToLinked {
                    source_path:  witness_key.clone(),
                    source_field: cf.name.clone(),
                    linked_path:  linked_path.clone(),
                    linked_field: lf_name,
                    group_by:     "_linked_idx".to_string(),
                    reducer:      binding.reducer.clone().unwrap_or(Reducer::Collect),
                    default_val:  def,
                });
                has_collect = true;
            }
        }
        if has_collect {
            if let Some(linked_ds) = all_datasets.get(&linked_path) {
                steps.push(ExecutionStep::EmitDataset {
                    path:    linked_path.clone(),
                    dataset: Arc::new(linked_ds.clone()),
                });
            }
        }
    }
    if !witness_specs.is_empty() {
        steps.push(ExecutionStep::AssembleFromWitness {
            staging_path: path.to_path_buf(),
            dataset: Arc::new(dataset.clone()),
            witness_specs,
        });
    }
}

/// Emit `AccumulateToLinked` + `EmitDataset` steps for any top-level collect bindings
/// in `dataset` that target a junction link's linked dataset (Case 1).
///
/// All `AccumulateToLinked` steps for a given linked dataset are emitted before its
/// `EmitDataset` so that every reducer result is written before the output file is finalised.
///
/// List-link collect bindings (Case 2) are handled by `emit_witness_steps`.
fn emit_top_level_collect_steps(
    dataset: &SyntheticDataset,
    path: &Path,
    all_datasets: &HashMap<PathBuf, SyntheticDataset>,
    steps: &mut Vec<ExecutionStep>,
) {
    let list_link_refs: HashSet<&str> = dataset.data.iter()
        .filter_map(|f| f.content.as_ref()?.from.as_deref())
        .collect();

    // Pass 1: emit all AccumulateToLinked steps, collecting which linked datasets need EmitDataset.
    let mut linked_to_emit: Vec<(PathBuf, Arc<SyntheticDataset>)> = Vec::new();
    let mut seen_linked: HashSet<PathBuf> = HashSet::new();
    for field in &dataset.data {
        for binding in field.collect_bindings() {
            let Some(bind) = binding.bind.as_deref() else { continue };
            let Some((linked_ref, linked_field)) = split_ref(bind) else { continue };
            let Some(link) = dataset.links.iter().find(|l| l.reference == linked_ref) else { continue };
            if list_link_refs.contains(link.reference.as_str()) { continue; }
            let Some(linked_path) = resolve_include(path, &link.file) else { continue };
            let lf_name = linked_field.to_string();
            let def = linked_field_default(&linked_path, &lf_name, all_datasets);
            steps.push(ExecutionStep::AccumulateToLinked {
                source_path:  path.to_path_buf(),
                source_field: field.name.clone(),
                linked_path:  linked_path.clone(),
                linked_field: lf_name,
                group_by:     "_linked_idx".to_string(),
                reducer:      binding.reducer.clone().unwrap_or(Reducer::Collect),
                default_val:  def,
            });
            if seen_linked.insert(linked_path.clone()) {
                if let Some(linked_ds) = all_datasets.get(&linked_path) {
                    linked_to_emit.push((linked_path.clone(), Arc::new(linked_ds.clone())));
                }
            }
        }
    }

    // Pass 2: emit one EmitDataset per linked dataset, after ALL AccumulateToLinked steps for it.
    for (linked_path, linked_ds) in linked_to_emit {
        steps.push(ExecutionStep::EmitDataset { path: linked_path, dataset: linked_ds });
    }
}

fn track_shared(
    dataset: &SyntheticDataset,
    outputs: &mut Vec<(String, Format)>,
    seen: &mut HashSet<String>,
) {
    if let Some(ref of) = dataset.output_file {
        if seen.insert(of.clone()) {
            outputs.push((of.clone(), dataset.format.clone()));
        }
    }
}

/// Compute the inherited fields for `path` by scanning every child dataset that includes
/// `path` (without a distribution) and has ref fields pointing back to it.
///
/// Because topo order visits children before parents, the child's batch is
/// already in `computed` by the time the parent runs — so the parent can pull
/// from it. Lower cover members are excluded: their ref columns are projected from
/// the parent batch inside `execute_lower_cover_group` instead.
fn compute_prefills(
    path: &Path,
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    lower_cover_set: &HashSet<PathBuf>,
) -> Vec<InheritedField> {
    let mut prefills = Vec::new();
    for (child_path, child_ds) in datasets {
        if lower_cover_set.contains(child_path) {
            continue;
        }
        for include in child_ds.include.iter() {
            let Some(resolved) = resolve_include(child_path, &include.file) else { continue };
            if resolved != path {
                continue;
            }
            for field in &child_ds.data {
                let Some(ref_str) = field.simple_ref() else { continue };
                let Some((ref_part, target_col)) = split_ref(ref_str) else { continue };
                if ref_part != include.reference {
                    continue;
                }
                prefills.push(InheritedField {
                    from_path: child_path.clone(),
                    from_column: field.name.clone(),
                    into_column: target_col.to_string(),
                });
            }
        }
    }
    prefills
}

fn build_lower_cover_groups(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
) -> HashMap<PathBuf, Vec<LowerCoverMember>> {
    let mut groups: HashMap<PathBuf, Vec<LowerCoverMember>> = HashMap::new();
    for (outer_path, dataset) in datasets {
        // Flat lower cover members: datasets that include a parent. Registered unconditionally —
        // even a ratio-1.0 child must enter conflict pruning so its field constraints are applied
        // jointly with lower cover constraints rather than silently winning via join order.
        for include in dataset.include.iter() {
            let Some(parent_path) = resolve_include(outer_path, &include.file) else { continue };
            groups.entry(parent_path).or_default().push(LowerCoverMember {
                path: outer_path.clone(),
                dataset: dataset.clone(),
                ratio: include.ratio.unwrap_or(1.0),
                cardinality: include.cardinality.clone(),
                reference: include.reference.clone(),
                is_witness_source: false,
            });
        }
        // Witness-source members: list-link fields whose content includes another dataset with a
        // ratio. The linked dataset represents the subset eligible for list sampling.
        // Its constraints (from list content ref fields) are applied to the parent's generation
        // rather than producing standalone rows.
        collect_linked_lower_cover_members(outer_path, dataset, &mut groups);
    }
    groups
}

/// Scan `dataset`'s fields for list-link includes and register a witness-source lower cover
/// member for each. Members are keyed by the path of the linked dataset.
fn collect_linked_lower_cover_members(
    outer_path: &Path,
    dataset: &SyntheticDataset,
    groups: &mut HashMap<PathBuf, Vec<LowerCoverMember>>,
) {
    for_each_list_link(&dataset.links, &dataset.data, &mut |field, link, item_fields| {
        let Some(inc_path) = resolve_include(outer_path, &link.file) else { return };
        // Virtual path uniquely identifies this witness-source member within the outer dataset.
        let member_path = linked_lower_cover_path(outer_path, &field.name);
        // The member "dataset" carries only the list-content item fields so that
        // lower_cover_field_constraints can extract ref-based constraints from them.
        let member_dataset = SyntheticDataset {
            name: format!("{}__{}_linked", dataset.name, field.name),
            format: dataset.format.clone(),
            output_file: None,
            rows: None,
            locale: dataset.locale.clone(),
            include: None,
            links: vec![],
            data: item_fields.to_vec(),
            variants: vec![],
        };
        groups.entry(inc_path).or_default().push(LowerCoverMember {
            path: member_path,
            dataset: member_dataset,
            ratio: link.ratio.unwrap_or(1.0),
            cardinality: None,
            reference: link.reference.clone(),
            is_witness_source: true,
        });
    });
}

fn linked_lower_cover_path(outer_path: &Path, field_name: &str) -> PathBuf {
    internal_path(outer_path, &format!("{field_name}___linked"))
}

fn plan_row_counts(datasets: &HashMap<PathBuf, SyntheticDataset>) -> HashMap<PathBuf, usize> {
    let mut counts: HashMap<PathBuf, usize> = datasets
        .iter()
        .filter_map(|(path, ds)| ds.rows.map(|r| (path.clone(), r)))
        .collect();

    loop {
        let mut changed = false;
        for (path, dataset) in datasets {
            if counts.contains_key(path) {
                continue;
            }
            if let Some(n) = rows_from_includes(path, dataset, &counts)
                .or_else(|| rows_from_children(path, datasets, &counts))
            {
                counts.insert(path.clone(), n);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    for path in datasets.keys() {
        counts.entry(path.clone()).or_insert(DEFAULT_ROWS);
    }
    counts
}

fn rows_from_includes(
    path: &Path,
    dataset: &SyntheticDataset,
    counts: &HashMap<PathBuf, usize>,
) -> Option<usize> {
    let inc = dataset.include.as_ref()?;
    let r = inc.ratio.unwrap_or(1.0);
    let inc_rows = *counts.get(&resolve_include(path, &inc.file)?)?;
    let base_rows = (inc_rows as f64 * r).round() as usize;
    let rows = if let Some(card) = &inc.cardinality {
        (base_rows as f64 * expected_cardinality(card)).round() as usize
    } else {
        base_rows
    };
    Some(rows.max(1))
}

fn rows_from_children(
    path: &Path,
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    counts: &HashMap<PathBuf, usize>,
) -> Option<usize> {
    datasets
        .iter()
        .flat_map(|(other_path, other_ds)| {
            other_ds.include.iter().map(move |inc| (other_path, inc))
        })
        .find_map(|(other_path, inc)| {
            let r = inc.ratio.filter(|&r| r > 0.0)?;
            if resolve_include(other_path, &inc.file)? != *path {
                return None;
            }
            let other_rows = *counts.get(other_path)?;
            Some(((other_rows as f64 / r).round() as usize).max(1))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Format, Schema, SyntheticDataset};

    fn bare_dataset(rows: Option<usize>) -> SyntheticDataset {
        SyntheticDataset {
            name: "test".into(),
            format: Format::Csv,
            rows,
            output_file: None,
            locale: None,
            include: None,
            links: vec![],
            data: Schema::default(),
            variants: vec![],
        }
    }

    #[test]
    fn explicit_rows_used_directly() {
        let path = PathBuf::from("/a/b/c.yaml");
        let mut datasets = HashMap::new();
        datasets.insert(path.clone(), bare_dataset(Some(77)));
        assert_eq!(plan_row_counts(&datasets)[&path], 77);
    }

    #[test]
    fn default_rows_when_nothing_resolves() {
        let path = PathBuf::from("/a/b/c.yaml");
        let mut datasets = HashMap::new();
        datasets.insert(path.clone(), bare_dataset(None));
        assert_eq!(plan_row_counts(&datasets)[&path], DEFAULT_ROWS);
    }

    #[test]
    fn multiple_explicit_rows_independent() {
        let p1 = PathBuf::from("/a/one.yaml");
        let p2 = PathBuf::from("/a/two.yaml");
        let mut datasets = HashMap::new();
        datasets.insert(p1.clone(), bare_dataset(Some(10)));
        datasets.insert(p2.clone(), bare_dataset(Some(25)));
        let counts = plan_row_counts(&datasets);
        assert_eq!(counts[&p1], 10);
        assert_eq!(counts[&p2], 25);
    }
}
