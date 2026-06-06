//! Variant expansion (`expand_field_variants`). Three kinds of `type: variant` field are
//! routed here:
//! - **heterogeneous** (multi-type / object cases) → an Arrow `DenseUnion` (`FieldType::Union`)
//!   via [`lower_heterogeneous_unions`] (VAR-1);
//! - **case-3** (`ref` + `variants`) → cross-producted into `dataset.variants` so the lower-cover
//!   planner can lower each case into a ref-bound, value-pinned case-member (VAR-SPECIALIZE);
//! - **same-type, no ref** → left in place with a unified concrete `field_type` *and* its
//!   `variants` retained, so the generator produces them **per-row** (VAR-UNIFY Phase 2).
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::models::{
    Field, FieldType, FieldVariant, ParquetConfig, RefsSpec, Schema, SyntheticDataset, UnionCase,
    VariantSchema, resolve_distributions,
};

/// Route every `type: variant` field to its generation path (see the module docs):
/// 1. Lower **heterogeneous** variants to `DenseUnion` columns.
/// 2. Cross-product **case-3** (`ref` + `variants`) fields into `dataset.variants` for
///    lower-cover lowering.
/// 3. Finalise the rest: **same-type, no-ref** variants keep their cases (with a unified
///    concrete `field_type`) for per-row generation; case-3 stubs keep only their `ref`.
pub fn expand_field_variants(
    mut datasets: HashMap<PathBuf, SyntheticDataset>,
) -> Result<HashMap<PathBuf, SyntheticDataset>> {
    for dataset in datasets.values_mut() {
        // Heterogeneous (multi-type) variant fields become Arrow `DenseUnion` columns
        // (VAR-1) — lower them first so only homogeneous variants remain below.
        lower_heterogeneous_unions(&mut dataset.data);

        // Case-3 (`ref` + `variants`) fields cross-product into `dataset.variants` for
        // lower-cover lowering. Plain same-type variants are left in place to generate per-row.
        let case3_paths = collect_variant_paths(&dataset.data, &[]);
        if !case3_paths.is_empty() {
            // `dataset.variants` is always empty here — top-level `variants:` is rejected at
            // validation (VAR-UNIFY U4) — so the case-3 combos become the variant set directly.
            dataset.variants = build_local_combinations(&case3_paths);
        }

        finalize_variant_fields(&mut dataset.data);
    }
    Ok(datasets)
}

// ---------------------------------------------------------------------------
// Collecting variant fields
// ---------------------------------------------------------------------------

/// Each entry: (path-to-field, variant-choices, outer-parquet-fallback, field-ref).
/// The ref is `Some` for a **case-3** field (`ref` + `variants`; VAR-SPECIALIZE): each
/// cross-product delta must then carry that ref so the lowered case inherits the parent
/// column (and so enters lower-cover conflict pruning).
type VariantPaths = Vec<(
    Vec<String>,
    Vec<FieldVariant>,
    Option<ParquetConfig>,
    Option<RefsSpec>,
)>;

fn collect_variant_paths(schema: &Schema, prefix: &[String]) -> VariantPaths {
    let mut result = VariantPaths::new();
    for field in schema {
        let mut path = prefix.to_vec();
        path.push(field.name.clone());

        // Only **case-3** fields (`ref` + `variants`) cross-product into `dataset.variants`
        // for lowering. Plain same-type `type: variant` fields are *not* collected — they
        // generate per-row (VAR-UNIFY Phase 2; see `finalize_variant_fields`).
        if field.refs.is_some() && !field.variants.is_empty() {
            result.push((
                path,
                field.variants.clone(),
                field.parquet.clone(),
                field.refs.clone(),
            ));
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

    for (path, choices, outer_parquet, refs) in variant_paths {
        let choice_dists: Vec<Option<f64>> = choices.iter().map(|v| v.ratio).collect();
        let dists = resolve_distributions(&choice_dists);
        let mut next = Vec::with_capacity(combos.len() * choices.len());

        for (joint_dist, delta) in &combos {
            for (choice, &dist) in choices.iter().zip(dists.iter()) {
                let mut new_delta = delta.clone();
                let delta_field =
                    build_delta_field(path, choice, outer_parquet.as_ref(), refs.as_ref());
                merge_delta_into(&mut new_delta, delta_field);
                next.push((joint_dist * dist, new_delta));
            }
        }
        combos = next;
    }

    combos
        .into_iter()
        .map(|(dist, data)| VariantSchema {
            data,
            ratio: Some(dist),
            locale: None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Delta field construction
// ---------------------------------------------------------------------------

/// Build a Field (possibly nested in Object wrappers) representing one Cartesian
/// choice for the field at `path`.  `outer_parquet` is the parent field's parquet
/// config and is used as a fallback when the variant choice has no parquet override.
fn build_delta_field(
    path: &[String],
    variant: &FieldVariant,
    outer_parquet: Option<&ParquetConfig>,
    refs: Option<&RefsSpec>,
) -> Field {
    if path.len() == 1 {
        let parquet = variant.parquet.clone().or_else(|| outer_parquet.cloned());
        let field_type = infer_field_type(variant);
        Field {
            name: path[0].clone(),
            field_type,
            // Case 3: carry the field's ref onto each delta so the lowered case-member
            // inherits the parent column (and its value pin enters conflict pruning).
            refs: refs.cloned(),
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
            fields: vec![build_delta_field(&path[1..], variant, outer_parquet, refs)],
            ..Default::default()
        }
    }
}

/// Infer the concrete `FieldType` for a variant choice that omits `type`.
/// Priority: explicit type > range present (→ Number) > value type (string/number/bool).
pub(crate) fn infer_field_type(variant: &FieldVariant) -> Option<FieldType> {
    variant
        .field_type
        .clone()
        .or_else(|| {
            if variant
                .range
                .as_ref()
                .is_some_and(|r| r.min.is_some() || r.max.is_some())
            {
                Some(FieldType::Number)
            } else {
                None
            }
        })
        .or_else(|| {
            variant.value.as_ref().and_then(|v| {
                if v.is_string() {
                    Some(FieldType::String)
                } else if v.is_number() {
                    Some(FieldType::Number)
                } else if v.is_bool() {
                    Some(FieldType::Boolean)
                } else {
                    None
                }
            })
        })
        // An object case is recognised by its nested `fields` even without `type: object`.
        .or_else(|| (!variant.fields.is_empty()).then_some(FieldType::Object))
}

// ---------------------------------------------------------------------------
// Heterogeneous (multi-type) union lowering — VAR-1
// ---------------------------------------------------------------------------

/// Lower every **heterogeneous** `type: variant` field (cases spanning ≥2 distinct types)
/// into a `FieldType::Union` field carrying one [`UnionCase`] per choice. Homogeneous
/// variants are left untouched for the same-type expansion path. Recurses into objects.
///
/// Scope (PR 3): cases are built from the scalar properties of each [`FieldVariant`]
/// (type/generator/value/range/locale/parquet). Object-schema cases need `FieldVariant`
/// to carry `fields:` — a follow-up; `UnionCase.field` is already a full `Field`.
fn lower_heterogeneous_unions(schema: &mut Schema) {
    for field in schema.iter_mut() {
        match field.field_type {
            Some(FieldType::Variant) if is_heterogeneous(&field.variants) => {
                let cases: Vec<UnionCase> = field
                    .variants
                    .iter()
                    .enumerate()
                    .map(|(i, choice)| {
                        let mut case_field = build_union_case_field(choice);
                        // Distinct child names keep the Arrow union schema readable.
                        if case_field.name.is_empty() {
                            case_field.name = format!("{}_{i}", field.name);
                        }
                        UnionCase {
                            field: case_field,
                            ratio: choice.ratio,
                        }
                    })
                    .collect();
                field.field_type = Some(FieldType::Union);
                field.union_cases = cases;
                field.variants = vec![];
            }
            Some(FieldType::Object) => lower_heterogeneous_unions(&mut field.fields),
            _ => {}
        }
    }
}

/// True when a variant field's choices require a tagged-union column rather than a
/// single stub type — i.e. they cannot share one Arrow column type:
///
/// - **any object case** ⇒ union. Two object cases are both `FieldType::Object` but may
///   carry *different schemas*, which `FieldType` equality can't distinguish; rather than
///   compare schemas, treat any object-bearing variant as a union (a same-schema object
///   variant riding the union path is harmless, and the scalar cross-product path never
///   copied per-case `fields` anyway).
/// - otherwise, **≥2 distinct scalar types** ⇒ union.
///
/// Shared with the validator (`validate.rs`) so the "what's a union" definition has one
/// source of truth — the gate rejects exactly what this lowers, until VAR-1 output
/// encoding (PR 4) makes unions writable.
pub(crate) fn is_heterogeneous(choices: &[FieldVariant]) -> bool {
    let types: Vec<FieldType> = choices.iter().filter_map(infer_field_type).collect();
    if types.iter().any(|t| matches!(t, FieldType::Object)) {
        return true;
    }
    // `windows(2).any(ne)` over the inferred types is true iff not all are equal.
    types.windows(2).any(|w| w[0] != w[1])
}

/// Build the concrete per-case `Field` for one union case from a variant choice.
/// Object cases carry their nested `fields` so the case generates a full struct.
fn build_union_case_field(choice: &FieldVariant) -> Field {
    Field {
        // A case label, when given, names the union child / superset sub-field; otherwise
        // `lower_heterogeneous_unions` fills a positional `<field>_<i>` name.
        name: choice.name.clone().unwrap_or_default(),
        field_type: infer_field_type(choice),
        generator: choice.generator.clone(),
        locale: choice.locale.clone(),
        range: choice.range,
        value: choice.value.clone(),
        fields: choice.fields.clone(),
        parquet: choice.parquet.clone(),
        ..Default::default()
    }
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
/// The single concrete type shared by a same-type variant's cases, or `None` if the cases
/// disagree (which means it should have lowered to a union — see `is_heterogeneous`).
pub(crate) fn unified_variant_type(choices: &[FieldVariant]) -> Option<FieldType> {
    let types: Vec<_> = choices.iter().filter_map(infer_field_type).collect();
    if types.is_empty() {
        None
    } else if types.windows(2).all(|w| w[0] == w[1]) {
        types.into_iter().next()
    } else {
        None
    }
}

/// Finalise variant fields after cross-producting case-3 (VAR-UNIFY Phase 2):
///
/// - **Case 3** (`ref` + `variants`): the variants now live in `dataset.variants`, so clear
///   them and keep the `ref` stub for `resolve_refs`.
/// - **Same-type field variant** (no ref): give it its unified concrete `field_type` (so
///   schema/ref resolution see an ordinary typed field) but **keep its `variants`** — the
///   generator dispatches on the non-empty `variants` to per-row categorical generation.
/// - Recurse into objects.
fn finalize_variant_fields(schema: &mut Schema) {
    for field in schema.iter_mut() {
        if !field.variants.is_empty() {
            if field.refs.is_some() {
                field.variants = vec![];
            } else {
                field.field_type = unified_variant_type(&field.variants);
            }
        } else if matches!(field.field_type, Some(FieldType::Object)) {
            finalize_variant_fields(&mut field.fields);
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
            import: None,
            links: vec![],
            data,
            variants: vec![],
        }
    }

    #[test]
    fn single_same_type_variant_stays_per_row() {
        // VAR-UNIFY Phase 2: a same-type variant field no longer cross-products into
        // `dataset.variants`; it keeps its cases (typed) for per-row generation.
        let ds = bare_ds(vec![make_variant_field(
            "status",
            vec![
                string_choice("active", Some(0.6)),
                string_choice("inactive", Some(0.4)),
            ],
        )]);

        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/test.yaml"), ds);
        let result = expand_field_variants(map).unwrap();
        let ds = result.values().next().unwrap();

        assert!(ds.variants.is_empty(), "no top-level cross-product");
        let status = ds.data.iter().find(|f| f.name == "status").unwrap();
        assert_eq!(
            status.field_type,
            Some(FieldType::String),
            "unified concrete type set for schema/refs"
        );
        assert_eq!(
            status.variants.len(),
            2,
            "cases retained for per-row generation"
        );
    }

    #[test]
    fn two_same_type_variants_stay_independent_per_row() {
        let ds = bare_ds(vec![
            make_variant_field(
                "status",
                vec![
                    string_choice("active", Some(0.6)),
                    string_choice("inactive", Some(0.4)),
                ],
            ),
            make_variant_field(
                "tier",
                vec![
                    string_choice("gold", Some(0.2)),
                    string_choice("silver", Some(0.3)),
                    string_choice("bronze", Some(0.5)),
                ],
            ),
        ]);

        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/test.yaml"), ds);
        let result = expand_field_variants(map).unwrap();
        let ds = result.values().next().unwrap();

        // Independent per-row columns now — no Cartesian pre-enumeration.
        assert!(ds.variants.is_empty(), "no cross-product");
        assert_eq!(
            ds.data
                .iter()
                .find(|f| f.name == "status")
                .unwrap()
                .variants
                .len(),
            2
        );
        assert_eq!(
            ds.data
                .iter()
                .find(|f| f.name == "tier")
                .unwrap()
                .variants
                .len(),
            3
        );
    }

    #[test]
    fn free_distributions_retained_for_per_row_resolution() {
        // Distributions are now resolved at generation time (per-row); expand just keeps the
        // cases as-is, ratios unset.
        let ds = bare_ds(vec![make_variant_field(
            "x",
            vec![
                FieldVariant {
                    field_type: Some(FieldType::String),
                    ratio: None,
                    ..Default::default()
                },
                FieldVariant {
                    field_type: Some(FieldType::String),
                    ratio: None,
                    ..Default::default()
                },
                FieldVariant {
                    field_type: Some(FieldType::String),
                    ratio: None,
                    ..Default::default()
                },
            ],
        )]);
        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/test.yaml"), ds);
        let result = expand_field_variants(map).unwrap();
        let ds = result.values().next().unwrap();
        assert!(ds.variants.is_empty());
        let x = ds.data.iter().find(|f| f.name == "x").unwrap();
        assert_eq!(x.variants.len(), 3);
        assert!(
            x.variants.iter().all(|v| v.ratio.is_none()),
            "ratios resolved at generation time, not expansion"
        );
    }

    #[test]
    fn nested_same_type_variant_stays_per_row() {
        let nested_variant = make_variant_field(
            "priority",
            vec![
                string_choice("high", Some(0.3)),
                string_choice("low", Some(0.7)),
            ],
        );
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

        assert!(ds.variants.is_empty());
        let meta = ds.data.iter().find(|f| f.name == "metadata").unwrap();
        let pr = meta.fields.iter().find(|f| f.name == "priority").unwrap();
        assert_eq!(
            pr.field_type,
            Some(FieldType::String),
            "unified type set on the nested variant"
        );
        assert_eq!(
            pr.variants.len(),
            2,
            "nested cases retained for per-row gen"
        );
    }

    #[test]
    fn range_only_variant_infers_number_type() {
        let choice = FieldVariant {
            range: Some(Range {
                min: Some(1.0),
                max: Some(10.0),
            }),
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
        let outer_parquet = ParquetConfig {
            datatype: ParquetDatatype::Int32,
        };
        let choice = FieldVariant {
            field_type: Some(FieldType::Number),
            range: Some(Range {
                min: Some(0.0),
                max: Some(100.0),
            }),
            ratio: Some(1.0),
            parquet: None,
            ..Default::default()
        };
        let delta = build_delta_field(&["amount".to_string()], &choice, Some(&outer_parquet), None);
        assert_eq!(
            delta.parquet.as_ref().map(|p| &p.datatype),
            Some(&ParquetDatatype::Int32)
        );
    }

    #[test]
    fn inner_parquet_overrides_outer() {
        use crate::models::{ParquetConfig, ParquetDatatype};
        let outer_parquet = ParquetConfig {
            datatype: ParquetDatatype::Int32,
        };
        let inner_parquet = ParquetConfig {
            datatype: ParquetDatatype::Float32,
        };
        let choice = FieldVariant {
            field_type: Some(FieldType::Number),
            ratio: Some(1.0),
            parquet: Some(inner_parquet),
            ..Default::default()
        };
        let delta = build_delta_field(&["x".to_string()], &choice, Some(&outer_parquet), None);
        assert_eq!(
            delta.parquet.as_ref().map(|p| &p.datatype),
            Some(&ParquetDatatype::Float32)
        );
    }

    // --- VAR-1 PR 3: heterogeneous variants lower to FieldType::Union ---

    fn number_choice(dist: Option<f64>) -> FieldVariant {
        FieldVariant {
            field_type: Some(FieldType::Number),
            range: Some(Range {
                min: Some(0.0),
                max: Some(10.0),
            }),
            ratio: dist,
            ..Default::default()
        }
    }

    fn expand_one(field: Field) -> SyntheticDataset {
        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/test.yaml"), bare_ds(vec![field]));
        let result = expand_field_variants(map).unwrap();
        result.into_values().next().unwrap()
    }

    #[test]
    fn heterogeneous_variant_lowers_to_union() {
        let ds = expand_one(make_variant_field(
            "payload",
            vec![string_choice("x", Some(0.5)), number_choice(Some(0.5))],
        ));
        // A union is a per-row column, NOT a dataset cross-product.
        assert!(
            ds.variants.is_empty(),
            "heterogeneous variant must not become global variants"
        );
        let f = ds.data.iter().find(|f| f.name == "payload").unwrap();
        assert_eq!(f.field_type, Some(FieldType::Union));
        assert_eq!(f.union_cases.len(), 2);
        assert_eq!(f.union_cases[0].field.field_type, Some(FieldType::String));
        assert_eq!(f.union_cases[1].field.field_type, Some(FieldType::Number));
        assert_eq!(f.union_cases[0].ratio, Some(0.5));
        assert!(f.variants.is_empty(), "variants cleared after lowering");
    }

    #[test]
    fn lowered_union_generates_denseunion_in_memory() {
        let ds = expand_one(make_variant_field(
            "payload",
            vec![string_choice("x", Some(0.5)), number_choice(Some(0.5))],
        ));
        let f = ds.data.iter().find(|f| f.name == "payload").unwrap();

        // Per-row categorical sampling over two 0.5 cases → ~500/500 (not exact).
        let arr = crate::generator::generate_column(f, 1000, &[]).unwrap();
        let u = arr
            .as_any()
            .downcast_ref::<arrow::array::UnionArray>()
            .expect("a UnionArray");
        assert_eq!(u.type_ids().len(), 1000);
        let mut hist = std::collections::BTreeMap::new();
        for &t in u.type_ids().iter() {
            *hist.entry(t).or_insert(0) += 1;
        }
        assert!(
            (400..=600).contains(hist.get(&0).unwrap_or(&0)),
            "string case ≈ 500"
        );
        assert!(
            (400..=600).contains(hist.get(&1).unwrap_or(&0)),
            "number case ≈ 500"
        );
    }

    #[test]
    fn homogeneous_variant_generates_per_row() {
        // Same-type variants generate per-row (VAR-UNIFY Phase 2): no cross-product, no
        // union; the field keeps a unified scalar type + its cases.
        let ds = expand_one(make_variant_field(
            "status",
            vec![string_choice("a", Some(0.5)), string_choice("b", Some(0.5))],
        ));
        assert!(
            ds.variants.is_empty(),
            "no cross-product for same-type variants"
        );
        let f = ds.data.iter().find(|f| f.name == "status").unwrap();
        assert_eq!(f.field_type, Some(FieldType::String));
        assert!(f.union_cases.is_empty(), "same-type → not a union");
        assert_eq!(f.variants.len(), 2, "cases retained for per-row generation");
    }

    // --- VAR-1 PR 3.5: object-schema cases ---

    /// An object case carrying a single nested string field `sub`.
    fn object_choice(sub: &str, dist: Option<f64>) -> FieldVariant {
        FieldVariant {
            field_type: Some(FieldType::Object),
            fields: vec![Field {
                name: sub.into(),
                field_type: Some(FieldType::String),
                ..Default::default()
            }],
            ratio: dist,
            ..Default::default()
        }
    }

    #[test]
    fn is_heterogeneous_flags_object_and_mixed_scalar() {
        // Two object schemas → union (FieldType::Object alone can't tell them apart).
        assert!(is_heterogeneous(&[
            object_choice("a", None),
            object_choice("b", None)
        ]));
        // Mixed scalar → union.
        assert!(is_heterogeneous(&[
            string_choice("x", None),
            number_choice(None)
        ]));
        // Same scalar type → not a union.
        assert!(!is_heterogeneous(&[
            string_choice("a", None),
            string_choice("b", None)
        ]));
    }

    #[test]
    fn object_schema_variant_lowers_to_union() {
        // The supplier-form shape: two different object schemas → a union of struct cases.
        let ds = expand_one(make_variant_field(
            "form",
            vec![
                object_choice("risk_level", Some(0.5)),
                object_choice("standard_code", Some(0.5)),
            ],
        ));
        assert!(
            ds.variants.is_empty(),
            "object union is not a global variant"
        );
        let f = ds.data.iter().find(|f| f.name == "form").unwrap();
        assert_eq!(f.field_type, Some(FieldType::Union));
        assert_eq!(f.union_cases.len(), 2);
        assert_eq!(f.union_cases[0].field.field_type, Some(FieldType::Object));
        assert_eq!(f.union_cases[0].field.fields[0].name, "risk_level");
        assert_eq!(f.union_cases[1].field.fields[0].name, "standard_code");

        // Generates a DenseUnion whose children are structs.
        let arr = crate::generator::generate_column(f, 8, &[]).unwrap();
        let u = arr
            .as_any()
            .downcast_ref::<arrow::array::UnionArray>()
            .expect("a UnionArray");
        assert_eq!(u.type_ids().len(), 8);
    }
}
