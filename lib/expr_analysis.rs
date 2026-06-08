//! Expression placement, evaluation, and static analysis (EXPR-RELOCATE).
//!
//! Everything about turning a YAML `expression:` into a column lives here:
//! - **placement** — [`classify_expression_placement`] buckets a dataset's expression fields into
//!   staging-tier vs assembly-tier by dependency;
//! - **evaluation** — [`evaluate_expression_fields`] runs a DataFusion CTE chain over a batch;
//! - **analysis** — [`infer_expression_types`] / [`infer_expression_bounds`] read the planned
//!   output type / derived interval **without executing rows** (an empty schema-only batch); and
//! - **coercion** — [`types_compatible`] / [`cast_computed_columns`] reconcile a computed column's
//!   result type with its declared type.
//!
//! The analysis half is the "ask DataFusion, don't write our own compiler" discipline; the
//! evaluation half is the one place that actually materialises expression columns.
use anyhow::{Result, anyhow};
use arrow::array::ArrayRef;
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use datafusion::common::ScalarValue;
use datafusion::logical_expr::interval_arithmetic::Interval;
use datafusion::physical_expr::PhysicalExpr;
use datafusion::physical_expr::expressions::Column;
use datafusion::physical_expr::intervals::cp_solver::ExprIntervalGraph;
use datafusion::physical_expr::utils::collect_columns;
use datafusion::prelude::SessionContext;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::constraints::NumericInterval;
use crate::expressions::extract_identifiers;
use crate::models::{Field, FieldType, SyntheticDataset};

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

// ---------------------------------------------------------------------------
// Placement + evaluation (relocated from executor.rs — EXPR-RELOCATE)
// ---------------------------------------------------------------------------

/// True when a field materialises a list column (a `type: list` field or a list-link).
/// Expression fields are excluded — their list-ness is decided transitively by deps.
fn is_list_producing(field: &Field) -> bool {
    field.expression.is_none()
        && (matches!(field.field_type, Some(FieldType::List)) || field.content.is_some())
}

/// Partition a dataset's expression fields into staging-tier and assembly-tier, preserving
/// YAML (`data`) order in each so the per-tier CTE chains keep dependency order. A single
/// forward pass suffices because expression deps point upward (`validate_expression_order`).
pub(crate) fn classify_expression_placement(
    dataset: &SyntheticDataset,
) -> (Vec<&Field>, Vec<&Field>) {
    let mut assembly_names: HashSet<&str> = HashSet::new();
    let mut staging: Vec<&Field> = Vec::new();
    let mut assembly: Vec<&Field> = Vec::new();

    for field in &dataset.data {
        if is_list_producing(field) {
            assembly_names.insert(field.name.as_str());
            continue;
        }
        let Some(expr) = &field.expression else {
            continue;
        };
        let depends_on_assembly = extract_identifiers(expr)
            .iter()
            .any(|id| assembly_names.contains(id));
        if depends_on_assembly {
            assembly_names.insert(field.name.as_str());
            assembly.push(field);
        } else {
            staging.push(field);
        }
    }
    (staging, assembly)
}

/// Evaluate all of `dataset`'s expression fields over `batch` (used at the non-list
/// generation/emit sites, where there is a single batch and no staging/assembly split).
pub(crate) async fn evaluate_expressions(
    batch: RecordBatch,
    dataset: &SyntheticDataset,
) -> Result<RecordBatch> {
    let expr_fields: Vec<_> = dataset
        .data
        .iter()
        .filter(|f| f.expression.is_some())
        .collect();
    evaluate_expression_fields(batch, &expr_fields).await
}

/// Evaluate a *subset* of expression fields over `batch` via a CTE chain, in the given
/// order. The caller selects which fields (a materialisation tier) — EXPR-RELOCATE PR1.
pub(crate) async fn evaluate_expression_fields(
    batch: RecordBatch,
    expr_fields: &[&Field],
) -> Result<RecordBatch> {
    if expr_fields.is_empty() {
        return Ok(batch);
    }

    // Fresh context per call — table name "src" is stable and the context is dropped
    // at function exit, so there is no registration lifecycle to manage.
    let ctx = SessionContext::new();
    crate::list_norm::register_list_udfs(&ctx);
    ctx.register_batch("src", batch)?;

    let mut ctes = Vec::new();
    let mut prev = "src".to_string();
    for (i, field) in expr_fields.iter().enumerate() {
        let step = format!("step{i}");
        let expr = field.expression.as_ref().unwrap();
        ctes.push(format!(
            "{step} AS (SELECT *, {expr} AS \"{fname}\" FROM {prev})",
            fname = field.name
        ));
        prev = step;
    }

    let sql = format!("WITH {} SELECT * FROM {prev}", ctes.join(", "));
    let df = ctx.sql(&sql).await?;
    let batches = df.collect().await?;

    let schema = batches
        .first()
        .map(|b| b.schema())
        .ok_or_else(|| anyhow!("expression evaluation returned no rows"))?;
    Ok(concat_batches(&schema, &batches)?)
}

/// Two Arrow types are compatible as a *computed shared column* vs its *declared* type when they
/// are equal or in the same family (numeric↔numeric, string↔string). Same-family differences
/// (e.g. `Int64` result for a `number`/`Float64` column, `Utf8View` for `Utf8`) are reconciled by
/// a cast; cross-family (e.g. a numeric result for a `string` column) is a genuine modelling error.
pub(crate) fn types_compatible(declared: &DataType, inferred: &DataType) -> bool {
    if declared == inferred {
        return true;
    }
    let stringy =
        |d: &DataType| matches!(d, DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View);
    (declared.is_numeric() && inferred.is_numeric()) || (stringy(declared) && stringy(inferred))
}

/// Cast the named computed columns of `batch` to their declared Arrow types, leaving all other
/// columns untouched. A failed cast is a genuine type mismatch surfaced as a clear error.
pub(crate) fn cast_computed_columns(
    batch: RecordBatch,
    expr_fields: &[&Field],
    declared: &[DataType],
) -> Result<RecordBatch> {
    let want: HashMap<&str, &DataType> = expr_fields
        .iter()
        .map(|f| f.name.as_str())
        .zip(declared.iter())
        .collect();
    let schema = batch.schema();
    let mut fields: Vec<ArrowField> = Vec::with_capacity(batch.num_columns());
    let mut cols: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for (f, c) in schema.fields().iter().zip(batch.columns()) {
        match want.get(f.name().as_str()) {
            Some(&target) if f.data_type() != target => {
                let casted = arrow::compute::cast(c, target).map_err(|e| {
                    anyhow!(
                        "computed shared column '{}': cannot cast result {:?} to declared {target:?}: {e}",
                        f.name(),
                        f.data_type(),
                    )
                })?;
                fields.push(ArrowField::new(f.name(), target.clone(), true));
                cols.push(casted);
            }
            _ => {
                fields.push(f.as_ref().clone());
                cols.push(c.clone());
            }
        }
    }
    Ok(RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), cols)?)
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
