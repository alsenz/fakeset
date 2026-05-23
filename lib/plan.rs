use anyhow::Result;
use petgraph::visit::Topo;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use serde_yaml::Value as YamlValue;

use crate::segment::{plan_segments, Segment, Sibling};
use crate::graph::DatasetGraph;
use crate::models::{expected_cardinality, for_each_link_content, resolve_distributions, resolve_include, split_ref, CountSpec, Field, Format, Include, Locale, Reducer, RefBinding, Schema, SyntheticDataset, VariantSchema};
use crate::rewrite::apply_locale_to_schema;

const DEFAULT_ROWS: usize = 100;

/// Wires a pre-generated column from an already-computed batch into a field of
/// the dataset being generated. Produced from `ref_field` strings at plan time;
/// consumed by the executor so ref columns are never re-generated.
#[derive(Debug)]
pub struct PrefillSource {
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
    /// When `skip_emit` is true the dataset has nested include fields: the scalar batch is stored
    /// in `computed` for `GenerateInnerFlat` to read, and `AssembleNestedInclude` does the emit.
    GenerateDataset {
        path: PathBuf,
        dataset: Arc<SyntheticDataset>,
        rows: usize,
        prefills: Vec<PrefillSource>,
        skip_emit: bool,
    },
    /// Generate a segmented parent and fan row segments out to siblings.
    /// When `skip_parent_emit` is true the parent has nested include fields: expressions and emit
    /// are deferred to `AssembleNestedInclude`; only the shuffled scalar batch is stored in
    /// `computed`.
    GenerateSiblingGroup {
        parent_path: PathBuf,
        parent: Arc<SyntheticDataset>,
        segments: Vec<Segment>,
        siblings: Vec<Sibling>,
        skip_parent_emit: bool,
    },
    /// Generate the joint-atom flat for one nested-include list field.
    ///
    /// Each row of the resulting batch is one **atom**: a single (outer-slot, pool-slot) pair.
    /// The batch is stored in `computed[flat_key]` and contains:
    ///   - `_slot_idx: UInt32` — which outer row this atom belongs to
    ///   - `_pool_idx: UInt32` — which pool slot this atom was assigned to
    ///   - one column per `inner_fields` field, resolved as follows:
    ///       - pool-scoped refs: the pushed-down pool-slot solution for `_pool_idx`
    ///       - outer-scoped refs: the outer row value for `_slot_idx`
    ///       - plain fields: generated fresh per atom
    ///
    /// `pool_slots_path` is the path of the pre-solved pool-slot batch (one row per eligible
    /// pool slot). Atom rows sharing the same `_pool_idx` carry identical pool-scoped values
    /// because they reference the same pre-solved slot.
    GenerateInnerFlat {
        flat_key: PathBuf,
        outer_path: PathBuf,
        list_field_name: String,
        inner_fields: Vec<Field>,
        include: Include,
        cardinality: CountSpec,
        pool_slots_path: PathBuf,
    },
    /// Assemble nested include columns into the outer batch and emit.
    ///
    /// Reads the scalar outer batch and each inner-flat from `computed`, builds one
    /// `ListArray` per spec, appends them to the outer batch, evaluates expressions,
    /// filters hidden columns, and writes output.
    AssembleNestedInclude {
        outer_path: PathBuf,
        dataset: Arc<SyntheticDataset>,
        /// `(list_field_name, flat_key, project_col)` — `project_col` is `Some(col_name)`
        /// when `content.project` is set, causing scalar-list assembly for that field.
        flat_specs: Vec<(String, PathBuf, Option<String>)>,
    },
    /// Accumulate values from a source batch into a pool dataset's field in `computed`.
    ///
    /// Groups source rows by `group_by` (always `"_pool_idx"` for MULT-2), aggregates the
    /// `source_field` column using `reducer`, and writes the result into the pool batch's
    /// `pool_field` column. Pool rows with no matching source rows receive the pool field's
    /// `default` value.
    CollectToPool {
        source_path:  PathBuf,
        source_field: String,
        pool_path:    PathBuf,
        pool_field:   String,
        group_by:     String,
        reducer:      Reducer,
        /// Declared `default:` from the pool field YAML, used as the fallback value for
        /// pool rows that have no matching source rows (scalar reducers only).
        /// For `Collect` the empty-list is built explicitly; this field is ignored.
        default_val:  serde_yaml::Value,
    },
    /// Emit the batch at `path` from `computed` to an output file.
    ///
    /// Used after `CollectToPool` to write the now-updated pool batch. Applies
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
/// must not be jointly segmented with another (non-pool) sibling. When it is, the correct
/// approach is a top-level junction dataset (Case 1).
fn check_case2_collect_restrictions(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    sibling_groups: &HashMap<PathBuf, Vec<Sibling>>,
) -> Result<()> {
    for (path, dataset) in datasets {
        for field in &dataset.data {
            let Some(content) = &field.content else { continue };
            let Some(group_ref) = &content.group else { continue };
            let has_collect = content.item.fields.iter().any(|cf| !cf.collect_bindings().is_empty());
            if !has_collect { continue; }
            let Some(link) = dataset.links.iter().find(|l| l.reference == *group_ref) else { continue };
            let Some(pool_path) = resolve_include(path, &link.file) else { continue };
            if let Some(siblings) = sibling_groups.get(&pool_path) {
                if siblings.iter().any(|s| !s.is_pool) {
                    anyhow::bail!(
                        "dataset '{}': nested-include collect on field '{}' is not supported \
                         when the pool dataset is jointly segmented with another sibling; \
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
            let Some(group_ref) = &content.group else { continue };
            let Some(link) = dataset.links.iter().find(|l| l.reference == *group_ref) else { continue };
            if link.reinforcement != Some(0.0) { continue; }
            let Some(pool_path) = resolve_include(path, &link.file) else { continue };
            let n_eligible = {
                let pool_rows = *row_counts.get(&pool_path).unwrap_or(&0);
                match link.ratio {
                    Some(r) => ((r * pool_rows as f64).round() as usize).max(1).min(pool_rows),
                    None    => pool_rows,
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
            .filter_map(|f| f.content.as_ref()?.group.as_deref())
            .collect();
        for link in &dataset.links {
            if list_link_refs.contains(link.reference.as_str()) { continue; }
            if link.reinforcement != Some(0.0) { continue; }
            let Some(pool_path) = resolve_include(path, &link.file) else { continue };
            let n_eligible = {
                let pool_rows = *row_counts.get(&pool_path).unwrap_or(&0);
                match link.ratio {
                    Some(r) => ((r * pool_rows as f64).round() as usize).max(1).min(pool_rows),
                    None    => pool_rows,
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

/// Walk all datasets and return the set of pool dataset paths that are collect targets.
///
/// A dataset is a collect target when any field (top-level or inside a nested-include
/// content block) carries a `reducer: collect` binding whose `bind` target resolves to
/// a field in that dataset.
fn scan_collect_targets(datasets: &HashMap<PathBuf, SyntheticDataset>) -> HashSet<PathBuf> {
    let mut targets = HashSet::new();
    for (path, dataset) in datasets {
        for field in &dataset.data {
            // Top-level collect bindings (Case 1 — junction datasets, activated in Stage 4).
            for binding in field.collect_bindings() {
                if let Some(pool_path) = resolve_collect_bind_target(path, dataset, binding) {
                    targets.insert(pool_path);
                }
            }
            // Nested-include content field collect bindings (Case 2).
            if let Some(content) = &field.content {
                if content.group.is_some() {
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
    let (pool_ref, _) = split_ref(bind)?;
    let link = dataset.links.iter().find(|l| l.reference == pool_ref)?;
    resolve_include(dataset_path, &link.file)
}

/// Look up the declared `default:` for `field_name` in the pool dataset at `pool_path`.
/// Returns `YamlValue::Null` when no default is declared (e.g. for Collect targets
/// where the fallback is an empty list built explicitly by `execute_collect_to_pool`).
fn pool_field_default(
    pool_path: &Path,
    field_name: &str,
    datasets: &HashMap<PathBuf, SyntheticDataset>,
) -> YamlValue {
    datasets.get(pool_path)
        .and_then(|ds| ds.data.iter().find(|f| f.name == field_name))
        .and_then(|f| f.default.clone())
        .unwrap_or(YamlValue::Null)
}

/// Build the execution plan from the resolved dataset map and its DAG.
///
/// All row counts, sibling segments, and prefill wiring are resolved here.
/// The executor receives a flat list of steps with no branching on dataset shape.
pub fn build_plan(
    dag: &DatasetGraph,
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    max_siblings: usize,
) -> Result<ExecutionPlan> {
    let row_counts = plan_row_counts(datasets);
    let sibling_groups = build_sibling_groups(datasets);
    let sibling_set: HashSet<PathBuf> = sibling_groups
        .values()
        .flat_map(|sibs| sibs.iter().map(|s| s.path.clone()))
        .collect();
    let collect_targets = scan_collect_targets(datasets);
    check_case2_collect_restrictions(datasets, &sibling_groups)?;
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

        // Pure siblings (no own sibling group) are generated inside their parent's step.
        // Datasets that are *both* a sibling and a parent need their own step so their
        // children are generated first and the result is available when the outer parent runs.
        if sibling_set.contains(path) && !sibling_groups.contains_key(path) {
            track_shared(dataset, &mut shared_outputs, &mut seen_shared);
            continue;
        }

        // Variant expansion: replace this dataset with N concrete variants.
        // Each variant writes to the same output_file so WriteSharedOutput shuffles them.
        // Note: prefill refs into a variant parent are not wired in v1 — prefills require
        // a single stable batch to pull columns from; variants produce N separate batches.
        if !dataset.variants.is_empty() {
            let output_key = dataset.output_file.clone().unwrap_or_else(|| dataset.name.clone());
            let variant_dists: Vec<Option<f64>> = dataset.variants.iter().map(|v| v.ratio).collect();
            let dists = resolve_distributions(&variant_dists);
            let row_counts_v = distribute_rows(row_counts[path], &dists);

            for (i, (variant, &variant_rows)) in dataset.variants.iter().zip(row_counts_v.iter()).enumerate() {
                let virtual_path = variant_key(path, i);
                let concrete = expand_variant_dataset(dataset, variant, i, variant_rows, &output_key);

                if let Some(siblings) = sibling_groups.get(path) {
                    // Each flat sibling accumulates rows from N variant groups; ensure it has an
                    // output_file so WriteSharedOutput fires once for the combined output.
                    // Pool siblings (is_pool=true) have no standalone output — leave them as-is.
                    let siblings_with_output: Vec<Sibling> = siblings.iter().map(|sib| {
                        let mut s = sib.clone();
                        if s.dataset.output_file.is_none() && !s.is_pool {
                            s.dataset.output_file = Some(sib.dataset.name.clone());
                        }
                        s
                    }).collect();
                    let segments = plan_segments(variant_rows, &siblings_with_output, max_siblings)?;
                    for sib in &siblings_with_output {
                        track_shared(&sib.dataset, &mut shared_outputs, &mut seen_shared);
                    }
                    track_shared(&concrete, &mut shared_outputs, &mut seen_shared);
                    let vpath = virtual_path.clone();
                    let c = Arc::new(concrete.clone());
                    push_with_nested_include(&mut steps, &concrete, &virtual_path, false, datasets, |rich| {
                        ExecutionStep::GenerateSiblingGroup {
                            parent_path: vpath,
                            parent: c,
                            segments,
                            siblings: siblings_with_output,
                            skip_parent_emit: rich,
                        }
                    });
                } else {
                    track_shared(&concrete, &mut shared_outputs, &mut seen_shared);
                    let vpath = virtual_path.clone();
                    let c = Arc::new(concrete.clone());
                    push_with_nested_include(&mut steps, &concrete, &virtual_path, false, datasets, |rich| {
                        ExecutionStep::GenerateDataset {
                            path: vpath,
                            dataset: c,
                            rows: variant_rows,
                            prefills: vec![],
                            skip_emit: rich,
                        }
                    });
                }
            }
            continue;
        }

        if let Some(siblings) = sibling_groups.get(path) {
            let segments = plan_segments(row_counts[path], siblings, max_siblings)?;
            for sib in siblings.iter() {
                track_shared(&sib.dataset, &mut shared_outputs, &mut seen_shared);
            }
            track_shared(dataset, &mut shared_outputs, &mut seen_shared);
            let p = path.clone();
            let d = Arc::new(dataset.clone());
            let sibs = siblings.clone();
            let is_collect_target = collect_targets.contains(path);
            push_with_nested_include(&mut steps, dataset, path, is_collect_target, datasets, |rich| {
                ExecutionStep::GenerateSiblingGroup {
                    parent_path: p,
                    parent: d,
                    segments,
                    siblings: sibs,
                    skip_parent_emit: rich,
                }
            });
            // Junction link siblings: emit CollectToPool + EmitDataset after the group step.
            for sib in siblings {
                if sib.is_pool { continue; }
                emit_top_level_collect_steps(&sib.dataset, &sib.path, datasets, &mut steps);
            }
            continue;
        }

        track_shared(dataset, &mut shared_outputs, &mut seen_shared);
        let p = path.clone();
        let d = Arc::new(dataset.clone());
        let prefills = compute_prefills(path, datasets, &sibling_set);
        let rows = row_counts[path];
        let is_collect_target = collect_targets.contains(path);
        push_with_nested_include(&mut steps, dataset, path, is_collect_target, datasets, |rich| {
            ExecutionStep::GenerateDataset {
                path: p,
                dataset: d,
                rows,
                prefills,
                skip_emit: rich,
            }
        });
        emit_top_level_collect_steps(dataset, path, datasets, &mut steps);
    }

    for (output_file, format) in shared_outputs {
        steps.push(ExecutionStep::WriteSharedOutput { output_file, format });
    }

    Ok(ExecutionPlan { steps })
}

fn inner_flat_key(outer_path: &Path, field_name: &str) -> PathBuf {
    internal_path(outer_path, &format!("{field_name}___flat"))
}

/// Push a step plus any follow-on nested include steps if `dataset` has nested include fields.
/// `skip_emit_extra` is ORed with the nested-include `rich` flag — when either is true the
/// main step's skip flag is set and expression evaluation / file emit are deferred.
fn push_with_nested_include(
    steps: &mut Vec<ExecutionStep>,
    dataset: &SyntheticDataset,
    path: &Path,
    skip_emit_extra: bool,
    all_datasets: &HashMap<PathBuf, SyntheticDataset>,
    make_step: impl FnOnce(bool) -> ExecutionStep,
) {
    let has_link_content = dataset.data.iter().any(|f| f.is_link_content());
    steps.push(make_step(has_link_content || skip_emit_extra));
    if has_link_content {
        emit_nested_include_steps(dataset, path, all_datasets, steps);
    }
}

fn emit_nested_include_steps(
    dataset: &SyntheticDataset,
    path: &Path,
    all_datasets: &HashMap<PathBuf, SyntheticDataset>,
    steps: &mut Vec<ExecutionStep>,
) {
    let mut flat_specs: Vec<(String, PathBuf, Option<String>)> = Vec::new();
    for field in &dataset.data {
        let Some(content) = &field.content else { continue };
        let Some(ref group_ref) = content.group else { continue };
        let Some(link) = dataset.links.iter().find(|l| l.reference == *group_ref) else { continue };
        let Some(pool_slots_path) = resolve_include(path, &link.file) else { continue };
        let flat_key = inner_flat_key(path, &field.name);
        let cardinality = link.cardinality.clone().unwrap_or(CountSpec::Fixed(1));
        steps.push(ExecutionStep::GenerateInnerFlat {
            flat_key: flat_key.clone(),
            outer_path: path.to_path_buf(),
            list_field_name: field.name.clone(),
            inner_fields: content.item.fields.clone(),
            include: link.clone(),
            cardinality,
            pool_slots_path: pool_slots_path.clone(),
        });
        let project_col = content.project.as_ref()
            .and_then(|p| split_ref(p))
            .map(|(_, f)| f.to_string());
        flat_specs.push((field.name.clone(), flat_key.clone(), project_col));

        // Collect bindings in content fields: insert CollectToPool + EmitDataset
        // between GenerateInnerFlat and AssembleNestedInclude so pool-node values
        // accumulate upward before the outer dataset is assembled (Case 2).
        // Pass 1: emit all CollectToPool steps; Pass 2: emit EmitDataset once after all.
        let mut has_collect = false;
        for cf in &content.item.fields {
            for binding in cf.collect_bindings() {
                let Some(bind) = binding.bind.as_deref() else { continue };
                let Some((_, pool_field)) = split_ref(bind) else { continue };
                let pf_name = pool_field.to_string();
                let def = pool_field_default(&pool_slots_path, &pf_name, all_datasets);
                steps.push(ExecutionStep::CollectToPool {
                    source_path:  flat_key.clone(),
                    source_field: cf.name.clone(),
                    pool_path:    pool_slots_path.clone(),
                    pool_field:   pf_name,
                    group_by:     "_pool_idx".to_string(),
                    reducer:      binding.reducer.clone().unwrap_or(Reducer::Collect),
                    default_val:  def,
                });
                has_collect = true;
            }
        }
        if has_collect {
            if let Some(pool_ds) = all_datasets.get(&pool_slots_path) {
                steps.push(ExecutionStep::EmitDataset {
                    path:    pool_slots_path.clone(),
                    dataset: Arc::new(pool_ds.clone()),
                });
            }
        }
    }
    if !flat_specs.is_empty() {
        steps.push(ExecutionStep::AssembleNestedInclude {
            outer_path: path.to_path_buf(),
            dataset: Arc::new(dataset.clone()),
            flat_specs,
        });
    }
}

/// Emit `CollectToPool` + `EmitDataset` steps for any top-level collect bindings
/// in `dataset` that target a junction link's pool dataset (Case 1).
///
/// All `CollectToPool` steps for a given pool are emitted before that pool's `EmitDataset`
/// so that every reducer result is written before the output file is finalised.
///
/// List-link collect bindings (Case 2) are handled by `emit_nested_include_steps`.
fn emit_top_level_collect_steps(
    dataset: &SyntheticDataset,
    path: &Path,
    all_datasets: &HashMap<PathBuf, SyntheticDataset>,
    steps: &mut Vec<ExecutionStep>,
) {
    let list_link_refs: HashSet<&str> = dataset.data.iter()
        .filter_map(|f| f.content.as_ref()?.group.as_deref())
        .collect();

    // Pass 1: emit all CollectToPool steps, collecting which pools need EmitDataset.
    let mut pools_to_emit: Vec<(PathBuf, Arc<SyntheticDataset>)> = Vec::new();
    let mut seen_pools: HashSet<PathBuf> = HashSet::new();
    for field in &dataset.data {
        for binding in field.collect_bindings() {
            let Some(bind) = binding.bind.as_deref() else { continue };
            let Some((pool_ref, pool_field)) = split_ref(bind) else { continue };
            let Some(link) = dataset.links.iter().find(|l| l.reference == pool_ref) else { continue };
            if list_link_refs.contains(link.reference.as_str()) { continue; }
            let Some(pool_path) = resolve_include(path, &link.file) else { continue };
            let pf_name = pool_field.to_string();
            let def = pool_field_default(&pool_path, &pf_name, all_datasets);
            steps.push(ExecutionStep::CollectToPool {
                source_path:  path.to_path_buf(),
                source_field: field.name.clone(),
                pool_path:    pool_path.clone(),
                pool_field:   pf_name,
                group_by:     "_pool_idx".to_string(),
                reducer:      binding.reducer.clone().unwrap_or(Reducer::Collect),
                default_val:  def,
            });
            if seen_pools.insert(pool_path.clone()) {
                if let Some(pool_ds) = all_datasets.get(&pool_path) {
                    pools_to_emit.push((pool_path.clone(), Arc::new(pool_ds.clone())));
                }
            }
        }
    }

    // Pass 2: emit one EmitDataset per pool, after ALL CollectToPool steps for that pool.
    for (pool_path, pool_ds) in pools_to_emit {
        steps.push(ExecutionStep::EmitDataset { path: pool_path, dataset: pool_ds });
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

/// Compute the prefills for `path` by scanning every child dataset that includes
/// `path` (without a distribution) and has ref fields pointing back to it.
///
/// Because topo order visits children before parents, the child's batch is
/// already in `computed` by the time the parent runs — so the parent can pull
/// from it. Siblings are excluded: their ref columns are projected from
/// the parent batch inside `execute_sibling_group` instead.
fn compute_prefills(
    path: &Path,
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    sibling_set: &HashSet<PathBuf>,
) -> Vec<PrefillSource> {
    let mut prefills = Vec::new();
    for (child_path, child_ds) in datasets {
        if sibling_set.contains(child_path) {
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
                prefills.push(PrefillSource {
                    from_path: child_path.clone(),
                    from_column: field.name.clone(),
                    into_column: target_col.to_string(),
                });
            }
        }
    }
    prefills
}

fn build_sibling_groups(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
) -> HashMap<PathBuf, Vec<Sibling>> {
    let mut groups: HashMap<PathBuf, Vec<Sibling>> = HashMap::new();
    for (outer_path, dataset) in datasets {
        // Flat siblings: datasets that include a parent. Registered unconditionally — even a
        // ratio-1.0 child must enter conflict pruning so its field constraints are applied
        // jointly with sibling constraints rather than silently winning via join order.
        for include in dataset.include.iter() {
            let Some(parent_path) = resolve_include(outer_path, &include.file) else { continue };
            groups.entry(parent_path).or_default().push(Sibling {
                path: outer_path.clone(),
                dataset: dataset.clone(),
                ratio: include.ratio.unwrap_or(1.0),
                cardinality: include.cardinality.clone(),
                reference: include.reference.clone(),
                is_pool: false,
            });
        }
        // Pool siblings: nested-include fields whose content includes another dataset with a
        // ratio. The pool represents the subset of the included dataset eligible
        // for list sampling. Its constraints (from list content ref fields) are applied
        // to the parent's generation rather than producing standalone rows.
        collect_pool_siblings(outer_path, dataset, &mut groups);
    }
    groups
}

/// Scan `dataset`'s fields for list-content includes with a distribution and register a
/// pool sibling for each. Pool siblings are keyed by the path of the included dataset.
fn collect_pool_siblings(
    outer_path: &Path,
    dataset: &SyntheticDataset,
    groups: &mut HashMap<PathBuf, Vec<Sibling>>,
) {
    for_each_link_content(&dataset.links, &dataset.data, &mut |field, link, item_fields| {
        let Some(inc_path) = resolve_include(outer_path, &link.file) else { return };
        // Virtual path uniquely identifies this pool within the outer dataset.
        let pool_path = pool_sibling_path(outer_path, &field.name);
        // The pool "dataset" carries only the list-content item fields so that
        // sibling_field_constraints can extract ref-based constraints from them.
        let pool_dataset = SyntheticDataset {
            name: format!("{}__{}_pool", dataset.name, field.name),
            format: dataset.format.clone(),
            output_file: None,
            rows: None,
            locale: dataset.locale.clone(),
            include: None,
            links: vec![],
            data: item_fields.to_vec(),
            variants: vec![],
        };
        groups.entry(inc_path).or_default().push(Sibling {
            path: pool_path,
            dataset: pool_dataset,
            ratio: link.ratio.unwrap_or(1.0),
            cardinality: None,
            reference: link.reference.clone(),
            is_pool: true,
        });
    });
}

fn pool_sibling_path(outer_path: &Path, field_name: &str) -> PathBuf {
    internal_path(outer_path, &format!("{field_name}___pool"))
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
