use fakeset::{
    expressions::pull_down_expression_deps, load_all_datasets,
    models::FieldType,
    rewrite::resolve_refs, validate::validate,
};
use std::path::PathBuf;

#[test]
fn test_resolve_refs_propagates_type() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/valid_refs")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("should pass validation");

    let resolved = resolve_refs(&datasets).expect("should resolve refs");

    let a = resolved
        .values()
        .find(|d| d.name == "a")
        .expect("should find dataset 'a'");

    let id_field = a
        .data
        .iter()
        .find(|f| f.name == "employee_id")
        .expect("should find field 'employee_id'");

    assert_eq!(
        id_field.field_type,
        Some(FieldType::String),
        "ref field should inherit `string` type from b.employee_id"
    );

    let score_field = a
        .data
        .iter()
        .find(|f| f.name == "score")
        .expect("should find field 'score'");

    assert_eq!(
        score_field.field_type,
        Some(FieldType::Number),
        "ref field should inherit `number` type from b.score"
    );
}

#[test]
fn test_resolve_refs_preserves_ref_field_string() {
    let paths = vec![PathBuf::from("tests/fixtures/validation/valid_refs")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("should pass validation");

    let resolved = resolve_refs(&datasets).expect("should resolve refs");

    let a = resolved
        .values()
        .find(|d| d.name == "a")
        .expect("should find dataset 'a'");

    let id_field = a
        .data
        .iter()
        .find(|f| f.name == "employee_id")
        .expect("should find field");

    // ref_field is kept so the executor can wire up pre-filled columns.
    assert_eq!(
        id_field.ref_field.as_deref(),
        Some("b_data.employee_id"),
        "ref_field should be preserved after resolution"
    );
}

#[test]
fn test_ref_conflicting_generators_errors_at_rewrite() {
    // a.email refs b.email (generator: email) but overrides with generator: company_name — conflict.
    let paths = vec![PathBuf::from("tests/fixtures/validation/ref_with_generator")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("ref+generator is valid at the validate stage");

    let err = resolve_refs(&datasets).expect_err("conflicting generators should error at rewrite");
    let msg = err.to_string();
    assert!(msg.contains("conflict"), "error should mention conflict: {msg}");
}

#[test]
fn test_ref_specialises_with_value() {
    let paths = vec![PathBuf::from("tests/fixtures/rewrite/ref_with_value")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("should pass validation");

    let resolved = resolve_refs(&datasets).expect("should resolve refs");

    let derived = resolved
        .values()
        .find(|d| d.name == "derived")
        .expect("should find derived");

    let status = derived
        .data
        .iter()
        .find(|f| f.name == "status")
        .expect("should find status field");

    assert_eq!(
        status.field_type,
        Some(FieldType::String),
        "should inherit string type from base.status"
    );
    assert_eq!(
        status.value.as_ref().and_then(|v| v.as_str()),
        Some("active"),
        "should keep local value after merge"
    );
}

#[test]
fn test_ref_to_expression_field_errors_at_rewrite() {
    // derived.full_name refs base.full_name, which is an expression field.
    // resolve_refs should error because expression field types are runtime-inferred.
    let paths = vec![PathBuf::from("tests/fixtures/rewrite/ref_to_expression")];
    let datasets = load_all_datasets(&paths).expect("should load");
    let datasets = pull_down_expression_deps(&datasets).expect("pull_down should succeed");
    validate(&datasets).expect("structural validation should pass");

    let err = resolve_refs(&datasets).expect_err("ref to expression field should error at rewrite");
    assert!(
        err.to_string().contains("expression"),
        "error should mention 'expression': {err}"
    );
}

#[test]
fn test_resolve_refs_for_rich_list_content_field() {
    // events.attendees content has:
    //   name: ref: person.full_name  (include-scoped — type inherited from people.full_name: string)
    //   event_title: type: string, ref: title  (outer-scoped — left as-is)
    // After resolve_refs the include-scoped field should carry field_type: String.
    let paths = vec![PathBuf::from("tests/fixtures/execute/rich_list")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("should pass validation");
    let resolved = resolve_refs(&datasets).expect("should resolve refs");

    let events = resolved.values().find(|d| d.name == "events").expect("find events");
    let attendees = events.data.iter()
        .find(|f| f.name == "attendees")
        .expect("find attendees field");

    let content_fields = match attendees.content.as_deref() {
        Some(c) if !c.includes.is_empty() => &c.item.fields,
        other => panic!("attendees should have rich content, got: {other:?}"),
    };

    let name_field = content_fields.iter().find(|f| f.name == "name").expect("find name field");
    assert_eq!(
        name_field.field_type,
        Some(FieldType::String),
        "include-scoped 'name' should inherit string type from people.full_name"
    );

    let title_field = content_fields.iter().find(|f| f.name == "event_title").expect("find event_title");
    assert_eq!(
        title_field.field_type,
        Some(FieldType::String),
        "outer-scoped 'event_title' should preserve its declared string type"
    );
}

#[test]
fn test_chained_ref_resolves_through_chain() {
    // c.id → b.id → a.id: two hops. The rewrite follows the chain and inherits the
    // base type (string) from a.id, so c.id should end up with field_type: String.
    let paths = vec![PathBuf::from("tests/fixtures/rewrite/chained_ref")];
    let datasets = load_all_datasets(&paths).expect("should load datasets");
    validate(&datasets).expect("chained ref is structurally valid");

    let resolved = resolve_refs(&datasets).expect("chained ref should now resolve successfully");

    let c = resolved.values().find(|d| d.name == "c").expect("find dataset c");
    let id_field = c.data.iter().find(|f| f.name == "id").expect("find c.id");
    assert_eq!(
        id_field.field_type,
        Some(FieldType::String),
        "c.id should inherit string type through the b → a chain"
    );
}