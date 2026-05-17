use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Specifies how many items to generate per list row.
///
/// Deserialized from either a plain integer (`count: 5`), a uniform range
/// (`count: {min: 2, max: 8}`), or a normal distribution
/// (`count: {mean: 5.0, std_dev: 2.0}` — rounded and clamped to ≥ 0).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CountSpec {
    Fixed(usize),
    Normal { mean: f64, std_dev: f64 },
    Uniform { min: usize, max: usize },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Parquet,
    Csv,
    Json,
    Jsonl,
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Format::Parquet => "parquet",
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Jsonl => "jsonl",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Number,
    Boolean,
    String,
    Object,
    List,
    Date,
    DateTime,
}

impl std::fmt::Display for FieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldType::Number => write!(f, "number"),
            FieldType::Boolean => write!(f, "boolean"),
            FieldType::String => write!(f, "string"),
            FieldType::Object => write!(f, "object"),
            FieldType::List => write!(f, "list"),
            FieldType::Date => write!(f, "date"),
            FieldType::DateTime => write!(f, "date_time"),
        }
    }
}

/// Selects a specific fake-rs faker to drive value generation.
/// When absent the field type's default faker is used.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Generator {
    // name — string only
    FirstName,
    LastName,
    Name,
    NameWithTitle,
    // lorem — string only
    Word,
    Sentence,
    Paragraph,
    // company — string only
    CompanyName,
    CompanySuffix,
    Industry,
    Profession,
    Buzzword,
    // internet — string only
    Email,
    Username,
    Password,
    #[serde(rename = "ipv4")]
    IPv4,
    #[serde(rename = "ipv6")]
    IPv6,
    MacAddress,
    UserAgent,
    // address — string only
    CityName,
    CountryName,
    CountryCode,
    StreetName,
    ZipCode,
    StateAbbr,
    TimeZone,
    // phone — string only
    PhoneNumber,
    // finance — string only
    CreditCardNumber,
    Bic,
    CurrencyCode,
    CurrencyName,
    CurrencySymbol,
    // geo — number or string
    Latitude,
    Longitude,
    // numeric — number or string
    PositiveDecimal,
    Decimal,
    // identity / codes — string only
    Uuid,
    Isin,
    LicencePlate,
    Isbn,
    Semver,
    // temporal — date/date_time or string
    Date,
    DateTime,
}

impl Generator {
    /// Returns true when this generator is compatible with the given field type.
    pub fn valid_for(&self, ft: &FieldType) -> bool {
        match self {
            Generator::FirstName | Generator::LastName | Generator::Name | Generator::NameWithTitle
            | Generator::Word | Generator::Sentence | Generator::Paragraph
            | Generator::CompanyName | Generator::CompanySuffix | Generator::Industry
            | Generator::Profession | Generator::Buzzword
            | Generator::Email | Generator::Username | Generator::Password
            | Generator::IPv4 | Generator::IPv6 | Generator::MacAddress | Generator::UserAgent
            | Generator::CityName | Generator::CountryName | Generator::CountryCode
            | Generator::StreetName | Generator::ZipCode | Generator::StateAbbr | Generator::TimeZone
            | Generator::PhoneNumber | Generator::CreditCardNumber | Generator::Bic
            | Generator::CurrencyCode | Generator::CurrencyName | Generator::CurrencySymbol
            | Generator::Uuid | Generator::Isin | Generator::LicencePlate | Generator::Isbn
            | Generator::Semver => matches!(ft, FieldType::String),
            Generator::Latitude | Generator::Longitude | Generator::PositiveDecimal
            | Generator::Decimal => matches!(ft, FieldType::Number | FieldType::String),
            Generator::Date => matches!(ft, FieldType::Date | FieldType::String),
            Generator::DateTime => matches!(ft, FieldType::DateTime | FieldType::String),
        }
    }

    /// Returns true when this generator produces meaningfully locale-specific output.
    /// Setting `locale` on a generator that returns false here is a validation error.
    pub fn supports_locale(&self) -> bool {
        matches!(self,
            Generator::FirstName | Generator::LastName | Generator::Name | Generator::NameWithTitle
            | Generator::Word | Generator::Sentence | Generator::Paragraph
            | Generator::CompanyName | Generator::CompanySuffix
            | Generator::Industry | Generator::Profession | Generator::Buzzword
            | Generator::CityName | Generator::CountryName | Generator::StreetName
            | Generator::ZipCode | Generator::StateAbbr
            | Generator::PhoneNumber
            | Generator::LicencePlate
        )
    }
}

/// BCP-47-style locale tag controlling which fake-rs locale data is used.
/// Only valid on generators that return true from [`Generator::supports_locale`].
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Locale {
    En, FrFr, DeDe, ItIt, NlNl, PtBr, PtPt, CyGb,
    ZhCn, ZhTw, JaJp, ArSa, TrTr, FaIr,
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Locale::En   => "en",   Locale::FrFr => "fr_fr",
            Locale::DeDe => "de_de", Locale::ItIt => "it_it",
            Locale::NlNl => "nl_nl", Locale::PtBr => "pt_br",
            Locale::PtPt => "pt_pt", Locale::CyGb => "cy_gb",
            Locale::ZhCn => "zh_cn", Locale::ZhTw => "zh_tw",
            Locale::JaJp => "ja_jp", Locale::ArSa => "ar_sa",
            Locale::TrTr => "tr_tr", Locale::FaIr => "fa_ir",
        };
        write!(f, "{s}")
    }
}

impl std::fmt::Display for Generator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Generator::FirstName => "first_name",
            Generator::LastName => "last_name",
            Generator::Name => "name",
            Generator::NameWithTitle => "name_with_title",
            Generator::Word => "word",
            Generator::Sentence => "sentence",
            Generator::Paragraph => "paragraph",
            Generator::CompanyName => "company_name",
            Generator::CompanySuffix => "company_suffix",
            Generator::Industry => "industry",
            Generator::Profession => "profession",
            Generator::Buzzword => "buzzword",
            Generator::Email => "email",
            Generator::Username => "username",
            Generator::Password => "password",
            Generator::IPv4 => "ipv4",
            Generator::IPv6 => "ipv6",
            Generator::MacAddress => "mac_address",
            Generator::UserAgent => "user_agent",
            Generator::CityName => "city_name",
            Generator::CountryName => "country_name",
            Generator::CountryCode => "country_code",
            Generator::StreetName => "street_name",
            Generator::ZipCode => "zip_code",
            Generator::StateAbbr => "state_abbr",
            Generator::TimeZone => "time_zone",
            Generator::PhoneNumber => "phone_number",
            Generator::CreditCardNumber => "credit_card_number",
            Generator::Bic => "bic",
            Generator::CurrencyCode => "currency_code",
            Generator::CurrencyName => "currency_name",
            Generator::CurrencySymbol => "currency_symbol",
            Generator::Latitude => "latitude",
            Generator::Longitude => "longitude",
            Generator::PositiveDecimal => "positive_decimal",
            Generator::Decimal => "decimal",
            Generator::Uuid => "uuid",
            Generator::Isin => "isin",
            Generator::LicencePlate => "licence_plate",
            Generator::Isbn => "isbn",
            Generator::Semver => "semver",
            Generator::Date => "date",
            Generator::DateTime => "date_time",
        };
        write!(f, "{s}")
    }
}

/// Inclusive numeric bounds for `number` type fields.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Range {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Field {
    #[serde(default)]
    pub name: String,
    /// Concrete type. Absent when `ref_field` is set; populated by the rewrite
    /// pass before execution.
    #[serde(rename = "type")]
    pub field_type: Option<FieldType>,
    /// Cross-dataset field reference in the form `"include_ref.field_name"`.
    /// When set, `field_type`, `fields`, and `content` must all be absent in
    /// the YAML; they are filled in by `resolve_refs`.
    #[serde(rename = "ref")]
    pub ref_field: Option<String>,
    /// Selects a specific fake-rs faker. Absent means the type default is used.
    pub generator: Option<Generator>,
    /// Locale for fake-rs data. Only valid on generators that support locale selection;
    /// see [`Generator::supports_locale`]. Defaults to `en` when absent.
    pub locale: Option<Locale>,
        /// Numeric bounds for `number` type fields.
    pub range: Option<Range>,
    /// Emit this constant value for every row instead of generating data.
    /// Incompatible with `generator`, `min`, and `max`.
    pub value: Option<serde_yaml::Value>,
    /// Nested fields for object type — written directly as `fields:` in YAML.
    #[serde(default)]
    pub fields: Vec<Field>,
    /// Element spec for list fields.
    pub content: Option<Box<ListContent>>,
    /// SQL expression evaluated against the batch after all other fields are generated.
    /// Variables must refer to fields defined above this one in the YAML (evaluation order).
    /// Mutually exclusive with `type`, `ref`, `generator`, `min`, `max`, and `value`.
    pub expression: Option<String>,
    /// When true, this field is present in the internal batch but excluded from output.
    /// Set automatically by the expression dependency pull-down pass for fields that are
    /// needed to evaluate an expression but not otherwise declared.
    #[serde(default)]
    pub hidden: bool,
    /// Number of items per row for `list` type fields. Ignored on all other field types.
    pub count: Option<CountSpec>,
}

/// Split a ref-field string of the form `"include_ref.field_name"` into its
/// two parts. Returns `None` if there is no `.` separator.
pub(crate) fn split_ref(s: &str) -> Option<(&str, &str)> {
    let dot = s.find('.')?;
    Some((&s[..dot], &s[dot + 1..]))
}

/// Resolve an include's file path relative to the dataset that declares it.
/// Returns `None` if the file does not exist or cannot be canonicalized.
pub(crate) fn resolve_include(dataset_path: &Path, file: &str) -> Option<PathBuf> {
    dataset_path
        .parent()
        .unwrap_or(Path::new(""))
        .join(file)
        .canonicalize()
        .ok()
}

/// Element spec for a `list` type field: a [`Field`] plus an optional set of includes.
///
/// When `includes` is non-empty this is a **rich list**: each list item is a struct whose
/// fields may be sourced from an included dataset (include-scoped ref: `ref: include_ref.field`)
/// or from the enclosing outer row (outer-scoped ref: `ref: field`). Include-scoped field names
/// live under `fields:`; the rich-list pipeline (GenerateInnerFlat / AssembleRichList) handles
/// generation and assembly.
///
/// When `includes` is empty this is a **simple list**: items are generated directly from the
/// `item` field spec, exactly as a plain field would be.
#[derive(Debug, Clone, Deserialize)]
pub struct ListContent {
    /// When non-empty, marks this as a rich list. Each entry names a dataset
    /// whose rows supply values for include-scoped ref fields (`ref: include_ref.field`).
    /// Set `distribution` on an include to narrow the sampled subset.
    #[serde(default)]
    pub includes: Vec<Include>,
    #[serde(flatten)]
    pub item: Field,
}

pub type Schema = Vec<Field>;

#[derive(Debug, Clone, Deserialize)]
pub struct Include {
    pub file: String,
    #[serde(rename = "ref")]
    pub reference: String,
    /// Fraction of the included population this dataset represents (0.0–1.0).
    /// When set on two or more siblings that share a common parent, the executor
    /// injects a Bernoulli segmentation to correctly model the overlap.
    pub distribution: Option<f64>,
}

/// One concrete variant of a dataset: a data schema fragment (merged on top of the
/// dataset's base `data`), an optional locale override, and an optional row-fraction.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VariantSchema {
    /// Fields that override or extend the dataset's base `data` for this variant.
    /// Same-named base fields are replaced; new names are appended.
    #[serde(default)]
    pub data: Schema,
    /// Locale override for this variant's fields. Falls back to the dataset-level locale.
    pub locale: Option<Locale>,
    /// Fraction of the parent dataset's rows this variant receives (0.0–1.0).
    /// Variants without a distribution share the remainder equally.
    /// All distributions must sum to ≤ 1.0; if all are set they must sum to exactly 1.0.
    pub distribution: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyntheticDataset {
    pub name: String,
    pub format: Format,
    /// Default locale applied to every locale-capable generator in this dataset
    /// that does not set its own explicit `locale`. Field-level `locale` takes precedence.
    pub locale: Option<Locale>,
    /// Explicit row count. Must not be set when any include specifies a
    /// `distribution` — in that case rows are derived from the parent size.
    pub rows: Option<usize>,
    /// When true, this dataset is intermediate and its output is not written.
    #[serde(default)]
    pub skip: bool,
    /// Write output appended into this named file rather than a per-dataset
    /// file, allowing multiple combinatorial factor datasets to be unioned
    /// and randomly shuffled into one output.
    pub output_file: Option<String>,
    #[serde(default)]
    pub includes: Vec<Include>,
    #[serde(default)]
    pub data: Schema,
    /// When non-empty, the dataset is virtually split into N concrete variants at plan time.
    /// Each variant inherits the base `data` fields and overrides/extends them with its own.
    /// All variant outputs are written to the same output file and shuffled together.
    #[serde(default)]
    pub variants: Vec<VariantSchema>,
}