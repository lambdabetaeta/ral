//! Portable host failure facts.

mod response;

use super::decode::{DecodeFailure, JsonInputFailure};
use super::io::{IoCause, IoKind};
use super::protocol::ConnectionFailure;
use super::shared::{
    AgentNameFailure, Count, CronFailure, DurationFact, DurationFailure, InvertedTurnSpan,
    JsonSyntaxFailure, RegexCause, Utf8Failure,
};
pub use crate::path::Kind as EntryKind;
use crate::process::CancelCause;
use crate::{label, record, variant};
use std::fmt::Write as _;
use strum::{IntoStaticStr, VariantArray};

pub type RefreshPayloadFailure = JsonInputFailure;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostFailure {
    Ral(RalFailure),
    Exarch(ExarchFailure),
}
variant!(HostFailure { Ral(RalFailure): "ral", Exarch(ExarchFailure): "exarch" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RalFailure {
    Boot(BootFailure),
    Editor(EditorFailure),
    Plugin(PluginFailure),
    Keybinding(KeybindingFailure),
}
variant!(RalFailure { Boot(BootFailure): "boot", Editor(EditorFailure): "editor", Plugin(PluginFailure): "plugin", Keybinding(KeybindingFailure): "keybinding" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootFailure {
    AlreadyBooted,
    ProfileNotAbsolute(BootFailureProfileNotAbsolute),
}
variant!(BootFailure { AlreadyBooted: "already-booted", ProfileNotAbsolute(BootFailureProfileNotAbsolute): "profile-not-absolute" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorFailure {
    NoPluginContext,
    UnknownStyle(EditorFailureUnknownStyle),
    Command(EditorFailureCommand),
    Draft(EditorFailureDraft),
    Workspace(EditorFailureWorkspace),
}
variant!(EditorFailure { NoPluginContext: "no-plugin-context", UnknownStyle(EditorFailureUnknownStyle): "unknown-style", Command(EditorFailureCommand): "command", Draft(EditorFailureDraft): "draft", Workspace(EditorFailureWorkspace): "workspace" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum EditorVariable {
    Visual,
    Editor,
}
label!(EditorVariable);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum EditorCommandCause {
    Nul,
    Unbalanced,
    NoProgram,
}
label!(EditorCommandCause);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum DraftOperation {
    Write,
    Read,
}
label!(DraftOperation);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftFailure {
    TooLarge(DraftFailureTooLarge),
    Read(IoCause),
    Utf8(Utf8Failure),
}
variant!(DraftFailure { TooLarge(DraftFailureTooLarge): "too-large", Read(IoCause): "read", Utf8(Utf8Failure): "utf8" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum EditorWorkspaceStep {
    TemporaryDirectory,
    CreateDirectory,
    CreateBuffer,
    WriteBuffer,
    BufferPath,
    LocateBuffer,
    InspectBuffer,
    OpenBuffer,
    RemoveWorkspace,
}
label!(EditorWorkspaceStep);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorWorkspaceFailure {
    Io(IoCause),
    ExpectedRegularFile(EditorWorkspaceFailureExpectedRegularFile),
}
variant!(EditorWorkspaceFailure { Io(IoCause): "io", ExpectedRegularFile(EditorWorkspaceFailureExpectedRegularFile): "expected-regular-file" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginFailure {
    AlreadyLoaded(PluginFailureAlreadyLoaded),
    NotFound(PluginFailureNotFound),
    OptionsWithoutFactory(PluginFailureOptionsWithoutFactory),
    FactoryReturnedBlock(PluginFailureFactoryReturnedBlock),
    UnknownHookEvent(PluginFailureUnknownHookEvent),
    HandlerInstallationEscaped(PluginFailureHandlerInstallationEscaped),
}
variant!(PluginFailure { AlreadyLoaded(PluginFailureAlreadyLoaded): "already-loaded", NotFound(PluginFailureNotFound): "not-found", OptionsWithoutFactory(PluginFailureOptionsWithoutFactory): "options-without-factory", FactoryReturnedBlock(PluginFailureFactoryReturnedBlock): "factory-returned-block", UnknownHookEvent(PluginFailureUnknownHookEvent): "unknown-hook-event", HandlerInstallationEscaped(PluginFailureHandlerInstallationEscaped): "handler-installation-escaped" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeybindingFailure {
    Notation(KeybindingFailureNotation),
    ReservedAction(KeybindingFailureReservedAction),
    SequencePrefix(KeybindingFailureSequencePrefix),
    GuardRegex(KeybindingFailureGuardRegex),
    UnguardedBuiltinShadow(KeybindingFailureUnguardedBuiltinShadow),
}
variant!(KeybindingFailure { Notation(KeybindingFailureNotation): "notation", ReservedAction(KeybindingFailureReservedAction): "reserved-action", SequencePrefix(KeybindingFailureSequencePrefix): "sequence-prefix", GuardRegex(KeybindingFailureGuardRegex): "guard-regex", UnguardedBuiltinShadow(KeybindingFailureUnguardedBuiltinShadow): "unguarded-builtin-shadow" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyNotationFailure {
    Empty,
    DuplicateModifier(KeyNotationFailureDuplicateModifier),
    UnknownKey(KeyNotationFailureUnknownKey),
    ControlCharacter(KeyNotationFailureControlCharacter),
    ShiftedCharacter(KeyNotationFailureShiftedCharacter),
    UppercaseControl(KeyNotationFailureUppercaseControl),
    TerminalAlias(KeyNotationFailureTerminalAlias),
}
variant!(KeyNotationFailure { Empty: "empty", DuplicateModifier(KeyNotationFailureDuplicateModifier): "duplicate-modifier", UnknownKey(KeyNotationFailureUnknownKey): "unknown-key", ControlCharacter(KeyNotationFailureControlCharacter): "control-character", ShiftedCharacter(KeyNotationFailureShiftedCharacter): "shifted-character", UppercaseControl(KeyNotationFailureUppercaseControl): "uppercase-control", TerminalAlias(KeyNotationFailureTerminalAlias): "terminal-alias" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum KeyModifier {
    Ctrl,
    Alt,
    Shift,
}
label!(KeyModifier);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ReservedKeyAction {
    Interrupt,
    EndOfFile,
}
label!(ReservedKeyAction);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum TerminalKeyAlias {
    Backspace,
    Tab,
    Enter,
    Escape,
}
label!(TerminalKeyAlias);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum BuiltinKeyAction {
    TextInsertion,
    Completion,
    AcceptLine,
    KeymapEscape,
    HistoryNavigation,
    CursorMovement,
    Deletion,
}
label!(BuiltinKeyAction);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExarchFailure {
    Policy(PolicyFailure),
    Agent(AgentFailure),
    Engine(EngineFailure),
    Recording(RecordingFailure),
    Context(ContextFailure),
    Schedule(ScheduleFailure),
    Edit(EditFailure),
    Skill(SkillFailure),
}
variant!(ExarchFailure { Policy(PolicyFailure): "policy", Agent(AgentFailure): "agent", Engine(EngineFailure): "engine", Recording(RecordingFailure): "recording", Context(ContextFailure): "context", Schedule(ScheduleFailure): "schedule", Edit(EditFailure): "edit", Skill(SkillFailure): "skill" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyFailure {
    OwnBinaryUnpinned,
    UnknownBase(PolicyFailureUnknownBase),
    EscapedLoad(PolicyFailureEscapedLoad),
}
variant!(PolicyFailure { OwnBinaryUnpinned: "own-binary-unpinned", UnknownBase(PolicyFailureUnknownBase): "unknown-base", EscapedLoad(PolicyFailureEscapedLoad): "escaped-load" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscapedLoad {
    Exit(u8),
    Stopped(CancelCause),
}
variant!(EscapedLoad { Exit(u8): "exit", Stopped(CancelCause): "stopped" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentFailure {
    NoFuel,
    NameMalformed(AgentNameFailure),
    NameTaken(AgentFailureNameTaken),
    UnknownProviderRole(AgentFailureUnknownProviderRole),
    NotDescendant(AgentFailureNotDescendant),
    NotOwned(AgentFailureNotOwned),
    SelfMessage(AgentFailureSelfMessage),
    NoLiveAgent(AgentFailureNoLiveAgent),
    Ended(AgentFailureEnded),
    Resting(AgentFailureResting),
    NoReturnRecipient,
    ReplyPending(AgentFailureReplyPending),
    ReplyAbsent(AgentFailureReplyAbsent),
    ReplyStopped(AgentFailureReplyStopped),
    ReplyFailed(AgentFailureReplyFailed),
    ReplyUnavailable(AgentFailureReplyUnavailable),
    QueuedUnsynced(AgentFailureQueuedUnsynced),
}
variant!(AgentFailure { NoFuel: "no-fuel", NameMalformed(AgentNameFailure): "name-malformed", NameTaken(AgentFailureNameTaken): "name-taken", UnknownProviderRole(AgentFailureUnknownProviderRole): "unknown-provider-role", NotDescendant(AgentFailureNotDescendant): "not-descendant", NotOwned(AgentFailureNotOwned): "not-owned", SelfMessage(AgentFailureSelfMessage): "self-message", NoLiveAgent(AgentFailureNoLiveAgent): "no-live-agent", Ended(AgentFailureEnded): "ended", Resting(AgentFailureResting): "resting", NoReturnRecipient: "no-return-recipient", ReplyPending(AgentFailureReplyPending): "reply-pending", ReplyAbsent(AgentFailureReplyAbsent): "reply-absent", ReplyStopped(AgentFailureReplyStopped): "reply-stopped", ReplyFailed(AgentFailureReplyFailed): "reply-failed", ReplyUnavailable(AgentFailureReplyUnavailable): "reply-unavailable", QueuedUnsynced(AgentFailureQueuedUnsynced): "queued-unsynced" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFailure {
    pub step: EngineStep,
    pub cause: EngineCause,
}
record!(EngineFailure {
    step: "step",
    cause: "cause"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum EngineStep {
    Factory,
    Adopt,
    Attach,
    Starting,
}
label!(EngineStep);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCause {
    Factory(EngineFactoryFailure),
    Io(IoCause),
    Connection(ConnectionFailure),
}
variant!(EngineCause { Factory(EngineFactoryFailure): "factory", Io(IoCause): "io", Connection(ConnectionFailure): "connection" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingFailure {
    Recording(RecordingFailureRecording),
    CellPoisoned(RecordingFailureCellPoisoned),
}
variant!(RecordingFailure { Recording(RecordingFailureRecording): "recording", CellPoisoned(RecordingFailureCellPoisoned): "cell-poisoned" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum RecordingStage {
    Append,
    Sync,
}
label!(RecordingStage);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSpan {
    pub from: Count,
    pub to: Count,
}
record!(TurnSpan {
    from: "from",
    to: "to"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextFailure {
    EmptySelection,
    InvertedSpan(InvertedTurnSpan),
    TurnNotRecorded(ContextFailureTurnNotRecorded),
    Withdrawn(ContextFailureWithdrawn),
    CurrentlyWritten(ContextFailureCurrentlyWritten),
    StraddledEviction(ContextFailureStraddledEviction),
    ReadBack(ContextFailureReadBack),
    Mutation(ContextFailureMutation),
    SteeringWithoutTurn,
    AssistantRole(ContextFailureAssistantRole),
    ToolResultCount(ContextFailureToolResultCount),
    UnknownToolResult(ContextFailureUnknownToolResult),
    DuplicateToolResult(ContextFailureDuplicateToolResult),
}
variant!(ContextFailure { EmptySelection: "empty-selection", InvertedSpan(InvertedTurnSpan): "inverted-span", TurnNotRecorded(ContextFailureTurnNotRecorded): "turn-not-recorded", Withdrawn(ContextFailureWithdrawn): "withdrawn", CurrentlyWritten(ContextFailureCurrentlyWritten): "currently-written", StraddledEviction(ContextFailureStraddledEviction): "straddled-eviction", ReadBack(ContextFailureReadBack): "read-back", Mutation(ContextFailureMutation): "mutation", SteeringWithoutTurn: "steering-without-turn", AssistantRole(ContextFailureAssistantRole): "assistant-role", ToolResultCount(ContextFailureToolResultCount): "tool-result-count", UnknownToolResult(ContextFailureUnknownToolResult): "unknown-tool-result", DuplicateToolResult(ContextFailureDuplicateToolResult): "duplicate-tool-result" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ContextOperation {
    RenderRequest,
    Import,
    AcceptUser,
    AcceptSteering,
    AppendAssistant,
    AppendToolResults,
    Clear,
    Rewind,
}
label!(ContextOperation);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextState {
    ReadyForUser,
    AwaitingAssistantAfterUser,
    AwaitingToolResults(ContextStateAwaitingToolResults),
    AwaitingAssistantAfterToolResults,
}
variant!(ContextState { ReadyForUser: "ready-for-user", AwaitingAssistantAfterUser: "awaiting-assistant-after-user", AwaitingToolResults(ContextStateAwaitingToolResults): "awaiting-tool-results", AwaitingAssistantAfterToolResults: "awaiting-assistant-after-tool-results" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleFailure {
    NoGrant(ScheduleFailureNoGrant),
    Cron(CronFailure),
    Duration(DurationFailure),
    NoFutureOccurrence(ScheduleFailureNoFutureOccurrence),
    DuplicateLiveLabel(ScheduleFailureDuplicateLiveLabel),
    Recording(ScheduleFailureRecording),
}
variant!(ScheduleFailure { NoGrant(ScheduleFailureNoGrant): "no-grant", Cron(CronFailure): "cron", Duration(DurationFailure): "duration", NoFutureOccurrence(ScheduleFailureNoFutureOccurrence): "no-future-occurrence", DuplicateLiveLabel(ScheduleFailureDuplicateLiveLabel): "duplicate-live-label", Recording(ScheduleFailureRecording): "recording" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditFailure {
    NoEdits,
    HashAbsent(EditFailureHashAbsent),
    HashAmbiguous(EditFailureHashAmbiguous),
    DuplicateTargetLine(EditFailureDuplicateTargetLine),
    EmptyFrom,
    MultipleLiteralMatches(EditFailureMultipleLiteralMatches),
    NoLiteralMatch(EditFailureNoLiteralMatch),
}
variant!(EditFailure { NoEdits: "no-edits", HashAbsent(EditFailureHashAbsent): "hash-absent", HashAmbiguous(EditFailureHashAmbiguous): "hash-ambiguous", DuplicateTargetLine(EditFailureDuplicateTargetLine): "duplicate-target-line", EmptyFrom: "empty-from", MultipleLiteralMatches(EditFailureMultipleLiteralMatches): "multiple-literal-matches", NoLiteralMatch(EditFailureNoLiteralMatch): "no-literal-match" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiteralMiss {
    Escapes,
    Indentation(Count),
    Absent,
}
variant!(LiteralMiss { Escapes: "escapes", Indentation(Count): "indentation", Absent: "absent" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillFailure {
    InvalidName(SkillFailureInvalidName),
    Missing(SkillFailureMissing),
    Read(SkillFailureRead),
    Yaml(SkillFailureYaml),
    Frontmatter(SkillFailureFrontmatter),
}
variant!(SkillFailure { InvalidName(SkillFailureInvalidName): "invalid-name", Missing(SkillFailureMissing): "missing", Read(SkillFailureRead): "read", Yaml(SkillFailureYaml): "yaml", Frontmatter(SkillFailureFrontmatter): "frontmatter" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillPlace {
    Disk(String),
    Builtin,
}
variant!(SkillPlace { Disk(String): "disk", Builtin: "builtin" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterFailure {
    Missing,
    NotMapping(FrontmatterFailureNotMapping),
    MissingField(SkillField),
    FieldShape(FrontmatterFailureFieldShape),
    InvalidName(FrontmatterFailureInvalidName),
    DirectoryNameMismatch(FrontmatterFailureDirectoryNameMismatch),
}
variant!(FrontmatterFailure { Missing: "missing", NotMapping(FrontmatterFailureNotMapping): "not-mapping", MissingField(SkillField): "missing-field", FieldShape(FrontmatterFailureFieldShape): "field-shape", InvalidName(FrontmatterFailureInvalidName): "invalid-name", DirectoryNameMismatch(FrontmatterFailureDirectoryNameMismatch): "directory-name-mismatch" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum SkillField {
    Name,
    Description,
}
label!(SkillField);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum PodKind {
    Null,
    String,
    Integer,
    Float,
    Boolean,
    Array,
    Hash,
}
label!(PodKind);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineFactoryFailure {
    Unsupported(EngineFactoryFailureUnsupported),
    MachineThreadStopped,
    MissingSocketDevice,
    NonVirtioSocketDevice,
    CompletionWithoutResult,
    DialTimedOut(EngineFactoryFailureDialTimedOut),
    DialWaitDisconnected(EngineFactoryFailureDialWaitDisconnected),
    Platform(EngineFactoryFailurePlatform),
    Io(IoCause),
}
variant!(EngineFactoryFailure { Unsupported(EngineFactoryFailureUnsupported): "unsupported", MachineThreadStopped: "machine-thread-stopped", MissingSocketDevice: "missing-socket-device", NonVirtioSocketDevice: "non-virtio-socket-device", CompletionWithoutResult: "completion-without-result", DialTimedOut(EngineFactoryFailureDialTimedOut): "dial-timed-out", DialWaitDisconnected(EngineFactoryFailureDialWaitDisconnected): "dial-wait-disconnected", Platform(EngineFactoryFailurePlatform): "platform", Io(IoCause): "io" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum FactoryBackend {
    Broker,
    HyperV,
    Virtualization,
}
label!(FactoryBackend);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordReadFailure {
    Io(IoCause),
    NonTranscriptRecord,
    MissingLocusRecord(RecordReadFailureMissingLocusRecord),
    LocusSealMismatch(RecordReadFailureLocusSealMismatch),
    Line(RecordReadFailureLine),
}
variant!(RecordReadFailure { Io(IoCause): "io", NonTranscriptRecord: "non-transcript-record", MissingLocusRecord(RecordReadFailureMissingLocusRecord): "missing-locus-record", LocusSealMismatch(RecordReadFailureLocusSealMismatch): "locus-seal-mismatch", Line(RecordReadFailureLine): "line" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordLineFailure {
    JsonSyntax(JsonSyntaxFailure),
    InvalidEnvelope(DecodeFailure),
    SealMismatch(RecordLineFailureSealMismatch),
    InvalidRecords(DecodeFailure),
}
variant!(RecordLineFailure { JsonSyntax(JsonSyntaxFailure): "json-syntax", InvalidEnvelope(DecodeFailure): "invalid-envelope", SealMismatch(RecordLineFailureSealMismatch): "seal-mismatch", InvalidRecords(DecodeFailure): "invalid-records" });

/// Context is outermost first; no link contains an enclosing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub context: Vec<ProviderContext>,
    pub leaf: ProviderLeaf,
}
record!(ProviderFailure {
    context: "context",
    leaf: "leaf"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ProviderContext {
    Truncated,
}
label!(ProviderContext);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderLeaf {
    Unavailable(RecordingFailure),
    Cancelled(ProviderCancelSite),
    Transient(ProviderLeafTransient),
    Refused(ProviderRefusal),
    Api(ProviderLeafApi),
    Overflow(ProviderLeafOverflow),
    OutputCap(ProviderLeafOutputCap),
    Terminal(ServiceFailure),
    Credential(CredentialFailure),
}
variant!(ProviderLeaf { Unavailable(RecordingFailure): "unavailable", Cancelled(ProviderCancelSite): "cancelled", Transient(ProviderLeafTransient): "transient", Refused(ProviderRefusal): "refused", Api(ProviderLeafApi): "api", Overflow(ProviderLeafOverflow): "overflow", OutputCap(ProviderLeafOutputCap): "output-cap", Terminal(ServiceFailure): "terminal", Credential(CredentialFailure): "credential" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ProviderCancelSite {
    BeforeRequest,
    MidStream,
    Backoff,
}
label!(ProviderCancelSite);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryableProviderCause {
    Service(ServiceFailure),
    StreamEndedWithoutEnd(RetryableProviderCauseStreamEndedWithoutEnd),
}
variant!(RetryableProviderCause { Service(ServiceFailure): "service", StreamEndedWithoutEnd(RetryableProviderCauseStreamEndedWithoutEnd): "stream-ended-without-end" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRefusal {
    pub limit: ProviderLimit,
    pub resets_at: Option<ProviderInstant>,
    pub received: ProviderInstant,
    pub cause: ProviderRefusalCause,
}
record!(ProviderRefusal {
    limit: "limit",
    resets_at: "resets-at",
    received: "received",
    cause: "cause"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ProviderLimit {
    Rate,
    Allowance,
}
label!(ProviderLimit);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderRefusalCause {
    Service(ServiceFailure),
    Held(ProviderRefusalCauseHeld),
}
variant!(ProviderRefusalCause { Service(ServiceFailure): "service", Held(ProviderRefusalCauseHeld): "held" });

/// Numeric meaning is seconds × `1_000_000_000` + `subsec_nanoseconds`, in i128.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInstant {
    pub seconds: i64,
    pub subsec_nanoseconds: i32,
}
record!(ProviderInstant {
    seconds: "seconds",
    subsec_nanoseconds: "subsec-nanoseconds"
});

/// Context is outermost first; the service owns the operation name and cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceFailure {
    pub service: String,
    pub operation: ServiceOperation,
    pub context: Vec<ServiceContext>,
    pub cause: Box<ServiceCause>,
}
record!(ServiceFailure {
    service: "service",
    operation: "operation",
    context: "context",
    cause: "cause"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ServiceOperation {
    Chat,
    BuildRefreshClient,
    RefreshRequest,
    RefreshResponse,
}
label!(ServiceOperation);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceContext {
    GenaiWebAdapterCall(ProviderAdapter),
    GenaiWebModelCall(ProviderModel),
    GenaiWebStream(ProviderModel),
    UnknownWebStreamSource(ServiceContextUnknownWebStreamSource),
}
variant!(ServiceContext { GenaiWebAdapterCall(ProviderAdapter): "genai-web-adapter-call", GenaiWebModelCall(ProviderModel): "genai-web-model-call", GenaiWebStream(ProviderModel): "genai-web-stream", UnknownWebStreamSource(ServiceContextUnknownWebStreamSource): "unknown-web-stream-source" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceCause {
    Genai(GenaiFailure),
    Webc(WebcFailure),
    Reqwest(ReqwestFailure),
    StreamTimeout(StreamTimeoutKind),
    Utf8(Utf8Failure),
    Io(IoCause),
    Url(ProviderUrlFailure),
    ValueExt(ValueExtFailure),
    Json(ForeignJsonFailure),
    UnknownWebStreamSource(ServiceCauseUnknownWebStreamSource),
    RefreshResponse(RefreshServiceResponse),
}
variant!(ServiceCause { Genai(GenaiFailure): "genai", Webc(WebcFailure): "webc", Reqwest(ReqwestFailure): "reqwest", StreamTimeout(StreamTimeoutKind): "stream-timeout", Utf8(Utf8Failure): "utf8", Io(IoCause): "io", Url(ProviderUrlFailure): "url", ValueExt(ValueExtFailure): "value-ext", Json(ForeignJsonFailure): "json", UnknownWebStreamSource(ServiceCauseUnknownWebStreamSource): "unknown-web-stream-source", RefreshResponse(RefreshServiceResponse): "refresh-response" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModel {
    pub adapter: ProviderAdapter,
    pub name: String,
}
record!(ProviderModel {
    adapter: "adapter",
    name: "name"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ProviderMessageRole {
    System,
    User,
    Assistant,
    Tool,
}
label!(ProviderMessageRole);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum StreamTimeoutKind {
    Headers,
    Read,
}
label!(StreamTimeoutKind);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenaiFailure {
    ChatReqHasNoMessages(ProviderModel),
    LastChatMessageIsNotUser(GenaiFailureLastChatMessageIsNotUser),
    MessageRoleNotSupported(GenaiFailureMessageRoleNotSupported),
    MessageContentTypeNotSupported(GenaiFailureMessageContentTypeNotSupported),
    JsonModeWithoutInstruction,
    VerbosityParsing(GenaiFailureVerbosityParsing),
    ReasoningParsing(GenaiFailureReasoningParsing),
    ServiceTierParsing(GenaiFailureServiceTierParsing),
    PromptCacheRetentionParsing(GenaiFailurePromptCacheRetentionParsing),
    NoChatResponse(ProviderModel),
    InvalidJsonResponseElement(GenaiFailureInvalidJsonResponseElement),
    RequiresApiKey(ProviderModel),
    NoAuthResolver(ProviderModel),
    NoAuthData(ProviderModel),
    ModelMapperFailed(GenaiFailureModelMapperFailed),
    ChatResponseGeneration(GenaiFailureChatResponseGeneration),
    ChatResponse(GenaiFailureChatResponse),
    StreamParse(GenaiFailureStreamParse),
    HttpError(GenaiFailureHttpError),
    Resolver(GenaiFailureResolver),
    AdapterNotSupported(GenaiFailureAdapterNotSupported),
    CacheBreakpointNoEligibleContent(GenaiFailureCacheBreakpointNoEligibleContent),
    AdapterKindMismatch(GenaiFailureAdapterKindMismatch),
    Internal(GenaiFailureInternal),
    ClientBuildFail(GenaiFailureClientBuildFail),
    JsonValueExt(ValueExtFailure),
    SerdeJson(ForeignJsonFailure),
}
variant!(GenaiFailure { ChatReqHasNoMessages(ProviderModel): "chat-req-has-no-messages", LastChatMessageIsNotUser(GenaiFailureLastChatMessageIsNotUser): "last-chat-message-is-not-user", MessageRoleNotSupported(GenaiFailureMessageRoleNotSupported): "message-role-not-supported", MessageContentTypeNotSupported(GenaiFailureMessageContentTypeNotSupported): "message-content-type-not-supported", JsonModeWithoutInstruction: "json-mode-without-instruction", VerbosityParsing(GenaiFailureVerbosityParsing): "verbosity-parsing", ReasoningParsing(GenaiFailureReasoningParsing): "reasoning-parsing", ServiceTierParsing(GenaiFailureServiceTierParsing): "service-tier-parsing", PromptCacheRetentionParsing(GenaiFailurePromptCacheRetentionParsing): "prompt-cache-retention-parsing", NoChatResponse(ProviderModel): "no-chat-response", InvalidJsonResponseElement(GenaiFailureInvalidJsonResponseElement): "invalid-json-response-element", RequiresApiKey(ProviderModel): "requires-api-key", NoAuthResolver(ProviderModel): "no-auth-resolver", NoAuthData(ProviderModel): "no-auth-data", ModelMapperFailed(GenaiFailureModelMapperFailed): "model-mapper-failed", ChatResponseGeneration(GenaiFailureChatResponseGeneration): "chat-response-generation", ChatResponse(GenaiFailureChatResponse): "chat-response", StreamParse(GenaiFailureStreamParse): "stream-parse", HttpError(GenaiFailureHttpError): "http-error", Resolver(GenaiFailureResolver): "resolver", AdapterNotSupported(GenaiFailureAdapterNotSupported): "adapter-not-supported", CacheBreakpointNoEligibleContent(GenaiFailureCacheBreakpointNoEligibleContent): "cache-breakpoint-no-eligible-content", AdapterKindMismatch(GenaiFailureAdapterKindMismatch): "adapter-kind-mismatch", Internal(GenaiFailureInternal): "internal", ClientBuildFail(GenaiFailureClientBuildFail): "client-build-fail", JsonValueExt(ValueExtFailure): "json-value-ext", SerdeJson(ForeignJsonFailure): "serde-json" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResponseHeader {
    pub name: String,
    pub value: crate::data::Bytes,
}
record!(ProviderResponseHeader {
    name: "name",
    value: "value"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAdapter {
    OpenAi,
    OpenAiResp,
    Gemini,
    GeminiIx,
    Anthropic,
    Fireworks,
    Together,
    Groq,
    Aihubmix,
    Kimi,
    Mimo,
    Moonshot,
    Nebius,
    Xai,
    DeepSeek,
    Mistral,
    Zai,
    BigModel,
    Aliyun,
    QwenCloud,
    Baidu,
    Cohere,
    Ollama,
    OllamaCloud,
    Omlx,
    Vertex,
    GithubCopilot,
    OpenCodeGo,
    BedrockApi,
    OpenRouter,
    AtlasCloud,
    Requesty,
    ApiRoute,
    Heabsy,
    Runcrate,
    CloudGpu,
    Cheapaimodel,
    MiniMax,
    Custom(u8),
}
variant!(ProviderAdapter { OpenAi: "open-ai", OpenAiResp: "open-ai-resp", Gemini: "gemini", GeminiIx: "gemini-ix", Anthropic: "anthropic", Fireworks: "fireworks", Together: "together", Groq: "groq", Aihubmix: "aihubmix", Kimi: "kimi", Mimo: "mimo", Moonshot: "moonshot", Nebius: "nebius", Xai: "xai", DeepSeek: "deep-seek", Mistral: "mistral", Zai: "zai", BigModel: "big-model", Aliyun: "aliyun", QwenCloud: "qwen-cloud", Baidu: "baidu", Cohere: "cohere", Ollama: "ollama", OllamaCloud: "ollama-cloud", Omlx: "omlx", Vertex: "vertex", GithubCopilot: "github-copilot", OpenCodeGo: "open-code-go", BedrockApi: "bedrock-api", OpenRouter: "open-router", AtlasCloud: "atlas-cloud", Requesty: "requesty", ApiRoute: "api-route", Heabsy: "heabsy", Runcrate: "runcrate", CloudGpu: "cloud-gpu", Cheapaimodel: "cheapaimodel", MiniMax: "mini-max", Custom(u8): "custom" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebcFailure {
    ResponseFailedNotJson(WebcFailureResponseFailedNotJson),
    ResponseFailedInvalidJson(WebcFailureResponseFailedInvalidJson),
    ResponseFailedStatus(WebcFailureResponseFailedStatus),
    JsonValueExt(ValueExtFailure),
}
variant!(WebcFailure { ResponseFailedNotJson(WebcFailureResponseFailedNotJson): "response-failed-not-json", ResponseFailedInvalidJson(WebcFailureResponseFailedInvalidJson): "response-failed-invalid-json", ResponseFailedStatus(WebcFailureResponseFailedStatus): "response-failed-status", JsonValueExt(ValueExtFailure): "json-value-ext" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolverFailure {
    ApiKeyEnvNotFound(ResolverFailureApiKeyEnvNotFound),
    AuthDataNotSingleValue,
    Custom(ResolverFailureCustom),
}
variant!(ResolverFailure { ApiKeyEnvNotFound(ResolverFailureApiKeyEnvNotFound): "api-key-env-not-found", AuthDataNotSingleValue: "auth-data-not-single-value", Custom(ResolverFailureCustom): "custom" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueExtFailure {
    Custom(ValueExtFailureCustom),
    PropertyNotFound(ValueExtFailurePropertyNotFound),
    PropertyValueNotOfType(ValueExtFailurePropertyValueNotOfType),
    ValueNotOfType(ValueExtFailureValueNotOfType),
    SerdeJson(ForeignJsonFailure),
}
variant!(ValueExtFailure { Custom(ValueExtFailureCustom): "custom", PropertyNotFound(ValueExtFailurePropertyNotFound): "property-not-found", PropertyValueNotOfType(ValueExtFailurePropertyValueNotOfType): "property-value-not-of-type", ValueNotOfType(ValueExtFailureValueNotOfType): "value-not-of-type", SerdeJson(ForeignJsonFailure): "serde-json" });

/// Only genai/value-ext decoding diagnostics; local schemas use `JsonInputFailure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignJsonFailure {
    pub category: ForeignJsonCategory,
    pub line: Count,
    pub column: Count,
    pub io_kind: Option<IoKind>,
    pub detail: String,
}
record!(ForeignJsonFailure {
    category: "category",
    line: "line",
    column: "column",
    io_kind: "io-kind",
    detail: "detail"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ForeignJsonCategory {
    Io,
    Syntax,
    Data,
    Eof,
}
label!(ForeignJsonCategory);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReqwestFailure {
    pub summary: ReqwestSummary,
    pub sources: Vec<ReqwestSource>,
}
record!(ReqwestFailure {
    summary: "summary",
    sources: "sources"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReqwestSummary {
    pub class: ReqwestClass,
    pub timeout: bool,
    pub connect: bool,
    pub dns: bool,
    pub url: Option<String>,
}
record!(ReqwestSummary {
    class: "class",
    timeout: "timeout",
    connect: "connect",
    dns: "dns",
    url: "url"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqwestClass {
    Builder,
    Redirect,
    Status(u32),
    Request,
    Body,
    Decode,
    Upgrade,
    Unclassified(ReqwestClassUnclassified),
}
variant!(ReqwestClass { Builder: "builder", Redirect: "redirect", Status(u32): "status", Request: "request", Body: "body", Decode: "decode", Upgrade: "upgrade", Unclassified(ReqwestClassUnclassified): "unclassified" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqwestSource {
    Reqwest(ReqwestSummary),
    Io(IoCause),
    Json(ForeignJsonFailure),
    Url(ProviderUrlFailure),
    Utf8(Utf8Failure),
    Foreign(ReqwestSourceForeign),
}
variant!(ReqwestSource { Reqwest(ReqwestSummary): "reqwest", Io(IoCause): "io", Json(ForeignJsonFailure): "json", Url(ProviderUrlFailure): "url", Utf8(Utf8Failure): "utf8", Foreign(ReqwestSourceForeign): "foreign" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderUrlFailure {
    EmptyHost,
    Idna,
    InvalidPort,
    InvalidIpv4Address,
    InvalidIpv6Address,
    InvalidDomainCharacter,
    RelativeWithoutBase,
    RelativeWithCannotBeABaseBase,
    SetHostOnCannotBeABaseUrl,
    Overflow,
    Unclassified(ProviderUrlFailureUnclassified),
}
variant!(ProviderUrlFailure { EmptyHost: "empty-host", Idna: "idna", InvalidPort: "invalid-port", InvalidIpv4Address: "invalid-ipv4-address", InvalidIpv6Address: "invalid-ipv6-address", InvalidDomainCharacter: "invalid-domain-character", RelativeWithoutBase: "relative-without-base", RelativeWithCannotBeABaseBase: "relative-with-cannot-be-a-base-base", SetHostOnCannotBeABaseUrl: "set-host-on-cannot-be-a-base-url", Overflow: "overflow", Unclassified(ProviderUrlFailureUnclassified): "unclassified" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialFailure {
    pub account: String,
    pub service: CredentialService,
    pub cause: CredentialCause,
}
record!(CredentialFailure {
    account: "account",
    service: "service",
    cause: "cause"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum CredentialService {
    Chatgpt,
    Codex,
    Grok,
}
label!(CredentialService);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialCause {
    OwnerPanicked,
    OwnerStopped,
    Removed,
    UnresolvedRotation,
    Store(CredentialCauseStore),
    LeaseTimedOut,
    PreparationDeadline,
    Refresh(CredentialCauseRefresh),
}
variant!(CredentialCause { OwnerPanicked: "owner-panicked", OwnerStopped: "owner-stopped", Removed: "removed", UnresolvedRotation: "unresolved-rotation", Store(CredentialCauseStore): "store", LeaseTimedOut: "lease-timed-out", PreparationDeadline: "preparation-deadline", Refresh(CredentialCauseRefresh): "refresh" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum CredentialStoreOperation {
    OpenLease,
    LockLease,
    Read,
    PersistSuccessor,
    PersistFreshRotation,
}
label!(CredentialStoreOperation);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum RefreshOutcome {
    NotSubmitted,
    Rejected,
    Ambiguous,
}
label!(RefreshOutcome);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshFailure {
    TimedOut(DurationFact),
    Service(ServiceFailure),
    Response(RefreshFailureResponse),
    MissingToken(RefreshTokenField),
    EmptyToken(RefreshTokenField),
    InvalidExpiry(RefreshFailureInvalidExpiry),
    ExpiryOverflow(RefreshFailureExpiryOverflow),
}
variant!(RefreshFailure { TimedOut(DurationFact): "timed-out", Service(ServiceFailure): "service", Response(RefreshFailureResponse): "response", MissingToken(RefreshTokenField): "missing-token", EmptyToken(RefreshTokenField): "empty-token", InvalidExpiry(RefreshFailureInvalidExpiry): "invalid-expiry", ExpiryOverflow(RefreshFailureExpiryOverflow): "expiry-overflow" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum RefreshTokenField {
    AccessToken,
    RefreshToken,
    IdToken,
}
label!(RefreshTokenField);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshServiceResponse {
    pub status: u32,
    pub body: RefreshResponseBody,
}
record!(RefreshServiceResponse {
    status: "status",
    body: "body"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshResponseBody {
    Detail(RefreshResponseBodyDetail),
    ReadFailed(ReqwestFailure),
}
variant!(RefreshResponseBody { Detail(RefreshResponseBodyDetail): "detail", ReadFailed(ReqwestFailure): "read-failed" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshResponseDetail {
    Text(RefreshResponseDetailText),
    Html,
    Empty,
}
variant!(RefreshResponseDetail { Text(RefreshResponseDetailText): "text", Html: "html", Empty: "empty" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperFailure {
    NoReply,
    EndedBeforeReporting,
    Cancelled,
    ProviderStopped(ProviderStop),
    Provider(ProviderFailure),
    Connection(ConnectionFailure),
    Context(ContextFailure),
    Recording(RecordingFailure),
    Panicked,
}
variant!(HelperFailure { NoReply: "no-reply", EndedBeforeReporting: "ended-before-reporting", Cancelled: "cancelled", ProviderStopped(ProviderStop): "provider-stopped", Provider(ProviderFailure): "provider", Connection(ConnectionFailure): "connection", Context(ContextFailure): "context", Recording(RecordingFailure): "recording", Panicked: "panicked" });

/// Payloads are raw provider stop codes, never host prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStop {
    Completed(String),
    MaxTokens(String),
    ToolCall(String),
    ContentFilter(String),
    StopSequence(String),
    Unclassified(String),
}
variant!(ProviderStop { Completed(String): "completed", MaxTokens(String): "max-tokens", ToolCall(String): "tool-call", ContentFilter(String): "content-filter", StopSequence(String): "stop-sequence", Unclassified(String): "unclassified" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithdrawalEvidence {
    Unavailable,
    EarliestRetained(Count),
    NothingRetained,
}
variant!(WithdrawalEvidence { Unavailable: "unavailable", EarliestRetained(Count): "earliest-retained", NothingRetained: "nothing-retained" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootFailureProfileNotAbsolute {
    pub path: String,
}
record!(BootFailureProfileNotAbsolute { path: "path" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorFailureUnknownStyle {
    pub style: String,
    pub offered: Vec<String>,
}
record!(EditorFailureUnknownStyle {
    style: "style",
    offered: "offered"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorFailureCommand {
    pub variable: EditorVariable,
    pub cause: EditorCommandCause,
}
record!(EditorFailureCommand {
    variable: "variable",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorFailureDraft {
    pub operation: DraftOperation,
    pub cause: DraftFailure,
}
record!(EditorFailureDraft {
    operation: "operation",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorFailureWorkspace {
    pub step: EditorWorkspaceStep,
    pub path: Option<String>,
    pub cause: EditorWorkspaceFailure,
}
record!(EditorFailureWorkspace {
    step: "step",
    path: "path",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftFailureTooLarge {
    pub limit: Count,
}
record!(DraftFailureTooLarge { limit: "limit" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorWorkspaceFailureExpectedRegularFile {
    pub found: Option<EntryKind>,
}
record!(EditorWorkspaceFailureExpectedRegularFile { found: "found" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFailureAlreadyLoaded {
    pub name: String,
}
record!(PluginFailureAlreadyLoaded { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFailureNotFound {
    pub requested: String,
}
record!(PluginFailureNotFound {
    requested: "requested"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFailureOptionsWithoutFactory {
    pub plugin: String,
}
record!(PluginFailureOptionsWithoutFactory { plugin: "plugin" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFailureFactoryReturnedBlock {
    pub plugin: String,
}
record!(PluginFailureFactoryReturnedBlock { plugin: "plugin" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFailureUnknownHookEvent {
    pub event: String,
    pub offered: Vec<String>,
}
record!(PluginFailureUnknownHookEvent {
    event: "event",
    offered: "offered"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFailureHandlerInstallationEscaped {
    pub handler: String,
    pub cause: EscapedLoad,
}
record!(PluginFailureHandlerInstallationEscaped {
    handler: "handler",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingFailureNotation {
    pub key: String,
    pub cause: KeyNotationFailure,
}
record!(KeybindingFailureNotation {
    key: "key",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingFailureReservedAction {
    pub key: String,
    pub action: ReservedKeyAction,
}
record!(KeybindingFailureReservedAction {
    key: "key",
    action: "action"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingFailureSequencePrefix {
    pub key: String,
}
record!(KeybindingFailureSequencePrefix { key: "key" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingFailureGuardRegex {
    pub key: String,
    pub pattern: String,
    pub cause: RegexCause,
}
record!(KeybindingFailureGuardRegex {
    key: "key",
    pattern: "pattern",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingFailureUnguardedBuiltinShadow {
    pub key: String,
    pub action: BuiltinKeyAction,
}
record!(KeybindingFailureUnguardedBuiltinShadow {
    key: "key",
    action: "action"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNotationFailureDuplicateModifier {
    pub chord: String,
    pub modifier: KeyModifier,
}
record!(KeyNotationFailureDuplicateModifier {
    chord: "chord",
    modifier: "modifier"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNotationFailureUnknownKey {
    pub token: String,
}
record!(KeyNotationFailureUnknownKey { token: "token" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNotationFailureControlCharacter {
    pub character: String,
}
record!(KeyNotationFailureControlCharacter {
    character: "character"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNotationFailureShiftedCharacter {
    pub chord: String,
    pub character: String,
    pub control: bool,
    pub alt: bool,
}
record!(KeyNotationFailureShiftedCharacter {
    chord: "chord",
    character: "character",
    control: "control",
    alt: "alt"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNotationFailureUppercaseControl {
    pub chord: String,
}
record!(KeyNotationFailureUppercaseControl { chord: "chord" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNotationFailureTerminalAlias {
    pub chord: String,
    pub alt: bool,
    pub alias: TerminalKeyAlias,
}
record!(KeyNotationFailureTerminalAlias {
    chord: "chord",
    alt: "alt",
    alias: "alias"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyFailureUnknownBase {
    pub name: String,
    pub offered: Vec<String>,
}
record!(PolicyFailureUnknownBase {
    name: "name",
    offered: "offered"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyFailureEscapedLoad {
    pub flag: String,
    pub cause: EscapedLoad,
}
record!(PolicyFailureEscapedLoad {
    flag: "flag",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureNameTaken {
    pub name: String,
}
record!(AgentFailureNameTaken { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureUnknownProviderRole {
    pub name: String,
    pub offered: Vec<String>,
}
record!(AgentFailureUnknownProviderRole {
    name: "name",
    offered: "offered"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureNotDescendant {
    pub name: String,
}
record!(AgentFailureNotDescendant { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureNotOwned {
    pub name: String,
}
record!(AgentFailureNotOwned { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureSelfMessage {
    pub name: String,
}
record!(AgentFailureSelfMessage { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureNoLiveAgent {
    pub name: String,
}
record!(AgentFailureNoLiveAgent { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureEnded {
    pub name: String,
}
record!(AgentFailureEnded { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureResting {
    pub name: String,
    pub parent: String,
}
record!(AgentFailureResting {
    name: "name",
    parent: "parent"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureReplyPending {
    pub name: String,
}
record!(AgentFailureReplyPending { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureReplyAbsent {
    pub name: String,
}
record!(AgentFailureReplyAbsent { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureReplyStopped {
    pub name: String,
}
record!(AgentFailureReplyStopped { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureReplyFailed {
    pub name: String,
    pub cause: HelperFailure,
}
record!(AgentFailureReplyFailed {
    name: "name",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureReplyUnavailable {
    pub name: String,
    pub cause: RecordingFailure,
}
record!(AgentFailureReplyUnavailable {
    name: "name",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFailureQueuedUnsynced {
    pub name: String,
    pub cause: RecordingFailure,
}
record!(AgentFailureQueuedUnsynced {
    name: "name",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingFailureRecording {
    pub record: Option<String>,
    pub stage: RecordingStage,
    pub cause: IoCause,
}
record!(RecordingFailureRecording {
    record: "record",
    stage: "stage",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingFailureCellPoisoned {
    pub agent: Count,
}
record!(RecordingFailureCellPoisoned { agent: "agent" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureTurnNotRecorded {
    pub turn: Count,
    pub reach: Option<Count>,
}
record!(ContextFailureTurnNotRecorded {
    turn: "turn",
    reach: "reach"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureWithdrawn {
    pub turn: Count,
    pub evidence: WithdrawalEvidence,
}
record!(ContextFailureWithdrawn {
    turn: "turn",
    evidence: "evidence"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureCurrentlyWritten {
    pub turn: Count,
}
record!(ContextFailureCurrentlyWritten { turn: "turn" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureStraddledEviction {
    pub requested: TurnSpan,
    pub covering: TurnSpan,
}
record!(ContextFailureStraddledEviction {
    requested: "requested",
    covering: "covering"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureReadBack {
    pub turn: Count,
    pub source: Count,
    pub record: String,
    pub cause: RecordReadFailure,
}
record!(ContextFailureReadBack {
    turn: "turn",
    source: "source",
    record: "record",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureMutation {
    pub operation: ContextOperation,
    pub state: ContextState,
}
record!(ContextFailureMutation {
    operation: "operation",
    state: "state"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureAssistantRole {
    pub found: ProviderMessageRole,
}
record!(ContextFailureAssistantRole { found: "found" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureToolResultCount {
    pub expected: Count,
    pub got: Count,
}
record!(ContextFailureToolResultCount {
    expected: "expected",
    got: "got"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureUnknownToolResult {
    pub id: String,
}
record!(ContextFailureUnknownToolResult { id: "id" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFailureDuplicateToolResult {
    pub id: String,
}
record!(ContextFailureDuplicateToolResult { id: "id" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextStateAwaitingToolResults {
    pub pending: Count,
}
record!(ContextStateAwaitingToolResults { pending: "pending" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleFailureNoGrant {
    pub verb: String,
}
record!(ScheduleFailureNoGrant { verb: "verb" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleFailureNoFutureOccurrence {
    pub trigger: String,
}
record!(ScheduleFailureNoFutureOccurrence { trigger: "trigger" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleFailureDuplicateLiveLabel {
    pub label: String,
}
record!(ScheduleFailureDuplicateLiveLabel { label: "label" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleFailureRecording {
    pub label: Option<String>,
    pub trigger: Option<String>,
    pub cause: RecordingFailure,
}
record!(ScheduleFailureRecording {
    label: "label",
    trigger: "trigger",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFailureHashAbsent {
    pub path: String,
    pub hash: String,
}
record!(EditFailureHashAbsent {
    path: "path",
    hash: "hash"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFailureHashAmbiguous {
    pub path: String,
    pub hash: String,
    pub lines: Vec<Count>,
}
record!(EditFailureHashAmbiguous {
    path: "path",
    hash: "hash",
    lines: "lines"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFailureDuplicateTargetLine {
    pub path: String,
    pub line: Count,
}
record!(EditFailureDuplicateTargetLine {
    path: "path",
    line: "line"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFailureMultipleLiteralMatches {
    pub path: String,
    pub matches: Count,
    pub lines: Vec<Count>,
}
record!(EditFailureMultipleLiteralMatches {
    path: "path",
    matches: "matches",
    lines: "lines"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFailureNoLiteralMatch {
    pub path: String,
    pub evidence: LiteralMiss,
}
record!(EditFailureNoLiteralMatch {
    path: "path",
    evidence: "evidence"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFailureInvalidName {
    pub name: String,
}
record!(SkillFailureInvalidName { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFailureMissing {
    pub name: String,
}
record!(SkillFailureMissing { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFailureRead {
    pub name: String,
    pub place: SkillPlace,
    pub cause: IoCause,
}
record!(SkillFailureRead {
    name: "name",
    place: "place",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFailureYaml {
    pub name: String,
    pub place: SkillPlace,
    pub detail: String,
}
record!(SkillFailureYaml {
    name: "name",
    place: "place",
    detail: "detail"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFailureFrontmatter {
    pub name: String,
    pub place: SkillPlace,
    pub cause: FrontmatterFailure,
}
record!(SkillFailureFrontmatter {
    name: "name",
    place: "place",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterFailureNotMapping {
    pub found: PodKind,
}
record!(FrontmatterFailureNotMapping { found: "found" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterFailureFieldShape {
    pub field: SkillField,
    pub found: PodKind,
}
record!(FrontmatterFailureFieldShape {
    field: "field",
    found: "found"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterFailureInvalidName {
    pub name: String,
}
record!(FrontmatterFailureInvalidName { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterFailureDirectoryNameMismatch {
    pub declared: String,
    pub directory: String,
}
record!(FrontmatterFailureDirectoryNameMismatch {
    declared: "declared",
    directory: "directory"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFactoryFailureUnsupported {
    pub backend: FactoryBackend,
}
record!(EngineFactoryFailureUnsupported { backend: "backend" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFactoryFailureDialTimedOut {
    pub port: u32,
    pub within: DurationFact,
}
record!(EngineFactoryFailureDialTimedOut {
    port: "port",
    within: "within"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFactoryFailureDialWaitDisconnected {
    pub port: u32,
}
record!(EngineFactoryFailureDialWaitDisconnected { port: "port" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFactoryFailurePlatform {
    pub backend: FactoryBackend,
    pub domain: String,
    pub code: i64,
    pub detail: String,
}
record!(EngineFactoryFailurePlatform {
    backend: "backend",
    domain: "domain",
    code: "code",
    detail: "detail"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordReadFailureMissingLocusRecord {
    pub commit: Count,
    pub index: Count,
}
record!(RecordReadFailureMissingLocusRecord {
    commit: "commit",
    index: "index"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordReadFailureLocusSealMismatch {
    pub commit: Count,
}
record!(RecordReadFailureLocusSealMismatch { commit: "commit" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordReadFailureLine {
    pub commit: Count,
    pub cause: RecordLineFailure,
}
record!(RecordReadFailureLine {
    commit: "commit",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordLineFailureSealMismatch {
    pub sealed: String,
    pub actual: String,
}
record!(RecordLineFailureSealMismatch {
    sealed: "sealed",
    actual: "actual"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLeafTransient {
    pub cause: RetryableProviderCause,
    pub attempts: Count,
}
record!(ProviderLeafTransient {
    cause: "cause",
    attempts: "attempts"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLeafApi {
    pub model: String,
    pub cause: ServiceFailure,
}
record!(ProviderLeafApi {
    model: "model",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLeafOverflow {
    pub model: String,
    pub provider: Option<String>,
    pub sent: Option<Count>,
    pub limit: Option<Count>,
    pub listed: Option<Count>,
    pub cause: ServiceFailure,
}
record!(ProviderLeafOverflow {
    model: "model",
    provider: "provider",
    sent: "sent",
    limit: "limit",
    listed: "listed",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLeafOutputCap {
    pub stop_reason: String,
}
record!(ProviderLeafOutputCap {
    stop_reason: "stop-reason"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryableProviderCauseStreamEndedWithoutEnd {
    pub service: String,
    pub model: String,
}
record!(RetryableProviderCauseStreamEndedWithoutEnd {
    service: "service",
    model: "model"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRefusalCauseHeld {
    pub service: String,
    pub model: String,
}
record!(ProviderRefusalCauseHeld {
    service: "service",
    model: "model"
});

/// Only an unknown foreign `WebStream` wrapper with a further source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceContextUnknownWebStreamSource {
    pub detail: String,
}
record!(ServiceContextUnknownWebStreamSource { detail: "detail" });

/// Only an unknown terminal foreign `WebStream` source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCauseUnknownWebStreamSource {
    pub detail: String,
}
record!(ServiceCauseUnknownWebStreamSource { detail: "detail" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureLastChatMessageIsNotUser {
    pub model: ProviderModel,
    pub actual_role: ProviderMessageRole,
}
record!(GenaiFailureLastChatMessageIsNotUser {
    model: "model",
    actual_role: "actual-role"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureMessageRoleNotSupported {
    pub model: ProviderModel,
    pub role: ProviderMessageRole,
}
record!(GenaiFailureMessageRoleNotSupported {
    model: "model",
    role: "role"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureMessageContentTypeNotSupported {
    pub model: ProviderModel,
    pub detail: String,
}
record!(GenaiFailureMessageContentTypeNotSupported {
    model: "model",
    detail: "detail"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureVerbosityParsing {
    pub actual: String,
}
record!(GenaiFailureVerbosityParsing { actual: "actual" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureReasoningParsing {
    pub actual: String,
}
record!(GenaiFailureReasoningParsing { actual: "actual" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureServiceTierParsing {
    pub actual: String,
}
record!(GenaiFailureServiceTierParsing { actual: "actual" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailurePromptCacheRetentionParsing {
    pub actual: String,
}
record!(GenaiFailurePromptCacheRetentionParsing { actual: "actual" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureInvalidJsonResponseElement {
    pub detail: String,
}
record!(GenaiFailureInvalidJsonResponseElement { detail: "detail" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureModelMapperFailed {
    pub model: ProviderModel,
    pub cause: ResolverFailure,
}
record!(GenaiFailureModelMapperFailed {
    model: "model",
    cause: "cause"
});

/// Retains the foreign response and intrinsic cause, never the request payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureChatResponseGeneration {
    pub model: ProviderModel,
    pub response_body: String,
    pub detail: String,
}
record!(GenaiFailureChatResponseGeneration {
    model: "model",
    response_body: "response-body",
    detail: "detail"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureChatResponse {
    pub model: ProviderModel,
    pub body: String,
}
record!(GenaiFailureChatResponse {
    model: "model",
    body: "body"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureStreamParse {
    pub model: ProviderModel,
    pub cause: ForeignJsonFailure,
}
record!(GenaiFailureStreamParse {
    model: "model",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureHttpError {
    pub status: u32,
    pub canonical_reason: String,
    pub body: String,
    pub headers: Vec<ProviderResponseHeader>,
}
record!(GenaiFailureHttpError {
    status: "status",
    canonical_reason: "canonical-reason",
    body: "body",
    headers: "headers"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureResolver {
    pub model: ProviderModel,
    pub cause: ResolverFailure,
}
record!(GenaiFailureResolver {
    model: "model",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureAdapterNotSupported {
    pub adapter: ProviderAdapter,
    pub feature: String,
}
record!(GenaiFailureAdapterNotSupported {
    adapter: "adapter",
    feature: "feature"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureCacheBreakpointNoEligibleContent {
    pub model: ProviderModel,
    pub scope: String,
}
record!(GenaiFailureCacheBreakpointNoEligibleContent {
    model: "model",
    scope: "scope"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureAdapterKindMismatch {
    pub bound: ProviderAdapter,
    pub requested: ProviderAdapter,
    pub model: String,
}
record!(GenaiFailureAdapterKindMismatch {
    bound: "bound",
    requested: "requested",
    model: "model"
});

/// The intrinsic genai diagnostic is verbatim, including endpoint text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureInternal {
    pub detail: String,
}
record!(GenaiFailureInternal { detail: "detail" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenaiFailureClientBuildFail {
    pub detail: String,
}
record!(GenaiFailureClientBuildFail { detail: "detail" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebcFailureResponseFailedNotJson {
    pub content_type: String,
    pub body: String,
}
record!(WebcFailureResponseFailedNotJson {
    content_type: "content-type",
    body: "body"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebcFailureResponseFailedInvalidJson {
    pub body: String,
    pub detail: String,
}
record!(WebcFailureResponseFailedInvalidJson {
    body: "body",
    detail: "detail"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebcFailureResponseFailedStatus {
    pub status: u32,
    pub body: String,
    pub headers: Vec<ProviderResponseHeader>,
}
record!(WebcFailureResponseFailedStatus {
    status: "status",
    body: "body",
    headers: "headers"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverFailureApiKeyEnvNotFound {
    pub env_name: String,
}
record!(ResolverFailureApiKeyEnvNotFound {
    env_name: "env-name"
});

/// Only the foreign resolver diagnostic, not a locally authored refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverFailureCustom {
    pub detail: String,
}
record!(ResolverFailureCustom { detail: "detail" });

/// Only the foreign value-ext diagnostic, not a local validator message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueExtFailureCustom {
    pub detail: String,
}
record!(ValueExtFailureCustom { detail: "detail" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueExtFailurePropertyNotFound {
    pub name: String,
}
record!(ValueExtFailurePropertyNotFound { name: "name" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueExtFailurePropertyValueNotOfType {
    pub name: String,
    pub required_library_type: String,
}
record!(ValueExtFailurePropertyValueNotOfType {
    name: "name",
    required_library_type: "required-library-type"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueExtFailureValueNotOfType {
    pub required_library_type: String,
}
record!(ValueExtFailureValueNotOfType {
    required_library_type: "required-library-type"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReqwestClassUnclassified {
    pub detail: String,
}
record!(ReqwestClassUnclassified { detail: "detail" });

/// Only an unknown object in the exposed reqwest source chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReqwestSourceForeign {
    pub detail: String,
}
record!(ReqwestSourceForeign { detail: "detail" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderUrlFailureUnclassified {
    pub detail: String,
}
record!(ProviderUrlFailureUnclassified { detail: "detail" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialCauseStore {
    pub operation: CredentialStoreOperation,
    pub cause: IoCause,
}
record!(CredentialCauseStore {
    operation: "operation",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialCauseRefresh {
    pub outcome: RefreshOutcome,
    pub cause: RefreshFailure,
}
record!(CredentialCauseRefresh {
    outcome: "outcome",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshFailureResponse {
    pub status: u32,
    pub cause: RefreshPayloadFailure,
}
record!(RefreshFailureResponse {
    status: "status",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshFailureInvalidExpiry {
    pub seconds: Count,
}
record!(RefreshFailureInvalidExpiry { seconds: "seconds" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshFailureExpiryOverflow {
    pub now: Count,
    pub seconds: Count,
}
record!(RefreshFailureExpiryOverflow {
    now: "now",
    seconds: "seconds"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshResponseBodyDetail {
    pub code: Option<String>,
    pub detail: RefreshResponseDetail,
}
record!(RefreshResponseBodyDetail {
    code: "code",
    detail: "detail"
});

/// Sanitized response prefix, at most 200 characters, with no appended ellipsis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshResponseDetailText {
    pub preview: String,
    pub truncated: bool,
}
record!(RefreshResponseDetailText {
    preview: "preview",
    truncated: "truncated"
});

impl HostFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Ral(value) => format!("ral: {}", value.describe()),
            Self::Exarch(value) => format!("exarch: {}", value.describe()),
        }
    }
}

impl RalFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Boot(value) => value.describe(),
            Self::Editor(value) => value.describe(),
            Self::Plugin(value) => value.describe(),
            Self::Keybinding(value) => value.describe(),
        }
    }
}

impl BootFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::AlreadyBooted => "the session has already booted".into(),
            Self::ProfileNotAbsolute(value) => value.describe(),
        }
    }
}

impl EditorFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::NoPluginContext => "no plugin context (not inside a plugin handler)".into(),
            Self::UnknownStyle(value) => value.describe(),
            Self::Command(value) => value.describe(),
            Self::Draft(value) => value.describe(),
            Self::Workspace(value) => value.describe(),
        }
    }

    pub fn advice(&self) -> Vec<String> {
        let mut advice = match self {
            Self::Command(command) => match command.cause {
                EditorCommandCause::Nul => vec!["remove it".into()],
                EditorCommandCause::Unbalanced => vec![
                    "it is split into words as a POSIX shell would, with nothing expanded".into(),
                ],
                EditorCommandCause::NoProgram => Vec::new(),
            },
            Self::Draft(_) | Self::Workspace(_) => Vec::new(),
            Self::NoPluginContext | Self::UnknownStyle(_) => return Vec::new(),
        };
        advice.push("the input buffer was not changed".into());
        advice
    }
}

impl EditorVariable {
    pub fn describe(&self) -> String {
        match self {
            Self::Visual => "VISUAL",
            Self::Editor => "EDITOR",
        }
        .into()
    }
}

impl EditorCommandCause {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl DraftOperation {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl DraftFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::TooLarge(value) => value.describe(),
            Self::Read(value) => format!("could not read the draft: {}", value.describe()),
            Self::Utf8(value) => format!("the draft is not UTF-8: {}", value.describe()),
        }
    }
}

impl EditorWorkspaceStep {
    pub fn describe(&self) -> String {
        match self {
            Self::TemporaryDirectory => "find a temporary directory",
            Self::CreateDirectory => "create the editor workspace",
            Self::CreateBuffer => "create the draft file",
            Self::WriteBuffer => "write the draft file",
            Self::BufferPath => "get the draft file path",
            Self::LocateBuffer => "locate the draft file",
            Self::InspectBuffer => "inspect the draft file",
            Self::OpenBuffer => "open the draft file",
            Self::RemoveWorkspace => "remove the editor workspace",
        }
        .into()
    }
}

impl EditorWorkspaceFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Io(value) => value.describe(),
            Self::ExpectedRegularFile(value) => value.describe(),
        }
    }
}

impl PluginFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::AlreadyLoaded(value) => value.describe(),
            Self::NotFound(value) => value.describe(),
            Self::OptionsWithoutFactory(value) => value.describe(),
            Self::FactoryReturnedBlock(value) => value.describe(),
            Self::UnknownHookEvent(value) => value.describe(),
            Self::HandlerInstallationEscaped(value) => value.describe(),
        }
    }
}

impl KeybindingFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Notation(value) => value.describe(),
            Self::ReservedAction(value) => value.describe(),
            Self::SequencePrefix(value) => value.describe(),
            Self::GuardRegex(value) => value.describe(),
            Self::UnguardedBuiltinShadow(value) => value.describe(),
        }
    }
}

impl KeyNotationFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Empty => "the key notation is empty".into(),
            Self::DuplicateModifier(value) => value.describe(),
            Self::UnknownKey(value) => value.describe(),
            Self::ControlCharacter(value) => value.describe(),
            Self::ShiftedCharacter(value) => value.describe(),
            Self::UppercaseControl(value) => value.describe(),
            Self::TerminalAlias(value) => value.describe(),
        }
    }

    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Empty => vec!["name a key such as 'ctrl-r'".into()],
            Self::DuplicateModifier(_) => Vec::new(),
            Self::UnknownKey(_) => vec!["a key is one character, f1 to f12, or one of tab, enter, escape, up, down, left, right, home, end, delete, backspace".into()],
            Self::ControlCharacter(_) => vec!["name the key that sends it, e.g. 'ctrl-c' or 'escape'".into()],
            Self::ShiftedCharacter(value) => {
                let mut chars = value.character.chars();
                let Some(character) = chars.next().filter(|c| c.is_alphabetic()) else {
                    return Vec::new();
                };
                if chars.next().is_some() {
                    return Vec::new();
                }
                let converted = if value.control { character.to_lowercase().collect::<String>() } else { character.to_uppercase().collect() };
                let mut chars = converted.chars();
                match (chars.next(), chars.next()) {
                    (Some(character), None) => {
                        let key = control_key_suggestion(character, value.control, value.alt);
                        vec![format!("did you mean '{key}'?")]
                    },
                    _ => Vec::new(),
                }
            }
            Self::UppercaseControl(value) => {
                let mut rest = value.chord.as_str();
                let (mut control, mut alt) = (false, false);
                while let Some(tail) = rest.strip_prefix("ctrl-").or_else(|| rest.strip_prefix("alt-")) {
                    control |= rest.starts_with("ctrl-");
                    alt |= rest.starts_with("alt-");
                    rest = tail;
                }
                let mut chars = rest.chars();
                match (chars.next(), chars.next()) {
                    (Some(character), None) if control && character.is_ascii_uppercase() => {
                        let key = control_key_suggestion(character.to_ascii_lowercase(), true, alt);
                        vec![format!("did you mean '{key}'?")]
                    },
                    _ => Vec::new(),
                }
            }
            Self::TerminalAlias(value) => vec![format!("did you mean '{}{}'?", if value.alt { "alt-" } else { "" }, value.alias.describe())],
        }
    }
}

fn control_key_suggestion(character: char, control: bool, alt: bool) -> String {
    let alias = if control {
        match character {
            'h' => Some("backspace"),
            'i' => Some("tab"),
            'm' => Some("enter"),
            '[' => Some("escape"),
            _ => None,
        }
    } else {
        None
    };
    format!(
        "{}{}{}",
        if control && alias.is_none() {
            "ctrl-"
        } else {
            ""
        },
        if alt { "alt-" } else { "" },
        alias.map_or_else(|| character.to_string(), str::to_owned)
    )
}

impl KeyModifier {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ReservedKeyAction {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl TerminalKeyAlias {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl BuiltinKeyAction {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ExarchFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Policy(value) => format!("policy: {}", value.describe()),
            Self::Agent(value) => format!("agent: {}", value.describe()),
            Self::Engine(value) => format!("engine: {}", value.describe()),
            Self::Recording(value) => format!("recording: {}", value.describe()),
            Self::Context(value) => format!("context: {}", value.describe()),
            Self::Schedule(value) => format!("schedule: {}", value.describe()),
            Self::Edit(value) => format!("edit: {}", value.describe()),
            Self::Skill(value) => format!("skill: {}", value.describe()),
        }
    }
}

impl PolicyFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::OwnBinaryUnpinned => "could not pin exarch's own binary at startup (is it missing or unreadable?), so cannot exclude it from exec".into(),
            Self::UnknownBase(value) => value.describe(),
            Self::EscapedLoad(value) => value.describe(),
        }
    }
}

impl EscapedLoad {
    pub fn describe(&self) -> String {
        match self {
            Self::Exit(value) => format!("exit: {value}"),
            Self::Stopped(value) => format!("stopped: {}", <&str>::from(*value).to_owned()),
        }
    }
}

impl AgentFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::NoFuel => "no spawn fuel remains at this depth. Fuel bounds how deep a chain of spawns may recurse, not how many helpers may be started at an allowed depth".into(),
            Self::NameMalformed(cause) => cause.describe(),
            Self::NoReturnRecipient => "you converse with the human; you do not return. `reply` hands a spawned or headless agent's answer to whoever started it; a conversing agent keeps talking, turn after turn".into(),
            Self::NameTaken(value) => value.describe(),
            Self::UnknownProviderRole(value) => value.describe(),
            Self::NotDescendant(value) => value.describe(),
            Self::NotOwned(value) => value.describe(),
            Self::SelfMessage(value) => value.describe(),
            Self::NoLiveAgent(value) => value.describe(),
            Self::Ended(value) => value.describe(),
            Self::Resting(value) => value.describe(),
            Self::ReplyPending(value) => value.describe(),
            Self::ReplyAbsent(value) => value.describe(),
            Self::ReplyStopped(value) => value.describe(),
            Self::ReplyFailed(value) => value.describe(),
            Self::ReplyUnavailable(value) => value.describe(),
            Self::QueuedUnsynced(value) => value.describe(),
        }
    }
}

impl EngineFailure {
    pub fn describe(&self) -> String {
        let headline = match self.step {
            EngineStep::Factory => "could not make a helper engine",
            EngineStep::Adopt => "could not adopt the helper engine's wire",
            EngineStep::Attach => "could not attach the helper engine",
            EngineStep::Starting => "could not start the agent's engine",
        };
        format!("{headline}: {}", self.cause.describe())
    }
}

impl EngineStep {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl EngineCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Factory(value) => value.describe(),
            Self::Io(value) => value.describe(),
            Self::Connection(value) => value.describe(),
        }
    }
}

impl RecordingFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Recording(value) => value.describe(),
            Self::CellPoisoned(value) => value.describe(),
        }
    }

    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Recording(_) => vec!["this session can record nothing more; its record keeps everything before the failure, so the run can be resumed once the cause is fixed".into()],
            Self::CellPoisoned(_) => vec![
                "a thread panicked inside the agent's transaction, so its state may no longer match its log".into(),
                "resuming the run rebuilds its state from the log".into(),
            ],
        }
    }
}

impl RecordingStage {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl TurnSpan {
    pub fn describe(&self) -> String {
        if self.from == self.to {
            format!("turn {}", self.from)
        } else {
            format!("turns {}–{}", self.from, self.to)
        }
    }
}

impl ContextFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::EmptySelection => "`turns` names no turn".into(),
            Self::InvertedSpan(value) => value.describe(),
            Self::TurnNotRecorded(value) => value.describe(),
            Self::Withdrawn(value) => value.describe(),
            Self::CurrentlyWritten(value) => value.describe(),
            Self::StraddledEviction(value) => value.describe(),
            Self::ReadBack(value) => value.describe(),
            Self::Mutation(value) => value.describe(),
            Self::SteeringWithoutTurn => {
                "cannot accept a steering prompt: no turn is being written".into()
            }
            Self::AssistantRole(value) => value.describe(),
            Self::ToolResultCount(value) => value.describe(),
            Self::UnknownToolResult(value) => value.describe(),
            Self::DuplicateToolResult(value) => value.describe(),
        }
    }

    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::StraddledEviction(value) => value.advice(),
            Self::ReadBack(_) => vec!["check that the recorded session file is present, readable, and unchanged; was its directory moved, copied or edited?".into()],
            Self::EmptySelection | Self::InvertedSpan(_) | Self::TurnNotRecorded(_)
            | Self::Withdrawn(_) | Self::CurrentlyWritten(_) | Self::Mutation(_)
            | Self::SteeringWithoutTurn | Self::AssistantRole(_) | Self::ToolResultCount(_)
            | Self::UnknownToolResult(_) | Self::DuplicateToolResult(_) => Vec::new(),
        }
    }
}

impl ContextOperation {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ContextState {
    pub fn describe(&self) -> String {
        match self {
            Self::ReadyForUser => "ready for your next prompt".into(),
            Self::AwaitingAssistantAfterUser => "waiting for the model's reply".into(),
            Self::AwaitingToolResults(value) => value.describe(),
            Self::AwaitingAssistantAfterToolResults => {
                "waiting for the model to read the tool results".into()
            }
        }
    }
}

impl ScheduleFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::NoGrant(value) => value.describe(),
            Self::Cron(value) => value.describe(),
            Self::Duration(value) => value.describe(),
            Self::NoFutureOccurrence(value) => value.describe(),
            Self::DuplicateLiveLabel(value) => value.describe(),
            Self::Recording(value) => value.describe(),
        }
    }
}

impl EditFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::NoEdits => "no edits given".into(),
            Self::HashAbsent(value) => value.describe(),
            Self::HashAmbiguous(value) => value.describe(),
            Self::DuplicateTargetLine(value) => value.describe(),
            Self::EmptyFrom => "FROM must be non-empty".into(),
            Self::MultipleLiteralMatches(value) => value.describe(),
            Self::NoLiteralMatch(value) => value.describe(),
        }
    }
}

impl LiteralMiss {
    pub fn describe(&self) -> String {
        match self {
            Self::Escapes => "escapes".to_owned(),
            Self::Indentation(value) => format!("indentation: {value}"),
            Self::Absent => "absent".to_owned(),
        }
    }
}

impl SkillFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::InvalidName(value) => value.describe(),
            Self::Missing(value) => value.describe(),
            Self::Read(value) => value.describe(),
            Self::Yaml(value) => value.describe(),
            Self::Frontmatter(value) => value.describe(),
        }
    }

    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Missing(_) => vec!["did you mean one that skill-list shows?".into()],
            Self::InvalidName(_) | Self::Read(_) | Self::Yaml(_) | Self::Frontmatter(_) => {
                Vec::new()
            }
        }
    }
}

impl SkillPlace {
    pub fn describe(&self) -> String {
        match self {
            Self::Disk(value) => value.clone(),
            Self::Builtin => "built into exarch".into(),
        }
    }
}

impl FrontmatterFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Missing => "missing YAML frontmatter".into(),
            Self::NotMapping(value) => value.describe(),
            Self::MissingField(field) => format!(
                "frontmatter must contain a String {} field",
                field.describe()
            ),
            Self::FieldShape(value) => value.describe(),
            Self::InvalidName(value) => value.describe(),
            Self::DirectoryNameMismatch(value) => value.describe(),
        }
    }
}

impl SkillField {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl PodKind {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl EngineFactoryFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Unsupported(value) => value.describe(),
            Self::MachineThreadStopped => {
                "the machine thread stopped before the guest connection could be obtained".into()
            }
            Self::MissingSocketDevice => "the machine has no virtio socket device".into(),
            Self::NonVirtioSocketDevice => "the machine's socket device is not virtio".into(),
            Self::CompletionWithoutResult => {
                "the guest connection completed with neither a connection nor an error".into()
            }
            Self::DialTimedOut(value) => value.describe(),
            Self::DialWaitDisconnected(value) => value.describe(),
            Self::Platform(value) => value.describe(),
            Self::Io(value) => value.describe(),
        }
    }
}

impl FactoryBackend {
    pub fn describe(&self) -> String {
        match self {
            Self::Broker => "broker",
            Self::HyperV => "Hyper-V",
            Self::Virtualization => "Virtualization.framework",
        }
        .into()
    }
}

impl RecordReadFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Io(value) => value.describe(),
            Self::NonTranscriptRecord => "a pointer named a non-transcript record: a turn's address no longer names the records it was measured over".into(),
            Self::MissingLocusRecord(value) => value.describe(),
            Self::LocusSealMismatch(value) => value.describe(),
            Self::Line(value) => value.describe(),
        }
    }
}

impl RecordLineFailure {
    pub fn describe(&self) -> String {
        const INTACT: &str = "the lines before it are intact, and nothing from it on can be read.";
        const OLDER: &str = "was this session recorded by an older exarch? Older sessions can be neither resumed nor exported.";
        match self {
            Self::JsonSyntax(value) => format!(
                "is corrupt: it is not JSON ({}), so the file was damaged or edited after it was written; {INTACT}",
                value.describe()
            ),
            Self::InvalidEnvelope(value) => format!(
                "is not a commit this exarch writes ({}); {OLDER}",
                value.describe()
            ),
            Self::SealMismatch(value) => value.describe(),
            Self::InvalidRecords(value) => format!(
                "holds records this exarch does not read ({}); {OLDER}",
                value.describe()
            ),
        }
    }
}

impl ProviderFailure {
    pub fn describe(&self) -> String {
        self.context
            .iter()
            .rev()
            .fold(self.leaf.describe(), |inner, context| match context {
                ProviderContext::Truncated => format!("reply truncated: {inner}"),
            })
    }
}

impl ProviderContext {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ProviderLeaf {
    pub fn describe(&self) -> String {
        match self {
            Self::Unavailable(value) => format!("unavailable: {}", value.describe()),
            Self::Cancelled(value) => format!("cancelled: {}", value.describe()),
            Self::Transient(value) => value.describe(),
            Self::Refused(value) => value.describe(),
            Self::Api(value) => value.describe(),
            Self::Overflow(value) => value.describe(),
            Self::OutputCap(value) => value.describe(),
            Self::Terminal(value) => value.describe(),
            Self::Credential(value) => value.describe(),
        }
    }
}

impl ProviderCancelSite {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl RetryableProviderCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Service(value) => value.describe(),
            Self::StreamEndedWithoutEnd(value) => value.describe(),
        }
    }
}

impl ProviderRefusal {
    pub fn describe(&self) -> String {
        let reset = self.resets_at.as_ref().map_or_else(
            || "no reset time was supplied".to_owned(),
            |at| {
                let nanos = |at: &ProviderInstant| {
                    i128::from(at.seconds) * 1_000_000_000 + i128::from(at.subsec_nanoseconds)
                };
                let wait = (nanos(at) - nanos(&self.received)).max(0);
                format!(
                    "retry after {}.{:09} seconds",
                    wait / 1_000_000_000,
                    wait % 1_000_000_000
                )
            },
        );
        format!(
            "{}; {reset}: {}",
            self.limit.describe(),
            self.cause.describe()
        )
    }
}

impl ProviderLimit {
    pub fn describe(&self) -> String {
        match self {
            Self::Rate => "rate limit reached",
            Self::Allowance => "usage allowance exhausted",
        }
        .into()
    }
}

impl ProviderRefusalCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Service(value) => value.describe(),
            Self::Held(value) => value.describe(),
        }
    }
}

impl ProviderInstant {
    pub fn describe(&self) -> String {
        format!(
            "{}ns since the Unix epoch",
            i128::from(self.seconds) * 1_000_000_000 + i128::from(self.subsec_nanoseconds)
        )
    }
}

impl ServiceFailure {
    pub fn describe(&self) -> String {
        let cause = self
            .context
            .iter()
            .rev()
            .fold(self.cause.describe(), |inner, context| {
                format!("{}: {inner}", context.describe())
            });
        format!(
            "service {:?} could not {}: {cause}",
            self.service,
            self.operation.describe()
        )
    }
}

impl ServiceOperation {
    pub fn describe(&self) -> String {
        match self {
            Self::Chat => "complete the chat request",
            Self::BuildRefreshClient => "build the credential refresh client",
            Self::RefreshRequest => "send the credential refresh request",
            Self::RefreshResponse => "read the credential refresh response",
        }
        .into()
    }
}

impl ServiceContext {
    pub fn describe(&self) -> String {
        match self {
            Self::GenaiWebAdapterCall(value) => {
                format!("calling the {} adapter", value.describe())
            }
            Self::GenaiWebModelCall(value) => format!("calling {}", value.describe()),
            Self::GenaiWebStream(value) => format!("streaming from {}", value.describe()),
            Self::UnknownWebStreamSource(value) => {
                format!("unknown web stream source: {}", value.describe())
            }
        }
    }
}

impl ServiceCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Genai(value) => value.describe(),
            Self::Webc(value) => value.describe(),
            Self::Reqwest(value) => value.describe(),
            Self::StreamTimeout(value) => format!("timed out {}", value.describe()),
            Self::Utf8(value) => format!("the response is not UTF-8: {}", value.describe()),
            Self::Io(value) => value.describe(),
            Self::Url(value) => format!("invalid provider URL: {}", value.describe()),
            Self::ValueExt(value) => value.describe(),
            Self::Json(value) => format!("invalid provider JSON: {}", value.describe()),
            Self::UnknownWebStreamSource(value) => {
                format!("unknown web stream source: {}", value.describe())
            }
            Self::RefreshResponse(value) => value.describe(),
        }
    }
}

impl ProviderModel {
    pub fn describe(&self) -> String {
        format!("{} model {:?}", self.adapter.describe(), self.name)
    }
}

impl ProviderMessageRole {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl StreamTimeoutKind {
    pub fn describe(&self) -> String {
        match self {
            Self::Headers => "waiting for response headers",
            Self::Read => "waiting for stream data",
        }
        .into()
    }
}

impl GenaiFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::ChatReqHasNoMessages(value) => {
                format!("the chat request for {} has no messages", value.describe())
            }
            Self::LastChatMessageIsNotUser(value) => value.describe(),
            Self::MessageRoleNotSupported(value) => value.describe(),
            Self::MessageContentTypeNotSupported(value) => value.describe(),
            Self::JsonModeWithoutInstruction => {
                "JSON response mode requires an instruction or prompt".to_owned()
            }
            Self::VerbosityParsing(value) => value.describe(),
            Self::ReasoningParsing(value) => value.describe(),
            Self::ServiceTierParsing(value) => value.describe(),
            Self::PromptCacheRetentionParsing(value) => value.describe(),
            Self::NoChatResponse(value) => {
                format!("{} returned no chat response", value.describe())
            }
            Self::InvalidJsonResponseElement(value) => {
                format!("invalid JSON response element: {}", value.describe())
            }
            Self::RequiresApiKey(value) => format!("{} requires an API key", value.describe()),
            Self::NoAuthResolver(value) => format!(
                "no authentication resolver is configured for {}",
                value.describe()
            ),
            Self::NoAuthData(value) => format!(
                "no authentication data is available for {}",
                value.describe()
            ),
            Self::ModelMapperFailed(value) => value.describe(),
            Self::ChatResponseGeneration(value) => value.describe(),
            Self::ChatResponse(value) => value.describe(),
            Self::StreamParse(value) => value.describe(),
            Self::HttpError(value) => value.describe(),
            Self::Resolver(value) => value.describe(),
            Self::AdapterNotSupported(value) => value.describe(),
            Self::CacheBreakpointNoEligibleContent(value) => value.describe(),
            Self::AdapterKindMismatch(value) => value.describe(),
            Self::Internal(value) => format!("provider library failed: {}", value.describe()),
            Self::ClientBuildFail(value) => value.describe(),
            Self::JsonValueExt(value) => value.describe(),
            Self::SerdeJson(value) => format!("invalid JSON: {}", value.describe()),
        }
    }
}

impl ProviderResponseHeader {
    pub fn describe(&self) -> String {
        let bytes: String = self
            .value
            .as_slice()
            .iter()
            .flat_map(|byte| std::ascii::escape_default(*byte))
            .map(char::from)
            .collect();
        format!("{}: \"{bytes}\"", self.name.escape_debug())
    }
}

impl ProviderAdapter {
    pub fn describe(&self) -> String {
        match self {
            Self::OpenAi => "open ai".to_owned(),
            Self::OpenAiResp => "open ai resp".to_owned(),
            Self::Gemini => "gemini".to_owned(),
            Self::GeminiIx => "gemini ix".to_owned(),
            Self::Anthropic => "anthropic".to_owned(),
            Self::Fireworks => "fireworks".to_owned(),
            Self::Together => "together".to_owned(),
            Self::Groq => "groq".to_owned(),
            Self::Aihubmix => "aihubmix".to_owned(),
            Self::Kimi => "kimi".to_owned(),
            Self::Mimo => "mimo".to_owned(),
            Self::Moonshot => "moonshot".to_owned(),
            Self::Nebius => "nebius".to_owned(),
            Self::Xai => "xai".to_owned(),
            Self::DeepSeek => "deep seek".to_owned(),
            Self::Mistral => "mistral".to_owned(),
            Self::Zai => "zai".to_owned(),
            Self::BigModel => "big model".to_owned(),
            Self::Aliyun => "aliyun".to_owned(),
            Self::QwenCloud => "qwen cloud".to_owned(),
            Self::Baidu => "baidu".to_owned(),
            Self::Cohere => "cohere".to_owned(),
            Self::Ollama => "ollama".to_owned(),
            Self::OllamaCloud => "ollama cloud".to_owned(),
            Self::Omlx => "omlx".to_owned(),
            Self::Vertex => "vertex".to_owned(),
            Self::GithubCopilot => "github copilot".to_owned(),
            Self::OpenCodeGo => "open code go".to_owned(),
            Self::BedrockApi => "bedrock api".to_owned(),
            Self::OpenRouter => "open router".to_owned(),
            Self::AtlasCloud => "atlas cloud".to_owned(),
            Self::Requesty => "requesty".to_owned(),
            Self::ApiRoute => "api route".to_owned(),
            Self::Heabsy => "heabsy".to_owned(),
            Self::Runcrate => "runcrate".to_owned(),
            Self::CloudGpu => "cloud gpu".to_owned(),
            Self::Cheapaimodel => "cheapaimodel".to_owned(),
            Self::MiniMax => "mini max".to_owned(),
            Self::Custom(value) => format!("custom: {value}"),
        }
    }
}

impl WebcFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::ResponseFailedNotJson(value) => value.describe(),
            Self::ResponseFailedInvalidJson(value) => value.describe(),
            Self::ResponseFailedStatus(value) => value.describe(),
            Self::JsonValueExt(value) => value.describe(),
        }
    }
}

impl ResolverFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::ApiKeyEnvNotFound(value) => value.describe(),
            Self::AuthDataNotSingleValue => "authentication data must be a single value".to_owned(),
            Self::Custom(value) => value.describe(),
        }
    }
}

impl ValueExtFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Custom(value) => value.describe(),
            Self::PropertyNotFound(value) => value.describe(),
            Self::PropertyValueNotOfType(value) => value.describe(),
            Self::ValueNotOfType(value) => value.describe(),
            Self::SerdeJson(value) => format!("invalid JSON: {}", value.describe()),
        }
    }
}

impl ForeignJsonFailure {
    pub fn describe(&self) -> String {
        self.detail.clone()
    }
}

impl ForeignJsonCategory {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ReqwestFailure {
    pub fn describe(&self) -> String {
        std::iter::once(self.summary.describe())
            .chain(self.sources.iter().map(ReqwestSource::describe))
            .collect::<Vec<_>>()
            .join(": ")
    }
}

impl ReqwestSummary {
    pub fn describe(&self) -> String {
        let url = self
            .url
            .as_ref()
            .map_or_else(String::new, |url| format!(" for {url:?}"));
        let observations = [
            (self.timeout, "timed out"),
            (self.connect, "connection failed"),
            (self.dns, "DNS lookup failed"),
        ]
        .into_iter()
        .filter_map(|(present, text)| present.then_some(text))
        .collect::<Vec<_>>();
        let detail = if observations.is_empty() {
            String::new()
        } else {
            format!(" ({})", observations.join(", "))
        };
        format!("{}{url}{detail}", self.class.describe())
    }
}

impl ReqwestClass {
    pub fn describe(&self) -> String {
        match self {
            Self::Builder => "could not build the HTTP request".to_owned(),
            Self::Redirect => "HTTP redirect failed".to_owned(),
            Self::Status(value) => format!("HTTP request failed with status {value}"),
            Self::Request => "HTTP request failed".to_owned(),
            Self::Body => "could not read the HTTP body".to_owned(),
            Self::Decode => "could not decode the HTTP response".to_owned(),
            Self::Upgrade => "HTTP connection upgrade failed".to_owned(),
            Self::Unclassified(value) => format!("unclassified: {}", value.describe()),
        }
    }
}

impl ReqwestSource {
    pub fn describe(&self) -> String {
        match self {
            Self::Reqwest(value) => value.describe(),
            Self::Io(value) => value.describe(),
            Self::Json(value) => format!("invalid JSON: {}", value.describe()),
            Self::Url(value) => value.describe(),
            Self::Utf8(value) => value.describe(),
            Self::Foreign(value) => value.describe(),
        }
    }
}

impl ProviderUrlFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::EmptyHost => "URL host is empty".to_owned(),
            Self::Idna => "invalid international domain name".to_owned(),
            Self::InvalidPort => "invalid URL port: expected an integer from 0 to 65535".to_owned(),
            Self::InvalidIpv4Address => "invalid IPv4 address".to_owned(),
            Self::InvalidIpv6Address => "invalid IPv6 address".to_owned(),
            Self::InvalidDomainCharacter => "URL domain contains an invalid character".to_owned(),
            Self::RelativeWithoutBase => "a relative URL requires a base URL".to_owned(),
            Self::RelativeWithCannotBeABaseBase => {
                "cannot resolve a relative URL against this base".to_owned()
            }
            Self::SetHostOnCannotBeABaseUrl => {
                "cannot set a host on a URL that cannot be a base".to_owned()
            }
            Self::Overflow => "URLs larger than 4 GB are not supported".to_owned(),
            Self::Unclassified(value) => format!("unclassified: {}", value.describe()),
        }
    }
}

impl CredentialFailure {
    pub fn describe(&self) -> String {
        format!(
            "{} account {:?}: {}",
            self.service.describe(),
            self.account,
            self.cause.describe()
        )
    }
}

impl CredentialService {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl CredentialCause {
    pub fn describe(&self) -> String {
        match self {
            Self::OwnerPanicked => "the credential owner panicked; a refresh may have rotated the credential; sign in again".to_owned(),
            Self::OwnerStopped => "the credential owner stopped before reporting; the refresh outcome is unknown; sign in again".to_owned(),
            Self::Removed => "the credential was removed from the store; sign in again".to_owned(),
            Self::UnresolvedRotation => "a previous refresh may have rotated this credential; sign in again".to_owned(),
            Self::Store(value) => value.describe(),
            Self::LeaseTimedOut => "timed out waiting for the credential store; another process may be holding it".to_owned(),
            Self::PreparationDeadline => "credential preparation timed out before sending a refresh request; retrying is safe".to_owned(),
            Self::Refresh(value) => value.describe(),
        }
    }
}

impl CredentialStoreOperation {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl RefreshOutcome {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl RefreshFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::TimedOut(value) => {
                format!("timed out: {}.{:09} seconds", value.seconds, value.nanos)
            }
            Self::Service(value) => value.describe(),
            Self::Response(value) => value.describe(),
            Self::MissingToken(value) => format!("missing token: {}", value.describe()),
            Self::EmptyToken(value) => format!("empty token: {}", value.describe()),
            Self::InvalidExpiry(value) => format!("invalid expiry: {}", value.describe()),
            Self::ExpiryOverflow(value) => format!("expiry overflow: {}", value.describe()),
        }
    }
}

impl RefreshTokenField {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl RefreshServiceResponse {
    pub fn describe(&self) -> String {
        format!(
            "credential refresh returned HTTP {}: {}",
            self.status,
            self.body.describe()
        )
    }
}

impl RefreshResponseBody {
    pub fn describe(&self) -> String {
        match self {
            Self::Detail(value) => value.describe(),
            Self::ReadFailed(value) => format!("could not read the response: {}", value.describe()),
        }
    }
}

impl RefreshResponseDetail {
    pub fn describe(&self) -> String {
        match self {
            Self::Text(text) => text.describe(),
            Self::Html => "HTML response".into(),
            Self::Empty => "empty response".into(),
        }
    }
}

impl HelperFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::NoReply => "ended without calling `reply`".to_owned(),
            Self::EndedBeforeReporting => "ended before reporting".to_owned(),
            Self::Cancelled => "cancelled".to_owned(),
            Self::ProviderStopped(value) => format!("provider stopped: {}", value.describe()),
            Self::Provider(value) => format!("provider: {}", value.describe()),
            Self::Connection(value) => format!("engine connection failed: {}", value.describe()),
            Self::Context(value) => format!("context: {}", value.describe()),
            Self::Recording(value) => value.describe(),
            Self::Panicked => "panicked".to_owned(),
        }
    }
}

impl ProviderStop {
    pub fn describe(&self) -> String {
        match self {
            Self::Completed(value) => {
                format!("completed: {}", ServiceFailure::response_summary(value))
            }
            Self::MaxTokens(value) => {
                format!("max tokens: {}", ServiceFailure::response_summary(value))
            }
            Self::ToolCall(value) => {
                format!("tool call: {}", ServiceFailure::response_summary(value))
            }
            Self::ContentFilter(value) => format!(
                "content filter: {}",
                ServiceFailure::response_summary(value)
            ),
            Self::StopSequence(value) => {
                format!("stop sequence: {}", ServiceFailure::response_summary(value))
            }
            Self::Unclassified(value) => {
                format!("unclassified: {}", ServiceFailure::response_summary(value))
            }
        }
    }
}

impl WithdrawalEvidence {
    pub fn describe(&self) -> String {
        match self {
            Self::Unavailable => "unavailable".to_owned(),
            Self::EarliestRetained(value) => format!("earliest retained: {value}"),
            Self::NothingRetained => "nothing retained".to_owned(),
        }
    }
}

impl BootFailureProfileNotAbsolute {
    pub fn describe(&self) -> String {
        format!("the --capabilities profile {:?} is not absolute", self.path)
    }
}

impl EditorFailureUnknownStyle {
    pub fn describe(&self) -> String {
        format!(
            "unknown style '{}'; available styles: {}",
            self.style.escape_debug(),
            self.offered
                .iter()
                .map(|style| format!("'{}'", style.escape_debug()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl EditorFailureCommand {
    pub fn describe(&self) -> String {
        let variable = self.variable.describe();
        match self.cause {
            EditorCommandCause::Nul => format!("{variable} contains a NUL byte"),
            EditorCommandCause::Unbalanced => {
                format!("{variable} has an unmatched quote or a trailing backslash")
            }
            EditorCommandCause::NoProgram => {
                format!("{variable} names no program: its first word is empty")
            }
        }
    }
}

impl EditorFailureDraft {
    pub fn describe(&self) -> String {
        format!(
            "{} draft: {}",
            self.operation.describe(),
            self.cause.describe()
        )
    }
}

impl EditorFailureWorkspace {
    pub fn describe(&self) -> String {
        let path = self
            .path
            .as_ref()
            .map_or_else(String::new, |path| format!(" {path}"));
        format!(
            "could not {}{path}: {}",
            self.step.describe(),
            self.cause.describe()
        )
    }
}

impl DraftFailureTooLarge {
    pub fn describe(&self) -> String {
        let bytes = self.limit.to_u64();
        let limit = if bytes.is_multiple_of(1 << 20) {
            format!("{} MiB", bytes >> 20)
        } else {
            format!("{bytes} bytes")
        };
        format!("the draft exceeds the {limit} limit")
    }
}

impl EditorWorkspaceFailureExpectedRegularFile {
    pub fn describe(&self) -> String {
        let found = match self.found {
            None => return "the draft file is missing".into(),
            Some(EntryKind::File) => "a regular file",
            Some(EntryKind::Dir) => "a directory",
            Some(EntryKind::Symlink) => "a symlink",
            Some(EntryKind::Other) => "another kind of non-regular file",
        };
        format!("expected a regular draft file, got {found}")
    }
}

impl PluginFailureAlreadyLoaded {
    pub fn describe(&self) -> String {
        format!("plugin '{}' is already loaded", self.name.escape_debug())
    }
}

impl PluginFailureNotFound {
    pub fn describe(&self) -> String {
        format!("plugin '{}' was not found", self.requested.escape_debug())
    }
}

impl PluginFailureOptionsWithoutFactory {
    pub fn describe(&self) -> String {
        format!(
            "plugin '{}' takes no configuration",
            self.plugin.escape_debug()
        )
    }
}

impl PluginFailureFactoryReturnedBlock {
    pub fn describe(&self) -> String {
        format!(
            "plugin '{}' returned a Block as its manifest",
            self.plugin.escape_debug()
        )
    }
}

impl PluginFailureUnknownHookEvent {
    pub fn describe(&self) -> String {
        format!("unknown hook event '{}'", self.event.escape_debug())
    }
}

impl PluginFailureHandlerInstallationEscaped {
    pub fn describe(&self) -> String {
        format!(
            "installation of handler '{}' escaped: {}",
            self.handler.escape_debug(),
            self.cause.describe()
        )
    }
}

impl KeybindingFailureNotation {
    pub fn describe(&self) -> String {
        format!(
            "keybinding '{}': {}",
            self.key.escape_debug(),
            self.cause.describe()
        )
    }
}

impl KeybindingFailureReservedAction {
    pub fn describe(&self) -> String {
        format!(
            "keybinding '{}' is reserved for {} and cannot be bound by a plugin",
            self.key.escape_debug(),
            self.action.describe()
        )
    }
}

impl KeybindingFailureSequencePrefix {
    pub fn describe(&self) -> String {
        format!(
            "keybinding '{}' does not begin with plain ctrl-x",
            self.key.escape_debug()
        )
    }
}

impl KeybindingFailureGuardRegex {
    pub fn describe(&self) -> String {
        format!(
            "keybinding '{}': invalid guard regex {:?}: {}",
            self.key.escape_debug(),
            self.pattern,
            self.cause.describe()
        )
    }
}

impl KeybindingFailureUnguardedBuiltinShadow {
    pub fn describe(&self) -> String {
        format!(
            "unguarded keybinding '{}' would shadow the built-in {} on every press",
            self.key.escape_debug(),
            self.action.describe()
        )
    }
}

impl KeyNotationFailureDuplicateModifier {
    pub fn describe(&self) -> String {
        format!(
            "modifier '{}' appears twice in '{}'",
            self.modifier.describe(),
            self.chord.escape_debug()
        )
    }
}

impl KeyNotationFailureUnknownKey {
    pub fn describe(&self) -> String {
        format!("'{}' is not a key", self.token.escape_debug())
    }
}

impl KeyNotationFailureControlCharacter {
    pub fn describe(&self) -> String {
        format!("'{}' is a control character", self.character.escape_debug())
    }
}

impl KeyNotationFailureShiftedCharacter {
    pub fn describe(&self) -> String {
        format!(
            "'{}' shifts a character, and the shifted character is itself the key",
            self.chord.escape_debug()
        )
    }
}

impl KeyNotationFailureUppercaseControl {
    pub fn describe(&self) -> String {
        format!(
            "'{}' is the same key to a terminal as its lowercase form",
            self.chord.escape_debug()
        )
    }
}

impl KeyNotationFailureTerminalAlias {
    pub fn describe(&self) -> String {
        format!(
            "'{}' is {} to a terminal",
            self.chord.escape_debug(),
            self.alias.describe()
        )
    }
}

impl PolicyFailureUnknownBase {
    pub fn describe(&self) -> String {
        let name = self.name.escape_debug();
        if self.offered.is_empty() {
            format!("unknown base '{name}'; no bases were offered")
        } else {
            format!(
                "unknown base '{name}'; expected one of: {}",
                self.offered
                    .iter()
                    .map(|name| name.escape_debug().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

impl PolicyFailureEscapedLoad {
    pub fn describe(&self) -> String {
        match &self.cause {
            EscapedLoad::Exit(status) => {
                format!("policy load for {} exited with status {status}", self.flag)
            }
            EscapedLoad::Stopped(cause) => format!(
                "policy load for {} stopped: {}",
                self.flag,
                <&str>::from(*cause)
            ),
        }
    }
}

impl AgentFailureNameTaken {
    pub fn describe(&self) -> String {
        format!(
            "an agent already bears the name '{}'",
            self.name.escape_debug()
        )
    }
}

impl AgentFailureUnknownProviderRole {
    pub fn describe(&self) -> String {
        if self.offered.is_empty() {
            format!(
                "no role '{}': no roles are configured, so every child runs on your own model",
                self.name.escape_debug()
            )
        } else {
            format!(
                "no role '{}': the configured roles are {}",
                self.name.escape_debug(),
                self.offered
                    .iter()
                    .map(|name| format!("'{}'", name.escape_debug()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

impl AgentFailureNotDescendant {
    pub fn describe(&self) -> String {
        format!(
            "agent '{}' is not a descendant of yours; exarch-agents `stop may only reach a descendant of yours",
            self.name.escape_debug()
        )
    }
}

impl AgentFailureNotOwned {
    pub fn describe(&self) -> String {
        format!(
            "agent '{}' is not one of your own helpers",
            self.name.escape_debug()
        )
    }
}

impl AgentFailureSelfMessage {
    pub fn describe(&self) -> String {
        format!(
            "agent '{}' is you; exarch-agents `message reaches another agent",
            self.name.escape_debug()
        )
    }
}

impl AgentFailureNoLiveAgent {
    pub fn describe(&self) -> String {
        format!(
            "no live agent named '{}'; did it finish already?",
            self.name.escape_debug()
        )
    }
}

impl AgentFailureEnded {
    pub fn describe(&self) -> String {
        format!(
            "agent '{}' has ended; nothing was queued",
            self.name.escape_debug()
        )
    }
}

impl AgentFailureResting {
    pub fn describe(&self) -> String {
        format!(
            "agent '{}' has answered '{}' and rests; only '{}' can message it now; nothing was queued",
            self.name.escape_debug(),
            self.parent.escape_debug(),
            self.parent.escape_debug()
        )
    }
}

impl AgentFailureReplyPending {
    pub fn describe(&self) -> String {
        format!(
            "agent '{}' has not replied yet; it is still working",
            self.name.escape_debug()
        )
    }
}

impl AgentFailureReplyAbsent {
    pub fn describe(&self) -> String {
        format!("agent '{}' has no reply to read", self.name.escape_debug())
    }
}

impl AgentFailureReplyStopped {
    pub fn describe(&self) -> String {
        format!(
            "agent '{}' stopped without a reply to read",
            self.name.escape_debug()
        )
    }
}

impl AgentFailureReplyFailed {
    pub fn describe(&self) -> String {
        format!(
            "agent '{}' failed without a reply to read: {}",
            self.name.escape_debug(),
            self.cause.describe()
        )
    }
}

impl AgentFailureReplyUnavailable {
    pub fn describe(&self) -> String {
        format!(
            "agent '{}' has ended and its state is unavailable: {}",
            self.name.escape_debug(),
            self.cause.describe()
        )
    }
}

impl AgentFailureQueuedUnsynced {
    pub fn describe(&self) -> String {
        format!(
            "message to {:?} was queued, but recording failed; it may already have been read: {}",
            self.name,
            self.cause.describe()
        )
    }
}

impl RecordingFailureRecording {
    pub fn describe(&self) -> String {
        let subject = match &self.record {
            Some(path) => format!("record '{}'", path.escape_debug()),
            None => "recording".into(),
        };
        format!(
            "session ended because {subject} {} failed: {}",
            self.stage.describe(),
            self.cause.describe()
        )
    }
}

impl RecordingFailureCellPoisoned {
    pub fn describe(&self) -> String {
        format!(
            "session ended because agent {}'s state cell is poisoned",
            self.agent
        )
    }
}

impl ContextFailureTurnNotRecorded {
    pub fn describe(&self) -> String {
        match self.reach {
            Some(reach) => format!("turn {} is not recorded: the latest is {reach}", self.turn),
            None => format!(
                "turn {} is not recorded: nothing has been recorded yet",
                self.turn
            ),
        }
    }
}

impl ContextFailureWithdrawn {
    pub fn describe(&self) -> String {
        let turn = self.turn;
        match self.evidence {
            WithdrawalEvidence::Unavailable => {
                format!("turn {turn} was rewound or cleared away: it is no longer in your context")
            }
            WithdrawalEvidence::EarliestRetained(reach) => format!(
                "turn {turn} was already rewound or cleared away; the earliest you can rewind to is turn {reach}"
            ),
            WithdrawalEvidence::NothingRetained => format!(
                "turn {turn} was already rewound or cleared away; nothing has been said since the last /clear or /rewind"
            ),
        }
    }
}

impl ContextFailureCurrentlyWritten {
    pub fn describe(&self) -> String {
        format!("turn {} is being written now", self.turn)
    }
}

impl ContextFailureStraddledEviction {
    pub fn describe(&self) -> String {
        format!(
            "{} cuts the eviction over {}; the covered turns stand together under one note",
            self.requested.describe(),
            self.covering.describe()
        )
    }

    pub fn advice(&self) -> Vec<String> {
        let TurnSpan { from, to } = self.covering;
        vec![
            format!("for an eviction, name from: {from}, to: {to} whole, or leave it"),
            match to.to_u64().checked_add(1) {
                Some(next) => format!(
                    "to rewind around this note, use its boundaries: turn {from} or turn {next}"
                ),
                None => format!("to rewind around this note, use its first boundary: turn {from}"),
            },
        ]
    }
}

impl ContextFailureReadBack {
    pub fn describe(&self) -> String {
        format!(
            "turn {} could not be read back from {} (source agent {}): {}",
            self.turn,
            self.record,
            self.source,
            self.cause.describe()
        )
    }
}

impl ContextFailureMutation {
    pub fn describe(&self) -> String {
        let state = self.state.describe();
        match self.operation {
            ContextOperation::RenderRequest => {
                format!("cannot render request while the session is {state}")
            }
            ContextOperation::Import => {
                format!("cannot import context while the session is {state}")
            }
            ContextOperation::AcceptUser => {
                format!("cannot accept a new user prompt while the session is {state}")
            }
            ContextOperation::AcceptSteering => match &self.state {
                ContextState::ReadyForUser => format!(
                    "cannot accept a steering prompt while the session is {state}; send a new user prompt"
                ),
                ContextState::AwaitingToolResults(_) => format!(
                    "tool results must be complete before accepting a steering prompt; the session is {state}"
                ),
                _ => format!("cannot accept a steering prompt while the session is {state}"),
            },
            ContextOperation::AppendAssistant => {
                format!("assistant message is not expected while the session is {state}")
            }
            ContextOperation::AppendToolResults => {
                format!("tool results are not expected while the session is {state}")
            }
            ContextOperation::Clear => format!("cannot clear while the session is {state}"),
            ContextOperation::Rewind => format!("cannot rewind while the session is {state}"),
        }
    }
}

impl ContextFailureAssistantRole {
    pub fn describe(&self) -> String {
        format!(
            "a {} message arrived where the assistant's reply was expected",
            self.found.describe()
        )
    }
}

impl ContextFailureToolResultCount {
    pub fn describe(&self) -> String {
        format!(
            "tool result count mismatch: expected {}, got {}",
            self.expected, self.got
        )
    }
}

impl ContextFailureUnknownToolResult {
    pub fn describe(&self) -> String {
        format!("unknown tool result id {}", self.id.escape_debug())
    }
}

impl ContextFailureDuplicateToolResult {
    pub fn describe(&self) -> String {
        format!("duplicate tool result id {}", self.id.escape_debug())
    }
}

impl ContextStateAwaitingToolResults {
    pub fn describe(&self) -> String {
        format!(
            "waiting for {} tool result{}",
            self.pending,
            if self.pending.to_u64() == 1 { "" } else { "s" }
        )
    }
}

impl ScheduleFailureNoGrant {
    pub fn describe(&self) -> String {
        format!("{}: scheduling is not allowed in this session", self.verb)
    }
}

impl ScheduleFailureNoFutureOccurrence {
    pub fn describe(&self) -> String {
        format!(
            "trigger '{}' has no next occurrence within the supported time/search horizon",
            self.trigger.escape_debug()
        )
    }
}

impl ScheduleFailureDuplicateLiveLabel {
    pub fn describe(&self) -> String {
        format!(
            "label '{}' is already borne by a live schedule",
            self.label.escape_debug()
        )
    }
}

impl ScheduleFailureRecording {
    pub fn describe(&self) -> String {
        let mut description = "could not record schedule".to_owned();
        if let Some(label) = &self.label {
            let _ = write!(description, " '{}'", label.escape_debug());
        }
        if let Some(trigger) = &self.trigger {
            let _ = write!(description, " for trigger '{}'", trigger.escape_debug());
        }
        format!("{description}: {}", self.cause.describe())
    }
}

impl EditFailureHashAbsent {
    pub fn describe(&self) -> String {
        format!("no line in {:?} hashes to {:?}", self.path, self.hash)
    }
}

impl EditFailureHashAmbiguous {
    pub fn describe(&self) -> String {
        format!(
            "hash {:?} matches multiple lines in {:?}: {}",
            self.hash,
            self.path,
            self.lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl EditFailureDuplicateTargetLine {
    pub fn describe(&self) -> String {
        format!("two edits name line {} in {:?}", self.line, self.path)
    }
}

impl EditFailureMultipleLiteralMatches {
    pub fn describe(&self) -> String {
        format!(
            "FROM matches {} times in {:?}, on {} {}; must match exactly once",
            self.matches,
            self.path,
            if self.lines.len() == 1 {
                "line"
            } else {
                "lines"
            },
            self.lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl EditFailureNoLiteralMatch {
    pub fn describe(&self) -> String {
        let path = format!("{:?}", self.path);
        match &self.evidence {
            LiteralMiss::Escapes => {
                format!("FROM was not found in {path}, and FROM carries a literal backslash escape")
            }
            LiteralMiss::Indentation(line) => format!(
                "FROM was not found in {path}, but line {line} matches FROM's first non-empty line apart from leading whitespace"
            ),
            LiteralMiss::Absent => format!("FROM was not found in {path}"),
        }
    }
}

fn describe_invalid_skill_name(name: &str) -> String {
    format!(
        "'{}' is not a skill name; use 1–64 lowercase ASCII letters or digits, with single interior hyphens",
        name.escape_debug()
    )
}

impl SkillFailureInvalidName {
    pub fn describe(&self) -> String {
        describe_invalid_skill_name(&self.name)
    }
}

impl SkillFailureMissing {
    pub fn describe(&self) -> String {
        format!("no skill named '{}'", self.name.escape_debug())
    }
}

impl SkillFailureRead {
    pub fn describe(&self) -> String {
        format!(
            "could not read skill '{}' from {}: {}",
            self.name.escape_debug(),
            self.place.describe(),
            self.cause.describe()
        )
    }
}

impl SkillFailureYaml {
    pub fn describe(&self) -> String {
        format!(
            "could not load skill '{}' from {}: invalid YAML frontmatter: {}",
            self.name.escape_debug(),
            self.place.describe(),
            self.detail
        )
    }
}

impl SkillFailureFrontmatter {
    pub fn describe(&self) -> String {
        format!(
            "could not load skill '{}' from {}: {}",
            self.name.escape_debug(),
            self.place.describe(),
            self.cause.describe()
        )
    }
}

impl FrontmatterFailureNotMapping {
    pub fn describe(&self) -> String {
        format!(
            "YAML frontmatter must be a mapping, got {}",
            self.found.describe()
        )
    }
}

impl FrontmatterFailureFieldShape {
    pub fn describe(&self) -> String {
        format!(
            "frontmatter field {} must be a String, got {}",
            self.field.describe(),
            self.found.describe()
        )
    }
}

impl FrontmatterFailureInvalidName {
    pub fn describe(&self) -> String {
        describe_invalid_skill_name(&self.name)
    }
}

impl FrontmatterFailureDirectoryNameMismatch {
    pub fn describe(&self) -> String {
        format!(
            "frontmatter name '{}' must match directory name '{}'",
            self.declared.escape_debug(),
            self.directory.escape_debug()
        )
    }
}

impl EngineFactoryFailureUnsupported {
    pub fn describe(&self) -> String {
        format!(
            "the {} backend does not support connecting to a guest socket",
            self.backend.describe()
        )
    }
}

impl EngineFactoryFailureDialTimedOut {
    pub fn describe(&self) -> String {
        format!(
            "no guest connection result arrived for port {} within {}.{:09} seconds",
            self.port, self.within.seconds, self.within.nanos
        )
    }
}

impl EngineFactoryFailureDialWaitDisconnected {
    pub fn describe(&self) -> String {
        format!(
            "the guest connection result channel disconnected while waiting for port {}",
            self.port
        )
    }
}

impl EngineFactoryFailurePlatform {
    pub fn describe(&self) -> String {
        format!(
            "{} refused the guest connection: {} error {}: {}",
            self.backend.describe(),
            self.domain,
            self.code,
            self.detail
        )
    }
}

impl RecordReadFailureMissingLocusRecord {
    pub fn describe(&self) -> String {
        format!(
            "a locus named record {} of line {}, which holds fewer: the log at this path is no longer the file those byte ranges were measured in; it may be a copied session directory or an edited log",
            self.index, self.commit
        )
    }
}

impl RecordReadFailureLocusSealMismatch {
    pub fn describe(&self) -> String {
        format!(
            "line {} did not hash to the locus that named it: the log at this path is no longer the file those byte ranges were measured in; it may be a copied session directory or an edited log",
            self.commit
        )
    }
}

impl RecordReadFailureLine {
    pub fn describe(&self) -> String {
        format!("line {} {}", self.commit, self.cause.describe())
    }
}

impl RecordLineFailureSealMismatch {
    pub fn describe(&self) -> String {
        format!(
            "is corrupt: its commit hashes to {:?} but was sealed as {:?}, so the file changed after it was written (edited by hand, or damaged on disk); the lines before it are intact, and nothing from it on can be read.",
            self.actual, self.sealed
        )
    }
}

impl ProviderLeafTransient {
    pub fn describe(&self) -> String {
        format!(
            "provider request failed after {}: {}",
            crate::text::plural(self.attempts.to_u64(), "attempt"),
            self.cause.describe()
        )
    }
}

impl ProviderLeafApi {
    pub fn describe(&self) -> String {
        format!(
            "API request for model {:?} failed: {}",
            self.model,
            self.cause.describe()
        )
    }
}

impl ProviderLeafOverflow {
    pub fn describe(&self) -> String {
        let mut message = format!("context too long for model {:?}", self.model);
        if let Some(provider) = &self.provider {
            let _ = write!(message, " on {provider:?}");
        }
        if let Some(sent) = self.sent {
            let _ = write!(message, "; {sent} tokens sent");
        }
        if let Some(limit) = self.limit {
            let _ = write!(message, "; {limit} tokens allowed");
        }
        if let Some(listed) = self.listed.filter(|listed| Some(*listed) != self.limit) {
            let _ = write!(
                message,
                "; previously listed context window: {listed} tokens"
            );
        }
        format!("{message}: {}", self.cause.describe())
    }
}

impl ProviderLeafOutputCap {
    pub fn describe(&self) -> String {
        format!(
            "output cap ({})",
            ServiceFailure::response_summary(&self.stop_reason)
        )
    }
}

impl RetryableProviderCauseStreamEndedWithoutEnd {
    pub fn describe(&self) -> String {
        format!(
            "service {:?} ended the stream for model {:?} without a completion event",
            self.service, self.model
        )
    }
}

impl ProviderRefusalCauseHeld {
    pub fn describe(&self) -> String {
        format!(
            "request to service {:?} for model {:?} was not sent because of an earlier limit",
            self.service, self.model
        )
    }
}

impl ServiceContextUnknownWebStreamSource {
    pub fn describe(&self) -> String {
        ServiceFailure::response_summary(&self.detail)
    }
}

impl ServiceCauseUnknownWebStreamSource {
    pub fn describe(&self) -> String {
        ServiceFailure::response_summary(&self.detail)
    }
}

impl GenaiFailureLastChatMessageIsNotUser {
    pub fn describe(&self) -> String {
        format!(
            "{} requires a final user message, but the final message has role {}",
            self.model.describe(),
            self.actual_role.describe()
        )
    }
}

impl GenaiFailureMessageRoleNotSupported {
    pub fn describe(&self) -> String {
        format!(
            "{} does not support the {} message role",
            self.model.describe(),
            self.role.describe()
        )
    }
}

impl GenaiFailureMessageContentTypeNotSupported {
    pub fn describe(&self) -> String {
        format!(
            "{} does not support this message content: {}",
            self.model.describe(),
            self.detail
        )
    }
}

impl GenaiFailureVerbosityParsing {
    pub fn describe(&self) -> String {
        format!(
            "invalid verbosity {:?}; expected low, medium, or high",
            self.actual
        )
    }
}

impl GenaiFailureReasoningParsing {
    pub fn describe(&self) -> String {
        format!(
            "invalid reasoning setting {:?}; expected zero, none, low, medium, high, xhigh, max, minimal, or a token budget from 0 to 4294967295",
            self.actual
        )
    }
}

impl GenaiFailureServiceTierParsing {
    pub fn describe(&self) -> String {
        format!(
            "invalid service tier {:?}; expected flex, auto, or default",
            self.actual
        )
    }
}

impl GenaiFailurePromptCacheRetentionParsing {
    pub fn describe(&self) -> String {
        format!("invalid prompt-cache retention {:?}", self.actual)
    }
}

impl GenaiFailureInvalidJsonResponseElement {
    pub fn describe(&self) -> String {
        self.detail.clone()
    }
}

impl GenaiFailureModelMapperFailed {
    pub fn describe(&self) -> String {
        format!(
            "could not map {}: {}",
            self.model.describe(),
            self.cause.describe()
        )
    }
}

impl GenaiFailureChatResponseGeneration {
    pub fn describe(&self) -> String {
        format!(
            "could not generate a chat response for {}: {}; {}",
            self.model.describe(),
            ServiceFailure::response_summary(&self.detail),
            ServiceFailure::response_detail(&self.response_body)
        )
    }
}

impl GenaiFailureChatResponse {
    pub fn describe(&self) -> String {
        format!(
            "{} returned an error: {}",
            self.model.describe(),
            ServiceFailure::response_detail(&self.body)
        )
    }
}

impl GenaiFailureStreamParse {
    pub fn describe(&self) -> String {
        format!(
            "could not parse the stream from {}: {}",
            self.model.describe(),
            self.cause.describe()
        )
    }
}

impl GenaiFailureHttpError {
    pub fn describe(&self) -> String {
        describe_http_response(self.status, &self.body, &self.headers)
    }
}

impl GenaiFailureResolver {
    pub fn describe(&self) -> String {
        format!(
            "could not resolve authentication for {}: {}",
            self.model.describe(),
            self.cause.describe()
        )
    }
}

impl GenaiFailureAdapterNotSupported {
    pub fn describe(&self) -> String {
        format!(
            "the {} adapter does not support {:?}",
            self.adapter.describe(),
            self.feature
        )
    }
}

impl GenaiFailureCacheBreakpointNoEligibleContent {
    pub fn describe(&self) -> String {
        format!(
            "{} has no eligible content for a cache breakpoint in {:?}",
            self.model.describe(),
            self.scope
        )
    }
}

impl GenaiFailureAdapterKindMismatch {
    pub fn describe(&self) -> String {
        format!(
            "the client is bound to {}, but model {:?} resolved to {}; remove the model's adapter namespace or use a client without a fixed adapter",
            self.bound.describe(),
            self.model,
            self.requested.describe()
        )
    }
}

impl GenaiFailureInternal {
    pub fn describe(&self) -> String {
        self.detail.clone()
    }
}

impl GenaiFailureClientBuildFail {
    pub fn describe(&self) -> String {
        format!("could not build the provider client: {}", self.detail)
    }
}

impl WebcFailureResponseFailedNotJson {
    pub fn describe(&self) -> String {
        format!(
            "expected a JSON response, got content type {:?}: {}",
            self.content_type,
            ServiceFailure::response_detail(&self.body)
        )
    }
}

impl WebcFailureResponseFailedInvalidJson {
    pub fn describe(&self) -> String {
        format!(
            "the response is not valid JSON: {}; {}",
            ServiceFailure::response_summary(&self.detail),
            ServiceFailure::response_detail(&self.body)
        )
    }
}

impl WebcFailureResponseFailedStatus {
    pub fn describe(&self) -> String {
        describe_http_response(self.status, &self.body, &self.headers)
    }
}

fn describe_http_response(status: u32, body: &str, headers: &[ProviderResponseHeader]) -> String {
    let headers = if headers.is_empty() {
        String::new()
    } else {
        format!(
            "; response headers: {}",
            headers
                .iter()
                .map(ProviderResponseHeader::describe)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "HTTP {status}: {}{headers}",
        ServiceFailure::response_detail(body)
    )
}

impl ResolverFailureApiKeyEnvNotFound {
    pub fn describe(&self) -> String {
        format!(
            "API key environment variable {:?} is not set",
            self.env_name
        )
    }
}

impl ResolverFailureCustom {
    pub fn describe(&self) -> String {
        self.detail.clone()
    }
}

impl ValueExtFailureCustom {
    pub fn describe(&self) -> String {
        self.detail.clone()
    }
}

impl ValueExtFailurePropertyNotFound {
    pub fn describe(&self) -> String {
        format!("JSON property {:?} was not found", self.name)
    }
}

impl ValueExtFailurePropertyValueNotOfType {
    pub fn describe(&self) -> String {
        format!(
            "JSON property {:?} must be {}",
            self.name, self.required_library_type
        )
    }
}

impl ValueExtFailureValueNotOfType {
    pub fn describe(&self) -> String {
        format!("JSON value must be {}", self.required_library_type)
    }
}

impl ReqwestClassUnclassified {
    pub fn describe(&self) -> String {
        self.detail.clone()
    }
}

impl ReqwestSourceForeign {
    pub fn describe(&self) -> String {
        self.detail.clone()
    }
}

impl ProviderUrlFailureUnclassified {
    pub fn describe(&self) -> String {
        self.detail.clone()
    }
}

impl CredentialCauseStore {
    pub fn describe(&self) -> String {
        let step = match self.operation {
            CredentialStoreOperation::OpenLease => "could not open the credential store lease",
            CredentialStoreOperation::LockLease => "could not lock the credential store lease",
            CredentialStoreOperation::Read => "could not read the credential store",
            CredentialStoreOperation::PersistSuccessor
            | CredentialStoreOperation::PersistFreshRotation => {
                "the credential rotated, but could not be saved"
            }
        };
        format!("{step}: {}", self.cause.describe())
    }
}

impl CredentialCauseRefresh {
    pub fn describe(&self) -> String {
        let outcome = match self.outcome {
            RefreshOutcome::NotSubmitted => "refresh request was not sent; retrying is safe",
            RefreshOutcome::Rejected => {
                "the issuer rejected the refresh request without rotating the credential"
            }
            RefreshOutcome::Ambiguous => {
                "could not confirm the refresh; the issuer may have rotated the credential; sign in again"
            }
        };
        format!("{outcome}: {}", self.cause.describe())
    }
}

impl RefreshFailureResponse {
    pub fn describe(&self) -> String {
        format!(
            "invalid credential refresh response (HTTP {}): {}",
            self.status,
            self.cause.describe()
        )
    }
}

impl RefreshFailureInvalidExpiry {
    pub fn describe(&self) -> String {
        format!(
            "credential lifetime must be positive, got {} seconds",
            self.seconds
        )
    }
}

impl RefreshFailureExpiryOverflow {
    pub fn describe(&self) -> String {
        format!(
            "credential lifetime of {} seconds overflows the expiry timestamp from {} seconds since the Unix epoch",
            self.seconds, self.now
        )
    }
}

impl RefreshResponseBodyDetail {
    pub fn describe(&self) -> String {
        match &self.code {
            Some(code) => format!("{} (code {code:?})", self.detail.describe()),
            None => self.detail.describe(),
        }
    }
}

impl RefreshResponseDetailText {
    pub fn describe(&self) -> String {
        format!("{}{}", self.preview, if self.truncated { "…" } else { "" })
    }
}

impl ProviderFailure {
    pub fn new(leaf: ProviderLeaf) -> Self {
        Self {
            context: Vec::new(),
            leaf,
        }
    }

    pub fn at(mut self, context: ProviderContext) -> Self {
        self.context.insert(0, context);
        self
    }

    pub fn advice(&self) -> Vec<String> {
        match &self.leaf {
            ProviderLeaf::Credential(credential) => credential.advice(),
            ProviderLeaf::Unavailable(recording) => recording.advice(),
            _ => Vec::new(),
        }
    }
}

impl ServiceFailure {
    pub fn at(mut self, context: ServiceContext) -> Self {
        self.context.insert(0, context);
        self
    }

    pub fn status(&self) -> Option<u32> {
        self.cause.status()
    }

    pub fn body(&self) -> Option<&str> {
        self.cause.body()
    }

    pub fn endpoint(&self) -> Option<&str> {
        match self.cause.as_ref() {
            ServiceCause::Reqwest(cause) => cause.summary.url.as_deref(),
            ServiceCause::RefreshResponse(response) => match &response.body {
                RefreshResponseBody::ReadFailed(cause) => cause.summary.url.as_deref(),
                RefreshResponseBody::Detail(_) => None,
            },
            ServiceCause::Genai(_)
            | ServiceCause::Webc(_)
            | ServiceCause::StreamTimeout(_)
            | ServiceCause::Utf8(_)
            | ServiceCause::Io(_)
            | ServiceCause::Url(_)
            | ServiceCause::ValueExt(_)
            | ServiceCause::Json(_)
            | ServiceCause::UnknownWebStreamSource(_) => None,
        }
    }
}

impl ServiceCause {
    pub fn status(&self) -> Option<u32> {
        match self {
            Self::Genai(GenaiFailure::HttpError(response)) => Some(response.status),
            Self::Webc(WebcFailure::ResponseFailedStatus(response)) => Some(response.status),
            Self::Reqwest(cause) => match cause.summary.class {
                ReqwestClass::Status(status) => Some(status),
                _ => None,
            },
            Self::RefreshResponse(response) => Some(response.status),
            Self::Genai(_)
            | Self::Webc(_)
            | Self::StreamTimeout(_)
            | Self::Utf8(_)
            | Self::Io(_)
            | Self::Url(_)
            | Self::ValueExt(_)
            | Self::Json(_)
            | Self::UnknownWebStreamSource(_) => None,
        }
    }

    pub fn body(&self) -> Option<&str> {
        match self {
            Self::Genai(cause) => match cause {
                GenaiFailure::ChatResponseGeneration(response) => Some(&response.response_body),
                GenaiFailure::ChatResponse(response) => Some(&response.body),
                GenaiFailure::HttpError(response) => Some(&response.body),
                _ => None,
            },
            Self::Webc(cause) => match cause {
                WebcFailure::ResponseFailedNotJson(response) => Some(&response.body),
                WebcFailure::ResponseFailedInvalidJson(response) => Some(&response.body),
                WebcFailure::ResponseFailedStatus(response) => Some(&response.body),
                WebcFailure::JsonValueExt(_) => None,
            },
            Self::RefreshResponse(response) => match &response.body {
                RefreshResponseBody::Detail(detail) => match &detail.detail {
                    RefreshResponseDetail::Text(text) => Some(&text.preview),
                    RefreshResponseDetail::Html | RefreshResponseDetail::Empty => None,
                },
                RefreshResponseBody::ReadFailed(_) => None,
            },
            Self::Reqwest(_)
            | Self::StreamTimeout(_)
            | Self::Utf8(_)
            | Self::Io(_)
            | Self::Url(_)
            | Self::ValueExt(_)
            | Self::Json(_)
            | Self::UnknownWebStreamSource(_) => None,
        }
    }
}

impl RefreshServiceResponse {
    pub fn expired_or_revoked(&self) -> bool {
        self.status == 401
            || match &self.body {
                RefreshResponseBody::Detail(detail) => detail.code.as_deref().is_some_and(|code| {
                    matches!(
                        code,
                        "invalid_grant"
                            | "invalid_refresh_token"
                            | "token_expired"
                            | "refresh_token_expired"
                            | "refresh_token_invalidated"
                            | "refresh_token_reused"
                    )
                }),
                RefreshResponseBody::ReadFailed(_) => false,
            }
    }
}

impl CredentialFailure {
    fn expired_or_revoked(&self) -> bool {
        match &self.cause {
            CredentialCause::Refresh(CredentialCauseRefresh {
                cause: RefreshFailure::Service(service),
                ..
            }) => {
                matches!(service.cause.as_ref(), ServiceCause::RefreshResponse(response) if response.expired_or_revoked())
            }
            _ => false,
        }
    }

    pub fn requires_sign_in(&self) -> bool {
        self.expired_or_revoked()
            || match &self.cause {
                CredentialCause::OwnerPanicked
                | CredentialCause::OwnerStopped
                | CredentialCause::Removed
                | CredentialCause::UnresolvedRotation => true,
                CredentialCause::Refresh(refresh) => refresh.outcome == RefreshOutcome::Ambiguous,
                CredentialCause::Store(_)
                | CredentialCause::LeaseTimedOut
                | CredentialCause::PreparationDeadline => false,
            }
    }

    pub fn advice(&self) -> Vec<String> {
        if self.expired_or_revoked() {
            vec![format!(
                "the {} login for {:?} has expired or been revoked; use account {:?} on {} to sign in again",
                self.service.describe(),
                self.account,
                self.account,
                self.service.describe(),
            )]
        } else if self.requires_sign_in() {
            vec![format!(
                "use account {:?} on {} to sign in again",
                self.account,
                self.service.describe()
            )]
        } else {
            Vec::new()
        }
    }
}

impl AgentFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::NoFuel => vec!["you may still stop a descendant's exchange or dismiss one of your own helpers".into()],
            Self::NameTaken(_) => vec!["pick another name, or wait for its owner to finish teardown; names stay reserved through teardown".into()],
            Self::UnknownProviderRole(role) if role.offered.is_empty() => vec!["pass model: `inherit".into(), "only the human can add a role, by launching exarch with --role NAME=MODEL".into()],
            Self::UnknownProviderRole(_) => vec!["choose a configured role, or pass model: `inherit".into()],
            Self::NotOwned(_) => vec!["exarch-agents `read reads only your own helpers, and /clear forgets earlier ones".into(), "dismiss only an agent you started; its parent may dismiss it".into()],
            Self::SelfMessage(_) => vec!["to wake yourself, arm an exarch-schedules fire".into()],
            Self::Ended(_) => vec!["start a new agent".into()],
            Self::QueuedUnsynced(value) => {
                let mut advice = vec!["the message was queued and may already have been read; do not automatically resend it".into()];
                advice.extend(value.cause.advice());
                advice
            },
            Self::ReplyUnavailable(value) => value.cause.advice(),
            Self::ReplyPending(_) => vec!["wait for its notice instead of polling".into()],
            Self::ReplyStopped(_) => vec!["message it to continue".into()],
            Self::ReplyFailed(reply) => reply.cause.advice(),
            _ => Vec::new(),
        }
    }
}

impl EditFailure {
    pub fn advice(&self) -> Vec<String> {
        let advice = match self {
            Self::HashAbsent(_) | Self::HashAmbiguous(_) => {
                "re-read with view-hash/view-hash-around before editing; the witness may be stale"
            }
            Self::MultipleLiteralMatches(_) => {
                "widen FROM with a neighbouring line to make it unique; to replace every match, use re-replace-all between from-string and to-string, noting that its pattern is a regex"
            }
            Self::NoLiteralMatch(miss) => match &miss.evidence {
                LiteralMiss::Escapes => {
                    "ral strings are verbatim: write real newlines inside a raw #'…'# string, not literal backslash escapes"
                }
                LiteralMiss::Indentation(_) => {
                    "copy the exact text, indentation included, from view-text-around"
                }
                LiteralMiss::Absent => {
                    "re-read the file with view-text-around before editing; it may already contain the intended text"
                }
            },
            Self::NoEdits => "pass a list of [hash: …, line: …] records",
            Self::DuplicateTargetLine(_) | Self::EmptyFrom => return Vec::new(),
        };
        vec![advice.into()]
    }
}

impl PluginFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::NotFound(_) => vec!["is it in <config>/ral/plugins or RAL_PATH?".into()],
            Self::OptionsWithoutFactory(_) => {
                vec!["its entry in the rc's plugins map wants the empty options map `[:]`".into()]
            }
            Self::FactoryReturnedBlock(_) => {
                vec!["expected a manifest Map, e.g. [name: 'my-plugin', hooks: [:], keybindings: []]".into()]
            }
            Self::UnknownHookEvent(value) => {
                vec![format!(
                    "Valid events: {}",
                    value
                        .offered
                        .iter()
                        .map(|event| format!("'{}'", event.escape_debug()))
                        .collect::<Vec<_>>()
                        .join(", ")
                )]
            }
            Self::AlreadyLoaded(_) | Self::HandlerInstallationEscaped(_) => Vec::new(),
        }
    }
}

impl KeybindingFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Notation(value) => value.cause.advice(),
            Self::SequencePrefix(_) => vec!["a key sequence begins with ctrl-x, the prefix the line editor leaves free for sequences".into()],
            Self::UnguardedBuiltinShadow(_) => vec!["add a 'guard:' regex so unmatched presses fall through".into()],
            Self::ReservedAction(_) | Self::GuardRegex(_) => Vec::new(),
        }
    }
}

impl HostFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Ral(RalFailure::Keybinding(cause)) => cause.advice(),
            Self::Ral(RalFailure::Plugin(cause)) => cause.advice(),
            Self::Ral(RalFailure::Editor(cause)) => cause.advice(),
            Self::Exarch(ExarchFailure::Agent(cause)) => cause.advice(),
            Self::Exarch(ExarchFailure::Edit(cause)) => cause.advice(),
            Self::Exarch(ExarchFailure::Skill(cause)) => cause.advice(),
            Self::Exarch(ExarchFailure::Recording(cause)) => cause.advice(),
            Self::Exarch(ExarchFailure::Context(cause)) => cause.advice(),
            Self::Exarch(ExarchFailure::Schedule(cause)) => cause.advice(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod presentation_tests {
    use crate::data::datum::Datum;
    use crate::error::Error;
    use crate::fact::Reason;

    #[test]
    fn editor_buffer_advice_does_not_leak_to_process_or_authority_refusals() {
        use crate::fact::{Denial, Failure, SingleOrNested, Withholding};
        for reason in [
            Reason::process("editor", Failure::NotFound),
            Reason::Denied(Box::new(Denial::exec_run(
                "editor".into(),
                Withholding::MissingAllowance(SingleOrNested::Single),
            ))),
        ] {
            let decoded = Reason::decode(&reason.clone().encode()).unwrap();
            assert_eq!(decoded, reason);
            assert!(
                decoded
                    .advice()
                    .iter()
                    .all(|advice| !advice.contains("input buffer"))
            );
            assert!(!Error::new(decoded).plain().contains("input buffer"));
        }
    }
}

impl ScheduleFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::NoGrant(_) => {
                vec!["relaunch exarch with --allow-schedule to allow self-wakeups".into()]
            }
            Self::DuplicateLiveLabel(_) => {
                vec!["pick another label, or remove it first with exarch-schedules `remove".into()]
            }
            Self::Recording(value) => value.cause.advice(),
            Self::Cron(_) | Self::Duration(_) | Self::NoFutureOccurrence(_) => Vec::new(),
        }
    }
}

impl HelperFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Provider(value) => value.advice(),
            Self::Context(value) => value.advice(),
            Self::Recording(value) => value.advice(),
            Self::NoReply
            | Self::EndedBeforeReporting
            | Self::Cancelled
            | Self::ProviderStopped(_)
            | Self::Connection(_)
            | Self::Panicked => Vec::new(),
        }
    }
}

#[cfg(test)]
mod agent_schedule_presentation_tests {
    use super::*;
    use crate::data::datum::Datum;
    use crate::error::Error;
    use crate::fact::{IoKind, IoLeaf, OsFailure, Reason};

    fn check(value: &ExarchFailure, description: &str, advice: &[&str]) {
        let decoded = ExarchFailure::decode(&value.clone().encode()).unwrap();
        assert_eq!(&decoded, value);
        for value in [value, &decoded] {
            let reason = Reason::Host(Box::new(HostFailure::Exarch(value.clone())));
            assert_eq!(reason.describe(), description);
            assert_eq!(reason.advice(), advice);
            let error = Error::new(reason);
            assert_eq!(error.hints(), []);
            let mut expected = description.to_owned();
            for hint in advice {
                let _ = write!(expected, "\nhint: {hint}");
            }
            assert_eq!(error.plain(), expected);
        }
    }

    #[test]
    fn recording_advice_reaches_every_direct_and_nested_route_once() {
        for recording in [
            RecordingFailure::CellPoisoned(RecordingFailureCellPoisoned {
                agent: u64::MAX.into(),
            }),
            RecordingFailure::Recording(RecordingFailureRecording {
                record: Some("log".into()),
                stage: RecordingStage::Sync,
                cause: IoCause::new(IoLeaf::Os(OsFailure {
                    kind: IoKind::PermissionDenied,
                    raw: None,
                    detail: None,
                })),
            }),
        ] {
            let advice = recording.advice();
            let borrowed: Vec<_> = advice.iter().map(String::as_str).collect();
            for failure in [
                ExarchFailure::Recording(recording.clone()),
                ExarchFailure::Schedule(ScheduleFailure::Recording(ScheduleFailureRecording {
                    label: None,
                    trigger: None,
                    cause: recording.clone(),
                })),
                ExarchFailure::Agent(AgentFailure::ReplyUnavailable(
                    AgentFailureReplyUnavailable {
                        name: "a".into(),
                        cause: recording.clone(),
                    },
                )),
                ExarchFailure::Agent(AgentFailure::ReplyFailed(AgentFailureReplyFailed {
                    name: "a".into(),
                    cause: HelperFailure::Recording(recording.clone()),
                })),
                ExarchFailure::Agent(AgentFailure::ReplyFailed(AgentFailureReplyFailed {
                    name: "a".into(),
                    cause: HelperFailure::Provider(ProviderFailure::new(
                        ProviderLeaf::Unavailable(recording.clone()),
                    )),
                })),
            ] {
                check(
                    &failure,
                    &format!("exarch: {}", failure.describe()),
                    &borrowed,
                );
            }
            let queued =
                ExarchFailure::Agent(AgentFailure::QueuedUnsynced(AgentFailureQueuedUnsynced {
                    name: "a".into(),
                    cause: recording,
                }));
            let mut expected = vec![
                "the message was queued and may already have been read; do not automatically resend it",
            ];
            expected.extend(borrowed);
            check(
                &queued,
                &format!("exarch: {}", queued.describe()),
                &expected,
            );
        }
    }
}
