//! Ref resolution and locale application. `resolve_refs` pushes field types and merged
//! constraints down the lattice toward child/ref targets; `apply_global_locales` stamps
//! locale onto locale-aware fields across all datasets.
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::constraints::FieldConstraints;
use crate::constraints::Merge;
use crate::models::{
    Field, Include, Locale, RefsSpec, SyntheticDataset, resolve_include, split_ref,
};

const MAX_REF_CHAIN_DEPTH: usize = 32;

/// Expand `include.fields` / `links[i].fields` wildcards into injected ref fields.
///
/// For each `Include` (driver or list link) whose `fields` list is non-empty, looks up the
/// target dataset and generates `Field { name, refs: "<ref>.<name>" }` entries for every
/// matched field that does not already appear in the target list. Injected fields are
/// prepended so that user-declared fields (which come later) take precedence on name
/// collisions during ref resolution.
///
/// Call this after `expand_field_variants` and before `resolve_refs`.
pub fn expand_include_fields(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
) -> Result<HashMap<PathBuf, SyntheticDataset>> {
    let mut result = datasets.clone();
    for (path, dataset) in datasets {
        // Driver include expansion.
        if let Some(inc) = &dataset.include
            && !inc.fields.is_empty()
            && let Some(target_path) = resolve_include(path, &inc.file)
            && let Some(target) = datasets.get(&target_path)
        {
            let existing: HashSet<&str> = dataset.data.iter().map(|f| f.name.as_str()).collect();
            let exclude: HashSet<&str> = inc
                .exclude
                .iter()
                .flat_map(|v| v.iter())
                .map(|s| s.as_str())
                .collect();
            let injected = expand_field_patterns(
                &inc.fields,
                &exclude,
                &target.data,
                &inc.reference,
                &existing,
            );
            let out = result.get_mut(path).unwrap();
            let mut new_data = injected;
            new_data.extend(std::mem::take(&mut out.data));
            out.data = new_data;
        }
        // List link expansion: inject into the matching content.item.fields.
        for link in &dataset.links {
            let out = result.get_mut(path).unwrap();
            for field in &mut out.data {
                let Some(ref mut content) = field.content else {
                    continue;
                };
                let Some(ref from_ref) = content.from else {
                    continue;
                };
                if *from_ref != link.reference {
                    continue;
                }
                // `project` injection: if project is set and no fields yet, inject a single ref.
                if let Some(ref proj) = content.project.clone() {
                    if content.item.fields.is_empty()
                        && let Some((_, field_part)) = split_ref(proj)
                    {
                        content.item.fields = vec![make_ref_field(field_part, &link.reference)];
                    }
                    continue; // project and fields are mutually exclusive; skip wildcard expansion
                }
                if link.fields.is_empty() {
                    continue;
                }
                let Some(target_path) = resolve_include(path, &link.file) else {
                    continue;
                };
                let Some(target) = datasets.get(&target_path) else {
                    continue;
                };
                let existing: HashSet<&str> = content
                    .item
                    .fields
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect();
                let exclude: HashSet<&str> = link
                    .exclude
                    .iter()
                    .flat_map(|v| v.iter())
                    .map(|s| s.as_str())
                    .collect();
                let injected = expand_field_patterns(
                    &link.fields,
                    &exclude,
                    &target.data,
                    &link.reference,
                    &existing,
                );
                let mut new_fields = injected;
                new_fields.extend(std::mem::take(&mut content.item.fields));
                content.item.fields = new_fields;
            }
        }
    }
    Ok(result)
}

fn expand_field_patterns(
    patterns: &[String],
    exclude: &HashSet<&str>,
    target_data: &[Field],
    ref_prefix: &str,
    existing: &HashSet<&str>,
) -> Vec<Field> {
    let mut out: Vec<Field> = Vec::new();
    for pattern in patterns {
        if pattern == "*" {
            for tf in target_data {
                let name = tf.name.as_str();
                // Never propagate imported (tainted) columns via wildcard expansion.
                // Children-by-inclusion may not ref imported fields; silently skipping
                // here means `fields: ["*"]` only copies synthetic fields, which is
                // the correct behaviour. Explicit named imports of tainted columns
                // are caught by the validator.
                if tf.imported_taint {
                    continue;
                }
                if !exclude.contains(name)
                    && !existing.contains(name)
                    && !out.iter().any(|f| f.name == tf.name)
                {
                    out.push(make_ref_field(&tf.name, ref_prefix));
                }
            }
        } else {
            let name = pattern.as_str();
            if !exclude.contains(name)
                && !existing.contains(name)
                && !out.iter().any(|f| f.name == name)
                && target_data.iter().any(|f| f.name == *pattern)
            {
                out.push(make_ref_field(pattern, ref_prefix));
            }
        }
    }
    out
}

fn make_ref_field(name: &str, ref_prefix: &str) -> Field {
    Field {
        name: name.to_string(),
        refs: Some(RefsSpec::Single(format!("{ref_prefix}.{name}"))),
        ..Default::default()
    }
}

/// For every field carrying a `ref`, copy `field_type`, `schema`, and `content`
/// from the referenced canonical field. The `ref_field` string is preserved so
/// the executor can use it to wire up pre-filled column data.
///
/// Call this after `validate` has confirmed that all refs are structurally sound.
pub fn resolve_refs(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
) -> Result<HashMap<PathBuf, SyntheticDataset>> {
    let mut resolved = datasets.clone();

    for (path, dataset) in datasets {
        let new_fields: Vec<Field> = dataset
            .data
            .iter()
            .map(|field| {
                let mut out = if let Some(ref_str) = field.simple_ref() {
                    resolve_field(path, dataset, datasets, field, ref_str).map(
                        |mut resolved_field| {
                            // Keep refs so the executor can locate pre-filled columns.
                            resolved_field.refs = field.refs.clone();
                            resolved_field
                        },
                    )?
                } else {
                    field.clone()
                };

                // Resolve linked-dataset refs inside list-link content.
                if let Some(content) = &field.content
                    && let Some(ref from_ref) = content.from
                {
                    let from_ref = from_ref.clone();
                    if let Some(link) = dataset.links.iter().find(|l| l.reference == from_ref) {
                        let link = link.clone();
                        let new_content_fields: Vec<Field> = content
                            .item
                            .fields
                            .iter()
                            .map(|cf| resolve_list_link_content_field(path, datasets, cf, &link))
                            .collect::<Result<_>>()?;
                        if let Some(ref mut c) = out.content {
                            c.item.fields = new_content_fields;
                        }
                    }
                }

                Ok(out)
            })
            .collect::<Result<_>>()?;

        resolved.get_mut(path).unwrap().data = new_fields;
    }

    Ok(resolved)
}

fn resolve_field(
    path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    field: &Field,
    ref_str: &str,
) -> Result<Field> {
    let field_name = &field.name;
    let (include_ref, target_name) = split_ref(ref_str).ok_or_else(|| {
        anyhow!(
            "field '{}.{}': malformed ref '{}' — expected 'include_ref.field_name'",
            dataset.name,
            field_name,
            ref_str
        )
    })?;

    let include = dataset
        .include
        .iter()
        .chain(dataset.links.iter())
        .find(|i| i.reference == include_ref)
        .ok_or_else(|| {
            anyhow!(
                "field '{}.{}': ref '{}' — no include or link with ref '{}'",
                dataset.name,
                field_name,
                ref_str,
                include_ref
            )
        })?;

    let include_path = resolve_include(path, &include.file).ok_or_else(|| {
        anyhow!(
            "field '{}.{}': ref '{}' — cannot resolve '{}'",
            dataset.name,
            field_name,
            ref_str,
            include.file
        )
    })?;

    let included_ds = all.get(&include_path).ok_or_else(|| {
        anyhow!(
            "field '{}.{}': ref '{}' — included dataset not loaded",
            dataset.name,
            field_name,
            ref_str
        )
    })?;

    let target = included_ds
        .data
        .iter()
        .find(|f| f.name == target_name)
        .ok_or_else(|| {
            anyhow!(
                "field '{}.{}': ref '{}' — target field not found",
                dataset.name,
                field_name,
                ref_str
            )
        })?;

    // Follow chains: if the target is itself a ref, traverse to the base field for type info.
    let base = resolve_to_base(target, included_ds, &include_path, all, 0).with_context(|| {
        format!(
            "field '{}.{}': ref '{}' — could not resolve ref chain",
            dataset.name, field_name, ref_str
        )
    })?;

    if base.expression.is_some() {
        bail!(
            "field '{}.{}': ref '{}' points to an expression field — \
             expression field types are inferred at runtime and cannot be referenced",
            dataset.name,
            field_name,
            ref_str
        );
    }

    // Merge local constraints with those of the BASE (ultimate non-ref) target.
    let merged = FieldConstraints::from(field)
        .merge(&FieldConstraints::from(base))
        .ok_or_else(|| {
            anyhow!(
                "field '{}.{}': ref '{}' — local constraints conflict with target field '{}'",
                dataset.name,
                field_name,
                ref_str,
                target_name,
            )
        })?;

    let mut resolved = merged_ref_field(field_name.to_string(), field, base, merged);
    resolved.expression = field.expression.clone();
    Ok(resolved)
}

/// Build a ref-resolved field — the single home for *what ref resolution produces*.
///
/// Ref resolution: inherit the **base** (ultimate non-ref target) field's `type` and nested
/// schema, take the **merged** value-source (`generator`/`range`/`value`/`one_of` intersected
/// with the local field's), and **propagate the variant carrier** — except a **case-3** field
/// (`ref` + its own `variants`) keeps its own constraint-bearing cases for the planner to lower,
/// rather than inheriting the parent's carrier (VAR-SPECIALIZE S4a). Per-case `constrain_cases`
/// (S5) and the `constraint_bearing` marker ride along.
///
/// Callers set the one field that differs by context: `expression` (top-level refs) or `refs`
/// (list-link content refs).
fn merged_ref_field(name: String, local: &Field, base: &Field, merged: FieldConstraints) -> Field {
    let range = merged.to_range();
    Field {
        name,
        field_type: base.field_type.clone(),
        generator: merged.generator,
        range,
        value: merged.value,
        one_of: merged.one_of,
        variants: if local.variants.is_empty() {
            base.variants.clone()
        } else {
            local.variants.clone()
        },
        constrain_cases: local.constrain_cases.clone(),
        constraint_bearing: local.constraint_bearing,
        fields: base.fields.clone(),
        content: base.content.clone(),
        hidden: local.hidden,
        ..Default::default()
    }
}

/// Walk a ref chain to its base (non-ref) field, returning a reference to it.
/// Errors if the chain exceeds 32 hops (cycle guard) or any step cannot be resolved.
fn resolve_to_base<'a>(
    field: &'a Field,
    dataset: &'a SyntheticDataset,
    dataset_path: &Path,
    all: &'a HashMap<PathBuf, SyntheticDataset>,
    depth: usize,
) -> Result<&'a Field> {
    if depth > MAX_REF_CHAIN_DEPTH {
        bail!("ref chain exceeds maximum depth — check for a circular reference");
    }
    let Some(ref_str) = field.simple_ref() else {
        return Ok(field);
    };
    let (inc_ref, field_name) =
        split_ref(ref_str).ok_or_else(|| anyhow!("malformed ref '{}' in chain", ref_str))?;
    let include = dataset
        .include
        .iter()
        .chain(dataset.links.iter())
        .find(|i| i.reference == inc_ref)
        .ok_or_else(|| {
            anyhow!(
                "ref '{}' — include or link '{}' not found",
                ref_str,
                inc_ref
            )
        })?;
    let inc_path = resolve_include(dataset_path, &include.file)
        .ok_or_else(|| anyhow!("cannot resolve include '{}' in chain", include.file))?;
    let next_ds = all
        .get(&inc_path)
        .ok_or_else(|| anyhow!("included dataset '{}' not loaded", include.file))?;
    let next_field = next_ds
        .data
        .iter()
        .find(|f| f.name == field_name)
        .ok_or_else(|| anyhow!("field '{}' not found in '{}'", field_name, include.file))?;
    resolve_to_base(next_field, next_ds, &inc_path, all, depth + 1)
}

/// Resolve a single field inside a list-link content block.
///
/// - **Linked-dataset ref** (`ref: linked_ref.field`): copies `field_type` and nested schema
///   from the target field in the linked dataset, merging any local constraints.
/// - **Outer-scoped ref** (no dot or dot not matching any content link): left unchanged
///   — the field already carries an explicit `type` (validated) and the executor will stamp
///   the value from the outer row at generation time.
/// - **No ref**: left unchanged.
fn resolve_list_link_content_field(
    dataset_path: &Path,
    all: &HashMap<PathBuf, SyntheticDataset>,
    field: &Field,
    link: &Include,
) -> Result<Field> {
    let Some(ref_str) = field.simple_ref() else {
        return Ok(field.clone());
    };

    // Determine scope by matching the dot-prefix against the link ref.
    // No dot, or prefix not matching the link → outer-scoped: leave as-is.
    let Some((ref_part, target_name)) = split_ref(ref_str) else {
        return Ok(field.clone());
    };
    if link.reference != ref_part {
        return Ok(field.clone());
    }
    let include = link;

    let include_path = resolve_include(dataset_path, &include.file).ok_or_else(|| {
        anyhow!(
            "list-link content field '{}': cannot resolve linked file '{}'",
            field.name,
            include.file
        )
    })?;

    let target = all
        .get(&include_path)
        .and_then(|ds| ds.data.iter().find(|f| f.name == target_name))
        .ok_or_else(|| {
            anyhow!(
                "list-link content field '{}': target field '{}' not found in '{}'",
                field.name,
                target_name,
                include.file
            )
        })?;

    let merged = FieldConstraints::from(field)
        .merge(&FieldConstraints::from(target))
        .ok_or_else(|| {
            anyhow!(
                "list-link content field '{}': local constraints conflict with target '{}'",
                field.name,
                target_name
            )
        })?;

    let mut resolved = merged_ref_field(field.name.clone(), field, target, merged);
    resolved.refs = field.refs.clone();
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Global locale propagation
// ---------------------------------------------------------------------------

/// Apply each dataset's `locale` to every field whose generator supports locale
/// selection but has no explicit `locale` of its own. Field-level locale wins.
///
/// Call this after `resolve_refs` so that ref-resolved fields are included.
pub fn apply_global_locales(datasets: &mut HashMap<PathBuf, SyntheticDataset>) {
    for dataset in datasets.values_mut() {
        let Some(global) = dataset.locale.clone() else {
            continue;
        };
        for field in &mut dataset.data {
            stamp_locale(field, &global);
        }
    }
}

/// Apply a locale to all fields in a schema slice (same rules as `apply_global_locales`).
pub fn apply_locale_to_schema(fields: &mut [Field], locale: &Locale) {
    for field in fields {
        stamp_locale(field, locale);
    }
}

pub(crate) fn stamp_locale(field: &mut Field, global: &Locale) {
    if field.locale.is_none()
        && let Some(ref g) = field.generator
        && g.supports_locale()
    {
        field.locale = Some(global.clone());
    }
    for sub in &mut field.fields {
        stamp_locale(sub, global);
    }
    if let Some(ref mut content) = field.content {
        stamp_locale(&mut content.item, global);
    }
}
