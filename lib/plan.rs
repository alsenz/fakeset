use anyhow::Result;
use petgraph::visit::Topo;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::segment::{plan_segments, Segment, Sibling};
use crate::graph::DatasetGraph;
use crate::models::{for_each_content_include, resolve_distributions, resolve_include, split_ref, CountSpec, Field, Format, Include, Locale, Schema, SyntheticDataset, VariantSchema};
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
    /// When `skip_emit` is true the dataset has rich list fields: the scalar batch is stored
    /// in `computed` for `GenerateInnerFlat` to read, and `AssembleRichList` does the emit.
    GenerateDataset {
        path: PathBuf,
        dataset: Arc<SyntheticDataset>,
        rows: usize,
        prefills: Vec<PrefillSource>,
        skip_emit: bool,
    },
    /// Generate a segmented parent and fan row segments out to siblings.
    /// When `skip_parent_emit` is true the parent has rich list fields: expressions and emit
    /// are deferred to `AssembleRichList`; only the shuffled scalar batch is stored in
    /// `computed`.
    GenerateSiblingGroup {
        parent_path: PathBuf,
        parent: Arc<SyntheticDataset>,
        segments: Vec<Segment>,
        siblings: Vec<Sibling>,
        skip_parent_emit: bool,
    },
    /// Generate the flat intermediate for one rich list field.
    ///
    /// Produces a RecordBatch stored in `computed[flat_key]` with:
    ///   - `_outer_idx: UInt32` — which outer row this item belongs to
    ///   - one column per `inner_fields` field — sourced from the include batch (include-scoped
    ///     refs), the outer batch (outer-scoped refs), or generated fresh (plain fields)
    GenerateInnerFlat {
        flat_key: PathBuf,
        outer_path: PathBuf,
        list_field_name: String,
        inner_fields: Vec<Field>,
        includes: Vec<Include>,
        count: CountSpec,
        include_path: PathBuf,
        include_distribution: Option<f64>,
    },
    /// Assemble rich list columns into the outer batch and emit.
    ///
    /// Reads the scalar outer batch and each inner-flat from `computed`, builds one
    /// `ListArray` per spec, appends them to the outer batch, evaluates expressions,
    /// filters hidden columns, and writes output.
    AssembleRichList {
        outer_path: PathBuf,
        dataset: Arc<SyntheticDataset>,
        flat_specs: Vec<(String, PathBuf)>,
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
        includes: base.includes.clone(),
        data: merge_variant_fields(&base.data, &variant_fields),
        variants: vec![],
    }
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
            let variant_dists: Vec<Option<f64>> = dataset.variants.iter().map(|v| v.distribution).collect();
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
                    push_with_rich_list(&mut steps, &concrete, &virtual_path, |rich| {
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
                    push_with_rich_list(&mut steps, &concrete, &virtual_path, |rich| {
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
            push_with_rich_list(&mut steps, dataset, path, |rich| {
                ExecutionStep::GenerateSiblingGroup {
                    parent_path: p,
                    parent: d,
                    segments,
                    siblings: sibs,
                    skip_parent_emit: rich,
                }
            });
            continue;
        }

        track_shared(dataset, &mut shared_outputs, &mut seen_shared);
        let p = path.clone();
        let d = Arc::new(dataset.clone());
        let prefills = compute_prefills(path, datasets, &sibling_set);
        let rows = row_counts[path];
        push_with_rich_list(&mut steps, dataset, path, |rich| {
            ExecutionStep::GenerateDataset {
                path: p,
                dataset: d,
                rows,
                prefills,
                skip_emit: rich,
            }
        });
    }

    for (output_file, format) in shared_outputs {
        steps.push(ExecutionStep::WriteSharedOutput { output_file, format });
    }

    Ok(ExecutionPlan { steps })
}

fn inner_flat_key(outer_path: &Path, field_name: &str) -> PathBuf {
    internal_path(outer_path, &format!("{field_name}___flat"))
}

/// Push a step plus any follow-on rich list steps if `dataset` has rich list fields.
/// The `skip` flag on the step is set to `true` when rich list steps are needed so that
/// expression evaluation and emit are deferred to `AssembleRichList`.
fn push_with_rich_list(
    steps: &mut Vec<ExecutionStep>,
    dataset: &SyntheticDataset,
    path: &Path,
    make_step: impl FnOnce(bool) -> ExecutionStep,
) {
    let rich = dataset.data.iter().any(|f| f.is_rich_list());
    steps.push(make_step(rich));
    if rich {
        emit_rich_list_steps(dataset, path, steps);
    }
}

fn emit_rich_list_steps(
    dataset: &SyntheticDataset,
    path: &Path,
    steps: &mut Vec<ExecutionStep>,
) {
    let mut flat_specs: Vec<(String, PathBuf)> = Vec::new();
    for field in &dataset.data {
        let Some(content) = &field.content else { continue };
        if content.includes.is_empty() {
            continue;
        }
        let inc = &content.includes[0];
        let Some(inc_path) = resolve_include(path, &inc.file) else { continue };
        let flat_key = inner_flat_key(path, &field.name);
        steps.push(ExecutionStep::GenerateInnerFlat {
            flat_key: flat_key.clone(),
            outer_path: path.to_path_buf(),
            list_field_name: field.name.clone(),
            inner_fields: content.item.fields.clone(),
            includes: content.includes.clone(),
            count: field.count.as_ref().cloned().unwrap_or(CountSpec::Fixed(1)),
            include_path: inc_path,
            include_distribution: inc.distribution,
        });
        flat_specs.push((field.name.clone(), flat_key));
    }
    if !flat_specs.is_empty() {
        steps.push(ExecutionStep::AssembleRichList {
            outer_path: path.to_path_buf(),
            dataset: Arc::new(dataset.clone()),
            flat_specs,
        });
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
        for include in &child_ds.includes {
            let Some(resolved) = resolve_include(child_path, &include.file) else { continue };
            if resolved != path {
                continue;
            }
            for field in &child_ds.data {
                let Some(ref ref_str) = field.ref_field else { continue };
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
        // Flat siblings: datasets that include a parent with a distribution.
        for include in &dataset.includes {
            let Some(d) = include.distribution else { continue };
            let Some(parent_path) = resolve_include(outer_path, &include.file) else { continue };
            groups.entry(parent_path).or_default().push(Sibling {
                path: outer_path.clone(),
                dataset: dataset.clone(),
                distribution: d,
                reference: include.reference.clone(),
                is_pool: false,
            });
        }
        // Pool siblings: rich-list fields whose content includes another dataset with a
        // distribution. The pool represents the subset of the included dataset eligible
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
    for_each_content_include(&dataset.data, &mut |field, inc, item_fields| {
        let Some(d) = inc.distribution else { return };
        let Some(inc_path) = resolve_include(outer_path, &inc.file) else { return };
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
            includes: vec![],
            data: item_fields.to_vec(),
            variants: vec![],
        };
        groups.entry(inc_path).or_default().push(Sibling {
            path: pool_path,
            dataset: pool_dataset,
            distribution: d,
            reference: inc.reference.clone(),
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
    dataset.includes.iter().find_map(|inc| {
        let d = inc.distribution.unwrap_or(1.0);
        let inc_rows = *counts.get(&resolve_include(path, &inc.file)?)?;
        Some(((inc_rows as f64 * d).round() as usize).max(1))
    })
}

fn rows_from_children(
    path: &Path,
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    counts: &HashMap<PathBuf, usize>,
) -> Option<usize> {
    datasets
        .iter()
        .flat_map(|(other_path, other_ds)| {
            other_ds.includes.iter().map(move |inc| (other_path, inc))
        })
        .find_map(|(other_path, inc)| {
            let d = inc.distribution.filter(|&d| d > 0.0)?;
            if resolve_include(other_path, &inc.file)? != *path {
                return None;
            }
            let other_rows = *counts.get(other_path)?;
            Some(((other_rows as f64 / d).round() as usize).max(1))
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
            includes: vec![],
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
