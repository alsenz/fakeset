//! Static type inference for expressions (EXPR-RELOCATE PR2b).
//!
//! Infers an expression's output Arrow `DataType` from its input column types using DataFusion's
//! logical planner — **without executing any rows**. We register an empty (zero-row) batch
//! carrying just the input schema, plan the same `WITH … SELECT *, <expr> AS <name>` chain that
//! [`crate::executor::evaluate_expression_fields`] runs, and read the planned `DataFrame` schema
//! rather than calling `.collect()`. Reading the logical schema is enough because type coercion
//! happens at planning time; no data is materialised.
//!
//! This is the analysis stepping-stone the EXPR-RELOCATE roadmap adopts ahead of PR3's interval
//! (bound) analysis: the same "ask DataFusion, don't write our own compiler" discipline.
use anyhow::{Result, anyhow};
use arrow::datatypes::{DataType, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use datafusion::common::ScalarValue;
use datafusion::logical_expr::interval_arithmetic::Interval;
use datafusion::physical_expr::PhysicalExpr;
use datafusion::physical_expr::expressions::Column;
use datafusion::physical_expr::intervals::cp_solver::ExprIntervalGraph;
use datafusion::physical_expr::utils::collect_columns;
use datafusion::prelude::SessionContext;
use std::collections::HashMap;
use std::sync::Arc;

use crate::constraints::NumericInterval;
use crate::models::Field;

/// Infer the output `DataType` of each expression field (in order), evaluated over a table whose
/// columns are `input_schema`. Earlier expressions are visible to later ones — identical scoping
/// to the CTE chain in `evaluate_expression_fields`, so the inferred types match what evaluation
/// would produce. Returns one `DataType` per entry in `expr_fields`.
pub async fn infer_expression_types(
    input_schema: &ArrowSchema,
    expr_fields: &[&Field],
) -> Result<Vec<DataType>> {
    if expr_fields.is_empty() {
        return Ok(vec![]);
    }

    let ctx = SessionContext::new();
    crate::list_norm::register_list_udfs(&ctx);
    // Schema-only registration: no rows are needed to read the planned output schema.
    let empty = RecordBatch::new_empty(Arc::new(input_schema.clone()));
    ctx.register_batch("src", empty)?;

    let mut ctes = Vec::new();
    let mut prev = "src".to_string();
    for (i, field) in expr_fields.iter().enumerate() {
        let step = format!("step{i}");
        let expr = field.expression.as_ref().ok_or_else(|| {
            anyhow!(
                "infer_expression_types: field '{}' has no expression",
                field.name
            )
        })?;
        ctes.push(format!(
            "{step} AS (SELECT *, {expr} AS \"{fname}\" FROM {prev})",
            fname = field.name
        ));
        prev = step;
    }
    let sql = format!("WITH {} SELECT * FROM {prev}", ctes.join(", "));
    let df = ctx.sql(&sql).await?;
    let schema = df.schema();

    expr_fields
        .iter()
        .map(|f| {
            schema
                .field_with_unqualified_name(&f.name)
                .map(|fld| fld.data_type().clone())
                .map_err(|e| {
                    anyhow!(
                        "infer_expression_types: could not determine output type of '{}': {e}",
                        f.name
                    )
                })
        })
        .collect()
}

/// Derive the numeric output **interval** of `expr` over input columns `input_schema`, given each
/// input column's known range in `input_ranges` (absent → unbounded). Uses DataFusion's
/// `ExprIntervalGraph` forward evaluation — **synchronous**, no rows materialised (EXPR-RELOCATE
/// PR3 spike). Returns:
/// - `Ok(None)` — the expression is **non-numeric** (no numeric support to reason about);
/// - `Ok(Some(interval))` — numeric; a `None` bound means *underivable / unbounded* on that side
///   (e.g. an unbounded input, or an operation the interval engine can't reason through).
///
/// A literal-only expression const-folds to a point interval `[c, c]`.
pub fn infer_expression_bounds(
    input_schema: &ArrowSchema,
    input_ranges: &HashMap<String, NumericInterval>,
    expr: &str,
) -> Result<Option<NumericInterval>> {
    let df_schema = datafusion::common::DFSchema::try_from(input_schema.clone())?;
    let ctx = SessionContext::new();
    crate::list_norm::register_list_udfs(&ctx);
    let logical = ctx.parse_sql_expr(expr, &df_schema)?;
    let phys = ctx.create_physical_expr(logical, &df_schema)?;

    if !phys.data_type(input_schema)?.is_numeric() {
        return Ok(None);
    }
    // Numeric: attempt interval derivation. Any failure means "not derivable" → fully unbounded,
    // which downstream reconciliation treats as unverifiable against a finite restriction.
    Ok(Some(
        derive_interval(&phys, input_schema, input_ranges).unwrap_or_default(),
    ))
}

/// Forward interval evaluation of a numeric physical expression (see `infer_expression_bounds`).
fn derive_interval(
    phys: &Arc<dyn PhysicalExpr>,
    input_schema: &ArrowSchema,
    input_ranges: &HashMap<String, NumericInterval>,
) -> Result<NumericInterval> {
    let cols: Vec<Arc<dyn PhysicalExpr>> = collect_columns(phys)
        .into_iter()
        .map(|c| Arc::new(c) as Arc<dyn PhysicalExpr>)
        .collect();
    let mut graph = ExprIntervalGraph::try_new(Arc::clone(phys), input_schema)?;
    let node_indices = graph.gather_node_indices(&cols);

    let mut assignments: Vec<(usize, Interval)> = Vec::with_capacity(node_indices.len());
    for (expr, idx) in &node_indices {
        let col = expr
            .as_any()
            .downcast_ref::<Column>()
            .ok_or_else(|| anyhow!("interval leaf is not a column"))?;
        let dt = input_schema.field_with_name(col.name())?.data_type().clone();
        let iv = input_ranges.get(col.name()).copied().unwrap_or_default();
        let interval = Interval::try_new(scalar_of(&dt, iv.min)?, scalar_of(&dt, iv.max)?)?;
        assignments.push((*idx, interval));
    }
    graph.assign_intervals(&assignments);
    let root = graph.evaluate_bounds()?;
    Ok(NumericInterval {
        min: scalar_to_opt_f64(root.lower()),
        max: scalar_to_opt_f64(root.upper()),
    })
}

/// Build the input context for deriving an expression's bounds over a field list: an Arrow schema
/// of the scalar (non-expression, non-list) columns, plus each column's declared numeric range.
/// A member's resolved fields carry the ranges inherited from their ref targets, so this is
/// self-contained — no parent lookup needed.
pub fn scalar_input_context(fields: &[Field]) -> (ArrowSchema, HashMap<String, NumericInterval>) {
    let arrow_fields: Vec<_> = fields
        .iter()
        .filter(|f| f.expression.is_none() && !f.is_list_link() && f.field_type.is_some())
        .map(crate::schema::field_to_arrow)
        .collect();
    let ranges = fields
        .iter()
        .filter_map(|f| {
            f.range.as_ref().map(|r| {
                (
                    f.name.clone(),
                    NumericInterval {
                        min: r.min,
                        max: r.max,
                    },
                )
            })
        })
        .collect();
    (ArrowSchema::new(arrow_fields), ranges)
}

/// A `ScalarValue` of arrow type `dt` carrying `v` (or null/unbounded when `v` is `None`).
fn scalar_of(dt: &DataType, v: Option<f64>) -> Result<ScalarValue> {
    Ok(ScalarValue::Float64(v).cast_to(dt)?)
}

/// Extract a finite `f64` from a `ScalarValue`, or `None` for null/unbounded bounds.
fn scalar_to_opt_f64(sv: &ScalarValue) -> Option<f64> {
    match sv.cast_to(&DataType::Float64) {
        Ok(ScalarValue::Float64(opt)) => opt,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FieldType;
    use arrow::datatypes::Field as ArrowField;

    fn expr_field(name: &str, expr: &str) -> Field {
        Field {
            name: name.into(),
            expression: Some(expr.into()),
            ..Default::default()
        }
    }

    fn schema() -> ArrowSchema {
        ArrowSchema::new(vec![
            ArrowField::new("qty", DataType::Int64, false),
            ArrowField::new("price", DataType::Float64, false),
            ArrowField::new("name", DataType::Utf8, false),
        ])
    }

    #[tokio::test]
    async fn infers_scalar_arithmetic_and_string_types() {
        let s = schema();
        let f1 = expr_field("a", "qty * 2");
        let f2 = expr_field("b", "qty * price");
        let f3 = expr_field("c", "concat(name, '-x')");
        let f4 = expr_field("d", "qty > 5");
        let types = infer_expression_types(&s, &[&f1, &f2, &f3, &f4])
            .await
            .expect("inference should succeed");
        assert_eq!(types[0], DataType::Int64, "qty * 2");
        assert_eq!(types[1], DataType::Float64, "qty * price");
        assert!(
            matches!(types[2], DataType::Utf8 | DataType::Utf8View),
            "concat -> string, got {:?}",
            types[2]
        );
        assert_eq!(types[3], DataType::Boolean, "comparison");
    }

    #[tokio::test]
    async fn later_expression_sees_earlier_one() {
        let s = schema();
        let f1 = expr_field("a", "qty * 2");
        let f2 = expr_field("b", "a + 1"); // references the prior computed column
        let types = infer_expression_types(&s, &[&f1, &f2])
            .await
            .expect("chained inference should succeed");
        assert_eq!(types[1], DataType::Int64);
    }

    #[test]
    fn is_field_type_arrow_mapping_sanity() {
        // Guard the assumption used by the friendly type check: `number` -> Float64.
        assert_eq!(
            crate::schema::field_to_arrow(&Field {
                name: "n".into(),
                field_type: Some(FieldType::Number),
                ..Default::default()
            })
            .data_type(),
            &DataType::Float64
        );
    }

    // --- PR3: infer_expression_bounds ---

    fn numeric_schema() -> ArrowSchema {
        ArrowSchema::new(vec![
            ArrowField::new("base", DataType::Float64, false),
            ArrowField::new("weight", DataType::Float64, false),
        ])
    }

    fn iv(min: f64, max: f64) -> NumericInterval {
        NumericInterval {
            min: Some(min),
            max: Some(max),
        }
    }

    #[test]
    fn bounds_of_product_and_sum() {
        let s = numeric_schema();
        let ranges = HashMap::from([("base".to_string(), iv(1.0, 9.0)), ("weight".to_string(), iv(1.0, 5.0))]);
        assert_eq!(
            infer_expression_bounds(&s, &ranges, "base * weight").unwrap(),
            Some(iv(1.0, 45.0))
        );
        assert_eq!(
            infer_expression_bounds(&s, &ranges, "base + weight").unwrap(),
            Some(iv(2.0, 14.0))
        );
    }

    #[test]
    fn bounds_const_folds_literal() {
        let s = numeric_schema();
        let ranges = HashMap::new();
        assert_eq!(
            infer_expression_bounds(&s, &ranges, "10 * 2").unwrap(),
            Some(iv(20.0, 20.0))
        );
    }

    #[test]
    fn bounds_of_non_numeric_is_none() {
        let s = ArrowSchema::new(vec![ArrowField::new("name", DataType::Utf8, false)]);
        let ranges = HashMap::new();
        assert_eq!(
            infer_expression_bounds(&s, &ranges, "concat(name, '-x')").unwrap(),
            None
        );
    }

    #[test]
    fn bounds_with_unbounded_input_are_underivable() {
        // `base` has no declared range → unbounded leaf → unbounded result.
        let s = numeric_schema();
        let ranges = HashMap::from([("weight".to_string(), iv(1.0, 5.0))]);
        let got = infer_expression_bounds(&s, &ranges, "base * weight").unwrap();
        assert!(
            matches!(got, Some(NumericInterval { min: None, max: None })),
            "expected fully unbounded, got {got:?}"
        );
    }
}
