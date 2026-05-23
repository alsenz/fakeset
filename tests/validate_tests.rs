use fakeset::{expressions::pull_down_expression_deps, load_all_datasets, validate::validate};
use std::path::PathBuf;

#[test]
fn test_rows_with_distribution_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/rows_with_distribution")];
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
    assert!(warnings.is_empty(), "expected no warnings; got: {warnings:?}");
}

#[test]
fn test_fields_on_non_object_field_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/schema_on_non_object")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("fields"), "error should mention `fields`: {msg}");
    assert!(msg.contains("object"), "error should mention `object`: {msg}");
}

#[test]
fn test_content_on_non_list_field_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/content_on_non_list")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("content"), "error should mention `content`: {msg}");
    assert!(msg.contains("list"), "error should mention `list`: {msg}");
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
    let paths = vec![PathBuf::from("tests/fixtures/validation/ref_missing_include")];
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
    assert!(warnings.is_empty(), "expected no warnings; got: {warnings:?}");
}

#[test]
fn test_generator_type_mismatch_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/generator_type_mismatch")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("generator"), "error should mention `generator`: {msg}");
    assert!(msg.contains("first_name"), "error should name the generator: {msg}");
    assert!(msg.contains("number"), "error should mention the type: {msg}");
}


#[test]
fn test_valid_refs_passes_validation() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/valid_refs")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let warnings = validate(&datasets).expect("should not error");
    assert!(warnings.is_empty(), "expected no warnings; got: {warnings:?}");
}

#[test]
fn test_valid_min_max_passes() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/valid_min_max")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let warnings = validate(&datasets).expect("should not error");
    assert!(warnings.is_empty(), "expected no warnings; got: {warnings:?}");
}

#[test]
fn test_min_max_on_non_number_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/min_max_on_string")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("number"), "error should mention `number`: {msg}");
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
    assert!(warnings.is_empty(), "expected no warnings; got: {warnings:?}");
}

#[test]
fn test_value_with_generator_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/value_with_generator")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("value"), "error should mention `value`: {msg}");
    assert!(msg.contains("generator"), "error should mention `generator`: {msg}");
}

#[test]
fn test_value_with_min_max_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/value_with_min_max")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
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
    assert!(warnings.is_empty(), "expected no warnings; got: {warnings:?}");
}

#[test]
fn test_expression_with_type_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/expression_with_type")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("expression"), "error should mention `expression`: {msg}");
    assert!(msg.contains("type"), "error should mention `type`: {msg}");
}

#[test]
fn test_expression_with_ref_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/expression_with_ref")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("expression"), "error should mention `expression`: {msg}");
    assert!(msg.contains("ref"), "error should mention `ref`: {msg}");
}

#[test]
fn test_expression_with_value_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/expression_with_value")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("expression"), "error should mention `expression`: {msg}");
    assert!(msg.contains("value"), "error should mention `value`: {msg}");
}

#[test]
fn test_expression_with_generator_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/expression_with_generator")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("expression"), "error should mention `expression`: {msg}");
    assert!(msg.contains("generator"), "error should mention `generator`: {msg}");
}

#[test]
fn test_expression_with_min_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/expression_with_min")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    let err = validate(&datasets).expect_err("should fail validation");
    let msg = err.to_string();
    assert!(msg.contains("expression"), "error should mention `expression`: {msg}");
    assert!(msg.contains("range"), "error should mention `range`: {msg}");
}

#[test]
fn test_expression_forward_ref_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/expression_forward_ref")];
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
// Rich list content validation
// ---------------------------------------------------------------------------

#[test]
fn test_link_content_include_scoped_ref_with_type_errors() {
    let paths =
        vec![PathBuf::from("tests/fixtures/validation/link_content_include_scoped_with_type")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("include-scoped ref with type: should error");
    let msg = err.to_string();
    assert!(
        msg.contains("pool-scoped"),
        "error should mention 'pool-scoped': {msg}"
    );
    assert!(msg.contains("type"), "error should mention 'type': {msg}");
}

#[test]
fn test_link_content_outer_scoped_ref_without_type_errors() {
    let paths =
        vec![PathBuf::from("tests/fixtures/validation/link_content_outer_scoped_no_type")];
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
fn test_link_content_outer_scoped_missing_outer_field_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/link_content_outer_scoped_missing_field",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err =
        validate(&datasets).expect_err("outer-scoped ref to missing field should error");
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
fn test_link_content_include_scoped_missing_target_field_errors() {
    let paths = vec![PathBuf::from(
        "tests/fixtures/validation/link_content_include_scoped_missing_field",
    )];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err =
        validate(&datasets).expect_err("include-scoped ref to missing field should error");
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
fn test_link_content_expression_in_content_errors() {
    let paths =
        vec![PathBuf::from("tests/fixtures/validation/link_content_expression_in_content")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("expression inside link content should error");
    let msg = err.to_string();
    assert!(
        msg.contains("expression"),
        "error should mention 'expression': {msg}"
    );
    assert!(
        msg.contains("nested include"),
        "error should mention 'nested include': {msg}"
    );
}

// ---------------------------------------------------------------------------
// Variant distribution tests
// ---------------------------------------------------------------------------

#[test]
fn test_variant_distributions_sum_over_one_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/variant_bad_sum")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("distributions > 1.0 should error");
    let msg = err.to_string();
    assert!(msg.contains("variant"), "error should mention 'variant': {msg}");
    assert!(msg.contains("exceeds 1.0") || msg.contains("sum"), "error should mention the sum: {msg}");
}

#[test]
fn test_variant_all_set_not_summing_to_one_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/variant_all_set_wrong")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("fully-specified variants not summing to 1.0 should error");
    let msg = err.to_string();
    assert!(msg.contains("variant"), "error should mention 'variant': {msg}");
}

#[test]
fn test_variant_valid_mixed_distributions_passes() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/variant_valid")];
    let datasets = load_all_datasets(&paths).expect("should load");
    validate(&datasets).expect("valid variant distribution should pass");
}

// ---------------------------------------------------------------------------
// Field-local variant validation tests
// ---------------------------------------------------------------------------

#[test]
fn test_field_variant_empty_variants_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/field_variant_empty")];
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
    let paths = vec![PathBuf::from("tests/fixtures/validation/field_variant_bad_sum")];
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
    let paths = vec![PathBuf::from("tests/fixtures/validation/field_variant_nested_variant")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("nested type:variant inside a variant choice should error");
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

// ---------------------------------------------------------------------------
// MULT-2a cardinality / links / count validation tests
// ---------------------------------------------------------------------------

#[test]
fn test_count_on_nested_include_list_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/count_on_nested_include_list")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("count on nested-include list should error");
    let msg = err.to_string();
    assert!(msg.contains("cardinality"), "error should mention 'cardinality': {msg}");
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
    let paths = vec![PathBuf::from("tests/fixtures/validation/junction_link_cardinality")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("junction link with cardinality should error");
    let msg = err.to_string();
    assert!(msg.contains("junction"), "error should mention 'junction': {msg}");
    assert!(msg.contains("cardinality"), "error should mention 'cardinality': {msg}");
}

#[test]
fn test_cardinality_min_zero_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/cardinality_min_zero")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("cardinality min: 0 should error");
    let msg = err.to_string();
    assert!(msg.contains("cardinality"), "error should mention 'cardinality': {msg}");
    assert!(msg.contains('0'), "error should show the bad value: {msg}");
}

#[test]
fn test_rows_with_include_cardinality_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/rows_with_cardinality")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("rows + include.cardinality should error");
    let msg = err.to_string();
    assert!(msg.contains("rows"), "error should mention 'rows': {msg}");
    assert!(msg.contains("cardinality"), "error should mention 'cardinality': {msg}");
}

// ---------------------------------------------------------------------------
// MULT-2 Stage 2: collect bindings and default type compatibility
// ---------------------------------------------------------------------------

#[test]
fn test_collect_bind_not_list_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/collect_bind_not_list")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("collect bind to non-list field should error");
    let msg = err.to_string();
    assert!(msg.contains("list"), "error should mention 'list': {msg}");
    assert!(msg.contains("collect"), "error should mention 'collect': {msg}");
}

#[test]
fn test_collect_bind_no_default_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/collect_bind_no_default")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("collect bind to field without default should error");
    let msg = err.to_string();
    assert!(msg.contains("default"), "error should mention 'default': {msg}");
    assert!(msg.contains("collect"), "error should mention 'collect': {msg}");
}

#[test]
fn test_default_type_mismatch_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/default_type_mismatch")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("default value with wrong type should error");
    let msg = err.to_string();
    assert!(msg.contains("default"), "error should mention 'default': {msg}");
    assert!(msg.contains("number"), "error should mention 'number': {msg}");
}

#[test]
fn test_include_fields_exclude_without_fields_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/include_fields_exclude_no_fields")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("exclude without fields should error");
    let msg = err.to_string();
    assert!(msg.contains("exclude"), "error should mention 'exclude': {msg}");
    assert!(msg.contains("fields"), "error should mention 'fields': {msg}");
}

#[test]
fn test_project_with_fields_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/project_with_fields")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("project + fields should error");
    let msg = err.to_string();
    assert!(msg.contains("project"), "error should mention 'project': {msg}");
    assert!(msg.contains("fields"), "error should mention 'fields': {msg}");
}

#[test]
fn test_project_ref_mismatch_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/project_ref_mismatch")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("project ref mismatch should error");
    let msg = err.to_string();
    assert!(msg.contains("project"), "error should mention 'project': {msg}");
    assert!(msg.contains("wrong_ref"), "error should mention the mismatched ref: {msg}");
}

#[test]
fn test_project_field_missing_errors() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/project_field_missing")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let err = validate(&datasets).expect_err("project with missing field should error");
    let msg = err.to_string();
    assert!(msg.contains("project"), "error should mention 'project': {msg}");
    assert!(msg.contains("nonexistent_field"), "error should name the missing field: {msg}");
}
