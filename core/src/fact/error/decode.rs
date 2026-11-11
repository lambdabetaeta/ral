//! Structured decoding and JSON encoding failures; no I/O or enclosing reasons.

use super::shared::{
    AgentNameFailure, Count, CountRule, CronFailure, DataStep, DurationFailure, EntryNameFailure,
    Found, InvertedTurnSpan, JsonSyntaxFailure, RegexFailure, ReservedVar, ShapeFailure,
    UnexpectedPayload, UnknownField, UnknownTag,
};
use crate::fact::{Decision, Resource};
use crate::path::PathRules;
use crate::{label, record, variant};
use std::fmt;
use strum::{IntoStaticStr, VariantArray};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeFailure {
    pub path: Vec<DataStep>,
    pub cause: Box<DecodeCause>,
}
record!(DecodeFailure {
    path: "path",
    cause: "cause"
});

impl DecodeFailure {
    #[must_use]
    pub fn new(cause: DecodeCause) -> Self {
        Self {
            path: Vec::new(),
            cause: Box::new(cause),
        }
    }

    #[must_use]
    pub fn at(mut self, step: DataStep) -> Self {
        self.path.insert(0, step);
        self
    }

    #[must_use]
    pub fn field(self, key: impl Into<String>) -> Self {
        self.at(DataStep::Field(key.into()))
    }

    #[must_use]
    pub fn index(self, index: Count) -> Self {
        self.at(DataStep::Index(index))
    }

    #[must_use]
    pub fn payload(self, tag: impl Into<String>) -> Self {
        self.at(DataStep::Payload(tag.into()))
    }

    #[must_use]
    pub fn describe(&self) -> String {
        format!("{self}")
    }
}

pub(super) fn write_path(path: &[DataStep], f: &mut impl fmt::Write) -> fmt::Result {
    for step in path {
        match step {
            DataStep::Field(key) => write!(f, "field {key:?}: ")?,
            DataStep::Index(index) => write!(f, "element {index}: ")?,
            DataStep::Payload(tag) => write!(f, "tag {tag:?}: ")?,
        }
    }
    Ok(())
}

impl fmt::Display for DecodeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_path(&self.path, f)?;
        f.write_str(&self.cause.describe())
    }
}

impl std::error::Error for DecodeFailure {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum DecodeIntegerTarget {
    Byte,
    FailureStatus,
    I32,
    U32,
    U64,
    Usize,
}
label!(DecodeIntegerTarget);

impl DecodeIntegerTarget {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Byte => "a byte (0..=255)",
            Self::FailureStatus => "a failure status (1..=255)",
            Self::I32 => "an i32 (-2147483648..=2147483647)",
            Self::U32 => "a u32 (0..=4294967295)",
            Self::U64 => "a non-negative integer",
            Self::Usize => "a non-negative integer this host can index",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeIntegerRange {
    pub value: i64,
    pub target: DecodeIntegerTarget,
}
record!(DecodeIntegerRange {
    value: "value",
    target: "target"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeLength {
    pub expected: CountRule,
    pub got: Count,
}
record!(DecodeLength {
    expected: "expected",
    got: "got"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckDecisionFailure {
    pub resource: Resource,
    pub decision: Decision,
}
record!(CheckDecisionFailure {
    resource: "resource",
    decision: "decision"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardFailure {
    NotCard(Found),
    Empty,
}
variant!(CardFailure { NotCard(Found): "not-card", Empty: "empty" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteFailureTooLong {
    pub bytes: Count,
    pub limit: Count,
}
record!(NoteFailureTooLong {
    bytes: "bytes",
    limit: "limit"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteFailure {
    Empty,
    TooLong(NoteFailureTooLong),
    Multiline,
}
variant!(NoteFailure { Empty: "empty", TooLong(NoteFailureTooLong): "too-long", Multiline: "multiline" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum JsonShape {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}
label!(JsonShape);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonShapeFailure {
    pub expected: Vec<JsonShape>,
    pub found: JsonShape,
}
record!(JsonShapeFailure {
    expected: "expected",
    found: "found"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum JsonNumberTarget {
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
    U128,
    F32,
    F64,
}
label!(JsonNumberTarget);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum JsonNumberFault {
    NotIntegerRepresentation,
    OutOfRange,
    NonFinite,
}
label!(JsonNumberFault);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonNumberFailure {
    pub text: String,
    pub target: JsonNumberTarget,
    pub cause: JsonNumberFault,
}
record!(JsonNumberFailure {
    text: "text",
    target: "target",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonMapKeyFailure {
    pub text: String,
    pub lexeme: String,
    pub target: JsonNumberTarget,
}
record!(JsonMapKeyFailure {
    text: "text",
    lexeme: "lexeme",
    target: "target"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum JsonStringFailure {
    BorrowedRequired,
}
label!(JsonStringFailure);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base64FailureInvalidByte {
    pub index: Count,
    pub byte: u8,
}
record!(Base64FailureInvalidByte {
    index: "index",
    byte: "byte"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base64FailureInvalidLength {
    pub length: Count,
}
record!(Base64FailureInvalidLength { length: "length" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base64FailureInvalidLastSymbol {
    pub index: Count,
    pub byte: u8,
}
record!(Base64FailureInvalidLastSymbol {
    index: "index",
    byte: "byte"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base64Failure {
    InvalidByte(Base64FailureInvalidByte),
    InvalidLength(Base64FailureInvalidLength),
    InvalidLastSymbol(Base64FailureInvalidLastSymbol),
    InvalidPadding,
}
variant!(Base64Failure {
    InvalidByte(Base64FailureInvalidByte): "invalid-byte", InvalidLength(Base64FailureInvalidLength): "invalid-length",
    InvalidLastSymbol(Base64FailureInvalidLastSymbol): "invalid-last-symbol", InvalidPadding: "invalid-padding",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiniteBitsFailure {
    pub bits: Count,
}
record!(FiniteBitsFailure { bits: "bits" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum OsStringPlatform {
    Unix,
    Windows,
}
label!(OsStringPlatform);

impl OsStringPlatform {
    fn describe(self) -> &'static str {
        match self {
            Self::Unix => "Unix",
            Self::Windows => "Windows",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsStringPlatformFailure {
    pub expected: OsStringPlatform,
    pub found: OsStringPlatform,
}
record!(OsStringPlatformFailure {
    expected: "expected",
    found: "found"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexicalPathFailureNotWorkingDirectory {
    pub rules: PathRules,
}
record!(LexicalPathFailureNotWorkingDirectory { rules: "rules" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexicalPathFailure {
    Relative,
    MalformedRoot,
    MoreThanRoot,
    EntryName(EntryNameFailure),
    ParentInPosition,
    NonCanonicalPosition,
    NotWorkingDirectory(LexicalPathFailureNotWorkingDirectory),
}
variant!(LexicalPathFailure {
    Relative: "relative", MalformedRoot: "malformed-root", MoreThanRoot: "more-than-root",
    EntryName(EntryNameFailure): "entry-name", ParentInPosition: "parent-in-position",
    NonCanonicalPosition: "non-canonical-position", NotWorkingDirectory(LexicalPathFailureNotWorkingDirectory): "not-working-directory",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathDecodeFailure {
    /// Escaped native spelling, including quotes.
    pub shown: String,
    pub cause: LexicalPathFailure,
}
record!(PathDecodeFailure {
    shown: "shown",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceDecodeFailurePositionMismatch {
    pub assembled: String,
    pub claimed: String,
}
record!(TraceDecodeFailurePositionMismatch {
    assembled: "assembled",
    claimed: "claimed"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceDecodeFailure {
    NonDirectorySpine,
    PositionMismatch(TraceDecodeFailurePositionMismatch),
}
variant!(TraceDecodeFailure {
    NonDirectorySpine: "non-directory-spine", PositionMismatch(TraceDecodeFailurePositionMismatch): "position-mismatch",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealSpellingFailure {
    pub text: String,
}
record!(SealSpellingFailure { text: "text" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedEnvironmentFailure {
    pub name: String,
    pub reserved: ReservedVar,
}
record!(ReservedEnvironmentFailure {
    name: "name",
    reserved: "reserved"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViaFailureDuplicateTag {
    pub tag: String,
}
record!(ViaFailureDuplicateTag { tag: "tag" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViaFailure {
    Empty,
    EmptyTag,
    DuplicateTag(ViaFailureDuplicateTag),
}
variant!(ViaFailure { Empty: "empty", EmptyTag: "empty-tag", DuplicateTag(ViaFailureDuplicateTag): "duplicate-tag" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationOverflow {
    pub seconds: Count,
    pub nanos: u32,
}
record!(DurationOverflow {
    seconds: "seconds",
    nanos: "nanos"
});

/// A scheme that writes a variable it neither quantifies, anchors by μ, nor
/// records as weak; both as the type printer shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenScheme {
    pub scheme: String,
    pub variable: String,
}
record!(OpenScheme {
    scheme: "scheme",
    variable: "variable"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimestampParseFailure {
    pub text: String,
    pub detail: String,
}
record!(TimestampParseFailure {
    text: "text",
    detail: "detail"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeCause {
    Shape(ShapeFailure),
    MissingField(String),
    UnknownField(UnknownField),
    DuplicateField(String),
    UnknownTag(UnknownTag),
    MissingPayload(String),
    UnexpectedPayload(UnexpectedPayload),
    IntegerRange(DecodeIntegerRange),
    Length(DecodeLength),
    CheckDecision(CheckDecisionFailure),
    AgentName(AgentNameFailure),
    Cron(CronFailure),
    Duration(DurationFailure),
    TurnSpan(InvertedTurnSpan),
    Regex(RegexFailure),
    EmptyProviderRole,
    Card(CardFailure),
    Note(NoteFailure),
    JsonShape(JsonShapeFailure),
    JsonNumber(JsonNumberFailure),
    JsonMapKey(JsonMapKeyFailure),
    JsonString(JsonStringFailure),
    JsonSyntax(JsonSyntaxFailure),
    Base64(Base64Failure),
    FiniteBits(FiniteBitsFailure),
    OsStringPlatform(OsStringPlatformFailure),
    Path(PathDecodeFailure),
    Trace(TraceDecodeFailure),
    SealSpelling(SealSpellingFailure),
    ReservedEnvironment(ReservedEnvironmentFailure),
    Via(ViaFailure),
    DurationOverflow(DurationOverflow),
    Timestamp(TimestampParseFailure),
    OpenScheme(OpenScheme),
}
variant!(DecodeCause {
    Shape(ShapeFailure): "shape", MissingField(String): "missing-field", UnknownField(UnknownField): "unknown-field",
    DuplicateField(String): "duplicate-field", UnknownTag(UnknownTag): "unknown-tag", MissingPayload(String): "missing-payload",
    UnexpectedPayload(UnexpectedPayload): "unexpected-payload", IntegerRange(DecodeIntegerRange): "integer-range",
    Length(DecodeLength): "length", CheckDecision(CheckDecisionFailure): "check-decision",
    AgentName(AgentNameFailure): "agent-name", Cron(CronFailure): "cron", Duration(DurationFailure): "duration",
    TurnSpan(InvertedTurnSpan): "turn-span", Regex(RegexFailure): "regex", EmptyProviderRole: "empty-provider-role",
    Card(CardFailure): "card", Note(NoteFailure): "note", JsonShape(JsonShapeFailure): "json-shape",
    JsonNumber(JsonNumberFailure): "json-number", JsonMapKey(JsonMapKeyFailure): "json-map-key",
    JsonString(JsonStringFailure): "json-string", JsonSyntax(JsonSyntaxFailure): "json-syntax",
    Base64(Base64Failure): "base64", FiniteBits(FiniteBitsFailure): "finite-bits",
    OsStringPlatform(OsStringPlatformFailure): "os-string-platform", Path(PathDecodeFailure): "path",
    Trace(TraceDecodeFailure): "trace", SealSpelling(SealSpellingFailure): "seal-spelling",
    ReservedEnvironment(ReservedEnvironmentFailure): "reserved-environment", Via(ViaFailure): "via",
    DurationOverflow(DurationOverflow): "duration-overflow", Timestamp(TimestampParseFailure): "timestamp", OpenScheme(OpenScheme): "open-scheme",
});

impl DecodeCause {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Shape(s) => format!(
                "expected {}, got {}",
                s.expected.describe(),
                s.found.describe()
            ),
            Self::MissingField(field) => format!("missing field {field:?}"),
            Self::UnknownField(u) => {
                let near =
                    crate::text::near_names(&u.field, u.offered.iter().map(String::as_str), 1);
                match near.first() {
                    Some(key) => format!("unknown field {:?}: did you mean {key:?}?", u.field),
                    None => format!(
                        "unknown field {:?}; expected {}",
                        u.field,
                        u.offered
                            .iter()
                            .map(|name| format!("{name:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
            Self::DuplicateField(field) => format!("field {field:?} given twice"),
            Self::UnknownTag(u) => format!(
                "unknown tag {:?}; expected {}",
                u.tag,
                u.offered
                    .iter()
                    .map(|name| format!("{name:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::MissingPayload(tag) => format!("tag {tag:?} requires a value"),
            Self::UnexpectedPayload(u) => {
                format!("tag {:?} takes no value, got {}", u.tag, u.found.describe())
            }
            Self::IntegerRange(r) => format!("{} is not {}", r.value, r.target.describe()),
            Self::Length(l) => format!("expected a length of {}, got {}", l.expected, l.got),
            Self::CheckDecision(c) => format!(
                "a `{}` check is never `{}`",
                <&str>::from(c.resource),
                <&str>::from(c.decision)
            ),
            Self::AgentName(c) => c.describe(),
            Self::Cron(c) => c.describe(),
            Self::Duration(c) => c.describe(),
            Self::TurnSpan(c) => c.describe(),
            Self::Regex(c) => c.describe(),
            Self::EmptyProviderRole => "provider role cannot be empty".into(),
            Self::Card(c) => c.describe(),
            Self::Note(c) => c.describe(),
            Self::JsonShape(c) => format!(
                "expected JSON {}, got {}",
                c.expected
                    .iter()
                    .map(|s| <&str>::from(*s))
                    .collect::<Vec<_>>()
                    .join(" or "),
                <&str>::from(c.found)
            ),
            Self::JsonNumber(c) => format!(
                "JSON number {:?} cannot decode as {}: {}",
                c.text,
                <&str>::from(c.target),
                match c.cause {
                    JsonNumberFault::NotIntegerRepresentation =>
                        "not an accepted integer representation",
                    JsonNumberFault::OutOfRange => "out of range",
                    JsonNumberFault::NonFinite => "not finite",
                }
            ),
            Self::JsonMapKey(c) => format!(
                "JSON map key {:?} (token {:?}) cannot decode as {}: expected an unescaped numeric spelling valid for this type",
                c.text,
                c.lexeme,
                <&str>::from(c.target)
            ),
            Self::JsonString(JsonStringFailure::BorrowedRequired) => {
                "JSON string requires borrowing but contains escapes".into()
            }
            Self::JsonSyntax(c) => c.describe(),
            Self::Base64(c) => c.describe(),
            Self::FiniteBits(c) => format!("IEEE bits {} do not represent a finite float", c.bits),
            Self::OsStringPlatform(c) => format!(
                "cannot decode a {} OS string on {}",
                c.found.describe(),
                c.expected.describe()
            ),
            Self::Path(c) => c.describe(),
            Self::Trace(c) => c.describe(),
            Self::SealSpelling(c) => format!(
                "invalid seal {:?}: expected exactly 16 lowercase hexadecimal digits",
                c.text
            ),
            Self::ReservedEnvironment(c) => format!(
                "environment variable {:?} is reserved ({})",
                c.name,
                <&str>::from(c.reserved)
            ),
            Self::Via(c) => c.describe(),
            Self::DurationOverflow(c) => format!(
                "duration overflows: {} seconds and {} nanoseconds",
                c.seconds, c.nanos
            ),
            Self::Timestamp(c) => format!("invalid timestamp {:?}: {}", c.text, c.detail),
            Self::OpenScheme(c) => c.describe(),
        }
    }
}

impl OpenScheme {
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "scheme `{}` is not closed: it writes {}, which it neither quantifies, anchors by μ, nor records as weak",
            self.scheme, self.variable
        )
    }
}

impl CardFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NotCard(found) => format!("expected a card, got {}", found.describe()),
            Self::Empty => "card cannot be empty".into(),
        }
    }
}

impl NoteFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Empty => "note cannot be empty".into(),
            Self::TooLong(n) => format!("note has {} bytes; limit is {}", n.bytes, n.limit),
            Self::Multiline => "note must be a single line".into(),
        }
    }
}

impl Base64Failure {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::InvalidByte(c) => format!("invalid base64 byte {} at index {}", c.byte, c.index),
            Self::InvalidLength(c) => format!("invalid base64 length {}", c.length),
            Self::InvalidLastSymbol(c) => format!(
                "invalid final base64 symbol {} at index {}",
                c.byte, c.index
            ),
            Self::InvalidPadding => "invalid base64 padding".into(),
        }
    }
}

impl LexicalPathFailure {
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Relative => "path must be absolute",
            Self::MalformedRoot => "malformed path root",
            Self::MoreThanRoot => "expected only a path root",
            Self::EntryName(c) => c.describe(),
            Self::ParentInPosition => "parent component is not a canonical position",
            Self::NonCanonicalPosition => "path position is not canonical",
            Self::NotWorkingDirectory(c) => match c.rules {
                PathRules::Posix => "working-directory path must be absolute",
                PathRules::Windows => "working-directory path must be absolute on a drive or share",
            },
        }
    }
}

impl PathDecodeFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        format!("path {}: {}", self.shown, self.cause.describe())
    }
}

impl TraceDecodeFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NonDirectorySpine => "path trace spine contains a non-directory".into(),
            Self::PositionMismatch(c) => format!(
                "path trace assembles {:?}, but claims {:?}",
                c.assembled, c.claimed
            ),
        }
    }
}

impl ViaFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Empty => "provider route cannot be empty".into(),
            Self::EmptyTag => "provider route tag cannot be empty".into(),
            Self::DuplicateTag(c) => format!("provider route tag {:?} given twice", c.tag),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonInputFailure {
    Syntax(JsonSyntaxFailure),
    Decode(DecodeFailure),
}
variant!(JsonInputFailure { Syntax(JsonSyntaxFailure): "syntax", Decode(DecodeFailure): "decode" });

impl JsonInputFailure {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Syntax(c) => c.describe(),
            Self::Decode(c) => c.describe(),
        }
    }
}

impl fmt::Display for JsonInputFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}
impl std::error::Error for JsonInputFailure {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonEncodeCauseNonUnicodePath {
    /// Escaped native spelling, including quotes.
    pub shown: String,
}
record!(JsonEncodeCauseNonUnicodePath { shown: "shown" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonEncodeCauseNativeComputation {
    pub name: String,
}
record!(JsonEncodeCauseNativeComputation { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonEncodeCause {
    NonUnicodePath(JsonEncodeCauseNonUnicodePath),
    NativeComputation(JsonEncodeCauseNativeComputation),
}
variant!(JsonEncodeCause {
    NonUnicodePath(JsonEncodeCauseNonUnicodePath): "non-unicode-path",
    NativeComputation(JsonEncodeCauseNativeComputation): "native-computation",
});

impl JsonEncodeCause {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NonUnicodePath(c) => format!(
                "path {} is not Unicode and cannot be encoded as JSON",
                c.shown
            ),
            Self::NativeComputation(c) => {
                format!("native computation {:?} cannot be encoded as JSON", c.name)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonEncodeFailure {
    pub path: Vec<DataStep>,
    pub cause: JsonEncodeCause,
}
record!(JsonEncodeFailure {
    path: "path",
    cause: "cause"
});

impl JsonEncodeFailure {
    #[must_use]
    pub fn new(cause: JsonEncodeCause) -> Self {
        Self {
            path: Vec::new(),
            cause,
        }
    }

    #[must_use]
    pub fn at(mut self, step: DataStep) -> Self {
        self.path.insert(0, step);
        self
    }

    #[must_use]
    pub fn field(self, key: impl Into<String>) -> Self {
        self.at(DataStep::Field(key.into()))
    }

    #[must_use]
    pub fn index(self, index: Count) -> Self {
        self.at(DataStep::Index(index))
    }

    #[must_use]
    pub fn payload(self, tag: impl Into<String>) -> Self {
        self.at(DataStep::Payload(tag.into()))
    }

    #[must_use]
    pub fn describe(&self) -> String {
        format!("{self}")
    }
}

impl fmt::Display for JsonEncodeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_path(&self.path, f)?;
        f.write_str(&self.cause.describe())
    }
}
impl std::error::Error for JsonEncodeFailure {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;

    #[test]
    fn decode_and_encode_paths_prepend_outer_context() {
        let path = vec![
            DataStep::Field("outer".into()),
            DataStep::Index(Count::from_u64(3)),
            DataStep::Payload("inner".into()),
        ];
        let decode = DecodeFailure::new(DecodeCause::EmptyProviderRole)
            .payload("inner")
            .index(Count::from_u64(3))
            .field("outer");
        let encode = JsonEncodeFailure::new(JsonEncodeCause::NativeComputation(
            JsonEncodeCauseNativeComputation {
                name: "native".into(),
            },
        ))
        .payload("inner")
        .index(Count::from_u64(3))
        .field("outer");
        assert_eq!(decode.path, path);
        assert_eq!(encode.path, path);
        assert_eq!(DecodeFailure::decode(&decode.clone().encode()), Ok(decode));
        assert_eq!(
            JsonEncodeFailure::decode(&encode.clone().encode()),
            Ok(encode)
        );
    }

    #[test]
    fn boxed_decode_cause_preserves_datum_and_json_equality() {
        let cause = DecodeCause::MissingField("required".into());
        let failure = DecodeFailure::new(cause.clone())
            .payload("inner")
            .index(u64::MAX.into())
            .field("outer");
        let expected = crate::data::FOValue::Map {
            entries: vec![
                ("cause".into(), cause.encode()),
                ("path".into(), failure.path.clone().encode()),
            ],
        };
        assert_eq!(failure.clone().encode(), expected);
        assert_eq!(DecodeFailure::decode(&expected), Ok(failure.clone()));
        let bytes = crate::data::json::to_datum_vec(&failure);
        assert_eq!(
            crate::data::json::from_datum_slice::<DecodeFailure>(&bytes),
            Ok(failure)
        );
    }

    #[test]
    fn malformed_decode_failure_is_an_ordinary_failure() {
        let malformed = crate::data::datum::tag("shape", None);
        let failure = DecodeCause::decode(&malformed).unwrap_err();
        assert_eq!(failure.path, vec![DataStep::Payload("shape".into())]);
        assert!(matches!(*failure.cause, DecodeCause::Shape(_)));
    }

    #[test]
    fn json_domain_facts_retain_lossless_operands() {
        let causes = [
            DecodeCause::FiniteBits(FiniteBitsFailure {
                bits: Count::from_u64(u64::MAX),
            }),
            DecodeCause::JsonNumber(JsonNumberFailure {
                text: "340282366920938463463374607431768211456".into(),
                target: JsonNumberTarget::U128,
                cause: JsonNumberFault::OutOfRange,
            }),
            DecodeCause::DurationOverflow(DurationOverflow {
                seconds: Count::from_u64(u64::MAX),
                nanos: u32::MAX,
            }),
            DecodeCause::Base64(Base64Failure::InvalidByte(Base64FailureInvalidByte {
                index: Count::from_u64(1_u64 << 63),
                byte: 255,
            })),
        ];
        for cause in causes {
            assert_eq!(DecodeCause::decode(&cause.clone().encode()), Ok(cause));
        }
    }
}
