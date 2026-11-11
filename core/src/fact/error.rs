//! A failure as the runtime knows it ([`ErrorFact`]: a typed [`Reason`] and
//! its provenance) and as a program reads it ([`ErrorRecord`], its
//! projection).  Bytes are absent by design: `audit` is the forensic path.
//! Minted from an `Error` in `shell::runtime::error`.

mod compile;
mod decode;
mod denial;
mod evaluation;
mod host;
mod invalid;
mod invariant;
mod io;
mod launch;
mod protocol;
mod shared;
mod state;

pub use compile::{CompileFailure, CompilePhase, TypeErrorCount};
pub use decode::{
    Base64Failure, Base64FailureInvalidByte, Base64FailureInvalidLastSymbol,
    Base64FailureInvalidLength, CardFailure, CheckDecisionFailure, DecodeCause, DecodeFailure,
    DecodeIntegerRange, DecodeIntegerTarget, DecodeLength, DurationOverflow, FiniteBitsFailure,
    JsonEncodeCause, JsonEncodeCauseNativeComputation, JsonEncodeCauseNonUnicodePath,
    JsonEncodeFailure, JsonInputFailure, JsonMapKeyFailure, JsonNumberFailure, JsonNumberFault,
    JsonNumberTarget, JsonShape, JsonShapeFailure, JsonStringFailure, LexicalPathFailure,
    LexicalPathFailureNotWorkingDirectory, NoteFailure, NoteFailureTooLong, OpenScheme,
    OsStringPlatform, OsStringPlatformFailure, PathDecodeFailure, ReservedEnvironmentFailure,
    SealSpellingFailure, TimestampParseFailure, TraceDecodeFailure,
    TraceDecodeFailurePositionMismatch, ViaFailure, ViaFailureDuplicateTag,
};
pub use denial::{
    Denial, DenialDevice, DenialExecRead, DenialExecRun, DenialFlag, DenialFs, EditorFlag,
    ExclusionMatch, ExecExclusion, ExecReadSubject, ExecReadSubjectBundled, FlagId, FsRefusal,
    PinnedBinary, ReadOrWrite, ShellFlag, SingleOrNested, Withholding, WithholdingExcluded,
};
pub use evaluation::{
    Access, ApplyFailure, ArithmeticFailure, BinaryArithmeticOp, CallOrForce, CaseArmFailure,
    CompareFailure, DivOrMod, EqualityOrOrder, EvalFailure, FloatBinaryOverflow, FloatOverflow,
    IndexBounds, IntegerBinaryOverflow, IntegerOverflow, KeyOrField, MissingEntry, PatternLength,
    RoundUnrepresentable, Unreadable,
};
pub use host::{
    AgentFailure, AgentFailureEnded, AgentFailureNameTaken, AgentFailureNoLiveAgent,
    AgentFailureNotDescendant, AgentFailureNotOwned, AgentFailureQueuedUnsynced,
    AgentFailureReplyAbsent, AgentFailureReplyFailed, AgentFailureReplyPending,
    AgentFailureReplyStopped, AgentFailureReplyUnavailable, AgentFailureResting,
    AgentFailureSelfMessage, AgentFailureUnknownProviderRole, BootFailure,
    BootFailureProfileNotAbsolute, BuiltinKeyAction, ContextFailure, ContextFailureAssistantRole,
    ContextFailureCurrentlyWritten, ContextFailureDuplicateToolResult, ContextFailureMutation,
    ContextFailureReadBack, ContextFailureStraddledEviction, ContextFailureToolResultCount,
    ContextFailureTurnNotRecorded, ContextFailureUnknownToolResult, ContextFailureWithdrawn,
    ContextOperation, ContextState, ContextStateAwaitingToolResults, CredentialCause,
    CredentialCauseRefresh, CredentialCauseStore, CredentialFailure, CredentialService,
    CredentialStoreOperation, DraftFailure, DraftFailureTooLarge, DraftOperation, EditFailure,
    EditFailureDuplicateTargetLine, EditFailureHashAbsent, EditFailureHashAmbiguous,
    EditFailureMultipleLiteralMatches, EditFailureNoLiteralMatch, EditorCommandCause,
    EditorFailure, EditorFailureCommand, EditorFailureDraft, EditorFailureUnknownStyle,
    EditorFailureWorkspace, EditorVariable, EditorWorkspaceFailure,
    EditorWorkspaceFailureExpectedRegularFile, EditorWorkspaceStep, EngineCause,
    EngineFactoryFailure, EngineFactoryFailureDialTimedOut,
    EngineFactoryFailureDialWaitDisconnected, EngineFactoryFailurePlatform,
    EngineFactoryFailureUnsupported, EngineFailure, EngineStep, EntryKind, EscapedLoad,
    ExarchFailure, FactoryBackend, ForeignJsonCategory, ForeignJsonFailure, FrontmatterFailure,
    FrontmatterFailureDirectoryNameMismatch, FrontmatterFailureFieldShape,
    FrontmatterFailureInvalidName, FrontmatterFailureNotMapping, GenaiFailure,
    GenaiFailureAdapterKindMismatch, GenaiFailureAdapterNotSupported,
    GenaiFailureCacheBreakpointNoEligibleContent, GenaiFailureChatResponse,
    GenaiFailureChatResponseGeneration, GenaiFailureClientBuildFail, GenaiFailureHttpError,
    GenaiFailureInternal, GenaiFailureInvalidJsonResponseElement,
    GenaiFailureLastChatMessageIsNotUser, GenaiFailureMessageContentTypeNotSupported,
    GenaiFailureMessageRoleNotSupported, GenaiFailureModelMapperFailed,
    GenaiFailurePromptCacheRetentionParsing, GenaiFailureReasoningParsing, GenaiFailureResolver,
    GenaiFailureServiceTierParsing, GenaiFailureStreamParse, GenaiFailureVerbosityParsing,
    HelperFailure, HostFailure, KeyModifier, KeyNotationFailure,
    KeyNotationFailureControlCharacter, KeyNotationFailureDuplicateModifier,
    KeyNotationFailureShiftedCharacter, KeyNotationFailureTerminalAlias,
    KeyNotationFailureUnknownKey, KeyNotationFailureUppercaseControl, KeybindingFailure,
    KeybindingFailureGuardRegex, KeybindingFailureNotation, KeybindingFailureReservedAction,
    KeybindingFailureSequencePrefix, KeybindingFailureUnguardedBuiltinShadow, LiteralMiss,
    PluginFailure, PluginFailureAlreadyLoaded, PluginFailureFactoryReturnedBlock,
    PluginFailureHandlerInstallationEscaped, PluginFailureNotFound,
    PluginFailureOptionsWithoutFactory, PluginFailureUnknownHookEvent, PodKind, PolicyFailure,
    PolicyFailureEscapedLoad, PolicyFailureUnknownBase, ProviderAdapter, ProviderCancelSite,
    ProviderContext, ProviderFailure, ProviderInstant, ProviderLeaf, ProviderLeafApi,
    ProviderLeafOutputCap, ProviderLeafOverflow, ProviderLeafTransient, ProviderLimit,
    ProviderMessageRole, ProviderModel, ProviderRefusal, ProviderRefusalCause,
    ProviderRefusalCauseHeld, ProviderResponseHeader, ProviderStop, ProviderUrlFailure,
    ProviderUrlFailureUnclassified, RalFailure, RecordLineFailure, RecordLineFailureSealMismatch,
    RecordReadFailure, RecordReadFailureLine, RecordReadFailureLocusSealMismatch,
    RecordReadFailureMissingLocusRecord, RecordingFailure, RecordingFailureCellPoisoned,
    RecordingFailureRecording, RecordingStage, RefreshFailure, RefreshFailureExpiryOverflow,
    RefreshFailureInvalidExpiry, RefreshFailureResponse, RefreshOutcome, RefreshPayloadFailure,
    RefreshResponseBody, RefreshResponseBodyDetail, RefreshResponseDetail,
    RefreshResponseDetailText, RefreshServiceResponse, RefreshTokenField, ReqwestClass,
    ReqwestClassUnclassified, ReqwestFailure, ReqwestSource, ReqwestSourceForeign, ReqwestSummary,
    ReservedKeyAction, ResolverFailure, ResolverFailureApiKeyEnvNotFound, ResolverFailureCustom,
    RetryableProviderCause, RetryableProviderCauseStreamEndedWithoutEnd, ScheduleFailure,
    ScheduleFailureDuplicateLiveLabel, ScheduleFailureNoFutureOccurrence, ScheduleFailureNoGrant,
    ScheduleFailureRecording, ServiceCause, ServiceCauseUnknownWebStreamSource, ServiceContext,
    ServiceContextUnknownWebStreamSource, ServiceFailure, ServiceOperation, SkillFailure,
    SkillFailureFrontmatter, SkillFailureInvalidName, SkillFailureMissing, SkillFailureRead,
    SkillFailureYaml, SkillField, SkillPlace, StreamTimeoutKind, TerminalKeyAlias, TurnSpan,
    ValueExtFailure, ValueExtFailureCustom, ValueExtFailurePropertyNotFound,
    ValueExtFailurePropertyValueNotOfType, ValueExtFailureValueNotOfType, WebcFailure,
    WebcFailureResponseFailedInvalidJson, WebcFailureResponseFailedNotJson,
    WebcFailureResponseFailedStatus, WithdrawalEvidence,
};
pub use invalid::{
    AbsolutePathRefusal, AddressableRange, AdmissionAdvice, AdmissionCause, AdmissionExport,
    AdmissionExtraField, AdmissionFailure, AdmissionShares, AdmissionUses, ArityFailure, BlockRole,
    CodecFailure, CodecFailureCsvCell, CodecFailureCsvKeys, CodecFailureCsvWidth,
    CodecFailureJsonLine, CodecFailureJsonPointer, CodecFailureMatchCount, CodecFailureNotJson,
    CodecFailureUtf8, CodecJsonNumberFailure, CsvDeserializeCause, CsvPosition, CsvReadFailure,
    CsvReadFailureDeserialize, CsvReadFailureUnequalLengths, CsvReadFailureUtf8, GitPointerFailure,
    GlobFailure, GlobFailureHiddenOnly, GlobFailureSyntax, GrantFailure, GrantFailureAbsolutePath,
    GrantFailureGitPointer, GrantFailureOutsideHome, GrantFailureRefusedKey,
    GrantFailureRelativeAnchor, GrantFailureSubpath, GrantFailureUnknownVocabulary,
    GrantFailureUnwalkable, GrantFailureWorldSelector, GrantForm, GrantVocabulary, HalfOpenRange,
    HomeClassification, IllTyped, InvalidCause, InvalidFailure, JsonPointerCause, JsonValueFailure,
    JsonValueNumberCause, LiteralOrRegex, MissingGrantAnchor, Multiline, NonData, NumberFailure,
    NumberRange, PathFailure, PathFailureGlob, PathFailureNoProcessCwd, PathFailureNotUnicode,
    PathFailureNotWorkingDirectory, RefusedGrantKey, RelativeSubpathRefusal, RuntimeSigilFailure,
    RuntimeSigilFailureTilde, RuntimeSigilFailureXdg, TypeDiagnostic, Utf8Subject, WorldRelation,
};
pub use invariant::InvariantFailure;
pub use io::{
    IoCause, IoContext, IoFailure, IoKind, IoLeaf, IoOperation, IoSubject, KernelReply, OsFailure,
    OwnAct, OwnIoContext, OwnJsonCause, OwnLeaf, OwnLeafDangling, OwnLeafJson, OwnLeafOutside,
    PathIoContext, PathIoContextDangling, PathIoContextStoredNameRead, PathIoContextUnsynced,
    PathIoLeaf, PathIoLeafEntryName, PathIoLeafLoop, PathIoLeafNoProtectedBoundary,
    PathIoLeafRequiredInformationClass, PathIoLeafStoredNameAmbiguous, PathIoLeafStoredNameUnheld,
    PathIoLeafVolumeAttributesUnsupported, StandardStream, WindowsInformationClass,
    WindowsPathOperation,
};
pub use launch::{
    AccessCheckOperation, AnchorReadinessFailure, BinaryRole, BudgetCountFailure,
    BudgetCountFailureKernel, BudgetFailure, BudgetFailureSetBudget, BudgetProbeOperation,
    BudgetSetFailure, BudgetSubject, BwrapProbeFailure, BwrapProbeFailureExited, CarveStep,
    CgroupLimitFile, DaclFailure, DaclFailureAccessCheck, DaclFailureLedgerParse,
    DaclFailureMutexTimeout, DaclFailureReachable, DaclFailureRedirected, DaclFailureWin32,
    DaclFailureWriteDacDenied, DaclOperation, DaclWin32Cause, DescriptorSweepFailure,
    DescriptorSweepFailureBufferTooLarge, DescriptorSweepFailureCloseDescriptor,
    DescriptorSweepFailureCloseRange, DescriptorSweepFailurePartialDescriptorRecord,
    DescriptorSweepFailureSetReportCloexec, DetachedPidCause, DetachedPidFailure, FilterFailure,
    FilterFailureArch, FilterFailureCompile, GrantLimit, HandoffField, JailFailure,
    JailFailureCreate, JailFailureDelegate, JailFailureLimit, JailFailureMembership,
    JailFailureSequence, KernelFeatureFailure, LaunchFailure, LaunchImageSubject, LauncherFailure,
    LauncherFailureChildFork, LauncherFailureChildPipe, LauncherFailureChildSetup,
    LauncherFailureDiedBeforeReport, LauncherFailureExec, LauncherFailureNonNativeExecError,
    LauncherFailureReportEncodingTooLarge, LauncherFailureSpawnBeforeSetup, LauncherRole,
    LedgerIoFailure, LedgerIoFailureRetryExhausted, MembershipStep, NativeStatus, ParcelCause,
    ParcelCauseSocketTooSmall, ParcelFailure, ParcelRole, ParcelStep, PayloadGroupFailure,
    PinCause, PinFailure, PrepareCause, PrepareFailure, PrepareInvalid, PrepareStep,
    PrepareSubject, ProjectionFailure, ProjectionFailureCarve, ProjectionFailureChangedSymlink,
    ProjectionFailureCopyHandle, ProjectionFailureHiddenWorkingDirectory, ProjectionFailureList,
    ProjectionFailurePinnedDirectoryRedirected, ProjectionFailureProcExcluded,
    ProjectionFailureResolve, ProjectionOpenFailure, ProjectionOpenFailureFailed,
    RedirectedTargetKind, RulesetFailure, RulesetFailureAdmit, SandboxBackend, SeatbeltFailure,
    SeatbeltFailureAlreadyConfined, SeatbeltFailureEntry, SequenceFailure, SessionLimit,
    SidFailure, SidFailureParse, SidFailureTooLong, UnavailableFailure,
    UnavailableFailureBwrapProbe, UnsupportedConfinement,
    UnsupportedConfinementWindowsAllowanceExclusion, UnsupportedConfinementWindowsEveryPath,
    WarrantFailure, WarrantFailureArgumentCount, WarrantFailureInvalidProgram,
    WarrantFailureMissingDescriptor, WarrantFailureSize, WarrantFailureSlot,
    WarrantFailureSlotCount, WarrantFailureStreamInstall, WarrantFailureUtf8, WarrantField,
};
pub use protocol::{
    ConnectionClosed, ConnectionClosedIo, ConnectionFailure, ConnectionFault,
    ConnectionFaultProbeAnswer, ConnectionFaultProbeRejected, ConnectionFaultUnencodable,
    ConnectionFaultUnexpectedFrame, ConnectionOperation, ForkKind, FrameChannel,
    FrameEncodeFailure, FrameEncodeFailureTooLarge, FrameFailure, FrameFailurePartial,
    FrameFailureTooLarge, FrameKind, FramePart, ProbeFailure, ProbeSubject, ProtocolData,
    ProtocolFailure, ProtocolFailureDecode, ProtocolFailureFrame, ProtocolFailureUnencodable,
    ProtocolFailureVersionMismatch, ProtocolFailureWrongForkKind, ProtocolOperation, ProtocolPeer,
    ProtocolPhase, ProtocolWork, SeedFrameFailure, SeedFrameFailureTrailing,
};
pub use shared::{
    AgentNameFailure, Count, CountRule, CronCause, CronCauseField, CronCauseFieldCount,
    CronFailure, CronField, CronFieldCause, CronFieldCauseDescendingRange,
    CronFieldCauseInvalidStep, CronFieldCauseOutOfRange, CronFieldCauseUnknownValue, DataHead,
    DataKind, DataStep, DurationCause, DurationCauseInvalidCount, DurationCauseUnknownUnit,
    DurationFact, DurationFailure, EntryNameFailure, Expected, Found, IntOrFloat,
    IntegerParseCause, InvertedTurnSpan, JsonCategory, JsonSyntaxFailure, Number, NumberFault,
    NumberOrText, OpaqueKind, RegexCause, RegexFailure, ReservedVar, ShapeFailure, TextPreview,
    UnexpectedPayload, UnknownField, UnknownTag, Utf8Failure, ValueShape,
};
pub use state::{
    AmbientSetting, CaptureLimit, DepthFailure, DepthKind, HookArity, HookNotFunction, HookRef,
    HookRegistrationFailure, HookSignature, ModuleCycle, OperationState, PluginHookRef,
    StateFailure, WorkerCap,
};

use super::FailureCode as _;
use crate::process::CancelCause;
use crate::source::CallSite;
use crate::{record, variant};
use std::borrow::Cow;
use std::num::NonZeroU8;

/// Why a failure happened.  Rust-side only: a program reads its projection,
/// [`ErrorRecord`]'s `message` and `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Process(Box<Process>),
    TimedOut,
    Cancelled,
    Raised(Box<Claim>),
    Denied(Box<Denial>),
    Io(Box<IoFailure>),
    Invalid(Box<InvalidFailure>),
    Evaluation(Box<EvalFailure>),
    State(Box<StateFailure>),
    Compile(Box<CompileFailure>),
    Launch(Box<LaunchFailure>),
    Protocol(Box<ProtocolFailure>),
    Host(Box<HostFailure>),
    Invariant(Box<InvariantFailure>),
}

variant!(Reason {
    Process(Box<Process>): "process",
    TimedOut: "timed-out",
    Cancelled: "cancelled",
    Raised(Box<Claim>): "raised",
    Denied(Box<Denial>): "denied",
    Io(Box<IoFailure>): "io",
    Invalid(Box<InvalidFailure>): "invalid",
    Evaluation(Box<EvalFailure>): "evaluation",
    State(Box<StateFailure>): "state",
    Compile(Box<CompileFailure>): "compile",
    Launch(Box<LaunchFailure>): "launch",
    Protocol(Box<ProtocolFailure>): "protocol",
    Host(Box<HostFailure>): "host",
    Invariant(Box<InvariantFailure>): "invariant",
});

/// A program that did not succeed: its head as dispatched, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub program: String,
    pub failure: Failure,
}

record!(Process {
    program: "program",
    failure: "failure",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Exited(i32),
    Signaled(i32),
    NotFound,
    NotRunnable(NotRunnable),
}

variant!(Failure {
    Exited(i32): "exited",
    Signaled(i32): "signaled",
    NotFound: "not-found",
    NotRunnable(NotRunnable): "not-runnable",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotRunnable {
    pub found: String,
    pub why: Unrunnable,
}

record!(NotRunnable {
    found: "found",
    why: "why"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unrunnable {
    PermissionDenied,
    Directory,
    NotExecutable,
    /// The `#!` interpreter the file names, when it names one.
    MissingInterpreter(Option<String>),
    Io(Box<IoCause>),
}

variant!(Unrunnable {
    PermissionDenied: "permission-denied",
    Directory: "directory",
    NotExecutable: "not-executable",
    MissingInterpreter(Option<String>): "missing-interpreter",
    Io(Box<IoCause>): "io",
});

impl Failure {
    /// `found`, where the program was sought, cannot run for `why`.
    pub(crate) fn not_runnable(found: impl Into<String>, why: Unrunnable) -> Self {
        Self::NotRunnable(NotRunnable {
            found: found.into(),
            why,
        })
    }
}

/// A failure the program states: `fail` (status 1), or a hand-built record
/// `raise` was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub message: String,
    pub status: NonZeroU8,
}

record!(Claim {
    message: "message",
    status: "status",
});

/// A failure as the runtime knows it: why, the frame it is reported at, and
/// its location.  What the wire, logs and exarch carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorFact {
    pub(crate) frame: String,
    pub(crate) reason: Reason,
    pub(crate) site: CallSite,
}

record!(ErrorFact {
    frame: "frame",
    reason: "reason",
    site: "site",
});

/// The record a program reads: [`ErrorFact::record`].  Its `message` and
/// `status` are the reason's projections, never stored beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorRecord {
    pub frame: String,
    pub message: String,
    pub status: NonZeroU8,
    pub site: CallSite,
}

record!(typed ErrorRecord {
    frame: "frame",
    message: "message",
    status: "status",
    site: "site",
});

impl Reason {
    /// The reason a worker's own switch records for `cause`: a cancel or a
    /// timeout, the two a worker converts; any other stop is the host's.
    pub fn stopped(cause: CancelCause) -> Option<Self> {
        match cause {
            CancelCause::Cancelled => Some(Self::Cancelled),
            CancelCause::TimedOut => Some(Self::TimedOut),
            _ => None,
        }
    }

    /// `fail message`: the program's claim, with status 1.
    pub fn raised(message: impl Into<String>) -> Self {
        Self::Raised(Box::new(Claim {
            message: message.into(),
            status: NonZeroU8::MIN,
        }))
    }

    pub fn process(program: impl Into<String>, failure: Failure) -> Self {
        Self::Process(Box::new(Process {
            program: program.into(),
            failure,
        }))
    }

    pub fn failure(&self) -> Option<&Failure> {
        match self {
            Self::Process(process) => Some(&process.failure),
            _ => None,
        }
    }

    pub fn describe(&self) -> Cow<'_, str> {
        match self {
            Self::Process(process) => process.describe().into(),
            Self::TimedOut => CancelCause::TimedOut.message().into(),
            Self::Cancelled => CancelCause::Cancelled.message().into(),
            Self::Raised(claim) => claim.message.as_str().into(),
            Self::Denied(cause) => cause.describe().into(),
            Self::Io(cause) => cause.describe().into(),
            Self::Invalid(cause) => cause.describe().into(),
            Self::Evaluation(cause) => cause.describe().into(),
            Self::State(cause) => cause.describe().into(),
            Self::Compile(cause) => cause.describe().into(),
            Self::Launch(cause) => cause.describe().into(),
            Self::Protocol(cause) => cause.describe().into(),
            Self::Host(cause) => cause.describe().into(),
            Self::Invariant(cause) => cause.describe().into(),
        }
    }

    /// Ordered advice determined only by this recorded reason.
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Process(process) => process.advice(),
            Self::Denied(cause) => cause.advice(),
            Self::Invalid(cause) => cause.advice(),
            Self::Evaluation(cause) => cause.advice(),
            Self::State(cause) => cause.advice(),
            Self::Launch(cause) => cause.advice(),
            Self::Host(cause) => cause.advice(),
            Self::Protocol(cause) => cause.advice(),
            Self::Invariant(cause) => cause.advice(),
            Self::TimedOut | Self::Cancelled | Self::Raised(_) | Self::Io(_) | Self::Compile(_) => {
                Vec::new()
            }
        }
    }
}

impl Process {
    fn advice(&self) -> Vec<String> {
        let hint = match self.failure {
            #[cfg(unix)]
            Failure::Signaled(libc::SIGKILL) => "the process was killed with SIGKILL; the kernel or another process may have terminated it".into(),
            #[cfg(unix)]
            Failure::Signaled(libc::SIGSEGV) => "the process crashed with a segmentation fault".into(),
            Failure::Signaled(n) => format!("the process terminated from {}", crate::process::Signal::new(n)),
            Failure::Exited(_) | Failure::NotFound | Failure::NotRunnable(_) => return Vec::new(),
        };
        vec![hint]
    }

    fn describe(&self) -> String {
        let Self { program, failure } = self;
        match failure {
            #[cfg(windows)]
            Failure::Exited(windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT) => {
                format!("{program}: ended by Ctrl-C")
            }
            Failure::Exited(n) => format!("{program}: exited with status {n}"),
            Failure::Signaled(n) => format!(
                "{program}: killed by signal {}",
                crate::process::Signal::new(*n)
            ),
            Failure::NotFound => format!("{program}: command not found"),
            Failure::NotRunnable(NotRunnable { found, why }) => {
                let at = if found == program {
                    program.clone()
                } else {
                    format!("{program}: {found}")
                };
                match why {
                    Unrunnable::PermissionDenied => format!("{at}: permission denied"),
                    Unrunnable::Directory => format!("{at} is a directory, not a command"),
                    Unrunnable::NotExecutable => format!("{at} is a file, not an executable"),
                    Unrunnable::MissingInterpreter(Some(interpreter)) => {
                        format!("{at}: its interpreter {interpreter} or its loader was not found")
                    }
                    Unrunnable::MissingInterpreter(None) => {
                        format!("{at}: its interpreter or loader was not found")
                    }
                    Unrunnable::Io(cause) => format!("{at}: {}", cause.describe()),
                }
            }
        }
    }
}

impl serde::Serialize for ErrorFact {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        crate::data::datum::as_datum::serialize(self, s)
    }
}

impl<'de> serde::Deserialize<'de> for ErrorFact {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::data::datum::as_datum::deserialize(d)
    }
}

impl serde::Serialize for ErrorRecord {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        crate::data::datum::as_datum::serialize(self, s)
    }
}

impl<'de> serde::Deserialize<'de> for ErrorRecord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::data::datum::as_datum::deserialize(d)
    }
}

impl ErrorFact {
    pub fn new(reason: Reason, frame: String, site: CallSite) -> Self {
        Self {
            frame,
            reason,
            site,
        }
    }

    pub fn frame(&self) -> &str {
        &self.frame
    }

    pub fn reason(&self) -> &Reason {
        &self.reason
    }

    pub fn describe(&self) -> Cow<'_, str> {
        self.reason.describe()
    }

    pub fn site(&self) -> &CallSite {
        &self.site
    }

    /// What a program reads of this failure.
    pub fn record(&self) -> ErrorRecord {
        ErrorRecord {
            frame: self.frame.clone(),
            message: self.reason.describe().into_owned(),
            status: self.reason.failure_code(),
            site: self.site.clone(),
        }
    }
}

impl ErrorRecord {
    /// A hand-built record, raised as the program's own claim.
    pub fn claim(self) -> ErrorFact {
        let Self {
            frame,
            message,
            status,
            site,
        } = self;
        ErrorFact::new(
            Reason::Raised(Box::new(Claim { message, status })),
            frame,
            site,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;
    use crate::fact::FailureCode;
    use crate::source::CallSite;

    fn operational_reasons() -> Vec<Reason> {
        vec![
            Reason::Denied(Box::new(Denial::exec_run(
                "git".into(),
                Withholding::MissingAllowance(SingleOrNested::Nested),
            ))),
            Reason::Io(Box::new(IoFailure {
                operation: IoOperation::Read,
                subject: IoSubject::Path("\"f\"".into()),
                cause: IoCause::new(IoLeaf::Torn),
            })),
            Reason::Invalid(Box::new(InvalidFailure {
                operation: "test".into(),
                path: vec![DataStep::Index(Count::from_u64(u64::MAX))],
                cause: InvalidCause::Empty,
            })),
            Reason::Evaluation(Box::new(EvalFailure::MissingBinding("STATUS".into()))),
            Reason::State(Box::new(StateFailure::CaptureLimit(CaptureLimit {
                bytes: Count::from_u64(u64::MAX),
            }))),
            Reason::Compile(Box::new(CompileFailure {
                source: "module.ral".into(),
                phase: CompilePhase::Types(TypeErrorCount {
                    count: Count::from_u64(i64::MAX as u64 + 1),
                }),
            })),
            Reason::Launch(Box::new(LaunchFailure::BatchImage("build.cmd".into()))),
            Reason::Protocol(Box::new(ProtocolFailure::VersionMismatch(
                ProtocolFailureVersionMismatch {
                    front: 1,
                    engine: 2,
                },
            ))),
            Reason::Host(Box::new(HostFailure::Ral(RalFailure::Boot(
                BootFailure::AlreadyBooted,
            )))),
            Reason::Invariant(Box::new(InvariantFailure::BareLambda)),
        ]
    }

    #[test]
    fn every_reason_family_survives_datum_and_record_json() {
        let mut reasons = operational_reasons();
        reasons.extend([
            Reason::process("sh", Failure::Exited(3)),
            Reason::TimedOut,
            Reason::Cancelled,
            Reason::raised("authored text"),
            Reason::Raised(Box::new(Claim {
                message: "claimed".into(),
                status: NonZeroU8::new(3).unwrap(),
            })),
        ]);
        for reason in reasons {
            let decoded = Reason::decode(&reason.clone().encode()).unwrap();
            assert_eq!(decoded, reason);
            assert_eq!(decoded.describe(), reason.describe());
            assert_eq!(decoded.advice(), reason.advice());
            assert_eq!(decoded.failure_code(), reason.failure_code());
            let record = ErrorFact::new(
                reason,
                "test".into(),
                CallSite {
                    script: "test.ral".into(),
                    line: 1,
                    col: 2,
                },
            );
            let bytes = crate::data::json::to_datum_vec(&record);
            let decoded = crate::data::json::from_datum_slice::<ErrorFact>(&bytes).unwrap();
            assert_eq!(decoded, record);
            let projected = record.record();
            assert_eq!(projected.message, record.describe());
            assert_eq!(projected.status, record.failure_code());
            assert_eq!(projected.clone().claim().record(), projected);
            assert_eq!(
                ErrorRecord::decode(&projected.clone().encode()),
                Ok(projected)
            );
        }
    }

    #[test]
    fn malformed_family_payload_retains_its_decode_path() {
        let value = crate::data::datum::tag("compile", Some("not a record".to_owned().encode()));
        let failure = Reason::decode(&value).unwrap_err();
        assert_eq!(failure.path, [DataStep::Payload("compile".into())]);
        assert!(matches!(*failure.cause, DecodeCause::Shape(_)));
    }
}
