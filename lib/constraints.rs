//! Field constraint types (`FieldConstraints`, `Satisfiable`, `Merge`) and
//! `validate_field_constraints` — used by the segment pruner and validator to detect
//! contradictory constraint sets (e.g. two lower cover members pinning the same field
//! to different constants).
use anyhow::{bail, Result};
use crate::models::{Field, Generator, Range};
use serde_yaml::Value as YamlValue;

/// A set of constraints has consistent semantics (no internal contradictions).
pub trait Satisfiable {
    fn satisfiable(&self) -> bool;
}

/// Two constraint sets can be narrowed into one. Returns `None` when they conflict.
pub trait Merge: Sized {
    fn merge(&self, other: &Self) -> Option<Self>;
}

/// All generation constraints that live on a field, extracted for merging and
/// satisfiability checks during lower cover segmentation.
#[derive(Debug, Clone, Default)]
pub struct FieldConstraints {
    pub generator: Option<Generator>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub value: Option<YamlValue>,
}

/// Validate the constraint set of a field at `path`, returning detailed error messages.
/// Covers value+generator, value+range, and min>max conflicts.
pub fn validate_field_constraints(path: &str, field: &Field) -> Result<()> {
    let fc = FieldConstraints::from(field);
    if fc.value.is_some() {
        if fc.generator.is_some() {
            bail!(
                "field '{path}': `value` and `generator` cannot both be set \
                 — `value` emits a constant, making the generator redundant"
            );
        }
        if fc.min.is_some() || fc.max.is_some() {
            bail!(
                "field '{path}': `value` and `range` cannot both be set \
                 — `value` emits a constant, making bounds meaningless"
            );
        }
    }
    if let (Some(lo), Some(hi)) = (fc.min, fc.max) {
        if lo > hi {
            bail!(
                "field '{path}': `range.min` ({lo}) must be less than or equal to `range.max` ({hi})"
            );
        }
    }
    Ok(())
}

impl Satisfiable for FieldConstraints {
    fn satisfiable(&self) -> bool {
        // A constant value conflicts with any generative constraint.
        if self.value.is_some() && (self.min.is_some() || self.max.is_some() || self.generator.is_some()) {
            return false;
        }
        // Numeric bounds must form a valid range.
        match (self.min, self.max) {
            (Some(lo), Some(hi)) => lo <= hi,
            _ => true,
        }
    }
}

impl From<&Field> for FieldConstraints {
    fn from(f: &Field) -> Self {
        FieldConstraints {
            generator: f.generator.clone(),
            min: f.range.as_ref().and_then(|r| r.min),
            max: f.range.as_ref().and_then(|r| r.max),
            value: f.value.clone(),
        }
    }
}

impl FieldConstraints {
    /// Convert the numeric bounds in this constraint set to a `Range`, returning `None`
    /// when neither bound is set.
    pub fn to_range(&self) -> Option<Range> {
        if self.min.is_some() || self.max.is_some() {
            Some(Range { min: self.min, max: self.max })
        } else {
            None
        }
    }
}

impl Merge for FieldConstraints {
    /// Combine two constraint sets, narrowing numeric bounds and requiring
    /// generators and constant values to agree. Returns `None` on conflict.
    fn merge(&self, other: &Self) -> Option<Self> {
        let value = merge_equal(&self.value, &other.value)?;
        let generator = merge_equal(&self.generator, &other.generator)?;
        let min = [self.min, other.min].into_iter().flatten().reduce(f64::max);
        let max = [self.max, other.max].into_iter().flatten().reduce(f64::min);
        let merged = FieldConstraints { generator, min, max, value };
        merged.satisfiable().then_some(merged)
    }
}

/// Merge two optional values that must be identical when both are present.
/// Returns `None` on conflict, `Some(merged)` otherwise.
fn merge_equal<T: Clone + PartialEq>(a: &Option<T>, b: &Option<T>) -> Option<Option<T>> {
    match (a, b) {
        (None, None) => Some(None),
        (Some(v), None) | (None, Some(v)) => Some(Some(v.clone())),
        (Some(av), Some(bv)) if av == bv => Some(Some(av.clone())),
        _ => None,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Generator;

    fn bounds(min: Option<f64>, max: Option<f64>) -> FieldConstraints {
        FieldConstraints { min, max, ..Default::default() }
    }

    fn with_gen(g: Generator) -> FieldConstraints {
        FieldConstraints { generator: Some(g), ..Default::default() }
    }

    fn with_value(v: &str) -> FieldConstraints {
        FieldConstraints { value: Some(YamlValue::String(v.into())), ..Default::default() }
    }

    // --- satisfiable ---

    #[test]
    fn empty_is_satisfiable() {
        assert!(FieldConstraints::default().satisfiable());
    }

    #[test]
    fn valid_range_is_satisfiable() {
        assert!(bounds(Some(0.0), Some(100.0)).satisfiable());
    }

    #[test]
    fn equal_bounds_is_satisfiable() {
        assert!(bounds(Some(5.0), Some(5.0)).satisfiable());
    }

    #[test]
    fn inverted_range_is_not_satisfiable() {
        assert!(!bounds(Some(100.0), Some(0.0)).satisfiable());
    }

    #[test]
    fn value_with_min_is_not_satisfiable() {
        let c = FieldConstraints { min: Some(1.0), value: Some(YamlValue::Number(42.into())), ..Default::default() };
        assert!(!c.satisfiable());
    }

    #[test]
    fn value_with_generator_is_not_satisfiable() {
        let c = FieldConstraints { generator: Some(Generator::Email), value: Some(YamlValue::String("x".into())), ..Default::default() };
        assert!(!c.satisfiable());
    }

    // --- merge: unconstrained ---

    #[test]
    fn two_empty_merge_to_empty() {
        let m = FieldConstraints::default().merge(&FieldConstraints::default()).unwrap();
        assert!(m.min.is_none() && m.max.is_none() && m.generator.is_none() && m.value.is_none());
    }

    // --- merge: numeric bounds ---

    #[test]
    fn merge_takes_tighter_lower_bound() {
        // [20, 80] merged with [0, 100] → [20, 80]
        let m = bounds(Some(20.0), Some(80.0)).merge(&bounds(Some(0.0), Some(100.0))).unwrap();
        assert_eq!((m.min, m.max), (Some(20.0), Some(80.0)));
    }

    #[test]
    fn merge_takes_tighter_upper_bound() {
        // [0, 50] merged with [0, 100] → [0, 50]
        let m = bounds(Some(0.0), Some(50.0)).merge(&bounds(Some(0.0), Some(100.0))).unwrap();
        assert_eq!((m.min, m.max), (Some(0.0), Some(50.0)));
    }

    #[test]
    fn one_sided_bounds_combine() {
        // min-only merged with max-only → both bounds present
        let m = bounds(Some(10.0), None).merge(&bounds(None, Some(50.0))).unwrap();
        assert_eq!((m.min, m.max), (Some(10.0), Some(50.0)));
    }

    #[test]
    fn non_overlapping_ranges_conflict() {
        // [60, 100] ∩ [0, 30] = ∅
        assert!(bounds(Some(60.0), Some(100.0)).merge(&bounds(Some(0.0), Some(30.0))).is_none());
    }

    #[test]
    fn one_sided_non_overlapping_conflicts() {
        // [50, ∞) ∩ (-∞, 30] = ∅
        assert!(bounds(Some(50.0), None).merge(&bounds(None, Some(30.0))).is_none());
    }

    // --- merge: generators ---

    #[test]
    fn same_generator_merges() {
        let m = with_gen(Generator::Email).merge(&with_gen(Generator::Email)).unwrap();
        assert_eq!(m.generator, Some(Generator::Email));
    }

    #[test]
    fn different_generators_conflict() {
        assert!(with_gen(Generator::Email).merge(&with_gen(Generator::Username)).is_none());
    }

    #[test]
    fn generator_merges_with_unconstrained() {
        let m = with_gen(Generator::Email).merge(&FieldConstraints::default()).unwrap();
        assert_eq!(m.generator, Some(Generator::Email));
    }

    // --- merge: constant values ---

    #[test]
    fn same_value_merges() {
        let m = with_value("active").merge(&with_value("active")).unwrap();
        assert_eq!(m.value.as_ref().and_then(|v| v.as_str()), Some("active"));
    }

    #[test]
    fn different_values_conflict() {
        assert!(with_value("active").merge(&with_value("inactive")).is_none());
    }

    #[test]
    fn value_merges_with_unconstrained() {
        // Adding a constant to an unconstrained base is a valid specialisation.
        let m = with_value("active").merge(&FieldConstraints::default()).unwrap();
        assert_eq!(m.value.as_ref().and_then(|v| v.as_str()), Some("active"));
    }

    #[test]
    fn value_and_min_after_merge_is_not_satisfiable() {
        // Neither side alone is invalid, but merging value + min produces an unsatisfiable result.
        let local = with_value("active");
        let base  = bounds(Some(0.0), None);
        assert!(local.merge(&base).is_none());
    }
}