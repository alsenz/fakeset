//! Schema validation: structural rules, ref validity, constraint consistency, and
//! cardinality feasibility checks. Called after loading YAML and before plan building.
use anyhow::{anyhow, bail, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::constraints::validate_field_constraints;
use crate::expressions::extract_identifiers;
use crate::models::{resolve_include, split_ref, CountSpec, Field, FieldType, FieldVariant, Generator, Include, Locale, RefBinding, Reducer, Schema, SyntheticDataset};

/// Validate all loaded datasets, returning any non-fatal warnings.
/// Hard errors (e.g. `rows` set alongside `ratio`) are returned as `Err`.
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
        let fixed_sum: f64 = dataset.variants.iter().filter_map(|v| v.ratio).sum();
        let n_free = dataset.variants.iter().filter(|v| v.ratio.is_none()).count();
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

    // Rule 1: explicit rows is incompatible with ratio includes.
    if dataset.rows.is_some() && dataset.include.as_ref().map_or(false, |i| i.ratio.is_some()) {
        bail!(
            "dataset '{}': `rows` cannot be set when `include` specifies a `ratio` \
             — the row count is derived from the ratio and the included \
             dataset's size. Remove the `rows` field.",
            dataset.name
        );
    }

    // Rule 2: root datasets (no include) must declare an explicit row count.
    if dataset.rows.is_none() && dataset.include.is_none() {
        warnings.push(format!(
            "warning: dataset '{}' has no includes and no explicit `rows` \
             — defaulting to 100 rows",
            dataset.name
        ));
    }

    // Rule: validate links.
    let group_refs = collect_group_refs(&dataset.data);
    for link in &dataset.links {
        if resolve_include(path, &link.file).is_none() {
            bail!(
                "dataset '{}': linked file not found: '{}'",
                dataset.name, link.file
            );
        }
        if !group_refs.contains(&link.reference) {
            // Junction link: cardinality is not meaningful (one linked-dataset row sampled per junction row).
            if link.cardinality.is_some() {
                bail!(
                    "dataset '{}': junction link '{}' must not set `cardinality` — \
                     junction links sample exactly one linked-dataset row per junction row",
                    dataset.name, link.reference
                );
            }
        }
        if let Some(r) = link.reinforcement {
            if r < 0.0 || (r > 0.0 && r < 1.0) {
                bail!(
                    "dataset '{}': link '{}': `reinforcement` must be 0 (without-replacement), \
                     1 (uniform), or > 1 (clumping); got {r}",
                    dataset.name, link.reference
                );
            }
        }
        if let Some(ov) = link.overlap {
            if ov < 0.0 || (ov > 0.0 && ov < 1.0) {
                bail!(
                    "dataset '{}': link '{}': `overlap` must be 0 (non-overlapping partitions), \
                     1 (default, unrestricted), or > 1 (preferential popularity); got {ov}",
                    dataset.name, link.reference
                );
            }
            if ov > 1.0 && link.reinforcement == Some(0.0) {
                bail!(
                    "dataset '{}': link '{}': `overlap > 1` and `reinforcement: 0` are \
                     incompatible — power-law weighting requires with-replacement sampling",
                    dataset.name, link.reference
                );
            }
        }
    }
    // group ref must match a link.
    for from_ref in &group_refs {
        if !dataset.links.iter().any(|l| &l.reference == from_ref) {
            bail!(
                "dataset '{}': `content.from: {}` does not match any entry in `links`",
                dataset.name, from_ref
            );
        }
    }
    // Two content.from fields may not reference the same link.
    let mut seen_from: HashSet<&str> = HashSet::new();
    for fr in &group_refs {
        if !seen_from.insert(fr.as_str()) {
            bail!(
                "dataset '{}': two or more list fields share `content.from: {}` — each link may be referenced by at most one list field",
                dataset.name, fr
            );
        }
    }

    // Rule: include.fields / exclude consistency.
    let check_fields_exclude = |inc: &Include, kind: &str| -> Result<()> {
        if inc.exclude.is_some() && inc.fields.is_empty() {
            bail!(
                "dataset '{}': {} '{}': `exclude` is only valid when `fields` is also set",
                dataset.name, kind, inc.reference
            );
        }
        Ok(())
    };
    if let Some(inc) = &dataset.include {
        check_fields_exclude(inc, "include")?;
        if !inc.fields.is_empty() {
            validate_include_fields(path, dataset, inc, all, warnings);
        }
    }
    for link in &dataset.links {
        check_fields_exclude(link, "link")?;
        if !link.fields.is_empty() {
            validate_include_fields(path, dataset, link, all, warnings);
        }
    }

    // Rule: reinforcement and overlap are links: only.
    if let Some(inc) = &dataset.include {
        if inc.reinforcement.is_some() {
            bail!(
                "dataset '{}': `include.reinforcement` is not valid — \
                 `reinforcement` only applies to `links:` entries",
                dataset.name
            );
        }
        if inc.overlap.is_some() {
            bail!(
                "dataset '{}': `include.overlap` is not valid — \
                 `overlap` only applies to `links:` entries",
                dataset.name
            );
        }
    }

    // Rule: top-level include cardinality constraints.
    if let Some(inc) = &dataset.include {
        if let Some(card) = &inc.cardinality {
            if dataset.rows.is_some() {
                bail!(
                    "dataset '{}': `rows` cannot be set when `include.cardinality` is present",
                    dataset.name
                );
            }
            validate_cardinality(card, &format!("dataset '{}'", dataset.name))?;
        }
    }

    // Rule 3: field-level structural constraints (type/ref/schema/content consistency).
    for field in &dataset.data {
        if field.name.is_empty() {
            bail!("dataset '{}': a field is missing a `name`", dataset.name);
        }
        let field_path = format!("{}.{}", dataset.name, field.name);
        validate_field(&field_path, field, warnings)?;

        // Link-content fields need full dataset context — handle separately.
        if let Some(content) = &field.content {
            if let Some(ref from_ref) = content.from {
                if let Some(link) = dataset.links.iter().find(|l| &l.reference == from_ref) {
                    let content_path = format!("{field_path}[]");
                    validate_project(content, link, &content_path, path, all)?;
                    validate_list_link_content(&content_path, link, &content.item.fields, path, dataset, all, warnings)?;
                }
            }
        }
    }

    // Rule 4: every field ref points to a real include and a real field.
    validate_dataset_refs(path, dataset, all)?;

    // Rule 5: expression variables only reference fields defined above them (YAML order).
    validate_expression_order(dataset)?;

    // Rule 6: collect reducer bindings are structurally valid.
    validate_collect_bindings(path, dataset, all)?;

    Ok(())
}

fn validate_locale_generator(path: &str, locale: Option<&Locale>, generator: Option<&Generator>) -> Result<()> {
    if locale.is_some() {
        match generator {
            None => bail!("field '{path}': `locale` requires `generator` to be set"),
            Some(g) if !g.supports_locale() => bail!(
                "field '{path}': generator `{g}` does not support locale selection"
            ),
            Some(_) => {}
        }
    }
    Ok(())
}

fn validate_field(path: &str, field: &Field, warnings: &mut Vec<String>) -> Result<()> {
    // Expression fields are fully self-contained: no type, ref, or generation constraints.
    if field.expression.is_some() {
        if field.field_type.is_some() {
            bail!("field '{path}': `expression` cannot be combined with `type`");
        }
        if field.simple_ref().is_some() {
            bail!("field '{path}': `expression` cannot be combined with `ref`");
        }
        if field.generator.is_some() || field.range.as_ref().is_some_and(|r| r.min.is_some() || r.max.is_some()) {
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
    if field.simple_ref().is_some() {
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

    validate_locale_generator(path, field.locale.as_ref(), field.generator.as_ref())?;

    // Type-dependent checks require a known type — deferred to the rewrite step for ref fields.
    if field.simple_ref().is_some() {
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
        let fixed_sum: f64 = field.variants.iter().filter_map(|v| v.ratio).sum();
        let n_free = field.variants.iter().filter(|v| v.ratio.is_none()).count();
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
            Some(c) if c.from.is_none() => {
                validate_field(&format!("{path}[]"), &c.item, warnings)?;
            }
            Some(_) => {
                // Rich list — count must not be set on the field; cardinality belongs on the link.
                if field.count.is_some() {
                    bail!(
                        "field '{path}': `count` cannot be set on a list-link field — \
                         use `cardinality` on the link in `links`"
                    );
                }
                // Remaining validation requires full dataset context; handled in validate_dataset.
            }
        }
    }

    // `default` value must be type-compatible with the declared field type.
    if let Some(default_val) = &field.default {
        let (compatible, expected) = match field_type {
            FieldType::Number   => (default_val.is_number(), "a number"),
            FieldType::String | FieldType::Date | FieldType::DateTime
                                => (default_val.is_string(), "a string"),
            FieldType::Boolean  => (default_val.is_bool(), "a boolean"),
            FieldType::List     => (default_val.is_sequence(), "a sequence (e.g. `default: []`)"),
            FieldType::Object   => (default_val.is_mapping(), "a mapping"),
            FieldType::Variant  => (true, ""),
        };
        if !compatible {
            bail!(
                "field '{path}': `default` value is incompatible with `type: {field_type}` — \
                 expected {expected}"
            );
        }
    }

    Ok(())
}

/// Validate all fields inside a `content: {group: <ref>, ...}` block.
///
/// Two ref scopes apply:
/// - **Pool-scoped** (`ref: link_ref.field`): dot-prefixed with the link ref; the target
///   field must exist in the linked dataset.
/// - **Outer-scoped** (`ref: field`): no dot (or dot not matching the link ref); the field
///   must exist in the enclosing `dataset`. An explicit `type:` must be set (auto-inference
///   from the outer field is not yet supported).
fn validate_list_link_content(
    path: &str,
    link: &Include,
    data: &Schema,
    dataset_path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    _warnings: &mut Vec<String>,
) -> Result<()> {
    for field in data {
        if field.name.is_empty() {
            bail!("list-link content at '{path}': a field is missing a `name`");
        }
        let fpath = format!("{path}.{}", field.name);

        if field.expression.is_some() {
            bail!("field '{fpath}': `expression` is not supported inside list-link content");
        }

        if let Some(ref_str) = field.simple_ref() {
            // Determine scope: linked-scoped (dot matches the link ref) or outer-scoped.
            let linked_scoped = split_ref(ref_str)
                .and_then(|(ref_part, _)| if link.reference == ref_part { Some(link) } else { None });

            if let Some(inc) = linked_scoped {
                // Linked-scoped ref — type must not be set (inherited from link target).
                if field.field_type.is_some() {
                    bail!(
                        "field '{fpath}': `type` cannot be set alongside a linked-scoped `ref` \
                         — the type is inherited from the referenced field"
                    );
                }
                let (_, target_name) = split_ref(ref_str).unwrap();
                let inc_path = resolve_include(dataset_path, &inc.file).ok_or_else(|| {
                    anyhow!("field '{fpath}': cannot resolve link file '{}'", inc.file)
                })?;
                let included = all.get(&inc_path).ok_or_else(|| {
                    anyhow!("field '{fpath}': linked dataset '{}' not loaded", inc.file)
                })?;
                if !included.data.iter().any(|f| f.name == target_name) {
                    bail!(
                        "field '{fpath}': ref '{}' — field '{}' does not exist in '{}'",
                        ref_str, target_name, inc.file
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
                if !dataset.data.iter().any(|f| f.name == ref_str) {
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
        validate_locale_generator(&fpath, field.locale.as_ref(), field.generator.as_ref())?;
        if let (Some(g), Some(ft)) = (&field.generator, &field.field_type) {
            if !g.valid_for(ft) {
                bail!("field '{fpath}': generator `{g}` is not valid for type `{ft}`");
            }
        }
    }
    Ok(())
}

fn validate_field_variant(path: &str, choice: &FieldVariant, _warnings: &mut Vec<String>) -> Result<()> {
    if let Some(ft) = &choice.field_type {
        if *ft == FieldType::Variant {
            bail!("field variant '{path}': nested `type: variant` is not supported");
        }
    }

    if choice.value.is_some() {
        if choice.generator.is_some() {
            bail!("field variant '{path}': `value` and `generator` cannot both be set");
        }
        if choice.range.as_ref().is_some_and(|r| r.min.is_some() || r.max.is_some()) {
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

    validate_locale_generator(path, choice.locale.as_ref(), choice.generator.as_ref())?;

    if let (Some(g), Some(ft)) = (&choice.generator, &choice.field_type) {
        if !g.valid_for(ft) {
            bail!("field variant '{path}': generator `{g}` is not valid for type `{ft}`");
        }
    }

    // Must be able to determine the concrete type at expansion time.
    let can_infer_type = choice.field_type.is_some()
        || choice.range.as_ref().is_some_and(|r| r.min.is_some() || r.max.is_some())
        || choice.value.as_ref().is_some_and(|v| v.is_string() || v.is_number() || v.is_bool());
    if !can_infer_type {
        bail!(
            "field variant '{path}': cannot determine type — set `type`, `value`, or `range`"
        );
    }

    Ok(())
}

fn validate_cardinality(card: &CountSpec, ctx: &str) -> Result<()> {
    match card {
        CountSpec::Fixed(n) if *n < 1 => {
            bail!("{ctx}: `cardinality` must be at least 1, got {n}");
        }
        CountSpec::Uniform { min, .. } if *min < 1 => {
            bail!("{ctx}: `cardinality.min` must be at least 1, got {min}");
        }
        CountSpec::Uniform { min, max } if min > max => {
            bail!("{ctx}: `cardinality.min` ({min}) must be ≤ `cardinality.max` ({max})");
        }
        _ => {}
    }
    Ok(())
}

fn validate_collect_bindings(
    dataset_path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
) -> Result<()> {
    // Top-level fields (Case 1 — junction dataset, activated in Stage 4).
    for field in &dataset.data {
        let field_path = format!("{}.{}", dataset.name, field.name);
        for binding in field.collect_bindings() {
            validate_single_collect_bind(dataset_path, dataset, all, &field_path, binding)?;
        }

        // Case 2 — fields inside list-link content blocks.
        if let Some(content) = &field.content {
            if content.from.is_some() {
                for cf in &content.item.fields {
                    let cf_path = format!("{field_path}[].{}", cf.name);
                    for binding in cf.collect_bindings() {
                        validate_single_collect_bind(dataset_path, dataset, all, &cf_path, binding)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_single_collect_bind(
    dataset_path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    field_path: &str,
    binding: &RefBinding,
) -> Result<()> {
    let bind = binding.bind.as_deref().ok_or_else(|| {
        anyhow!("field '{field_path}': collect binding has no `bind` target")
    })?;

    let (linked_ref, linked_field_name) = split_ref(bind).ok_or_else(|| {
        anyhow!(
            "field '{field_path}': collect `bind: {bind}` must be in the form \
             'linked_ref.field_name'"
        )
    })?;

    let link = dataset
        .links
        .iter()
        .find(|l| l.reference == linked_ref)
        .ok_or_else(|| {
            anyhow!(
                "field '{field_path}': collect `bind: {bind}` — \
                 no link with ref '{linked_ref}' in this dataset"
            )
        })?;

    let linked_path = resolve_include(dataset_path, &link.file).ok_or_else(|| {
        anyhow!(
            "field '{field_path}': collect `bind: {bind}` — \
             cannot resolve linked file '{}'",
            link.file
        )
    })?;

    let linked_ds = all.get(&linked_path).ok_or_else(|| {
        anyhow!("field '{field_path}': collect `bind: {bind}` — linked dataset not loaded")
    })?;

    let linked_field = linked_ds
        .data
        .iter()
        .find(|f| f.name == linked_field_name)
        .ok_or_else(|| {
            anyhow!(
                "field '{field_path}': collect `bind: {bind}` — \
                 field '{linked_field_name}' not found in '{}'",
                link.file
            )
        })?;

    // Type compatibility: each reducer requires a specific linked field type.
    let reducer = binding.reducer.as_ref().unwrap_or(&Reducer::Collect);
    match reducer {
        Reducer::Collect => {
            if !matches!(linked_field.field_type, Some(FieldType::List)) {
                bail!(
                    "field '{field_path}': collect `bind: {bind}` — \
                     target field '{linked_field_name}' in '{}' must be `type: list` \
                     (collect accumulates values into a list; got type: {:?})",
                    link.file,
                    linked_field.field_type.as_ref().map(|t| t.to_string()).unwrap_or_default()
                );
            }
        }
        Reducer::Sum => {
            if !matches!(linked_field.field_type, Some(FieldType::Number)) {
                bail!(
                    "field '{field_path}': sum `bind: {bind}` — \
                     target field '{linked_field_name}' in '{}' must be `type: number` \
                     (sum requires a numeric target; got type: {:?})",
                    link.file,
                    linked_field.field_type.as_ref().map(|t| t.to_string()).unwrap_or_default()
                );
            }
        }
        Reducer::Max | Reducer::Min | Reducer::TakeOne => {
            // No type restriction — max/min/take_one work on any orderable type.
        }
    }

    if linked_field.default.is_none() {
        bail!(
            "field '{field_path}': {:?} `bind: {bind}` — \
             target field '{linked_field_name}' in '{}' must declare `default:` \
             (the default is used when no atoms map to that linked row)",
            reducer,
            link.file
        );
    }

    Ok(())
}

fn validate_dataset_refs(
    path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
) -> Result<()> {
    for field in &dataset.data {
        if let Some(ref_str) = field.simple_ref() {
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
        .include
        .iter()
        .chain(dataset.links.iter())
        .find(|i| i.reference == include_ref)
        .ok_or_else(|| {
            anyhow!(
                "field '{}.{}': ref '{}' — no include or link with ref '{}' in this dataset",
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

/// Collect all `content.from` ref strings found recursively in a field list.
fn collect_group_refs(fields: &[Field]) -> Vec<String> {
    let mut refs = Vec::new();
    for field in fields {
        if let Some(content) = &field.content {
            if let Some(ref g) = content.from {
                refs.push(g.clone());
            }
            refs.extend(collect_group_refs(&content.item.fields));
        }
        refs.extend(collect_group_refs(&field.fields));
    }
    refs
}

/// Warn about unknown exclude entries and empty expansions for an `include.fields` declaration.
fn validate_include_fields(
    path: &Path,
    dataset: &SyntheticDataset,
    include: &Include,
    all: &HashMap<PathBuf, SyntheticDataset>,
    warnings: &mut Vec<String>,
) {
    let Some(target_path) = resolve_include(path, &include.file) else { return };
    let Some(target) = all.get(&target_path) else { return };
    let target_names: HashSet<&str> = target.data.iter().map(|f| f.name.as_str()).collect();
    let exclude_set: HashSet<&str> = include
        .exclude
        .iter()
        .flat_map(|v| v.iter())
        .map(|s| s.as_str())
        .collect();

    for ex in &exclude_set {
        if !target_names.contains(ex) {
            warnings.push(format!(
                "warning: dataset '{}': include '{}': `exclude` names field '{}' \
                 which does not exist in '{}'",
                dataset.name, include.reference, ex, include.file
            ));
        }
    }

    let would_expand = include.fields.iter().any(|p| {
        if p == "*" {
            target_names.iter().any(|n| !exclude_set.contains(n))
        } else {
            target_names.contains(p.as_str()) && !exclude_set.contains(p.as_str())
        }
    });
    if !would_expand {
        warnings.push(format!(
            "warning: dataset '{}': include '{}': `fields` expands to no fields \
             after applying `exclude`",
            dataset.name, include.reference
        ));
    }
}

/// Validate the `project:` directive on a link-content block.
///
/// - `project` and explicit `content.fields` are mutually exclusive.
/// - The ref part of `project` must match the link's reference.
/// - The field part must exist in the link's target dataset.
fn validate_project(
    content: &crate::models::ListContent,
    link: &Include,
    content_path: &str,
    dataset_path: &Path,
    all: &HashMap<PathBuf, SyntheticDataset>,
) -> Result<()> {
    let Some(ref proj) = content.project else { return Ok(()) };

    if !content.item.fields.is_empty() {
        bail!(
            "field '{}': `project` and `fields` are mutually exclusive — remove `fields` when using `project`",
            content_path
        );
    }

    let (ref_part, field_part) = split_ref(proj).ok_or_else(|| {
        anyhow!(
            "field '{}': `project: {}` — expected `<link_ref>.<field_name>` format",
            content_path, proj
        )
    })?;

    if ref_part != link.reference {
        bail!(
            "field '{}': `project: {}` — ref part '{}' does not match the link ref '{}'",
            content_path, proj, ref_part, link.reference
        );
    }

    let inc_path = resolve_include(dataset_path, &link.file).ok_or_else(|| {
        anyhow!(
            "field '{}': `project: {}` — cannot resolve link file '{}'",
            content_path, proj, link.file
        )
    })?;
    let linked = all.get(&inc_path).ok_or_else(|| {
        anyhow!(
            "field '{}': `project: {}` — linked dataset '{}' not loaded",
            content_path, proj, link.file
        )
    })?;

    if !linked.data.iter().any(|f| f.name == field_part) {
        bail!(
            "field '{}': `project: {}` — field '{}' does not exist in '{}'",
            content_path, proj, field_part, link.file
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
