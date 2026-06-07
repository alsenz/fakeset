//! Schema validation: structural rules, ref validity, constraint consistency, and
//! cardinality feasibility checks. Called after loading YAML and before plan building.
use anyhow::{Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::constraints::validate_field_constraints;
use crate::expand_variants::is_heterogeneous;
use crate::expressions::extract_identifiers;
use crate::models::{
    Corruptions, CountSpec, DataQuality, Field, FieldType, FieldVariant, FlattenStrategy, Format,
    Generator, Include, Locale, Reducer, RefBinding, Schema, SyntheticDataset,
    discriminant_tag_column, resolve_include, split_ref,
};

/// Validate all loaded datasets, returning any non-fatal warnings.
/// Hard errors (e.g. `rows` set alongside `ratio`) are returned as `Err`.
pub fn validate(datasets: &HashMap<PathBuf, SyntheticDataset>) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for (path, dataset) in datasets {
        validate_dataset(path, dataset, datasets, &mut warnings)?;
    }
    check_import_taint(datasets)?;
    Ok(warnings)
}

fn validate_dataset(
    path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    // VAR-UNIFY U4: top-level dataset `variants:` is retired. It is no longer a field on
    // `SyntheticDataset`, so `#[serde(deny_unknown_fields)]` rejects the key at *load* time —
    // no validation rule is needed here.
    check_csv_heterogeneous_variant(dataset)?; // Rule 0b (VAR-1)
    check_row_count_rules(dataset, warnings)?; // Rules 1a/1b/2
    check_import_ring(dataset)?;
    check_links(path, dataset)?;
    check_include_options(path, dataset, all, warnings)?;
    check_fields(path, dataset, all, warnings)?; // Rule 3

    // Rule 3b (VAR-UNIFY): `flatten` placement + output name-collision checks.
    validate_flatten(&dataset.data, &dataset.format, "", warnings)?;
    // Rule 4: every field ref points to a real include and a real field.
    validate_dataset_refs(path, dataset, all)?;
    // Rule 5: expression variables only reference fields defined above them (YAML order).
    validate_expression_order(dataset)?;
    // Rule 6: collect reducer bindings are structurally valid.
    validate_collect_bindings(path, dataset, all)?;
    // Rule 7: data quality stanzas are structurally valid.
    check_data_quality_stanzas(dataset)?;

    Ok(())
}

/// Rule 0b (VAR-1): a heterogeneous variant lowers to a nested union column (emitted as a
/// nullable-superset struct). CSV is flat and cannot represent nested types — the same
/// limitation object fields have — so reject it here rather than failing at write time.
/// Struct-capable formats (parquet/json/jsonl) are fine.
fn check_csv_heterogeneous_variant(dataset: &SyntheticDataset) -> Result<()> {
    if matches!(dataset.format, Format::Csv)
        && let Some(field_path) = first_heterogeneous_variant(&dataset.data, "")
    {
        bail!(
            "dataset '{}': field '{field_path}' is a heterogeneous (multi-type) variant, \
             which becomes a nested union column that CSV cannot represent. Use `format: \
             parquet`, `json`, or `jsonl` for this dataset. See specs/VAR-1.md.",
            dataset.name
        );
    }
    Ok(())
}

/// Rules 1a/1b/2: an explicit `rows` count is incompatible with ratio includes and with
/// imports (the count is derived in both cases); a root dataset with no count warns.
fn check_row_count_rules(dataset: &SyntheticDataset, warnings: &mut Vec<String>) -> Result<()> {
    if dataset.rows.is_some() && dataset.include.as_ref().is_some_and(|i| i.ratio.is_some()) {
        bail!(
            "dataset '{}': `rows` cannot be set when `include` specifies a `ratio` \
             — the row count is derived from the ratio and the included \
             dataset's size. Remove the `rows` field.",
            dataset.name
        );
    }
    if dataset.rows.is_some() && dataset.import.is_some() {
        bail!(
            "dataset '{}': `rows` cannot be set when `import` is present \
             — row count is determined by the imported file. Remove the `rows` field.",
            dataset.name
        );
    }
    if dataset.rows.is_none() && dataset.include.is_none() && dataset.import.is_none() {
        warnings.push(format!(
            "warning: dataset '{}' has no includes and no explicit `rows` \
             — defaulting to 100 rows",
            dataset.name
        ));
    }
    Ok(())
}

/// Import ring bounds must lie in [0.0, 1.0) with `start < end`.
fn check_import_ring(dataset: &SyntheticDataset) -> Result<()> {
    let Some(ring) = dataset.import.as_ref().and_then(|s| s.ring.as_ref()) else {
        return Ok(());
    };
    if ring.start < 0.0 || ring.end > 1.0 {
        bail!(
            "dataset '{}': `import.ring` values must lie in [0.0, 1.0); got [{}, {})",
            dataset.name,
            ring.start,
            ring.end
        );
    }
    if ring.start >= ring.end {
        bail!(
            "dataset '{}': `import.ring.start` ({}) must be less than `import.ring.end` ({})",
            dataset.name,
            ring.start,
            ring.end
        );
    }
    Ok(())
}

/// Validate `links:`: target file existence, junction-link cardinality, reinforcement/overlap
/// ranges, and the `content.from` ↔ link correspondence (every group ref maps to exactly one link).
fn check_links(path: &Path, dataset: &SyntheticDataset) -> Result<()> {
    let group_refs = collect_group_refs(&dataset.data);
    for link in &dataset.links {
        if resolve_include(path, &link.file).is_none() {
            bail!(
                "dataset '{}': linked file not found: '{}'",
                dataset.name,
                link.file
            );
        }
        // Junction link: cardinality is not meaningful (one linked-dataset row per junction row).
        if !group_refs.contains(&link.reference) && link.cardinality.is_some() {
            bail!(
                "dataset '{}': junction link '{}' must not set `cardinality` — \
                 junction links sample exactly one linked-dataset row per junction row",
                dataset.name,
                link.reference
            );
        }
        if let Some(r) = link.reinforcement
            && (r < 0.0 || (r > 0.0 && r < 1.0))
        {
            bail!(
                "dataset '{}': link '{}': `reinforcement` must be 0 (without-replacement), \
                     1 (uniform), or > 1 (clumping); got {r}",
                dataset.name,
                link.reference
            );
        }
        if let Some(ov) = link.overlap {
            if ov < 0.0 || (ov > 0.0 && ov < 1.0) {
                bail!(
                    "dataset '{}': link '{}': `overlap` must be 0 (non-overlapping partitions), \
                     1 (default, unrestricted), or > 1 (preferential popularity); got {ov}",
                    dataset.name,
                    link.reference
                );
            }
            if ov > 1.0 && link.reinforcement == Some(0.0) {
                bail!(
                    "dataset '{}': link '{}': `overlap > 1` and `reinforcement: 0` are \
                     incompatible — power-law weighting requires with-replacement sampling",
                    dataset.name,
                    link.reference
                );
            }
        }
    }
    // group ref must match a link.
    for from_ref in &group_refs {
        if !dataset.links.iter().any(|l| &l.reference == from_ref) {
            bail!(
                "dataset '{}': `content.from: {}` does not match any entry in `links`",
                dataset.name,
                from_ref
            );
        }
    }
    // Two content.from fields may not reference the same link.
    let mut seen_from: HashSet<&str> = HashSet::new();
    for fr in &group_refs {
        if !seen_from.insert(fr.as_str()) {
            bail!(
                "dataset '{}': two or more list fields share `content.from: {}` — each link may be referenced by at most one list field",
                dataset.name,
                fr
            );
        }
    }
    Ok(())
}

/// Validate include/link options: `fields`/`exclude` consistency, `reinforcement`/`overlap`
/// being links-only, and top-level include `cardinality`.
fn check_include_options(
    path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let check_fields_exclude = |inc: &Include, kind: &str| -> Result<()> {
        if inc.exclude.is_some() && inc.fields.is_empty() {
            bail!(
                "dataset '{}': {} '{}': `exclude` is only valid when `fields` is also set",
                dataset.name,
                kind,
                inc.reference
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

    // reinforcement and overlap are links: only.
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

    // top-level include cardinality constraints.
    if let Some(inc) = &dataset.include
        && let Some(card) = &inc.cardinality
    {
        if dataset.rows.is_some() {
            bail!(
                "dataset '{}': `rows` cannot be set when `include.cardinality` is present",
                dataset.name
            );
        }
        validate_cardinality(card, &format!("dataset '{}'", dataset.name))?;
    }
    Ok(())
}

/// Rule 3: per-field structural validation (`validate_field`), plus list-link content fields
/// (which need full dataset context for ref scoping).
fn check_fields(
    path: &Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    for field in &dataset.data {
        if field.name.is_empty() {
            bail!("dataset '{}': a field is missing a `name`", dataset.name);
        }
        let field_path = format!("{}.{}", dataset.name, field.name);
        validate_field(&field_path, field, warnings)?;

        // Link-content fields need full dataset context — handle separately.
        if let Some(content) = &field.content
            && let Some(ref from_ref) = content.from
            && let Some(link) = dataset.links.iter().find(|l| &l.reference == from_ref)
        {
            let content_path = format!("{field_path}[]");
            validate_project(content, link, &content_path, path, all)?;
            validate_list_link_content(
                &content_path,
                link,
                &content.item.fields,
                path,
                dataset,
                all,
                warnings,
            )?;
        }
    }
    Ok(())
}

/// Rule 7: data quality stanzas (output-level and field-level) are structurally valid, and a
/// field-level stanza requires an output-level one.
fn check_data_quality_stanzas(dataset: &SyntheticDataset) -> Result<()> {
    let output_has_quality = dataset
        .resolved_outputs()
        .iter()
        .any(|o| o.quality.is_some());
    for out in &dataset.resolved_outputs() {
        if let Some(ref q) = out.quality {
            validate_data_quality(&dataset.name, &out.file, q, true)?;
        }
    }
    for field in &dataset.data {
        if let Some(ref q) = field.quality {
            let field_path = format!("{}.{}", dataset.name, field.name);
            validate_data_quality(&dataset.name, &field_path, q, false)?;
            if !output_has_quality {
                bail!(
                    "field '{}' has a `quality` stanza but the dataset output block has no `quality` stanza",
                    field_path
                );
            }
            if let Some(ref c) = q.corruptions {
                let ft = field.field_type.as_ref().unwrap_or(&FieldType::String);
                validate_corruption_modes(&field_path, c, ft)?;
            }
        }
    }
    Ok(())
}

fn validate_data_quality(
    dataset_name: &str,
    ctx: &str,
    q: &DataQuality,
    is_output_level: bool,
) -> Result<()> {
    let check_prob = |name: &str, val: f64| -> Result<()> {
        if !(0.0..=1.0).contains(&val) {
            bail!(
                "dataset '{}': quality field '{}' must be between 0.0 and 1.0, got {}",
                dataset_name,
                name,
                val
            );
        }
        Ok(())
    };

    if let Some(v) = q.nulls {
        check_prob("nulls", v)?;
    }
    if let Some(v) = q.default_rate {
        check_prob("default_rate", v)?;
    }

    if is_output_level {
        if let Some(v) = q.duplication {
            check_prob("duplication", v)?;
        }
        if let Some(v) = q.missing {
            check_prob("missing", v)?;
        }
        if q.default_values.is_some() {
            bail!(
                "dataset '{}' output '{}': `quality.default_values` is only valid on a field, not on the output block",
                dataset_name,
                ctx
            );
        }
        if q.defaults_mode.is_some() {
            bail!(
                "dataset '{}' output '{}': `quality.defaults_mode` is only valid on a field, not on the output block",
                dataset_name,
                ctx
            );
        }
    } else {
        if q.duplication.is_some() {
            bail!(
                "field '{}': `quality.duplication` is only valid on an output block, not on a field",
                ctx
            );
        }
        if q.missing.is_some() {
            bail!(
                "field '{}': `quality.missing` is only valid on an output block, not on a field",
                ctx
            );
        }
    }

    if let Some(ref c) = q.corruptions {
        if let Some(v) = c.character_deletion {
            check_prob("corruptions.character_deletion", v)?;
        }
        if let Some(v) = c.character_insertion {
            check_prob("corruptions.character_insertion", v)?;
        }
        if let Some(v) = c.truncation {
            check_prob("corruptions.truncation", v)?;
        }
        if let Some(v) = c.encoding {
            check_prob("corruptions.encoding", v)?;
        }
        if let Some(v) = c.noise {
            check_prob("corruptions.noise", v)?;
        }
        if let Some(v) = c.day_shift {
            check_prob("corruptions.day_shift", v)?;
        }
    }

    Ok(())
}

fn validate_corruption_modes(field_path: &str, c: &Corruptions, ft: &FieldType) -> Result<()> {
    let is_string = matches!(ft, FieldType::String);
    let is_number = matches!(ft, FieldType::Number);
    let is_temporal = matches!(ft, FieldType::Date | FieldType::DateTime);

    if !is_string {
        if c.character_deletion.is_some() {
            bail!(
                "field '{field_path}': `corruptions.character_deletion` is not applicable to {ft} fields"
            );
        }
        if c.character_insertion.is_some() {
            bail!(
                "field '{field_path}': `corruptions.character_insertion` is not applicable to {ft} fields"
            );
        }
        if c.truncation.is_some() {
            bail!(
                "field '{field_path}': `corruptions.truncation` is not applicable to {ft} fields"
            );
        }
        if c.encoding.is_some() {
            bail!("field '{field_path}': `corruptions.encoding` is not applicable to {ft} fields");
        }
    }
    if !is_number && c.noise.is_some() {
        bail!("field '{field_path}': `corruptions.noise` is not applicable to {ft} fields");
    }
    if !is_temporal && c.day_shift.is_some() {
        bail!("field '{field_path}': `corruptions.day_shift` is not applicable to {ft} fields");
    }
    Ok(())
}

fn validate_date_bounds(path: &str, field: &Field) -> Result<()> {
    let ft = match &field.field_type {
        Some(t) => t,
        None => return Ok(()),
    };

    if field.after.is_none() && field.before.is_none() {
        return Ok(());
    }

    match ft {
        FieldType::Date => {
            let parse = |s: &str| {
                chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|e| anyhow!("field '{path}': `after`/`before` for date field must be YYYY-MM-DD, got '{s}': {e}"))
            };
            let after = field.after.as_deref().map(parse).transpose()?;
            let before = field.before.as_deref().map(parse).transpose()?;
            if let (Some(a), Some(b)) = (after, before)
                && a >= b
            {
                bail!("field '{path}': `after` ({a}) must be before `before` ({b})");
            }
        }
        FieldType::DateTime => {
            let parse = |s: &str| {
                chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|e| anyhow!("field '{path}': `after`/`before` for date_time field must be RFC 3339, got '{s}': {e}"))
            };
            let after = field.after.as_deref().map(parse).transpose()?;
            let before = field.before.as_deref().map(parse).transpose()?;
            if let (Some(a), Some(b)) = (after, before)
                && a >= b
            {
                bail!("field '{path}': `after` must be before `before`");
            }
        }
        _ => {
            bail!(
                "field '{path}': `after`/`before` is only valid on `date` or `date_time` fields, \
                 but this field has type `{ft}`"
            );
        }
    }
    Ok(())
}

fn validate_args(path: &str, field: &Field) -> Result<()> {
    let Some(ref args) = field.args else {
        return Ok(());
    };

    if matches!(&field.field_type, Some(FieldType::Boolean)) {
        return validate_boolean_args(path, args);
    }

    // For all other types, args require a generator.
    let g = field.generator.as_ref().ok_or_else(|| {
        anyhow!(
            "field '{path}': `args` requires a `generator` to be set (or `type: boolean` for `ratio`)"
        )
    })?;

    let valid_keys = g
        .valid_args()
        .ok_or_else(|| anyhow!("field '{path}': generator `{g}` does not accept `args`"))?;

    for key in args.keys() {
        if !valid_keys.contains(&key.as_str()) {
            bail!(
                "field '{path}': unknown arg '{key}' for generator `{g}` — valid keys: {}",
                valid_keys.join(", ")
            );
        }
    }

    validate_generator_args(path, g, args)
}

/// Boolean fields accept only `ratio` (an integer 0–100, no generator required).
fn validate_boolean_args(path: &str, args: &HashMap<String, serde_yaml::Value>) -> Result<()> {
    for key in args.keys() {
        if key != "ratio" {
            bail!("field '{path}': unknown arg '{key}' for boolean field — valid key: `ratio`");
        }
    }
    if let Some(v) = args.get("ratio") {
        let ratio = v
            .as_u64()
            .ok_or_else(|| anyhow!("field '{path}': `args.ratio` must be an integer 0–100"))?;
        if ratio > 100 {
            bail!("field '{path}': `args.ratio` must be between 0 and 100, got {ratio}");
        }
    }
    Ok(())
}

/// Type-check the values of generator-specific args (keys already validated against `valid_args`).
fn validate_generator_args(
    path: &str,
    g: &Generator,
    args: &HashMap<String, serde_yaml::Value>,
) -> Result<()> {
    match g {
        Generator::Sentence
        | Generator::Paragraph
        | Generator::Words
        | Generator::Sentences
        | Generator::Paragraphs
        | Generator::Password => {
            for key in ["min", "max"] {
                if let Some(v) = args.get(key) {
                    v.as_u64().ok_or_else(|| {
                        anyhow!("field '{path}': `args.{key}` must be a non-negative integer")
                    })?;
                }
            }
            if let (Some(min_v), Some(max_v)) = (args.get("min"), args.get("max")) {
                let min = min_v.as_u64().unwrap();
                let max = max_v.as_u64().unwrap();
                if min >= max {
                    bail!(
                        "field '{path}': `args.min` ({min}) must be less than `args.max` ({max})"
                    );
                }
            }
        }
        Generator::Geohash => {
            if let Some(v) = args.get("precision") {
                let p = v.as_u64().ok_or_else(|| {
                    anyhow!("field '{path}': `args.precision` must be an integer 1–12")
                })?;
                if !(1..=12).contains(&p) {
                    bail!("field '{path}': `args.precision` must be between 1 and 12, got {p}");
                }
            }
        }
        Generator::NumberWithFormat => match args.get("format") {
            Some(v) if v.as_str().is_none() => {
                bail!("field '{path}': `args.format` must be a string")
            }
            None => {
                bail!("field '{path}': generator `number_with_format` requires `args.format`")
            }
            Some(_) => {}
        },
        _ => {}
    }
    Ok(())
}

fn validate_locale_generator(
    path: &str,
    locale: Option<&Locale>,
    generator: Option<&Generator>,
) -> Result<()> {
    if locale.is_some() {
        match generator {
            None => bail!("field '{path}': `locale` requires `generator` to be set"),
            Some(g) if !g.supports_locale() => {
                bail!("field '{path}': generator `{g}` does not support locale selection")
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Dotted path of the first heterogeneous-variant field (recursing into objects), if any.
/// Used to gate CSV output, which cannot represent the resulting nested union column.
fn first_heterogeneous_variant(fields: &[Field], prefix: &str) -> Option<String> {
    for f in fields {
        let path = if prefix.is_empty() {
            f.name.clone()
        } else {
            format!("{prefix}.{}", f.name)
        };
        match f.field_type {
            Some(FieldType::Variant) if is_heterogeneous(&f.variants) => return Some(path),
            Some(FieldType::Object) => {
                if let Some(p) = first_heterogeneous_variant(&f.fields, &path) {
                    return Some(p);
                }
            }
            _ => {}
        }
    }
    None
}

/// VAR-UNIFY: validate `flatten` field placement and output name-collisions.
///
/// `flatten` elides a field's nesting at write time, pulling its sub-columns up to the
/// parent level (one level; on a variant, distributed to object cases). The pulled-up names
/// must therefore not collide with sibling fields (any format), nor — for the Parquet/CSV
/// nullable-superset — with each other across union cases. JSON/JSONL emit per-row keys
/// (only the active case fires), so cross-case collisions are harmless there. Runs before
/// `expand_field_variants`, so a variant is still `FieldType::Variant` with `variants:` here.
fn validate_flatten(
    fields: &[Field],
    format: &Format,
    prefix: &str,
    _warnings: &mut Vec<String>,
) -> Result<()> {
    let superset = !matches!(format, Format::Json | Format::Jsonl);
    for field in fields {
        let path = if prefix.is_empty() {
            field.name.clone()
        } else {
            format!("{prefix}.{}", field.name)
        };

        // `flatten_strategy` is only meaningful on a flatten variant field.
        if field.flatten_strategy.is_some()
            && !(field.flatten && matches!(field.field_type, Some(FieldType::Variant)))
        {
            bail!("field '{path}': `flatten_strategy` is only valid on a `flatten` variant field");
        }

        if field.flatten {
            // Placement: only object/variant carry a nesting to elide.
            match field.field_type {
                Some(FieldType::Object) | Some(FieldType::Variant) => {}
                _ => {
                    bail!("field '{path}': `flatten` is only valid on `object` or `variant` fields")
                }
            }
            // Name: flatten changes output shape only, never identity — refs resolve against
            // the named, nested model before output, so the field must stay addressable.
            if field.name.is_empty() {
                bail!("field '{path}': a `flatten` field must have a `name`");
            }
            // Scope (VAR-UNIFY U2): only top-level flatten is implemented at write time.
            // A nested flatten (inside an object) would need to pull up into the containing
            // struct — gated here so it errors rather than silently emitting nested.
            if !prefix.is_empty() {
                bail!(
                    "field '{path}': `flatten` is only supported on a top-level field for now \
                     (nested flatten not yet implemented)"
                );
            }

            // Effective strategy: JSON/JSONL ignore it (per-row keys, raw names); flat
            // columnar formats apply the declared strategy (default superset).
            let strategy = if superset {
                field.flatten_strategy.unwrap_or_default()
            } else {
                FlattenStrategy::Superset
            };

            let sibling_names: HashSet<&str> = fields
                .iter()
                .filter(|f| !std::ptr::eq(*f, field))
                .map(|f| f.name.as_str())
                .collect();
            let groups = flatten_pullup_groups(field, strategy);

            // Sibling collision (any format), on the effective (possibly prefixed) names.
            for name in groups.iter().flatten() {
                if sibling_names.contains(name.as_str()) {
                    bail!(
                        "field '{path}': flattening pulls up `{name}`, which collides with a \
                         sibling field of the same name — rename one of them."
                    );
                }
            }

            // Cross-case collision (flat columnar formats only). `prefixed` namespaces the
            // pulled-up names by case, so it naturally avoids this; `superset`/`discriminant`
            // share the column, so a shared name is a real collision.
            if superset && groups.len() > 1 {
                let mut seen: HashSet<&str> = HashSet::new();
                for name in groups.iter().flatten() {
                    if !seen.insert(name.as_str()) {
                        bail!(
                            "field '{path}': flatten pulls up `{name}` from more than one variant \
                             case into a `{format}` superset, which collides. Use \
                             `flatten_strategy: prefixed`, `format: json`/`jsonl`, or rename the \
                             colliding case fields."
                        );
                    }
                }
            }

            // The discriminant tag column must not collide either.
            if superset && strategy == FlattenStrategy::Discriminant {
                let tag = discriminant_tag_column(&field.name);
                if sibling_names.contains(tag.as_str())
                    || groups.iter().flatten().any(|n| n == &tag)
                {
                    bail!(
                        "field '{path}': the `discriminant` tag column `{tag}` collides with an \
                         existing field — rename the colliding field."
                    );
                }
            }
        }

        if matches!(field.field_type, Some(FieldType::Object)) {
            validate_flatten(&field.fields, format, &path, _warnings)?;
        }
    }
    Ok(())
}

/// The names a `flatten` field pulls up to the parent under `strategy`, grouped by source:
/// an object field yields one group (its sub-field names); a variant yields one group per
/// case (an object case → its field names, prefixed by the case label under `Prefixed`; a
/// scalar case → its single case label).
fn flatten_pullup_groups(field: &Field, strategy: FlattenStrategy) -> Vec<Vec<String>> {
    match field.field_type {
        Some(FieldType::Object) => vec![field.fields.iter().map(|f| f.name.clone()).collect()],
        Some(FieldType::Variant) => field
            .variants
            .iter()
            .enumerate()
            .map(|(i, case)| {
                let label = case
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}_{i}", field.name));
                if case.fields.is_empty() {
                    vec![label]
                } else if strategy == FlattenStrategy::Prefixed {
                    case.fields
                        .iter()
                        .map(|f| format!("{label}_{}", f.name))
                        .collect()
                } else {
                    case.fields.iter().map(|f| f.name.clone()).collect()
                }
            })
            .collect(),
        _ => vec![],
    }
}

/// VAR-SPECIALIZE case 3: validate the `variants:` on a `ref` field — a value-distribution
/// specialising the inherited (scalar) field per case. Cases must be value-source-only
/// (no object/structural keys); the distribution obeys the usual sum rules.
fn validate_case3_variants(
    path: &str,
    variants: &[FieldVariant],
    warnings: &mut Vec<String>,
) -> Result<()> {
    for (i, c) in variants.iter().enumerate() {
        let cp = format!("{path}.variants[{i}]");
        if !c.fields.is_empty() || matches!(c.field_type, Some(FieldType::Object)) {
            bail!(
                "field '{cp}': a `ref` + `variants` (per-case specialisation) case cannot be an \
                 object case — it specialises a scalar inherited field; use `value` / \
                 `generator` / `range` only"
            );
        }
        let has_range = c
            .range
            .as_ref()
            .is_some_and(|r| r.min.is_some() || r.max.is_some());
        if c.value.is_none() && c.generator.is_none() && !has_range {
            bail!(
                "field '{cp}': a `ref` + `variants` case must specialise something \
                 (`value`, `generator`, or `range`)"
            );
        }
    }
    let fixed_sum: f64 = variants.iter().filter_map(|v| v.ratio).sum();
    let n_free = variants.iter().filter(|v| v.ratio.is_none()).count();
    if fixed_sum > 1.0 + 1e-9 {
        bail!("field '{path}': variant distributions sum to {fixed_sum:.4} which exceeds 1.0");
    }
    if n_free == 0 && (fixed_sum - 1.0).abs() > 1e-9 {
        bail!(
            "field '{path}': all variant distributions are explicit but sum to {fixed_sum:.4}, not 1.0"
        );
    }
    if variants.len() == 1 {
        warnings.push(format!(
            "warning: field '{path}' has only one variant case — this is equivalent to a plain ref specialisation"
        ));
    }
    Ok(())
}

fn validate_field(path: &str, field: &Field, warnings: &mut Vec<String>) -> Result<()> {
    // Tainted fields come from an imported Arrow schema, not from YAML.
    // Their type is already resolved by load_import_headers; YAML structural rules don't apply.
    if field.imported_taint {
        return Ok(());
    }
    // Expression fields are fully self-contained: no type, ref, or generation constraints.
    if field.expression.is_some() {
        return validate_expression_field(path, field);
    }

    validate_ref_structural_bans(path, field)?;

    // Constraint internal-consistency checks — apply regardless of whether `ref` is set.
    validate_field_constraints(path, field)?;
    validate_specialization_keys(path, field)?;

    validate_locale_generator(path, field.locale.as_ref(), field.generator.as_ref())?;
    validate_args(path, field)?;

    // Type-dependent checks require a known type — deferred to the rewrite step for ref fields.
    if field.simple_ref().is_some() {
        // Case 3 (VAR-SPECIALIZE): a `ref` field may carry `variants:` — a value-distribution
        // that specialises the inherited field per case (each case lowers to a ref-bound,
        // value-pinned case-member entering segmentation). Validate the cases here.
        if !field.variants.is_empty() {
            validate_case3_variants(path, &field.variants, warnings)?;
        }
        return Ok(());
    }

    let field_type = match &field.field_type {
        Some(t) => t,
        None => bail!("field '{path}': must have `type`, `ref`, or `expression`"),
    };

    validate_date_bounds(path, field)?;

    // Variant field: validate its choices and distribution, then stop.
    if *field_type == FieldType::Variant {
        return validate_variant_field(path, field, warnings);
    }

    validate_typed_field(path, field, field_type, warnings)
}

/// An `expression` field carries no type/ref/generation: every other source is banned.
fn validate_expression_field(path: &str, field: &Field) -> Result<()> {
    if field.field_type.is_some() {
        bail!("field '{path}': `expression` cannot be combined with `type`");
    }
    if field.simple_ref().is_some() {
        bail!("field '{path}': `expression` cannot be combined with `ref`");
    }
    if field.generator.is_some()
        || field
            .range
            .as_ref()
            .is_some_and(|r| r.min.is_some() || r.max.is_some())
    {
        bail!("field '{path}': `expression` cannot be combined with `generator` or `range`");
    }
    if field.value.is_some() {
        bail!("field '{path}': `expression` cannot be combined with `value`");
    }
    if !field.fields.is_empty() || field.content.is_some() {
        bail!("field '{path}': `expression` cannot be combined with `fields` or `content`");
    }
    Ok(())
}

/// Structural keys banned alongside `ref`. `type` is un-mergeable (the ref target owns the
/// type); `fields`/`content` are structural, not constraints. Constraint fields (generator,
/// min, max, value) are allowed — they specialise the referenced field and merge during rewrite.
fn validate_ref_structural_bans(path: &str, field: &Field) -> Result<()> {
    if field.simple_ref().is_none() {
        return Ok(());
    }
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
    Ok(())
}

/// VAR-SPECIALIZE keys: `one_of` (finite-set selector), `preserve_marginal` (variant pin),
/// and `constrain_cases` (per-case specialisation of a ref'd parent variant).
fn validate_specialization_keys(path: &str, field: &Field) -> Result<()> {
    // `one_of`: non-empty, and mutually exclusive with a single `value`.
    if let Some(set) = &field.one_of {
        if set.is_empty() {
            bail!("field '{path}': `one_of` must be non-empty");
        }
        if field.value.is_some() {
            bail!(
                "field '{path}': `value` and `one_of` cannot both be set — use `value` for a \
                 single constant, `one_of` for a finite set"
            );
        }
    }

    // `preserve_marginal` (S4c) pins a variant's global case marginal — variant-only.
    if field.preserve_marginal && !matches!(field.field_type, Some(FieldType::Variant)) {
        bail!("field '{path}': `preserve_marginal` is only valid on a `type: variant` field");
    }

    // `constrain_cases` (S5) specialises named cases of a ref'd parent variant.
    if !field.constrain_cases.is_empty() {
        if field.simple_ref().is_none() {
            bail!(
                "field '{path}': `constrain_cases` is only valid on a field that `ref`s a parent \
                 variant (it specialises that variant's cases by name)"
            );
        }
        for (i, d) in field.constrain_cases.iter().enumerate() {
            if d.name.is_empty() {
                bail!(
                    "field '{path}': `constrain_cases[{i}]` must name the parent case it specialises"
                );
            }
        }
    }
    Ok(())
}

/// A `type: variant` field: validate each case, then check the case-distribution sums.
fn validate_variant_field(path: &str, field: &Field, warnings: &mut Vec<String>) -> Result<()> {
    if field.variants.is_empty() {
        bail!("field '{path}': `type: variant` requires a non-empty `variants` list");
    }
    for (i, choice) in field.variants.iter().enumerate() {
        validate_field_variant(&format!("{path}.variants[{i}]"), choice, warnings)?;
    }
    // Heterogeneous (multi-type / multi-object) variants are supported (VAR-1): they lower to
    // an Arrow union, emitted as a nullable-superset struct. The only output constraint is CSV
    // (a flat format that can't hold the nested struct) — checked per-dataset in
    // `validate_dataset`, where the output format is known.
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
    Ok(())
}

/// Structural and value checks for a non-ref, non-variant field with a known type:
/// type/`fields`/`content`/`range`/generator compatibility, object & list recursion, and
/// `default`/`one_of` value-type compatibility.
fn validate_typed_field(
    path: &str,
    field: &Field,
    field_type: &FieldType,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let range_min = field.range.as_ref().and_then(|r| r.min);
    let range_max = field.range.as_ref().and_then(|r| r.max);

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

    if (range_min.is_some() || range_max.is_some()) && *field_type != FieldType::Number {
        bail!(
            "field '{path}': `range` is only valid on `number` type fields, \
                 but this field has type `{field_type}`"
        );
    }

    if let Some(g) = &field.generator
        && !g.valid_for(field_type)
    {
        bail!("field '{path}': generator `{g}` is not valid for type `{field_type}`");
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
            FieldType::Number => (default_val.is_number(), "a number"),
            FieldType::String | FieldType::Date | FieldType::DateTime => {
                (default_val.is_string(), "a string")
            }
            FieldType::Boolean => (default_val.is_bool(), "a boolean"),
            FieldType::List => (default_val.is_sequence(), "a sequence (e.g. `default: []`)"),
            FieldType::Object => (default_val.is_mapping(), "a mapping"),
            // Variant/Union are pre-expansion or internal types; not reached here (validation
            // runs before expand_field_variants). Accept to keep the match exhaustive.
            FieldType::Variant | FieldType::Union => (true, ""),
        };
        if !compatible {
            bail!(
                "field '{path}': `default` value is incompatible with `type: {field_type}` — \
                 expected {expected}"
            );
        }
    }

    // `one_of` entries must be type-compatible with the declared field type (VAR-SPECIALIZE).
    if let Some(set) = &field.one_of {
        for v in set {
            let (compatible, expected) = match field_type {
                FieldType::Number => (v.is_number(), "numbers"),
                FieldType::String | FieldType::Date | FieldType::DateTime => {
                    (v.is_string(), "strings")
                }
                FieldType::Boolean => (v.is_bool(), "booleans"),
                FieldType::List | FieldType::Object | FieldType::Variant | FieldType::Union => {
                    (true, "")
                }
            };
            if !compatible {
                bail!(
                    "field '{path}': `one_of` entries must be {expected} for `type: {field_type}`"
                );
            }
        }
    }

    Ok(())
}

/// Validate all fields inside a `content: {group: <ref>, ...}` block.
///
/// Two ref scopes apply:
/// - **Linked-scoped** (`ref: link_ref.field`): dot-prefixed with the link ref; the target
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

        // VAR-LINKED-CONTENT gate: tagged-union (`type: variant`) item fields on a linked
        // content list interact with the witness/staging pipeline (`n_eligible_slots`,
        // `_staging_refs`) in ways VAR-EXPAND deliberately deferred. Reject until designed.
        if matches!(field.field_type, Some(FieldType::Variant)) {
            bail!(
                "field '{fpath}': `type: variant` is not yet supported inside list-link content \
                 (linked content lists) — see specs/VAR-LINKED-CONTENT.md"
            );
        }

        if let Some(ref_str) = field.simple_ref() {
            // Determine scope: linked-scoped (dot matches the link ref) or outer-scoped.
            let linked_scoped = split_ref(ref_str).and_then(|(ref_part, _)| {
                if link.reference == ref_part {
                    Some(link)
                } else {
                    None
                }
            });

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
                        ref_str,
                        target_name,
                        inc.file
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
        if let (Some(g), Some(ft)) = (&field.generator, &field.field_type)
            && !g.valid_for(ft)
        {
            bail!("field '{fpath}': generator `{g}` is not valid for type `{ft}`");
        }
    }
    Ok(())
}

fn validate_field_variant(
    path: &str,
    choice: &FieldVariant,
    _warnings: &mut Vec<String>,
) -> Result<()> {
    if let Some(ft) = &choice.field_type
        && *ft == FieldType::Variant
    {
        bail!("field variant '{path}': nested `type: variant` is not supported");
    }

    if choice.value.is_some() {
        if choice.generator.is_some() {
            bail!("field variant '{path}': `value` and `generator` cannot both be set");
        }
        if choice
            .range
            .as_ref()
            .is_some_and(|r| r.min.is_some() || r.max.is_some())
        {
            bail!("field variant '{path}': `value` and `range` cannot both be set");
        }
    }

    if let Some(r) = &choice.range
        && let (Some(lo), Some(hi)) = (r.min, r.max)
        && lo > hi
    {
        bail!("field variant '{path}': range.min ({lo}) must be ≤ range.max ({hi})");
    }

    validate_locale_generator(path, choice.locale.as_ref(), choice.generator.as_ref())?;

    if let (Some(g), Some(ft)) = (&choice.generator, &choice.field_type)
        && !g.valid_for(ft)
    {
        bail!("field variant '{path}': generator `{g}` is not valid for type `{ft}`");
    }

    // Must be able to determine the concrete type at expansion time.
    let can_infer_type = choice.field_type.is_some()
        || choice
            .range
            .as_ref()
            .is_some_and(|r| r.min.is_some() || r.max.is_some())
        || choice
            .value
            .as_ref()
            .is_some_and(|v| v.is_string() || v.is_number() || v.is_bool());
    if !can_infer_type {
        bail!("field variant '{path}': cannot determine type — set `type`, `value`, or `range`");
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
        if let Some(content) = &field.content
            && content.from.is_some()
        {
            for cf in &content.item.fields {
                let cf_path = format!("{field_path}[].{}", cf.name);
                for binding in cf.collect_bindings() {
                    validate_single_collect_bind(dataset_path, dataset, all, &cf_path, binding)?;
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
    let bind = binding
        .bind
        .as_deref()
        .ok_or_else(|| anyhow!("field '{field_path}': collect binding has no `bind` target"))?;

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
                    linked_field
                        .field_type
                        .as_ref()
                        .map(|t| t.to_string())
                        .unwrap_or_default()
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
                    linked_field
                        .field_type
                        .as_ref()
                        .map(|t| t.to_string())
                        .unwrap_or_default()
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
                "field '{}.{}': ref '{}' — no include or link with ref '{}' in this dataset",
                dataset.name,
                field_name,
                ref_str,
                include_ref
            )
        })?;

    let include_path = resolve_include(path, &include.file).ok_or_else(|| {
        anyhow!(
            "field '{}.{}': ref '{}' — cannot resolve include file '{}'",
            dataset.name,
            field_name,
            ref_str,
            include.file
        )
    })?;

    let included = all.get(&include_path).ok_or_else(|| {
        anyhow!(
            "field '{}.{}': ref '{}' — included dataset not loaded",
            dataset.name,
            field_name,
            ref_str
        )
    })?;

    if !included.data.iter().any(|f| f.name == target_field) {
        bail!(
            "field '{}.{}': ref '{}' — field '{}' does not exist in '{}'",
            dataset.name,
            field_name,
            ref_str,
            target_field,
            include.file
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
    let Some(target_path) = resolve_include(path, &include.file) else {
        return;
    };
    let Some(target) = all.get(&target_path) else {
        return;
    };
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
    let Some(ref proj) = content.project else {
        return Ok(());
    };

    if !content.item.fields.is_empty() {
        bail!(
            "field '{}': `project` and `fields` are mutually exclusive — remove `fields` when using `project`",
            content_path
        );
    }

    let (ref_part, field_part) = split_ref(proj).ok_or_else(|| {
        anyhow!(
            "field '{}': `project: {}` — expected `<link_ref>.<field_name>` format",
            content_path,
            proj
        )
    })?;

    if ref_part != link.reference {
        bail!(
            "field '{}': `project: {}` — ref part '{}' does not match the link ref '{}'",
            content_path,
            proj,
            ref_part,
            link.reference
        );
    }

    let inc_path = resolve_include(dataset_path, &link.file).ok_or_else(|| {
        anyhow!(
            "field '{}': `project: {}` — cannot resolve link file '{}'",
            content_path,
            proj,
            link.file
        )
    })?;
    let linked = all.get(&inc_path).ok_or_else(|| {
        anyhow!(
            "field '{}': `project: {}` — linked dataset '{}' not loaded",
            content_path,
            proj,
            link.file
        )
    })?;

    if !linked.data.iter().any(|f| f.name == field_part) {
        bail!(
            "field '{}': `project: {}` — field '{}' does not exist in '{}'",
            content_path,
            proj,
            field_part,
            link.file
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

// ---------------------------------------------------------------------------
// Import taint checks
// ---------------------------------------------------------------------------

/// For every imported dataset, ensure that no child-by-inclusion refs or
/// specialises an imported column (or an expression field derived from one).
///
/// Called once from `validate` after all per-dataset checks complete, so that
/// all `imported_taint` flags are already set by `load_import_headers`.
fn check_import_taint(datasets: &HashMap<PathBuf, SyntheticDataset>) -> Result<()> {
    for (parent_path, parent_ds) in datasets {
        if parent_ds.import.is_none() {
            continue;
        }

        let taint = taint_closure(parent_ds);
        if taint.is_empty() {
            continue;
        }

        for (child_path, child_ds) in datasets {
            let Some(inc) = &child_ds.include else {
                continue;
            };
            let Some(resolved) = resolve_include(child_path, &inc.file) else {
                continue;
            };
            if resolved != *parent_path {
                continue;
            }
            check_child_against_taint(child_ds, inc, &taint)?;
        }
    }
    Ok(())
}

/// Compute the taint closure for an imported dataset:
/// - All directly imported fields (`imported_taint == true`).
/// - Plus any `expression:` fields whose expression AST references any tainted name.
///
/// One pass is sufficient because expressions may only reference fields defined above
/// them (enforced by `validate_expression_order`), so the closure is already stable
/// after a single top-to-bottom sweep.
fn taint_closure(dataset: &SyntheticDataset) -> HashSet<String> {
    let mut tainted: HashSet<String> = dataset
        .data
        .iter()
        .filter(|f| f.imported_taint)
        .map(|f| f.name.clone())
        .collect();

    for field in &dataset.data {
        let Some(ref expr) = field.expression else {
            continue;
        };
        if extract_identifiers(expr)
            .iter()
            .any(|id| tainted.contains(*id))
        {
            tainted.insert(field.name.clone());
        }
    }

    tainted
}

/// Validate that a child-by-inclusion does not ref or specialise any tainted field.
fn check_child_against_taint(
    child: &SyntheticDataset,
    inc: &Include,
    taint: &HashSet<String>,
) -> Result<()> {
    // include.fields: explicit list of field names to wildcard-copy from the parent.
    // Wildcard "*" is already filtered by expand_include_fields; named tainted entries
    // must be rejected here (before expansion runs).
    for field_name in &inc.fields {
        if field_name != "*" && taint.contains(field_name.as_str()) {
            bail!(
                "dataset '{}': `include.fields` lists '{}' which is an imported \
                 column (or derived from one) in the included dataset; \
                 imported fields may not be propagated to children-by-inclusion",
                child.name,
                field_name
            );
        }
    }

    // data.fields: same-name shadowing or explicit ref to a tainted column.
    for field in &child.data {
        // Same name as a tainted field — would silently shadow or specialise it.
        if taint.contains(field.name.as_str()) {
            bail!(
                "dataset '{}': field '{}' has the same name as an imported column \
                 (or derived field) in the included dataset '{}'; imported fields \
                 may not be specialised by children-by-inclusion",
                child.name,
                field.name,
                inc.reference
            );
        }

        // Explicit ref pointing at a tainted column via the include reference.
        if let Some((ref_part, col_part)) = field
            .simple_ref()
            .and_then(|r| split_ref(r))
            .filter(|(rp, cp)| *rp == inc.reference && taint.contains(*cp))
        {
            bail!(
                "dataset '{}': field '{}' references imported column \
                 '{}.{}'; imported fields may not be referenced by \
                 children-by-inclusion",
                child.name,
                field.name,
                ref_part,
                col_part
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod import_taint_tests {
    use super::*;
    use crate::models::{Format, ImportSpec, Include, RefsSpec};
    use std::path::PathBuf;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn make_include(file: &str, reference: &str, fields: Vec<String>) -> Include {
        Include {
            file: file.into(),
            reference: reference.into(),
            ratio: None,
            cardinality: None,
            reinforcement: None,
            overlap: None,
            fields,
            exclude: None,
        }
    }

    fn imported_ds(name: &str, cols: &[(&str, bool)]) -> SyntheticDataset {
        let mut ds = SyntheticDataset {
            name: name.into(),
            format: Format::Csv,
            rows: None,
            output: None,
            outputs: vec![],
            locale: None,
            include: None,
            import: Some(ImportSpec {
                file: format!("{name}.csv"),
                reference: name.into(),
                fields: vec![],
                exclude: None,
                ring: None,
                total_rows: 100,
            }),
            links: vec![],
            data: vec![],
        };
        for (col, tainted) in cols {
            ds.data.push(Field {
                name: col.to_string(),
                imported_taint: *tainted,
                ..Default::default()
            });
        }
        ds
    }

    fn plain_ds(name: &str, data: Vec<Field>) -> SyntheticDataset {
        SyntheticDataset {
            name: name.into(),
            format: Format::Csv,
            rows: Some(50),
            output: None,
            outputs: vec![],
            locale: None,
            include: None,
            import: None,
            links: vec![],
            data,
        }
    }

    /// Build a (parent, child) dataset map. The child's include file resolves to the
    /// parent by path equality — no disk access needed when calling `check_import_taint`
    /// directly (bypassing `validate_dataset_refs` which requires real files).
    #[allow(dead_code)]
    fn parent_child_map(
        parent_data: Vec<Field>,
        child_include_fields: Vec<String>,
        child_data: Vec<Field>,
    ) -> HashMap<PathBuf, SyntheticDataset> {
        let parent_path = PathBuf::from("/schema/parent.yaml");
        let child_path = PathBuf::from("/schema/child.yaml");
        let mut parent = imported_ds("parent", &[]);
        parent.data = parent_data;
        let mut child = plain_ds("child", child_data);
        // The include file "parent.yaml" must resolve (via resolve_include) to parent_path.
        // resolve_include calls canonicalize, so we need a real path. Instead we call
        // check_import_taint directly, which uses resolve_include internally. Since those
        // paths don't exist on disk the taint check falls through — so we call
        // check_child_against_taint directly for the cross-dataset assertions.
        child.include = Some(make_include("parent.yaml", "par", child_include_fields));
        let mut map = HashMap::new();
        map.insert(parent_path, parent);
        map.insert(child_path, child);
        map
    }

    // ── rows + import mutual exclusion (use full validate — no cross-dataset refs) ──

    #[test]
    fn rows_and_import_errors() {
        let mut ds = imported_ds("tickers", &[("symbol", true)]);
        ds.rows = Some(50);
        let mut datasets = HashMap::new();
        datasets.insert(PathBuf::from("/s/tickers.yaml"), ds);
        let err = validate(&datasets).unwrap_err().to_string();
        assert!(
            err.contains("`rows` cannot be set when `import` is present"),
            "{err}"
        );
    }

    #[test]
    fn import_without_rows_is_ok() {
        let ds = imported_ds("tickers", &[("symbol", true)]);
        let mut datasets = HashMap::new();
        datasets.insert(PathBuf::from("/s/tickers.yaml"), ds);
        assert!(validate(&datasets).is_ok());
    }

    #[test]
    fn imported_dataset_no_false_row_warning() {
        let ds = imported_ds("tickers", &[("symbol", true)]);
        let mut datasets = HashMap::new();
        datasets.insert(PathBuf::from("/s/tickers.yaml"), ds);
        let warnings = validate(&datasets).unwrap();
        assert!(
            !warnings
                .iter()
                .any(|w| w.contains("defaulting to 100 rows")),
            "spurious row-count warning: {warnings:?}"
        );
    }

    // ── ring bounds (single dataset — full validate is fine) ─────────────────

    #[test]
    fn ring_start_ge_end_errors() {
        let mut ds = imported_ds("tickers", &[("symbol", true)]);
        ds.import.as_mut().unwrap().ring = Some(crate::models::RingBounds {
            start: 0.6,
            end: 0.3,
        });
        let mut datasets = HashMap::new();
        datasets.insert(PathBuf::from("/s/tickers.yaml"), ds);
        let err = validate(&datasets).unwrap_err().to_string();
        assert!(err.contains("must be less than"), "{err}");
    }

    #[test]
    fn ring_out_of_range_errors() {
        let mut ds = imported_ds("tickers", &[("symbol", true)]);
        ds.import.as_mut().unwrap().ring = Some(crate::models::RingBounds {
            start: 0.0,
            end: 1.5,
        });
        let mut datasets = HashMap::new();
        datasets.insert(PathBuf::from("/s/tickers.yaml"), ds);
        let err = validate(&datasets).unwrap_err().to_string();
        assert!(err.contains("[0.0, 1.0)"), "{err}");
    }

    // ── taint closure (unit-test taint_closure directly) ─────────────────────

    #[test]
    fn taint_closure_includes_imported_fields() {
        let ds = imported_ds("p", &[("symbol", true), ("region", false)]);
        let t = taint_closure(&ds);
        assert!(t.contains("symbol"));
        assert!(!t.contains("region"));
    }

    #[test]
    fn taint_closure_expands_expression_referencing_tainted_field() {
        let mut ds = imported_ds("p", &[("symbol", true)]);
        ds.data.push(Field {
            name: "display".into(),
            expression: Some("CONCAT(symbol, '-USD')".into()),
            ..Default::default()
        });
        let t = taint_closure(&ds);
        assert!(
            t.contains("display"),
            "expression derived from imported field must be tainted"
        );
    }

    #[test]
    fn taint_closure_does_not_taint_unrelated_expression() {
        let mut ds = imported_ds("p", &[("symbol", true), ("exchange", false)]);
        ds.data.push(Field {
            name: "suffix".into(),
            expression: Some("CONCAT(exchange, '-SFX')".into()),
            ..Default::default()
        });
        let t = taint_closure(&ds);
        assert!(
            !t.contains("suffix"),
            "expression with no imported deps should not be tainted"
        );
    }

    // ── check_child_against_taint (called directly to avoid disk-path resolution) ──

    fn taint(cols: &[&str]) -> HashSet<String> {
        cols.iter().map(|s| s.to_string()).collect()
    }

    fn child_with_include(
        reference: &str,
        include_fields: Vec<String>,
        data: Vec<Field>,
    ) -> (SyntheticDataset, Include) {
        let mut ds = plain_ds("child", data);
        let inc = make_include("parent.yaml", reference, include_fields);
        ds.include = Some(inc.clone());
        (ds, inc)
    }

    #[test]
    fn child_same_name_as_imported_column_errors() {
        let (child, inc) = child_with_include(
            "par",
            vec![],
            vec![Field {
                name: "symbol".into(),
                ..Default::default()
            }],
        );
        let err = check_child_against_taint(&child, &inc, &taint(&["symbol"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("same name as an imported column"), "{err}");
    }

    #[test]
    fn child_ref_to_imported_column_errors() {
        let (child, inc) = child_with_include(
            "par",
            vec![],
            vec![Field {
                name: "ticker".into(),
                refs: Some(RefsSpec::Single("par.symbol".into())),
                ..Default::default()
            }],
        );
        let err = check_child_against_taint(&child, &inc, &taint(&["symbol"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("references imported column"), "{err}");
    }

    #[test]
    fn child_ref_to_synthetic_column_is_ok() {
        let (child, inc) = child_with_include(
            "par",
            vec![],
            vec![Field {
                name: "area".into(),
                refs: Some(RefsSpec::Single("par.region".into())),
                ..Default::default()
            }],
        );
        // "region" is NOT in the taint set — only "symbol" is.
        assert!(check_child_against_taint(&child, &inc, &taint(&["symbol"])).is_ok());
    }

    #[test]
    fn child_include_fields_listing_imported_column_errors() {
        let (child, inc) = child_with_include("par", vec!["symbol".into()], vec![]);
        let err = check_child_against_taint(&child, &inc, &taint(&["symbol"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`include.fields` lists"), "{err}");
    }

    #[test]
    fn child_include_fields_wildcard_is_ok() {
        // Wildcard is silently filtered by expand_include_fields; the validator
        // should not reject it here.
        let (child, inc) = child_with_include("par", vec!["*".into()], vec![]);
        assert!(check_child_against_taint(&child, &inc, &taint(&["symbol"])).is_ok());
    }

    #[test]
    fn child_ref_to_expression_derived_column_errors() {
        let (child, inc) = child_with_include(
            "par",
            vec![],
            vec![Field {
                name: "label".into(),
                refs: Some(RefsSpec::Single("par.display".into())),
                ..Default::default()
            }],
        );
        // "display" is in the taint set (derived from imported "symbol").
        let err = check_child_against_taint(&child, &inc, &taint(&["symbol", "display"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("references imported column"), "{err}");
    }

    #[test]
    fn child_ref_to_expression_from_synthetic_is_ok() {
        let (child, inc) = child_with_include(
            "par",
            vec![],
            vec![Field {
                name: "exch_tag".into(),
                refs: Some(RefsSpec::Single("par.suffix".into())),
                ..Default::default()
            }],
        );
        // "suffix" is NOT in the taint set.
        assert!(check_child_against_taint(&child, &inc, &taint(&["symbol"])).is_ok());
    }
}
