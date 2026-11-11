//! Pure observations and validation leaves, below decoding and I/O.

use crate::data::{FOValue, Finite};
use crate::path::PathRules;
use crate::{label, record, variant};
use std::fmt;
use strum::{IntoStaticStr, VariantArray};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    pub high: u32,
    pub low: u32,
}

record!(Count {
    high: "high",
    low: "low"
});

impl Count {
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        let bytes = value.to_be_bytes();
        Self {
            high: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            low: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        }
    }

    #[must_use]
    pub const fn to_u64(self) -> u64 {
        let high = self.high.to_be_bytes();
        let low = self.low.to_be_bytes();
        u64::from_be_bytes([
            high[0], high[1], high[2], high[3], low[0], low[1], low[2], low[3],
        ])
    }
}

impl From<u64> for Count {
    fn from(value: u64) -> Self {
        Self::from_u64(value)
    }
}

impl From<usize> for Count {
    fn from(value: usize) -> Self {
        Self::from_u64(u64::try_from(value).expect("supported hosts have at most 64-bit indices"))
    }
}

impl fmt::Display for Count {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_u64().fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountRule {
    Exactly(Count),
    AtLeast(Count),
}
variant!(CountRule { Exactly(Count): "exactly", AtLeast(Count): "at-least" });

impl fmt::Display for CountRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exactly(n) => write!(f, "exactly {n}"),
            Self::AtLeast(n) => write!(f, "at least {n}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataStep {
    Field(String),
    Index(Count),
    Payload(String),
}
variant!(DataStep { Field(String): "field", Index(Count): "index", Payload(String): "payload" });

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Number {
    Int(i64),
    Float(Finite),
}
variant!(Number { Int(i64): "int", Float(Finite): "float" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum IntOrFloat {
    Int,
    Float,
}
label!(IntOrFloat);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum NumberFault {
    Syntax,
    OutOfRange,
    NonFinite,
}
label!(NumberFault);

impl NumberFault {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Syntax => "invalid numeric syntax",
            Self::OutOfRange => "number is out of range",
            Self::NonFinite => "number is not finite",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum IntegerParseCause {
    Empty,
    InvalidDigit,
    PositiveOverflow,
    NegativeOverflow,
    Zero,
    Unclassified,
}
label!(IntegerParseCause);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ValueShape {
    Unit,
    Bool,
    Int,
    Float,
    String,
    Bytes,
    List,
    Map,
    Variant,
    Block,
    Function,
    Handle,
}
label!(ValueShape);

impl ValueShape {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Unit => "()",
            Self::Bool => "a boolean",
            Self::Int => "an integer",
            Self::Float => "a float",
            Self::String => "a string",
            Self::Bytes => "bytes",
            Self::List => "a list",
            Self::Map => "a record",
            Self::Variant => "a tag",
            Self::Block => "a block",
            Self::Function => "a function",
            Self::Handle => "a handle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPreview {
    pub preview: String,
    pub truncated: bool,
}
record!(TextPreview {
    preview: "preview",
    truncated: "truncated"
});

impl TextPreview {
    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut chars = text.chars();
        let preview = chars.by_ref().take(24).collect();
        Self {
            preview,
            truncated: chars.next().is_some(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Unit,
    Bool(bool),
    Int(i64),
    Float(Finite),
    Text(TextPreview),
    Bytes,
    List,
    Map,
    Variant(String),
    Block,
    Function,
    Handle,
}
variant!(Found {
    Unit: "unit", Bool(bool): "bool", Int(i64): "int", Float(Finite): "float",
    Text(TextPreview): "text", Bytes: "bytes", List: "list", Map: "map",
    Variant(String): "variant", Block: "block", Function: "function", Handle: "handle",
});

impl Found {
    #[must_use]
    pub fn of(value: &FOValue) -> Self {
        match value {
            FOValue::Unit => Self::Unit,
            FOValue::Bool { value } => Self::Bool(*value),
            FOValue::Int { value } => Self::Int(*value),
            FOValue::Float { value } => Self::Float(*value),
            FOValue::String { value } => Self::Text(TextPreview::new(value)),
            FOValue::Bytes { .. } => Self::Bytes,
            FOValue::List { .. } => Self::List,
            FOValue::Map { .. } => Self::Map,
            FOValue::Variant { label, .. } => Self::Variant(label.clone()),
            #[expect(
                unreachable_code,
                reason = "NoExt is uninhabited; the empty match eliminates the cloned value"
            )]
            FOValue::Ext(never) => match never.clone() {},
        }
    }

    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Unit => "()".into(),
            Self::Bool(b) => format!("the boolean {b}"),
            Self::Int(n) => format!("the integer {n}"),
            Self::Float(n) => format!("the float {n}"),
            Self::Text(p) => format!(
                "the string {:?}{}",
                p.preview,
                if p.truncated { "…" } else { "" }
            ),
            Self::Bytes => "bytes".into(),
            Self::List => "a list".into(),
            Self::Map => "a record".into(),
            Self::Variant(tag) => format!("the tag {tag:?}"),
            Self::Block => "a block".into(),
            Self::Function => "a function".into(),
            Self::Handle => "a handle".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum DataHead {
    Unit,
    Bool,
    Int,
    Float,
    String,
    Bytes,
    List,
    Map,
    Record,
    Variant,
    Thunk,
    Handle,
}
label!(DataHead);

impl DataHead {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Unit => "()",
            Self::Bool => "a boolean",
            Self::Int => "an integer",
            Self::Float => "a float",
            Self::String => "a string",
            Self::Bytes => "bytes",
            Self::List => "a list",
            Self::Map => "a map",
            Self::Record => "a record",
            Self::Variant => "a tag",
            Self::Thunk => "a block or a function",
            Self::Handle => "a handle",
        }
    }
}

fn alternatives(items: &[&str]) -> String {
    match items {
        [] => "no value".into(),
        [only] => (*only).into(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataKind {
    pub heads: Vec<DataHead>,
    pub deep: bool,
}
record!(DataKind {
    heads: "heads",
    deep: "deep"
});

impl DataKind {
    #[must_use]
    pub fn describe(&self) -> String {
        let heads = alternatives(
            &self
                .heads
                .iter()
                .map(|head| head.describe())
                .collect::<Vec<_>>(),
        );
        if self.deep {
            format!("{heads}, with no blocks, functions or handles at any depth")
        } else {
            heads
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    Shape(ValueShape),
    AnyOf(Vec<ValueShape>),
    Record(Vec<String>),
    Tags(Vec<String>),
    StringLiterals(Vec<String>),
    Kind(DataKind),
    Type(String),
    Closure,
}
variant!(Expected {
    Shape(ValueShape): "shape", AnyOf(Vec<ValueShape>): "any-of", Record(Vec<String>): "record",
    Tags(Vec<String>): "tags", StringLiterals(Vec<String>): "string-literals",
    Kind(DataKind): "kind", Type(String): "type", Closure: "closure",
});

impl Expected {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Shape(shape) => shape.describe().into(),
            Self::AnyOf(shapes) => alternatives(
                &shapes
                    .iter()
                    .map(|shape| shape.describe())
                    .collect::<Vec<_>>(),
            ),
            Self::Record(fields) if fields.is_empty() => "a record with no fields".into(),
            Self::Record(fields) => format!(
                "a record with fields {}",
                fields
                    .iter()
                    .map(|field| format!("{field:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Tags(tags) => format!(
                "one of the tags {}",
                tags.iter()
                    .map(|tag| format!("{tag:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::StringLiterals(texts) => format!(
                "one of {}",
                texts
                    .iter()
                    .map(|s| format!("{s:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Kind(kind) => kind.describe(),
            Self::Type(ty) => ty.clone(),
            Self::Closure => "a closure (not a native function)".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeFailure {
    pub expected: Expected,
    pub found: Found,
}
record!(ShapeFailure {
    expected: "expected",
    found: "found"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownField {
    pub field: String,
    pub offered: Vec<String>,
}
record!(UnknownField {
    field: "field",
    offered: "offered"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTag {
    pub tag: String,
    pub offered: Vec<String>,
}
record!(UnknownTag {
    tag: "tag",
    offered: "offered"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnexpectedPayload {
    pub tag: String,
    pub found: Found,
}
record!(UnexpectedPayload {
    tag: "tag",
    found: "found"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum OpaqueKind {
    Block,
    Function,
    Handle,
}
label!(OpaqueKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum NumberOrText {
    Number,
    Text,
}
label!(NumberOrText);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ReservedVar {
    Pwd,
    Oldpwd,
}
label!(ReservedVar);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum EntryNameFailure {
    Empty,
    Dots,
    Separator,
    Stream,
}
label!(EntryNameFailure);

impl EntryNameFailure {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Empty => "an entry name cannot be empty",
            Self::Dots => "an entry name cannot be . or ..",
            Self::Separator => "an entry name cannot contain a path separator",
            Self::Stream => "an entry name cannot name a Windows stream",
        }
    }
}

variant!(PathRules {
    Posix: "posix",
    Windows: "windows"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentNameFailure {
    pub name: String,
}
record!(AgentNameFailure { name: "name" });

impl AgentNameFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "invalid agent name {:?}: use 1–24 ASCII letters, digits, '-' or '_'",
            self.name
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvertedTurnSpan {
    pub from: Count,
    pub to: Count,
}
record!(InvertedTurnSpan {
    from: "from",
    to: "to"
});

impl InvertedTurnSpan {
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "turn span starts at {} after its end {}",
            self.from, self.to
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFailure {
    pub expression: String,
    pub cause: CronCause,
}
record!(CronFailure {
    expression: "expression",
    cause: "cause"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum CronField {
    Minute,
    Hour,
    DayOfMonth,
    Month,
    DayOfWeek,
}
label!(CronField);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronCauseFieldCount {
    pub got: Count,
}
record!(CronCauseFieldCount { got: "got" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronCauseField {
    pub field: CronField,
    pub specification: String,
    pub cause: CronFieldCause,
}
record!(CronCauseField {
    field: "field",
    specification: "specification",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronCause {
    FieldCount(CronCauseFieldCount),
    Field(CronCauseField),
}
variant!(CronCause { FieldCount(CronCauseFieldCount): "field-count", Field(CronCauseField): "field" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFieldCauseInvalidStep {
    pub text: String,
    pub fault: NumberFault,
}
record!(CronFieldCauseInvalidStep {
    text: "text",
    fault: "fault"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFieldCauseUnknownValue {
    pub token: String,
    pub offered: Vec<String>,
}
record!(CronFieldCauseUnknownValue {
    token: "token",
    offered: "offered"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFieldCauseOutOfRange {
    pub token: String,
    pub minimum: u8,
    pub maximum: u8,
}
record!(CronFieldCauseOutOfRange {
    token: "token",
    minimum: "minimum",
    maximum: "maximum"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFieldCauseDescendingRange {
    pub range: String,
    pub from: u8,
    pub to: u8,
}
record!(CronFieldCauseDescendingRange {
    range: "range",
    from: "from",
    to: "to"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronFieldCause {
    Empty,
    InvalidStep(CronFieldCauseInvalidStep),
    ZeroStep,
    UnknownValue(CronFieldCauseUnknownValue),
    OutOfRange(CronFieldCauseOutOfRange),
    DescendingRange(CronFieldCauseDescendingRange),
}
variant!(CronFieldCause {
    Empty: "empty", InvalidStep(CronFieldCauseInvalidStep): "invalid-step", ZeroStep: "zero-step",
    UnknownValue(CronFieldCauseUnknownValue): "unknown-value", OutOfRange(CronFieldCauseOutOfRange): "out-of-range",
    DescendingRange(CronFieldCauseDescendingRange): "descending-range",
});

impl CronFieldCause {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Empty => "empty field".into(),
            Self::InvalidStep(s) => format!(
                "invalid step '{}': {}; use an integer from 1 through 255",
                s.text.escape_debug(),
                s.fault.describe()
            ),
            Self::ZeroStep => "step must be greater than zero".into(),
            Self::UnknownValue(v) if v.offered.is_empty() => format!(
                "unrecognised cron value '{}'; use a supported numeric value for this field",
                v.token.escape_debug()
            ),
            Self::UnknownValue(v) => format!(
                "unrecognised cron value '{}'; use a numeric value or one of: {}",
                v.token.escape_debug(),
                v.offered
                    .iter()
                    .map(|name| name.escape_debug().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::OutOfRange(v) => format!(
                "value '{}' is outside {}..={}",
                v.token.escape_debug(),
                v.minimum,
                v.maximum
            ),
            Self::DescendingRange(r) => format!(
                "range '{}' is descending: {} is greater than {}; put the lower endpoint first",
                r.range.escape_debug(),
                r.from,
                r.to
            ),
        }
    }
}

impl CronFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        let why = match &self.cause {
            CronCause::FieldCount(n) => format!(
                "expected five fields (minute hour day-of-month month day-of-week), got {}",
                n.got
            ),
            CronCause::Field(f) => format!(
                "{} field '{}': {}",
                <&str>::from(f.field),
                f.specification.escape_debug(),
                f.cause.describe()
            ),
        };
        format!(
            "invalid cron expression '{}': {why}",
            self.expression.escape_debug()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationFailure {
    pub text: String,
    pub cause: DurationCause,
}
record!(DurationFailure {
    text: "text",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationCauseInvalidCount {
    pub text: String,
    pub fault: NumberFault,
}
record!(DurationCauseInvalidCount {
    text: "text",
    fault: "fault"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationCauseUnknownUnit {
    pub unit: String,
}
record!(DurationCauseUnknownUnit { unit: "unit" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurationCause {
    MissingUnit,
    InvalidCount(DurationCauseInvalidCount),
    UnknownUnit(DurationCauseUnknownUnit),
    Zero,
    Overflow,
}
variant!(DurationCause {
    MissingUnit: "missing-unit", InvalidCount(DurationCauseInvalidCount): "invalid-count",
    UnknownUnit(DurationCauseUnknownUnit): "unknown-unit", Zero: "zero", Overflow: "overflow",
});

impl DurationFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        let why = match &self.cause {
            DurationCause::MissingUnit => "a duration needs a unit (s/m/h/d), e.g. 30m".into(),
            DurationCause::InvalidCount(n) => {
                let remedy = match n.fault {
                    NumberFault::Syntax => "use an unsigned whole-number count, e.g. 30m",
                    NumberFault::OutOfRange => "use a smaller unsigned whole-number count",
                    NumberFault::NonFinite => "use an unsigned whole-number count",
                };
                format!(
                    "invalid count '{}': {}; {remedy}",
                    n.text.escape_debug(),
                    n.fault.describe()
                )
            }
            DurationCause::UnknownUnit(u) => format!(
                "unknown duration unit '{}'; use s/m/h/d",
                u.unit.escape_debug()
            ),
            DurationCause::Zero => "duration must be greater than zero".into(),
            DurationCause::Overflow => {
                "duration is too large; use a smaller count or a shorter unit".into()
            }
        };
        format!("invalid duration '{}': {why}", self.text.escape_debug())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utf8Failure {
    pub valid_up_to: Count,
    pub error_len: Option<Count>,
}
record!(Utf8Failure {
    valid_up_to: "valid-up-to",
    error_len: "error-len"
});

impl Utf8Failure {
    #[must_use]
    pub fn describe(&self) -> String {
        match self.error_len {
            Some(n) => format!(
                "invalid UTF-8 at byte {} ({})",
                self.valid_up_to,
                crate::text::plural(n.to_u64(), "invalid byte")
            ),
            None => format!("incomplete UTF-8 at byte {}", self.valid_up_to),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegexFailure {
    pub pattern: String,
    pub cause: RegexCause,
}
record!(RegexFailure {
    pattern: "pattern",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegexCause {
    Syntax(String),
    CompiledTooBig(Count),
    GrepCompiler(String),
    Unclassified(String),
}
variant!(RegexCause {
    Syntax(String): "syntax", CompiledTooBig(Count): "compiled-too-big",
    GrepCompiler(String): "grep-compiler", Unclassified(String): "unclassified",
});

impl RegexCause {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Syntax(detail) => format!("regex syntax: {detail}"),
            Self::CompiledTooBig(limit) => format!("compiled regex exceeds {limit} bytes"),
            Self::GrepCompiler(detail) => format!("grep regex compiler: {detail}"),
            Self::Unclassified(detail) => format!("regex compiler: {detail}"),
        }
    }
}

impl RegexFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "invalid regex {:?}: {}",
            self.pattern,
            self.cause.describe()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum JsonCategory {
    Syntax,
    Eof,
}
label!(JsonCategory);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonSyntaxFailure {
    pub class: JsonCategory,
    pub line: Count,
    pub column: Count,
    pub detail: String,
}
record!(JsonSyntaxFailure {
    class: "class",
    line: "line",
    column: "column",
    detail: "detail"
});

impl JsonSyntaxFailure {
    pub(crate) fn parser_detail(error: &serde_json::Error) -> String {
        let detail = error.to_string();
        let suffix = format!(" at line {} column {}", error.line(), error.column());
        detail.strip_suffix(&suffix).unwrap_or(&detail).into()
    }

    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "JSON {} at line {}, column {}: {}",
            match self.class {
                JsonCategory::Syntax => "syntax error",
                JsonCategory::Eof => "ended unexpectedly",
            },
            self.line,
            self.column,
            self.detail
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationFact {
    pub seconds: Count,
    pub nanos: u32,
}
record!(DurationFact {
    seconds: "seconds",
    nanos: "nanos"
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;

    #[test]
    fn count_retains_every_u64_bit() {
        for n in [0, i64::MAX.unsigned_abs(), 1_u64 << 63, u64::MAX] {
            let count = Count::from_u64(n);
            assert_eq!(count.to_u64(), n);
            assert_eq!(Count::decode(&count.encode()).unwrap().to_u64(), n);
        }
        assert_eq!(
            Count::from_u64(1_u64 << 63),
            Count {
                high: 0x8000_0000,
                low: 0
            }
        );
    }

    #[test]
    fn shared_tags_keep_runtime_shapes_distinct_from_checker_heads() {
        assert_eq!(
            DataHead::Thunk.encode(),
            crate::data::datum::tag("thunk", None)
        );
        assert_eq!(
            ValueShape::String.encode(),
            crate::data::datum::tag("string", None)
        );
        let text = Found::Text(TextPreview::new("text"));
        assert_eq!(crate::data::datum::untag(&text.encode()).unwrap().0, "text");
        let expectations = [
            Expected::Shape(ValueShape::String),
            Expected::AnyOf(vec![ValueShape::Int, ValueShape::Float]),
            Expected::Record(vec!["field".into()]),
            Expected::Tags(vec!["tag".into()]),
            Expected::StringLiterals(vec!["minimal".into(), "readline".into()]),
            Expected::Kind(DataKind {
                heads: vec![DataHead::Map, DataHead::Record, DataHead::Thunk],
                deep: true,
            }),
            Expected::Type("a → b".into()),
            Expected::Closure,
        ];
        for expected in expectations {
            assert_eq!(Expected::decode(&expected.clone().encode()), Ok(expected));
        }
    }

    #[test]
    fn text_preview_counts_unicode_scalars_without_an_ellipsis() {
        for n in [0, 23, 24, 25] {
            let preview = TextPreview::new(&"🦀".repeat(n));
            assert_eq!(preview.preview, "🦀".repeat(n.min(24)));
            assert_eq!(preview.truncated, n > 24);
        }
    }
}

#[cfg(test)]
mod parser_presentation_tests {
    use super::*;
    use crate::data::datum::Datum;
    use crate::error::Error;
    use crate::fact::{
        DecodeCause, DecodeFailure, ExarchFailure, HostFailure, ProtocolData, ProtocolFailure,
        ProtocolFailureDecode, Reason, ScheduleFailure,
    };
    use std::fmt::Debug;

    fn round_trip<T>(value: &T) -> T
    where
        T: Datum + Clone + Debug + PartialEq,
    {
        let decoded = T::decode(&value.clone().encode()).unwrap();
        assert_eq!(&decoded, value);
        decoded
    }

    fn channels(schedule: ScheduleFailure, cause: DecodeCause, description: &str) {
        let decode = DecodeFailure::new(cause);
        assert_eq!(decode.describe(), description);
        assert_eq!(round_trip(&decode).describe(), description);
        for reason in [
            Reason::Host(Box::new(HostFailure::Exarch(ExarchFailure::Schedule(
                schedule,
            )))),
            Reason::Protocol(Box::new(ProtocolFailure::Decode(ProtocolFailureDecode {
                kind: ProtocolData::Request,
                operation: "exarch-schedules".into(),
                cause: decode,
            }))),
        ] {
            let decoded = round_trip(&reason);
            for reason in [reason, decoded] {
                assert_eq!(reason.advice(), Vec::<String>::new());
                let error = Error::new(reason);
                assert_eq!(error.hints(), []);
                assert!(error.context().is_none());
                assert_eq!(error.plain().matches(description).count(), 1);
            }
        }
    }

    fn cron(failure: CronFailure, description: &str) {
        assert_eq!(failure.describe(), description);
        assert_eq!(round_trip(&failure).describe(), description);
        channels(
            ScheduleFailure::Cron(failure.clone()),
            DecodeCause::Cron(failure),
            description,
        );
    }

    fn cron_field(specification: &str, cause: &CronFieldCause, description: &str) {
        assert_eq!(cause.describe(), description);
        assert_eq!(round_trip(cause).describe(), description);
        for (field, shown) in [
            (CronField::Minute, "minute"),
            (CronField::Hour, "hour"),
            (CronField::DayOfMonth, "day-of-month"),
            (CronField::Month, "month"),
            (CronField::DayOfWeek, "day-of-week"),
        ] {
            let field_cause = CronCauseField {
                field,
                specification: specification.into(),
                cause: cause.clone(),
            };
            let _ = round_trip(&field_cause);
            cron(
                CronFailure {
                    expression: "  original ' cron\n".into(),
                    cause: CronCause::Field(field_cause),
                },
                &format!(
                    "invalid cron expression '  original \\' cron\\n': {shown} field '{}': {description}",
                    specification.escape_debug()
                ),
            );
        }
    }

    fn duration(cause: DurationCause, description: &str) {
        let _ = round_trip(&cause);
        let failure = DurationFailure {
            text: "  original ' duration\n  ".into(),
            cause,
        };
        let description = format!("invalid duration '  original \\' duration\\n  ': {description}");
        assert_eq!(failure.describe(), description);
        assert_eq!(round_trip(&failure).describe(), description);
        channels(
            ScheduleFailure::Duration(failure.clone()),
            DecodeCause::Duration(failure),
            &description,
        );
    }

    #[test]
    fn cron_field_count_explains_the_five_fields_in_grammar_order() {
        for (got, shown) in [
            (0, "0"),
            (4, "4"),
            (6, "6"),
            (u64::MAX, "18446744073709551615"),
        ] {
            let count = CronCauseFieldCount {
                got: Count::from_u64(got),
            };
            let _ = round_trip(&count);
            cron(
                CronFailure {
                    expression: "*".into(),
                    cause: CronCause::FieldCount(count),
                },
                &format!(
                    "invalid cron expression '*': expected five fields (minute hour day-of-month month day-of-week), got {shown}"
                ),
            );
        }
    }

    #[test]
    fn cron_empty_zero_and_invalid_steps_retain_faults_and_exact_remedies() {
        cron_field("", &CronFieldCause::Empty, "empty field");
        cron_field(
            "*/0",
            &CronFieldCause::ZeroStep,
            "step must be greater than zero",
        );
        for (fault, shown) in [
            (NumberFault::Syntax, "invalid numeric syntax"),
            (NumberFault::OutOfRange, "number is out of range"),
            (NumberFault::NonFinite, "number is not finite"),
        ] {
            let step = CronFieldCauseInvalidStep {
                text: "x'\n".into(),
                fault,
            };
            let _ = round_trip(&step);
            cron_field(
                "*/x'\n",
                &CronFieldCause::InvalidStep(step),
                &format!("invalid step 'x\\'\\n': {shown}; use an integer from 1 through 255"),
            );
        }
    }

    #[test]
    fn cron_unknown_values_keep_empty_or_ordered_offerings_and_overflow_token() {
        for (token, offered, description) in [
            (
                "256",
                vec![],
                "unrecognised cron value '256'; use a supported numeric value for this field",
            ),
            (
                "unknown",
                vec!["mar", "jan"],
                "unrecognised cron value 'unknown'; use a numeric value or one of: mar, jan",
            ),
            (
                "o'\n",
                vec!["z\n", "a'"],
                "unrecognised cron value 'o\\'\\n'; use a numeric value or one of: z\\n, a\\'",
            ),
        ] {
            let value = CronFieldCauseUnknownValue {
                token: token.into(),
                offered: offered.into_iter().map(str::to_owned).collect(),
            };
            let _ = round_trip(&value);
            cron_field(token, &CronFieldCause::UnknownValue(value), description);
        }
    }

    #[test]
    fn cron_range_causes_keep_recorded_bounds_and_lower_endpoint_remedy() {
        let range = CronFieldCauseOutOfRange {
            token: "60".into(),
            minimum: 0,
            maximum: 59,
        };
        let _ = round_trip(&range);
        cron_field(
            "60",
            &CronFieldCause::OutOfRange(range),
            "value '60' is outside 0..=59",
        );
        let descending = CronFieldCauseDescendingRange {
            range: "7-0".into(),
            from: 7,
            to: 0,
        };
        let _ = round_trip(&descending);
        cron_field(
            "7-0",
            &CronFieldCause::DescendingRange(descending),
            "range '7-0' is descending: 7 is greater than 0; put the lower endpoint first",
        );
        let escaped = CronFieldCauseDescendingRange {
            range: "o'\n".into(),
            from: 255,
            to: 0,
        };
        let _ = round_trip(&escaped);
        cron_field(
            "o'\n",
            &CronFieldCause::DescendingRange(escaped),
            "range 'o\\'\\n' is descending: 255 is greater than 0; put the lower endpoint first",
        );
    }

    #[test]
    fn duration_missing_unknown_zero_and_overflow_keep_padded_original_spelling() {
        duration(
            DurationCause::MissingUnit,
            "a duration needs a unit (s/m/h/d), e.g. 30m",
        );
        for (unit, description) in [
            ("w", "unknown duration unit 'w'; use s/m/h/d"),
            ("o'\n", "unknown duration unit 'o\\'\\n'; use s/m/h/d"),
        ] {
            let unknown = DurationCauseUnknownUnit { unit: unit.into() };
            let _ = round_trip(&unknown);
            duration(DurationCause::UnknownUnit(unknown), description);
        }
        duration(DurationCause::Zero, "duration must be greater than zero");
        duration(
            DurationCause::Overflow,
            "duration is too large; use a smaller count or a shorter unit",
        );
    }

    #[test]
    fn duration_invalid_count_faults_have_distinct_unsigned_count_remedies() {
        for (fault, description) in [
            (
                NumberFault::Syntax,
                "invalid count 'o\\'\\n': invalid numeric syntax; use an unsigned whole-number count, e.g. 30m",
            ),
            (
                NumberFault::OutOfRange,
                "invalid count 'o\\'\\n': number is out of range; use a smaller unsigned whole-number count",
            ),
            (
                NumberFault::NonFinite,
                "invalid count 'o\\'\\n': number is not finite; use an unsigned whole-number count",
            ),
        ] {
            let count = DurationCauseInvalidCount {
                text: "o'\n".into(),
                fault,
            };
            let _ = round_trip(&count);
            duration(DurationCause::InvalidCount(count), description);
        }
    }
}
