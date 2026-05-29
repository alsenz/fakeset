//! Variant expansion. `expand_field_variants` resolves `type: variant` fields into
//! concrete global `variants:` entries so the planner and executor see a uniform schema.
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::models::{resolve_distributions, Field, FieldType, FieldVariant, ParquetConfig, Schema, SyntheticDataset, VariantSchema};

/// Replace all `type: variant` fields in every dataset with global `VariantSchema` entries.
///
/// The transformation is:
/// 1. Collect all `type: variant` fields recursively (including inside objects).
/// 2. Compute the Cartesian product of their choices → one `VariantSchema` per combination,
///    with `distribution = product of the chosen per-field distributions`.
/// 3. If the dataset already has top-level `variants`, cross-product them with the local
///    combinations (global_dist × local_dist, data deep-merged).
/// 4. Remove the `type: variant` fields from the dataset's base `data`.
///
/// After this pass every dataset's `data` contains only concrete (non-Variant) fields, and
/// any Variant semantics are expressed purely as global `VariantSchema` entries.
pub fn expand_field_variants(
    mut datasets: HashMap<PathBuf, SyntheticDataset>,
) -> Result<HashMap<PathBuf, SyntheticDataset>> {
    for dataset in datasets.values_mut() {
        let variant_paths = collect_variant_paths(&dataset.data, &[]);
        if variant_paths.is_empty() {
            continue;
        }

        let local_combos = build_local_combinations(&variant_paths);

        dataset.variants = if dataset.variants.is_empty() {
            local_combos
        } else {
            cross_product_variants(&dataset.variants, &local_combos)
        };

        stub_variant_fields(&mut dataset.data, &variant_paths, &[]);
    }
    Ok(datasets)
}

// ---------------------------------------------------------------------------
// Collecting variant fields
// ---------------------------------------------------------------------------

/// Each entry: (path-to-field, variant-choices, outer-parquet-fallback).
type VariantPaths = Vec<(Vec<String>, Vec<FieldVariant>, Option<ParquetConfig>)>;

fn collect_variant_paths(schema: &Schema, prefix: &[String]) -> VariantPaths {
    let mut result = VariantPaths::new();
    for field in schema {
        let mut path = prefix.to_vec();
        path.push(field.name.clone());

        if matches!(field.field_type, Some(FieldType::Variant)) {
            result.push((path, field.variants.clone(), field.parquet.clone()));
        } else if matches!(field.field_type, Some(FieldType::Object)) {
            result.extend(collect_variant_paths(&field.fields, &path));
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Building local combinations (Cartesian product)
// ---------------------------------------------------------------------------

fn build_local_combinations(variant_paths: &VariantPaths) -> Vec<VariantSchema> {
    // Each combo accumulates (joint_dist, delta_schema).
    let mut combos: Vec<(f64, Schema)> = vec![(1.0, vec![])];

    for (path, choices, outer_parquet) in variant_paths {
        let choice_dists: Vec<Option<f64>> = choices.iter().map(|v| v.ratio).collect();
        let dists = resolve_distributions(&choice_dists);
        let mut next = Vec::with_capacity(combos.len() * choices.len());

        for (joint_dist, delta) in &combos {
            for (choice, &dist) in choices.iter().zip(dists.iter()) {
                let mut new_delta = delta.clone();
                let delta_field = build_delta_field(path, choice, outer_parquet.as_ref());
                merge_delta_into(&mut new_delta, delta_field);
                next.push((joint_dist * dist, new_delta));
            }
        }
        combos = next;
    }

    combos
        .into_iter()
        .map(|(dist, data)| VariantSchema { data, ratio: Some(dist), locale: None })
        .collect()
}

/// Cross-product existing global variants with the local combinations.
/// Each pair gets joint_dist = global_dist × local_dist; data is deep-merged
/// (local overrides global on name collisions), locale comes from the global variant.
fn cross_product_variants(
    globals: &[VariantSchema],
    locals: &[VariantSchema],
) -> Vec<VariantSchema> {
    let global_raw: Vec<Option<f64>> = globals.iter().map(|v| v.ratio).collect();
    let global_dists = resolve_distributions(&global_raw);
    let mut result = Vec::with_capacity(globals.len() * locals.len());

    for (gv, &gd) in globals.iter().zip(global_dists.iter()) {
        for lv in locals {
            let joint_dist = gd * lv.ratio.unwrap_or(1.0);
            let merged_data = deep_merge_schemas(&gv.data, &lv.data);
            result.push(VariantSchema {
                data: merged_data,
                ratio: Some(joint_dist),
                locale: gv.locale.clone(),
            });
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Delta field construction
// ---------------------------------------------------------------------------

/// Build a Field (possibly nested in Object wrappers) representing one Cartesian
/// choice for the field at `path`.  `outer_parquet` is the parent field's parquet
/// config and is used as a fallback when the variant choice has no parquet override.
fn build_delta_field(path: &[String], variant: &FieldVariant, outer_parquet: Option<&ParquetConfig>) -> Field {
    if path.len() == 1 {
        let parquet = variant.parquet.clone().or_else(|| outer_parquet.cloned());
        let field_type = infer_field_type(variant);
        Field {
            name: path[0].clone(),
            field_type,
            generator: variant.generator.clone(),
            locale: variant.locale.clone(),
            range: variant.range,
            value: variant.value.clone(),
            parquet,
            ..Default::default()
        }
    } else {
        Field {
            name: path[0].clone(),
            field_type: Some(FieldType::Object),
            fields: vec![build_delta_field(&path[1..], variant, outer_parquet)],
            ..Default::default()
        }
    }
}

/// Infer the concrete `FieldType` for a variant choice that omits `type`.
/// Priority: explicit type > range present (→ Number) > value type (string/number/bool).
fn infer_field_type(variant: &FieldVariant) -> Option<FieldType> {
    variant.field_type.clone()
        .or_else(|| {
            if variant.range.as_ref().is_some_and(|r| r.min.is_some() || r.max.is_some()) {
                Some(FieldType::Number)
            } else {
                None
            }
        })
        .or_else(|| {
            variant.value.as_ref().and_then(|v| {
                if v.is_string()  { Some(FieldType::String)  }
                else if v.is_number() { Some(FieldType::Number) }
                else if v.is_bool()   { Some(FieldType::Boolean) }
                else { None }
            })
        })
}

// ---------------------------------------------------------------------------
// Schema merge helpers
// ---------------------------------------------------------------------------

/// Deep-merge `delta` onto `base`: same-named scalar fields are replaced,
/// same-named Object fields recurse into their sub-fields.
pub fn deep_merge_schemas(base: &Schema, delta: &Schema) -> Schema {
    let mut result = base.clone();
    for delta_field in delta {
        merge_delta_into(&mut result, delta_field.clone());
    }
    result
}

/// Insert/replace `field` into `schema` with deep-merge semantics for Objects.
pub(crate) fn merge_delta_into(schema: &mut Schema, field: Field) {
    if let Some(existing) = schema.iter_mut().find(|f| f.name == field.name) {
        if matches!(existing.field_type, Some(FieldType::Object))
            && matches!(field.field_type, Some(FieldType::Object))
            && !field.fields.is_empty()
        {
            for sub in field.fields {
                merge_delta_into(&mut existing.fields, sub);
            }
        } else {
            *existing = field;
        }
    } else {
        schema.push(field);
    }
}

/// Replace all `type: variant` fields in a schema with name-preserving typed stubs.
///
/// The stub retains the field name and infers a concrete type from the variant choices
/// (using the type common to all choices, or `None` if choices have inconsistent types).
/// Keeping the field in `data` ensures `resolve_refs` can still locate it by name after
/// variant expansion — without this, refs like `ref: policy.policy_type` would fail.
fn stub_variant_fields(schema: &mut Schema, variant_paths: &VariantPaths, prefix: &[String]) {
    for field in schema.iter_mut() {
        if matches!(field.field_type, Some(FieldType::Variant)) {
            let mut path = prefix.to_vec();
            path.push(field.name.clone());
            let unified_type = variant_paths
                .iter()
                .find(|(p, _, _)| p == &path)
                .and_then(|(_, choices, _)| {
                    let types: Vec<_> = choices.iter().filter_map(|v| infer_field_type(v)).collect();
                    if types.is_empty() {
                        None
                    } else if types.windows(2).all(|w| w[0] == w[1]) {
                        types.into_iter().next()
                    } else {
                        None // mixed types — leave untyped
                    }
                });
            *field = Field { name: field.name.clone(), field_type: unified_type, ..Default::default() };
        } else if matches!(field.field_type, Some(FieldType::Object)) {
            let mut path = prefix.to_vec();
            path.push(field.name.clone());
            stub_variant_fields(&mut field.fields, variant_paths, &path);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Format, Range};

    fn make_variant_field(name: &str, choices: Vec<FieldVariant>) -> Field {
        Field {
            name: name.to_string(),
            field_type: Some(FieldType::Variant),
            variants: choices,
            ..Default::default()
        }
    }

    fn string_choice(val: &str, dist: Option<f64>) -> FieldVariant {
        FieldVariant {
            field_type: Some(FieldType::String),
            value: Some(serde_yaml::Value::String(val.to_string())),
            ratio: dist,
            ..Default::default()
        }
    }

    fn bare_ds(data: Schema) -> SyntheticDataset {
        SyntheticDataset {
            name: "test".into(),
            format: Format::Csv,
            output: None,
            outputs: vec![],
            rows: Some(100),
            locale: None,
            include: None,
            links: vec![],
            data,
            variants: vec![],
        }
    }

    #[test]
    fn single_variant_field_produces_n_global_variants() {
        let ds = bare_ds(vec![
            make_variant_field("status", vec![
                string_choice("active",   Some(0.6)),
                string_choice("inactive", Some(0.4)),
            ]),
        ]);

        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/test.yaml"), ds);
        let result = expand_field_variants(map).unwrap();
        let ds = result.values().next().unwrap();

        assert_eq!(ds.variants.len(), 2);
        assert!(ds.data.iter().all(|f| !matches!(f.field_type, Some(FieldType::Variant))));

        let dists: Vec<f64> = ds.variants.iter().map(|v| v.ratio.unwrap()).collect();
        assert!((dists[0] - 0.6).abs() < 1e-9);
        assert!((dists[1] - 0.4).abs() < 1e-9);
    }

    #[test]
    fn two_variant_fields_produce_cartesian_product() {
        let ds = bare_ds(vec![
            make_variant_field("status", vec![
                string_choice("active",   Some(0.6)),
                string_choice("inactive", Some(0.4)),
            ]),
            make_variant_field("tier", vec![
                string_choice("gold",   Some(0.2)),
                string_choice("silver", Some(0.3)),
                string_choice("bronze", Some(0.5)),
            ]),
        ]);

        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/test.yaml"), ds);
        let result = expand_field_variants(map).unwrap();
        let ds = result.values().next().unwrap();

        assert_eq!(ds.variants.len(), 6, "2 × 3 = 6 combinations");

        let sum: f64 = ds.variants.iter().map(|v| v.ratio.unwrap()).sum();
        assert!((sum - 1.0).abs() < 1e-9, "joint distributions must sum to 1.0; got {sum}");
    }

    #[test]
    fn free_distributions_split_remainder_equally() {
        let ds = bare_ds(vec![
            make_variant_field("x", vec![
                FieldVariant { field_type: Some(FieldType::String), ratio: None, ..Default::default() },
                FieldVariant { field_type: Some(FieldType::String), ratio: None, ..Default::default() },
                FieldVariant { field_type: Some(FieldType::String), ratio: None, ..Default::default() },
            ]),
        ]);
        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/test.yaml"), ds);
        let result = expand_field_variants(map).unwrap();
        let ds = result.values().next().unwrap();
        for v in &ds.variants {
            assert!((v.ratio.unwrap() - 1.0 / 3.0).abs() < 1e-9);
        }
    }

    #[test]
    fn cross_product_with_existing_global_variants() {
        let mut ds = bare_ds(vec![
            make_variant_field("status", vec![
                string_choice("a", Some(0.5)),
                string_choice("b", Some(0.5)),
            ]),
        ]);
        // Two existing global variants (50/50)
        ds.variants = vec![
            VariantSchema { data: vec![], ratio: Some(0.5), locale: None },
            VariantSchema { data: vec![], ratio: Some(0.5), locale: None },
        ];

        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/test.yaml"), ds);
        let result = expand_field_variants(map).unwrap();
        let ds = result.values().next().unwrap();

        // 2 global × 2 local = 4
        assert_eq!(ds.variants.len(), 4);
        let sum: f64 = ds.variants.iter().map(|v| v.ratio.unwrap()).sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }

    #[test]
    fn nested_object_variant_field_is_collected() {
        let nested_variant = make_variant_field("priority", vec![
            string_choice("high", Some(0.3)),
            string_choice("low",  Some(0.7)),
        ]);
        let object_field = Field {
            name: "metadata".to_string(),
            field_type: Some(FieldType::Object),
            fields: vec![nested_variant],
            ..Default::default()
        };

        let ds = bare_ds(vec![object_field]);
        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/test.yaml"), ds);
        let result = expand_field_variants(map).unwrap();
        let ds = result.values().next().unwrap();

        assert_eq!(ds.variants.len(), 2);
        // The delta schema should contain a metadata object with a priority sub-field
        let v0 = &ds.variants[0];
        let meta = v0.data.iter().find(|f| f.name == "metadata").expect("metadata in delta");
        assert!(meta.fields.iter().any(|f| f.name == "priority"));
    }

    #[test]
    fn range_only_variant_infers_number_type() {
        let choice = FieldVariant {
            range: Some(Range { min: Some(1.0), max: Some(10.0) }),
            ratio: Some(1.0),
            ..Default::default()
        };
        let inferred = infer_field_type(&choice);
        assert_eq!(inferred, Some(FieldType::Number));
    }

    #[test]
    fn string_value_variant_infers_string_type() {
        let choice = FieldVariant {
            value: Some(serde_yaml::Value::String("hello".into())),
            ratio: Some(1.0),
            ..Default::default()
        };
        let inferred = infer_field_type(&choice);
        assert_eq!(inferred, Some(FieldType::String));
    }

    #[test]
    fn outer_parquet_propagates_to_delta_when_inner_absent() {
        use crate::models::{ParquetConfig, ParquetDatatype};
        let outer_parquet = ParquetConfig { datatype: ParquetDatatype::Int32 };
        let choice = FieldVariant {
            field_type: Some(FieldType::Number),
            range: Some(Range { min: Some(0.0), max: Some(100.0) }),
            ratio: Some(1.0),
            parquet: None,
            ..Default::default()
        };
        let delta = build_delta_field(&["amount".to_string()], &choice, Some(&outer_parquet));
        assert_eq!(delta.parquet.as_ref().map(|p| &p.datatype), Some(&ParquetDatatype::Int32));
    }

    #[test]
    fn inner_parquet_overrides_outer() {
        use crate::models::{ParquetConfig, ParquetDatatype};
        let outer_parquet = ParquetConfig { datatype: ParquetDatatype::Int32 };
        let inner_parquet = ParquetConfig { datatype: ParquetDatatype::Float32 };
        let choice = FieldVariant {
            field_type: Some(FieldType::Number),
            ratio: Some(1.0),
            parquet: Some(inner_parquet),
            ..Default::default()
        };
        let delta = build_delta_field(&["x".to_string()], &choice, Some(&outer_parquet));
        assert_eq!(delta.parquet.as_ref().map(|p| &p.datatype), Some(&ParquetDatatype::Float32));
    }
}
