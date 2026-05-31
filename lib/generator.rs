//! Per-field fake data generation. `generate_column` dispatches to fake-rs generators
//! based on field type and generator settings; `sample_count` draws a cardinality value
//! from a `CountSpec` for list-link witness generation.
use anyhow::{Result, anyhow, bail};
use arrow::array::{
    Array, ArrayRef, BooleanArray, Date32Array, Float64Array, ListArray, StringArray, StructArray,
    TimestampMicrosecondArray,
};
use arrow::buffer::OffsetBuffer;
use arrow::compute::{cast, concat};
use arrow::datatypes::{DataType, Field as ArrowField};
use fake::{Fake, Faker};
use std::sync::Arc;

use crate::models::{CountSpec, Field, FieldType, Generator, Locale};
use crate::schema::{field_to_arrow, parquet_datatype_to_arrow};
use std::collections::HashMap;

/// Dispatch a locale-aware faker to the right fake-rs locale struct.
/// All 14 fake-rs locales are supported for every faker; locale data that
/// hasn't been customised for a particular locale falls back to the EN defaults.
macro_rules! locale_fake {
    ($loc:expr, $path:path) => {
        match $loc {
            Locale::En   => $path(fake::locales::EN).fake::<String>(),
            Locale::FrFr => $path(fake::locales::FR_FR).fake::<String>(),
            Locale::DeDe => $path(fake::locales::DE_DE).fake::<String>(),
            Locale::ItIt => $path(fake::locales::IT_IT).fake::<String>(),
            Locale::NlNl => $path(fake::locales::NL_NL).fake::<String>(),
            Locale::PtBr => $path(fake::locales::PT_BR).fake::<String>(),
            Locale::PtPt => $path(fake::locales::PT_PT).fake::<String>(),
            Locale::CyGb => $path(fake::locales::CY_GB).fake::<String>(),
            Locale::ZhCn => $path(fake::locales::ZH_CN).fake::<String>(),
            Locale::ZhTw => $path(fake::locales::ZH_TW).fake::<String>(),
            Locale::JaJp => $path(fake::locales::JA_JP).fake::<String>(),
            Locale::ArSa => $path(fake::locales::AR_SA).fake::<String>(),
            Locale::TrTr => $path(fake::locales::TR_TR).fake::<String>(),
            Locale::FaIr => $path(fake::locales::FA_IR).fake::<String>(),
        }
    };
    ($loc:expr, $path:path, $($arg:expr),+) => {
        match $loc {
            Locale::En   => $path(fake::locales::EN,   $($arg),+).fake::<String>(),
            Locale::FrFr => $path(fake::locales::FR_FR, $($arg),+).fake::<String>(),
            Locale::DeDe => $path(fake::locales::DE_DE, $($arg),+).fake::<String>(),
            Locale::ItIt => $path(fake::locales::IT_IT, $($arg),+).fake::<String>(),
            Locale::NlNl => $path(fake::locales::NL_NL, $($arg),+).fake::<String>(),
            Locale::PtBr => $path(fake::locales::PT_BR, $($arg),+).fake::<String>(),
            Locale::PtPt => $path(fake::locales::PT_PT, $($arg),+).fake::<String>(),
            Locale::CyGb => $path(fake::locales::CY_GB, $($arg),+).fake::<String>(),
            Locale::ZhCn => $path(fake::locales::ZH_CN, $($arg),+).fake::<String>(),
            Locale::ZhTw => $path(fake::locales::ZH_TW, $($arg),+).fake::<String>(),
            Locale::JaJp => $path(fake::locales::JA_JP, $($arg),+).fake::<String>(),
            Locale::ArSa => $path(fake::locales::AR_SA, $($arg),+).fake::<String>(),
            Locale::TrTr => $path(fake::locales::TR_TR, $($arg),+).fake::<String>(),
            Locale::FaIr => $path(fake::locales::FA_IR, $($arg),+).fake::<String>(),
        }
    };
}

/// Like `locale_fake!` but collects a `Vec<String>` result and joins with a separator.
macro_rules! locale_fake_join {
    ($loc:expr, $path:path, $arg:expr, $sep:expr) => {
        match $loc {
            Locale::En => $path(fake::locales::EN, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::FrFr => $path(fake::locales::FR_FR, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::DeDe => $path(fake::locales::DE_DE, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::ItIt => $path(fake::locales::IT_IT, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::NlNl => $path(fake::locales::NL_NL, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::PtBr => $path(fake::locales::PT_BR, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::PtPt => $path(fake::locales::PT_PT, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::CyGb => $path(fake::locales::CY_GB, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::ZhCn => $path(fake::locales::ZH_CN, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::ZhTw => $path(fake::locales::ZH_TW, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::JaJp => $path(fake::locales::JA_JP, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::ArSa => $path(fake::locales::AR_SA, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::TrTr => $path(fake::locales::TR_TR, $arg)
                .fake::<Vec<String>>()
                .join($sep),
            Locale::FaIr => $path(fake::locales::FA_IR, $arg)
                .fake::<Vec<String>>()
                .join($sep),
        }
    };
}

// ---------------------------------------------------------------------------
// Column generation
// ---------------------------------------------------------------------------

pub fn sample_count(spec: &CountSpec) -> usize {
    match spec {
        CountSpec::Fixed(n) => *n,
        CountSpec::Uniform { min, max } => (*min as u64..=*max as u64).fake::<u64>() as usize,
        CountSpec::Normal { mean, std_dev } => {
            // Irwin-Hall approximation of N(0,1): sum 12 U[0,1] samples, subtract 6.
            let z: f64 = (0..12).map(|_| (0.0f64..1.0f64).fake::<f64>()).sum::<f64>() - 6.0;
            let s = mean + std_dev * z;
            (s.round() as i64).max(0) as usize
        }
    }
}

pub fn generate_column(field: &Field, rows: usize, prefix: &[ArrayRef]) -> Result<ArrayRef> {
    let mut col = generate_column_raw(field, rows, prefix)?;
    if let (Some(precision), Some(FieldType::Number)) = (field.precision, &field.field_type) {
        let factor = 10f64.powi(precision);
        let arr = col.as_any().downcast_ref::<Float64Array>().ok_or_else(|| {
            anyhow!(
                "precision: expected Float64 column for number field '{}'",
                field.name
            )
        })?;
        col = Arc::new(Float64Array::from(
            arr.values()
                .iter()
                .map(|&v| (v * factor).round() / factor)
                .collect::<Vec<_>>(),
        ));
    }
    if let Some(ref pcfg) = field.parquet {
        let target = parquet_datatype_to_arrow(&pcfg.datatype);
        if col.data_type() != &target {
            return Ok(cast(&col, &target)?);
        }
    }
    Ok(col)
}

fn generate_column_raw(field: &Field, rows: usize, prefix: &[ArrayRef]) -> Result<ArrayRef> {
    let prefix_len: usize = prefix.iter().map(|a| a.len()).sum();
    let n = rows.saturating_sub(prefix_len);

    let ft = field
        .field_type
        .as_ref()
        .expect("field_type unresolved; call resolve_refs before executing");

    if let Some(ref val) = field.value {
        return prepend_prefix(prefix, constant_column(ft, val, n)?);
    }

    let g = field.generator.as_ref();
    let locale = field.locale.as_ref();

    let generated: ArrayRef = match ft {
        FieldType::Number => Arc::new(Float64Array::from(
            (0..n)
                .map(|_| {
                    fake_number(
                        g,
                        field.range.as_ref().and_then(|r| r.min),
                        field.range.as_ref().and_then(|r| r.max),
                    )
                })
                .collect::<Vec<_>>(),
        )),
        FieldType::Boolean => {
            let ratio: Option<u8> = field
                .args
                .as_ref()
                .and_then(|a| a.get("ratio"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u8);
            Arc::new(BooleanArray::from(
                (0..n)
                    .map(|_| match ratio {
                        Some(r) => fake::faker::boolean::en::Boolean(r).fake::<bool>(),
                        None => Faker.fake::<bool>(),
                    })
                    .collect::<Vec<_>>(),
            ))
        }
        FieldType::String => Arc::new(StringArray::from(
            (0..n)
                .map(|_| fake_string(g, locale, field.args.as_ref()))
                .collect::<Vec<_>>(),
        )),
        FieldType::Date => {
            let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            Arc::new(Date32Array::from(
                (0..n)
                    .map(|_| {
                        let d = fake_date(field.after.as_deref(), field.before.as_deref());
                        (d - epoch).num_days() as i32
                    })
                    .collect::<Vec<_>>(),
            ))
        }
        FieldType::DateTime => Arc::new(TimestampMicrosecondArray::from(
            (0..n)
                .map(|_| {
                    fake_datetime(field.after.as_deref(), field.before.as_deref())
                        .timestamp_micros()
                })
                .collect::<Vec<_>>(),
        )),
        FieldType::Object => {
            let sub_columns: Vec<(Arc<ArrowField>, ArrayRef)> = field
                .fields
                .iter()
                .map(|sub| {
                    let col = generate_column(sub, n, &[])?;
                    Ok((Arc::new(field_to_arrow(sub)), col))
                })
                .collect::<Result<_>>()?;
            Arc::new(StructArray::from(sub_columns))
        }
        FieldType::Variant => {
            bail!(
                "variant field '{}' must be expanded before execution; call expand_field_variants first",
                field.name
            )
        }
        FieldType::List => match field.content.as_deref() {
            None => {
                let offsets = OffsetBuffer::<i32>::from_lengths(std::iter::repeat_n(0, n));
                let child = Arc::new(StringArray::from(Vec::<String>::new())) as ArrayRef;
                let child_field = Arc::new(ArrowField::new("item", DataType::Utf8, true));
                Arc::new(ListArray::new(child_field, offsets, child, None))
            }
            Some(c) if c.from.is_none() => {
                let count_spec = field.count.as_ref().cloned().unwrap_or(CountSpec::Fixed(1));
                let counts: Vec<usize> = (0..n).map(|_| sample_count(&count_spec)).collect();
                let total: usize = counts.iter().sum();
                let child_values = generate_column(&c.item, total, &[])?;
                let offsets = OffsetBuffer::<i32>::from_lengths(counts.iter().copied());
                let child_field = Arc::new(field_to_arrow(&c.item));
                Arc::new(ListArray::new(child_field, offsets, child_values, None))
            }
            Some(_) => {
                bail!(
                    "list-link field '{}' must be generated via GenerateWitness / AssembleFromWitness",
                    field.name
                )
            }
        },
    };

    prepend_prefix(prefix, generated)
}

fn prepend_prefix(prefix: &[ArrayRef], generated: ArrayRef) -> Result<ArrayRef> {
    if prefix.is_empty() {
        return Ok(generated);
    }
    let mut parts: Vec<&dyn Array> = prefix.iter().map(|a| a.as_ref() as &dyn Array).collect();
    parts.push(generated.as_ref());
    Ok(concat(&parts)?)
}

fn fake_date(after: Option<&str>, before: Option<&str>) -> chrono::NaiveDate {
    match (after, before) {
        (Some(a), Some(b)) => {
            let start = chrono::NaiveDate::parse_from_str(a, "%Y-%m-%d").expect("validated");
            let end = chrono::NaiveDate::parse_from_str(b, "%Y-%m-%d").expect("validated");
            let days = (end - start).num_days();
            let offset: i64 = (0i64..=days).fake();
            start + chrono::Duration::days(offset)
        }
        (Some(a), None) => {
            let start = chrono::NaiveDate::parse_from_str(a, "%Y-%m-%d").expect("validated");
            let offset: i64 = (0i64..=365_000i64).fake();
            start + chrono::Duration::days(offset)
        }
        (None, Some(b)) => {
            let end = chrono::NaiveDate::parse_from_str(b, "%Y-%m-%d").expect("validated");
            let offset: i64 = (0i64..=365_000i64).fake();
            end - chrono::Duration::days(offset)
        }
        (None, None) => fake::faker::chrono::en::Date().fake(),
    }
}

fn fake_datetime(after: Option<&str>, before: Option<&str>) -> chrono::DateTime<chrono::Utc> {
    match (after, before) {
        (Some(a), Some(b)) => {
            let start = chrono::DateTime::parse_from_rfc3339(a)
                .expect("validated")
                .with_timezone(&chrono::Utc);
            let end = chrono::DateTime::parse_from_rfc3339(b)
                .expect("validated")
                .with_timezone(&chrono::Utc);
            fake::faker::chrono::en::DateTimeBetween(start, end).fake()
        }
        (Some(a), None) => {
            let start = chrono::DateTime::parse_from_rfc3339(a)
                .expect("validated")
                .with_timezone(&chrono::Utc);
            fake::faker::chrono::en::DateTimeAfter(start).fake()
        }
        (None, Some(b)) => {
            let end = chrono::DateTime::parse_from_rfc3339(b)
                .expect("validated")
                .with_timezone(&chrono::Utc);
            fake::faker::chrono::en::DateTimeBefore(end).fake()
        }
        (None, None) => fake::faker::chrono::en::DateTime().fake(),
    }
}

fn fake_number(g: Option<&Generator>, min: Option<f64>, max: Option<f64>) -> f64 {
    match g {
        Some(Generator::Latitude) => fake::faker::address::en::Latitude()
            .fake::<String>()
            .parse()
            .unwrap_or(0.0),
        Some(Generator::Longitude) => fake::faker::address::en::Longitude()
            .fake::<String>()
            .parse()
            .unwrap_or(0.0),
        Some(Generator::PositiveDecimal) => Faker.fake::<f64>().abs(),
        Some(Generator::Decimal) => Faker.fake::<f64>(),
        _ => match (min, max) {
            (Some(lo), Some(hi)) => (lo..=hi).fake::<f64>(),
            (Some(lo), None) => (lo..=f64::MAX).fake::<f64>(),
            (None, Some(hi)) => (f64::MIN..=hi).fake::<f64>(),
            (None, None) => Faker.fake::<f64>(),
        },
    }
}

fn constant_column(ft: &FieldType, val: &serde_yaml::Value, n: usize) -> Result<ArrayRef> {
    match ft {
        FieldType::Number => {
            let v = val
                .as_f64()
                .ok_or_else(|| anyhow!("constant `value` for number field is not numeric"))?;
            Ok(Arc::new(Float64Array::from(vec![v; n])))
        }
        FieldType::Boolean => {
            let v = val
                .as_bool()
                .ok_or_else(|| anyhow!("constant `value` for boolean field is not a boolean"))?;
            Ok(Arc::new(BooleanArray::from(vec![v; n])))
        }
        FieldType::String => {
            let v = val
                .as_str()
                .ok_or_else(|| anyhow!("constant `value` for string field is not a string"))?;
            Ok(Arc::new(StringArray::from(vec![v; n])))
        }
        FieldType::Date => {
            let s = val.as_str().ok_or_else(|| {
                anyhow!("constant `value` for date field must be a string (YYYY-MM-DD)")
            })?;
            let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|e| anyhow!("invalid date value '{s}': {e}"))?;
            let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let days = (d - epoch).num_days() as i32;
            Ok(Arc::new(Date32Array::from(vec![days; n])))
        }
        FieldType::DateTime => {
            let s = val.as_str().ok_or_else(|| {
                anyhow!("constant `value` for date_time field must be a string (RFC 3339)")
            })?;
            let dt = chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|e| anyhow!("invalid date_time value '{s}': {e}"))?;
            Ok(Arc::new(TimestampMicrosecondArray::from(
                vec![dt.timestamp_micros(); n],
            )))
        }
        FieldType::Object | FieldType::List | FieldType::Variant => {
            bail!("constant `value` is not supported for object/list/variant fields")
        }
    }
}

/// Extract a `usize` range from `args` with given key names and defaults.
fn arg_range(
    args: Option<&HashMap<String, serde_yaml::Value>>,
    default_min: usize,
    default_max: usize,
) -> std::ops::Range<usize> {
    let min = args
        .and_then(|a| a.get("min"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(default_min);
    let max = args
        .and_then(|a| a.get("max"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(default_max);
    min..max
}

fn fake_string(
    g: Option<&Generator>,
    locale: Option<&Locale>,
    args: Option<&HashMap<String, serde_yaml::Value>>,
) -> String {
    let loc = locale.unwrap_or(&Locale::En);
    match g {
        None => Faker.fake::<String>(),
        Some(g) => match g {
            // Locale-aware generators — dispatch via locale_fake! macro.
            Generator::FirstName => locale_fake!(loc, fake::faker::name::raw::FirstName),
            Generator::LastName => locale_fake!(loc, fake::faker::name::raw::LastName),
            Generator::Name => locale_fake!(loc, fake::faker::name::raw::Name),
            Generator::NameWithTitle => locale_fake!(loc, fake::faker::name::raw::NameWithTitle),
            Generator::Word => locale_fake!(loc, fake::faker::lorem::raw::Word),
            Generator::Sentence => {
                let r = arg_range(args, 5, 10);
                locale_fake!(loc, fake::faker::lorem::raw::Sentence, r)
            }
            Generator::Paragraph => {
                let r = arg_range(args, 3, 6);
                locale_fake!(loc, fake::faker::lorem::raw::Paragraph, r)
            }
            Generator::Words => {
                let r = arg_range(args, 3, 8);
                locale_fake_join!(loc, fake::faker::lorem::raw::Words, r, " ")
            }
            Generator::Sentences => {
                let r = arg_range(args, 2, 5);
                locale_fake_join!(loc, fake::faker::lorem::raw::Sentences, r, " ")
            }
            Generator::Paragraphs => {
                let r = arg_range(args, 2, 4);
                locale_fake_join!(loc, fake::faker::lorem::raw::Paragraphs, r, "\n\n")
            }
            Generator::CompanyName => locale_fake!(loc, fake::faker::company::raw::CompanyName),
            Generator::CompanySuffix => locale_fake!(loc, fake::faker::company::raw::CompanySuffix),
            Generator::Industry => locale_fake!(loc, fake::faker::company::raw::Industry),
            Generator::Profession => locale_fake!(loc, fake::faker::company::raw::Profession),
            Generator::Buzzword => locale_fake!(loc, fake::faker::company::raw::Buzzword),
            Generator::CityName => locale_fake!(loc, fake::faker::address::raw::CityName),
            Generator::CountryName => locale_fake!(loc, fake::faker::address::raw::CountryName),
            Generator::StreetName => locale_fake!(loc, fake::faker::address::raw::StreetName),
            Generator::ZipCode => locale_fake!(loc, fake::faker::address::raw::ZipCode),
            Generator::StateAbbr => locale_fake!(loc, fake::faker::address::raw::StateAbbr),
            Generator::PhoneNumber => {
                locale_fake!(loc, fake::faker::phone_number::raw::PhoneNumber)
            }
            Generator::LicencePlate => match loc {
                Locale::FrFr => fake::faker::automotive::fr_fr::LicencePlate().fake(),
                Locale::ItIt => fake::faker::automotive::it_it::LicencePlate().fake(),
                Locale::NlNl => fake::faker::automotive::nl_nl::LicencePlate().fake(),
                _ => fake::faker::automotive::fr_fr::LicencePlate().fake(),
            },
            // Locale-agnostic generators — fixed en (or locale-independent) implementation.
            Generator::Email => fake::faker::internet::en::FreeEmail().fake(),
            Generator::Username => fake::faker::internet::en::Username().fake(),
            Generator::Password => {
                let r = arg_range(args, 8, 20);
                fake::faker::internet::en::Password(r).fake()
            }
            Generator::IPv4 => fake::faker::internet::en::IPv4().fake(),
            Generator::IPv6 => fake::faker::internet::en::IPv6().fake(),
            Generator::MacAddress => fake::faker::internet::en::MACAddress().fake(),
            Generator::UserAgent => fake::faker::internet::en::UserAgent().fake(),
            Generator::CountryCode => fake::faker::address::en::CountryCode().fake(),
            Generator::TimeZone => fake::faker::address::en::TimeZone().fake(),
            Generator::CreditCardNumber => fake::faker::creditcard::en::CreditCardNumber().fake(),
            Generator::Bic => fake::faker::finance::en::Bic().fake(),
            Generator::CurrencyCode => fake::faker::currency::en::CurrencyCode().fake(),
            Generator::CurrencyName => fake::faker::currency::en::CurrencyName().fake(),
            Generator::CurrencySymbol => fake::faker::currency::en::CurrencySymbol().fake(),
            Generator::Latitude => fake::faker::address::en::Latitude().fake(),
            Generator::Longitude => fake::faker::address::en::Longitude().fake(),
            Generator::Geohash => {
                let precision = args
                    .and_then(|a| a.get("precision"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u8)
                    .unwrap_or(6);
                fake::faker::address::en::Geohash(precision).fake()
            }
            Generator::PositiveDecimal => format!("{:.4}", Faker.fake::<f64>().abs()),
            Generator::Decimal => format!("{:.4}", Faker.fake::<f64>()),
            Generator::NumberWithFormat => {
                let fmt = args
                    .and_then(|a| a.get("format"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("###");
                fake::faker::number::en::NumberWithFormat(fmt).fake()
            }
            Generator::Uuid => Faker.fake::<uuid::Uuid>().to_string(),
            Generator::Isin => fake::faker::finance::en::Isin().fake(),
            Generator::Isbn => fake::faker::barcode::en::Isbn().fake(),
            Generator::Semver => Faker.fake::<semver::Version>().to_string(),
            Generator::Date => fake_date(None, None).format("%Y-%m-%d").to_string(),
            Generator::DateTime => fake_datetime(None, None).to_rfc3339(),
        },
    }
}
