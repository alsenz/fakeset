//! Arrow schema conversion. `schema_to_arrow` and `field_to_arrow` map fakeset field
//! types to Arrow `DataType`s; `parquet_datatype_to_arrow` handles Parquet overrides.
use arrow::datatypes::{DataType, Field as ArrowField, TimeUnit, UnionFields, UnionMode};
use std::sync::Arc;

use crate::models::{Field, FieldType, ParquetDatatype, Schema};

pub fn schema_to_arrow(schema: &Schema) -> arrow::datatypes::Schema {
    arrow::datatypes::Schema::new(
        schema
            .iter()
            .filter(|f| f.expression.is_none() && !f.is_list_link())
            .map(field_to_arrow)
            .collect::<Vec<_>>(),
    )
}

pub fn field_to_arrow(field: &Field) -> ArrowField {
    let name = if field.name.is_empty() {
        "item"
    } else {
        &field.name
    };

    if let Some(ref pcfg) = field.parquet {
        return ArrowField::new(name, parquet_datatype_to_arrow(&pcfg.datatype), true);
    }

    let ft = field
        .field_type
        .as_ref()
        .expect("field_type unresolved; call resolve_refs before executing");
    let dt = match ft {
        FieldType::Number => DataType::Float64,
        FieldType::Boolean => DataType::Boolean,
        FieldType::String => DataType::Utf8,
        FieldType::Date => DataType::Date32,
        FieldType::DateTime => DataType::Timestamp(TimeUnit::Microsecond, None),
        FieldType::Object => {
            let sub: Vec<ArrowField> = field.fields.iter().map(field_to_arrow).collect();
            DataType::Struct(sub.into())
        }
        FieldType::List => {
            let item_dt = match field.content.as_deref() {
                None => DataType::Utf8,
                Some(c) if c.from.is_none() => field_to_arrow(&c.item).data_type().clone(),
                Some(c) => {
                    let sub: Vec<ArrowField> = c.item.fields.iter().map(field_to_arrow).collect();
                    DataType::Struct(sub.into())
                }
            };
            DataType::List(Arc::new(ArrowField::new("item", item_dt, true)))
        }
        FieldType::Variant => panic!(
            "variant field '{}' must be expanded before execution; call expand_field_variants first",
            field.name
        ),
        FieldType::Union => {
            // Heterogeneous tagged union (VAR-1) → Arrow DenseUnion of the case types.
            // type_id `i` ↔ the i-th case, matching `generate_column`'s union build.
            let union_fields: UnionFields = field
                .union_cases
                .iter()
                .enumerate()
                .map(|(i, c)| (i as i8, Arc::new(field_to_arrow(&c.field))))
                .collect();
            DataType::Union(union_fields, UnionMode::Dense)
        }
    };
    ArrowField::new(name, dt, true)
}

pub(crate) fn parquet_datatype_to_arrow(dt: &ParquetDatatype) -> DataType {
    match dt {
        ParquetDatatype::Int8 => DataType::Int8,
        ParquetDatatype::Int16 => DataType::Int16,
        ParquetDatatype::Int32 => DataType::Int32,
        ParquetDatatype::Int64 => DataType::Int64,
        ParquetDatatype::UInt8 => DataType::UInt8,
        ParquetDatatype::UInt16 => DataType::UInt16,
        ParquetDatatype::UInt32 => DataType::UInt32,
        ParquetDatatype::UInt64 => DataType::UInt64,
        ParquetDatatype::Float32 => DataType::Float32,
        ParquetDatatype::Float64 => DataType::Float64,
        ParquetDatatype::Utf8 => DataType::Utf8,
        ParquetDatatype::Boolean => DataType::Boolean,
        ParquetDatatype::Date32 => DataType::Date32,
        ParquetDatatype::TimestampMs => DataType::Timestamp(TimeUnit::Millisecond, None),
        ParquetDatatype::TimestampUs => DataType::Timestamp(TimeUnit::Microsecond, None),
    }
}
