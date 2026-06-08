use fakeset::{expressions::pull_down_expression_deps, load_all_datasets, validate::validate};
use std::path::PathBuf;

#[test]
fn test_rows_with_distribution_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/rows_with_distribution",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("rows"), "error should mention `rows`: {msg}");
    assert!(msg.contains("ratio"), "error should mention `ratio`: {msg}");
}

#[test]
fn test_no_warnings_for_valid_datasets() {
    let paths = vec![PathBuf::from("tests/fixtures/basic")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let warnings = validate(&datasets).expect("should not error");
    assert!(
        warnings.is_empty(),
        "expected no warnings; got: {warnings:?}"
    );
}

#[test]
fn test_fields_on_non_object_field_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/schema_on_non_object",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("fields"),
        "error should mention `fields`: {msg}"
    );
    assert!(
        msg.contains("object"),
        "error should mention `object`: {msg}"
    );
}

#[test]
fn test_content_on_non_list_field_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/content_on_non_list",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("content"),
        "error should mention `content`: {msg}"
    );
    assert!(msg.contains("list"), "error should mention `list`: {msg}");
}

/// VAR-LINKED-CONTENT gate: a `type: variant` field among linked content list item
/// fields is rejected until the feature is designed (see specs/VAR-LINKED-CONTENT.md).
#[test]
fn test_variant_in_list_link_content_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/variant_in_list_link_content",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("variant"),
        "error should mention `variant`: {msg}"
    );
    assert!(
        msg.contains("list-link content"),
        "error should mention `list-link content`: {msg}"
    );
}

#[test]
fn test_ref_with_type_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/ref_with_type")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("ref"), "error should mention `ref`: {msg}");
    assert!(msg.contains("type"), "error should mention `type`: {msg}");
}

#[test]
fn test_ref_missing_include_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/ref_missing_include",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("nonexistent_ref"),
        "error should name the missing include ref: {msg}"
    );
}

#[test]
fn test_ref_missing_field_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/ref_missing_field")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("name"),
        "error should name the missing target field: {msg}"
    );
}

#[test]
fn test_valid_generator_passes() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/valid_generator")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let warnings = validate(&datasets).expect("should not error");
    assert!(
        warnings.is_empty(),
        "expected no warnings; got: {warnings:?}"
    );
}

#[test]
fn test_generator_type_mismatch_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/generator_type_mismatch",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("generator"),
        "error should mention `generator`: {msg}"
    );
    assert!(
        msg.contains("first_name"),
        "error should name the generator: {msg}"
    );
    assert!(
        msg.contains("number"),
        "error should mention the type: {msg}"
    );
}

#[test]
fn test_valid_refs_passes_validation() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/valid_refs")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let warnings = validate(&datasets).expect("should not error");
    assert!(
        warnings.is_empty(),
        "expected no warnings; got: {warnings:?}"
    );
}

#[test]
fn test_valid_min_max_passes() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/valid_min_max")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let warnings = validate(&datasets).expect("should not error");
    assert!(
        warnings.is_empty(),
        "expected no warnings; got: {warnings:?}"
    );
}

#[test]
fn test_min_max_on_non_number_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/min_max_on_string")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("number"),
        "error should mention `number`: {msg}"
    );
}

#[test]
fn test_min_gt_max_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/min_gt_max")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("min"), "error should mention `min`: {msg}");
    assert!(msg.contains("max"), "error should mention `max`: {msg}");
}

#[test]
fn test_valid_value_passes() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/valid_value")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let warnings = validate(&datasets).expect("should not error");
    assert!(
        warnings.is_empty(),
        "expected no warnings; got: {warnings:?}"
    );
}

#[test]
fn test_value_with_generator_specialises() {
    // VAR-SPECIALIZE S1: `value` + `generator` is no longer a conflict — `value` is the
    // tightest point on the value-source spectrum and supersedes the generator.
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/value_with_generator",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("value specialising a generator should pass validation");
}

#[test]
fn test_value_within_range_passes() {
    // VAR-SPECIALIZE S1: `value` + `range` is fine when the constant lies within the bounds.
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/value_with_min_max",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("a value within its range should pass validation");
}

#[test]
fn test_value_outside_range_errors() {
    // The one real numeric error: a constant `value` outside its declared range.
    let paths = vec![PathBuf::from("tests/fixtures/validation/value_below_range")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("a value outside its range should fail");
    let msg = err.to_string();
    assert!(msg.contains("value"), "error should mention `value`: {msg}");
    assert!(msg.contains("range"), "error should mention `range`: {msg}");
}

#[test]
fn test_valid_expression_passes_validation() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/valid_expression")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let datasets = pull_down_expression_deps(&datasets).expect("pull_down should succeed");
    let warnings = validate(&datasets).expect("valid expression should pass validation");
    assert!(
        warnings.is_empty(),
        "expected no warnings; got: {warnings:?}"
    );
}

#[test]
fn test_expression_with_type_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/expression_with_type",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("expression"),
        "error should mention `expression`: {msg}"
    );
    assert!(msg.contains("type"), "error should mention `type`: {msg}");
}

#[test]
fn test_ref_expression_plus_value_errors() {
    // EXPR-RELOCATE PR2: `ref` + `expression` is now allowed (a computed shared column), but it
    // is a single value-source — adding `value` alongside it must error.
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/expression_with_ref",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("value-source"),
        "error should mention the single value-source rule: {msg}"
    );
}

#[test]
fn test_expression_with_value_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/expression_with_value",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("expression"),
        "error should mention `expression`: {msg}"
    );
    assert!(msg.contains("value"), "error should mention `value`: {msg}");
}

#[test]
fn test_expression_with_generator_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/expression_with_generator",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("expression"),
        "error should mention `expression`: {msg}"
    );
    assert!(
        msg.contains("generator"),
        "error should mention `generator`: {msg}"
    );
}

#[test]
fn test_expression_with_min_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/expression_with_min",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("expression"),
        "error should mention `expression`: {msg}"
    );
    assert!(msg.contains("range"), "error should mention `range`: {msg}");
}

#[test]
fn test_expression_forward_ref_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/expression_forward_ref",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let datasets = pull_down_expression_deps(&datasets).expect("pull_down should succeed");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("age"),
        "error should name the forward-referenced field 'age': {msg}"
    );
}

// ---------------------------------------------------------------------------
// List-link content validation
// ---------------------------------------------------------------------------

#[test]
fn test_list_link_include_scoped_ref_with_type_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/list_link_include_scoped_with_type",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("linked-scoped ref with type: should error");
    let msg = err.to_string();
    assert!(
        msg.contains("linked-scoped"),
        "error should mention 'linked-scoped': {msg}"
    );
    assert!(msg.contains("type"), "error should mention 'type': {msg}");
}

#[test]
fn test_list_link_outer_scoped_ref_without_type_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/list_link_outer_scoped_no_type",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("outer-scoped ref without type: should error");
    let msg = err.to_string();
    assert!(
        msg.contains("outer-scoped"),
        "error should mention 'outer-scoped': {msg}"
    );
    assert!(msg.contains("type"), "error should mention 'type': {msg}");
}

#[test]
fn test_list_link_outer_scoped_missing_outer_field_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/list_link_outer_scoped_missing_field",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("outer-scoped ref to missing field should error");
    let msg = err.to_string();
    assert!(
        msg.contains("ghost_field"),
        "error should name the missing field 'ghost_field': {msg}"
    );
    assert!(
        msg.contains("does not exist"),
        "error should say 'does not exist': {msg}"
    );
}

#[test]
fn test_list_link_include_scoped_missing_target_field_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/list_link_include_scoped_missing_field",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("include-scoped ref to missing field should error");
    let msg = err.to_string();
    assert!(
        msg.contains("nonexistent"),
        "error should name the missing target field 'nonexistent': {msg}"
    );
    assert!(
        msg.contains("does not exist"),
        "error should say 'does not exist': {msg}"
    );
}

#[test]
fn test_list_link_expression_in_content_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/list_link_expression_in_content",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("expression inside link content should error");
    let msg = err.to_string();
    assert!(
        msg.contains("expression"),
        "error should mention 'expression': {msg}"
    );
    assert!(
        msg.contains("list-link") || msg.contains("nested include"),
        "error should mention 'list-link' or 'nested include': {msg}"
    );
}

// ---------------------------------------------------------------------------
// Variant distribution tests
// ---------------------------------------------------------------------------

#[test]
fn test_top_level_variants_rejected() {
    // VAR-UNIFY U4: top-level dataset `variants:` is retired. It is no longer a field on
    // `SyntheticDataset`, so `#[serde(deny_unknown_fields)]` rejects the key at *load* time.
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/top_level_variants_retired",
    )];
    let err = load_all_datasets(&paths)
        .expect_err("top-level `variants:` should be rejected at load (deny_unknown_fields)");
    let msg = err.to_string();
    assert!(
        msg.contains("variants"),
        "error should name the unknown `variants` key: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Field-local variant validation tests
// ---------------------------------------------------------------------------

#[test]
fn test_field_variant_empty_variants_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/field_variant_empty",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("empty variants list should error");
    let msg = err.to_string();
    assert!(
        msg.contains("variant") && msg.contains("empty"),
        "error should mention 'variant' and 'empty': {msg}"
    );
}

#[test]
fn test_field_variant_bad_distribution_sum_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/field_variant_bad_sum",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("distributions > 1.0 should error");
    let msg = err.to_string();
    assert!(
        msg.contains("variant") || msg.contains("distribution"),
        "error should mention 'variant' or 'distribution': {msg}"
    );
}

#[test]
fn test_field_variant_nested_variant_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/field_variant_nested_variant",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err =
        validate(&datasets).expect_err("nested type:variant inside a variant choice should error");
    let msg = err.to_string();
    assert!(
        msg.contains("variant"),
        "error should mention 'variant': {msg}"
    );
}

#[test]
fn test_valid_field_variant_passes() {
    let paths = vec![PathBuf::from("tests/fixtures/execute/field_variants")];
    let datasets = load_all_datasets(&paths).expect("should load");
    validate(&datasets).expect("valid field variant config should pass validation");
}

// VAR-1: a heterogeneous (multi-type) variant is supported (lowers to a union), EXCEPT
// for CSV output, which can't hold the resulting nested struct — that's a clean
// validation error (not a write-time failure). The fixture is `format: csv`.
#[test]
fn test_variant_mixed_types_csv_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/variant_mixed_types",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("mixed-type variant to CSV should fail validation");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("csv"),
        "error should mention the CSV limitation: {msg}"
    );
    assert!(
        msg.contains("VAR-1"),
        "error should point at the VAR-1 spec: {msg}"
    );
}

// VAR-1: the same heterogeneous variant validates fine for a struct-capable format (json).
#[test]
fn test_variant_mixed_types_json_passes() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/variant_mixed_types_json",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    validate(&datasets).expect("mixed-type variant to a struct-capable format should validate");
}

// ---------------------------------------------------------------------------
// MULT-2a cardinality / links / count validation tests
// ---------------------------------------------------------------------------

#[test]
fn test_count_on_nested_include_list_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/count_on_nested_include_list",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("count on nested-include list should error");
    let msg = err.to_string();
    assert!(
        msg.contains("cardinality"),
        "error should mention 'cardinality': {msg}"
    );
    assert!(msg.contains("count"), "error should mention 'count': {msg}");
}

#[test]
fn test_junction_link_errors() {
    // Junction links without cardinality are valid since Stage 4.
    let paths = vec![PathBuf::from("tests/fixtures/validation/include_couple")];
    let datasets = load_all_datasets(&paths).expect("should load");
    validate(&datasets).expect("junction link without cardinality should now be valid");
}

#[test]
fn test_junction_link_cardinality_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/junction_link_cardinality",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("junction link with cardinality should error");
    let msg = err.to_string();
    assert!(
        msg.contains("junction"),
        "error should mention 'junction': {msg}"
    );
    assert!(
        msg.contains("cardinality"),
        "error should mention 'cardinality': {msg}"
    );
}

#[test]
fn test_include_reinforcement_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/include_reinforcement",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("reinforcement on include: should error");
    let msg = err.to_string();
    assert!(
        msg.contains("reinforcement"),
        "error should mention 'reinforcement': {msg}"
    );
    assert!(msg.contains("links"), "error should mention 'links': {msg}");
}

#[test]
fn test_link_reinforcement_invalid_value_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/link_reinforcement_invalid",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("reinforcement in (0,1) on a link should error");
    let msg = err.to_string();
    assert!(
        msg.contains("reinforcement"),
        "error should mention 'reinforcement': {msg}"
    );
}

#[test]
fn test_include_overlap_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/include_overlap")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("overlap on include: should error");
    let msg = err.to_string();
    assert!(
        msg.contains("overlap"),
        "error should mention 'overlap': {msg}"
    );
    assert!(msg.contains("links"), "error should mention 'links': {msg}");
}

#[test]
fn test_link_overlap_invalid_value_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/link_overlap_invalid",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("overlap in (0,1) on a link should error");
    let msg = err.to_string();
    assert!(
        msg.contains("overlap"),
        "error should mention 'overlap': {msg}"
    );
}

#[test]
fn test_cardinality_min_zero_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/cardinality_min_zero",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("cardinality min: 0 should error");
    let msg = err.to_string();
    assert!(
        msg.contains("cardinality"),
        "error should mention 'cardinality': {msg}"
    );
    assert!(msg.contains('0'), "error should show the bad value: {msg}");
}

#[test]
fn test_rows_with_include_cardinality_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/rows_with_cardinality",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("rows + include.cardinality should error");
    let msg = err.to_string();
    assert!(msg.contains("rows"), "error should mention 'rows': {msg}");
    assert!(
        msg.contains("cardinality"),
        "error should mention 'cardinality': {msg}"
    );
}

// ---------------------------------------------------------------------------
// MULT-2 Stage 2: collect bindings and default type compatibility
// ---------------------------------------------------------------------------

#[test]
fn test_collect_bind_not_list_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/collect_bind_not_list",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("collect bind to non-list field should error");
    let msg = err.to_string();
    assert!(msg.contains("list"), "error should mention 'list': {msg}");
    assert!(
        msg.contains("collect"),
        "error should mention 'collect': {msg}"
    );
}

#[test]
fn test_collect_bind_no_default_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/collect_bind_no_default",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("collect bind to field without default should error");
    let msg = err.to_string();
    assert!(
        msg.contains("default"),
        "error should mention 'default': {msg}"
    );
    assert!(
        msg.contains("collect"),
        "error should mention 'collect': {msg}"
    );
}

#[test]
fn test_default_type_mismatch_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/default_type_mismatch",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("default value with wrong type should error");
    let msg = err.to_string();
    assert!(
        msg.contains("default"),
        "error should mention 'default': {msg}"
    );
    assert!(
        msg.contains("number"),
        "error should mention 'number': {msg}"
    );
}

#[test]
fn test_include_fields_exclude_without_fields_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/include_fields_exclude_no_fields",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("exclude without fields should error");
    let msg = err.to_string();
    assert!(
        msg.contains("exclude"),
        "error should mention 'exclude': {msg}"
    );
    assert!(
        msg.contains("fields"),
        "error should mention 'fields': {msg}"
    );
}

#[test]
fn test_project_with_fields_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/project_with_fields",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("project + fields should error");
    let msg = err.to_string();
    assert!(
        msg.contains("project"),
        "error should mention 'project': {msg}"
    );
    assert!(
        msg.contains("fields"),
        "error should mention 'fields': {msg}"
    );
}

#[test]
fn test_project_ref_mismatch_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/project_ref_mismatch",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("project ref mismatch should error");
    let msg = err.to_string();
    assert!(
        msg.contains("project"),
        "error should mention 'project': {msg}"
    );
    assert!(
        msg.contains("wrong_ref"),
        "error should mention the mismatched ref: {msg}"
    );
}

#[test]
fn test_project_field_missing_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/project_field_missing",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("project with missing field should error");
    let msg = err.to_string();
    assert!(
        msg.contains("project"),
        "error should mention 'project': {msg}"
    );
    assert!(
        msg.contains("nonexistent_field"),
        "error should name the missing field: {msg}"
    );
}

// --- PROJECT-FIELD: bare `project` validation ---

#[test]
fn test_bare_project_with_fields_accepted() {
    let paths = vec![PathBuf::from("tests/fixtures/execute/project_bare")];
    let datasets = load_all_datasets(&paths).expect("should load");
    validate(&datasets).expect("bare project alongside fields should validate");
}

#[test]
fn test_bare_project_missing_field_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/project_bare_missing",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("bare project naming a missing field should error");
    let msg = err.to_string();
    assert!(
        msg.contains("project") && msg.contains("nonexistent"),
        "error should name the missing projected field: {msg}"
    );
}

#[test]
fn test_bare_project_nonscalar_field_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/project_bare_nonscalar",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("bare project of a non-scalar field should error");
    let msg = err.to_string();
    assert!(
        msg.contains("scalar"),
        "error should mention the scalar requirement: {msg}"
    );
}

// --- VAR-UNIFY PR U1: `flatten` validation ---

#[test]
fn test_flatten_on_scalar_field_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/flatten_on_scalar")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("flatten on a scalar should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("flatten"),
        "error should mention `flatten`: {msg}"
    );
    assert!(
        msg.contains("object") && msg.contains("variant"),
        "error should name the valid field types: {msg}"
    );
}

#[test]
fn test_flatten_sibling_collision_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/flatten_collision_sibling",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("flatten colliding with a sibling should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("city"),
        "error should name the colliding field: {msg}"
    );
    assert!(
        msg.contains("collides"),
        "error should mention the collision: {msg}"
    );
}

#[test]
fn test_flatten_cross_case_collision_parquet_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/flatten_collision_parquet",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err =
        validate(&datasets).expect_err("cross-case flatten collision under parquet should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("amount"),
        "error should name the colliding field: {msg}"
    );
    assert!(
        msg.contains("variant case") || msg.contains("superset"),
        "error should explain the cross-case superset collision: {msg}"
    );
}

#[test]
fn test_flatten_json_cross_case_collision_ok() {
    // The same cross-case `amount` collision is harmless for JSON (per-row keys), and a
    // collision-free flatten object also passes.
    let paths = vec![PathBuf::from("tests/fixtures/validation/flatten_ok_json")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("flatten over jsonl with per-row keys should pass validation");
}

#[test]
fn test_flatten_nested_not_supported_errors() {
    // VAR-UNIFY U2 scope: only top-level flatten is implemented; a nested flatten errors
    // rather than silently emitting nested output.
    let paths = vec![PathBuf::from("tests/fixtures/validation/flatten_nested")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("nested flatten should fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("flatten"),
        "error should mention `flatten`: {msg}"
    );
    assert!(
        msg.contains("top-level") || msg.contains("nested"),
        "error should explain the top-level-only scope: {msg}"
    );
}

#[test]
fn test_flatten_prefixed_resolves_cross_case_collision() {
    // VAR-UNIFY U3: `prefixed` namespaces colliding case fields, so the parquet superset
    // that would otherwise be rejected now validates.
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/flatten_prefixed_parquet",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("prefixed strategy should resolve the cross-case collision");
}

#[test]
fn test_flatten_strategy_on_object_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/flatten_strategy_misplaced",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("flatten_strategy on an object should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("flatten_strategy"),
        "error should mention `flatten_strategy`: {msg}"
    );
}

#[test]
fn test_ref_variants_object_case_errors() {
    // VAR-SPECIALIZE S3: a `ref` + `variants` (case-3) case must be value-source-only;
    // an object case is rejected.
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/ref_variants_object_case",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("object case in a ref+variants field should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("object case") || msg.contains("scalar inherited"),
        "error should explain the value-source-only rule: {msg}"
    );
}

#[test]
fn test_value_and_one_of_errors() {
    // VAR-SPECIALIZE S2: `value` and `one_of` are mutually exclusive.
    let paths = vec![PathBuf::from("tests/fixtures/validation/value_and_one_of")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("value + one_of should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("value") && msg.contains("one_of"),
        "error should mention both: {msg}"
    );
}

#[test]
fn test_one_of_type_mismatch_errors() {
    // VAR-SPECIALIZE S2: `one_of` entries must match the field type.
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/one_of_type_mismatch",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("type-mismatched one_of should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("one_of"),
        "error should mention `one_of`: {msg}"
    );
}

#[test]
fn test_constrain_cases_without_ref_errors() {
    // VAR-SPECIALIZE S5: `constrain_cases` only specialises a ref'd parent variant.
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/constrain_cases_no_ref",
    )];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("constrain_cases without a ref should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("constrain_cases"),
        "error should mention `constrain_cases`: {msg}"
    );
}
