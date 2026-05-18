use anyhow::{anyhow, bail, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::constraints::validate_field_constraints;
use crate::expressions::extract_identifiers;
use crate::models::{resolve_include, split_ref, Field, FieldType, FieldVariant, Include, Schema, SyntheticDataset};

/// Validate all loaded datasets, returning any non-fatal warnings.
/// Hard errors (e.g. `rows` set alongside `distribution`) are returned as `Err`.
pub fn validate(datasets: &HashMap<PathBuf, SyntheticDataset>) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for (path, dataset) in datasets {
        validate_dataset(path, dataset, datasets, &mut warnings)?;
    }
    Ok(warnings)
}

fn validate_dataset(
    path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    // Rule 0: variant distribution consistency.
    if !dataset.variants.is_empty() {
        let fixed_sum: f64 = dataset.variants.iter().filter_map(|v| v.distribution).sum();
        let n_free = dataset.variants.iter().filter(|v| v.distribution.is_none()).count();
        if fixed_sum > 1.0 + 1e-9 {
            bail!(
                "dataset '{}': variant distributions sum to {:.4} which exceeds 1.0",
                dataset.name, fixed_sum
            );
        }
        if n_free == 0 && (fixed_sum - 1.0).abs() > 1e-9 {
            bail!(
                "dataset '{}': all variant distributions are explicit but sum to {:.4}, not 1.0",
                dataset.name, fixed_sum
            );
        }
        if dataset.variants.len() == 1 {
            warnings.push(format!(
                "warning: dataset '{}' has only one variant — this is equivalent to plain `data`",
                dataset.name
            ));
        }
    }

    // Rule 1: explicit rows is incompatible with distribution includes.
    if dataset.rows.is_some() && dataset.includes.iter().any(|i| i.distribution.is_some()) {
        bail!(
            "dataset '{}': `rows` cannot be set when any include specifies a `distribution` \
             — the row count is derived from the distribution percentage and the included \
             dataset's size. Remove the `rows` field.",
            dataset.name
        );
    }

    // Rule 2: root datasets (no includes) must declare an explicit row count.
    if dataset.rows.is_none() && dataset.includes.is_empty() {
        warnings.push(format!(
            "warning: dataset '{}' has no includes and no explicit `rows` \
             — defaulting to 100 rows",
            dataset.name
        ));
    }

    // Rule 3: multiple includes with mismatched expected row counts.
    if dataset.includes.len() > 1 {
        if let Some(warning) = check_row_count_mismatch(path, dataset, all) {
            warnings.push(warning);
        }
    }

    // Rule 3: field-level structural constraints (type/ref/schema/content consistency).
    for field in &dataset.data {
        if field.name.is_empty() {
            bail!("dataset '{}': a field is missing a `name`", dataset.name);
        }
        let field_path = format!("{}.{}", dataset.name, field.name);
        validate_field(&field_path, field, warnings)?;

        // Rich list content needs full dataset context — handle separately.
        if let Some(content) = &field.content {
            if !content.includes.is_empty() {
                let content_path = format!("{field_path}[]");
                validate_rich_content(&content_path, &content.includes, &content.item.fields, path, dataset, all, warnings)?;
            }
        }
    }

    // Rule 4: every field ref points to a real include and a real field.
    validate_dataset_refs(path, dataset, all)?;

    // Rule 5: expression variables only reference fields defined above them (YAML order).
    validate_expression_order(dataset)?;

    Ok(())
}

fn validate_field(path: &str, field: &Field, warnings: &mut Vec<String>) -> Result<()> {
    // Expression fields are fully self-contained: no type, ref, or generation constraints.
    if field.expression.is_some() {
        if field.field_type.is_some() {
            bail!("field '{path}': `expression` cannot be combined with `type`");
        }
        if field.ref_field.is_some() {
            bail!("field '{path}': `expression` cannot be combined with `ref`");
        }
        let has_range = field.range.as_ref().map(|r| r.min.is_some() || r.max.is_some()).unwrap_or(false);
        if field.generator.is_some() || has_range {
            bail!("field '{path}': `expression` cannot be combined with `generator` or `range`");
        }
        if field.value.is_some() {
            bail!("field '{path}': `expression` cannot be combined with `value`");
        }
        if !field.fields.is_empty() || field.content.is_some() {
            bail!("field '{path}': `expression` cannot be combined with `fields` or `content`");
        }
        return Ok(());
    }

    // Structural bans that apply when `ref` is set.
    // `type` is un-mergeable (the ref target owns the type), so it stays banned.
    // `fields` and `content` are structural, not constraints, so also banned.
    // Constraint fields (generator, min, max, value) are allowed — they specialise
    // the referenced field and are merged with its constraints during the rewrite step.
    if field.ref_field.is_some() {
        if field.field_type.is_some() {
            bail!(
                "field '{path}': `type` cannot be set alongside `ref` \
                 — the type is inherited from the referenced field"
            );
        }
        if !field.fields.is_empty() {
            bail!("field '{path}': `ref` and `fields` cannot both be set");
        }
        if field.content.is_some() {
            bail!("field '{path}': `ref` and `content` cannot both be set");
        }
    }

    // Constraint internal-consistency checks — apply regardless of whether `ref` is set.
    validate_field_constraints(path, field)?;
    let range_min = field.range.as_ref().and_then(|r| r.min);
    let range_max = field.range.as_ref().and_then(|r| r.max);

    if field.locale.is_some() {
        match &field.generator {
            None => bail!(
                "field '{path}': `locale` requires `generator` to be set"
            ),
            Some(g) if !g.supports_locale() => bail!(
                "field '{path}': generator `{g}` does not support locale selection"
            ),
            Some(_) => {}
        }
    }

    // Type-dependent checks require a known type — deferred to the rewrite step for ref fields.
    if field.ref_field.is_some() {
        return Ok(());
    }

    let field_type = match &field.field_type {
        Some(t) => t,
        None => bail!("field '{path}': must have `type`, `ref`, or `expression`"),
    };

    // Variant field: validate its choices and distribution, then stop.
    if *field_type == FieldType::Variant {
        if field.variants.is_empty() {
            bail!("field '{path}': `type: variant` requires a non-empty `variants` list");
        }
        for (i, choice) in field.variants.iter().enumerate() {
            validate_field_variant(&format!("{path}.variants[{i}]"), choice, warnings)?;
        }
        let fixed_sum: f64 = field.variants.iter().filter_map(|v| v.distribution).sum();
        let n_free = field.variants.iter().filter(|v| v.distribution.is_none()).count();
        if fixed_sum > 1.0 + 1e-9 {
            bail!(
                "field '{path}': variant distributions sum to {:.4} which exceeds 1.0",
                fixed_sum
            );
        }
        if n_free == 0 && (fixed_sum - 1.0).abs() > 1e-9 {
            bail!(
                "field '{path}': all variant distributions are explicit but sum to {:.4}, not 1.0",
                fixed_sum
            );
        }
        if field.variants.len() == 1 {
            warnings.push(format!(
                "warning: field '{path}' has only one variant choice — this is equivalent to a plain field"
            ));
        }
        return Ok(());
    }

    if !field.fields.is_empty() && *field_type != FieldType::Object {
        bail!(
            "field '{path}': `fields` is only valid on `object` type fields, \
             but this field has type `{field_type}`"
        );
    }

    if field.content.is_some() && *field_type != FieldType::List {
        bail!(
            "field '{path}': `content` is only valid on `list` type fields, \
             but this field has type `{field_type}`"
        );
    }

    if range_min.is_some() || range_max.is_some() {
        if *field_type != FieldType::Number {
            bail!(
                "field '{path}': `range` is only valid on `number` type fields, \
                 but this field has type `{field_type}`"
            );
        }
    }

    if let Some(g) = &field.generator {
        if !g.valid_for(field_type) {
            bail!(
                "field '{path}': generator `{g}` is not valid for type `{field_type}`"
            );
        }
    }

    if *field_type == FieldType::Object {
        if field.fields.is_empty() {
            warnings.push(format!(
                "warning: field '{path}' is `object` type but has no `fields` \
                 — will generate empty objects"
            ));
        } else {
            for sub in &field.fields {
                if sub.name.is_empty() {
                    bail!("field '{path}': a nested field is missing a `name`");
                }
                validate_field(&format!("{path}.{}", sub.name), sub, warnings)?;
            }
        }
    }

    if *field_type == FieldType::List {
        match field.content.as_deref() {
            None => warnings.push(format!(
                "warning: field '{path}' is `list` type but has no `content` \
                 — will generate empty lists"
            )),
            Some(c) if c.includes.is_empty() => {
                validate_field(&format!("{path}[]"), &c.item, warnings)?;
            }
            Some(_) => {
                // Rich list — validated separately in validate_dataset (requires full context).
            }
        }
    }

    Ok(())
}

/// Validate all fields inside a `content: {includes: [...], data: {...}}` block.
///
/// Two ref scopes apply:
/// - **Include-scoped** (`ref: include_ref.field`): dot-prefixed with the content include ref;
///   the include must exist in `rich_includes` and the target field in the included dataset.
/// - **Outer-scoped** (`ref: field`): no dot (or dot not matching any include ref); the field
///   must exist in the enclosing `dataset`. An explicit `type:` must be set (auto-inference
///   from the outer field is not yet supported).
fn validate_rich_content(
    path: &str,
    rich_includes: &[Include],
    data: &Schema,
    dataset_path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    _warnings: &mut Vec<String>,
) -> Result<()> {
    for field in data {
        if field.name.is_empty() {
            bail!("rich list content at '{path}': a field is missing a `name`");
        }
        let fpath = format!("{path}.{}", field.name);

        if field.expression.is_some() {
            bail!("field '{fpath}': `expression` is not supported inside rich list content");
        }

        if let Some(ref ref_str) = field.ref_field {
            // Determine scope: include-scoped (dot matches a content include) or outer-scoped.
            let include_scoped = split_ref(ref_str)
                .and_then(|(ref_part, _)| rich_includes.iter().find(|i| i.reference == ref_part));

            if let Some(include) = include_scoped {
                // Include-scoped ref — type must not be set (inherited from include target).
                if field.field_type.is_some() {
                    bail!(
                        "field '{fpath}': `type` cannot be set alongside an include-scoped `ref` \
                         — the type is inherited from the referenced field"
                    );
                }
                let (_, target_name) = split_ref(ref_str).unwrap();
                let inc_path = resolve_include(dataset_path, &include.file).ok_or_else(|| {
                    anyhow!("field '{fpath}': cannot resolve include file '{}'", include.file)
                })?;
                let included = all.get(&inc_path).ok_or_else(|| {
                    anyhow!("field '{fpath}': included dataset '{}' not loaded", include.file)
                })?;
                if !included.data.iter().any(|f| f.name == target_name) {
                    bail!(
                        "field '{fpath}': ref '{}' — field '{}' does not exist in '{}'",
                        ref_str, target_name, include.file
                    );
                }
            } else {
                // Outer-scoped ref — type must be set explicitly.
                if field.field_type.is_none() {
                    bail!(
                        "field '{fpath}': outer-scoped `ref: {ref_str}` requires an explicit `type` \
                         — type cannot be inferred from the enclosing dataset automatically"
                    );
                }
                // Validate that the outer field actually exists in the enclosing dataset.
                if !dataset.data.iter().any(|f| f.name == ref_str.as_str()) {
                    bail!(
                        "field '{fpath}': outer-scoped ref '{ref_str}' — \
                         field '{ref_str}' does not exist in dataset '{}'",
                        dataset.name
                    );
                }
            }
        } else {
            // Plain field — must have a type.
            if field.field_type.is_none() {
                bail!("field '{fpath}': must have `type`, `ref`, or `expression`");
            }
        }

        // Basic constraint checks.
        validate_field_constraints(&fpath, field)?;
        if field.locale.is_some() {
            match &field.generator {
                None => bail!("field '{fpath}': `locale` requires `generator` to be set"),
                Some(g) if !g.supports_locale() => bail!(
                    "field '{fpath}': generator `{g}` does not support locale selection"
                ),
                Some(_) => {}
            }
        }
        if let (Some(g), Some(ft)) = (&field.generator, &field.field_type) {
            if !g.valid_for(ft) {
                bail!("field '{fpath}': generator `{g}` is not valid for type `{ft}`");
            }
        }
    }
    Ok(())
}

fn validate_field_variant(path: &str, choice: &FieldVariant, warnings: &mut Vec<String>) -> Result<()> {
    if let Some(ft) = &choice.field_type {
        if *ft == FieldType::Variant {
            bail!("field variant '{path}': nested `type: variant` is not supported");
        }
    }

    if choice.value.is_some() {
        if choice.generator.is_some() {
            bail!("field variant '{path}': `value` and `generator` cannot both be set");
        }
        let has_range = choice.range.as_ref().map_or(false, |r| r.min.is_some() || r.max.is_some());
        if has_range {
            bail!("field variant '{path}': `value` and `range` cannot both be set");
        }
    }

    if let Some(r) = &choice.range {
        if let (Some(lo), Some(hi)) = (r.min, r.max) {
            if lo > hi {
                bail!("field variant '{path}': range.min ({lo}) must be ≤ range.max ({hi})");
            }
        }
    }

    if choice.locale.is_some() {
        match &choice.generator {
            None => bail!("field variant '{path}': `locale` requires `generator` to be set"),
            Some(g) if !g.supports_locale() => bail!(
                "field variant '{path}': generator `{g}` does not support locale selection"
            ),
            Some(_) => {}
        }
    }

    if let (Some(g), Some(ft)) = (&choice.generator, &choice.field_type) {
        if !g.valid_for(ft) {
            bail!("field variant '{path}': generator `{g}` is not valid for type `{ft}`");
        }
    }

    // Must be able to determine the concrete type at expansion time.
    let can_infer_type = choice.field_type.is_some()
        || choice.range.as_ref().map_or(false, |r| r.min.is_some() || r.max.is_some())
        || choice.value.as_ref().map_or(false, |v| v.is_string() || v.is_number() || v.is_bool());
    if !can_infer_type {
        bail!(
            "field variant '{path}': cannot determine type — set `type`, `value`, or `range`"
        );
    }

    let _ = warnings; // reserved for future use
    Ok(())
}

fn validate_dataset_refs(
    path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
) -> Result<()> {
    for field in &dataset.data {
        if let Some(ref ref_str) = field.ref_field {
            validate_ref_target(path, dataset, all, &field.name, ref_str)?;
        }
    }
    Ok(())
}

fn validate_ref_target(
    path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    field_name: &str,
    ref_str: &str,
) -> Result<()> {
    let (include_ref, target_field) = split_ref(ref_str).ok_or_else(|| {
        anyhow!(
            "field '{}.{}': ref '{}' must be in the form 'include_ref.field_name'",
            dataset.name, field_name, ref_str
        )
    })?;

    let include = dataset
        .includes
        .iter()
        .find(|i| i.reference == include_ref)
        .ok_or_else(|| {
            anyhow!(
                "field '{}.{}': ref '{}' — no include with ref '{}' in this dataset",
                dataset.name, field_name, ref_str, include_ref
            )
        })?;

    let include_path = resolve_include(path, &include.file).ok_or_else(|| {
        anyhow!(
            "field '{}.{}': ref '{}' — cannot resolve include file '{}'",
            dataset.name, field_name, ref_str, include.file
        )
    })?;

    let included = all.get(&include_path).ok_or_else(|| {
        anyhow!(
            "field '{}.{}': ref '{}' — included dataset not loaded",
            dataset.name, field_name, ref_str
        )
    })?;

    if !included.data.iter().any(|f| f.name == target_field) {
        bail!(
            "field '{}.{}': ref '{}' — field '{}' does not exist in '{}'",
            dataset.name, field_name, ref_str, target_field, include.file
        );
    }

    Ok(())
}

/// Check that every variable in an expression field refers to a field defined
/// above it in the YAML (evaluation order). Only tokens that match a known field
/// name are checked; SQL keywords and function names are passed through to DataFusion.
fn validate_expression_order(dataset: &SyntheticDataset) -> Result<()> {
    let all_names: HashSet<&str> = dataset.data.iter().map(|f| f.name.as_str()).collect();
    let mut available: HashSet<&str> = HashSet::new();

    for field in &dataset.data {
        if let Some(ref expr) = field.expression {
            for ident in extract_identifiers(expr) {
                if all_names.contains(ident) && !available.contains(ident) {
                    bail!(
                        "field '{}.{}': expression references '{}' which must be defined above it",
                        dataset.name,
                        field.name,
                        ident
                    );
                }
            }
        }
        available.insert(field.name.as_str());
    }
    Ok(())
}

fn check_row_count_mismatch(
    path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
) -> Option<String> {
    let parent_rows = dataset.rows.unwrap_or(100);

    let counts: Vec<(String, usize)> = dataset
        .includes
        .iter()
        .filter_map(|inc| {
            let expected = if let Some(dist) = inc.distribution {
                (parent_rows as f64 * dist).round() as usize
            } else {
                // No distribution: the include contributes all its rows; warn if they differ.
                let canonical = resolve_include(path, &inc.file)?;
                all.get(&canonical)?.rows.unwrap_or(parent_rows)
            };
            Some((inc.file.clone(), expected))
        })
        .collect();

    if counts.len() < 2 {
        return None;
    }

    let min = counts.iter().map(|(_, n)| *n).min().unwrap();
    let max = counts.iter().map(|(_, n)| *n).max().unwrap();

    if min == max {
        return None;
    }

    let detail: Vec<String> = counts
        .iter()
        .map(|(file, n)| format!("  {file}: {n} row(s)"))
        .collect();

    Some(format!(
        "warning: dataset '{}' includes datasets with mismatched expected row counts:\n{}\n  \
         {} row(s) will be used; {} excess row(s) discarded — \
         output may lack referential integrity between included datasets.",
        dataset.name,
        detail.join("\n"),
        min,
        max - min,
    ))
}