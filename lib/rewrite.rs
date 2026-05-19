use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::constraints::Merge;
use crate::constraints::FieldConstraints;
use crate::models::{resolve_include, split_ref, Field, Include, Locale, Range, SyntheticDataset};

const MAX_REF_CHAIN_DEPTH: usize = 32;

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
                let mut out = if let Some(ref ref_str) = field.ref_field {
                    resolve_field(path, dataset, datasets, field, ref_str).map(
                        |mut resolved_field| {
                            // Keep ref_field so the executor can locate pre-filled columns.
                            resolved_field.ref_field = field.ref_field.clone();
                            resolved_field
                        },
                    )?
                } else {
                    field.clone()
                };

                // Resolve include-scoped refs inside nested include content.
                if let Some(content) = &field.content {
                    if !content.includes.is_empty() {
                        let content_includes = content.includes.clone();
                        let new_content_fields: Vec<Field> = content.item.fields.iter()
                            .map(|cf| resolve_nested_include_content_field(path, datasets, cf, &content_includes))
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
        .includes
        .iter()
        .find(|i| i.reference == include_ref)
        .ok_or_else(|| {
            anyhow!(
                "field '{}.{}': ref '{}' — no include with ref '{}'",
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
            dataset.name, field_name, ref_str
        )
    })?;

    let target = included_ds.data.iter()
        .find(|f| f.name == target_name)
        .ok_or_else(|| {
            anyhow!(
                "field '{}.{}': ref '{}' — target field not found",
                dataset.name, field_name, ref_str
            )
        })?;

    // Follow chains: if the target is itself a ref, traverse to the base field for type info.
    let base = resolve_to_base(target, included_ds, &include_path, all, 0)
        .with_context(|| {
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
    let merged = FieldConstraints::from(field).merge(&FieldConstraints::from(base)).ok_or_else(|| {
        anyhow!(
            "field '{}.{}': ref '{}' — local constraints conflict with target field '{}'",
            dataset.name,
            field_name,
            ref_str,
            target_name,
        )
    })?;

    Ok(Field {
        name: field_name.to_string(),
        field_type: base.field_type.clone(),
        generator: merged.generator,
        range: if merged.min.is_some() || merged.max.is_some() {
            Some(Range { min: merged.min, max: merged.max })
        } else {
            None
        },
        value: merged.value,
        fields: base.fields.clone(),
        content: base.content.clone(),
        expression: field.expression.clone(),
        hidden: field.hidden,
        ..Default::default()
    })
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
    let Some(ref ref_str) = field.ref_field else {
        return Ok(field);
    };
    let (inc_ref, field_name) = split_ref(ref_str).ok_or_else(|| {
        anyhow!("malformed ref '{}' in chain", ref_str)
    })?;
    let include = dataset.includes.iter()
        .find(|i| i.reference == inc_ref)
        .ok_or_else(|| anyhow!("ref '{}' — include '{}' not found", ref_str, inc_ref))?;
    let inc_path = resolve_include(dataset_path, &include.file)
        .ok_or_else(|| anyhow!("cannot resolve include '{}' in chain", include.file))?;
    let next_ds = all.get(&inc_path)
        .ok_or_else(|| anyhow!("included dataset '{}' not loaded", include.file))?;
    let next_field = next_ds.data.iter()
        .find(|f| f.name == field_name)
        .ok_or_else(|| anyhow!("field '{}' not found in '{}'", field_name, include.file))?;
    resolve_to_base(next_field, next_ds, &inc_path, all, depth + 1)
}

/// Resolve a single field inside a nested include content block.
///
/// - **Include-scoped ref** (`ref: include_ref.field`): copies `field_type` and nested schema
///   from the target field in the included dataset, merging any local constraints.
/// - **Outer-scoped ref** (no dot or dot not matching any content include): left unchanged
///   — the field already carries an explicit `type` (validated) and the executor will stamp
///   the value from the outer row at generation time.
/// - **No ref**: left unchanged.
fn resolve_nested_include_content_field(
    dataset_path: &Path,
    all: &HashMap<PathBuf, SyntheticDataset>,
    field: &Field,
    content_includes: &[Include],
) -> Result<Field> {
    let Some(ref ref_str) = field.ref_field else {
        return Ok(field.clone());
    };

    // Determine scope by matching the dot-prefix against content includes.
    // No dot, or prefix not matching any include → outer-scoped: leave as-is.
    let Some((ref_part, target_name)) = split_ref(ref_str) else {
        return Ok(field.clone());
    };
    let Some(include) = content_includes.iter().find(|i| i.reference == ref_part) else {
        return Ok(field.clone());
    };

    let include_path = resolve_include(dataset_path, &include.file).ok_or_else(|| {
        anyhow!(
            "nested include field '{}': cannot resolve include '{}'",
            field.name, include.file
        )
    })?;

    let target = all
        .get(&include_path)
        .and_then(|ds| ds.data.iter().find(|f| f.name == target_name))
        .ok_or_else(|| {
            anyhow!(
                "nested include field '{}': target field '{}' not found in '{}'",
                field.name, target_name, include.file
            )
        })?;

    let merged = FieldConstraints::from(field).merge(&FieldConstraints::from(target)).ok_or_else(|| {
        anyhow!(
            "nested include field '{}': local constraints conflict with target '{}'",
            field.name, target_name
        )
    })?;

    Ok(Field {
        name: field.name.clone(),
        field_type: target.field_type.clone(),
        generator: merged.generator,
        range: if merged.min.is_some() || merged.max.is_some() {
            Some(Range { min: merged.min, max: merged.max })
        } else {
            None
        },
        value: merged.value,
        fields: target.fields.clone(),
        content: target.content.clone(),
        ref_field: field.ref_field.clone(),
        hidden: field.hidden,
        ..Default::default()
    })
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
        let Some(global) = dataset.locale.clone() else { continue };
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
    if field.locale.is_none() {
        if let Some(ref g) = field.generator {
            if g.supports_locale() {
                field.locale = Some(global.clone());
            }
        }
    }
    for sub in &mut field.fields {
        stamp_locale(sub, global);
    }
    if let Some(ref mut content) = field.content {
        stamp_locale(&mut content.item, global);
    }
}