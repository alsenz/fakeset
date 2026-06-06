use fakeset::constraints::{FieldConstraints, Merge, Satisfiable};
use fakeset::models::Generator;

fn constraints(
    generator: Option<Generator>,
    min: Option<f64>,
    max: Option<f64>,
    value: Option<serde_yaml::Value>,
) -> FieldConstraints {
    FieldConstraints {
        generator,
        min,
        max,
        value,
        one_of: None,
        case_overrides: vec![],
    }
}

// ---------------------------------------------------------------------------
// Satisfiable
// ---------------------------------------------------------------------------

#[test]
fn test_satisfiable_unconstrained() {
    assert!(constraints(None, None, None, None).satisfiable());
}

#[test]
fn test_satisfiable_valid_range() {
    assert!(constraints(None, Some(0.0), Some(100.0), None).satisfiable());
}

#[test]
fn test_satisfiable_equal_bounds() {
    assert!(constraints(None, Some(5.0), Some(5.0), None).satisfiable());
}

#[test]
fn test_not_satisfiable_inverted_range() {
    assert!(!constraints(None, Some(100.0), Some(1.0), None).satisfiable());
}

#[test]
fn test_satisfiable_numeric_value_within_min() {
    // VAR-SPECIALIZE: a numeric `value` within bounds is satisfiable (value is the tightest
    // source; bounds are a containment check, not a conflict).
    assert!(
        constraints(
            None,
            Some(0.0),
            None,
            Some(serde_yaml::Value::Number(42.into()))
        )
        .satisfiable()
    );
}

#[test]
fn test_not_satisfiable_numeric_value_below_min() {
    assert!(
        !constraints(
            None,
            Some(100.0),
            None,
            Some(serde_yaml::Value::Number(42.into()))
        )
        .satisfiable()
    );
}

#[test]
fn test_satisfiable_value_with_generator() {
    // VAR-SPECIALIZE: `value` supersedes the generator — no longer a conflict.
    assert!(
        constraints(
            Some(Generator::FirstName),
            None,
            None,
            Some(serde_yaml::Value::String("Alice".into()))
        )
        .satisfiable()
    );
}

// ---------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------

#[test]
fn test_merge_two_unconstrained() {
    let a = constraints(None, None, None, None);
    let b = constraints(None, None, None, None);
    let m = a.merge(&b).expect("unconstrained fields always merge");
    assert!(m.satisfiable());
    assert!(m.min.is_none() && m.max.is_none());
}

#[test]
fn test_merge_narrows_bounds() {
    let a = constraints(None, Some(0.0), Some(100.0), None);
    let b = constraints(None, Some(20.0), Some(80.0), None);
    let m = a.merge(&b).expect("should merge");
    assert_eq!(m.min, Some(20.0));
    assert_eq!(m.max, Some(80.0));
}

#[test]
fn test_merge_one_sided_bounds() {
    let a = constraints(None, Some(10.0), None, None);
    let b = constraints(None, None, Some(50.0), None);
    let m = a.merge(&b).expect("should merge");
    assert_eq!(m.min, Some(10.0));
    assert_eq!(m.max, Some(50.0));
}

#[test]
fn test_merge_incompatible_bounds() {
    let a = constraints(None, Some(60.0), Some(100.0), None);
    let b = constraints(None, Some(0.0), Some(30.0), None);
    assert!(
        a.merge(&b).is_none(),
        "merged range is empty — should return None"
    );
}

#[test]
fn test_merge_one_sided_non_overlapping() {
    // a allows [50, ∞), b allows (-∞, 30] — intersection is empty
    let a = constraints(None, Some(50.0), None, None);
    let b = constraints(None, None, Some(30.0), None);
    assert!(
        a.merge(&b).is_none(),
        "min(50) > max(30) after merge — should return None"
    );
}

#[test]
fn test_merge_same_generator() {
    let a = constraints(Some(Generator::Email), None, None, None);
    let b = constraints(Some(Generator::Email), None, None, None);
    let m = a.merge(&b).expect("identical generators should merge");
    assert_eq!(m.generator, Some(Generator::Email));
}

#[test]
fn test_merge_conflicting_generators() {
    let a = constraints(Some(Generator::Email), None, None, None);
    let b = constraints(Some(Generator::Username), None, None, None);
    assert!(a.merge(&b).is_none(), "different generators conflict");
}

#[test]
fn test_merge_generator_with_unconstrained() {
    let a = constraints(Some(Generator::Email), None, None, None);
    let b = constraints(None, None, None, None);
    let m = a.merge(&b).expect("should carry through generator");
    assert_eq!(m.generator, Some(Generator::Email));
}

#[test]
fn test_merge_same_value() {
    let v = serde_yaml::Value::String("active".into());
    let a = constraints(None, None, None, Some(v.clone()));
    let b = constraints(None, None, None, Some(v));
    let m = a.merge(&b).expect("same constant value should merge");
    assert_eq!(m.value, Some(serde_yaml::Value::String("active".into())));
}

#[test]
fn test_merge_conflicting_values() {
    let a = constraints(
        None,
        None,
        None,
        Some(serde_yaml::Value::String("active".into())),
    );
    let b = constraints(
        None,
        None,
        None,
        Some(serde_yaml::Value::String("inactive".into())),
    );
    assert!(a.merge(&b).is_none(), "different constant values conflict");
}
