use anyhow::Result;
use petgraph::visit::Topo;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::segment::{plan_segments, Segment, Sibling};
use crate::graph::DatasetGraph;
use crate::models::{resolve_include, split_ref, CountSpec, Field, Format, Include, Locale, Schema, SyntheticDataset, VariantSchema};
use crate::rewrite::stamp_locale;

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
        dataset: SyntheticDataset,
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
        parent: SyntheticDataset,
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
        dataset: SyntheticDataset,
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

fn variant_key(path: &Path, i: usize) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{stem}___variant_{i}.internal"))
}

/// Fill in distributions for variants that have none, sharing the remainder equally.
fn resolve_variant_distributions(variants: &[VariantSchema]) -> Vec<f64> {
    let fixed_sum: f64 = variants.iter().filter_map(|v| v.distribution).sum();
    let n_free = variants.iter().filter(|v| v.distribution.is_none()).count();
    let free_share = if n_free > 0 { (1.0 - fixed_sum) / n_free as f64 } else { 0.0 };
    variants.iter().map(|v| v.distribution.unwrap_or(free_share)).collect()
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
fn merge_variant_fields(base: &Schema, variant_data: &Schema) -> Schema {
    let mut result = base.clone();
    for vfield in variant_data {
        if let Some(existing) = result.iter_mut().find(|f| f.name == vfield.name) {
            *existing = vfield.clone();
        } else {
            result.push(vfield.clone());
        }
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
        for f in &mut variant_fields { stamp_locale(f, loc); }
    }

    SyntheticDataset {
        name: format!("{}__v{}", base.name, variant_index),
        format: base.format.clone(),
        locale: effective_locale,
        rows: Some(rows),
        skip: base.skip,
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

        if sibling_set.contains(path) {
            track_shared(dataset, &mut shared_outputs, &mut seen_shared);
            continue;
        }

        // Variant expansion: replace this dataset with N concrete variants.
        // Each variant writes to the same output_file so WriteSharedOutput shuffles them.
        // Note: prefill refs into a variant parent are not wired in v1 — prefills require
        // a single stable batch to pull columns from; variants produce N separate batches.
        if !dataset.variants.is_empty() {
            let output_key = dataset.output_file.clone().unwrap_or_else(|| dataset.name.clone());
            let dists = resolve_variant_distributions(&dataset.variants);
            let row_counts_v = distribute_rows(row_counts[path], &dists);

            for (i, (variant, &variant_rows)) in dataset.variants.iter().zip(row_counts_v.iter()).enumerate() {
                let virtual_path = variant_key(path, i);
                let concrete = expand_variant_dataset(dataset, variant, i, variant_rows, &output_key);

                if let Some(siblings) = sibling_groups.get(path) {
                    // Each sibling accumulates rows from N variant groups; ensure it has an
                    // output_file so WriteSharedOutput fires once for the combined output.
                    let adjusted_siblings: Vec<Sibling> = siblings.iter().map(|sib| {
                        let mut s = sib.clone();
                        if s.dataset.output_file.is_none() {
                            s.dataset.output_file = Some(sib.dataset.name.clone());
                        }
                        s
                    }).collect();
                    let segments = plan_segments(variant_rows, &adjusted_siblings, max_siblings)?;
                    for sib in &adjusted_siblings {
                        track_shared(&sib.dataset, &mut shared_outputs, &mut seen_shared);
                    }
                    track_shared(&concrete, &mut shared_outputs, &mut seen_shared);
                    let rich = has_rich_list(&concrete);
                    steps.push(ExecutionStep::GenerateSiblingGroup {
                        parent_path: virtual_path.clone(),
                        parent: concrete.clone(),
                        segments,
                        siblings: adjusted_siblings,
                        skip_parent_emit: rich,
                    });
                    if rich { emit_rich_list_steps(&concrete, &virtual_path, &mut steps); }
                } else {
                    track_shared(&concrete, &mut shared_outputs, &mut seen_shared);
                    let rich = has_rich_list(&concrete);
                    steps.push(ExecutionStep::GenerateDataset {
                        path: virtual_path.clone(),
                        dataset: concrete.clone(),
                        rows: variant_rows,
                        prefills: vec![],
                        skip_emit: rich,
                    });
                    if rich { emit_rich_list_steps(&concrete, &virtual_path, &mut steps); }
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
            let rich = has_rich_list(dataset);
            steps.push(ExecutionStep::GenerateSiblingGroup {
                parent_path: path.clone(),
                parent: dataset.clone(),
                segments,
                siblings: siblings.clone(),
                skip_parent_emit: rich,
            });
            if rich {
                emit_rich_list_steps(dataset, path, &mut steps);
            }
            continue;
        }

        track_shared(dataset, &mut shared_outputs, &mut seen_shared);
        let rich = has_rich_list(dataset);
        steps.push(ExecutionStep::GenerateDataset {
            path: path.clone(),
            dataset: dataset.clone(),
            rows: row_counts[path],
            prefills: compute_prefills(path, datasets, &sibling_set),
            skip_emit: rich,
        });
        if rich {
            emit_rich_list_steps(dataset, path, &mut steps);
        }
    }

    for (output_file, format) in shared_outputs {
        steps.push(ExecutionStep::WriteSharedOutput { output_file, format });
    }

    Ok(ExecutionPlan { steps })
}

fn has_rich_list(dataset: &SyntheticDataset) -> bool {
    dataset.data.iter().any(|f| f.content.as_deref().is_some_and(|c| !c.includes.is_empty()))
}

fn inner_flat_key(outer_path: &Path, field_name: &str) -> PathBuf {
    let stem = outer_path.file_stem().unwrap_or_default().to_string_lossy();
    outer_path.with_file_name(format!("{stem}___{field_name}___flat.internal"))
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
            dataset: dataset.clone(),
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
    for (sibling_path, dataset) in datasets {
        for include in &dataset.includes {
            let Some(d) = include.distribution else { continue };
            let Some(parent_path) = resolve_include(sibling_path, &include.file) else { continue };
            groups.entry(parent_path).or_default().push(Sibling {
                path: sibling_path.clone(),
                dataset: dataset.clone(),
                distribution: d,
                include_ref: include.reference.clone(),
            });
        }
    }
    groups
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
            skip: false,
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
