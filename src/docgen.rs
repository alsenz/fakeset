/// Generates a machine-readable JSON description of the fakeset YAML schema.
///
/// Run via:  cargo run --bin docgen > docs/src/data/schema.json
///
/// The output is consumed by the Astro docs build to keep the YAML reference
/// page in sync with the Rust models. Update this file whenever models.rs changes.
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize)]
struct SchemaDoc {
    types: BTreeMap<String, TypeDoc>,
}

#[derive(Serialize)]
struct TypeDoc {
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fields: Option<Vec<FieldDoc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    variants: Option<Vec<EnumVariantDoc>>,
}

#[derive(Serialize)]
struct FieldDoc {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    required: bool,
    description: String,
}

#[derive(Serialize)]
struct EnumVariantDoc {
    value: String,
    description: String,
}

fn main() {
    let mut types: BTreeMap<String, TypeDoc> = BTreeMap::new();

    types.insert("SyntheticDataset".into(), TypeDoc {
        description: "Top-level dataset definition. Every YAML file defines one dataset.".into(),
        fields: Some(vec![
            f("name",        "string",           true,  "Dataset name; used as the default output filename."),
            f("format",      "Format",           true,  "Output format: parquet, csv, json, or jsonl."),
            f("rows",        "integer?",         false, "Explicit row count. Mutually exclusive with ratio on include."),
            f("locale",      "Locale?",          false, "Default locale for locale-capable generators. Field-level locale takes precedence."),
            f("output_file", "string?",          false, "Override the output filename (default: name). Datasets sharing the same output_file are unioned and shuffled."),
            f("include",     "Include?",         false, "Declares this dataset as a constrained subset of another."),
            f("links",       "Include[]",        false, "Linked datasets for list-link sampling."),
            f("data",        "Field[]",          false, "Field definitions. Evaluated in declaration order for expressions."),
            f("variants",    "VariantSchema[]",  false, "Virtually splits the dataset into N concrete variants."),
        ]),
        variants: None,
    });

    types.insert("Include".into(), TypeDoc {
        description: "Used in include: (constraint inclusion) and links: (list-link declarations).".into(),
        fields: Some(vec![
            f("file",        "string",       true,  "Path to the included YAML file, relative to this file."),
            f("ref",         "string",       true,  "Name used to reference fields: ref: <ref>.<field>."),
            f("ratio",          "float?",       false, "Marginal row-membership probability (0.0–1.0). In include: drives row count. In links: limits eligible linked-dataset rows."),
            f("cardinality",    "CountSpec?",   false, "links: only. Items to draw per outer row."),
            f("reinforcement",  "float?",       false, "links: only. Sampling intensity: 0 = without-replacement, 1 or absent = uniform (default), >1 = clumping. Values in (0,1) are invalid."),
            f("fields",         "string[]",     false, "Field names to copy as ref entries. Use [\"*\"] for all fields."),
            f("exclude",        "string[]?",    false, "Field names to suppress after fields expansion."),
        ]),
        variants: None,
    });

    types.insert("Field".into(), TypeDoc {
        description: "Defines one column in a dataset schema.".into(),
        fields: Some(vec![
            f("name",        "string",           true,  "Column name in the output file."),
            f("type",        "FieldType?",       false, "Column type. Required unless ref is set (type inherited from ref target)."),
            f("ref/refs",    "RefsSpec?",        false, "Cross-dataset field reference. ref: <ref>.<field> for a single ref; refs: for multiple or collect bindings."),
            f("generator",   "Generator?",       false, "Specific fake-rs generator. When absent, the type default is used."),
            f("locale",      "Locale?",          false, "Locale for this field's generator. Overrides dataset-level locale."),
            f("range",       "Range?",           false, "Inclusive numeric bounds for number fields."),
            f("value",       "any?",             false, "Emit this constant for every row. Incompatible with generator, range."),
            f("fields",      "Field[]",          false, "Nested fields for object type."),
            f("content",     "ListContent?",     false, "Element spec for list type."),
            f("variants",    "FieldVariant[]",   false, "Alternatives for type: variant fields."),
            f("parquet",     "ParquetConfig?",   false, "Arrow/Parquet type override."),
            f("expression",  "string?",          false, "SQL expression evaluated after all other fields are generated."),
            f("hidden",      "boolean",          false, "Present in internal batch but excluded from output."),
            f("count",       "CountSpec?",       false, "Items per row for list fields."),
            f("precision",   "integer?",         false, "Decimal precision for number fields. Positive = decimal places; negative = rounds by powers of 10."),
            f("default",     "any?",             false, "Fallback when no child provides an inherited value. List collect targets use default: []."),
        ]),
        variants: None,
    });

    types.insert("ListContent".into(), TypeDoc {
        description: "Element spec for a list field. When from: is set, activates the witness/assembly pipeline.".into(),
        fields: Some(vec![
            f("from",    "string?",  false, "Names the link ref (from links:) from which items are drawn."),
            f("project", "string?",  false, "Project a single linked field as a scalar list. Mutually exclusive with explicit fields."),
        ]),
        variants: None,
    });

    types.insert("FieldVariant".into(), TypeDoc {
        description: "One alternative within a type: variant field.".into(),
        fields: Some(vec![
            f("type",      "FieldType?",      false, "Type for this variant."),
            f("generator", "Generator?",      false, "Generator for this variant."),
            f("locale",    "Locale?",         false, "Locale for this variant."),
            f("range",     "Range?",          false, "Numeric range for this variant."),
            f("value",     "any?",            false, "Constant value for this variant."),
            f("parquet",   "ParquetConfig?",  false, "Type override for this variant. Falls back to parent field parquet."),
            f("ratio",     "float?",          false, "Fraction allocated to this choice. Unset choices share the remainder equally."),
        ]),
        variants: None,
    });

    types.insert("VariantSchema".into(), TypeDoc {
        description: "One concrete variant of a dataset (used in variants:).".into(),
        fields: Some(vec![
            f("data",   "Field[]",  false, "Fields that override or extend base data. Same-named base fields replaced; new names appended."),
            f("locale", "Locale?",  false, "Locale override for this variant's fields."),
            f("ratio",  "float?",   false, "Fraction of parent rows allocated to this variant. Unset variants share the remainder equally."),
        ]),
        variants: None,
    });

    types.insert("CountSpec".into(), TypeDoc {
        description: "Specifies a count — fixed integer, uniform range, or normal distribution.".into(),
        fields: None,
        variants: Some(vec![
            ev("5",                                  "Fixed count."),
            ev("{ min: 2, max: 8 }",                "Uniform random integer in [min, max]."),
            ev("{ mean: 5.0, std_dev: 2.0 }",       "Normal distribution, rounded and clamped ≥ 0."),
        ]),
    });

    types.insert("Range".into(), TypeDoc {
        description: "Inclusive numeric bounds for number fields. Either bound may be omitted.".into(),
        fields: Some(vec![
            f("min", "float?", false, "Lower bound (inclusive). Omit for unbounded."),
            f("max", "float?", false, "Upper bound (inclusive). Omit for unbounded."),
        ]),
        variants: None,
    });

    types.insert("ParquetConfig".into(), TypeDoc {
        description: "Override the Arrow/Parquet type for a field.".into(),
        fields: Some(vec![
            f("datatype", "ParquetDatatype", true, "Target Arrow datatype."),
        ]),
        variants: None,
    });

    types.insert("FieldType".into(), TypeDoc {
        description: "Column data type.".into(),
        fields: None,
        variants: Some(vec![
            ev("number",    "64-bit float. Use parquet.datatype to coerce to int or narrower float."),
            ev("boolean",   "Boolean."),
            ev("string",    "UTF-8 string."),
            ev("object",    "Nested struct. Requires fields: to define sub-columns."),
            ev("list",      "Array of items. Requires content: to define the element spec."),
            ev("date",      "Calendar date (Arrow Date32)."),
            ev("date_time", "Timestamp with microsecond precision (Arrow TimestampMicrosecond)."),
            ev("variant",   "Multi-alternative field. Expanded into global dataset variants before execution. Requires variants: list."),
        ]),
    });

    types.insert("Format".into(), TypeDoc {
        description: "Output file format.".into(),
        fields: None,
        variants: Some(vec![
            ev("parquet", "Apache Parquet columnar format."),
            ev("csv",     "Comma-separated values."),
            ev("json",    "JSON array of objects."),
            ev("jsonl",   "Newline-delimited JSON (one object per line)."),
        ]),
    });

    types.insert("Reducer".into(), TypeDoc {
        description: "How values are assembled when a ref spans a cardinality boundary.".into(),
        fields: None,
        variants: Some(vec![
            ev("take_one (alias: take_first)", "Take the first value."),
            ev("sum",     "Sum numeric values."),
            ev("max",     "Maximum value."),
            ev("min",     "Minimum value."),
            ev("collect", "Gather all values as a list."),
        ]),
    });

    types.insert("ParquetDatatype".into(), TypeDoc {
        description: "Valid values for parquet.datatype.".into(),
        fields: None,
        variants: Some(vec![
            ev("int8",         "Arrow Int8."),
            ev("int16",        "Arrow Int16."),
            ev("int32",        "Arrow Int32."),
            ev("int64",        "Arrow Int64."),
            ev("uint8",        "Arrow UInt8."),
            ev("uint16",       "Arrow UInt16."),
            ev("uint32",       "Arrow UInt32."),
            ev("uint64",       "Arrow UInt64."),
            ev("float32",      "Arrow Float32."),
            ev("float64",      "Arrow Float64."),
            ev("utf8",         "Arrow Utf8 (string)."),
            ev("boolean",      "Arrow Boolean."),
            ev("date32",       "Arrow Date32."),
            ev("timestamp_ms", "Arrow Timestamp (millisecond)."),
            ev("timestamp_us", "Arrow Timestamp (microsecond)."),
        ]),
    });

    types.insert("Generator".into(), TypeDoc {
        description: "Specific fake-rs generator. See the Generators & Locales reference for details.".into(),
        fields: None,
        variants: Some(vec![
            ev("first_name",         "Locale-aware given name."),
            ev("last_name",          "Locale-aware family name."),
            ev("name",               "Locale-aware full name."),
            ev("name_with_title",    "Locale-aware full name with title."),
            ev("word",               "Locale-aware lorem word."),
            ev("sentence",           "Locale-aware lorem sentence."),
            ev("paragraph",          "Locale-aware lorem paragraph."),
            ev("company_name",       "Locale-aware company name."),
            ev("company_suffix",     "Locale-aware company suffix."),
            ev("industry",           "Locale-aware industry."),
            ev("profession",         "Locale-aware profession."),
            ev("buzzword",           "Locale-aware business buzzword."),
            ev("email",              "Email address."),
            ev("username",           "Internet username."),
            ev("password",           "Password (8–20 chars)."),
            ev("ipv4",               "IPv4 address."),
            ev("ipv6",               "IPv6 address."),
            ev("mac_address",        "MAC address."),
            ev("user_agent",         "HTTP User-Agent string."),
            ev("city_name",          "Locale-aware city name."),
            ev("country_name",       "Locale-aware country name."),
            ev("country_code",       "ISO 3166-1 alpha-2 country code."),
            ev("street_name",        "Locale-aware street name."),
            ev("zip_code",           "Locale-aware postal code."),
            ev("state_abbr",         "Locale-aware state/province abbreviation."),
            ev("time_zone",          "IANA time zone name."),
            ev("phone_number",       "Locale-aware phone number."),
            ev("credit_card_number", "Credit card number."),
            ev("bic",                "Bank Identifier Code."),
            ev("currency_code",      "ISO 4217 currency code."),
            ev("currency_name",      "Currency name."),
            ev("currency_symbol",    "Currency symbol."),
            ev("latitude",           "Latitude (number or string)."),
            ev("longitude",          "Longitude (number or string)."),
            ev("positive_decimal",   "Positive float (number or string)."),
            ev("decimal",            "Float, possibly negative (number or string)."),
            ev("uuid",               "UUID v4."),
            ev("isin",               "ISIN financial identifier."),
            ev("licence_plate",      "Licence plate (fr_fr, it_it, nl_nl locales supported)."),
            ev("isbn",               "ISBN barcode."),
            ev("semver",             "Semantic version string."),
            ev("date",               "Random date (date type) or ISO 8601 string."),
            ev("date_time",          "Random timestamp (date_time type) or RFC 3339 string."),
        ]),
    });

    types.insert("Locale".into(), TypeDoc {
        description: "BCP-47-style locale tag. Only valid on generators that support locale selection.".into(),
        fields: None,
        variants: Some(vec![
            ev("en",    "English."),
            ev("fr_fr", "French (France)."),
            ev("de_de", "German (Germany)."),
            ev("it_it", "Italian (Italy)."),
            ev("nl_nl", "Dutch (Netherlands)."),
            ev("pt_br", "Portuguese (Brazil)."),
            ev("pt_pt", "Portuguese (Portugal)."),
            ev("cy_gb", "Welsh (UK)."),
            ev("zh_cn", "Chinese (Simplified)."),
            ev("zh_tw", "Chinese (Traditional)."),
            ev("ja_jp", "Japanese."),
            ev("ar_sa", "Arabic (Saudi Arabia)."),
            ev("tr_tr", "Turkish."),
            ev("fa_ir", "Persian (Iran)."),
        ]),
    });

    let doc = SchemaDoc { types };
    println!("{}", serde_json::to_string_pretty(&doc).expect("JSON serialization failed"));
}

fn f(name: &str, ty: &str, required: bool, description: &str) -> FieldDoc {
    FieldDoc {
        name: name.into(),
        ty: ty.into(),
        required,
        description: description.into(),
    }
}

fn ev(value: &str, description: &str) -> EnumVariantDoc {
    EnumVariantDoc {
        value: value.into(),
        description: description.into(),
    }
}
