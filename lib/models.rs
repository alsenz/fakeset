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

/// Returns the expected (mean) value of a `CountSpec` for row-count planning purposes.
/// This is a planning estimate, not a stochastic sample — use `generator::sample_count`
/// at execution time when an actual draw is needed.
pub fn expected_cardinality(spec: &CountSpec) -> f64 {
    match spec {
        CountSpec::Fixed(n)             => *n as f64,
        CountSpec::Uniform { min, max } => (*min + *max) as f64 / 2.0,
        CountSpec::Normal  { mean, .. } => *mean,
    }
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

impl std::str::FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "parquet" => Ok(Format::Parquet),
            "csv"     => Ok(Format::Csv),
            "json"    => Ok(Format::Json),
            "jsonl"   => Ok(Format::Jsonl),
            _ => Err(format!("unknown format '{s}'; expected one of: parquet, csv, json, jsonl")),
        }
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
    /// A field whose value is drawn from one of several alternatives.
    /// Expanded into global dataset variants by `expand_field_variants`
    /// before execution; never reaches the generator.
    Variant,
}

impl std::fmt::Display for FieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldType::Number   => write!(f, "number"),
            FieldType::Boolean  => write!(f, "boolean"),
            FieldType::String   => write!(f, "string"),
            FieldType::Object   => write!(f, "object"),
            FieldType::List     => write!(f, "list"),
            FieldType::Date     => write!(f, "date"),
            FieldType::DateTime => write!(f, "date_time"),
            FieldType::Variant  => write!(f, "variant"),
        }
    }
}

/// Arrow/Parquet datatype override for a field.
/// When set, controls the Arrow schema type used for that field instead of the
/// type inferred from `field_type`.  Applies during both schema construction and
/// column generation (the generated values are cast to the target type).
///
/// Valid on any field, not just variants.  Useful for coercing `number` fields
/// to specific integer or float widths, or for forcing a consistent type across
/// variant choices that would otherwise produce mixed types.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ParquetConfig {
    pub datatype: ParquetDatatype,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub enum ParquetDatatype {
    #[serde(rename = "int8")]         Int8,
    #[serde(rename = "int16")]        Int16,
    #[serde(rename = "int32")]        Int32,
    #[serde(rename = "int64")]        Int64,
    #[serde(rename = "uint8")]        UInt8,
    #[serde(rename = "uint16")]       UInt16,
    #[serde(rename = "uint32")]       UInt32,
    #[serde(rename = "uint64")]       UInt64,
    #[serde(rename = "float32")]      Float32,
    #[serde(rename = "float64")]      Float64,
    #[serde(rename = "utf8")]         Utf8,
    #[serde(rename = "boolean")]      Boolean,
    #[serde(rename = "date32")]       Date32,
    #[serde(rename = "timestamp_ms")] TimestampMs,
    #[serde(rename = "timestamp_us")] TimestampUs,
}

/// One concrete alternative within a `type: variant` field.
///
/// Carries the field properties for this choice (type, generator, value, range,
/// locale, parquet) plus an optional distribution weight.  Name is inherited from
/// the parent field.  Nested `variants` and structural properties (`fields`,
/// `content`, `expression`, `ref`) are not permitted inside a variant choice.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FieldVariant {
    #[serde(rename = "type")]
    pub field_type: Option<FieldType>,
    pub generator: Option<Generator>,
    pub locale: Option<Locale>,
    pub range: Option<Range>,
    pub value: Option<serde_yaml::Value>,
    /// Parquet type override for this specific choice.  If absent, the outer
    /// field's `parquet` annotation is used as a fallback.
    pub parquet: Option<ParquetConfig>,
    /// Fraction of this variant field's population allocated to this choice.
    /// Free slots (None) share the remainder equally.  Must sum to ≤ 1.0;
    /// if all are set they must sum to exactly 1.0.
    #[serde(alias = "distribution")]
    pub ratio: Option<f64>,
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

/// Specifies one or more ref bindings on a field.
///
/// `Single` covers the common case (`ref: include_ref.field`) and `Multi` supports
/// multiple entries including collect reducer bindings (`refs: [...]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RefsSpec {
    Single(String),
    Multi(Vec<RefEntry>),
}

/// One entry in a multi-ref `refs:` list.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RefEntry {
    Simple(String),
    Rich(RefBinding),
}

/// A rich ref binding — carries an optional type-source target and an optional collect binding.
#[derive(Debug, Clone, Deserialize)]
pub struct RefBinding {
    /// The ref target (`"include_ref.field_name"`). Absent on bind-only entries.
    pub target: Option<String>,
    /// The collect target (`"pool_ref.field_name"`). Used with `reducer: collect`.
    pub bind: Option<String>,
    pub reducer: Option<Reducer>,
}

/// How values are assembled when the referenced include has a different cardinality.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reducer {
    TakeFirst,
    Sum,
    Max,
    Min,
    Collect,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Field {
    #[serde(default)]
    pub name: String,
    /// Concrete type. Absent when `refs` is set; populated by the rewrite
    /// pass before execution.
    #[serde(rename = "type")]
    pub field_type: Option<FieldType>,
    /// Cross-dataset field reference(s). `ref` is a serde alias for backward-compat.
    /// Use `simple_ref()` to get the type-sourcing ref string; use `collect_bindings()`
    /// for collect reducer targets.
    #[serde(alias = "ref")]
    pub refs: Option<RefsSpec>,
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
    /// For `type: variant` fields: the list of alternative field definitions.
    /// Expanded into global dataset variants by `expand_field_variants` before execution.
    /// Must be empty on all other field types.
    #[serde(default)]
    pub variants: Vec<FieldVariant>,
    /// Parquet/Arrow datatype override.  When set, the field's Arrow schema uses
    /// this type and generated values are cast to it.  Valid on any field type.
    pub parquet: Option<ParquetConfig>,
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
    /// Decimal precision for `number` fields. Positive = decimal places; negative = round by
    /// powers of 10 (e.g. -2 rounds to the nearest 100). Applied after generation.
    /// Ignored on non-number fields.
    pub precision: Option<i32>,
    /// Default value used when this field is not prefilled by any child.
    /// Must be type-compatible with `field_type`. List fields use `default: []`
    /// as the empty-collect fallback required by `reducer: collect` bindings.
    pub default: Option<serde_yaml::Value>,
}

impl Field {
    /// Returns true when this field is a list whose items are drawn from a linked dataset
    /// (i.e. `content.from:` is set).
    pub fn is_list_link(&self) -> bool {
        self.content.as_deref().is_some_and(|c| c.from.is_some())
    }

    /// Returns the type-sourcing ref string — the first non-bind-only target from `refs`, if any.
    pub fn simple_ref(&self) -> Option<&str> {
        match &self.refs {
            Some(RefsSpec::Single(s)) => Some(s.as_str()),
            Some(RefsSpec::Multi(entries)) => entries.iter().find_map(|e| match e {
                RefEntry::Simple(s)  => Some(s.as_str()),
                RefEntry::Rich(b)    => b.target.as_deref(),
            }),
            None => None,
        }
    }

    /// Returns all collect bindings declared on this field.
    /// Returns all ref bindings that carry a `reducer` (i.e. planning annotations, not
    /// type-sourcing refs). This includes all reducer variants: `collect`, `sum`, `max`,
    /// `min`, and `take_first`.
    pub fn collect_bindings(&self) -> Vec<&RefBinding> {
        match &self.refs {
            Some(RefsSpec::Multi(entries)) => entries.iter().filter_map(|e| match e {
                RefEntry::Rich(b) if b.reducer.is_some() => Some(b),
                _ => None,
            }).collect(),
            _ => vec![],
        }
    }
}

/// Fill in free-slot distributions: `None` entries share the remainder after fixed entries equally.
pub fn resolve_distributions(dists: &[Option<f64>]) -> Vec<f64> {
    let fixed_sum: f64 = dists.iter().filter_map(|d| *d).sum();
    let n_free = dists.iter().filter(|d| d.is_none()).count();
    let free_share = if n_free > 0 { (1.0 - fixed_sum) / n_free as f64 } else { 0.0 };
    dists.iter().map(|d| d.unwrap_or(free_share)).collect()
}

/// Call `visitor(field, link, item_fields)` for every list-link field (`content.from:` set)
/// found by recursing through `fields`, pairing each with the matching link from `links`.
/// Also recurses into content item fields and object sub-fields.
pub fn for_each_list_link<'a>(
    links: &'a [Include],
    fields: &'a [Field],
    visitor: &mut impl FnMut(&'a Field, &'a Include, &'a [Field]),
) {
    for field in fields {
        if let Some(content) = &field.content {
            if let Some(from_ref) = &content.from {
                if let Some(link) = links.iter().find(|l| &l.reference == from_ref) {
                    visitor(field, link, &content.item.fields);
                }
            }
            for_each_list_link(links, &content.item.fields, visitor);
        }
        for_each_list_link(links, &field.fields, visitor);
    }
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

/// Element spec for a `list` type field.
///
/// When `from` is set this is a **list-link field**: each list item is a struct whose fields
/// may be sourced from the linked dataset named by `from` (linked-dataset ref: `ref: <ref>.field`)
/// or from the enclosing outer row (outer-scoped ref: `ref: field`). The named linked dataset
/// must be declared in the parent dataset's `links:` list. The witness/assembly pipeline
/// (`GenerateWitness` / `AssembleFromWitness`) handles generation and assembly.
///
/// When `from` is absent this is a **scalar list**: items are generated directly from the
/// `item` field spec, exactly as a plain field would be.
#[derive(Debug, Clone, Deserialize)]
pub struct ListContent {
    /// When set, names the link (by its `ref:` value in the dataset's `links:` list) from
    /// which list items are drawn. Marks this as a list-link field.
    pub from: Option<String>,
    /// Project a single field from the linked dataset, producing a scalar list instead of a
    /// list of structs. Value must be `"<link_ref>.<field_name>"`. Mutually exclusive with
    /// explicit `fields` in `item`.
    pub project: Option<String>,
    #[serde(flatten)]
    pub item: Field,
}

/// One concrete variant of a dataset: a data schema fragment (merged on top of the
/// dataset's base `data`), an optional locale override, and an optional row-fraction.
pub type Schema = Vec<Field>;

#[derive(Debug, Clone, Deserialize)]
pub struct Include {
    pub file: String,
    #[serde(rename = "ref")]
    pub reference: String,
    /// Marginal row-membership probability (0.0–1.0). When set on two or more siblings that
    /// share a common parent, the executor uses Bernoulli segmentation + IPF to correctly
    /// model the overlap.
    #[serde(alias = "distribution")]
    pub ratio: Option<f64>,
    /// How many times each child row is replicated into the parent batch (top-level include),
    /// or how many items to draw per outer row (links entry used as a pool partner).
    pub cardinality: Option<CountSpec>,
    /// Sampling intensity: 0 = without-replacement, 1 = uniform, >1 = clumping.
    /// Model field only; execution deferred to MULT-2.
    pub reinforcement: Option<f64>,
    /// Field names to automatically copy from the included dataset into this dataset's `data`
    /// (driver `include`) or into the associated list field's `content.fields` (list links).
    /// Use `["*"]` to copy all fields; otherwise list specific field names.
    /// Each matched field becomes a `ref: <ref>.<field>` entry, resolved in the rewrite phase.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Field names to suppress after wildcard/pattern expansion.
    /// Only valid when `fields` is non-empty.
    pub exclude: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VariantSchema {
    /// Fields that override or extend the dataset's base `data` for this variant.
    /// Same-named base fields are replaced; new names are appended.
    #[serde(default)]
    pub data: Schema,
    /// Locale override for this variant's fields. Falls back to the dataset-level locale.
    pub locale: Option<Locale>,
    /// Fraction of the parent dataset's rows this variant receives (0.0–1.0).
    /// Variants without a ratio share the remainder equally.
    /// All ratios must sum to ≤ 1.0; if all are set they must sum to exactly 1.0.
    #[serde(alias = "distribution")]
    pub ratio: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyntheticDataset {
    pub name: String,
    pub format: Format,
    /// Default locale applied to every locale-capable generator in this dataset
    /// that does not set its own explicit `locale`. Field-level `locale` takes precedence.
    pub locale: Option<Locale>,
    /// Explicit row count. Must not be set when any include specifies a
    /// `ratio` — in that case rows are derived from the parent size.
    pub rows: Option<usize>,
    /// Write output appended into this named file rather than a per-dataset
    /// file, allowing multiple combinatorial factor datasets to be unioned
    /// and randomly shuffled into one output.
    pub output_file: Option<String>,
    pub include: Option<Include>,
    /// Linked datasets for junction or list-link sampling.
    /// Each entry names a dataset (by file + ref) from which atoms draw linked-dataset values.
    /// A link referenced by a `content.from:` field is a **list link** (witness/assembly pipeline).
    /// A link with no `content.from:` reference is a **junction link** (activated in MULT-2).
    #[serde(default)]
    pub links: Vec<Include>,
    #[serde(default)]
    pub data: Schema,
    /// When non-empty, the dataset is virtually split into N concrete variants at plan time.
    /// Each variant inherits the base `data` fields and overrides/extends them with its own.
    /// All variant outputs are written to the same output file and shuffled together.
    #[serde(default)]
    pub variants: Vec<VariantSchema>,
}