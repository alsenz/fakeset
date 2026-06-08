//! Within-list numeric normalisation UDFs (LIST-NORM).
//!
//! Two Arrow scalar UDFs rescale a numeric quantity so each list window sums to a target
//! `total`. They are the real primitive behind the declarative `normalize:` field key
//! (desugared in `desugar_normalize`), and are equally usable directly from any `expression:`:
//!
//! ```text
//! array_normalize(list_of_number, total [, precision])
//! array_normalize_field(list_of_struct, 'src', total [, 'dst'] [, precision])
//! ```
//!
//! Int-vs-float output is a runtime decision: an explicit trailing `precision` wins
//! (`0` → integer, `> 0` → float); otherwise it is read off the input element type. The
//! integer path routes through [`crate::segment::largest_remainder`] (the codebase's single
//! rounding primitive — CLAUDE.md invariant #5) so each list sums to **exactly** `total`.
//!
//! Registration is via [`register_list_udfs`] on a `SessionContext`; it pins no pipeline
//! position (keeps `specs/EXPR-RELOCATE.md` unblocked).

use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use arrow::array::cast::AsArray;
use arrow::array::{Array, ArrayRef, Float64Array, Int64Array, ListArray, StructArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field as ArrowField, FieldRef, Fields, Float64Type};
use datafusion::common::{ScalarValue, exec_datafusion_err, exec_err};
use datafusion::error::Result as DfResult;
use datafusion::logical_expr::{
    ColumnarValue, ReturnFieldArgs, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature,
    Volatility,
};
use datafusion::prelude::SessionContext;

use crate::models::{Field, Normalize, SyntheticDataset};

/// Register `array_normalize` and `array_normalize_field` on `ctx`.
pub fn register_list_udfs(ctx: &SessionContext) {
    ctx.register_udf(ScalarUDF::new_from_impl(ArrayNormalize::new()));
    ctx.register_udf(ScalarUDF::new_from_impl(ArrayNormalizeField::new()));
}

// ---------------------------------------------------------------------------
// `normalize:` declarative sugar (desugar pass)
// ---------------------------------------------------------------------------

/// Suffix appended to a normalised field's original name to hold the raw list source.
const NORM_SRC_SUFFIX: &str = "__norm_src";

/// Desugar every `normalize:` field into the existing expression + UDF primitives.
///
/// For each field carrying `normalize`, the list-producing field is renamed in place to a hidden
/// `<name>__norm_src` and an `expression:` field `<name>` calling the matching
/// `array_normalize`/`array_normalize_field` arity is injected immediately after it. The
/// rename-in-place + inject-after ordering keeps the source ahead of its expression, so the
/// injected field needs no separate ordering pass and rides the existing
/// `evaluate_expressions` CTE chain — wherever `specs/EXPR-RELOCATE.md` later places it.
///
/// Runs **after** `validate` (which checks the declarative `normalize` block) so validation and
/// the naive expression-identifier scan never see the injected UDF call.
pub fn desugar_normalize(
    mut datasets: HashMap<PathBuf, SyntheticDataset>,
) -> Result<HashMap<PathBuf, SyntheticDataset>> {
    for dataset in datasets.values_mut() {
        let mut out: Vec<Field> = Vec::with_capacity(dataset.data.len());
        for mut field in std::mem::take(&mut dataset.data) {
            let Some(norm) = field.normalize.take() else {
                out.push(field);
                continue;
            };
            let result_name = field.name.clone();
            let src_name = format!("{result_name}{NORM_SRC_SUFFIX}");
            let result_hidden = field.hidden;
            let quality = field.quality.take();

            // Rename the list-producing field to the hidden source.
            field.name = src_name.clone();
            field.hidden = true;

            let expr = build_normalize_expression(&src_name, &norm);
            let result = Field {
                name: result_name,
                expression: Some(expr),
                hidden: result_hidden,
                quality,
                ..Default::default()
            };

            out.push(field);
            out.push(result);
        }
        dataset.data = out;
    }
    Ok(datasets)
}

/// Build the `array_normalize`/`array_normalize_field` call for a `normalize` block. The source
/// column is double-quoted to preserve its exact case; the sub-field name is a SQL string
/// literal.
fn build_normalize_expression(src_name: &str, norm: &Normalize) -> String {
    let total = format_total(norm.total);
    let precision = norm
        .precision
        .map(|p| format!(", {p}"))
        .unwrap_or_default();
    match (&norm.field, &norm.into) {
        (None, _) => format!("array_normalize(\"{src_name}\", {total}{precision})"),
        (Some(field), None) => {
            format!("array_normalize_field(\"{src_name}\", '{field}', {total}{precision})")
        }
        (Some(field), Some(into)) => format!(
            "array_normalize_field(\"{src_name}\", '{field}', {total}, '{into}'{precision})"
        ),
    }
}

/// Render `total` as an integer literal when whole (so `total: 100` stays `100`), else as a float.
fn format_total(total: f64) -> String {
    if total.fract() == 0.0 {
        format!("{}", total as i64)
    } else {
        format!("{total}")
    }
}

// ---------------------------------------------------------------------------
// Output-type resolution (shared by `return_field_from_args` and `invoke`)
// ---------------------------------------------------------------------------

/// Whether the normalised output should be integer: an explicit `precision: 0` forces it,
/// `precision > 0` forces float, and absent reads off the source element type.
fn out_is_int(src: &DataType, precision: Option<i32>) -> bool {
    match precision {
        Some(0) => true,
        Some(_) => false,
        None => src.is_integer(),
    }
}

/// Arrow element type of the normalised output: `Int64` when integer, else `Float64`.
fn out_elem_type(src: &DataType, precision: Option<i32>) -> DataType {
    if out_is_int(src, precision) {
        DataType::Int64
    } else {
        DataType::Float64
    }
}

// ---------------------------------------------------------------------------
// Per-window normalisation kernels
// ---------------------------------------------------------------------------

/// Float rescale of one list window: `vᵢ ← vᵢ / Σv · total`. Empty → empty; `Σv == 0` → equal
/// split (`total / n`).
fn normalize_window_float(win: &[f64], total: f64) -> Vec<f64> {
    let n = win.len();
    if n == 0 {
        return Vec::new();
    }
    let sum: f64 = win.iter().sum();
    if sum <= 0.0 {
        vec![total / n as f64; n]
    } else {
        win.iter().map(|w| w / sum * total).collect()
    }
}

/// Integer rescale of one list window via largest-remainder so the window sums to **exactly**
/// `total`. Empty → empty; `Σv == 0` → equal split.
fn normalize_window_int(win: &[f64], total: f64) -> Vec<i64> {
    let n = win.len();
    if n == 0 {
        return Vec::new();
    }
    let total_u = total.round().max(0.0) as usize;
    let sum: f64 = win.iter().sum();
    let weights = if sum <= 0.0 { vec![1.0; n] } else { win.to_vec() };
    crate::segment::largest_remainder(&weights, total_u)
        .into_iter()
        .map(|c| c as i64)
        .collect()
}

/// Apply the windowed kernel across the whole flat values buffer, producing a new flat array
/// (`Int64` or `Float64`) of the same length, ready to drop under the original list offsets.
fn normalize_flat(weights: &[f64], offsets: &[i32], total: f64, to_int: bool) -> ArrayRef {
    let n_vals = weights.len();
    if to_int {
        let mut out = vec![0i64; n_vals];
        for w in offsets.windows(2) {
            let (s, e) = (w[0] as usize, w[1] as usize);
            for (k, v) in normalize_window_int(&weights[s..e], total).into_iter().enumerate() {
                out[s + k] = v;
            }
        }
        Arc::new(Int64Array::from(out))
    } else {
        let mut out = vec![0f64; n_vals];
        for w in offsets.windows(2) {
            let (s, e) = (w[0] as usize, w[1] as usize);
            for (k, v) in normalize_window_float(&weights[s..e], total).into_iter().enumerate() {
                out[s + k] = v;
            }
        }
        Arc::new(Float64Array::from(out))
    }
}

/// Cast a flat numeric array to `&[f64]` for weight reading. Nulls cast to 0.0, which is
/// harmless: a null contributes 0 to its window sum and receives 0 of the split.
fn weights_of(values: &ArrayRef) -> DfResult<Vec<f64>> {
    let f = cast(values, &DataType::Float64)
        .map_err(|e| exec_datafusion_err!("array_normalize: cannot read numeric values: {e}"))?;
    Ok(f.as_primitive::<Float64Type>().values().to_vec())
}

// ---------------------------------------------------------------------------
// Scalar-argument helpers
// ---------------------------------------------------------------------------

fn scalar_f64(sv: &ScalarValue) -> DfResult<f64> {
    match sv.cast_to(&DataType::Float64)? {
        ScalarValue::Float64(Some(v)) => Ok(v),
        _ => exec_err!("array_normalize: `total` must be a non-null number"),
    }
}

fn scalar_i32(sv: &ScalarValue) -> DfResult<i32> {
    match sv.cast_to(&DataType::Int32)? {
        ScalarValue::Int32(Some(v)) => Ok(v),
        _ => exec_err!("array_normalize: `precision` must be a non-null integer"),
    }
}

fn scalar_str(sv: &ScalarValue) -> Option<String> {
    match sv {
        ScalarValue::Utf8(Some(s))
        | ScalarValue::LargeUtf8(Some(s))
        | ScalarValue::Utf8View(Some(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Convert a non-list `ColumnarValue` argument to its `ScalarValue` (constant-folding an
/// array arg to its first element if the planner passed one).
fn as_scalar(cv: &ColumnarValue) -> DfResult<ScalarValue> {
    match cv {
        ColumnarValue::Scalar(sv) => Ok(sv.clone()),
        ColumnarValue::Array(a) => ScalarValue::try_from_array(a, 0),
    }
}

// ---------------------------------------------------------------------------
// array_normalize(list_of_number, total [, precision])
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Hash)]
struct ArrayNormalize {
    signature: Signature,
}

impl ArrayNormalize {
    fn new() -> Self {
        Self {
            signature: Signature::variadic_any(Volatility::Immutable),
        }
    }
}

/// Element type of a `List`/`LargeList` data type.
fn list_elem_type(dt: &DataType) -> DfResult<&DataType> {
    match dt {
        DataType::List(f) | DataType::LargeList(f) => Ok(f.data_type()),
        other => exec_err!("array_normalize: expected a list argument, got {other}"),
    }
}

impl ScalarUDFImpl for ArrayNormalize {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn name(&self) -> &str {
        "array_normalize"
    }
    fn signature(&self) -> &Signature {
        &self.signature
    }
    fn return_type(&self, _arg_types: &[DataType]) -> DfResult<DataType> {
        exec_err!("array_normalize: return type depends on argument values; use return_field_from_args")
    }
    fn return_field_from_args(&self, args: ReturnFieldArgs) -> DfResult<FieldRef> {
        let elem = list_elem_type(args.arg_fields[0].data_type())?;
        let precision = match args.scalar_arguments.get(2).and_then(|o| *o) {
            Some(sv) => Some(scalar_i32(sv)?),
            None => None,
        };
        let item = Arc::new(ArrowField::new("item", out_elem_type(elem, precision), true));
        Ok(Arc::new(ArrowField::new(
            self.name(),
            DataType::List(item),
            true,
        )))
    }
    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> DfResult<ColumnarValue> {
        let list_arr = args.args[0].clone().into_array(args.number_rows)?;
        let list = list_arr
            .as_any()
            .downcast_ref::<ListArray>()
            .ok_or_else(|| exec_datafusion_err!("array_normalize: first argument must be a List"))?;
        let total = scalar_f64(&as_scalar(&args.args[1])?)?;
        let precision = match args.args.get(2) {
            Some(cv) => Some(scalar_i32(&as_scalar(cv)?)?),
            None => None,
        };
        let to_int = out_is_int(list.values().data_type(), precision);

        let weights = weights_of(list.values())?;
        let new_values = normalize_flat(&weights, list.value_offsets(), total, to_int);
        let item = Arc::new(ArrowField::new("item", new_values.data_type().clone(), true));
        let out = ListArray::new(item, list.offsets().clone(), new_values, list.nulls().cloned());
        Ok(ColumnarValue::Array(Arc::new(out)))
    }
}

// ---------------------------------------------------------------------------
// array_normalize_field(list_of_struct, 'src', total [, 'dst'] [, precision])
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Hash)]
struct ArrayNormalizeField {
    signature: Signature,
}

impl ArrayNormalizeField {
    fn new() -> Self {
        Self {
            signature: Signature::variadic_any(Volatility::Immutable),
        }
    }
}

/// Parsed trailing arguments of `array_normalize_field` (everything after the list + src name).
struct FieldArgs {
    total: f64,
    into: Option<String>,
    precision: Option<i32>,
}

/// Parse `[total, dst?|precision?, precision?]` (the args after `list, 'src'`). A `dst` is a
/// string literal; `precision` is an integer literal — so the two optionals are unambiguous.
fn parse_field_tail(scalars: &[Option<ScalarValue>]) -> DfResult<FieldArgs> {
    let total = scalars
        .first()
        .and_then(|o| o.as_ref())
        .ok_or_else(|| exec_datafusion_err!("array_normalize_field: missing `total`"))?;
    let total = scalar_f64(total)?;
    let mut into = None;
    let mut precision = None;
    for sv in scalars.iter().skip(1).flatten() {
        if let Some(name) = scalar_str(sv) {
            into = Some(name);
        } else {
            precision = Some(scalar_i32(sv)?);
        }
    }
    Ok(FieldArgs {
        total,
        into,
        precision,
    })
}

/// The struct element fields of a `List<Struct>` data type.
fn list_struct_fields(dt: &DataType) -> DfResult<&Fields> {
    match dt {
        DataType::List(f) | DataType::LargeList(f) => match f.data_type() {
            DataType::Struct(fields) => Ok(fields),
            other => exec_err!("array_normalize_field: list elements must be structs, got {other}"),
        },
        other => exec_err!("array_normalize_field: expected a list argument, got {other}"),
    }
}

/// Build the output struct `Fields` for the in-place vs `into` cases.
fn field_output_fields(
    fields: &Fields,
    src: &str,
    into: Option<&str>,
    out_type: DataType,
) -> DfResult<Fields> {
    let idx = fields
        .iter()
        .position(|f| f.name() == src)
        .ok_or_else(|| exec_datafusion_err!("array_normalize_field: no sub-field '{src}'"))?;
    let mut out: Vec<FieldRef> = fields.iter().cloned().collect();
    match into {
        Some(dst) => out.push(Arc::new(ArrowField::new(dst, out_type, true))),
        None => out[idx] = Arc::new(ArrowField::new(src, out_type, true)),
    }
    Ok(out.into())
}

impl ScalarUDFImpl for ArrayNormalizeField {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn name(&self) -> &str {
        "array_normalize_field"
    }
    fn signature(&self) -> &Signature {
        &self.signature
    }
    fn return_type(&self, _arg_types: &[DataType]) -> DfResult<DataType> {
        exec_err!(
            "array_normalize_field: return type depends on argument values; use return_field_from_args"
        )
    }
    fn return_field_from_args(&self, args: ReturnFieldArgs) -> DfResult<FieldRef> {
        let fields = list_struct_fields(args.arg_fields[0].data_type())?;
        let src = args
            .scalar_arguments
            .get(1)
            .and_then(|o| *o)
            .and_then(scalar_str)
            .ok_or_else(|| exec_datafusion_err!("array_normalize_field: `src` must be a string literal"))?;
        let owned: Vec<Option<ScalarValue>> = args
            .scalar_arguments
            .iter()
            .skip(2)
            .map(|o| o.cloned())
            .collect();
        let parsed = parse_field_tail(&owned)?;
        let src_type = fields
            .iter()
            .find(|f| f.name() == &src)
            .map(|f| f.data_type().clone())
            .ok_or_else(|| exec_datafusion_err!("array_normalize_field: no sub-field '{src}'"))?;
        let out_type = out_elem_type(&src_type, parsed.precision);
        let out_fields = field_output_fields(fields, &src, parsed.into.as_deref(), out_type)?;
        let item = Arc::new(ArrowField::new("item", DataType::Struct(out_fields), true));
        Ok(Arc::new(ArrowField::new(
            self.name(),
            DataType::List(item),
            true,
        )))
    }
    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> DfResult<ColumnarValue> {
        let list_arr = args.args[0].clone().into_array(args.number_rows)?;
        let list = list_arr.as_any().downcast_ref::<ListArray>().ok_or_else(|| {
            exec_datafusion_err!("array_normalize_field: first argument must be a List")
        })?;
        let src = scalar_str(&as_scalar(&args.args[1])?)
            .ok_or_else(|| exec_datafusion_err!("array_normalize_field: `src` must be a string"))?;
        let tail: Vec<Option<ScalarValue>> = args.args[2..]
            .iter()
            .map(|cv| as_scalar(cv).map(Some))
            .collect::<DfResult<_>>()?;
        let parsed = parse_field_tail(&tail)?;

        let structs = list.values().as_any().downcast_ref::<StructArray>().ok_or_else(|| {
            exec_datafusion_err!("array_normalize_field: list elements must be structs")
        })?;
        let src_col = structs
            .column_by_name(&src)
            .ok_or_else(|| exec_datafusion_err!("array_normalize_field: no sub-field '{src}'"))?;
        let to_int = out_is_int(src_col.data_type(), parsed.precision);

        let weights = weights_of(src_col)?;
        let new_col = normalize_flat(&weights, list.value_offsets(), parsed.total, to_int);

        // Rebuild the struct: overwrite `src` in place, or append a new `into` column.
        let mut fields: Vec<FieldRef> = structs.fields().iter().cloned().collect();
        let mut cols: Vec<ArrayRef> = structs.columns().to_vec();
        let idx = fields.iter().position(|f| f.name() == &src).unwrap();
        match parsed.into.as_deref() {
            Some(dst) => {
                fields.push(Arc::new(ArrowField::new(dst, new_col.data_type().clone(), true)));
                cols.push(new_col);
            }
            None => {
                fields[idx] = Arc::new(ArrowField::new(&src, new_col.data_type().clone(), true));
                cols[idx] = new_col;
            }
        }
        let new_struct = StructArray::new(fields.into(), cols, structs.nulls().cloned());
        let item = Arc::new(ArrowField::new("item", new_struct.data_type().clone(), true));
        let out = ListArray::new(
            item,
            list.offsets().clone(),
            Arc::new(new_struct),
            list.nulls().cloned(),
        );
        Ok(ColumnarValue::Array(Arc::new(out)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, ListBuilder, RecordBatch, StructBuilder};
    use arrow::buffer::OffsetBuffer;
    use arrow::datatypes::Schema as ArrowSchema;
    use datafusion::common::ScalarValue;

    /// Build a `List<Float64>` from per-row windows.
    fn list_f64(rows: &[&[f64]]) -> ArrayRef {
        let mut values: Vec<f64> = Vec::new();
        let mut offsets: Vec<i32> = vec![0];
        for r in rows {
            values.extend_from_slice(r);
            offsets.push(values.len() as i32);
        }
        let item = Arc::new(ArrowField::new("item", DataType::Float64, true));
        Arc::new(ListArray::new(
            item,
            OffsetBuffer::new(offsets.into()),
            Arc::new(Float64Array::from(values)),
            None,
        ))
    }

    fn call_normalize(list: ArrayRef, total: ScalarValue, precision: Option<i32>) -> ArrayRef {
        let udf = ArrayNormalize::new();
        let n = list.len();
        let elem = list_elem_type(list.data_type()).unwrap().clone();
        let mut args = vec![
            ColumnarValue::Array(list),
            ColumnarValue::Scalar(total),
        ];
        if let Some(p) = precision {
            args.push(ColumnarValue::Scalar(ScalarValue::Int32(Some(p))));
        }
        let out_item = Arc::new(ArrowField::new("item", out_elem_type(&elem, precision), true));
        let sfa = ScalarFunctionArgs {
            args,
            arg_fields: vec![],
            number_rows: n,
            return_field: Arc::new(ArrowField::new("o", DataType::List(out_item), true)),
            config_options: Arc::new(Default::default()),
        };
        match udf.invoke_with_args(sfa).unwrap() {
            ColumnarValue::Array(a) => a,
            _ => panic!("expected array"),
        }
    }

    fn window_sums_f64(list: &ArrayRef) -> Vec<f64> {
        let l = list.as_any().downcast_ref::<ListArray>().unwrap();
        (0..l.len())
            .map(|i| {
                let w = l.value(i);
                let f = w.as_any().downcast_ref::<Float64Array>().unwrap();
                (0..f.len()).map(|j| f.value(j)).sum()
            })
            .collect()
    }

    fn window_sums_i64(list: &ArrayRef) -> Vec<i64> {
        let l = list.as_any().downcast_ref::<ListArray>().unwrap();
        (0..l.len())
            .map(|i| {
                let w = l.value(i);
                let a = w.as_any().downcast_ref::<Int64Array>().unwrap();
                (0..a.len()).map(|j| a.value(j)).sum()
            })
            .collect()
    }

    #[test]
    fn float_each_window_sums_to_total() {
        let list = list_f64(&[&[1.0, 1.0, 2.0], &[3.0, 1.0]]);
        let out = call_normalize(list, ScalarValue::Float64(Some(100.0)), None);
        for s in window_sums_f64(&out) {
            assert!((s - 100.0).abs() < 1e-9, "window sum {s} != 100");
        }
    }

    #[test]
    fn integer_precision_sums_exactly() {
        // Float source + precision:0 → integer output summing to exactly the total.
        let list = list_f64(&[&[1.0, 1.0, 1.0], &[2.0, 1.0]]);
        let out = call_normalize(list, ScalarValue::Int64(Some(100)), Some(0));
        assert_eq!(out.data_type(), &DataType::List(Arc::new(ArrowField::new("item", DataType::Int64, true))));
        assert_eq!(window_sums_i64(&out), vec![100, 100]);
    }

    #[test]
    fn empty_window_stays_empty() {
        let list = list_f64(&[&[], &[5.0]]);
        let out = call_normalize(list, ScalarValue::Float64(Some(10.0)), None);
        let l = out.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(l.value(0).len(), 0);
        assert_eq!(window_sums_f64(&out)[1], 10.0);
    }

    #[test]
    fn all_zero_window_equal_splits() {
        let list = list_f64(&[&[0.0, 0.0, 0.0, 0.0]]);
        let out = call_normalize(list, ScalarValue::Int64(Some(100)), Some(0));
        let l = out.as_any().downcast_ref::<ListArray>().unwrap();
        let w = l.value(0);
        let a = w.as_any().downcast_ref::<Int64Array>().unwrap();
        let vals: Vec<i64> = (0..a.len()).map(|j| a.value(j)).collect();
        assert_eq!(vals.iter().sum::<i64>(), 100);
        // Equal split of 100 over 4 → 25 each.
        assert_eq!(vals, vec![25, 25, 25, 25]);
    }

    /// Build a `List<Struct{tag: Utf8, amt: Float64}>` from per-row windows of amounts.
    fn list_struct(rows: &[&[f64]]) -> ArrayRef {
        let tag = Arc::new(ArrowField::new("tag", DataType::Utf8, true));
        let amt = Arc::new(ArrowField::new("amt", DataType::Float64, true));
        let struct_fields = Fields::from(vec![tag, amt]);
        let sb = StructBuilder::from_fields(struct_fields, 0);
        let mut lb = ListBuilder::new(sb);
        for r in rows {
            for v in *r {
                let s = lb.values();
                s.field_builder::<arrow::array::StringBuilder>(0)
                    .unwrap()
                    .append_value("x");
                s.field_builder::<arrow::array::Float64Builder>(1)
                    .unwrap()
                    .append_value(*v);
                s.append(true);
            }
            lb.append(true);
        }
        Arc::new(lb.finish())
    }

    fn call_field(
        list: ArrayRef,
        src: &str,
        total: ScalarValue,
        into: Option<&str>,
        precision: Option<i32>,
    ) -> ArrayRef {
        let udf = ArrayNormalizeField::new();
        let n = list.len();
        let mut args = vec![
            ColumnarValue::Array(list),
            ColumnarValue::Scalar(ScalarValue::Utf8(Some(src.to_string()))),
            ColumnarValue::Scalar(total),
        ];
        if let Some(d) = into {
            args.push(ColumnarValue::Scalar(ScalarValue::Utf8(Some(d.to_string()))));
        }
        if let Some(p) = precision {
            args.push(ColumnarValue::Scalar(ScalarValue::Int32(Some(p))));
        }
        let sfa = ScalarFunctionArgs {
            args,
            arg_fields: vec![],
            number_rows: n,
            return_field: Arc::new(ArrowField::new("o", DataType::Null, true)),
            config_options: Arc::new(Default::default()),
        };
        match udf.invoke_with_args(sfa).unwrap() {
            ColumnarValue::Array(a) => a,
            _ => panic!("expected array"),
        }
    }

    fn sub_field_sums(list: &ArrayRef, name: &str) -> Vec<f64> {
        let l = list.as_any().downcast_ref::<ListArray>().unwrap();
        (0..l.len())
            .map(|i| {
                let w = l.value(i);
                let s = w.as_any().downcast_ref::<StructArray>().unwrap();
                let c = cast(s.column_by_name(name).unwrap(), &DataType::Float64).unwrap();
                let f = c.as_primitive::<Float64Type>();
                (0..f.len()).map(|j| f.value(j)).sum()
            })
            .collect()
    }

    #[test]
    fn field_in_place_overwrites_and_sums() {
        let list = list_struct(&[&[1.0, 3.0], &[2.0, 2.0, 4.0]]);
        let out = call_field(list, "amt", ScalarValue::Float64(Some(100.0)), None, None);
        let fields = list_struct_fields(out.data_type()).unwrap();
        assert_eq!(fields.len(), 2, "in-place keeps the same fields");
        for s in sub_field_sums(&out, "amt") {
            assert!((s - 100.0).abs() < 1e-9);
        }
    }

    #[test]
    fn field_into_keeps_source_and_adds_integer_pct() {
        let list = list_struct(&[&[1.0, 1.0, 1.0], &[3.0, 1.0]]);
        let out = call_field(list, "amt", ScalarValue::Int64(Some(100)), Some("pct"), Some(0));
        let fields = list_struct_fields(out.data_type()).unwrap();
        assert_eq!(fields.len(), 3, "into appends a new field");
        assert_eq!(fields.iter().find(|f| f.name() == "pct").unwrap().data_type(), &DataType::Int64);
        // Raw amt retained unchanged.
        assert_eq!(sub_field_sums(&out, "amt"), vec![3.0, 4.0]);
        // pct sums to exactly 100 per window.
        let l = out.as_any().downcast_ref::<ListArray>().unwrap();
        for i in 0..l.len() {
            let w = l.value(i);
            let s = w.as_any().downcast_ref::<StructArray>().unwrap();
            let pct = s.column_by_name("pct").unwrap().as_any().downcast_ref::<Int64Array>().unwrap();
            assert_eq!((0..pct.len()).map(|j| pct.value(j)).sum::<i64>(), 100);
        }
    }

    #[test]
    fn expression_shapes_match_arity() {
        let scalar = Normalize {
            total: 1.0,
            ..Default::default()
        };
        assert_eq!(
            build_normalize_expression("w__norm_src", &scalar),
            "array_normalize(\"w__norm_src\", 1)"
        );
        let in_place = Normalize {
            total: 100.0,
            field: Some("stake".into()),
            ..Default::default()
        };
        assert_eq!(
            build_normalize_expression("s__norm_src", &in_place),
            "array_normalize_field(\"s__norm_src\", 'stake', 100)"
        );
        let into = Normalize {
            total: 100.0,
            field: Some("shareholding".into()),
            into: Some("ownership_pc".into()),
            precision: Some(0),
        };
        assert_eq!(
            build_normalize_expression("h__norm_src", &into),
            "array_normalize_field(\"h__norm_src\", 'shareholding', 100, 'ownership_pc', 0)"
        );
    }

    #[test]
    fn desugar_renames_source_and_injects_expression() {
        let yaml = r#"
name: companies
format: parquet
output_file: companies
rows: 10
data:
  - name: shareholders
    type: list
    normalize: { field: shareholding, into: ownership_pc, total: 100, precision: 0 }
    content:
      from: subsidiary
      fields:
        - { name: shareholding, type: number, range: { min: 1, max: 9 } }
"#;
        let ds: SyntheticDataset = serde_yaml::from_str(yaml).unwrap();
        let mut map = HashMap::new();
        map.insert(PathBuf::from("/a/companies.yaml"), ds);
        let out = desugar_normalize(map).unwrap();
        let ds = out.values().next().unwrap();

        assert_eq!(ds.data.len(), 2, "source + injected expression");
        let src = &ds.data[0];
        assert_eq!(src.name, "shareholders__norm_src");
        assert!(src.hidden, "renamed source is hidden");
        assert!(src.normalize.is_none(), "normalize consumed");
        assert!(src.content.is_some(), "list content preserved on the source");

        let result = &ds.data[1];
        assert_eq!(result.name, "shareholders");
        assert!(!result.hidden);
        assert_eq!(
            result.expression.as_deref(),
            Some("array_normalize_field(\"shareholders__norm_src\", 'shareholding', 100, 'ownership_pc', 0)")
        );
    }

    /// End-to-end through DataFusion SQL planning — exercises `return_field_from_args`,
    /// coercion, and `array_cat`-then-normalise composition (the registration path used by
    /// `evaluate_expressions`).
    #[tokio::test]
    async fn sql_roundtrip_array_cat_then_normalize_field() {
        let a = list_struct(&[&[1.0, 1.0], &[2.0]]);
        let b = list_struct(&[&[2.0], &[1.0, 1.0]]);
        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("a", a.data_type().clone(), true),
            ArrowField::new("b", b.data_type().clone(), true),
        ]));
        let batch = RecordBatch::try_new(schema, vec![a, b]).unwrap();

        let ctx = SessionContext::new();
        register_list_udfs(&ctx);
        ctx.register_batch("src", batch).unwrap();
        let df = ctx
            .sql(
                "SELECT array_normalize_field(array_cat(a, b), 'amt', 100, 'pct', 0) AS s FROM src",
            )
            .await
            .unwrap();
        let out = df.collect().await.unwrap();
        let col = out[0].column(0);
        let l = col.as_any().downcast_ref::<ListArray>().unwrap();
        for i in 0..l.len() {
            let w = l.value(i);
            let s = w.as_any().downcast_ref::<StructArray>().unwrap();
            let pct = s
                .column_by_name("pct")
                .unwrap()
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            assert_eq!((0..pct.len()).map(|j| pct.value(j)).sum::<i64>(), 100);
        }
    }
}
