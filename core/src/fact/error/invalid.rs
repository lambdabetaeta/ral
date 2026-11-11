//! Portable invalid failure facts.

use super::decode::{DecodeCause, DecodeFailure, DecodeLength, JsonShape};
use super::io::IoCause;
use super::shared::{
    Count, CountRule, DataStep, Expected, Found, IntOrFloat, IntegerParseCause, JsonSyntaxFailure,
    Number, NumberFault, NumberOrText, OpaqueKind, RegexFailure, ReservedVar, ShapeFailure,
    UnexpectedPayload, UnknownField, UnknownTag, Utf8Failure, ValueShape,
};
use crate::authority::grant::freeze::{Anchor, Place, Selector};
use crate::authority::grant::value::Target;
use crate::data::Finite;
use crate::path::XdgKind;
use crate::path::{PathRules, relative_suffix};
use crate::syntax::quote::quote_word;
use crate::text::{code, indefinite, near_names, plural};
use crate::ty::{RefusedArg, TAG_PREFIX};
use crate::{label, record, variant};
#[cfg(test)]
use std::fmt::Write as _;
use strum::{IntoStaticStr, VariantArray};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidFailure {
    pub operation: String,
    pub path: Vec<DataStep>,
    pub cause: InvalidCause,
}
record!(InvalidFailure {
    operation: "operation",
    path: "path",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArityFailure {
    pub expected: CountRule,
    pub got: Count,
}
record!(ArityFailure {
    expected: "expected",
    got: "got"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberRange {
    pub value: Number,
    pub minimum: Number,
    pub maximum: Number,
}
record!(NumberRange {
    value: "value",
    minimum: "minimum",
    maximum: "maximum"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressableRange {
    pub start: i64,
    pub end: i64,
}
record!(AddressableRange {
    start: "start",
    end: "end"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HalfOpenRange {
    pub start: Count,
    pub end: Count,
}
record!(HalfOpenRange {
    start: "start",
    end: "end"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonData {
    pub leaf: OpaqueKind,
    pub nested: bool,
}
record!(NonData {
    leaf: "leaf",
    nested: "nested"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Multiline {
    pub lines: Count,
}
record!(Multiline { lines: "lines" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberFailure {
    pub text: String,
    pub target: IntOrFloat,
    pub why: NumberFault,
}
record!(NumberFailure {
    text: "text",
    target: "target",
    why: "why"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidCause {
    Shape(ShapeFailure),
    Arity(ArityFailure),
    MissingField(String),
    UnknownField(UnknownField),
    DuplicateField(String),
    UnknownTag(UnknownTag),
    MissingPayload(String),
    UnexpectedPayload(UnexpectedPayload),
    Range(NumberRange),
    AddressableRange(AddressableRange),
    HalfOpenRange(HalfOpenRange),
    NonData(NonData),
    TitleRequired,
    Empty,
    Multiline(Multiline),
    Number(NumberFailure),
    NonIntegral(Finite),
    IntegerRange(Finite),
    EmptyPath,
    ReservedEnvironment(ReservedVar),
    ZeroStatus,
    Admission(AdmissionFailure),
    Decode(DecodeFailure),
    IllTyped(IllTyped),
    Path(PathFailure),
    Grant(GrantFailure),
    Codec(CodecFailure),
}
variant!(InvalidCause { Shape(ShapeFailure): "shape", Arity(ArityFailure): "arity", MissingField(String): "missing-field", UnknownField(UnknownField): "unknown-field", DuplicateField(String): "duplicate-field", UnknownTag(UnknownTag): "unknown-tag", MissingPayload(String): "missing-payload", UnexpectedPayload(UnexpectedPayload): "unexpected-payload", Range(NumberRange): "range", AddressableRange(AddressableRange): "addressable-range", HalfOpenRange(HalfOpenRange): "half-open-range", NonData(NonData): "non-data", TitleRequired: "title-required", Empty: "empty", Multiline(Multiline): "multiline", Number(NumberFailure): "number", NonIntegral(Finite): "non-integral", IntegerRange(Finite): "integer-range", EmptyPath: "empty-path", ReservedEnvironment(ReservedVar): "reserved-environment", ZeroStatus: "zero-status", Admission(AdmissionFailure): "admission", Decode(DecodeFailure): "decode", IllTyped(IllTyped): "ill-typed", Path(PathFailure): "path", Grant(GrantFailure): "grant", Codec(CodecFailure): "codec" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionFailure {
    pub pointer: String,
    pub cause: AdmissionCause,
}
record!(AdmissionFailure {
    pointer: "pointer",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionUses {
    pub found: Found,
    pub expected: Expected,
}
record!(AdmissionUses {
    found: "found",
    expected: "expected"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionExtraField {
    pub field: String,
    pub fields: Vec<String>,
}
record!(AdmissionExtraField {
    field: "field",
    fields: "fields"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionShares {
    pub other: String,
    pub other_side: NumberOrText,
    pub side: NumberOrText,
}
record!(AdmissionShares {
    other: "other",
    other_side: "other-side",
    side: "side"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionExport {
    pub found: String,
    pub expected: String,
}
record!(AdmissionExport {
    found: "found",
    expected: "expected"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionCause {
    Uses(AdmissionUses),
    NoField(String),
    ExtraField(AdmissionExtraField),
    Shares(AdmissionShares),
    Export(AdmissionExport),
}
variant!(AdmissionCause { Uses(AdmissionUses): "uses", NoField(String): "no-field", ExtraField(AdmissionExtraField): "extra-field", Shares(AdmissionShares): "shares", Export(AdmissionExport): "export" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum AdmissionAdvice {
    NumericConversion,
    DecoderHasNoVariants,
}
label!(AdmissionAdvice);

/// A block the runtime asked the checker to vet, and the checker refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IllTyped {
    pub role: BlockRole,
    diagnostics: Vec<TypeDiagnostic>,
}

impl IllTyped {
    pub fn new(role: BlockRole, diagnostics: Vec<TypeDiagnostic>) -> Option<Self> {
        (!diagnostics.is_empty()).then_some(Self { role, diagnostics })
    }

    pub fn diagnostics(&self) -> &[TypeDiagnostic] {
        &self.diagnostics
    }
}

impl crate::data::datum::Datum for IllTyped {
    fn encode(self) -> crate::data::FOValue {
        crate::data::FOValue::Map {
            entries: vec![
                ("diagnostics".into(), self.diagnostics.encode()),
                ("role".into(), self.role.encode()),
            ],
        }
    }

    fn decode(value: &crate::data::FOValue) -> Result<Self, DecodeFailure> {
        use crate::data::datum::{exact_keys, field};
        exact_keys(value, &["role", "diagnostics"])?;
        let role = field(value, "role")?;
        let diagnostics = field(value, "diagnostics")?;
        Self::new(role, diagnostics).ok_or_else(|| {
            DecodeFailure::new(DecodeCause::Length(DecodeLength {
                expected: CountRule::AtLeast(1usize.into()),
                got: 0usize.into(),
            }))
            .field("diagnostics")
        })
    }
}

/// What a vetted block was to stand in for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockRole {
    /// A per-command handler (`alias`, `within [handlers: …]`).
    Handler(String),
    /// `within [handler: …]`, which stands in for every command.
    CatchAll,
    Hook(String),
    /// An rc `bindings:` entry.
    Binding(String),
}
variant!(BlockRole { Handler(String): "handler", CatchAll: "catch-all", Hook(String): "hook", Binding(String): "binding" });

/// One checker diagnostic: its stable code (`T0057`) and rendered message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDiagnostic {
    pub code: String,
    pub message: String,
}
record!(TypeDiagnostic {
    code: "code",
    message: "message"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum GrantVocabulary {
    Place,
    ExecTarget,
    DirTarget,
    Net,
    Gpu,
    Editor,
    Shell,
}
label!(GrantVocabulary);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum GrantForm {
    Grant,
    Manifest,
}
label!(GrantForm);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum RefusedGrantKey {
    DetachMovedToShell,
    ManifestCapabilitiesUnenforced,
}
label!(RefusedGrantKey);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum AbsolutePathRefusal {
    Malformed,
    MixedHead,
    NtObject,
    Driveless,
    DriveRelative,
    ForeignOrRelative,
}
label!(AbsolutePathRefusal);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum RelativeSubpathRefusal {
    Empty,
    Drive,
    MixedHead,
    Rooted,
    NtObject,
    Driveless,
}
label!(RelativeSubpathRefusal);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum HomeClassification {
    Relative,
    Absolute,
}
label!(HomeClassification);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum MissingGrantAnchor {
    Home,
    Scratch,
}
label!(MissingGrantAnchor);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum WorldRelation {
    Root,
    Home,
    AncestorOfHome,
}
label!(WorldRelation);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeSigilFailure {
    Tilde(RuntimeSigilFailureTilde),
    Xdg(RuntimeSigilFailureXdg),
    UnknownXdg(String),
}
variant!(RuntimeSigilFailure { Tilde(RuntimeSigilFailureTilde): "tilde", Xdg(RuntimeSigilFailureXdg): "xdg", UnknownXdg(String): "unknown-xdg" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitPointerFailure {
    NoGitdirLine,
    Unclaimed(String),
    Unreadable(Box<IoCause>),
}
variant!(GitPointerFailure { NoGitdirLine: "no-gitdir-line", Unclaimed(String): "unclaimed", Unreadable(Box<IoCause>): "unreadable" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathFailure {
    NotUnicode(PathFailureNotUnicode),
    NotWorkingDirectory(PathFailureNotWorkingDirectory),
    NoProcessCwd(PathFailureNoProcessCwd),
    Glob(PathFailureGlob),
}
variant!(PathFailure { NotUnicode(PathFailureNotUnicode): "not-unicode", NotWorkingDirectory(PathFailureNotWorkingDirectory): "not-working-directory", NoProcessCwd(PathFailureNoProcessCwd): "no-process-cwd", Glob(PathFailureGlob): "glob" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobFailure {
    ParentAfterWildcard,
    HiddenOnly(GlobFailureHiddenOnly),
    NotAbsolute,
    Syntax(GlobFailureSyntax),
}
variant!(GlobFailure { ParentAfterWildcard: "parent-after-wildcard", HiddenOnly(GlobFailureHiddenOnly): "hidden-only", NotAbsolute: "not-absolute", Syntax(GlobFailureSyntax): "syntax" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantFailure {
    UnknownVocabulary(GrantFailureUnknownVocabulary),
    RefusedKey(GrantFailureRefusedKey),
    BareNameRequired(String),
    ProgramIsDirectory(Target),
    RootExclusion(Place),
    Unwalkable(GrantFailureUnwalkable),
    GitPointer(GrantFailureGitPointer),
    OutsideHome(GrantFailureOutsideHome),
    RelativeAnchor(GrantFailureRelativeAnchor),
    AbsolutePath(GrantFailureAbsolutePath),
    Subpath(GrantFailureSubpath),
    RuntimeSigil(RuntimeSigilFailure),
    MissingAnchor(MissingGrantAnchor),
    WorldSelector(GrantFailureWorldSelector),
    EmptyPathSelector,
}
variant!(GrantFailure { UnknownVocabulary(GrantFailureUnknownVocabulary): "unknown-vocabulary", RefusedKey(GrantFailureRefusedKey): "refused-key", BareNameRequired(String): "bare-name-required", ProgramIsDirectory(Target): "program-is-directory", RootExclusion(Place): "root-exclusion", Unwalkable(GrantFailureUnwalkable): "unwalkable", GitPointer(GrantFailureGitPointer): "git-pointer", OutsideHome(GrantFailureOutsideHome): "outside-home", RelativeAnchor(GrantFailureRelativeAnchor): "relative-anchor", AbsolutePath(GrantFailureAbsolutePath): "absolute-path", Subpath(GrantFailureSubpath): "subpath", RuntimeSigil(RuntimeSigilFailure): "runtime-sigil", MissingAnchor(MissingGrantAnchor): "missing-anchor", WorldSelector(GrantFailureWorldSelector): "world-selector", EmptyPathSelector: "empty-path-selector" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Utf8Subject {
    Input,
    CapturedOutput,
    Path(String),
}
variant!(Utf8Subject { Input: "input", CapturedOutput: "captured-output", Path(String): "path" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecJsonNumberFailure {
    pub text: String,
    pub cause: JsonValueNumberCause,
}
record!(CodecJsonNumberFailure {
    text: "text",
    cause: "cause"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum JsonValueNumberCause {
    OutOfI64,
    NonFinite,
}
label!(JsonValueNumberCause);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonValueFailure {
    Syntax(JsonSyntaxFailure),
    Number(CodecJsonNumberFailure),
}
variant!(JsonValueFailure { Syntax(JsonSyntaxFailure): "syntax", Number(CodecJsonNumberFailure): "number" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPointerCause {
    MissingMember(Vec<String>),
    NoElement(Count),
    Scalar(JsonShape),
}
variant!(JsonPointerCause { MissingMember(Vec<String>): "missing-member", NoElement(Count): "no-element", Scalar(JsonShape): "scalar" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum LiteralOrRegex {
    Literal,
    Regex,
}
label!(LiteralOrRegex);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecFailure {
    Utf8(CodecFailureUtf8),
    JsonSyntax(JsonSyntaxFailure),
    JsonNumber(CodecJsonNumberFailure),
    JsonPointer(CodecFailureJsonPointer),
    JsonLine(CodecFailureJsonLine),
    CsvDuplicateHeader(String),
    CsvWidth(CodecFailureCsvWidth),
    CsvNoColumns,
    CsvKeys(CodecFailureCsvKeys),
    CsvCell(CodecFailureCsvCell),
    CsvRead(CsvReadFailure),
    CsvFinalize(IoCause),
    NotJson(CodecFailureNotJson),
    PosixSplit,
    PosixQuoteNul,
    PosixQuoteUnclassified(String),
    Regex(RegexFailure),
    MatchCount(CodecFailureMatchCount),
    EmptyReplacementPattern,
}
variant!(CodecFailure { Utf8(CodecFailureUtf8): "utf8", JsonSyntax(JsonSyntaxFailure): "json-syntax", JsonNumber(CodecJsonNumberFailure): "json-number", JsonPointer(CodecFailureJsonPointer): "json-pointer", JsonLine(CodecFailureJsonLine): "json-line", CsvDuplicateHeader(String): "csv-duplicate-header", CsvWidth(CodecFailureCsvWidth): "csv-width", CsvNoColumns: "csv-no-columns", CsvKeys(CodecFailureCsvKeys): "csv-keys", CsvCell(CodecFailureCsvCell): "csv-cell", CsvRead(CsvReadFailure): "csv-read", CsvFinalize(IoCause): "csv-finalize", NotJson(CodecFailureNotJson): "not-json", PosixSplit: "posix-split", PosixQuoteNul: "posix-quote-nul", PosixQuoteUnclassified(String): "posix-quote-unclassified", Regex(RegexFailure): "regex", MatchCount(CodecFailureMatchCount): "match-count", EmptyReplacementPattern: "empty-replacement-pattern" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvPosition {
    pub byte: Count,
    pub line: Count,
    pub record: Count,
}
record!(CsvPosition {
    byte: "byte",
    line: "line",
    record: "record"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsvReadFailure {
    Io(IoCause),
    Utf8(CsvReadFailureUtf8),
    UnequalLengths(CsvReadFailureUnequalLengths),
    Seek,
    Serialize(String),
    Deserialize(CsvReadFailureDeserialize),
    Unclassified(String),
}
variant!(CsvReadFailure { Io(IoCause): "io", Utf8(CsvReadFailureUtf8): "utf8", UnequalLengths(CsvReadFailureUnequalLengths): "unequal-lengths", Seek: "seek", Serialize(String): "serialize", Deserialize(CsvReadFailureDeserialize): "deserialize", Unclassified(String): "unclassified" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsvDeserializeCause {
    Message(String),
    Unsupported(String),
    UnexpectedEndOfRow,
    InvalidUtf8(Utf8Failure),
    ParseBool,
    ParseInt(IntegerParseCause),
    ParseFloat,
}
variant!(CsvDeserializeCause { Message(String): "message", Unsupported(String): "unsupported", UnexpectedEndOfRow: "unexpected-end-of-row", InvalidUtf8(Utf8Failure): "invalid-utf8", ParseBool: "parse-bool", ParseInt(IntegerParseCause): "parse-int", ParseFloat: "parse-float" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSigilFailureTilde {
    pub suffix: String,
}
record!(RuntimeSigilFailureTilde { suffix: "suffix" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSigilFailureXdg {
    pub kind: XdgKind,
    pub suffix: String,
}
record!(RuntimeSigilFailureXdg {
    kind: "kind",
    suffix: "suffix"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFailureNotUnicode {
    pub shown: String,
}
record!(PathFailureNotUnicode { shown: "shown" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFailureNotWorkingDirectory {
    /// Escaped native spelling, including quotes.
    pub path: String,
    pub rules: PathRules,
}
record!(PathFailureNotWorkingDirectory {
    path: "path",
    rules: "rules"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFailureNoProcessCwd {
    /// Escaped native spelling, including quotes.
    pub path: String,
}
record!(PathFailureNoProcessCwd { path: "path" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFailureGlob {
    pub pattern: String,
    pub cause: GlobFailure,
}
record!(PathFailureGlob {
    pattern: "pattern",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobFailureHiddenOnly {
    pub component: String,
}
record!(GlobFailureHiddenOnly {
    component: "component"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobFailureSyntax {
    pub component: String,
    pub detail: String,
}
record!(GlobFailureSyntax {
    component: "component",
    detail: "detail"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFailureUnknownVocabulary {
    pub vocabulary: GrantVocabulary,
    pub tag: String,
    pub offered: Vec<String>,
}
record!(GrantFailureUnknownVocabulary {
    vocabulary: "vocabulary",
    tag: "tag",
    offered: "offered"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFailureRefusedKey {
    pub form: GrantForm,
    pub key: String,
    pub cause: RefusedGrantKey,
}
record!(GrantFailureRefusedKey {
    form: "form",
    key: "key",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFailureUnwalkable {
    pub place: Place,
    pub cause: Box<IoCause>,
}
record!(GrantFailureUnwalkable {
    place: "place",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFailureGitPointer {
    pub path: String,
    pub cause: GitPointerFailure,
}
record!(GrantFailureGitPointer {
    path: "path",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFailureOutsideHome {
    pub anchor: Anchor,
    pub written: String,
    pub resolved: String,
    pub home: String,
    pub xdg: Option<String>,
    pub classification: HomeClassification,
}
record!(GrantFailureOutsideHome {
    anchor: "anchor",
    written: "written",
    resolved: "resolved",
    home: "home",
    xdg: "xdg",
    classification: "classification"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFailureRelativeAnchor {
    pub anchor: Anchor,
    pub path: String,
}
record!(GrantFailureRelativeAnchor {
    anchor: "anchor",
    path: "path"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFailureAbsolutePath {
    pub raw: String,
    pub cause: AbsolutePathRefusal,
}
record!(GrantFailureAbsolutePath {
    raw: "raw",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFailureSubpath {
    pub anchor: Anchor,
    pub raw: String,
    pub cause: RelativeSubpathRefusal,
}
record!(GrantFailureSubpath {
    anchor: "anchor",
    raw: "raw",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFailureWorldSelector {
    pub selector: Selector,
    pub requested: String,
    pub relation: WorldRelation,
}
record!(GrantFailureWorldSelector {
    selector: "selector",
    requested: "requested",
    relation: "relation"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecFailureUtf8 {
    pub subject: Utf8Subject,
    pub cause: Utf8Failure,
}
record!(CodecFailureUtf8 {
    subject: "subject",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecFailureJsonPointer {
    pub at: String,
    pub token: String,
    pub cause: JsonPointerCause,
}
record!(CodecFailureJsonPointer {
    at: "at",
    token: "token",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecFailureJsonLine {
    pub line: Count,
    pub cause: JsonValueFailure,
}
record!(CodecFailureJsonLine {
    line: "line",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecFailureCsvWidth {
    pub line: Option<Count>,
    pub expected: Count,
    pub got: Count,
}
record!(CodecFailureCsvWidth {
    line: "line",
    expected: "expected",
    got: "got"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecFailureCsvKeys {
    pub has_row: Count,
    pub lacks_row: Count,
    pub key: String,
}
record!(CodecFailureCsvKeys {
    has_row: "has-row",
    lacks_row: "lacks-row",
    key: "key"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecFailureCsvCell {
    pub row: Count,
    pub key: String,
    pub found: ValueShape,
}
record!(CodecFailureCsvCell {
    row: "row",
    key: "key",
    found: "found"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecFailureMatchCount {
    pub kind: LiteralOrRegex,
    pub pattern: String,
    pub matches: Count,
}
record!(CodecFailureMatchCount {
    kind: "kind",
    pattern: "pattern",
    matches: "matches"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvReadFailureUtf8 {
    pub position: Option<CsvPosition>,
    pub field: Count,
    pub valid_up_to: Count,
}
record!(CsvReadFailureUtf8 {
    position: "position",
    field: "field",
    valid_up_to: "valid-up-to"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvReadFailureUnequalLengths {
    pub position: Option<CsvPosition>,
    pub expected: Count,
    pub got: Count,
}
record!(CsvReadFailureUnequalLengths {
    position: "position",
    expected: "expected",
    got: "got"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvReadFailureDeserialize {
    pub position: Option<CsvPosition>,
    pub field: Option<Count>,
    pub cause: CsvDeserializeCause,
}
record!(CsvReadFailureDeserialize {
    position: "position",
    field: "field",
    cause: "cause"
});

impl InvalidFailure {
    pub fn describe(&self) -> String {
        if let Some((index, found, _)) = self.argv_refusal() {
            return format!(
                "cannot pass {} as argument at index {index} to `{}`",
                found.describe(),
                self.operation
            );
        }
        let mut context = format!("{}: ", self.operation);
        let _ = super::decode::write_path(&self.path, &mut context);
        context.push_str(&self.intrinsic_description());
        context
    }
}

impl ArityFailure {
    pub fn describe(&self) -> String {
        let (rule, count) = match self.expected {
            CountRule::Exactly(count) => ("exactly", count),
            CountRule::AtLeast(count) => ("at least", count),
        };
        format!(
            "expected {rule} {}, got {}",
            plural(count.to_u64(), "argument"),
            self.got
        )
    }
}

impl NumberRange {
    pub fn describe(&self) -> String {
        format!(
            "{} is outside {}..={}",
            number_description(self.value),
            number_description(self.minimum),
            number_description(self.maximum)
        )
    }
}

impl AddressableRange {
    pub fn describe(&self) -> String {
        format!("range {}..{} cannot be addressed", self.start, self.end)
    }
}

impl HalfOpenRange {
    pub fn describe(&self) -> String {
        format!("range {}..{} is inverted", self.start, self.end)
    }
}

impl NonData {
    pub fn describe(&self) -> String {
        if self.nested {
            format!(
                "expected plain data, but the value contains {}",
                indefinite(<&str>::from(self.leaf))
            )
        } else {
            format!(
                "expected plain data, found {}",
                indefinite(<&str>::from(self.leaf))
            )
        }
    }
}

impl Multiline {
    pub fn describe(&self) -> String {
        format!("lines: {}", self.lines)
    }
}

impl NumberFailure {
    pub fn describe(&self) -> String {
        let text = self.text.escape_debug();
        match (self.target, self.why) {
            (IntOrFloat::Int, NumberFault::Syntax) => {
                format!("'{text}' is not a valid integer")
            }
            (IntOrFloat::Int, NumberFault::OutOfRange) => {
                format!("integer '{text}' is outside the integer range")
            }
            (IntOrFloat::Float, NumberFault::Syntax) => {
                format!("'{text}' is not a valid number")
            }
            (_, NumberFault::NonFinite) => format!("'{text}' is not a finite number"),
            (IntOrFloat::Float, NumberFault::OutOfRange) => {
                format!("number '{text}' is outside the Float range")
            }
        }
    }
}

impl InvalidCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Shape(shape) => format!(
                "expected {}, got {}",
                shape.expected.describe(),
                shape.found.describe()
            ),
            Self::Arity(cause) => cause.describe(),
            Self::MissingField(name) => format!("missing required field {name:?}"),
            Self::UnknownField(field) => {
                let near = near_names(&field.field, field.offered.iter().map(String::as_str), 2);
                let suggestion = match near.as_slice() {
                    [nearest] => Some(*nearest),
                    [nearest, next]
                        if strsim::damerau_levenshtein(&field.field, nearest)
                            < strsim::damerau_levenshtein(&field.field, next) =>
                    {
                        Some(*nearest)
                    }
                    _ => None,
                };
                format!(
                    "unknown field {:?}; offered: {}{}",
                    field.field,
                    field.offered.join(", "),
                    suggestion
                        .map_or_else(String::new, |name| format!(" (did you mean '{name}'?)"))
                )
            }
            Self::DuplicateField(name) => format!("duplicate field {name:?}"),
            Self::UnknownTag(tag) => format!(
                "unknown tag {:?}; offered: {}",
                tag.tag,
                tag.offered.join(", ")
            ),
            Self::MissingPayload(tag) => format!("tag {tag:?} needs a value"),
            Self::UnexpectedPayload(payload) => format!(
                "tag {:?} takes no value; got {}",
                payload.tag,
                payload.found.describe()
            ),
            Self::Range(cause) => cause.describe(),
            Self::AddressableRange(cause) => cause.describe(),
            Self::HalfOpenRange(cause) => cause.describe(),
            Self::NonData(cause) => cause.describe(),
            Self::TitleRequired => "a title is required".into(),
            Self::Empty => "must not be empty".into(),
            Self::Multiline(lines) => format!("expected one line, got {} lines", lines.lines),
            Self::Number(cause) => cause.describe(),
            Self::NonIntegral(number) => format!("{number} is not integral"),
            Self::IntegerRange(number) => format!("{number} is outside the integer range"),
            Self::EmptyPath => "a path must not be empty".into(),
            Self::ReservedEnvironment(var) => format!(
                "environment variable {} is reserved",
                match var {
                    ReservedVar::Pwd => "PWD",
                    ReservedVar::Oldpwd => "OLDPWD",
                }
            ),
            Self::ZeroStatus => "a failure's status is 1 to 255; 0 is success".to_owned(),
            Self::Admission(cause) => cause.describe(),
            Self::Decode(cause) => cause.describe(),
            Self::IllTyped(cause) => cause.describe(),
            Self::Path(cause) => cause.describe(),
            Self::Grant(cause) => cause.describe(),
            Self::Codec(cause) => cause.describe(),
        }
    }
}

impl AdmissionFailure {
    pub fn describe(&self) -> String {
        let place = if self.pointer.is_empty() {
            "the value".to_owned()
        } else {
            format!("the value at `{}`", self.pointer)
        };
        match &self.cause {
            AdmissionCause::Uses(uses) => format!(
                "{place} is {}, but this script uses it as {}",
                uses.found.describe(),
                uses.expected.describe()
            ),
            AdmissionCause::NoField(name) => {
                format!("{place} has no field {name:?}, which this script reads")
            }
            AdmissionCause::ExtraField(extra) => {
                let fields = if extra.fields.is_empty() {
                    "no fields".into()
                } else {
                    format!(
                        "exactly the fields {}",
                        extra
                            .fields
                            .iter()
                            .map(|field| format!("{field:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                format!(
                    "{place} has a field {:?}, but this script uses it as a record with {fields}",
                    extra.field
                )
            }
            AdmissionCause::Shares(shares) => {
                let at = |pointer: &str| {
                    if pointer.is_empty() {
                        "the whole value".to_owned()
                    } else {
                        code(pointer)
                    }
                };
                let side = |side| match side {
                    NumberOrText::Number => "a number",
                    NumberOrText::Text => "text",
                };
                format!(
                    "{} is {} and {} is {}, and this script compares them",
                    at(&shares.other),
                    side(shares.other_side),
                    at(&self.pointer),
                    side(shares.side)
                )
            }
            AdmissionCause::Export(export) => format!(
                "{place} is a block of type {}, but this script uses it as {}",
                export.found, export.expected
            ),
        }
    }
}

impl AdmissionUses {
    pub fn describe(&self) -> String {
        format!(
            "found: {}, expected: {}",
            self.found.describe(),
            self.expected.describe()
        )
    }
}

impl AdmissionExtraField {
    pub fn describe(&self) -> String {
        format!("field: {}, fields: {}", self.field, self.fields.join(", "))
    }
}

impl AdmissionShares {
    pub fn describe(&self) -> String {
        format!(
            "other: {}, other side: {}, side: {}",
            self.other,
            <&str>::from(self.other_side),
            <&str>::from(self.side)
        )
    }
}

impl AdmissionExport {
    pub fn describe(&self) -> String {
        format!("found: {}, expected: {}", self.found, self.expected)
    }
}

impl AdmissionCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Uses(value) => format!("uses: {}", value.describe()),
            Self::NoField(value) => format!("no field: {value}"),
            Self::ExtraField(value) => format!("extra field: {}", value.describe()),
            Self::Shares(value) => format!("shares: {}", value.describe()),
            Self::Export(value) => format!("export: {}", value.describe()),
        }
    }
}

impl AdmissionAdvice {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl IllTyped {
    pub fn describe(&self) -> String {
        let diagnostics = self
            .diagnostics
            .iter()
            .map(|TypeDiagnostic { code, message }| format!("{code} {message}"))
            .collect::<Vec<_>>()
            .join("; ");
        format!("{} does not typecheck: {diagnostics}", self.role.describe())
    }
}

impl BlockRole {
    pub fn describe(&self) -> String {
        match self {
            Self::Handler(command) => format!("the `{command}` handler"),
            Self::CatchAll => "the catch-all handler".to_owned(),
            Self::Hook(name) => format!("hook `{name}`"),
            Self::Binding(name) => format!("binding `{name}`"),
        }
    }
}

impl GrantVocabulary {
    pub fn noun(&self) -> &'static str {
        match self {
            Self::Place => "place",
            Self::ExecTarget => "exec target",
            Self::DirTarget => "place or a selector",
            Self::Net => "net mode",
            Self::Gpu => "GPU mode",
            Self::Editor | Self::Shell => "verb",
        }
    }

    pub fn miss(&self, tag: &str) -> Option<String> {
        const FS_TAKES: &str = "fs lists take places such as `` `absolute `` or `` `cwd ``";
        match self {
            Self::Place => match tag {
                "name" | "dir" => Some(format!(
                    "`` `name `` and `` `dir `` belong in an exec list; {FS_TAKES}"
                )),
                _ if tag.parse::<Selector>().is_ok() => Some(format!(
                    "`` `{tag} `` names directories to run programs from, so it belongs in an exec list \
                     as `` `dir `{tag} ``; {FS_TAKES}"
                )),
                _ => None,
            },
            Self::ExecTarget => tag.parse::<Selector>().is_ok().then(|| {
                format!("`` `{tag} `` names directories to run programs from; write `` `dir `{tag} ``")
            }),
            Self::DirTarget => match tag {
                "name" => Some(
                    "`` `dir `` takes a place or a selector; write `` `name git `` in the list itself for a command"
                        .into(),
                ),
                "dir" => Some("`` `dir `` does not nest; write `` `dir `cwd tools ``".into()),
                _ => None,
            },
            Self::Net | Self::Gpu | Self::Editor | Self::Shell => None,
        }
    }

    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl GrantForm {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl RefusedGrantKey {
    pub fn advice(&self) -> &'static str {
        match self {
            Self::DetachMovedToShell => {
                "`detach` is a shell verb now: list it in `shell`, as in `` shell: [`chdir, `detach] ``"
            }
            Self::ManifestCapabilitiesUnenforced => {
                "manifest 'capabilities:' is not enforced: plugins run with host \
                 authority. Remove the key; to confine a plugin invocation, wrap it \
                 in `grant { ... }`."
            }
        }
    }

    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl AbsolutePathRefusal {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl RelativeSubpathRefusal {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl HomeClassification {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl MissingGrantAnchor {
    pub fn describe(&self) -> String {
        match self {
            Self::Home => format!(
                "HOME is unset, so {} and {} places in the policy can't be resolved.  Set HOME in the environment, or replace the home-relative places in the policy with {} paths.",
                code("`home"),
                code("`xdg-*"),
                code("`absolute")
            ),
            Self::Scratch => format!(
                "this session has no scratch directory, so {} places in the policy can't be resolved.  Replace them with {} paths.",
                code("`scratch"),
                code("`absolute")
            ),
        }
    }
}

impl WorldRelation {
    pub fn describe(&self) -> String {
        match self {
            Self::Root => "the filesystem root",
            Self::Home => "the home directory itself",
            Self::AncestorOfHome => "an ancestor of the home directory",
        }
        .into()
    }
}

impl RuntimeSigilFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Tilde(value) => value.describe(),
            Self::Xdg(value) => value.describe(),
            Self::UnknownXdg(token) => format!(
                "unknown xdg token '{token}'; the places are {}. Did you mean one of those?",
                XdgKind::VARIANTS
                    .iter()
                    .map(|&kind| code(&format!("`{}", Anchor::Xdg(kind).tag())))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl GitPointerFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::NoGitdirLine => "no gitdir line".to_owned(),
            Self::Unclaimed(value) => format!("unclaimed: {value}"),
            Self::Unreadable(value) => format!("unreadable: {}", value.as_ref().describe()),
        }
    }
}

impl PathFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::NotUnicode(value) => value.describe(),
            Self::NotWorkingDirectory(value) => value.describe(),
            Self::NoProcessCwd(value) => value.describe(),
            Self::Glob(value) => value.describe(),
        }
    }
}

impl GlobFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::ParentAfterWildcard => "'..' climbs only before the first wildcard".to_owned(),
            Self::HiddenOnly(value) => value.describe(),
            Self::NotAbsolute => "not absolute".to_owned(),
            Self::Syntax(value) => value.describe(),
        }
    }
}

impl GrantFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::UnknownVocabulary(value) => value.describe(),
            Self::RefusedKey(value) => value.describe(),
            Self::BareNameRequired(name) => format!(
                "{} takes a bare command name, not {}", code("`name"), quote_word(name)
            ),
            Self::ProgramIsDirectory(Target::File(place))
                if place.anchor == Anchor::Absolute && place.payload == "/" =>
            {
                "'/' is the filesystem root, a directory, so as an exec target it names a binary that cannot exist".into()
            }
            Self::ProgramIsDirectory(target) => format!(
                "{} is a directory, so as an exec target it names a binary that cannot exist",
                code(&target.to_string())
            ),
            Self::RootExclusion(place) => format!(
                "'{}' is the filesystem root, and excluding the root excludes every path",
                place.payload
            ),
            Self::Unwalkable(value) => value.describe(),
            Self::GitPointer(value) => value.describe(),
            Self::OutsideHome(value) => value.describe(),
            Self::RelativeAnchor(value) => value.describe(),
            Self::AbsolutePath(value) => value.describe(),
            Self::Subpath(value) => value.describe(),
            Self::RuntimeSigil(value) => value.describe(),
            Self::MissingAnchor(value) => value.describe(),
            Self::WorldSelector(value) => value.describe(),
            Self::EmptyPathSelector => format!(
                "{} expands to zero absolute directories; PATH is empty, unset, or contains only relative entries",
                code("`path")
            ),
        }
    }
}

impl Utf8Subject {
    pub fn describe(&self) -> String {
        match self {
            Self::Input => "input".to_owned(),
            Self::CapturedOutput => "captured output".to_owned(),
            Self::Path(value) => format!("path: {value}"),
        }
    }
}

impl CodecJsonNumberFailure {
    pub fn describe(&self) -> String {
        match self.cause {
            JsonValueNumberCause::OutOfI64 => format!(
                "integer {} is outside the supported integer range",
                self.text
            ),
            JsonValueNumberCause::NonFinite => format!(
                "number {} is not representable as a finite Float",
                self.text
            ),
        }
    }
}

impl JsonValueNumberCause {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl JsonValueFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Syntax(value) => value.describe(),
            Self::Number(value) => value.describe(),
        }
    }
}

impl JsonPointerCause {
    pub fn describe(&self) -> String {
        match self {
            Self::MissingMember(value) => format!("missing member: {}", value.join(", ")),
            Self::NoElement(value) => format!("no element: {value}"),
            Self::Scalar(value) => format!("scalar: {}", <&str>::from(*value).to_owned()),
        }
    }
}

impl LiteralOrRegex {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl CodecFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Utf8(value) => value.describe(),
            Self::JsonSyntax(value) => value.describe(),
            Self::JsonNumber(value) => value.describe(),
            Self::JsonPointer(value) => value.describe(),
            Self::JsonLine(value) => value.describe(),
            Self::CsvDuplicateHeader(value) => format!("duplicate header column {value:?}"),
            Self::CsvWidth(value) => value.describe(),
            Self::CsvNoColumns => {
                "the row at index 0 has no keys, and CSV cannot write a row of no columns"
                    .to_owned()
            }
            Self::CsvKeys(value) => value.describe(),
            Self::CsvCell(value) => value.describe(),
            Self::CsvRead(value) => value.describe(),
            Self::CsvFinalize(value) => format!("CSV finalization failed: {}", value.describe()),
            Self::NotJson(value) => value.describe(),
            Self::PosixSplit => "unclosed quote or trailing escape".to_owned(),
            Self::PosixQuoteNul => {
                "cannot quote a string containing NUL for a POSIX shell".to_owned()
            }
            Self::PosixQuoteUnclassified(value) => {
                format!("posix quote unclassified: {value}")
            }
            Self::Regex(value) => value.describe(),
            Self::MatchCount(value) => value.describe(),
            Self::EmptyReplacementPattern => "'from' must be non-empty".to_owned(),
        }
    }
}

impl CsvPosition {
    pub fn describe(&self) -> String {
        format!(
            "byte: {}, line: {}, record: {}",
            self.byte, self.line, self.record
        )
    }
}

impl CsvReadFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Io(value) => format!("CSV reading failed: {}", value.describe()),
            Self::Utf8(value) => value.describe(),
            Self::UnequalLengths(value) => value.describe(),
            Self::Seek => {
                "CSV reading failed because the reader was seeked before its headers were read"
                    .to_owned()
            }
            Self::Serialize(value) => format!("CSV serialization failed: {value}"),
            Self::Deserialize(value) => value.describe(),
            Self::Unclassified(value) => format!("CSV reading failed: {value}"),
        }
    }
}

impl CsvDeserializeCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Message(value) => value.clone(),
            Self::Unsupported(value) => format!("unsupported CSV conversion: {value}"),
            Self::UnexpectedEndOfRow => "unexpected end of row".to_owned(),
            Self::InvalidUtf8(value) => format!("invalid UTF-8: {}", value.describe()),
            Self::ParseBool => "cannot parse a boolean".to_owned(),
            Self::ParseInt(value) => format!(
                "cannot parse an integer: {}",
                <&str>::from(*value).to_owned()
            ),
            Self::ParseFloat => "cannot parse a Float".to_owned(),
        }
    }
}

impl RuntimeSigilFailureTilde {
    pub fn describe(&self) -> String {
        let suffix = relative_suffix(&self.suffix, PathRules::Windows);
        let rest = if suffix.is_empty() {
            ".".into()
        } else {
            quote_word(suffix)
        };
        format!(
            "'~' is not expanded in a grant; for a path under the home directory write {}",
            code(&format!("`home {rest}"))
        )
    }
}

impl RuntimeSigilFailureXdg {
    pub fn describe(&self) -> String {
        let suffix = relative_suffix(&self.suffix, PathRules::Windows);
        let rest = if suffix.is_empty() {
            ".".into()
        } else {
            quote_word(suffix)
        };
        format!(
            "{} is not expanded in a grant; write {}",
            code(&format!("`xdg:{}", <&str>::from(self.kind))),
            code(&format!("`{} {rest}", Anchor::Xdg(self.kind).tag()))
        )
    }
}

impl PathFailureNotUnicode {
    pub fn describe(&self) -> String {
        format!("{} is not UTF-8", self.shown)
    }
}

impl PathFailureNotWorkingDirectory {
    pub fn describe(&self) -> String {
        format!(
            "{} cannot be a working directory: it is not an absolute path{}",
            self.path,
            match self.rules {
                PathRules::Posix => "",
                PathRules::Windows => " on a drive or a share",
            }
        )
    }
}

impl PathFailureNoProcessCwd {
    pub fn describe(&self) -> String {
        format!(
            "cannot resolve {}: the process has no working directory",
            self.path
        )
    }
}

impl PathFailureGlob {
    pub fn describe(&self) -> String {
        format!("'{}': {}", self.pattern, self.cause.describe())
    }
}

impl GlobFailureHiddenOnly {
    pub fn describe(&self) -> String {
        format!(
            "a wildcard never matches a name starting with '.', and '{}' matches nothing else",
            self.component
        )
    }
}

impl GlobFailureSyntax {
    pub fn describe(&self) -> String {
        format!(
            "'{}' is not a valid pattern: {}",
            self.component, self.detail
        )
    }
}

impl GrantFailureUnknownVocabulary {
    pub fn describe(&self) -> String {
        let known = self
            .offered
            .iter()
            .map(|tag| format!("{TAG_PREFIX}{tag}"))
            .collect::<Vec<_>>();
        let nearest = near_names(
            &self.tag,
            known.iter().map(|tag| tag.trim_start_matches(TAG_PREFIX)),
            1,
        );
        let near = known
            .iter()
            .filter(|tag| nearest.contains(&tag.trim_start_matches(TAG_PREFIX)))
            .collect::<Vec<_>>();
        let suggestion = match near[..] {
            [near] if self.vocabulary.miss(&self.tag).is_none() => {
                format!(" (did you mean {}?)", code(near))
            }
            _ => String::new(),
        };
        let noun = indefinite(self.vocabulary.noun());
        format!(
            "{} is not {noun}; {noun} is one of {}{suggestion}",
            code(&format!("{TAG_PREFIX}{}", self.tag)),
            known
                .iter()
                .map(|tag| code(tag))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl GrantFailureRefusedKey {
    pub fn describe(&self) -> String {
        format!(
            "the {} field {:?} is not supported: {}",
            self.form.describe(),
            self.key,
            match self.cause {
                RefusedGrantKey::DetachMovedToShell => "detach is a shell verb, not a field",
                RefusedGrantKey::ManifestCapabilitiesUnenforced =>
                    "plugins run with host authority, so manifest capabilities are not enforced",
            }
        )
    }
}

impl GrantFailureUnwalkable {
    pub fn describe(&self) -> String {
        format!(
            "ral cannot resolve '{}', which the grant names: {}",
            self.place.payload,
            self.cause.describe()
        )
    }
}

impl GrantFailureGitPointer {
    pub fn describe(&self) -> String {
        let path = &self.path;
        let gitdir = code("`gitdir");
        match &self.cause {
            GitPointerFailure::Unclaimed(target) => format!(
                "the .git file at '{path}' points at '{target}', but that directory does not name this working tree back, so {gitdir} will not grant it; a pointer written inside the tree would otherwise choose the grant."
            ),
            GitPointerFailure::NoGitdirLine => format!(
                "the .git entry at '{path}' is a file with no `gitdir:` line, so there is no git directory for {gitdir} to name."
            ),
            GitPointerFailure::Unreadable(cause) => format!(
                "the .git file at '{path}' cannot be read ({}), so {gitdir} cannot say which git directory the grant covers.",
                cause.describe()
            ),
        }
    }
}

impl GrantFailureOutsideHome {
    pub fn describe(&self) -> String {
        let anchor = code(&format!("`{}", self.anchor.tag()));
        let var = match self.anchor {
            Anchor::Xdg(kind) => kind.env_var().into(),
            _ => self.anchor.tag(),
        };
        let clause = match &self.xdg {
            None => format!(
                "{var} is unset, so the default base was used. Is HOME ({:?}) set correctly?",
                self.home
            ),
            Some(value) => format!(
                "{var}={value:?}: set it to a subpath of an absolute HOME, unset it to use the default base, or replace {anchor} in the policy with an {} path.",
                code("`absolute")
            ),
        };
        let via = if self.written == self.resolved {
            String::new()
        } else {
            format!(" (written {:?})", self.written)
        };
        match self.classification {
            HomeClassification::Absolute => format!(
                "{anchor} resolves to {:?}{via}, outside HOME ({:?}); refusing to widen the grant.  {clause}",
                self.resolved, self.home
            ),
            HomeClassification::Relative => format!(
                "{anchor} resolves to {:?}{via}, but HOME ({:?}) is relative, so containment cannot be established; refusing to widen the grant. Set HOME to an absolute path.  {clause}",
                self.resolved, self.home
            ),
        }
    }
}

impl GrantFailureRelativeAnchor {
    pub fn describe(&self) -> String {
        format!(
            "{} freezes to the relative path '{}'; an anchor must be absolute. Is HOME or TMPDIR relative?",
            code(&format!("`{}", self.anchor.tag())),
            self.path
        )
    }
}

impl GrantFailureAbsolutePath {
    pub fn describe(&self) -> String {
        let word = quote_word(&self.raw);
        match self.cause {
            AbsolutePathRefusal::Malformed => format!(
                "{word} is no well-formed UNC or verbatim path; a UNC path needs a server and a share, as in '\\\\server\\share\\x'; a verbatim path is '\\\\?\\C:\\x' or '\\\\?\\UNC\\server\\share\\x'.  A '?' is no server name, '.' and '..' are neither a server nor a share name, and device paths are not accepted"
            ),
            AbsolutePathRefusal::MixedHead => format!(
                "{word} opens with two different separators, which Windows reads as a UNC or device path and POSIX does not"
            ),
            AbsolutePathRefusal::NtObject => format!("{word} is an NT object-manager path"),
            AbsolutePathRefusal::Driveless => format!("{word} names no drive"),
            AbsolutePathRefusal::DriveRelative => format!("{word} is drive-relative"),
            AbsolutePathRefusal::ForeignOrRelative => format!("{word} is not an absolute path"),
        }
    }

    pub fn advice(&self) -> Option<String> {
        let abs = |rest: &str| code(&format!("`absolute {rest}"));
        Some(match self.cause {
            AbsolutePathRefusal::Malformed => return None,
            AbsolutePathRefusal::MixedHead => {
                format!("write {} or {}", abs("'\\\\server\\share\\x'"), abs("/x"))
            }
            AbsolutePathRefusal::NtObject => {
                format!("no grant can name one; write {}", abs("'C:\\x'"))
            }
            AbsolutePathRefusal::Driveless => format!("write {}", abs("'C:\\x'")),
            AbsolutePathRefusal::DriveRelative => format!("write {}", abs("'C:\\…'")),
            AbsolutePathRefusal::ForeignOrRelative => format!(
                "{} needs an absolute path; for a path under the working directory write {}",
                code("`absolute"),
                code(&format!("`cwd {}", quote_word(&self.raw)))
            ),
        })
    }
}

impl GrantFailureSubpath {
    pub fn describe(&self) -> String {
        let tag = self.anchor.tag();
        let this = code(&format!("`{tag}"));
        let word = quote_word(&self.raw);
        let drive = code("`absolute 'C:\\…'");
        match self.cause {
            RelativeSubpathRefusal::Empty => format!(
                "{this} needs a path; write {} for the anchor itself",
                code(&format!("`{tag} ."))
            ),
            RelativeSubpathRefusal::Drive => {
                format!("{this} takes a relative path, not {word}; a drive belongs in {drive}")
            }
            RelativeSubpathRefusal::MixedHead => format!(
                "{this} takes a relative path, not {word}, which opens with two different separators"
            ),
            RelativeSubpathRefusal::Rooted => format!(
                "{this} takes a relative path, not {word}; for {word} write {}",
                code(&format!("`absolute {word}"))
            ),
            RelativeSubpathRefusal::NtObject => format!(
                "{this} takes a relative path, not {word}, an NT object-manager path, which no grant can name"
            ),
            RelativeSubpathRefusal::Driveless => format!(
                "{this} takes a relative path, not {word}, which names no drive; a drive belongs in {drive}"
            ),
        }
    }
}

impl GrantFailureWorldSelector {
    pub fn describe(&self) -> String {
        format!(
            "{} resolves to {}, so {} would run and read all of it.",
            self.requested,
            self.relation.describe(),
            code(&format!(
                "`dir `{selector}",
                selector = self.selector.name()
            ))
        )
    }
}

impl CodecFailureUtf8 {
    pub fn describe(&self) -> String {
        match &self.subject {
            Utf8Subject::Input => format!("input is not valid UTF-8: {}", self.cause.describe()),
            Utf8Subject::CapturedOutput => format!(
                "captured output is not valid UTF-8 text: {}",
                self.cause.describe()
            ),
            Utf8Subject::Path(path) => {
                format!("'{path}' is not valid UTF-8: {}", self.cause.describe())
            }
        }
    }
}

impl CodecFailureJsonPointer {
    pub fn describe(&self) -> String {
        let here = if self.at.is_empty() {
            "the document".into()
        } else {
            format!("`{}`", self.at)
        };
        let token = &self.token;
        match &self.cause {
            JsonPointerCause::MissingMember(keys) => format!(
                "{here} has no member `{token}`; its keys are {}",
                if keys.is_empty() {
                    "none".into()
                } else {
                    keys.iter()
                        .map(|key| format!("`{key}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
            JsonPointerCause::NoElement(count) => {
                format!(
                    "{here} has {}, none numbered `{token}`",
                    plural(count.to_u64(), "element")
                )
            }
            JsonPointerCause::Scalar(shape) => format!(
                "{here} is {}, which has nothing at `{token}`",
                match shape {
                    JsonShape::Null => "null",
                    JsonShape::Bool => "a boolean",
                    JsonShape::Number => "a number",
                    JsonShape::String => "text",
                    JsonShape::Array => "an array",
                    JsonShape::Object => "an object",
                }
            ),
        }
    }
}

impl CodecFailureJsonLine {
    pub fn describe(&self) -> String {
        format!("input line {}: {}", self.line, self.cause.describe())
    }
}

impl CodecFailureCsvWidth {
    pub fn describe(&self) -> String {
        let subject = self
            .line
            .map_or_else(|| "a CSV record".into(), |line| format!("line {line}"));
        format!(
            "{subject} has {}, but the header has {}",
            plural(self.got.to_u64(), "field"),
            plural(self.expected.to_u64(), "field")
        )
    }
}

impl CodecFailureCsvKeys {
    pub fn describe(&self) -> String {
        format!(
            "the row at index {} has the key {:?}, but the row at index {} does not",
            self.has_row, self.key, self.lacks_row
        )
    }
}

impl CodecFailureCsvCell {
    pub fn describe(&self) -> String {
        format!(
            "the cell `{}` of the row at index {} is {}; a CSV cell is text, a number or a boolean",
            self.key,
            self.row,
            self.found.describe()
        )
    }
}

impl CodecFailureMatchCount {
    pub fn describe(&self) -> String {
        format!(
            "{} pattern {:?} matches {}",
            self.kind.describe(),
            self.pattern,
            plural(self.matches.to_u64(), "time")
        )
    }
}

impl CsvReadFailureUtf8 {
    pub fn describe(&self) -> String {
        let position = self.position.as_ref().map_or_else(String::new, |position| {
            format!(" ({})", position.describe())
        });
        format!(
            "CSV field {} is not valid UTF-8 at byte offset {}{position}",
            self.field, self.valid_up_to
        )
    }
}

impl CsvReadFailureUnequalLengths {
    pub fn describe(&self) -> String {
        let subject = self.position.as_ref().map_or_else(
            || "a CSV record".into(),
            |position| format!("line {}", position.line),
        );
        let position = self.position.as_ref().map_or_else(String::new, |position| {
            format!(" (byte {}, record {})", position.byte, position.record)
        });
        format!(
            "{subject} has {}, but the header has {}{position}",
            plural(self.got.to_u64(), "field"),
            plural(self.expected.to_u64(), "field")
        )
    }
}

impl CsvReadFailureDeserialize {
    pub fn describe(&self) -> String {
        let field = self
            .field
            .map_or_else(String::new, |field| format!(" for field {field}"));
        let position = self.position.as_ref().map_or_else(String::new, |position| {
            format!(" ({})", position.describe())
        });
        format!(
            "CSV deserialization failed{field}{position}: {}",
            self.cause.describe()
        )
    }
}

fn nonnegative_index_range(range: &NumberRange) -> bool {
    matches!(
        range,
        NumberRange {
            minimum: Number::Int(0),
            maximum: Number::Int(i64::MAX),
            ..
        }
    )
}

fn number_description(number: Number) -> String {
    match number {
        Number::Int(n) => n.to_string(),
        Number::Float(n) => n.to_string(),
    }
}

impl AdmissionFailure {
    pub fn advice(&self) -> Option<AdmissionAdvice> {
        match &self.cause {
            AdmissionCause::Uses(uses) => {
                let numeric = match &uses.expected {
                    Expected::Shape(ValueShape::Int) => matches!(uses.found, Found::Float(_)),
                    Expected::Shape(ValueShape::Float) => matches!(uses.found, Found::Int(_)),
                    _ => false,
                };
                if numeric {
                    return Some(AdmissionAdvice::NumericConversion);
                }
                (matches!(
                    &uses.expected,
                    Expected::Shape(ValueShape::Variant) | Expected::Tags(_)
                ) && !matches!(uses.found, Found::Variant(_)))
                .then_some(AdmissionAdvice::DecoderHasNoVariants)
            }
            AdmissionCause::NoField(_)
            | AdmissionCause::ExtraField(_)
            | AdmissionCause::Shares(_)
            | AdmissionCause::Export(_) => None,
        }
    }
}

impl AdmissionAdvice {
    pub fn text(self) -> &'static str {
        match self {
            Self::NumericConversion => {
                "JSON has one number type, and ral reads a whole number as an Int and a number with a fraction as a Float; `float` or `int` at the use accepts the other"
            }
            Self::DecoderHasNoVariants => {
                "a decoder produces no variants: a tag is matched only on a value the program built, or one a host answered"
            }
        }
    }
}

impl InvalidFailure {
    pub fn new(operation: String, cause: InvalidCause) -> Self {
        Self {
            operation,
            path: Vec::new(),
            cause,
        }
    }

    pub fn at(mut self, step: DataStep) -> Self {
        self.path.insert(0, step);
        self
    }

    pub fn field(self, field: String) -> Self {
        self.at(DataStep::Field(field))
    }

    pub fn index(self, index: Count) -> Self {
        self.at(DataStep::Index(index))
    }

    pub fn payload(self, tag: String) -> Self {
        self.at(DataStep::Payload(tag))
    }

    fn byte_decoder_arity(&self) -> bool {
        matches!(
            self.operation.as_str(),
            "from-bytes"
                | "from-string"
                | "from-line"
                | "from-lines"
                | "from-json"
                | "from-jsonl"
                | "from-csv"
        ) && self.path.is_empty()
            && matches!(&self.cause, InvalidCause::Arity(ArityFailure { expected: CountRule::Exactly(n), got }) if n.to_u64() == 0 && got.to_u64() > 0)
    }

    fn echo_unit(&self) -> bool {
        self.operation == "echo"
            && matches!(self.path.as_slice(), [DataStep::Index(_)])
            && matches!(&self.cause, InvalidCause::Shape(ShapeFailure {
                expected: Expected::AnyOf(shapes), found: Found::Unit,
            }) if shapes.as_slice() == [ValueShape::Bool, ValueShape::Int, ValueShape::Float, ValueShape::String, ValueShape::Bytes, ValueShape::List, ValueShape::Map, ValueShape::Variant, ValueShape::Block, ValueShape::Function, ValueShape::Handle])
    }

    fn intrinsic_description(&self) -> String {
        if self.byte_decoder_arity() {
            return "takes no arguments; it reads the byte channel".into();
        }
        if self.echo_unit() {
            return "cannot print Unit".into();
        }
        match (self.operation.as_str(), self.path.as_slice(), &self.cause) {
            (
                "view-text" | "view-hash",
                [DataStep::Field(field)],
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Int),
                    found,
                }),
            ) if matches!(field.as_str(), "start" | "end") => {
                format!("{field} must be an Int >= 1, got {}", found.describe())
            }
            (
                "view-text" | "view-hash",
                [DataStep::Field(field)],
                InvalidCause::Range(NumberRange {
                    value: Number::Int(value),
                    minimum: Number::Int(1),
                    maximum: Number::Int(i64::MAX),
                }),
            ) if matches!(field.as_str(), "start" | "end") => {
                format!("{field} must be an Int >= 1, got {value}")
            }
            ("view-text" | "view-hash", [], InvalidCause::HalfOpenRange(range))
                if range.end.to_u64() <= range.start.to_u64() =>
            {
                format!(
                    "end must be greater than start (the range [start, end) is half-open), got start={}, end={}",
                    range.start, range.end
                )
            }
            (
                "edit-hash",
                [],
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::List),
                    found,
                }),
            ) => format!(
                "expected a List of [hash: …, line: …] records, got {}",
                found.describe()
            ),
            (
                "edit-hash",
                [DataStep::Index(_)],
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Record(fields),
                    found,
                }),
            ) if fields.as_slice() == ["hash", "line"] => format!(
                "each edit must be a [hash: …, line: …] record, got {}",
                found.describe()
            ),
            (
                "explore-dir",
                [DataStep::Field(field)],
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Int),
                    found,
                }),
            ) if field == "depth" => {
                format!("depth must be a non-negative Int, got {}", found.describe())
            }
            (
                "explore-dir",
                [DataStep::Field(field)],
                InvalidCause::Range(NumberRange {
                    value: Number::Int(value),
                    minimum: Number::Int(0),
                    maximum: Number::Int(i64::MAX),
                }),
            ) if field == "depth" => format!("depth must be non-negative, got {value}"),
            (
                "load-plugin",
                [DataStep::Field(_)],
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Map),
                    found,
                }),
            ) => format!(
                "the value under a plugin name is its options map; got {}",
                found.describe()
            ),
            ("_ed-state", [], InvalidCause::NonData(cause)) => format!(
                "the updater returned {}{}, but the plugin's persistent state cell holds only data",
                if cause.nested {
                    "a value containing "
                } else {
                    ""
                },
                indefinite(<&str>::from(cause.leaf)),
            ),
            ("_external-editor", [], InvalidCause::Path(PathFailure::NotUnicode(cause))) => {
                format!("the editor workspace path {} is not UTF-8", cause.shown)
            }
            (
                "map" | "lookup",
                _,
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::String),
                    found,
                }),
            ) => format!("map key must be a String, got {}", found.describe()),
            (
                "spread",
                _,
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(shape @ (ValueShape::List | ValueShape::Map)),
                    found,
                }),
            ) => format!(
                "spread requires a {}, got {}",
                if *shape == ValueShape::List {
                    "List"
                } else {
                    "Map"
                },
                found.describe()
            ),
            (
                "arithmetic",
                _,
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::AnyOf(shapes),
                    found,
                }),
            ) if shapes.as_slice() == [ValueShape::Int, ValueShape::Float] => format!(
                "expected Int or Float in arithmetic, got {}",
                found.describe()
            ),
            (
                "nth",
                [DataStep::Index(index)],
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Int),
                    found,
                }),
            ) if index.to_u64() == 0 => format!(
                "list index must be a non-negative Int, got {}",
                found.describe()
            ),
            ("nth", [DataStep::Index(index)], InvalidCause::Range(range))
                if index.to_u64() == 0 && nonnegative_index_range(range) =>
            {
                format!(
                    "list index must be a non-negative Int, got {}",
                    number_description(range.value)
                )
            }
            (
                "round",
                [DataStep::Index(index)],
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Int),
                    found,
                }),
            ) if index.to_u64() == 1 => format!("places must be an Int, got {}", found.describe()),
            (
                "round",
                [DataStep::Index(index)],
                InvalidCause::Range(NumberRange {
                    minimum: Number::Int(0),
                    maximum: Number::Int(maximum),
                    value: Number::Int(value),
                }),
            ) if index.to_u64() == 1 => {
                format!("places must be between 0 and {maximum}, got {value}")
            }
            ("run", [], InvalidCause::NonData(cause)) => {
                format!("a run can return only data: {}", cause.describe())
            }
            (_, [], InvalidCause::Codec(CodecFailure::MatchCount(cause))) => {
                match (self.operation.as_str(), cause.kind, cause.matches.to_u64()) {
                    ("re-replace", LiteralOrRegex::Regex, 0) => {
                        format!("pattern {:?} not found in input", cause.pattern)
                    }
                    ("re-replace", LiteralOrRegex::Regex, n) if n > 1 => format!(
                        "pattern {:?} matches {}; must match exactly once",
                        cause.pattern,
                        plural(n, "time")
                    ),
                    ("string-replace", LiteralOrRegex::Literal, 0) => {
                        format!("'from' {:?} not found in input", cause.pattern)
                    }
                    ("string-replace", LiteralOrRegex::Literal, n) if n > 1 => format!(
                        "'from' {:?} matches {}; must match exactly once",
                        cause.pattern,
                        plural(n, "time")
                    ),
                    ("re-find-match", LiteralOrRegex::Regex, 0) => {
                        format!("no match for pattern {:?}", cause.pattern)
                    }
                    _ => cause.describe(),
                }
            }
            _ => self.cause.describe(),
        }
    }

    fn shape_advice(&self, shape: &ShapeFailure) -> Option<String> {
        let advice = match (
            self.operation.as_str(),
            self.path.as_slice(),
            &shape.expected,
        ) {
            ("map" | "lookup", _, Expected::Shape(ValueShape::String)) => "use str to convert",
            ("spread", _, Expected::Shape(ValueShape::List)) => "spread (...) expands a list",
            ("spread", _, Expected::Shape(ValueShape::Map)) => {
                "spread (...) in a map expands key-value pairs"
            }
            ("pattern", _, Expected::Shape(ValueShape::List)) => "right-hand side must be a list",
            ("pattern", _, Expected::Shape(ValueShape::Map)) => "right-hand side must be a map",
            ("not", _, Expected::Shape(ValueShape::Bool)) => "use a comparison or explicit Bool",
            ("arithmetic", _, Expected::AnyOf(shapes))
                if shapes.as_slice() == [ValueShape::Int, ValueShape::Float] =>
            {
                "use int or float to convert"
            }
            (_, _, Expected::Shape(ValueShape::Bytes)) => {
                "a list of numbers is `ints-to-bytes`; a Bytes value comes from `from-bytes`"
            }
            (_, _, Expected::Shape(ValueShape::Handle)) => "use spawn to create a handle",
            (operation @ ("spawn" | "watch"), _, Expected::Closure) => {
                return Some(format!(
                    "{operation} requires a block: {operation} {{ ... }}"
                ));
            }
            ("nth", [DataStep::Index(index)], Expected::Shape(ValueShape::Int))
                if index.to_u64() == 0 =>
            {
                "list indices are zero-based integers"
            }
            ("ints-to-bytes", [DataStep::Index(_)], Expected::Shape(ValueShape::Int)) => {
                "bytes must be Int values in range 0..255"
            }
            ("round" | "floor" | "ceil" | "trunc", [], Expected::Shape(ValueShape::Float)) => {
                "e.g. round 3.7 0"
            }
            ("round", [DataStep::Index(index)], Expected::Shape(ValueShape::Int))
                if index.to_u64() == 1 =>
            {
                "e.g. round 3.14159 2"
            }
            ("int", [], Expected::AnyOf(shapes))
                if shapes.as_slice()
                    == [ValueShape::String, ValueShape::Int, ValueShape::Float] =>
            {
                "e.g. int '42'"
            }
            ("float", [], Expected::AnyOf(shapes))
                if shapes.as_slice()
                    == [ValueShape::String, ValueShape::Int, ValueShape::Float] =>
            {
                "e.g. float '3.14'"
            }
            ("fail", [], Expected::Shape(ValueShape::String)) => {
                "use `raise $e` to re-raise an error record"
            }
            _ => return None,
        };
        Some(advice.into())
    }

    fn match_count_advice(&self, cause: &CodecFailureMatchCount) -> Option<String> {
        let advice = match (self.operation.as_str(), cause.kind, cause.matches.to_u64()) {
            ("re-replace", LiteralOrRegex::Regex, 0) => {
                "is the file already updated, or did the pattern get whitespace-mangled?"
            }
            ("re-replace", LiteralOrRegex::Regex, n) if n > 1 => {
                "widen the pattern with surrounding context to disambiguate, or use re-replace-all to replace every match"
            }
            ("string-replace", LiteralOrRegex::Literal, 0) => {
                "did the pattern get whitespace-mangled?"
            }
            ("string-replace", LiteralOrRegex::Literal, n) if n > 1 => {
                "widen the pattern with surrounding context to disambiguate"
            }
            _ => return None,
        };
        Some(advice.into())
    }

    fn argv_refusal(&self) -> Option<(Count, &Found, RefusedArg)> {
        let [DataStep::Index(index)] = self.path.as_slice() else {
            return None;
        };
        let InvalidCause::Shape(ShapeFailure {
            expected: Expected::AnyOf(shapes),
            found,
        }) = &self.cause
        else {
            return None;
        };
        if shapes.as_slice()
            != [
                ValueShape::Bool,
                ValueShape::Int,
                ValueShape::Float,
                ValueShape::String,
                ValueShape::Variant,
            ]
        {
            return None;
        }
        let refused = match found {
            Found::Unit => RefusedArg::Unit,
            Found::List => RefusedArg::List,
            Found::Map => RefusedArg::Map,
            Found::Block | Found::Function => RefusedArg::Block,
            Found::Handle => RefusedArg::Handle,
            Found::Bytes => RefusedArg::Bytes,
            Found::Bool(_)
            | Found::Int(_)
            | Found::Float(_)
            | Found::Text(_)
            | Found::Variant(_) => return None,
        };
        Some((*index, found, refused))
    }

    pub fn advice(&self) -> Vec<String> {
        if let Some((_, _, refused)) = self.argv_refusal() {
            return vec![refused.remedy(&self.operation)];
        }
        if self.echo_unit() {
            return vec![RefusedArg::Unit.remedy("echo")];
        }
        if self.byte_decoder_arity() {
            let encoder = if self.operation == "from-bytes" {
                "to-bytes $bytes"
            } else {
                "to-string $text"
            };
            return vec![format!(
                "to decode a value in hand, pipe it through the matching encoder: `{encoder} | {}`",
                self.operation
            )];
        }
        if matches!(
            (&*self.operation, self.path.as_slice(), &self.cause),
            (
                "_external-editor",
                [],
                InvalidCause::Path(PathFailure::NotUnicode(_))
            )
        ) {
            return vec![
                "use a temporary directory whose path is valid UTF-8".into(),
                "the input buffer was not changed".into(),
            ];
        }
        let advice = match (&*self.operation, self.path.as_slice(), &self.cause) {
            ("edit-hash", [DataStep::Index(_)], InvalidCause::MissingField(field)) if field == "hash" => Some("set hash to a witness from view-hash/view-hash-around".into()),
            ("edit-hash", [DataStep::Index(_)], InvalidCause::MissingField(field)) if field == "line" => Some("set line to the replacement text".into()),
            ("exarch-agents" | "exarch-schedules" | "exarch-pins" | "exarch-context" | "exarch-transcript", [], InvalidCause::NonData(_)) => Some("the argument crosses to the host as plain data; pass no closures, handles, or environments".into()),
            ("view-text" | "view-hash" | "edit-hash" | "edit-replace", [], InvalidCause::Codec(CodecFailure::Utf8(CodecFailureUtf8 { subject: Utf8Subject::Path(_), .. }))) => Some("these tools read and edit text only".into()),
            ("skill", [], InvalidCause::Codec(CodecFailure::Utf8(CodecFailureUtf8 { subject: Utf8Subject::Path(_), .. }))) => Some("skills are loaded as UTF-8 text only".into()),
            ("load-plugin", [DataStep::Field(name)], InvalidCause::Shape(ShapeFailure { expected: Expected::Shape(ValueShape::Map), .. })) => Some(format!("Write `{name}: [:]` if it takes no options.")),
            ("glob", [], InvalidCause::Path(PathFailure::Glob(PathFailureGlob { cause: GlobFailure::HiddenOnly(_), .. }))) => Some("name each file literally, or filter `list-dir`".into()),
            ("spawn", [], InvalidCause::TitleRequired) => Some("spawn \"what it does\" { … }".into()),
            ("spawn" | "detach", [], InvalidCause::Empty | InvalidCause::Multiline(_)) => Some("say what the work is in a few words, on one line: \"build the docs\"".into()),
            ("run", [], InvalidCause::NonData(cause)) => Some(match (cause.leaf, cause.nested) {
                (OpaqueKind::Handle, false) => "did you mean to keep it? bind it (`let h = spawn \"title\" { … }`) and `await $h` when you need its value",
                (OpaqueKind::Block | OpaqueKind::Function, false) => "did you mean to run it? force it with `!{ … }`, or pass it its arguments",
                (OpaqueKind::Handle, true) => "bind the handle with `let h = …`, and return only data",
                (OpaqueKind::Block | OpaqueKind::Function, true) => "return only data: force a block with `!{ … }`, or bind it with `let`",
            }.into()),
            (_, _, InvalidCause::Shape(shape)) => self.shape_advice(shape),
            ("nth", [DataStep::Index(index)], InvalidCause::Range(range)) if index.to_u64() == 0 && nonnegative_index_range(range) => Some("list indices are zero-based integers".into()),
            ("ints-to-bytes", [DataStep::Index(_)], InvalidCause::Range(NumberRange { minimum: Number::Int(0), maximum: Number::Int(255), .. })) => Some("bytes must be Int values in range 0..255".into()),
            ("round", [DataStep::Index(index)], InvalidCause::Range(NumberRange { minimum: Number::Int(0), maximum: Number::Int(308), .. })) if index.to_u64() == 1 => Some("0 rounds to a whole number, 2 to hundredths".into()),
            ("int", [], InvalidCause::NonIntegral(_)) => Some("convert with floor, ceil or trunc, which return an Int".into()),
            ("int", [], InvalidCause::Number(NumberFailure { target: IntOrFloat::Int, why: NumberFault::Syntax | NumberFault::OutOfRange, .. })) => Some("expected a whole-number string, e.g. int '42'".into()),
            ("float", [], InvalidCause::Number(NumberFailure { target: IntOrFloat::Float, why: NumberFault::Syntax, .. })) => Some("expected a numeric string, e.g. float '3.14'".into()),
            ("fail", [], InvalidCause::Arity(ArityFailure { expected: CountRule::Exactly(n), got })) if n.to_u64() == 1 && got.to_u64() == 0 => Some("use `raise $e` to re-raise an error record".into()),
            (_, _, InvalidCause::Codec(CodecFailure::Utf8(cause))) => match &cause.subject {
                Utf8Subject::CapturedOutput => Some("to keep it as bytes, pipe it: `… | from-bytes`".into()),
                Utf8Subject::Input if self.operation == "fold-lines" => Some("from-lines decodes lossily".into()),
                Utf8Subject::Input => Some("use from-bytes to keep raw bytes".into()),
                Utf8Subject::Path(_) => None,
            },
            (_, [], InvalidCause::Codec(CodecFailure::MatchCount(cause))) => self.match_count_advice(cause),
            _ => None,
        };
        advice.map_or_else(|| self.cause.advice(), |advice| vec![advice])
    }
}

impl InvalidCause {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Admission(cause) => cause
                .advice()
                .map(|advice| advice.text().to_owned())
                .into_iter()
                .collect(),
            Self::ZeroStatus => vec!["to succeed, use `return`".into()],
            Self::Grant(cause) => cause.advice(),
            Self::Codec(cause) => cause.advice(),
            _ => Vec::new(),
        }
    }
}

impl GrantFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::UnknownVocabulary(value) => {
                value.vocabulary.miss(&value.tag).into_iter().collect()
            }
            Self::RefusedKey(value) => vec![value.cause.advice().to_owned()],
            Self::BareNameRequired(_) => vec!["for a path write `` `absolute /path `` or `` `cwd relative/path ``; for a directory of programs, `` `dir `absolute /path ``".into()],
            Self::ProgramIsDirectory(Target::File(place)) if place.anchor == Anchor::Absolute && place.payload == "/" => vec![
                "To run every program leave `exec` out; for every program beneath a directory write `dir` before the place.".into()
            ],
            Self::ProgramIsDirectory(Target::File(place)) => vec![format!(
                "did you mean {}?", code(&Target::Dir(place.clone()).to_string())
            )],
            Self::Unwalkable(_) => vec![
                "A grant is resolved when it loads, so every name it passes through must be one ral can follow: is a link in a loop, or a path component not a directory?".into()
            ],
            Self::GitPointer(pointer) => vec![match &pointer.cause {
                GitPointerFailure::Unclaimed(_) => format!(
                    "A linked worktree's git directory holds a `gitdir` file naming this very .git file, and a repository split off with `--separate-git-dir` holds `core.worktree` in its config; neither names this tree.  Was the worktree moved?  `git worktree repair` rewrites both ends.  If the pointer is hand-written, name the git directory explicitly in the policy instead of {}.", code("`gitdir")
                ),
                GitPointerFailure::NoGitdirLine => format!(
                    "A worktree's or submodule's `.git` file holds one line, `gitdir: <path>`.  Is this file something else?  If the tree is not a repository, drop {} from the policy or replace it with an explicit path.", code("`gitdir")
                ),
                GitPointerFailure::Unreadable(_) => format!("The freeze reads the pointer as the user launching the session.  Check the file's permissions, or replace {} in the policy with an explicit path.", code("`gitdir")),
            }],
            Self::AbsolutePath(value) => value.advice().into_iter().collect(),
            Self::RootExclusion(_) => vec![
                "To withhold every path, grant none: leave the fs `read` and `write` lists empty."
                    .into(),
            ],
            _ => Vec::new(),
        }
    }
}

impl CodecFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::CsvKeys(_) => vec!["CSV cannot leave a field out: give every row every key, as `union $blank $row` does where `$blank` maps each column to ''".into()],
            _ => Vec::new(),
        }
    }
}

// Authored places and targets retain the grant value's flattened tags.
impl crate::data::datum::Datum for Anchor {
    fn encode(self) -> crate::data::FOValue {
        crate::data::datum::tag(&self.tag(), None)
    }

    fn decode(value: &crate::data::FOValue) -> Result<Self, DecodeFailure> {
        let offered = Self::all()
            .map(|anchor| anchor.tag().into_owned())
            .collect::<Vec<_>>();
        let Some((tag, payload)) = crate::data::datum::untag(value) else {
            return Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::Tags(offered),
                found: Found::of(value),
            })));
        };
        let anchor = Self::of_tag(tag).ok_or_else(|| {
            DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
                tag: tag.into(),
                offered,
            }))
        })?;
        if let Some(payload) = payload {
            return Err(DecodeFailure::new(DecodeCause::UnexpectedPayload(
                UnexpectedPayload {
                    tag: tag.into(),
                    found: Found::of(payload),
                },
            )));
        }
        Ok(anchor)
    }
}

impl crate::data::datum::Datum for Place {
    fn encode(self) -> crate::data::FOValue {
        crate::data::datum::tag(&self.anchor.tag(), Some(self.payload.encode()))
    }

    fn decode(value: &crate::data::FOValue) -> Result<Self, DecodeFailure> {
        let offered = Anchor::all()
            .map(|anchor| anchor.tag().into_owned())
            .collect::<Vec<_>>();
        let Some((tag, payload)) = crate::data::datum::untag(value) else {
            return Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::Tags(offered),
                found: Found::of(value),
            })));
        };
        let anchor = Anchor::of_tag(tag).ok_or_else(|| {
            DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
                tag: tag.into(),
                offered,
            }))
        })?;
        let payload = String::decode(payload.unwrap_or(&crate::data::FOValue::Unit))
            .map_err(|cause| cause.payload(tag))?;
        Ok(Self { anchor, payload })
    }
}

impl crate::data::datum::Datum for Target {
    fn encode(self) -> crate::data::FOValue {
        match self {
            Self::Name(name) => crate::data::datum::tag("name", Some(name.encode())),
            Self::File(place) => place.encode(),
            Self::Dir(place) => crate::data::datum::tag("dir", Some(place.encode())),
        }
    }

    fn decode(value: &crate::data::FOValue) -> Result<Self, DecodeFailure> {
        let offered = std::iter::once("name".to_owned())
            .chain(Anchor::all().map(|anchor| anchor.tag().into_owned()))
            .chain(std::iter::once("dir".to_owned()))
            .collect::<Vec<_>>();
        let Some((tag, payload)) = crate::data::datum::untag(value) else {
            return Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::Tags(offered),
                found: Found::of(value),
            })));
        };
        match tag {
            "name" => String::decode(payload.unwrap_or(&crate::data::FOValue::Unit))
                .map(Self::Name)
                .map_err(|cause| cause.payload(tag)),
            "dir" => Place::decode(payload.unwrap_or(&crate::data::FOValue::Unit))
                .map(Self::Dir)
                .map_err(|cause| cause.payload(tag)),
            tag if Anchor::of_tag(tag).is_some() => Place::decode(value).map(Self::File),
            _ => Err(DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
                tag: tag.into(),
                offered,
            }))),
        }
    }
}

impl crate::data::datum::Datum for Selector {
    fn encode(self) -> crate::data::FOValue {
        crate::data::datum::tag(self.name(), None)
    }

    fn decode(value: &crate::data::FOValue) -> Result<Self, DecodeFailure> {
        let offered = Self::names().map(str::to_owned).collect::<Vec<_>>();
        let Some((tag, payload)) = crate::data::datum::untag(value) else {
            return Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::Tags(offered),
                found: Found::of(value),
            })));
        };
        let selector = tag.parse().map_err(|_| {
            DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
                tag: tag.into(),
                offered,
            }))
        })?;
        if let Some(payload) = payload {
            return Err(DecodeFailure::new(DecodeCause::UnexpectedPayload(
                UnexpectedPayload {
                    tag: tag.into(),
                    found: Found::of(payload),
                },
            )));
        }
        Ok(selector)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecFailureNotJson {
    pub leaf: OpaqueKind,
    pub nested: bool,
}
record!(CodecFailureNotJson {
    leaf: "leaf",
    nested: "nested"
});
impl CodecFailureNotJson {
    pub fn describe(&self) -> String {
        if self.nested {
            format!(
                "cannot encode a value containing {} as JSON",
                indefinite(<&str>::from(self.leaf))
            )
        } else {
            format!(
                "cannot encode {} as JSON",
                indefinite(<&str>::from(self.leaf))
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;

    fn assert_grant_projection(failure: &GrantFailure, description: &str, advice: &[&str]) {
        assert_eq!(failure.describe(), description);
        assert_eq!(failure.advice(), advice);
        let decoded = GrantFailure::decode(&failure.clone().encode()).unwrap();
        assert_eq!(&decoded, failure);
        assert_eq!(decoded.describe(), description);
        assert_eq!(decoded.advice(), advice);
        let invalid = InvalidFailure::new("grant".into(), InvalidCause::Grant(decoded));
        assert_eq!(invalid.advice(), advice);
    }

    #[test]
    fn grant_runtime_sigils_strip_windows_separators_on_every_host() {
        for (suffix, rest) in [("", "."), (r"\x", "x"), ("/x", "x")] {
            assert_grant_projection(
                &GrantFailure::RuntimeSigil(RuntimeSigilFailure::Tilde(RuntimeSigilFailureTilde {
                    suffix: suffix.into(),
                })),
                &format!(
                    "'~' is not expanded in a grant; for a path under the home directory write `` `home {rest} ``"
                ),
                &[],
            );
            assert_grant_projection(
                &GrantFailure::RuntimeSigil(RuntimeSigilFailure::Xdg(RuntimeSigilFailureXdg {
                    kind: XdgKind::Config,
                    suffix: suffix.into(),
                })),
                &format!(
                    "`` `xdg:config `` is not expanded in a grant; write `` `xdg-config {rest} ``"
                ),
                &[],
            );
        }
        assert_grant_projection(
            &GrantFailure::RuntimeSigil(RuntimeSigilFailure::UnknownXdg("xdg:nope".into())),
            "unknown xdg token 'xdg:nope'; the places are `` `xdg-config ``, `` `xdg-data ``, `` `xdg-cache ``, `` `xdg-state ``, `` `xdg-bin ``. Did you mean one of those?",
            &[],
        );
        assert_grant_projection(
            &GrantFailure::EmptyPathSelector,
            "`` `path `` expands to zero absolute directories; PATH is empty, unset, or contains only relative entries",
            &[],
        );
    }

    #[test]
    fn invalid_unknown_field_suggests_only_a_unique_nearest_offered_name() {
        for (field, offered, suggestion) in [
            ("alow", vec!["allow", "deny"], Some("allow")),
            ("alow", vec!["allow", "slow"], None),
            ("alow", vec!["allow", "allows"], Some("allow")),
            ("qqqqq", vec!["allow", "deny"], None),
            ("alow", vec!["allow", "allow"], Some("allow")),
            ("alow", vec![], None),
        ] {
            let failure = InvalidCause::UnknownField(UnknownField {
                field: field.into(),
                offered: offered.iter().map(|name| (*name).into()).collect(),
            });
            let expected = format!(
                "unknown field {field:?}; offered: {}{}",
                offered.join(", "),
                suggestion.map_or_else(String::new, |name| format!(" (did you mean '{name}'?)"))
            );
            assert_eq!(failure.describe(), expected);
            assert_eq!(failure.advice(), [] as [String; 0]);
            let decoded = InvalidCause::decode(&failure.clone().encode()).unwrap();
            assert_eq!(decoded, failure);
            assert_eq!(decoded.describe(), expected);
        }
    }

    #[test]
    fn argv_remedies_require_the_scalar_shape_contract_and_one_argument_index() {
        let failure = InvalidFailure {
            operation: "arbitrary-program".into(),
            path: vec![DataStep::Index(3usize.into())],
            cause: InvalidCause::Shape(ShapeFailure {
                expected: Expected::AnyOf(vec![
                    ValueShape::Bool,
                    ValueShape::Int,
                    ValueShape::Float,
                    ValueShape::String,
                    ValueShape::Variant,
                ]),
                found: Found::List,
            }),
        };
        assert_eq!(
            failure.advice(),
            [RefusedArg::List.remedy("arbitrary-program")]
        );
        assert_eq!(
            failure.describe(),
            "cannot pass a list as argument at index 3 to `arbitrary-program`"
        );
        let decoded = InvalidFailure::decode(&failure.clone().encode()).unwrap();
        assert_eq!(decoded.describe(), failure.describe());
        assert_eq!(decoded.advice(), failure.advice());
        for path in [
            vec![],
            vec![DataStep::Field("argv".into())],
            vec![
                DataStep::Index(3usize.into()),
                DataStep::Index(0usize.into()),
            ],
        ] {
            let other = InvalidFailure {
                path,
                ..failure.clone()
            };
            assert_eq!(other.advice(), [] as [String; 0]);
            assert!(!other.describe().starts_with("cannot pass"));
        }
        let other = InvalidFailure {
            cause: InvalidCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::String),
                found: Found::List,
            }),
            ..failure
        };
        assert_eq!(other.advice(), [] as [String; 0]);
        assert!(!other.describe().starts_with("cannot pass"));
    }
}

#[cfg(test)]
mod presentation_tests {
    use super::*;
    use crate::data::datum::Datum;

    fn projection(failure: &InvalidFailure, description: &str, advice: &[&str]) {
        assert_eq!(failure.describe(), description);
        assert_eq!(failure.advice(), advice);
        let decoded = InvalidFailure::decode(&failure.clone().encode()).unwrap();
        assert_eq!(&decoded, failure);
        assert_eq!(decoded.describe(), description);
        assert_eq!(decoded.advice(), advice);
    }

    #[test]
    fn ral_operation_projections_do_not_reclassify_neighboring_shapes_paths_or_operations() {
        for (operation, path, expected) in [
            ("other", vec![DataStep::Field("p".into())], ValueShape::Map),
            ("load-plugin", vec![], ValueShape::Map),
            (
                "load-plugin",
                vec![DataStep::Field("p".into()), DataStep::Field("x".into())],
                ValueShape::Map,
            ),
            (
                "load-plugin",
                vec![DataStep::Index(Count::from_u64(0))],
                ValueShape::Map,
            ),
            (
                "load-plugin",
                vec![DataStep::Field("p".into())],
                ValueShape::String,
            ),
        ] {
            let failure = shape(operation, Expected::Shape(expected), path);
            assert_eq!(failure.intrinsic_description(), failure.cause.describe());
            assert_eq!(failure.advice(), [] as [String; 0]);
        }
        for (operation, path, cause) in [
            (
                "other",
                vec![],
                InvalidCause::NonData(NonData {
                    leaf: OpaqueKind::Handle,
                    nested: true,
                }),
            ),
            (
                "_ed-state",
                vec![DataStep::Field("p".into())],
                InvalidCause::NonData(NonData {
                    leaf: OpaqueKind::Block,
                    nested: false,
                }),
            ),
            (
                "other",
                vec![],
                InvalidCause::Path(PathFailure::NotUnicode(PathFailureNotUnicode {
                    shown: "\"shown\"".into(),
                })),
            ),
            (
                "_external-editor",
                vec![DataStep::Field("p".into())],
                InvalidCause::Path(PathFailure::NotUnicode(PathFailureNotUnicode {
                    shown: "\"shown\"".into(),
                })),
            ),
        ] {
            let failure = InvalidFailure {
                operation: operation.into(),
                path,
                cause,
            };
            assert_eq!(failure.intrinsic_description(), failure.cause.describe());
            assert_eq!(failure.advice(), [] as [String; 0]);
        }
    }

    fn shape(operation: &str, expected: Expected, path: Vec<DataStep>) -> InvalidFailure {
        InvalidFailure {
            operation: operation.into(),
            path,
            cause: InvalidCause::Shape(ShapeFailure {
                expected,
                found: Found::Unit,
            }),
        }
    }

    #[test]
    fn indexed_numeric_remedies_do_not_leak_to_other_paths_or_ranges() {
        for (operation, index, description, advice) in [
            (
                "nth",
                0,
                "nth: element 0: list index must be a non-negative Int, got ()",
                "list indices are zero-based integers",
            ),
            (
                "round",
                1,
                "round: element 1: places must be an Int, got ()",
                "e.g. round 3.14159 2",
            ),
            (
                "ints-to-bytes",
                7,
                "ints-to-bytes: element 7: expected an integer, got ()",
                "bytes must be Int values in range 0..255",
            ),
        ] {
            projection(
                &shape(
                    operation,
                    Expected::Shape(ValueShape::Int),
                    vec![DataStep::Index(Count::from_u64(index))],
                ),
                description,
                &[advice],
            );
            assert_eq!(
                shape(operation, Expected::Shape(ValueShape::Int), vec![]).advice(),
                [] as [String; 0]
            );
            assert_eq!(
                shape(
                    operation,
                    Expected::Shape(ValueShape::Int),
                    vec![DataStep::Field("x".into())]
                )
                .advice(),
                [] as [String; 0]
            );
        }
        assert_eq!(
            shape(
                "nth",
                Expected::Shape(ValueShape::Int),
                vec![DataStep::Index(Count::from_u64(1))]
            )
            .advice(),
            [] as [String; 0]
        );
        assert_eq!(
            shape(
                "round",
                Expected::Shape(ValueShape::Int),
                vec![DataStep::Index(Count::from_u64(0))]
            )
            .advice(),
            [] as [String; 0]
        );
        for (operation, index, maximum, value, description, advice) in [
            (
                "nth",
                0,
                i64::MAX,
                -1,
                "nth: element 0: list index must be a non-negative Int, got -1",
                "list indices are zero-based integers",
            ),
            (
                "round",
                1,
                308,
                309,
                "round: element 1: places must be between 0 and 308, got 309",
                "0 rounds to a whole number, 2 to hundredths",
            ),
            (
                "ints-to-bytes",
                7,
                255,
                256,
                "ints-to-bytes: element 7: 256 is outside 0..=255",
                "bytes must be Int values in range 0..255",
            ),
        ] {
            let failure = InvalidFailure::new(
                operation.into(),
                InvalidCause::Range(NumberRange {
                    minimum: Number::Int(0),
                    maximum: Number::Int(maximum),
                    value: Number::Int(value),
                }),
            )
            .index(Count::from_u64(index));
            projection(&failure, description, &[advice]);
            let mut unrelated = failure.clone();
            unrelated.operation = "other".into();
            assert_eq!(unrelated.advice(), [] as [String; 0]);
            let mut wrong_path = failure;
            wrong_path.path = vec![DataStep::Field("x".into())];
            assert_eq!(wrong_path.advice(), [] as [String; 0]);
        }
        let custom = InvalidFailure::new(
            "round".into(),
            InvalidCause::Range(NumberRange {
                minimum: Number::Int(0),
                maximum: Number::Int(17),
                value: Number::Int(18),
            }),
        )
        .index(Count::from_u64(1));
        projection(
            &custom,
            "round: element 1: places must be between 0 and 17, got 18",
            &[],
        );
    }
}

#[cfg(test)]
mod tool_presentation_tests {
    use super::*;
    use crate::data::datum::Datum;
    use crate::error::Error;
    use crate::fact::Reason;

    fn failure(operation: &str, path: Vec<DataStep>, cause: InvalidCause) -> InvalidFailure {
        InvalidFailure {
            operation: operation.into(),
            path,
            cause,
        }
    }
    fn shape(expected: Expected) -> InvalidCause {
        InvalidCause::Shape(ShapeFailure {
            expected,
            found: Found::Unit,
        })
    }
    fn check(value: &InvalidFailure, intrinsic: &str, advice: &[&str]) {
        let back = InvalidFailure::decode(&value.clone().encode()).unwrap();
        assert_eq!(&back, value);
        for value in [value, &back] {
            assert_eq!(value.intrinsic_description(), intrinsic);
            assert_eq!(value.advice(), advice);
            let reason = Reason::Invalid(Box::new(value.clone()));
            assert_eq!(reason.advice(), advice);
            let error = Error::new(reason);
            assert_eq!(error.hints(), []);
            let mut expected = value.describe();
            for hint in advice {
                let _ = write!(expected, "\nhint: {hint}");
            }
            assert_eq!(error.plain(), expected);
        }
    }
    fn generic(value: &InvalidFailure) {
        check(value, &value.cause.describe(), &[]);
    }

    #[test]
    fn text_bounds_and_depth_guards_match_only_exact_shapes_paths_and_ranges() {
        for operation in ["view-text", "view-hash", "explore-dir"] {
            let (fields, minimum) = if operation == "explore-dir" {
                (vec!["depth"], 0)
            } else {
                (vec!["start", "end"], 1)
            };
            for field in fields {
                for cause in [
                    shape(Expected::Shape(ValueShape::Int)),
                    InvalidCause::Range(NumberRange {
                        value: Number::Int(-1),
                        minimum: Number::Int(minimum),
                        maximum: Number::Int(i64::MAX),
                    }),
                ] {
                    let expected = match (&cause, operation) {
                        (InvalidCause::Shape(_), "explore-dir") => {
                            "depth must be a non-negative Int, got ()".into()
                        }
                        (InvalidCause::Range(_), "explore-dir") => {
                            "depth must be non-negative, got -1".into()
                        }
                        (InvalidCause::Shape(_), _) => {
                            format!("{field} must be an Int >= 1, got ()")
                        }
                        _ => format!("{field} must be an Int >= 1, got -1"),
                    };
                    let value = failure(operation, vec![DataStep::Field(field.into())], cause);
                    check(&value, &expected, &[]);
                    generic(&failure(
                        &format!("{operation}-around"),
                        value.path.clone(),
                        value.cause.clone(),
                    ));
                    generic(&failure(
                        operation,
                        vec![DataStep::Field("other".into())],
                        value.cause.clone(),
                    ));
                    generic(&failure(
                        operation,
                        vec![
                            DataStep::Index(0usize.into()),
                            DataStep::Field(field.into()),
                        ],
                        value.cause.clone(),
                    ));
                }
                for cause in [
                    shape(Expected::Shape(ValueShape::String)),
                    InvalidCause::Range(NumberRange {
                        value: Number::Int(-1),
                        minimum: Number::Int(minimum),
                        maximum: Number::Int(42),
                    }),
                    InvalidCause::Range(NumberRange {
                        value: Number::Int(-1),
                        minimum: Number::Int(minimum + 1),
                        maximum: Number::Int(i64::MAX),
                    }),
                ] {
                    generic(&failure(
                        operation,
                        vec![DataStep::Field(field.into())],
                        cause,
                    ));
                }
            }
        }
        for operation in ["view-text", "view-hash"] {
            for (start, end) in [(2u64, 1u64), (u64::MAX, u64::MAX)] {
                let value = failure(
                    operation,
                    vec![],
                    InvalidCause::HalfOpenRange(HalfOpenRange {
                        start: start.into(),
                        end: end.into(),
                    }),
                );
                check(
                    &value,
                    &format!(
                        "end must be greater than start (the range [start, end) is half-open), got start={start}, end={end}"
                    ),
                    &[],
                );
                generic(&failure(
                    operation,
                    vec![DataStep::Field("end".into())],
                    value.cause.clone(),
                ));
                generic(&failure("view", vec![], value.cause.clone()));
            }
            generic(&failure(
                operation,
                vec![],
                InvalidCause::HalfOpenRange(HalfOpenRange {
                    start: 1u64.into(),
                    end: 2u64.into(),
                }),
            ));
        }
    }

    #[test]
    fn edit_records_and_missing_fields_keep_exact_index_paths() {
        let root = failure(
            "edit-hash",
            vec![],
            shape(Expected::Shape(ValueShape::List)),
        );
        check(
            &root,
            "expected a List of [hash: …, line: …] records, got ()",
            &[],
        );
        generic(&failure("edit-replace", vec![], root.cause.clone()));
        generic(&failure(
            "edit-hash",
            vec![DataStep::Index(0usize.into())],
            root.cause,
        ));
        let item = failure(
            "edit-hash",
            vec![DataStep::Index(u64::MAX.into())],
            shape(Expected::Record(vec!["hash".into(), "line".into()])),
        );
        check(
            &item,
            "each edit must be a [hash: …, line: …] record, got ()",
            &[],
        );
        for fields in [
            vec!["line".into(), "hash".into()],
            vec!["hash".into()],
            vec!["hash".into(), "line".into(), "other".into()],
        ] {
            generic(&failure(
                "edit-hash",
                item.path.clone(),
                shape(Expected::Record(fields)),
            ));
        }
        generic(&failure("edit-hash", vec![], item.cause.clone()));
        generic(&failure("edit", item.path.clone(), item.cause.clone()));
        for (field, advice) in [
            (
                "hash",
                "set hash to a witness from view-hash/view-hash-around",
            ),
            ("line", "set line to the replacement text"),
        ] {
            let value = failure(
                "edit-hash",
                item.path.clone(),
                InvalidCause::MissingField(field.into()),
            );
            check(&value, &value.cause.describe(), &[advice]);
            generic(&failure("edit-hash", vec![], value.cause.clone()));
            generic(&failure(
                "edit-replace",
                item.path.clone(),
                value.cause.clone(),
            ));
            let nested = vec![
                DataStep::Index(0usize.into()),
                DataStep::Field(field.into()),
            ];
            generic(&failure("edit-hash", nested.clone(), value.cause.clone()));
            generic(&failure(
                "edit-hash",
                nested,
                shape(Expected::Shape(ValueShape::String)),
            ));
        }
        generic(&failure(
            "edit-hash",
            item.path,
            InvalidCause::MissingField("other".into()),
        ));
    }

    #[test]
    fn host_crossing_and_text_only_advice_preserve_negative_subjects_and_paths() {
        for operation in [
            "exarch-agents",
            "exarch-schedules",
            "exarch-pins",
            "exarch-context",
            "exarch-transcript",
        ] {
            for leaf in [OpaqueKind::Block, OpaqueKind::Function, OpaqueKind::Handle] {
                for nested in [false, true] {
                    let value = failure(
                        operation,
                        vec![],
                        InvalidCause::NonData(NonData { leaf, nested }),
                    );
                    check(
                        &value,
                        &value.cause.describe(),
                        &[
                            "the argument crosses to the host as plain data; pass no closures, handles, or environments",
                        ],
                    );
                    generic(&failure(
                        operation,
                        vec![DataStep::Field("arg".into())],
                        value.cause.clone(),
                    ));
                    generic(&failure("exarch-other", vec![], value.cause.clone()));
                }
            }
        }
        for operation in [
            "view-text",
            "view-hash",
            "edit-hash",
            "edit-replace",
            "skill",
        ] {
            let cause = InvalidCause::Codec(CodecFailure::Utf8(CodecFailureUtf8 {
                subject: Utf8Subject::Path("dir/SKILL.md".into()),
                cause: Utf8Failure {
                    valid_up_to: u64::MAX.into(),
                    error_len: Some(1u64.into()),
                },
            }));
            let value = failure(operation, vec![], cause);
            check(
                &value,
                &value.cause.describe(),
                &[if operation == "skill" {
                    "skills are loaded as UTF-8 text only"
                } else {
                    "these tools read and edit text only"
                }],
            );
            generic(&failure(
                &format!("{operation}-other"),
                vec![],
                value.cause.clone(),
            ));
            generic(&failure(
                operation,
                vec![DataStep::Index(0usize.into())],
                value.cause.clone(),
            ));
            for (subject, advice) in [
                (Utf8Subject::Input, "use from-bytes to keep raw bytes"),
                (
                    Utf8Subject::CapturedOutput,
                    "to keep it as bytes, pipe it: `… | from-bytes`",
                ),
            ] {
                let value = failure(
                    operation,
                    vec![],
                    InvalidCause::Codec(CodecFailure::Utf8(CodecFailureUtf8 {
                        subject,
                        cause: Utf8Failure {
                            valid_up_to: 0usize.into(),
                            error_len: None,
                        },
                    })),
                );
                check(&value, &value.cause.describe(), &[advice]);
            }
        }
    }
}
