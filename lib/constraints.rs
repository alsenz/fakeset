//! Field constraint types (`FieldConstraints`, `Satisfiable`, `Merge`) and
//! `validate_field_constraints` — used by the segment pruner and validator to detect
//! contradictory constraint sets (e.g. two lower cover members pinning the same field
//! to different constants).
use crate::models::{CaseDelta, Field, Generator, Range};
use anyhow::{Result, bail};
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
///
/// `generator` / `one_of` / `value` form a single **value-source spectrum** ordered by how
/// much of the domain they pin down (VAR-SPECIALIZE): type-default ≻ `generator` ≻ `one_of`
/// (finite set) ≻ `value` (static singleton). A child specialises a parent by moving to a
/// *tighter* point on the spectrum — never a conflict. `min`/`max` are orthogonal numeric
/// bounds that always intersect.
#[derive(Debug, Clone, Default)]
pub struct FieldConstraints {
    pub generator: Option<Generator>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub value: Option<YamlValue>,
    /// Finite-set support selector (VAR-SPECIALIZE). Populated from `Field.one_of`.
    pub one_of: Option<Vec<YamlValue>>,
    /// Per-case carrier specialisations (VAR-SPECIALIZE S5; `constrain_cases`). Carried through
    /// merge and applied to a variant field's cases at generation (`apply_constraints`); they do
    /// not participate in conflict pruning (carrier narrowing is always satisfiable).
    pub case_overrides: Vec<CaseDelta>,
    /// **Rigid** numeric support of an expression-authored (computed) column (EXPR-RELOCATE PR3).
    /// Unlike `min`/`max` — a *malleable* range a restriction may narrow — this interval is the
    /// derived output range of the column's `expression` and **cannot be narrowed**: a restriction
    /// can only be *satisfied* (contained), *contradicted* (disjoint → prune), or *unsatisfiable as
    /// stated* (partial overlap / underivable → hard error). Set by the planner, never from YAML.
    /// See [`FieldConstraints::reconcile`].
    pub computed: Option<NumericInterval>,
}

/// A numeric interval `[min, max]`; either bound `None` means unbounded on that side. The support
/// of a numeric value-source (EXPR-RELOCATE PR3 — the reusable primitive for computed-column bound
/// reasoning, and the substrate for future distribution-constrained / slice-sampled generators).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NumericInterval {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// The outcome of reconciling two constraint sets when at least one carries a **rigid** computed
/// support. Richer than `merge`'s `Option` because a rigid support admits a third case: a
/// restriction that is neither satisfiable nor cleanly contradictory is *unsound* (a hard error),
/// not a silent prune.
#[derive(Debug)]
pub enum Reconciled {
    /// Compatible — keep this merged constraint set.
    Feasible(FieldConstraints),
    /// Contradictory — this segment/combination has no rows; prune it.
    Infeasible,
    /// Structurally unsound (a restriction would have to narrow a rigid computed column, or the
    /// computed bounds are not derivable on a constrained side). Surface as a hard error.
    Unsound(String),
}

/// Validate the constraint set of a field at `path`, returning detailed error messages.
///
/// Under the value-source spectrum (VAR-SPECIALIZE) `value` + `generator` is **not** a
/// conflict — `value` is simply the tightest point and supersedes the generator. The only
/// real numeric error is a constant `value` that falls outside a declared `range`, or an
/// inverted range.
pub fn validate_field_constraints(path: &str, field: &Field) -> Result<()> {
    let fc = FieldConstraints::from(field);
    if let (Some(lo), Some(hi)) = (fc.min, fc.max)
        && lo > hi
    {
        bail!(
            "field '{path}': `range.min` ({lo}) must be less than or equal to `range.max` ({hi})"
        );
    }
    // A constant numeric `value` must lie within any declared bounds.
    if let Some(v) = &fc.value
        && let Some(n) = v.as_f64()
    {
        if let Some(lo) = fc.min
            && n < lo
        {
            bail!("field '{path}': `value` ({n}) is below `range.min` ({lo})");
        }
        if let Some(hi) = fc.max
            && n > hi
        {
            bail!("field '{path}': `value` ({n}) is above `range.max` ({hi})");
        }
    }
    Ok(())
}

impl Satisfiable for FieldConstraints {
    fn satisfiable(&self) -> bool {
        // Numeric bounds must form a valid range.
        if let (Some(lo), Some(hi)) = (self.min, self.max)
            && lo > hi
        {
            return false;
        }
        // A constant numeric `value` must lie within any bounds (a non-numeric value ignores
        // them — `value` is the winning source and bounds are moot).
        if let Some(v) = &self.value
            && let Some(n) = v.as_f64()
        {
            if let Some(lo) = self.min
                && n < lo
            {
                return false;
            }
            if let Some(hi) = self.max
                && n > hi
            {
                return false;
            }
        }
        // An empty `one_of` (e.g. an empty support intersection) selects nothing.
        if let Some(s) = &self.one_of
            && s.is_empty()
        {
            return false;
        }
        true
    }
}

impl From<&Field> for FieldConstraints {
    fn from(f: &Field) -> Self {
        FieldConstraints {
            generator: f.generator.clone(),
            min: f.range.as_ref().and_then(|r| r.min),
            max: f.range.as_ref().and_then(|r| r.max),
            value: f.value.clone(),
            one_of: f.one_of.clone(),
            case_overrides: f.constrain_cases.clone(),
            // A computed (rigid) support is derived by the planner from the expression, not read
            // off the field — `From<&Field>` always yields `None` here.
            computed: None,
        }
    }
}

impl FieldConstraints {
    /// Convert the numeric bounds in this constraint set to a `Range`, returning `None`
    /// when neither bound is set.
    pub fn to_range(&self) -> Option<Range> {
        if self.min.is_some() || self.max.is_some() {
            Some(Range {
                min: self.min,
                max: self.max,
            })
        } else {
            None
        }
    }

    /// Reconcile two constraint sets, accounting for **rigid** computed supports (EXPR-RELOCATE
    /// PR3). When neither side is computed this is exactly `merge` (lifted into the richer
    /// `Reconciled` outcome). When one side carries a rigid computed interval, a range/value
    /// restriction on the other side is checked for *containment* rather than intersected:
    /// contained → `Feasible`; disjoint → `Infeasible` (prune); partial overlap, a non-range
    /// restriction (`one_of`), or a bound the computed interval cannot determine → `Unsound`.
    pub fn reconcile(&self, other: &Self) -> Reconciled {
        match (self.computed, other.computed) {
            (None, None) => match self.merge(other) {
                Some(m) => Reconciled::Feasible(m),
                None => Reconciled::Infeasible,
            },
            (Some(rigid), None) => reconcile_rigid(rigid, other),
            (None, Some(rigid)) => reconcile_rigid(rigid, self),
            (Some(_), Some(_)) => Reconciled::Unsound(
                "two expression-authored value-sources for one shared column".into(),
            ),
        }
    }
}

/// Reconcile a **rigid** computed interval against a (malleable) restriction `fc`. The restriction
/// may only be a numeric range and/or constant `value`; a `one_of` finite set cannot be guaranteed
/// for an arbitrary computed value, so it is `Unsound`.
pub(crate) fn reconcile_rigid(rigid: NumericInterval, fc: &FieldConstraints) -> Reconciled {
    if fc.one_of.is_some() {
        return Reconciled::Unsound(
            "a `one_of` finite-set restriction cannot be reconciled with a computed column".into(),
        );
    }
    // Restriction interval: range bounds, tightened by a numeric constant `value` if present.
    let (mut qlo, mut qhi) = (fc.min, fc.max);
    if let Some(v) = fc.value.as_ref().and_then(|v| v.as_f64()) {
        qlo = Some(qlo.map_or(v, |m| m.max(v)));
        qhi = Some(qhi.map_or(v, |m| m.min(v)));
    }
    // No actual restriction → trivially satisfied.
    if qlo.is_none() && qhi.is_none() {
        return Reconciled::Feasible(FieldConstraints {
            computed: Some(rigid),
            ..Default::default()
        });
    }
    let (rlo, rhi) = (rigid.min, rigid.max);
    // The computed interval is unbounded on a side the restriction constrains → cannot verify.
    if (qlo.is_some() && rlo.is_none()) || (qhi.is_some() && rhi.is_none()) {
        return Reconciled::Unsound(
            "computed column bounds are not statically determinable on a side constrained by a \
             range — cannot verify the restriction (expression output bounds not derivable)"
                .into(),
        );
    }
    // Disjoint → infeasible (prune).
    if let (Some(rh), Some(ql)) = (rhi, qlo)
        && rh < ql
    {
        return Reconciled::Infeasible;
    }
    if let (Some(rl), Some(qh)) = (rlo, qhi)
        && rl > qh
    {
        return Reconciled::Infeasible;
    }
    // Containment: the whole computed interval lies within the restriction.
    let lo_ok = qlo.is_none_or(|ql| rlo.is_some_and(|rl| rl >= ql));
    let hi_ok = qhi.is_none_or(|qh| rhi.is_some_and(|rh| rh <= qh));
    if lo_ok && hi_ok {
        return Reconciled::Feasible(FieldConstraints {
            computed: Some(rigid),
            ..Default::default()
        });
    }
    // Overlapping but not contained: a restriction would have to narrow a rigid column.
    Reconciled::Unsound(format!(
        "a range restriction [{}, {}] only partially overlaps the computed column's derived \
         interval [{}, {}] — a computed column cannot be narrowed to satisfy it",
        qlo.map(|v| v.to_string()).unwrap_or_else(|| "-∞".into()),
        qhi.map(|v| v.to_string()).unwrap_or_else(|| "+∞".into()),
        rlo.map(|v| v.to_string()).unwrap_or_else(|| "-∞".into()),
        rhi.map(|v| v.to_string()).unwrap_or_else(|| "+∞".into()),
    ))
}

impl Merge for FieldConstraints {
    /// Combine two constraint sets along the value-source spectrum (VAR-SPECIALIZE): keep the
    /// **tightest compatible source** (`value` ≺ `one_of` ≺ `generator`) and **intersect
    /// supports**; intersect numeric bounds. Returns `None` only on a genuine conflict —
    /// two different constants, disjoint `one_of` sets, a `value` outside a `one_of`, two
    /// different generators, or non-overlapping ranges.
    fn merge(&self, other: &Self) -> Option<Self> {
        let (value, one_of, generator) = merge_source(source(self), source(other))?;
        let min = [self.min, other.min].into_iter().flatten().reduce(f64::max);
        let max = [self.max, other.max].into_iter().flatten().reduce(f64::min);
        // Per-case carrier specialisations accumulate (applied sequentially at generation);
        // they don't gate satisfiability.
        let mut case_overrides = self.case_overrides.clone();
        case_overrides.extend(other.case_overrides.iter().cloned());
        let merged = FieldConstraints {
            generator,
            min,
            max,
            value,
            one_of,
            case_overrides,
            // `merge` is the malleable (non-computed) algebra; `reconcile` intercepts computed
            // supports before delegating here. Carry a rigid support through defensively.
            computed: self.computed.or(other.computed),
        };
        merged.satisfiable().then_some(merged)
    }
}

/// The tightest value-source of a constraint set, in spectrum order
/// (`Value` ≺ `OneOf` ≺ `Gen` ≺ `None`).
enum Source<'a> {
    Value(&'a YamlValue),
    OneOf(&'a [YamlValue]),
    Gen(&'a Generator),
    None,
}

fn source(fc: &FieldConstraints) -> Source<'_> {
    if let Some(v) = &fc.value {
        Source::Value(v)
    } else if let Some(s) = &fc.one_of {
        Source::OneOf(s)
    } else if let Some(g) = &fc.generator {
        Source::Gen(g)
    } else {
        Source::None
    }
}

type MergedSource = (Option<YamlValue>, Option<Vec<YamlValue>>, Option<Generator>);

fn materialize(s: Source) -> MergedSource {
    match s {
        Source::Value(v) => (Some(v.clone()), None, None),
        Source::OneOf(set) => (None, Some(set.to_vec()), None),
        Source::Gen(g) => (None, None, Some(g.clone())),
        Source::None => (None, None, None),
    }
}

/// Combine two value-sources into the tightest compatible one, intersecting supports.
/// Returns `None` on a genuine conflict.
fn merge_source(a: Source, b: Source) -> Option<MergedSource> {
    use Source::{Gen, None as SNone, OneOf, Value};
    Some(match (a, b) {
        (SNone, SNone) => (None, None, None),
        (SNone, x) | (x, SNone) => materialize(x),
        // generator vs generator: must agree.
        (Gen(g1), Gen(g2)) => {
            if g1 == g2 {
                (None, None, Some(g1.clone()))
            } else {
                return None;
            }
        }
        // generator vs a tighter source: the tighter source wins (generator dropped).
        (Gen(_), OneOf(set)) | (OneOf(set), Gen(_)) => (None, Some(set.to_vec()), None),
        (Gen(_), Value(v)) | (Value(v), Gen(_)) => (Some(v.clone()), None, None),
        // one_of vs one_of: intersect (preserving left order); empty ⇒ conflict.
        (OneOf(s1), OneOf(s2)) => {
            let inter: Vec<YamlValue> = s1.iter().filter(|x| s2.contains(x)).cloned().collect();
            if inter.is_empty() {
                return None;
            }
            (None, Some(inter), None)
        }
        // value vs one_of: the value wins iff it is in the set.
        (OneOf(set), Value(v)) | (Value(v), OneOf(set)) => {
            if set.contains(v) {
                (Some(v.clone()), None, None)
            } else {
                return None;
            }
        }
        // value vs value: must agree.
        (Value(v1), Value(v2)) => {
            if v1 == v2 {
                (Some(v1.clone()), None, None)
            } else {
                return None;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Generator;

    fn bounds(min: Option<f64>, max: Option<f64>) -> FieldConstraints {
        FieldConstraints {
            min,
            max,
            ..Default::default()
        }
    }

    fn with_gen(g: Generator) -> FieldConstraints {
        FieldConstraints {
            generator: Some(g),
            ..Default::default()
        }
    }

    fn with_value(v: &str) -> FieldConstraints {
        FieldConstraints {
            value: Some(YamlValue::String(v.into())),
            ..Default::default()
        }
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
    fn numeric_value_inside_range_is_satisfiable() {
        // VAR-SPECIALIZE: `value` is the tightest source; a numeric value within bounds is fine.
        let c = FieldConstraints {
            min: Some(1.0),
            value: Some(YamlValue::Number(42.into())),
            ..Default::default()
        };
        assert!(c.satisfiable());
    }

    #[test]
    fn numeric_value_outside_range_is_not_satisfiable() {
        let c = FieldConstraints {
            max: Some(10.0),
            value: Some(YamlValue::Number(42.into())),
            ..Default::default()
        };
        assert!(!c.satisfiable());
    }

    #[test]
    fn value_with_generator_is_satisfiable() {
        // VAR-SPECIALIZE: `value` supersedes the generator (tightest point on the spectrum).
        let c = FieldConstraints {
            generator: Some(Generator::Email),
            value: Some(YamlValue::String("x".into())),
            ..Default::default()
        };
        assert!(c.satisfiable());
    }

    #[test]
    fn empty_one_of_is_not_satisfiable() {
        let c = FieldConstraints {
            one_of: Some(vec![]),
            ..Default::default()
        };
        assert!(!c.satisfiable());
    }

    // --- merge: unconstrained ---

    #[test]
    fn two_empty_merge_to_empty() {
        let m = FieldConstraints::default()
            .merge(&FieldConstraints::default())
            .unwrap();
        assert!(m.min.is_none() && m.max.is_none() && m.generator.is_none() && m.value.is_none());
    }

    // --- merge: numeric bounds ---

    #[test]
    fn merge_takes_tighter_lower_bound() {
        // [20, 80] merged with [0, 100] → [20, 80]
        let m = bounds(Some(20.0), Some(80.0))
            .merge(&bounds(Some(0.0), Some(100.0)))
            .unwrap();
        assert_eq!((m.min, m.max), (Some(20.0), Some(80.0)));
    }

    #[test]
    fn merge_takes_tighter_upper_bound() {
        // [0, 50] merged with [0, 100] → [0, 50]
        let m = bounds(Some(0.0), Some(50.0))
            .merge(&bounds(Some(0.0), Some(100.0)))
            .unwrap();
        assert_eq!((m.min, m.max), (Some(0.0), Some(50.0)));
    }

    #[test]
    fn one_sided_bounds_combine() {
        // min-only merged with max-only → both bounds present
        let m = bounds(Some(10.0), None)
            .merge(&bounds(None, Some(50.0)))
            .unwrap();
        assert_eq!((m.min, m.max), (Some(10.0), Some(50.0)));
    }

    #[test]
    fn non_overlapping_ranges_conflict() {
        // [60, 100] ∩ [0, 30] = ∅
        assert!(
            bounds(Some(60.0), Some(100.0))
                .merge(&bounds(Some(0.0), Some(30.0)))
                .is_none()
        );
    }

    #[test]
    fn one_sided_non_overlapping_conflicts() {
        // [50, ∞) ∩ (-∞, 30] = ∅
        assert!(
            bounds(Some(50.0), None)
                .merge(&bounds(None, Some(30.0)))
                .is_none()
        );
    }

    // --- merge: generators ---

    #[test]
    fn same_generator_merges() {
        let m = with_gen(Generator::Email)
            .merge(&with_gen(Generator::Email))
            .unwrap();
        assert_eq!(m.generator, Some(Generator::Email));
    }

    #[test]
    fn different_generators_conflict() {
        assert!(
            with_gen(Generator::Email)
                .merge(&with_gen(Generator::Username))
                .is_none()
        );
    }

    #[test]
    fn generator_merges_with_unconstrained() {
        let m = with_gen(Generator::Email)
            .merge(&FieldConstraints::default())
            .unwrap();
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
        assert!(
            with_value("active")
                .merge(&with_value("inactive"))
                .is_none()
        );
    }

    #[test]
    fn value_merges_with_unconstrained() {
        // Adding a constant to an unconstrained base is a valid specialisation.
        let m = with_value("active")
            .merge(&FieldConstraints::default())
            .unwrap();
        assert_eq!(m.value.as_ref().and_then(|v| v.as_str()), Some("active"));
    }

    #[test]
    fn string_value_merges_with_numeric_min() {
        // VAR-SPECIALIZE: a string `value` wins; the numeric `min` is moot (not a conflict).
        let m = with_value("active")
            .merge(&bounds(Some(0.0), None))
            .unwrap();
        assert_eq!(m.value.as_ref().and_then(|v| v.as_str()), Some("active"));
    }

    #[test]
    fn numeric_value_outside_merged_range_conflicts() {
        // A numeric value pinned outside the merged bound is a genuine conflict.
        let value = FieldConstraints {
            value: Some(YamlValue::Number(42.into())),
            ..Default::default()
        };
        assert!(value.merge(&bounds(None, Some(10.0))).is_none());
    }

    // --- merge: value-source spectrum (VAR-SPECIALIZE S1) ---

    fn with_one_of(vals: &[&str]) -> FieldConstraints {
        FieldConstraints {
            one_of: Some(
                vals.iter()
                    .map(|s| YamlValue::String((*s).into()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn generator_plus_value_specialises_to_value() {
        // The old "value + generator" conflict is now a specialisation: value wins.
        let m = with_gen(Generator::Word)
            .merge(&with_value("active"))
            .unwrap();
        assert_eq!(m.value.as_ref().and_then(|v| v.as_str()), Some("active"));
        assert!(
            m.generator.is_none(),
            "generator dropped when value supersedes it"
        );
    }

    #[test]
    fn generator_plus_one_of_specialises_to_one_of() {
        let m = with_gen(Generator::Word)
            .merge(&with_one_of(&["a", "b"]))
            .unwrap();
        assert_eq!(m.one_of.as_ref().map(|s| s.len()), Some(2));
        assert!(
            m.generator.is_none(),
            "generator dropped when one_of restricts it"
        );
    }

    #[test]
    fn one_of_intersects() {
        let m = with_one_of(&["a", "b", "c"])
            .merge(&with_one_of(&["b", "c", "d"]))
            .unwrap();
        let got: Vec<&str> = m
            .one_of
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(got, vec!["b", "c"], "intersection preserves left order");
    }

    #[test]
    fn disjoint_one_of_conflicts() {
        assert!(
            with_one_of(&["a", "b"])
                .merge(&with_one_of(&["x", "y"]))
                .is_none()
        );
    }

    #[test]
    fn value_within_one_of_wins() {
        let m = with_one_of(&["a", "b"]).merge(&with_value("a")).unwrap();
        assert_eq!(m.value.as_ref().and_then(|v| v.as_str()), Some("a"));
        assert!(m.one_of.is_none(), "one_of collapses to the pinned value");
    }

    #[test]
    fn value_outside_one_of_conflicts() {
        assert!(with_one_of(&["a", "b"]).merge(&with_value("z")).is_none());
    }

    // --- reconcile: rigid computed support (EXPR-RELOCATE PR3) ---

    fn rigid(min: f64, max: f64) -> FieldConstraints {
        FieldConstraints {
            computed: Some(NumericInterval {
                min: Some(min),
                max: Some(max),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn reconcile_without_computed_matches_merge() {
        match bounds(Some(20.0), Some(80.0)).reconcile(&bounds(Some(0.0), Some(100.0))) {
            Reconciled::Feasible(m) => assert_eq!((m.min, m.max), (Some(20.0), Some(80.0))),
            other => panic!("expected Feasible, got {other:?}"),
        }
        assert!(matches!(
            bounds(Some(60.0), Some(100.0)).reconcile(&bounds(Some(0.0), Some(30.0))),
            Reconciled::Infeasible
        ));
    }

    #[test]
    fn reconcile_rigid_contained_is_feasible() {
        // computed [30,45] within restriction [0,100]
        assert!(matches!(
            rigid(30.0, 45.0).reconcile(&bounds(Some(0.0), Some(100.0))),
            Reconciled::Feasible(_)
        ));
    }

    #[test]
    fn reconcile_rigid_disjoint_is_infeasible() {
        // computed [1,4] vs restriction [30,50] — no overlap
        assert!(matches!(
            rigid(1.0, 4.0).reconcile(&bounds(Some(30.0), Some(50.0))),
            Reconciled::Infeasible
        ));
    }

    #[test]
    fn reconcile_rigid_partial_overlap_is_unsound() {
        // computed [1,45] vs restriction [30,50] — overlaps [30,45] but not contained
        assert!(matches!(
            rigid(1.0, 45.0).reconcile(&bounds(Some(30.0), Some(50.0))),
            Reconciled::Unsound(_)
        ));
    }

    #[test]
    fn reconcile_rigid_is_symmetric() {
        assert!(matches!(
            bounds(Some(30.0), Some(50.0)).reconcile(&rigid(1.0, 45.0)),
            Reconciled::Unsound(_)
        ));
    }

    #[test]
    fn reconcile_rigid_unbounded_against_range_is_unsound() {
        // computed unbounded above, restriction caps it → cannot verify
        let r = FieldConstraints {
            computed: Some(NumericInterval {
                min: Some(0.0),
                max: None,
            }),
            ..Default::default()
        };
        assert!(matches!(
            r.reconcile(&bounds(None, Some(100.0))),
            Reconciled::Unsound(_)
        ));
    }

    #[test]
    fn reconcile_rigid_no_restriction_is_feasible() {
        assert!(matches!(
            rigid(1.0, 45.0).reconcile(&FieldConstraints::default()),
            Reconciled::Feasible(_)
        ));
    }

    #[test]
    fn reconcile_rigid_against_one_of_is_unsound() {
        assert!(matches!(
            rigid(1.0, 45.0).reconcile(&with_one_of(&["a", "b"])),
            Reconciled::Unsound(_)
        ));
    }
}
