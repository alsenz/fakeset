//! Data model: all YAML-deserialisable types (`SyntheticDataset`, `Field`, `Include`,
//! `Schema`, `CountSpec`, `Reducer`, …) plus lattice-traversal helpers (`for_each_content_include`,
//! `resolve_include`) and the `links:`-visitor used by planner and executor.
use serde::Deserialize;
use std::collections::HashMap;
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
        CountSpec::Fixed(n) => *n as f64,
        CountSpec::Uniform { min, max } => (*min + *max) as f64 / 2.0,
        CountSpec::Normal { mean, .. } => *mean,
    }
}

/// Compute the number of eligible linked-dataset rows after applying the declared `ratio`.
///
/// This is the single canonical formula used at plan time (`check_cardinality_feasibility`)
/// and execution time (`execute_witness`, `inject_linked_idx`). When no ratio is declared,
/// all rows are eligible. When `linked_rows == 0`, returns 0 regardless of ratio.
pub fn eligible_linked_rows(linked_rows: usize, ratio: Option<f64>) -> usize {
    match ratio {
        Some(r) => ((r * linked_rows as f64).round() as usize)
            .max(1)
            .min(linked_rows),
        None => linked_rows,
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
            "csv" => Ok(Format::Csv),
            "json" => Ok(Format::Json),
            "jsonl" => Ok(Format::Jsonl),
            _ => Err(format!(
                "unknown format '{s}'; expected one of: parquet, csv, json, jsonl"
            )),
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
    /// A heterogeneous (multi-type) tagged union — a `type: variant` field whose cases
    /// span more than one type, produced internally by `expand_field_variants` (VAR-1).
    /// The concrete per-case specs live in [`Field::union_cases`]; the column is
    /// materialised as an Arrow `DenseUnion`. Never written in YAML, so skipped by serde.
    #[serde(skip)]
    Union,
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
            FieldType::Variant => write!(f, "variant"),
            FieldType::Union => write!(f, "union"),
        }
    }
}

/// Output column name for the materialised case tag of a `flatten` variant under the
/// `discriminant` strategy (VAR-UNIFY): a **visible** output column naming the active case
/// per row. (Tagged-union *exclusivity* needs no sentinel — it is structural in the DFS, so no
/// internal discriminant column is ever materialised.)
pub fn discriminant_tag_column(union_field: &str) -> String {
    format!("{union_field}_case")
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
    #[serde(rename = "int8")]
    Int8,
    #[serde(rename = "int16")]
    Int16,
    #[serde(rename = "int32")]
    Int32,
    #[serde(rename = "int64")]
    Int64,
    #[serde(rename = "uint8")]
    UInt8,
    #[serde(rename = "uint16")]
    UInt16,
    #[serde(rename = "uint32")]
    UInt32,
    #[serde(rename = "uint64")]
    UInt64,
    #[serde(rename = "float32")]
    Float32,
    #[serde(rename = "float64")]
    Float64,
    #[serde(rename = "utf8")]
    Utf8,
    #[serde(rename = "boolean")]
    Boolean,
    #[serde(rename = "date32")]
    Date32,
    #[serde(rename = "timestamp_ms")]
    TimestampMs,
    #[serde(rename = "timestamp_us")]
    TimestampUs,
}

/// How a `flatten`ed **variant** field's case fields are laid out in a flat columnar
/// output (Parquet/CSV), where one schema must cover every row (VAR-UNIFY). JSON/JSONL
/// ignore this — they emit per-row keys (only the active case's fields) regardless.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub enum FlattenStrategy {
    /// Case fields side by side as a nullable superset; the populated set is the case tag.
    /// Cross-case name collisions are rejected at validation (use `prefixed`).
    #[serde(rename = "superset")]
    #[default]
    Superset,
    /// Prefix each pulled-up field by its case name (`<case>_<field>`), so cases with
    /// same-named fields don't collide.
    #[serde(rename = "prefixed")]
    Prefixed,
    /// Superset layout plus a materialised `<field>_case` string column naming the active
    /// case per row.
    #[serde(rename = "discriminant")]
    Discriminant,
}

/// One concrete alternative within a `type: variant` field.
///
/// Carries the field properties for this choice (type, generator, value, range,
/// locale, parquet, and — for object cases — nested `fields`) plus an optional
/// distribution weight.  Name is inherited from the parent field.  Nested `variants`
/// and the other structural properties (`content`, `expression`, `ref`) are not
/// permitted inside a variant choice.
///
/// A choice carrying `fields:` (or `type: object`) is an **object case** of a
/// heterogeneous tagged union (VAR-1): such variants lower to a `FieldType::Union`
/// rather than the same-type cross-product path.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FieldVariant {
    /// Optional case label. For a **heterogeneous union** (VAR-1) it names the case in
    /// the output (the union child / nullable-superset sub-field); without it, cases are
    /// named positionally (`<field>_<i>`). Ignored for same-type variants.
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub field_type: Option<FieldType>,
    pub generator: Option<Generator>,
    pub locale: Option<Locale>,
    pub range: Option<Range>,
    pub value: Option<serde_yaml::Value>,
    /// Nested fields for an **object case** of a heterogeneous union (VAR-1).
    /// Empty for scalar cases.
    #[serde(default)]
    pub fields: Vec<Field>,
    /// Parquet type override for this specific choice.  If absent, the outer
    /// field's `parquet` annotation is used as a fallback.
    pub parquet: Option<ParquetConfig>,
    /// Fraction of this variant field's population allocated to this choice.
    /// Free slots (None) share the remainder equally.  Must sum to ≤ 1.0;
    /// if all are set they must sum to exactly 1.0.
    #[serde(alias = "distribution")]
    pub ratio: Option<f64>,
}

/// A per-case specialisation of a ref'd parent variant (VAR-SPECIALIZE S5; `constrain_cases`).
/// Addresses a parent case by `name` and tightens its value-source — value/generator/range/
/// `one_of` — by merging with the case's existing source. Non-restrictive (the case survives);
/// other cases are untouched. Structural keys (`type`/`fields`/`ref`) are not permitted.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CaseDelta {
    /// The parent variant case this delta specialises (matched by case `name`).
    pub name: String,
    pub generator: Option<Generator>,
    pub value: Option<serde_yaml::Value>,
    pub range: Option<Range>,
}

/// One case of a heterogeneous tagged union (VAR-1; `FieldType::Union`).
///
/// Unlike [`FieldVariant`], a case is a full [`Field`], so it can carry a nested object
/// schema (`fields`) per case and generates through its **own** generator/value/type
/// (a `value:` case is the static generator). `ratio` is this case's share of the
/// union's rows. Constructed internally by `expand_field_variants`; never from YAML.
#[derive(Debug, Clone)]
pub struct UnionCase {
    pub field: Field,
    pub ratio: Option<f64>,
}

/// Normalised output file descriptor used inside `ExecutionPlan`.
#[derive(Debug, Clone, Deserialize)]
pub struct Output {
    pub file: String,
    pub quality: Option<DataQuality>,
}

/// YAML-level `output:` field — accepts either a plain path string or a full `Output` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum OutputSpec {
    Shorthand(String),
    Block(Output),
}

impl OutputSpec {
    pub fn into_output(self) -> Output {
        match self {
            OutputSpec::Shorthand(s) => Output {
                file: s,
                quality: None,
            },
            OutputSpec::Block(o) => o,
        }
    }
}

/// Data quality degradation applied to an output file after the clean batch is finalised.
///
/// All probability fields are independent Bernoulli rates: each eligible cell fires
/// independently at the stated probability. Dataset-level fields (`duplication`,
/// `missing`) are applied first; per-column transforms follow in the order:
/// nulls → defaults → corruptions.
#[derive(Debug, Clone, Deserialize)]
pub struct DataQuality {
    // dataset-level only
    pub duplication: Option<f64>,
    pub missing: Option<f64>,
    // all levels
    pub nulls: Option<f64>,
    pub default_rate: Option<f64>,
    pub corruptions: Option<Corruptions>,
    // field-level only
    pub default_values: Option<Vec<serde_yaml::Value>>,
    pub defaults_mode: Option<DefaultsMode>,
}

/// Whether field-level `default_values` replaces or augments the built-in default set.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DefaultsMode {
    Override,
    Extend,
}

/// Per-mode corruption probabilities. Each sub-field is an independent per-cell Bernoulli rate.
/// Only modes applicable to the field's type are evaluated.
#[derive(Debug, Clone, Deserialize)]
pub struct Corruptions {
    // string modes
    pub character_deletion: Option<f64>,
    pub character_insertion: Option<f64>,
    pub truncation: Option<f64>,
    pub encoding: Option<f64>,
    // number modes
    pub noise: Option<f64>,
    #[serde(default = "default_noise_scale")]
    pub noise_scale: f64,
    // date / date_time modes
    pub day_shift: Option<f64>,
    #[serde(default = "default_day_shift_max")]
    pub day_shift_max: i64,
}

fn default_noise_scale() -> f64 {
    1.0
}
fn default_day_shift_max() -> i64 {
    30
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
    Words,
    Sentences,
    Paragraphs,
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
    // geo — string only
    Geohash,
    // numeric — number or string
    PositiveDecimal,
    Decimal,
    // numeric — string only
    NumberWithFormat,
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
            Generator::FirstName
            | Generator::LastName
            | Generator::Name
            | Generator::NameWithTitle
            | Generator::Word
            | Generator::Sentence
            | Generator::Paragraph
            | Generator::Words
            | Generator::Sentences
            | Generator::Paragraphs
            | Generator::CompanyName
            | Generator::CompanySuffix
            | Generator::Industry
            | Generator::Profession
            | Generator::Buzzword
            | Generator::Email
            | Generator::Username
            | Generator::Password
            | Generator::IPv4
            | Generator::IPv6
            | Generator::MacAddress
            | Generator::UserAgent
            | Generator::CityName
            | Generator::CountryName
            | Generator::CountryCode
            | Generator::StreetName
            | Generator::ZipCode
            | Generator::StateAbbr
            | Generator::TimeZone
            | Generator::PhoneNumber
            | Generator::CreditCardNumber
            | Generator::Bic
            | Generator::CurrencyCode
            | Generator::CurrencyName
            | Generator::CurrencySymbol
            | Generator::Geohash
            | Generator::NumberWithFormat
            | Generator::Uuid
            | Generator::Isin
            | Generator::LicencePlate
            | Generator::Isbn
            | Generator::Semver => matches!(ft, FieldType::String),
            Generator::Latitude
            | Generator::Longitude
            | Generator::PositiveDecimal
            | Generator::Decimal => matches!(ft, FieldType::Number | FieldType::String),
            Generator::Date => matches!(ft, FieldType::Date | FieldType::String),
            Generator::DateTime => matches!(ft, FieldType::DateTime | FieldType::String),
        }
    }

    /// Returns true when this generator produces meaningfully locale-specific output.
    /// Setting `locale` on a generator that returns false here is a validation error.
    pub fn supports_locale(&self) -> bool {
        matches!(
            self,
            Generator::FirstName
                | Generator::LastName
                | Generator::Name
                | Generator::NameWithTitle
                | Generator::Word
                | Generator::Sentence
                | Generator::Paragraph
                | Generator::Words
                | Generator::Sentences
                | Generator::Paragraphs
                | Generator::CompanyName
                | Generator::CompanySuffix
                | Generator::Industry
                | Generator::Profession
                | Generator::Buzzword
                | Generator::CityName
                | Generator::CountryName
                | Generator::StreetName
                | Generator::ZipCode
                | Generator::StateAbbr
                | Generator::PhoneNumber
                | Generator::LicencePlate
        )
    }

    /// Returns the set of valid `args` keys for this generator, or `None` if it takes no args.
    pub fn valid_args(&self) -> Option<&'static [&'static str]> {
        match self {
            Generator::Sentence
            | Generator::Paragraph
            | Generator::Words
            | Generator::Sentences
            | Generator::Paragraphs
            | Generator::Password => Some(&["min", "max"]),
            Generator::Geohash => Some(&["precision"]),
            Generator::NumberWithFormat => Some(&["format"]),
            _ => None,
        }
    }
}

/// BCP-47-style locale tag controlling which fake-rs locale data is used.
/// Only valid on generators that return true from [`Generator::supports_locale`].
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Locale {
    En,
    FrFr,
    DeDe,
    ItIt,
    NlNl,
    PtBr,
    PtPt,
    CyGb,
    ZhCn,
    ZhTw,
    JaJp,
    ArSa,
    TrTr,
    FaIr,
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Locale::En => "en",
            Locale::FrFr => "fr_fr",
            Locale::DeDe => "de_de",
            Locale::ItIt => "it_it",
            Locale::NlNl => "nl_nl",
            Locale::PtBr => "pt_br",
            Locale::PtPt => "pt_pt",
            Locale::CyGb => "cy_gb",
            Locale::ZhCn => "zh_cn",
            Locale::ZhTw => "zh_tw",
            Locale::JaJp => "ja_jp",
            Locale::ArSa => "ar_sa",
            Locale::TrTr => "tr_tr",
            Locale::FaIr => "fa_ir",
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
            Generator::Words => "words",
            Generator::Sentences => "sentences",
            Generator::Paragraphs => "paragraphs",
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
            Generator::Geohash => "geohash",
            Generator::PositiveDecimal => "positive_decimal",
            Generator::Decimal => "decimal",
            Generator::NumberWithFormat => "number_with_format",
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

/// Within-list normalisation (LIST-NORM): rescale a numeric quantity so each list window sums
/// to `total`. Desugared by `desugar_normalize` into a hidden `<name>__norm_src` source field
/// plus an injected `expression:` calling `array_normalize`/`array_normalize_field`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Normalize {
    /// Per-list sum target (`> 0`). Int or float in YAML; the integer-vs-float output is chosen
    /// by `precision` (or the source field type) — not by whether `total` is written `100` or
    /// `100.0`.
    pub total: f64,
    /// Numeric sub-field to rescale for a `List<Struct>`. Omit for a bare `List<number>`.
    pub field: Option<String>,
    /// Write the rescaled result to this **new** sub-field instead of overwriting `field`.
    /// Keeps the raw value alongside the derived one.
    pub into: Option<String>,
    /// Force the output element type: `0` → integer (exact sum via largest-remainder); `> 0` →
    /// float. Absent inherits the source field's type.
    pub precision: Option<i32>,
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
    /// The collect target (`"linked_ref.field_name"`). Used with `reducer: collect`.
    pub bind: Option<String>,
    pub reducer: Option<Reducer>,
}

/// How values are assembled when the referenced include has a different cardinality.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reducer {
    #[serde(alias = "take_first")]
    TakeOne,
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
    /// Finite-set generator (VAR-SPECIALIZE): draw each row uniformly from this set. On a
    /// plain field it is a standalone "one of these values" generator; on a field that `ref`s a
    /// parent it *restricts* the inherited domain (a support selector — see `FieldConstraints`).
    /// Mutually exclusive with `value`.
    pub one_of: Option<Vec<serde_yaml::Value>>,
    /// Opt-in marginal pinning for a `type: variant` field (VAR-SPECIALIZE S4c). When true, the
    /// declared case `ratio`s are preserved as a **global** marginal across the whole
    /// population: if a child restricts the variant to a subset, the unrestricted rows
    /// compensate so the parent's overall case distribution still matches. Default false =
    /// free-by-default (ratios are within-population draw weights; restrictions reshape the mix).
    #[serde(default)]
    pub preserve_marginal: bool,
    /// Per-case specialisations of a ref'd parent variant (VAR-SPECIALIZE S5): tighten named
    /// cases' value-sources without dropping any. See [`CaseDelta`].
    #[serde(default)]
    pub constrain_cases: Vec<CaseDelta>,
    /// Nested fields for object type — written directly as `fields:` in YAML.
    #[serde(default)]
    pub fields: Vec<Field>,
    /// Element spec for list fields.
    pub content: Option<Box<ListContent>>,
    /// Within-list numeric normalisation (LIST-NORM). Valid on any list-producing field
    /// (`type: list` or a list-valued `expression`). Desugared to a hidden source field plus an
    /// injected `array_normalize`/`array_normalize_field` expression before `resolve_refs`.
    pub normalize: Option<Normalize>,
    /// For `type: variant` fields: the list of alternative field definitions.
    /// Expanded into global dataset variants by `expand_field_variants` before execution.
    /// Must be empty on all other field types.
    #[serde(default)]
    pub variants: Vec<FieldVariant>,
    /// Cases of a heterogeneous tagged union when `field_type == Some(FieldType::Union)`
    /// (VAR-1). Populated internally by `expand_field_variants`; never deserialised from YAML.
    #[serde(skip)]
    pub union_cases: Vec<UnionCase>,
    /// Parquet/Arrow datatype override.  When set, the field's Arrow schema uses
    /// this type and generated values are cast to it.  Valid on any field type.
    pub parquet: Option<ParquetConfig>,
    /// Output-write-time pull-up (serde `#[serde(flatten)]` analogue; VAR-UNIFY). When
    /// true, this field's nesting is elided at output: an `object` field's sub-fields are
    /// pulled up into the parent level; a `variant` (union) field distributes flatten to its
    /// object cases, emitting the active case's fields flat at the parent (nullable superset
    /// for Parquet, per-row keys for JSON). Output-only — the internal model, refs, and
    /// generation are untouched, so a flatten field **must have a name**. Valid only on
    /// `object` and `variant` fields.
    #[serde(default)]
    pub flatten: bool,
    /// Layout for a `flatten`ed **variant** field's case fields in flat columnar output
    /// (Parquet/CSV); see [`FlattenStrategy`]. Only meaningful when `flatten` is true on a
    /// variant field. Defaults to `superset`. Ignored for JSON/JSONL (per-row keys).
    pub flatten_strategy: Option<FlattenStrategy>,
    /// SQL expression evaluated against the batch after all other fields are generated.
    /// Variables must refer to fields defined above this one in the YAML (evaluation order).
    /// Mutually exclusive with `type`, `ref`, `generator`, `min`, `max`, and `value`.
    pub expression: Option<String>,
    /// When true, this field is present in the internal batch but excluded from output.
    /// Set automatically by the expression dependency pull-down pass for fields that are
    /// needed to evaluate an expression but not otherwise declared.
    #[serde(default)]
    pub hidden: bool,
    /// When true, this field originates from an `import:` file rather than YAML generation.
    /// Set by `load_import_headers`; never present in YAML. Children-by-inclusion may not
    /// ref or specialise tainted fields (see §Specialisation restrictions in specs/IMPORT.md).
    #[serde(skip)]
    pub imported_taint: bool,
    /// When true, this is a **constraint-bearing variant** (`ref` + its *own* `variants`) that the
    /// lower-cover planner lowers into case-members. Set by `expand_field_variants` (the only point
    /// where the user-written `variants` are still distinguishable from a parent carrier later
    /// copied onto a plain ref by `resolve_refs`). Never present in YAML.
    #[serde(skip)]
    pub constraint_bearing: bool,
    /// Number of items per row for `list` type fields. Ignored on all other field types.
    pub count: Option<CountSpec>,
    /// Decimal precision for `number` fields. Positive = decimal places; negative = round by
    /// powers of 10 (e.g. -2 rounds to the nearest 100). Applied after generation.
    /// Ignored on non-number fields.
    pub precision: Option<i32>,
    /// Default value used when this field has no inherited value from any child.
    /// Must be type-compatible with `field_type`. List fields use `default: []`
    /// as the empty-collect fallback required by `reducer: collect` bindings.
    pub default: Option<serde_yaml::Value>,
    /// Per-field data-quality overrides. Only valid when the owning dataset's output
    /// block also declares a `quality` stanza.
    pub quality: Option<DataQuality>,
    /// Lower bound for `date` / `date_time` generation.
    /// Format: `YYYY-MM-DD` for `date`, RFC 3339 for `date_time`.
    /// If both `after` and `before` are set, `after` must precede `before`.
    pub after: Option<String>,
    /// Upper bound for `date` / `date_time` generation. See `after`.
    pub before: Option<String>,
    /// Generator-specific arguments. Only valid when `generator` is set (or for `boolean` ratio).
    /// Valid keys per generator:
    /// `sentence`/`paragraph`/`words`/`sentences`/`paragraphs`/`password` → `min`, `max` (integer);
    /// `geohash` → `precision` (integer 1–12);
    /// `number_with_format` → `format` (string);
    /// `boolean` (no generator required) → `ratio` (integer 0–100, percent-true).
    pub args: Option<HashMap<String, serde_yaml::Value>>,
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
                RefEntry::Simple(s) => Some(s.as_str()),
                RefEntry::Rich(b) => b.target.as_deref(),
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
            Some(RefsSpec::Multi(entries)) => entries
                .iter()
                .filter_map(|e| match e {
                    RefEntry::Rich(b) if b.reducer.is_some() => Some(b),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        }
    }
}

/// Fill in free-slot distributions: `None` entries share the remainder after fixed entries equally.
pub fn resolve_distributions(dists: &[Option<f64>]) -> Vec<f64> {
    let fixed_sum: f64 = dists.iter().filter_map(|d| *d).sum();
    let n_free = dists.iter().filter(|d| d.is_none()).count();
    let free_share = if n_free > 0 {
        (1.0 - fixed_sum) / n_free as f64
    } else {
        0.0
    };
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
            if let Some(from_ref) = &content.from
                && let Some(link) = links.iter().find(|l| &l.reference == from_ref)
            {
                visitor(field, link, &content.item.fields);
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
    /// Project a single field out of the per-item content, producing a scalar list instead of a
    /// list of structs. Two syntactic forms (PROJECT-FIELD):
    /// - **dotted** `"<link_ref>.<field>"` — project a field straight from the linked dataset;
    ///   mutually exclusive with explicit `fields`.
    /// - **bare** `"<identifier>"` — project a field defined in `content.fields`; *requires*
    ///   `fields`.
    pub project: Option<String>,
    #[serde(flatten)]
    pub item: Field,
}

impl ListContent {
    /// True when `project` is the **bare** form (no `.`) — projects a field from `content.fields`
    /// rather than straight from the linked dataset.
    pub(crate) fn is_bare_project(&self) -> bool {
        self.project.as_deref().is_some_and(|p| !p.contains('.'))
    }

    /// The column name to project out of the assembled per-item batch, if `project` is set:
    /// the `<field>` part for the dotted form, the whole identifier for the bare form.
    pub(crate) fn project_col(&self) -> Option<String> {
        self.project.as_ref().map(|p| match split_ref(p) {
            Some((_, f)) => f.to_string(),
            None => p.clone(),
        })
    }
}

/// One concrete variant of a dataset: a data schema fragment (merged on top of the
/// dataset's base `data`), an optional locale override, and an optional row-fraction.
pub type Schema = Vec<Field>;

/// Runtime seed configuration, bundled so additional seed types can be added later
/// without breaking call sites.
#[derive(Debug, Clone)]
pub struct SeedConfig {
    /// Seed for the import hash ring. Controls which rows of each imported file
    /// are assigned to each ring partition. Set to a fixed value for reproducibility;
    /// by default a random value is chosen at startup (see `--seed.ring` in Phase 7).
    pub ring: u64,
}

/// Hash-ring bounds for partitioning an imported file.
///
/// Row `i` of the imported file is included iff `h(i) ∈ [start, end)` where
/// `h` is a deterministic positional hash seeded by `--seed.ring`. Bounds must
/// satisfy `0.0 ≤ start < end ≤ 1.0`.
#[derive(Debug, Clone, Deserialize)]
pub struct RingBounds {
    pub start: f64,
    pub end: f64,
}

/// Declares that this dataset's rows come from a pre-existing external file rather
/// than being generated from scratch. Mutually exclusive with `rows:`.
///
/// Imported columns are merged into the dataset's schema (as tainted fields) by
/// `load_import_headers` before validation; synthetic `data.fields` are appended
/// per-row at execution time.
#[derive(Debug, Clone, Deserialize)]
pub struct ImportSpec {
    /// Path to the imported file, relative to the schema root.
    /// Supported formats: Parquet, CSV, JSON array, JSONL.
    pub file: String,
    /// Reference namespace used to qualify imported columns in `expression:` fields
    /// within this dataset (e.g. `ref: tickers` → `tickers.symbol` in expressions).
    #[serde(rename = "ref")]
    pub reference: String,
    /// Column names to project in. `["*"]` or absent means all columns.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Column names to suppress after projection. Most useful with `["*"]`.
    pub exclude: Option<Vec<String>>,
    /// Hash ring bounds restricting which rows of the file are used.
    /// Assigned automatically by the planner when a lower cover exists;
    /// may be set manually to restrict the dataset to a fraction of the file.
    pub ring: Option<RingBounds>,
    /// Total row count of the imported file, set by `load_import_headers`.
    /// Not present in YAML.
    #[serde(skip)]
    pub total_rows: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Include {
    pub file: String,
    #[serde(rename = "ref")]
    pub reference: String,
    /// Marginal row-membership probability (0.0–1.0). When set on two or more lower cover
    /// members of a common parent, the executor uses Bernoulli segmentation + IPF to
    /// correctly model the overlap.
    #[serde(alias = "distribution")]
    pub ratio: Option<f64>,
    /// How many times each child row is replicated into the parent batch (top-level include),
    /// or how many items to draw per outer row (list-link entry).
    pub cardinality: Option<CountSpec>,
    /// Sampling intensity for list-link and junction-link draws. `links:` only.
    /// `0` = without-replacement (Fisher-Yates); `1` or absent = uniform with-replacement;
    /// `> 1` = Pólya-urn clumping (previously-drawn rows are preferred on subsequent draws).
    /// Values in the range `(0, 1)` are invalid.
    /// Note: `reinforcement` applies Pólya sampling *within the partition defined by `overlap`*.
    pub reinforcement: Option<f64>,
    /// Cross-list sampling scope for list-link draws. `links:` only.
    /// `0` = non-overlapping: each staging row draws from an exclusive shard of the eligible
    ///       linked rows (the shard is determined by the staging row's index); `1` or absent =
    ///       unrestricted (all staging rows draw from the full set of eligible linked rows); `> 1` = power-law
    ///       preferential weighting (lower-indexed linked rows are progressively more likely to
    ///       be drawn across all staging rows, producing cross-list clumping).
    /// Values in the open interval `(0, 1)` are invalid.
    pub overlap: Option<f64>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyntheticDataset {
    pub name: String,
    pub format: Format,
    /// Default locale applied to every locale-capable generator in this dataset
    /// that does not set its own explicit `locale`. Field-level `locale` takes precedence.
    pub locale: Option<Locale>,
    /// Explicit row count. Must not be set when any include specifies a
    /// `ratio` — in that case rows are derived from the parent size.
    pub rows: Option<usize>,
    /// Single output file for this dataset. Accepts either a plain path string
    /// (shorthand) or a full `Output` block (with optional `quality` stanza).
    /// Aliased as `output_file` for backward compatibility.
    /// Sugar for a single-entry `outputs` list; if both are set, `outputs` wins.
    #[serde(alias = "output_file")]
    pub output: Option<OutputSpec>,
    /// Multiple output files (e.g. a clean and a degraded copy). `output` is
    /// syntactic sugar for `outputs` with a single entry.
    #[serde(default)]
    pub outputs: Vec<OutputSpec>,
    pub include: Option<Include>,
    /// Pre-existing external file whose rows become this dataset's rows.
    /// Mutually exclusive with `rows:`. Imported columns are merged into
    /// `data` as tainted fields by `load_import_headers` before planning.
    pub import: Option<ImportSpec>,
    /// Linked datasets for junction or list-link sampling.
    /// Each entry names a dataset (by file + ref) from which atoms draw linked-dataset values.
    /// A link referenced by a `content.from:` field is a **list link** (witness/assembly pipeline).
    /// A link with no `content.from:` reference is a **junction link** (activated in MULT-2).
    #[serde(default)]
    pub links: Vec<Include>,
    #[serde(default)]
    pub data: Schema,
}

impl SyntheticDataset {
    /// Returns a flat, normalised list of output descriptors.
    ///
    /// - If `outputs` is non-empty it is returned as-is (wins over `output`).
    /// - If only `output` is set, returns a single-element vec.
    /// - If neither is set, returns an empty vec.
    pub fn resolved_outputs(&self) -> Vec<Output> {
        if !self.outputs.is_empty() {
            return self
                .outputs
                .iter()
                .cloned()
                .map(OutputSpec::into_output)
                .collect();
        }
        match &self.output {
            Some(spec) => vec![spec.clone().into_output()],
            None => vec![],
        }
    }
}
