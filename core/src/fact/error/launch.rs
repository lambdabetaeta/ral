//! Portable launch failure facts.

use super::decode::JsonInputFailure;
use super::io::{IoCause, OsFailure, StandardStream};
use super::shared::Count;
use crate::{label, record, variant};
use strum::{IntoStaticStr, VariantArray};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchFailure {
    Unavailable(UnavailableFailure),
    Prepare(PrepareFailure),
    Pin(PinFailure),
    Projection(ProjectionFailure),
    Parcel(ParcelFailure),
    Ruleset(RulesetFailure),
    Filter(FilterFailure),
    Warrant(WarrantFailure),
    Seatbelt(SeatbeltFailure),
    Dacl(DaclFailure),
    Budget(BudgetFailure),
    Jail(JailFailure),
    Launcher(LauncherFailure),
    BatchImage(String),
    DetachedPid(DetachedPidFailure),
}
variant!(LaunchFailure { Unavailable(UnavailableFailure): "unavailable", Prepare(PrepareFailure): "prepare", Pin(PinFailure): "pin", Projection(ProjectionFailure): "projection", Parcel(ParcelFailure): "parcel", Ruleset(RulesetFailure): "ruleset", Filter(FilterFailure): "filter", Warrant(WarrantFailure): "warrant", Seatbelt(SeatbeltFailure): "seatbelt", Dacl(DaclFailure): "dacl", Budget(BudgetFailure): "budget", Jail(JailFailure): "jail", Launcher(LauncherFailure): "launcher", BatchImage(String): "batch-image", DetachedPid(DetachedPidFailure): "detached-pid" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum BinaryRole {
    Ral,
    Bwrap,
}
label!(BinaryRole);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum SandboxBackend {
    Bubblewrap,
    Landlock,
    Seatbelt,
    AppContainer,
}
label!(SandboxBackend);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeStatus {
    Exited(i32),
    Signaled(i32),
}
variant!(NativeStatus { Exited(i32): "exited", Signaled(i32): "signaled" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnavailableFailure {
    MissingBootPin(BinaryRole),
    Unsupported(UnsupportedConfinement),
    BwrapProbe(UnavailableFailureBwrapProbe),
    Kernel(KernelFeatureFailure),
}
variant!(UnavailableFailure { MissingBootPin(BinaryRole): "missing-boot-pin", Unsupported(UnsupportedConfinement): "unsupported", BwrapProbe(UnavailableFailureBwrapProbe): "bwrap-probe", Kernel(KernelFeatureFailure): "kernel" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedConfinement {
    WindowsEveryPath(UnsupportedConfinementWindowsEveryPath),
    WindowsAllowanceExclusion(UnsupportedConfinementWindowsAllowanceExclusion),
}
variant!(UnsupportedConfinement { WindowsEveryPath(UnsupportedConfinementWindowsEveryPath): "windows-every-path", WindowsAllowanceExclusion(UnsupportedConfinementWindowsAllowanceExclusion): "windows-allowance-exclusion" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BwrapProbeFailure {
    Prepare(PrepareFailure),
    Spawn(OsFailure),
    Exited(BwrapProbeFailureExited),
    RootOpen(ProjectionOpenFailure),
    RootAbsent,
    Lend(ParcelFailure),
    RedirectedDestinationAccepted,
}
variant!(BwrapProbeFailure { Prepare(PrepareFailure): "prepare", Spawn(OsFailure): "spawn", Exited(BwrapProbeFailureExited): "exited", RootOpen(ProjectionOpenFailure): "root-open", RootAbsent: "root-absent", Lend(ParcelFailure): "lend", RedirectedDestinationAccepted: "redirected-destination-accepted" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelFeatureFailure {
    LandlockAbsent,
    LandlockProbe(OsFailure),
    CloseRangeAbsent,
}
variant!(KernelFeatureFailure { LandlockAbsent: "landlock-absent", LandlockProbe(OsFailure): "landlock-probe", CloseRangeAbsent: "close-range-absent" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareFailure {
    pub step: PrepareStep,
    pub subject: PrepareSubject,
    pub cause: PrepareCause,
}
record!(PrepareFailure {
    step: "step",
    subject: "subject",
    cause: "cause"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum PrepareStep {
    Stdio,
    MergeStderr,
    CapturePayloadStreams,
    Argv,
    Application,
    Environment,
    WorkingDirectory,
    AttributeList,
    HandleList,
    SecurityCapabilities,
    HandleInheritance,
    SessionLimit,
    GrantLimit,
    Group,
    SignalReset,
    ReadinessMark,
    DetachedFork,
    DetachedSession,
    DetachedHandshake,
    Job,
    AssignJob,
    ResumeThread,
    TerminalHandover,
    AppContainerProfileCreate,
    AppContainerProfileDeriveSid,
    NetworkCapabilitySids,
    FilesystemCapabilitySid,
    SidString,
}
label!(PrepareStep);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareSubject {
    Program(String),
    Stream(StandardStream),
    Argument(Count),
    EnvironmentKey(String),
    EnvironmentValue(String),
    Path(String),
    Group(Count),
    SessionLimit(SessionLimit),
    GrantLimit(GrantLimit),
    Capability(String),
    Child,
}
variant!(PrepareSubject { Program(String): "program", Stream(StandardStream): "stream", Argument(Count): "argument", EnvironmentKey(String): "environment-key", EnvironmentValue(String): "environment-value", Path(String): "path", Group(Count): "group", SessionLimit(SessionLimit): "session-limit", GrantLimit(GrantLimit): "grant-limit", Capability(String): "capability", Child: "child" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum SessionLimit {
    OpenFiles,
    CoreBytes,
    StackBytes,
}
label!(SessionLimit);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum GrantLimit {
    CoreBytes,
    Processes,
}
label!(GrantLimit);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareCause {
    Io(Box<IoCause>),
    Invalid(PrepareInvalid),
    Hresult(i32),
}
variant!(PrepareCause { Io(Box<IoCause>): "io", Invalid(PrepareInvalid): "invalid", Hresult(i32): "hresult" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareInvalid {
    Nul,
    GroupNoLongerLive(Count),
    GroupHasNoJob(Count),
    NoCapabilitySid(String),
}
variant!(PrepareInvalid { Nul: "nul", GroupNoLongerLive(Count): "group-no-longer-live", GroupHasNoJob(Count): "group-has-no-job", NoCapabilitySid(String): "no-capability-sid" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinFailure {
    pub role: BinaryRole,
    pub spelling: Option<String>,
    pub cause: PinCause,
}
record!(PinFailure {
    role: "role",
    spelling: "spelling",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinCause {
    Unavailable,
    VerificationWalk(Box<IoCause>),
    ChangedIdentity,
    CurrentNameRead(Box<IoCause>),
    CurrentExecutableRead(Box<IoCause>),
    RelativePath,
}
variant!(PinCause { Unavailable: "unavailable", VerificationWalk(Box<IoCause>): "verification-walk", ChangedIdentity: "changed-identity", CurrentNameRead(Box<IoCause>): "current-name-read", CurrentExecutableRead(Box<IoCause>): "current-executable-read", RelativePath: "relative-path" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionFailure {
    UnrenderablePath(String),
    ChangedSymlink(ProjectionFailureChangedSymlink),
    PinnedDirectoryRedirected(ProjectionFailurePinnedDirectoryRedirected),
    Open(ProjectionOpenFailure),
    Resolve(ProjectionFailureResolve),
    List(ProjectionFailureList),
    Carve(ProjectionFailureCarve),
    CopyHandle(ProjectionFailureCopyHandle),
    HiddenWorkingDirectory(ProjectionFailureHiddenWorkingDirectory),
    ProcExcluded(ProjectionFailureProcExcluded),
}
variant!(ProjectionFailure { UnrenderablePath(String): "unrenderable-path", ChangedSymlink(ProjectionFailureChangedSymlink): "changed-symlink", PinnedDirectoryRedirected(ProjectionFailurePinnedDirectoryRedirected): "pinned-directory-redirected", Open(ProjectionOpenFailure): "open", Resolve(ProjectionFailureResolve): "resolve", List(ProjectionFailureList): "list", Carve(ProjectionFailureCarve): "carve", CopyHandle(ProjectionFailureCopyHandle): "copy-handle", HiddenWorkingDirectory(ProjectionFailureHiddenWorkingDirectory): "hidden-working-directory", ProcExcluded(ProjectionFailureProcExcluded): "proc-excluded" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionOpenFailure {
    Linked(String),
    Failed(ProjectionOpenFailureFailed),
}
variant!(ProjectionOpenFailure { Linked(String): "linked", Failed(ProjectionOpenFailureFailed): "failed" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum CarveStep {
    OpenDirectory,
    ListDirectory,
    LocateEntry,
}
label!(CarveStep);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParcelFailure {
    pub role: ParcelRole,
    pub step: ParcelStep,
    pub cause: ParcelCause,
}
record!(ParcelFailure {
    role: "role",
    step: "step",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParcelRole {
    Warrant,
    Seccomp,
    BwrapArgs,
    ReportSocket,
    InfoSocket,
    Trampoline,
    Ruleset,
    Bind(String),
    PayloadStream(StandardStream),
    EnvelopeNullStreams,
    EnvelopeDiagnostics,
}
variant!(ParcelRole { Warrant: "warrant", Seccomp: "seccomp", BwrapArgs: "bwrap-args", ReportSocket: "report-socket", InfoSocket: "info-socket", Trampoline: "trampoline", Ruleset: "ruleset", Bind(String): "bind", PayloadStream(StandardStream): "payload-stream", EnvelopeNullStreams: "envelope-null-streams", EnvelopeDiagnostics: "envelope-diagnostics" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ParcelStep {
    Create,
    Write,
    Seal,
    Rewind,
    ReadBack,
    Lend,
    Keep,
    Lift,
    SetSendBuffer,
    SetReceiveBuffer,
    SetNonblocking,
}
label!(ParcelStep);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParcelCause {
    Io(Box<IoCause>),
    ChangedBytes,
    SocketTooSmall(ParcelCauseSocketTooSmall),
}
variant!(ParcelCause { Io(Box<IoCause>): "io", ChangedBytes: "changed-bytes", SocketTooSmall(ParcelCauseSocketTooSmall): "socket-too-small" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesetFailure {
    Create(OsFailure),
    Admit(RulesetFailureAdmit),
    Open(ProjectionOpenFailure),
    Restrict(OsFailure),
    MissingFd(i32),
    MissingRoot,
}
variant!(RulesetFailure { Create(OsFailure): "create", Admit(RulesetFailureAdmit): "admit", Open(ProjectionOpenFailure): "open", Restrict(OsFailure): "restrict", MissingFd(i32): "missing-fd", MissingRoot: "missing-root" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterFailure {
    Arch(FilterFailureArch),
    Compile(FilterFailureCompile),
}
variant!(FilterFailure { Arch(FilterFailureArch): "arch", Compile(FilterFailureCompile): "compile" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarrantFailure {
    MissingTerminator,
    Magic,
    MissingField(WarrantField),
    Utf8(WarrantFailureUtf8),
    Confinement(String),
    Slot(WarrantFailureSlot),
    SlotCount(WarrantFailureSlotCount),
    DuplicateSlot(i32),
    MissingDescriptor(WarrantFailureMissingDescriptor),
    NotReportSocket(i32),
    UnknownImageKind(String),
    InvalidProgram(WarrantFailureInvalidProgram),
    UnknownBundledTool(String),
    ArgumentCountSyntax(String),
    ArgumentCount(WarrantFailureArgumentCount),
    Nul(WarrantField),
    Noncanonical,
    Size(WarrantFailureSize),
    Read(Box<IoCause>),
    SealProbe(OsFailure),
    MissingSeals,
    Rewind(OsFailure),
    NotSocket,
    SocketStat(OsFailure),
    StreamInstall(WarrantFailureStreamInstall),
    DescriptorSweep(DescriptorSweepFailure),
}
variant!(WarrantFailure { MissingTerminator: "missing-terminator", Magic: "magic", MissingField(WarrantField): "missing-field", Utf8(WarrantFailureUtf8): "utf8", Confinement(String): "confinement", Slot(WarrantFailureSlot): "slot", SlotCount(WarrantFailureSlotCount): "slot-count", DuplicateSlot(i32): "duplicate-slot", MissingDescriptor(WarrantFailureMissingDescriptor): "missing-descriptor", NotReportSocket(i32): "not-report-socket", UnknownImageKind(String): "unknown-image-kind", InvalidProgram(WarrantFailureInvalidProgram): "invalid-program", UnknownBundledTool(String): "unknown-bundled-tool", ArgumentCountSyntax(String): "argument-count-syntax", ArgumentCount(WarrantFailureArgumentCount): "argument-count", Nul(WarrantField): "nul", Noncanonical: "noncanonical", Size(WarrantFailureSize): "size", Read(Box<IoCause>): "read", SealProbe(OsFailure): "seal-probe", MissingSeals: "missing-seals", Rewind(OsFailure): "rewind", NotSocket: "not-socket", SocketStat(OsFailure): "socket-stat", StreamInstall(WarrantFailureStreamInstall): "stream-install", DescriptorSweep(DescriptorSweepFailure): "descriptor-sweep" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarrantField {
    Confinement,
    ImageKind,
    Program,
    Argv0,
    ArgumentCount,
    Argument(Count),
    BwrapOption(Count),
}
variant!(WarrantField { Confinement: "confinement", ImageKind: "image-kind", Program: "program", Argv0: "argv0", ArgumentCount: "argument-count", Argument(Count): "argument", BwrapOption(Count): "bwrap-option" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffField {
    Warrant,
    Report,
    Stream(StandardStream),
    Ruleset,
}
variant!(HandoffField { Warrant: "warrant", Report: "report", Stream(StandardStream): "stream", Ruleset: "ruleset" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorSweepFailure {
    CloseRange(DescriptorSweepFailureCloseRange),
    SetReportCloexec(DescriptorSweepFailureSetReportCloexec),
    BufferTooLarge(DescriptorSweepFailureBufferTooLarge),
    ListDescriptors(OsFailure),
    PartialDescriptorRecord(DescriptorSweepFailurePartialDescriptorRecord),
    CloseDescriptor(DescriptorSweepFailureCloseDescriptor),
}
variant!(DescriptorSweepFailure { CloseRange(DescriptorSweepFailureCloseRange): "close-range", SetReportCloexec(DescriptorSweepFailureSetReportCloexec): "set-report-cloexec", BufferTooLarge(DescriptorSweepFailureBufferTooLarge): "buffer-too-large", ListDescriptors(OsFailure): "list-descriptors", PartialDescriptorRecord(DescriptorSweepFailurePartialDescriptorRecord): "partial-descriptor-record", CloseDescriptor(DescriptorSweepFailureCloseDescriptor): "close-descriptor" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatbeltFailure {
    AlreadyConfined(SeatbeltFailureAlreadyConfined),
    Profile(ProjectionFailure),
    Nul,
    Entry(SeatbeltFailureEntry),
}
variant!(SeatbeltFailure { AlreadyConfined(SeatbeltFailureAlreadyConfined): "already-confined", Profile(ProjectionFailure): "profile", Nul: "nul", Entry(SeatbeltFailureEntry): "entry" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaclFailure {
    NotLocal(String),
    NotUnicode(String),
    Redirected(DaclFailureRedirected),
    WriteDacDenied(DaclFailureWriteDacDenied),
    Reachable(DaclFailureReachable),
    AccessCheck(DaclFailureAccessCheck),
    Win32(DaclFailureWin32),
    LedgerIo(LedgerIoFailure),
    LedgerParse(DaclFailureLedgerParse),
    InvalidSid(SidFailure),
    MutexTimeout(DaclFailureMutexTimeout),
}
variant!(DaclFailure { NotLocal(String): "not-local", NotUnicode(String): "not-unicode", Redirected(DaclFailureRedirected): "redirected", WriteDacDenied(DaclFailureWriteDacDenied): "write-dac-denied", Reachable(DaclFailureReachable): "reachable", AccessCheck(DaclFailureAccessCheck): "access-check", Win32(DaclFailureWin32): "win32", LedgerIo(LedgerIoFailure): "ledger-io", LedgerParse(DaclFailureLedgerParse): "ledger-parse", InvalidSid(SidFailure): "invalid-sid", MutexTimeout(DaclFailureMutexTimeout): "mutex-timeout" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum RedirectedTargetKind {
    MountedVolume,
    JunctionOrLink,
    ReparsePoint,
}
label!(RedirectedTargetKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum AccessCheckOperation {
    InitializeResourceManager,
    OpenProcessToken,
    InitializeContextFromToken,
    SetAppContainerInformation,
    Check,
}
label!(AccessCheckOperation);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum DaclOperation {
    OpenFrozenPath,
    ProbeLocalVolume,
    ReopenFrozenPath,
    Metadata,
    GetSecurityInfo,
    SetSecurityInfo,
    SetSecurityInfoLabel,
    SetEntriesInAcl,
    InitializeAcl,
    AddMandatoryAce,
    GetAce,
    WalkExclusion,
    AccessCheck,
    CreateMutex,
    WaitMutex,
    GetProcessTimes,
}
label!(DaclOperation);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaclWin32Cause {
    Io(Box<IoCause>),
    UnexpectedWaitResult(u32),
}
variant!(DaclWin32Cause { Io(Box<IoCause>): "io", UnexpectedWaitResult(u32): "unexpected-wait-result" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerIoFailure {
    Io(Box<IoCause>),
    RetryExhausted(LedgerIoFailureRetryExhausted),
}
variant!(LedgerIoFailure { Io(Box<IoCause>): "io", RetryExhausted(LedgerIoFailureRetryExhausted): "retry-exhausted" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidFailure {
    Empty,
    TooLong(SidFailureTooLong),
    Parse(SidFailureParse),
    Invalid(String),
}
variant!(SidFailure { Empty: "empty", TooLong(SidFailureTooLong): "too-long", Parse(SidFailureParse): "parse", Invalid(String): "invalid" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetFailure {
    CountProcesses(BudgetCountFailure),
    SetBudget(BudgetFailureSetBudget),
}
variant!(BudgetFailure { CountProcesses(BudgetCountFailure): "count-processes", SetBudget(BudgetFailureSetBudget): "set-budget" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum BudgetSubject {
    ChildProcesses,
    ChildJob,
}
label!(BudgetSubject);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetCountFailure {
    Kernel(BudgetCountFailureKernel),
    InvalidCeiling(i32),
    CapacityUnaddressable(Count),
    BufferTooLarge(Count),
}
variant!(BudgetCountFailure { Kernel(BudgetCountFailureKernel): "kernel", InvalidCeiling(i32): "invalid-ceiling", CapacityUnaddressable(Count): "capacity-unaddressable", BufferTooLarge(Count): "buffer-too-large" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum BudgetProbeOperation {
    PerUserCeiling,
    LiveProcesses,
}
label!(BudgetProbeOperation);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetSetFailure {
    CreateJob(OsFailure),
    SetJobLimit(OsFailure),
    SetProcessLimit(OsFailure),
}
variant!(BudgetSetFailure { CreateJob(OsFailure): "create-job", SetJobLimit(OsFailure): "set-job-limit", SetProcessLimit(OsFailure): "set-process-limit" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JailFailure {
    Sequence(JailFailureSequence),
    NoParent(String),
    Delegate(JailFailureDelegate),
    Create(JailFailureCreate),
    Limit(JailFailureLimit),
    Membership(JailFailureMembership),
}
variant!(JailFailure { Sequence(JailFailureSequence): "sequence", NoParent(String): "no-parent", Delegate(JailFailureDelegate): "delegate", Create(JailFailureCreate): "create", Limit(JailFailureLimit): "limit", Membership(JailFailureMembership): "membership" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceFailure {
    CreateParent(Box<IoCause>),
    Open(Box<IoCause>),
    Lock(OsFailure),
    Read(Box<IoCause>),
    Malformed(Vec<u8>),
    Exhausted,
    Write(Box<IoCause>),
}
variant!(SequenceFailure { CreateParent(Box<IoCause>): "create-parent", Open(Box<IoCause>): "open", Lock(OsFailure): "lock", Read(Box<IoCause>): "read", Malformed(Vec<u8>): "malformed", Exhausted: "exhausted", Write(Box<IoCause>): "write" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum CgroupLimitFile {
    MemoryMax,
    PidsMax,
    CpuMax,
}
label!(CgroupLimitFile);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum MembershipStep {
    OpenProcs,
    WritePid,
    UnshareNetwork,
    LoopbackSocket,
    EnableLoopback,
    DropGroups,
    SetGid,
    SetUid,
    NoNewPrivileges,
    MarkDescriptors,
    InstallFilter,
}
label!(MembershipStep);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherFailure {
    Exec(LauncherFailureExec),
    DiedBeforeReport(LauncherFailureDiedBeforeReport),
    ReportRead(Box<IoCause>),
    ReportUnknownTag(u8),
    ReportEndedBeforeStarted,
    ReportDecode(JsonInputFailure),
    ReportEncodingTooLarge(LauncherFailureReportEncodingTooLarge),
    PayloadGroup(PayloadGroupFailure),
    ChildPipe(LauncherFailureChildPipe),
    ChildFork(LauncherFailureChildFork),
    ChildSetup(LauncherFailureChildSetup),
    ChildDiedBeforeSetup(LaunchImageSubject),
    SpawnBeforeSetup(LauncherFailureSpawnBeforeSetup),
    NonNativeExecError(LauncherFailureNonNativeExecError),
    AnchorReadiness(AnchorReadinessFailure),
}
variant!(LauncherFailure { Exec(LauncherFailureExec): "exec", DiedBeforeReport(LauncherFailureDiedBeforeReport): "died-before-report", ReportRead(Box<IoCause>): "report-read", ReportUnknownTag(u8): "report-unknown-tag", ReportEndedBeforeStarted: "report-ended-before-started", ReportDecode(JsonInputFailure): "report-decode", ReportEncodingTooLarge(LauncherFailureReportEncodingTooLarge): "report-encoding-too-large", PayloadGroup(PayloadGroupFailure): "payload-group", ChildPipe(LauncherFailureChildPipe): "child-pipe", ChildFork(LauncherFailureChildFork): "child-fork", ChildSetup(LauncherFailureChildSetup): "child-setup", ChildDiedBeforeSetup(LaunchImageSubject): "child-died-before-setup", SpawnBeforeSetup(LauncherFailureSpawnBeforeSetup): "spawn-before-setup", NonNativeExecError(LauncherFailureNonNativeExecError): "non-native-exec-error", AnchorReadiness(AnchorReadinessFailure): "anchor-readiness" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum LauncherRole {
    RalTrampoline,
    Bwrap,
}
label!(LauncherRole);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchImageSubject {
    File(String),
    Bundled(String),
}
variant!(LaunchImageSubject { File(String): "file", Bundled(String): "bundled" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadGroupFailure {
    Read(Box<IoCause>),
    Json(JsonInputFailure),
    InvalidPid(i32),
}
variant!(PayloadGroupFailure { Read(Box<IoCause>): "read", Json(JsonInputFailure): "json", InvalidPid(i32): "invalid-pid" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorReadinessFailure {
    Read(Box<IoCause>),
    UnexpectedByte(u8),
}
variant!(AnchorReadinessFailure { Read(Box<IoCause>): "read", UnexpectedByte(u8): "unexpected-byte" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedPidFailure {
    pub program: String,
    pub cause: DetachedPidCause,
}
record!(DetachedPidFailure {
    program: "program",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetachedPidCause {
    ReaperSilent,
    Reap(Box<IoCause>),
    IntermediateEnded(NativeStatus),
    ReceiptRead(Box<IoCause>),
    InvalidPid(i32),
}
variant!(DetachedPidCause { ReaperSilent: "reaper-silent", Reap(Box<IoCause>): "reap", IntermediateEnded(NativeStatus): "intermediate-ended", ReceiptRead(Box<IoCause>): "receipt-read", InvalidPid(i32): "invalid-pid" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableFailureBwrapProbe {
    pub binary: String,
    pub cause: BwrapProbeFailure,
}
record!(UnavailableFailureBwrapProbe {
    binary: "binary",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedConfinementWindowsEveryPath {
    pub excluding: bool,
}
record!(UnsupportedConfinementWindowsEveryPath {
    excluding: "excluding"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedConfinementWindowsAllowanceExclusion {
    pub granted: String,
    pub denied: String,
}
record!(UnsupportedConfinementWindowsAllowanceExclusion {
    granted: "granted",
    denied: "denied"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BwrapProbeFailureExited {
    pub status: NativeStatus,
    pub stderr: Option<String>,
}
record!(BwrapProbeFailureExited {
    status: "status",
    stderr: "stderr"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailureChangedSymlink {
    pub granted: String,
}
record!(ProjectionFailureChangedSymlink { granted: "granted" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailurePinnedDirectoryRedirected {
    pub granted: String,
    pub reached: String,
}
record!(ProjectionFailurePinnedDirectoryRedirected {
    granted: "granted",
    reached: "reached"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailureResolve {
    pub path: String,
    pub cause: Box<IoCause>,
}
record!(ProjectionFailureResolve {
    path: "path",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailureList {
    pub path: String,
    pub cause: Box<IoCause>,
}
record!(ProjectionFailureList {
    path: "path",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailureCarve {
    pub path: String,
    pub step: CarveStep,
    pub cause: Box<IoCause>,
}
record!(ProjectionFailureCarve {
    path: "path",
    step: "step",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailureCopyHandle {
    pub path: String,
    pub cause: Box<IoCause>,
}
record!(ProjectionFailureCopyHandle {
    path: "path",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailureHiddenWorkingDirectory {
    pub cwd: String,
    pub exclusion: String,
}
record!(ProjectionFailureHiddenWorkingDirectory {
    cwd: "cwd",
    exclusion: "exclusion"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailureProcExcluded {
    pub exclusion: String,
}
record!(ProjectionFailureProcExcluded {
    exclusion: "exclusion"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionOpenFailureFailed {
    pub path: String,
    pub cause: Box<IoCause>,
}
record!(ProjectionOpenFailureFailed {
    path: "path",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParcelCauseSocketTooSmall {
    pub bytes: Count,
    pub cause: Box<IoCause>,
}
record!(ParcelCauseSocketTooSmall {
    bytes: "bytes",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesetFailureAdmit {
    pub name: String,
    pub cause: Box<IoCause>,
}
record!(RulesetFailureAdmit {
    name: "name",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterFailureArch {
    pub architecture: String,
    pub detail: String,
}
record!(FilterFailureArch {
    architecture: "architecture",
    detail: "detail"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterFailureCompile {
    pub detail: String,
}
record!(FilterFailureCompile { detail: "detail" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarrantFailureUtf8 {
    pub field: WarrantField,
    pub valid_up_to: Count,
    pub error_len: Option<Count>,
}
record!(WarrantFailureUtf8 {
    field: "field",
    valid_up_to: "valid-up-to",
    error_len: "error-len"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarrantFailureSlot {
    pub field: HandoffField,
    pub spelling: String,
}
record!(WarrantFailureSlot {
    field: "field",
    spelling: "spelling"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarrantFailureSlotCount {
    pub actual: Count,
}
record!(WarrantFailureSlotCount { actual: "actual" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarrantFailureMissingDescriptor {
    pub field: HandoffField,
    pub fd: i32,
}
record!(WarrantFailureMissingDescriptor {
    field: "field",
    fd: "fd"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarrantFailureInvalidProgram {
    pub shown: String,
    pub cause: Box<IoCause>,
}
record!(WarrantFailureInvalidProgram {
    shown: "shown",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarrantFailureArgumentCount {
    pub declared: Count,
    pub actual: Count,
}
record!(WarrantFailureArgumentCount {
    declared: "declared",
    actual: "actual"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarrantFailureSize {
    pub actual: Count,
    pub limit: Count,
}
record!(WarrantFailureSize {
    actual: "actual",
    limit: "limit"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarrantFailureStreamInstall {
    pub stream: StandardStream,
    pub source: i32,
    pub cause: OsFailure,
}
record!(WarrantFailureStreamInstall {
    stream: "stream",
    source: "source",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorSweepFailureCloseRange {
    pub first: Count,
    pub last: Count,
    pub cause: OsFailure,
}
record!(DescriptorSweepFailureCloseRange {
    first: "first",
    last: "last",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorSweepFailureSetReportCloexec {
    pub fd: i32,
    pub cause: OsFailure,
}
record!(DescriptorSweepFailureSetReportCloexec {
    fd: "fd",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorSweepFailureBufferTooLarge {
    pub bytes: Count,
}
record!(DescriptorSweepFailureBufferTooLarge { bytes: "bytes" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorSweepFailurePartialDescriptorRecord {
    pub bytes: Count,
    pub width: Count,
}
record!(DescriptorSweepFailurePartialDescriptorRecord {
    bytes: "bytes",
    width: "width"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorSweepFailureCloseDescriptor {
    pub fd: i32,
    pub cause: OsFailure,
}
record!(DescriptorSweepFailureCloseDescriptor {
    fd: "fd",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatbeltFailureAlreadyConfined {
    pub detail: Option<String>,
}
record!(SeatbeltFailureAlreadyConfined { detail: "detail" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatbeltFailureEntry {
    pub cause: OsFailure,
    pub detail: Option<String>,
}
record!(SeatbeltFailureEntry {
    cause: "cause",
    detail: "detail"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaclFailureRedirected {
    pub granted: String,
    pub at: String,
    pub target_kind: RedirectedTargetKind,
}
record!(DaclFailureRedirected {
    granted: "granted",
    at: "at",
    target_kind: "target-kind"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaclFailureWriteDacDenied {
    pub path: String,
    pub operation: DaclOperation,
    pub cause: OsFailure,
}
record!(DaclFailureWriteDacDenied {
    path: "path",
    operation: "operation",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaclFailureReachable {
    pub denied: String,
    pub at: String,
}
record!(DaclFailureReachable {
    denied: "denied",
    at: "at"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaclFailureAccessCheck {
    pub operation: AccessCheckOperation,
    pub cause: OsFailure,
}
record!(DaclFailureAccessCheck {
    operation: "operation",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaclFailureWin32 {
    pub path: Option<String>,
    pub operation: DaclOperation,
    pub cause: DaclWin32Cause,
}
record!(DaclFailureWin32 {
    path: "path",
    operation: "operation",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaclFailureLedgerParse {
    pub path: String,
    pub cause: JsonInputFailure,
}
record!(DaclFailureLedgerParse {
    path: "path",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaclFailureMutexTimeout {
    pub path: String,
    pub timeout_ms: Count,
}
record!(DaclFailureMutexTimeout {
    path: "path",
    timeout_ms: "timeout-ms"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerIoFailureRetryExhausted {
    pub attempts: Count,
}
record!(LedgerIoFailureRetryExhausted {
    attempts: "attempts"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidFailureTooLong {
    pub bytes: Count,
    pub limit: Count,
}
record!(SidFailureTooLong {
    bytes: "bytes",
    limit: "limit"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidFailureParse {
    pub text: String,
    pub cause: OsFailure,
}
record!(SidFailureParse {
    text: "text",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetFailureSetBudget {
    pub subject: BudgetSubject,
    pub limit: Count,
    pub cause: BudgetSetFailure,
}
record!(BudgetFailureSetBudget {
    subject: "subject",
    limit: "limit",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetCountFailureKernel {
    pub operation: BudgetProbeOperation,
    pub cause: OsFailure,
}
record!(BudgetCountFailureKernel {
    operation: "operation",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailFailureSequence {
    pub path: String,
    pub cause: SequenceFailure,
}
record!(JailFailureSequence {
    path: "path",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailFailureDelegate {
    pub parent: String,
    pub cause: Box<IoCause>,
}
record!(JailFailureDelegate {
    parent: "parent",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailFailureCreate {
    pub cgroup: String,
    pub cause: Box<IoCause>,
}
record!(JailFailureCreate {
    cgroup: "cgroup",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailFailureLimit {
    pub cgroup: String,
    pub file: CgroupLimitFile,
    pub cause: Box<IoCause>,
}
record!(JailFailureLimit {
    cgroup: "cgroup",
    file: "file",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailFailureMembership {
    pub cgroup: String,
    pub step: MembershipStep,
    pub cause: OsFailure,
}
record!(JailFailureMembership {
    cgroup: "cgroup",
    step: "step",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherFailureExec {
    pub launcher: LauncherRole,
    pub cause: OsFailure,
}
record!(LauncherFailureExec {
    launcher: "launcher",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherFailureDiedBeforeReport {
    pub launcher: LauncherRole,
    pub stderr: Option<String>,
}
record!(LauncherFailureDiedBeforeReport {
    launcher: "launcher",
    stderr: "stderr"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherFailureReportEncodingTooLarge {
    pub actual: Count,
    pub limit: Count,
}
record!(LauncherFailureReportEncodingTooLarge {
    actual: "actual",
    limit: "limit"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherFailureChildPipe {
    pub subject: LaunchImageSubject,
    pub cause: OsFailure,
}
record!(LauncherFailureChildPipe {
    subject: "subject",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherFailureChildFork {
    pub subject: LaunchImageSubject,
    pub cause: OsFailure,
}
record!(LauncherFailureChildFork {
    subject: "subject",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherFailureChildSetup {
    pub subject: LaunchImageSubject,
    pub cause: OsFailure,
}
record!(LauncherFailureChildSetup {
    subject: "subject",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherFailureSpawnBeforeSetup {
    pub subject: LaunchImageSubject,
    pub cause: OsFailure,
}
record!(LauncherFailureSpawnBeforeSetup {
    subject: "subject",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherFailureNonNativeExecError {
    pub subject: LaunchImageSubject,
    pub cause: Box<IoCause>,
}
record!(LauncherFailureNonNativeExecError {
    subject: "subject",
    cause: "cause"
});

impl LaunchFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Unavailable(cause) => {
                format!("sandbox confinement unavailable: {}", cause.describe())
            }
            Self::Prepare(cause) => cause.describe(),
            Self::Pin(cause) => cause.describe(),
            Self::Projection(cause) => cause.describe(),
            Self::Parcel(cause) => cause.describe(),
            Self::Ruleset(cause) => cause.describe(),
            Self::Filter(cause) => cause.describe(),
            Self::Warrant(cause) => cause.describe(),
            Self::Seatbelt(cause) => cause.describe(),
            Self::Dacl(cause @ DaclFailure::Reachable(_)) => cause.describe(),
            Self::Dacl(cause) => format!("AppContainer confinement failed: {}", cause.describe()),
            Self::Budget(cause) => cause.describe(),
            Self::Jail(cause) => format!("guest jail: {}", cause.describe()),
            Self::Launcher(cause) => cause.describe(),
            Self::BatchImage(program) => format!("refused Windows batch program {program:?}"),
            Self::DetachedPid(cause) => cause.describe(),
        }
    }
}

impl BinaryRole {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl SandboxBackend {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl NativeStatus {
    pub fn describe(&self) -> String {
        match self {
            Self::Exited(value) => format!("exited: {value}"),
            Self::Signaled(value) => format!("signaled: {value}"),
        }
    }
}

impl UnavailableFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::MissingBootPin(BinaryRole::Bwrap) => "ral could not find and pin bwrap at startup; a grant confines every external command under bubblewrap, so nothing can start under this grant without it".into(),
            Self::MissingBootPin(BinaryRole::Ral) => PinCause::Unavailable.describe(),
            Self::Unsupported(value) => value.describe(),
            Self::BwrapProbe(value) => value.describe(),
            Self::Kernel(value) => value.describe(),
        }
    }
}

impl UnsupportedConfinement {
    pub fn describe(&self) -> String {
        match self {
            Self::WindowsEveryPath(value) => value.describe(),
            Self::WindowsAllowanceExclusion(value) => value.describe(),
        }
    }
}

impl BwrapProbeFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Prepare(failure) => format!("ral could not prepare the bwrap probe: {}", failure.describe()),
            Self::Spawn(value) => format!("it did not start: {}", value.describe()),
            Self::Exited(value) => value.describe(),
            Self::RootOpen(value) => format!("ral cannot open / to lend bwrap a descriptor: {}", value.describe()),
            Self::RootAbsent => "ral cannot open / to lend bwrap a descriptor: no root object was found".into(),
            Self::Lend(value) => value.describe(),
            Self::RedirectedDestinationAccepted => "it mounts a lent descriptor wherever a symlink at its destination points, rather than refusing it".into(),
        }
    }
}

impl KernelFeatureFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::LandlockAbsent => "Landlock is unavailable, so this kernel cannot enforce the grant's limits on which programs may run; ral refuses rather than run them unchecked".into(),
            Self::LandlockProbe(value) => format!("the Landlock version probe failed: {}; this does not establish that Landlock is absent, so ral refuses to launch confined", value.describe()),
            Self::CloseRangeAbsent => "this kernel has no close_range(2) (Linux 5.9); the handoff cannot be swept, so the program would inherit it; refusing to run".into(),
        }
    }
}

impl PrepareFailure {
    pub fn describe(&self) -> String {
        let operation = match self.step {
            PrepareStep::Stdio => match &self.subject {
                PrepareSubject::Program(program) => {
                    format!("connect standard streams for program '{program}'")
                }
                PrepareSubject::Stream(stream) => {
                    format!("prepare the child's {}", stream.describe())
                }
                _ => "prepare the child's standard streams".into(),
            },
            PrepareStep::MergeStderr => "connect the child's stderr to stdout".into(),
            PrepareStep::CapturePayloadStreams => {
                "preserve the payload's standard streams before starting its envelope".into()
            }
            PrepareStep::Argv => match &self.subject {
                PrepareSubject::Argument(index) => format!("prepare argv[{index}]"),
                _ => "prepare the child's argv".into(),
            },
            PrepareStep::Application => format!("prepare {} for launch", self.subject.describe()),
            PrepareStep::Environment => match &self.subject {
                PrepareSubject::EnvironmentKey(key) => format!("prepare environment key {key}"),
                PrepareSubject::EnvironmentValue(key) => {
                    format!("prepare the value of environment key {key}")
                }
                _ => "prepare the child's environment".into(),
            },
            PrepareStep::WorkingDirectory => match &self.subject {
                PrepareSubject::Path(path) => format!("prepare working directory '{path}'"),
                _ => "prepare the child's working directory".into(),
            },
            PrepareStep::AttributeList => "initialize the Windows process attribute list".into(),
            PrepareStep::HandleList => "install the child's inherited-handle list".into(),
            PrepareStep::SecurityCapabilities => {
                "install the child's AppContainer security capabilities".into()
            }
            PrepareStep::HandleInheritance => {
                "make the admitted handles inheritable by the child".into()
            }
            PrepareStep::SessionLimit => match &self.subject {
                PrepareSubject::SessionLimit(limit) => {
                    format!("apply the session's {} limit", limit.describe())
                }
                _ => "apply the session's resource limit".into(),
            },
            PrepareStep::GrantLimit => match &self.subject {
                PrepareSubject::GrantLimit(limit) => {
                    format!("apply the grant's {} limit", limit.describe())
                }
                _ => "apply the grant's resource limit".into(),
            },
            PrepareStep::Group => match &self.subject {
                PrepareSubject::Group(group) => format!("place the child in process group {group}"),
                _ => "establish the child's process-group placement".into(),
            },
            PrepareStep::SignalReset => "reset the child's signals".into(),
            PrepareStep::ReadinessMark => "establish the child's exec-readiness evidence".into(),
            PrepareStep::DetachedFork => "fork the detached survivor".into(),
            PrepareStep::DetachedSession => "create the detached survivor's session".into(),
            PrepareStep::DetachedHandshake => {
                "establish the detached launch's PID handshake".into()
            }
            PrepareStep::Job => "prepare the child's Windows Job Object".into(),
            PrepareStep::AssignJob => "place the suspended child in its Windows Job Object".into(),
            PrepareStep::ResumeThread => {
                "resume the child after assigning its Windows Job Object".into()
            }
            PrepareStep::TerminalHandover => {
                format!("hand the terminal to {}", self.subject.describe())
            }
            PrepareStep::AppContainerProfileCreate => {
                format!("create AppContainer profile {}", self.subject.describe())
            }
            PrepareStep::AppContainerProfileDeriveSid => format!(
                "derive the SID of existing AppContainer profile {}",
                self.subject.describe()
            ),
            PrepareStep::NetworkCapabilitySids => "derive the network capability SIDs".into(),
            PrepareStep::FilesystemCapabilitySid => format!(
                "derive filesystem capability SID {}",
                self.subject.describe()
            ),
            PrepareStep::SidString => {
                format!("render the SID for capability {}", self.subject.describe())
            }
        };
        format!("ral could not {operation}: {}", self.cause.describe())
    }
}

impl PrepareStep {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl PrepareSubject {
    pub fn describe(&self) -> String {
        match self {
            Self::Program(value) => format!("program '{value}'"),
            Self::Stream(value) => value.describe(),
            Self::Argument(value) => format!("argv[{value}]"),
            Self::EnvironmentKey(value) => format!("environment key {value}"),
            Self::EnvironmentValue(value) => format!("the value of environment key {value}"),
            Self::Path(value) | Self::Capability(value) => format!("'{value}'"),
            Self::Group(value) => format!("process group {value}"),
            Self::SessionLimit(value) => format!("the session's {} limit", value.describe()),
            Self::GrantLimit(value) => format!("the grant's {} limit", value.describe()),
            Self::Child => "the child".into(),
        }
    }
}

impl SessionLimit {
    pub fn describe(&self) -> String {
        match self {
            Self::OpenFiles => "open-file",
            Self::CoreBytes => "core-file byte",
            Self::StackBytes => "stack-byte",
        }
        .into()
    }
}

impl GrantLimit {
    pub fn describe(&self) -> String {
        match self {
            Self::CoreBytes => "core-file byte",
            Self::Processes => "process",
        }
        .into()
    }
}

impl PrepareCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Io(value) => value.describe(),
            Self::Invalid(value) => value.describe(),
            Self::Hresult(code) => {
                format!("Windows returned HRESULT 0x{:08x}", code.cast_unsigned())
            }
        }
    }
}

impl PrepareInvalid {
    pub fn describe(&self) -> String {
        match self {
            Self::Nul => {
                "it contains a NUL byte, which cannot cross the process-launch boundary".into()
            }
            Self::GroupNoLongerLive(group) => format!("pipeline group {group} is no longer live"),
            Self::GroupHasNoJob(group) => {
                format!("pipeline group {group} has no Windows Job Object")
            }
            Self::NoCapabilitySid(name) => {
                format!("Windows returned no capability SID for '{name}'")
            }
        }
    }
}

impl PinFailure {
    pub fn describe(&self) -> String {
        let binary = format!(
            "{} binary{}",
            self.role.describe(),
            self.spelling
                .as_ref()
                .map_or_else(String::new, |spelling| format!(" at '{spelling}'"))
        );
        match &self.cause {
            PinCause::Unavailable => self.cause.describe(),
            PinCause::VerificationWalk(cause) => {
                format!("cannot verify the pinned {binary}: {}", cause.describe())
            }
            PinCause::ChangedIdentity => format!("{binary} changed since startup"),
            PinCause::CurrentNameRead(cause) => {
                let spelling = self
                    .spelling
                    .as_ref()
                    .map_or_else(String::new, |spelling| format!(" '{spelling}'"));
                format!(
                    "cannot find where the pinned {} binary{spelling} lives now: {}",
                    self.role.describe(),
                    cause.describe()
                )
            }
            PinCause::CurrentExecutableRead(cause) => {
                format!("cannot resolve ral's own program: {}", cause.describe())
            }
            PinCause::RelativePath => self.spelling.as_ref().map_or_else(
                || "a pin must name an absolute path".into(),
                |spelling| {
                    format!("a pin must name an absolute path, but '{spelling}' is relative")
                },
            ),
        }
    }
}

impl PinCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Unavailable => "ral could not pin its own program at startup, so it cannot vouch that the sandboxed copy is itself; refusing to launch it".into(),
            Self::VerificationWalk(value) => {
                format!("verification walk: {}", value.as_ref().describe())
            }
            Self::ChangedIdentity => "changed identity".to_owned(),
            Self::CurrentNameRead(value) => {
                format!("current name read: {}", value.as_ref().describe())
            }
            Self::CurrentExecutableRead(value) => {
                format!("current executable read: {}", value.as_ref().describe())
            }
            Self::RelativePath => "relative path".to_owned(),
        }
    }
}

impl ProjectionFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::UnrenderablePath(path) => format!(
                "sandbox grant path {path} is not valid Unicode; refusing to render a sandbox rule that could name the wrong path"
            ),
            Self::ChangedSymlink(value) => value.describe(),
            Self::PinnedDirectoryRedirected(value) => value.describe(),
            Self::Open(value) => value.describe(),
            Self::Resolve(value) => value.describe(),
            Self::List(value) => value.describe(),
            Self::Carve(value) => value.describe(),
            Self::CopyHandle(value) => value.describe(),
            Self::HiddenWorkingDirectory(value) => value.describe(),
            Self::ProcExcluded(value) => value.describe(),
        }
    }
}

impl ProjectionOpenFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Linked(granted) => format!(
                "'{granted}' was granted by its real path, but it now goes through a symbolic link"
            ),
            Self::Failed(value) => value.describe(),
        }
    }
}

impl CarveStep {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ParcelFailure {
    pub fn describe(&self) -> String {
        match &self.cause {
            ParcelCause::ChangedBytes | ParcelCause::SocketTooSmall(_) => self.cause.describe(),
            ParcelCause::Io(cause) => {
                let operation = match self.step {
                    ParcelStep::Lend | ParcelStep::Keep => format!(
                        "prepare a descriptor for {} to hand to the confined child",
                        self.role.describe()
                    ),
                    ParcelStep::Lift => format!(
                        "copy a descriptor for {} to fd 98 or above for the confined child",
                        self.role.describe()
                    ),
                    ParcelStep::Create if self.role == ParcelRole::InfoSocket => {
                        format!("open {}", self.role.describe())
                    }
                    ParcelStep::SetSendBuffer => {
                        format!("set the send buffer for {}", self.role.describe())
                    }
                    ParcelStep::SetReceiveBuffer => {
                        format!("set the receive buffer for {}", self.role.describe())
                    }
                    ParcelStep::SetNonblocking => {
                        format!("make {} nonblocking", self.role.describe())
                    }
                    _ => format!("{} {}", self.step.describe(), self.role.describe()),
                };
                format!("cannot {operation}: {}", cause.describe())
            }
        }
    }
}

impl ParcelRole {
    pub fn describe(&self) -> String {
        match self {
            Self::Warrant => "the warrant parcel".into(),
            Self::Seccomp => "the seccomp filter parcel".into(),
            Self::BwrapArgs => "the bwrap arguments parcel".into(),
            Self::ReportSocket => "the confined launch's report socket".into(),
            Self::InfoSocket => "bwrap's --info-fd socket".into(),
            Self::Trampoline => "the trampoline program".into(),
            Self::Ruleset => "the Landlock ruleset descriptor".into(),
            Self::Bind(value) => format!("the bind mount at '{value}'"),
            Self::PayloadStream(value) => format!("payload {}", value.describe()),
            Self::EnvelopeNullStreams => "the envelope's null streams".into(),
            Self::EnvelopeDiagnostics => "the envelope's diagnostic stream".into(),
        }
    }
}

impl ParcelStep {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ParcelCause {
    pub fn describe(&self) -> String {
        match self {
            Self::Io(value) => value.describe(),
            Self::ChangedBytes => "the parcel changed before it was sealed".into(),
            Self::SocketTooSmall(value) => value.describe(),
        }
    }
}

impl RulesetFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Create(cause) => format!("this kernel refused the Landlock ruleset: {}", cause.describe()),
            Self::Admit(value) => value.describe(),
            Self::Open(value) => value.describe(),
            Self::Restrict(cause) => format!("cannot enter the Landlock ruleset: {}; refusing to run unconfined", cause.describe()),
            Self::MissingFd(fd) => format!("the launch promised a Landlock ruleset at fd {fd} and none arrived; refusing to run unconfined"),
            Self::MissingRoot => "the envelope has no root to admit to the Landlock ruleset; refusing to run unconfined".into(),
        }
    }
}

impl FilterFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Arch(value) => value.describe(),
            Self::Compile(value) => value.describe(),
        }
    }
}

impl WarrantFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::MissingTerminator => "the warrant does not end in NUL: it was cut short".into(),
            Self::Magic => "not a ral-warrant/5 warrant".into(),
            Self::MissingField(field) => {
                format!("the warrant ends before its {}", field.describe())
            }
            Self::Utf8(value) => value.describe(),
            Self::Confinement(spelling) => format!(
                "the warrant's confinement {spelling:?} is not unconfined, landlock@<fd>, or landlock+refer@<fd>"
            ),
            Self::Slot(value) => value.describe(),
            Self::SlotCount(value) => value.describe(),
            Self::DuplicateSlot(fd) => {
                format!("two handoff owners name fd {fd}; one would be closed twice")
            }
            Self::MissingDescriptor(value) => value.describe(),
            Self::NotReportSocket(fd) => format!("fd {fd} is not ral's report socket"),
            Self::UnknownImageKind(kind) => {
                format!("the warrant names an unknown kind of program, {kind:?}")
            }
            Self::InvalidProgram(value) => value.describe(),
            Self::UnknownBundledTool(tool) => {
                format!("the warrant names an unknown bundled tool, {tool:?}")
            }
            Self::ArgumentCountSyntax(text) => format!(
                "the warrant's argument count {text:?} is not an addressable unsigned integer"
            ),
            Self::ArgumentCount(value) => value.describe(),
            Self::Nul(field) => format!(
                "the warrant's {} holds a NUL byte, which would end it early",
                field.describe()
            ),
            Self::Noncanonical => "the warrant is not in canonical form".into(),
            Self::Size(value) => value.describe(),
            Self::Read(cause) => format!("cannot read the warrant: {}", cause.describe()),
            Self::SealProbe(cause) => {
                format!(
                    "cannot inspect the warrant's memfd seals: {}",
                    cause.describe()
                )
            }
            Self::MissingSeals => "the warrant's memfd lacks required seals; refusing it".into(),
            Self::Rewind(cause) => format!("cannot rewind the warrant: {}", cause.describe()),
            Self::NotSocket => "the warrant is not a socket; refusing it".into(),
            Self::SocketStat(cause) => format!("cannot stat the warrant: {}", cause.describe()),
            Self::StreamInstall(value) => value.describe(),
            Self::DescriptorSweep(value) => value.describe(),
        }
    }
}

impl WarrantField {
    pub fn describe(&self) -> String {
        match self {
            Self::Confinement => "confinement".to_owned(),
            Self::ImageKind => "image kind".to_owned(),
            Self::Program => "program".to_owned(),
            Self::Argv0 => "argv0".to_owned(),
            Self::ArgumentCount => "argument count".to_owned(),
            Self::Argument(value) => format!("argument {value}"),
            Self::BwrapOption(value) => format!("bwrap option {value}"),
        }
    }
}

impl HandoffField {
    pub fn describe(&self) -> String {
        match self {
            Self::Warrant => "warrant".to_owned(),
            Self::Report => "report".to_owned(),
            Self::Stream(value) => format!("{} stream", value.describe()),
            Self::Ruleset => "ruleset".to_owned(),
        }
    }
}

impl DescriptorSweepFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::CloseRange(value) => format!("{}; refusing to run", value.describe()),
            Self::SetReportCloexec(value) => format!("{}; refusing to run", value.describe()),
            Self::BufferTooLarge(value) => format!("{}; refusing to run", value.describe()),
            Self::ListDescriptors(cause) => format!(
                "proc_pidinfo(PROC_PIDLISTFDS) failed: {}; refusing to run",
                cause.describe()
            ),
            Self::PartialDescriptorRecord(value) => {
                format!("{}; refusing to run", value.describe())
            }
            Self::CloseDescriptor(value) => format!("{}; refusing to run", value.describe()),
        }
    }
}

impl SeatbeltFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::AlreadyConfined(value) => value.describe(),
            Self::Profile(value) => {
                format!("cannot build the Seatbelt profile: {}", value.describe())
            }
            Self::Nul => "the Seatbelt profile contains a NUL byte".into(),
            Self::Entry(value) => value.describe(),
        }
    }
}

impl DaclFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::NotLocal(path) => format!(
                "'{path}' is not on a local fixed volume; network shares, removable drives, and unnamed volumes cannot be granted"
            ),
            Self::NotUnicode(path) => format!(
                "Windows grants name their objects by text, but {path} is not valid Unicode; refusing an unfaithful spelling rather than approximating it"
            ),
            Self::Redirected(value) => value.describe(),
            Self::WriteDacDenied(value) => value.describe(),
            Self::Reachable(value) => value.describe(),
            Self::AccessCheck(value) => value.describe(),
            Self::Win32(value) => value.describe(),
            Self::LedgerIo(value) => format!("DACL ledger I/O failed: {}", value.describe()),
            Self::LedgerParse(value) => value.describe(),
            Self::InvalidSid(value) => value.describe(),
            Self::MutexTimeout(value) => value.describe(),
        }
    }
}

impl RedirectedTargetKind {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl AccessCheckOperation {
    pub fn describe(&self) -> String {
        match self {
            Self::InitializeResourceManager => {
                "initialize the Authz resource manager with AuthzInitializeResourceManager"
            }
            Self::OpenProcessToken => {
                "open the process token for the Authz exclusion check with OpenProcessToken"
            }
            Self::InitializeContextFromToken => {
                "initialize the Authz client context with AuthzInitializeContextFromToken"
            }
            Self::SetAppContainerInformation => {
                "set the Authz context's AppContainer information with AuthzSetAppContainerInformation"
            }
            Self::Check => "verify the sandbox exclusion with AuthzAccessCheck",
        }
        .into()
    }
}

impl DaclOperation {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl DaclWin32Cause {
    pub fn describe(&self) -> String {
        match self {
            Self::Io(value) => value.describe(),
            Self::UnexpectedWaitResult(value) => {
                format!("unexpected wait result: {value}")
            }
        }
    }
}

impl LedgerIoFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Io(value) => value.describe(),
            Self::RetryExhausted(value) => value.describe(),
        }
    }
}

impl SidFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Empty => "the DACL SID is empty".into(),
            Self::TooLong(value) => value.describe(),
            Self::Parse(value) => value.describe(),
            Self::Invalid(text) => {
                format!("the Windows SID parser returned an invalid SID for {text:?}")
            }
        }
    }
}

impl BudgetFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::CountProcesses(cause) => format!(
                "cannot count your processes: {}; refusing to start the command without a process budget",
                cause.describe()
            ),
            Self::SetBudget(value) => value.describe(),
        }
    }
}

impl BudgetSubject {
    pub fn describe(&self) -> String {
        match self {
            Self::ChildProcesses => "child process",
            Self::ChildJob => "child's Windows Job Object",
        }
        .into()
    }
}

impl BudgetCountFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Kernel(value) => value.describe(),
            Self::InvalidCeiling(value) => {
                format!("kern.maxprocperuid returned invalid process ceiling {value}")
            }
            Self::CapacityUnaddressable(value) => {
                format!("a process-list capacity of {value} cannot be addressed")
            }
            Self::BufferTooLarge(value) => format!(
                "the process-list buffer requires {value} bytes, beyond proc_listpids' buffer-size range"
            ),
        }
    }
}

impl BudgetProbeOperation {
    pub fn describe(&self) -> String {
        match self {
            Self::PerUserCeiling => "read kern.maxprocperuid",
            Self::LiveProcesses => "list the current user's processes",
        }
        .into()
    }
}

impl BudgetSetFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::CreateJob(cause) => format!(
                "cannot create the budget's Windows Job Object: {}",
                cause.describe()
            ),
            Self::SetJobLimit(cause) => format!(
                "cannot set the Windows Job Object's active-process limit: {}",
                cause.describe()
            ),
            Self::SetProcessLimit(cause) => {
                format!("cannot set the child's process limit: {}", cause.describe())
            }
        }
    }
}

impl JailFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Sequence(value) => value.describe(),
            Self::NoParent(path) => {
                format!("'{path}' has no parent, so it is not inside a cgroup2 mount")
            }
            Self::Delegate(value) => value.describe(),
            Self::Create(value) => value.describe(),
            Self::Limit(value) => value.describe(),
            Self::Membership(value) => value.describe(),
        }
    }
}

impl SequenceFailure {
    pub fn describe(&self) -> String {
        self.describe_file("the sequence file")
    }

    fn describe_file(&self, file: &str) -> String {
        match self {
            Self::CreateParent(cause) => format!(
                "cannot create the parent directory for {file}: {}",
                cause.describe()
            ),
            Self::Open(cause) => format!("cannot open {file}: {}", cause.describe()),
            Self::Lock(cause) => format!("cannot lock {file}: {}", cause.describe()),
            Self::Read(cause) => format!("cannot read {file}: {}", cause.describe()),
            Self::Write(cause) => format!("cannot write {file}: {}", cause.describe()),
            Self::Malformed(bytes) => {
                format!("{file} holds {bytes:?}, not a count of jailed commands")
            }
            Self::Exhausted => "every UID reserved for jailed commands has been issued".into(),
        }
    }
}

impl CgroupLimitFile {
    pub fn describe(&self) -> String {
        match self {
            Self::MemoryMax => "memory.max",
            Self::PidsMax => "pids.max",
            Self::CpuMax => "cpu.max",
        }
        .into()
    }
}

impl MembershipStep {
    pub fn describe(&self) -> String {
        match self {
            Self::OpenProcs => "open cgroup.procs",
            Self::WritePid => "write the child PID to cgroup.procs",
            Self::UnshareNetwork => "create an isolated network namespace",
            Self::LoopbackSocket => "open the loopback configuration socket",
            Self::EnableLoopback => "enable loopback",
            Self::DropGroups => "drop supplementary groups",
            Self::SetGid => "set the jail GID",
            Self::SetUid => "set the jail UID",
            Self::NoNewPrivileges => "enable no_new_privs",
            Self::MarkDescriptors => "mark inherited descriptors close-on-exec",
            Self::InstallFilter => "install the seccomp filter",
        }
        .into()
    }
}

impl LauncherFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Exec(value) => value.describe(),
            Self::DiedBeforeReport(value) => value.describe(),
            Self::ReportRead(cause) => format!(
                "cannot read the trampoline's launch report: {}",
                cause.describe()
            ),
            Self::ReportUnknownTag(tag) => {
                format!("the trampoline's launch report contains unknown tag {tag}")
            }
            Self::ReportEndedBeforeStarted => {
                "the trampoline reported the program's end before reporting its start".into()
            }
            Self::ReportDecode(cause) => format!(
                "cannot decode the trampoline's launch refusal: {}",
                cause.describe()
            ),
            Self::ReportEncodingTooLarge(value) => value.describe(),
            Self::PayloadGroup(value) => value.describe(),
            Self::ChildPipe(value) => value.describe(),
            Self::ChildFork(value) => value.describe(),
            Self::ChildSetup(value) => value.describe(),
            Self::ChildDiedBeforeSetup(image) => format!(
                "the child for {} died without reporting completion of its setup",
                image.describe()
            ),
            Self::SpawnBeforeSetup(value) => value.describe(),
            Self::NonNativeExecError(value) => value.describe(),
            Self::AnchorReadiness(value) => value.describe(),
        }
    }
}

impl LauncherRole {
    pub fn describe(&self) -> String {
        match self {
            Self::Bwrap => "bwrap",
            Self::RalTrampoline => "ral's sandbox trampoline",
        }
        .into()
    }
}

impl LaunchImageSubject {
    pub fn describe(&self) -> String {
        match self {
            Self::File(path) => format!("program '{path}'"),
            Self::Bundled(name) => format!("bundled tool '{name}'"),
        }
    }
}

impl PayloadGroupFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Read(cause) => {
                format!("cannot read bwrap's --info-fd record: {}", cause.describe())
            }
            Self::Json(cause) => format!("malformed bwrap --info-fd record: {}", cause.describe()),
            Self::InvalidPid(pid) => {
                format!("bwrap's --info-fd record names invalid child-pid {pid}")
            }
        }
    }
}

impl AnchorReadinessFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::Read(cause) => format!(
                "cannot read the pipeline anchor's readiness report: {}",
                cause.describe()
            ),
            Self::UnexpectedByte(byte) => {
                format!("the pipeline anchor reported readiness byte {byte}, expected 0")
            }
        }
    }
}

impl DetachedPidFailure {
    pub fn describe(&self) -> String {
        format!(
            "program {:?} started, but its survivor PID is unavailable: {}",
            self.program,
            self.cause.describe()
        )
    }
}

impl DetachedPidCause {
    pub fn describe(&self) -> String {
        match self {
            Self::ReaperSilent => {
                "the reaper never reported the intermediate process's exit".into()
            }
            Self::Reap(cause) => {
                format!("cannot reap the intermediate process: {}", cause.describe())
            }
            Self::IntermediateEnded(status) => {
                let status = match status {
                    NativeStatus::Exited(code) => format!("exited with status {code}"),
                    NativeStatus::Signaled(signal) => format!("was terminated by signal {signal}"),
                };
                format!("the intermediate process {status}, instead of exiting with status 0")
            }
            Self::ReceiptRead(cause) => format!(
                "cannot read the survivor's PID receipt: {}",
                cause.describe()
            ),
            Self::InvalidPid(pid) => {
                format!("the receipt contained {pid}, which is not a process ID")
            }
        }
    }
}

impl UnavailableFailureBwrapProbe {
    pub fn describe(&self) -> String {
        format!(
            "bwrap at '{}' cannot build an envelope on this host: {}",
            self.binary,
            self.cause.describe()
        )
    }
}

impl UnsupportedConfinementWindowsEveryPath {
    pub fn describe(&self) -> String {
        format!(
            "{}, which Windows cannot enforce: a sandboxed program reaches only folders stamped for it, and ral will not stamp every drive",
            if self.excluding {
                "every path except some exclusions"
            } else {
                "every path"
            }
        )
    }
}

impl UnsupportedConfinementWindowsAllowanceExclusion {
    pub fn describe(&self) -> String {
        format!(
            "this command is allowed '{}' and '{}' is excluded, which Windows cannot enforce together: a sandbox can be kept out of what lies beneath an allowance, but not out of the allowance itself or what holds it",
            self.granted, self.denied
        )
    }
}

impl BwrapProbeFailureExited {
    pub fn describe(&self) -> String {
        let status = match self.status {
            NativeStatus::Exited(code) => format!("it exited with status {code}"),
            NativeStatus::Signaled(signal) => format!("it was terminated by signal {signal}"),
        };
        match &self.stderr {
            Some(stderr) => format!("{status}: {stderr}"),
            None => status,
        }
    }
}

impl ProjectionFailureChangedSymlink {
    pub fn describe(&self) -> String {
        format!(
            "'{}' was granted by its real path, but it now goes through a symbolic link",
            self.granted
        )
    }
}

impl ProjectionFailurePinnedDirectoryRedirected {
    pub fn describe(&self) -> String {
        format!(
            "the directory '{}' is pinned for this launch, but now reaches '{}' through a symbolic link, which would be shown at its name",
            self.granted, self.reached
        )
    }
}

impl ProjectionFailureResolve {
    pub fn describe(&self) -> String {
        format!("cannot resolve '{}': {}", self.path, self.cause.describe())
    }
}

impl ProjectionFailureList {
    pub fn describe(&self) -> String {
        format!("cannot list '{}': {}", self.path, self.cause.describe())
    }
}

impl ProjectionFailureCarve {
    pub fn describe(&self) -> String {
        format!(
            "cannot {} '{}' to grant around an exclusion: {}",
            self.step.describe(),
            self.path,
            self.cause.describe()
        )
    }
}

impl ProjectionFailureCopyHandle {
    pub fn describe(&self) -> String {
        format!(
            "cannot copy the handle on '{}': {}",
            self.path,
            self.cause.describe()
        )
    }
}

impl ProjectionFailureHiddenWorkingDirectory {
    pub fn describe(&self) -> String {
        format!(
            "the working directory '{}' is excluded by '{}', and no confined command can start inside an exclusion",
            self.cwd, self.exclusion
        )
    }
}

impl ProjectionFailureProcExcluded {
    pub fn describe(&self) -> String {
        format!(
            "the grant denies '{}', which hides /proc; the envelope needs /proc to start its program",
            self.exclusion
        )
    }
}

impl ProjectionOpenFailureFailed {
    pub fn describe(&self) -> String {
        format!("cannot open '{}': {}", self.path, self.cause.describe())
    }
}

impl ParcelCauseSocketTooSmall {
    pub fn describe(&self) -> String {
        format!(
            "a {}-byte parcel does not fit a socket: {}",
            self.bytes,
            self.cause.describe()
        )
    }
}

impl RulesetFailureAdmit {
    pub fn describe(&self) -> String {
        format!(
            "cannot admit '{}' to the Landlock ruleset: {}",
            self.name,
            self.cause.describe()
        )
    }
}

impl FilterFailureArch {
    pub fn describe(&self) -> String {
        format!(
            "seccomp does not support architecture '{}': {}",
            self.architecture, self.detail
        )
    }
}

impl FilterFailureCompile {
    pub fn describe(&self) -> String {
        format!("the seccomp deny-set failed to compile: {}", self.detail)
    }
}

impl WarrantFailureUtf8 {
    pub fn describe(&self) -> String {
        let sequence = match self.error_len {
            Some(length) => format!("invalid {length}-byte sequence"),
            None => "the sequence is incomplete".into(),
        };
        format!(
            "the warrant's {} is not UTF-8 at byte {}: {sequence}",
            self.field.describe(),
            self.valid_up_to
        )
    }
}

impl WarrantFailureSlot {
    pub fn describe(&self) -> String {
        format!(
            "{:?} names no {} handoff descriptor; expected plain digits naming fd 98 or above",
            self.spelling,
            self.field.describe()
        )
    }
}

impl WarrantFailureSlotCount {
    pub fn describe(&self) -> String {
        format!(
            "--warrant descriptor-argument count is {}; expected 2 (warrant and report), or 5 under a Linux envelope",
            self.actual
        )
    }
}

impl WarrantFailureMissingDescriptor {
    pub fn describe(&self) -> String {
        format!("no {} descriptor at fd {}", self.field.describe(), self.fd)
    }
}

impl WarrantFailureInvalidProgram {
    pub fn describe(&self) -> String {
        format!(
            "the warrant's file program '{}' is not a canonical absolute real path: {}; refusing PATH lookup",
            self.shown,
            self.cause.describe()
        )
    }
}

impl WarrantFailureArgumentCount {
    pub fn describe(&self) -> String {
        format!(
            "the warrant declares {} but carries {}",
            crate::text::plural(self.declared.to_u64(), "argument"),
            self.actual
        )
    }
}

impl WarrantFailureSize {
    pub fn describe(&self) -> String {
        format!(
            "the warrant is {} bytes, over its {}-byte cap",
            self.actual, self.limit
        )
    }
}

impl WarrantFailureStreamInstall {
    pub fn describe(&self) -> String {
        format!(
            "cannot install payload {} from fd {}: {}",
            self.stream.describe(),
            self.source,
            self.cause.describe()
        )
    }
}

impl DescriptorSweepFailureCloseRange {
    pub fn describe(&self) -> String {
        format!(
            "cannot close inherited descriptors {} through {} with close_range(2): {}",
            self.first,
            self.last,
            self.cause.describe()
        )
    }
}

impl DescriptorSweepFailureSetReportCloexec {
    pub fn describe(&self) -> String {
        format!(
            "cannot set the report socket at fd {} close-on-exec: {}",
            self.fd,
            self.cause.describe()
        )
    }
}

impl DescriptorSweepFailureBufferTooLarge {
    pub fn describe(&self) -> String {
        let bytes = if self.bytes.to_u64() == 1 {
            "byte is"
        } else {
            "bytes are"
        };
        format!(
            "cannot size the proc_pidinfo(PROC_PIDLISTFDS) buffer: {} {bytes} not addressable by that API",
            self.bytes
        )
    }
}

impl DescriptorSweepFailurePartialDescriptorRecord {
    pub fn describe(&self) -> String {
        format!(
            "proc_pidinfo(PROC_PIDLISTFDS) returned {}, not a whole number of {}-byte descriptor records",
            crate::text::plural(self.bytes.to_u64(), "byte"),
            self.width
        )
    }
}

impl DescriptorSweepFailureCloseDescriptor {
    pub fn describe(&self) -> String {
        format!(
            "cannot close inherited fd {}: {}",
            self.fd,
            self.cause.describe()
        )
    }
}

impl SeatbeltFailureAlreadyConfined {
    pub fn describe(&self) -> String {
        let explanation = "this process is already inside a macOS sandbox profile and macOS will not let it enter a second, so the launch is refused rather than run under that wider profile";
        match &self.detail {
            Some(detail) => format!("{explanation}: {detail}"),
            None => explanation.into(),
        }
    }
}

impl SeatbeltFailureEntry {
    pub fn describe(&self) -> String {
        let explanation = format!(
            "cannot enter the Seatbelt sandbox: {}",
            self.cause.describe()
        );
        match &self.detail {
            Some(detail) => format!("{explanation}: {detail}"),
            None => explanation,
        }
    }
}

impl DaclFailureRedirected {
    pub fn describe(&self) -> String {
        let opening = format!(
            "'{}' was granted by its real path, but '{}'",
            self.granted, self.at
        );
        match self.target_kind {
            RedirectedTargetKind::MountedVolume => format!(
                "{opening} is where a volume is mounted; a volume mounted in a folder cannot be granted on Windows because it cannot be told from a junction"
            ),
            RedirectedTargetKind::JunctionOrLink => {
                format!("{opening} is now a junction or symbolic link")
            }
            RedirectedTargetKind::ReparsePoint => format!("{opening} is now a reparse point"),
        }
    }
}

impl DaclFailureWriteDacDenied {
    pub fn describe(&self) -> String {
        format!(
            "WRITE_DAC denied on '{}' during {}: {}",
            self.path,
            self.operation.describe(),
            self.cause.describe()
        )
    }
}

impl DaclFailureReachable {
    pub fn describe(&self) -> String {
        let inside = if self.at == self.denied {
            String::new()
        } else {
            format!(" (it lies inside the excluded '{}')", self.denied)
        };
        format!(
            "sandbox confinement unavailable: '{}' is excluded{inside}, but a sandboxed program could still open it: its permissions may admit every Windows sandbox (as ALL APPLICATION PACKAGES does), or it may retain access inherited from an allowed folder (for example, through a hard-linked file), and Windows cannot take such access away",
            self.at
        )
    }
}

impl DaclFailureAccessCheck {
    pub fn describe(&self) -> String {
        format!(
            "cannot {}: {}",
            self.operation.describe(),
            self.cause.describe()
        )
    }
}

impl DaclFailureWin32 {
    pub fn describe(&self) -> String {
        let path = self
            .path
            .as_ref()
            .map_or_else(String::new, |path| format!(" on '{path}'"));
        format!(
            "{} failed{path}: {}",
            self.operation.describe(),
            self.cause.describe()
        )
    }
}

impl DaclFailureLedgerParse {
    pub fn describe(&self) -> String {
        format!(
            "cannot decode DACL ledger '{}': {}",
            self.path,
            self.cause.describe()
        )
    }
}

impl DaclFailureMutexTimeout {
    pub fn describe(&self) -> String {
        format!(
            "timed out acquiring DACL mutex on '{}' after {} ms",
            self.path, self.timeout_ms
        )
    }
}

impl LedgerIoFailureRetryExhausted {
    pub fn describe(&self) -> String {
        format!("retry exhausted after {} attempts", self.attempts)
    }
}

impl SidFailureTooLong {
    pub fn describe(&self) -> String {
        format!(
            "the DACL SID is {} bytes, over its {}-byte limit",
            self.bytes, self.limit
        )
    }
}

impl SidFailureParse {
    pub fn describe(&self) -> String {
        format!(
            "cannot parse Windows SID {:?}: {}",
            self.text,
            self.cause.describe()
        )
    }
}

impl BudgetFailureSetBudget {
    pub fn describe(&self) -> String {
        format!(
            "cannot set the {} budget to {}: {}; refusing to start the command without that budget",
            self.subject.describe(),
            self.limit,
            self.cause.describe()
        )
    }
}

impl BudgetCountFailureKernel {
    pub fn describe(&self) -> String {
        format!(
            "cannot {}: {}",
            self.operation.describe(),
            self.cause.describe()
        )
    }
}

impl JailFailureSequence {
    pub fn describe(&self) -> String {
        self.cause
            .describe_file(&format!("sequence file '{}'", self.path))
    }
}

impl JailFailureDelegate {
    pub fn describe(&self) -> String {
        format!(
            "cannot delegate +memory +pids +cpu from '{}': {}; without these controllers the child cgroup cannot enforce its limits, so the command is refused rather than run uncapped",
            self.parent,
            self.cause.describe()
        )
    }
}

impl JailFailureCreate {
    pub fn describe(&self) -> String {
        format!(
            "cannot create cgroup '{}': {}",
            self.cgroup,
            self.cause.describe()
        )
    }
}

impl JailFailureLimit {
    pub fn describe(&self) -> String {
        format!(
            "cannot write {} in cgroup '{}': {}; refusing to run the command without this limit",
            self.file.describe(),
            self.cgroup,
            self.cause.describe()
        )
    }
}

impl JailFailureMembership {
    pub fn describe(&self) -> String {
        format!(
            "cannot {} for guest cgroup '{}': {}",
            self.step.describe(),
            self.cgroup,
            self.cause.describe()
        )
    }
}

impl LauncherFailureExec {
    pub fn describe(&self) -> String {
        format!(
            "cannot start {}: {}; the command never ran",
            self.launcher.describe(),
            self.cause.describe()
        )
    }
}

impl LauncherFailureDiedBeforeReport {
    pub fn describe(&self) -> String {
        let message = format!(
            "{} ended before reporting whether the program started",
            self.launcher.describe()
        );
        match &self.stderr {
            Some(stderr) => format!("{message}: {stderr}"),
            None => format!("{message}; no startup diagnostic was captured"),
        }
    }
}

impl LauncherFailureReportEncodingTooLarge {
    pub fn describe(&self) -> String {
        format!(
            "the encoded launch refusal is {} bytes, exceeding the report's {}-byte limit; the original refusal could not be transmitted",
            self.actual, self.limit
        )
    }
}

impl LauncherFailureChildPipe {
    pub fn describe(&self) -> String {
        format!(
            "cannot create the child-setup channel for {}: {}",
            self.subject.describe(),
            self.cause.describe()
        )
    }
}

impl LauncherFailureChildFork {
    pub fn describe(&self) -> String {
        format!(
            "cannot fork {}: {}",
            self.subject.describe(),
            self.cause.describe()
        )
    }
}

impl LauncherFailureChildSetup {
    pub fn describe(&self) -> String {
        format!(
            "ral's child setup failed before starting {}: {}",
            self.subject.describe(),
            self.cause.describe()
        )
    }
}

impl LauncherFailureSpawnBeforeSetup {
    pub fn describe(&self) -> String {
        format!(
            "ral could not spawn {} before setup completion was established: {}",
            self.subject.describe(),
            self.cause.describe()
        )
    }
}

impl LauncherFailureNonNativeExecError {
    pub fn describe(&self) -> String {
        format!(
            "ral could not construct the execution request for {}: {}",
            self.subject.describe(),
            self.cause.describe()
        )
    }
}

const FROZEN_ALLOWANCE_ADVICE: &str = "an allowance names what it named when granted. To allow the new target, grant it where the ceiling is set (in exarch, restart with a grant that names it)";

impl LaunchFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Unavailable(cause) => cause.advice(),
            Self::Prepare(cause) => cause.advice(),
            Self::Jail(cause) => cause.advice(),
            Self::Pin(cause) => cause.advice(),
            Self::Projection(cause) => cause.advice(),
            Self::Parcel(cause) => cause.advice(),
            Self::Ruleset(cause) => cause.advice(),
            Self::Warrant(cause) => cause.advice(),
            Self::Seatbelt(cause) => cause.advice(),
            Self::Dacl(cause) => cause.advice(),
            Self::BatchImage(_) => vec!["ral will not synthesize a `cmd /c` wrapper: batch-file argument quoting has no safe general encoding (CVE-2024-24576). Invoke cmd.exe explicitly only if you accept that risk, or run the underlying program directly".into()],
            _ => Vec::new(),
        }
    }
}

impl PrepareFailure {
    pub fn advice(&self) -> Vec<String> {
        match self.step {
            PrepareStep::AssignJob => {
                vec!["Is ral already running in a restrictive Windows Job Object?".into()]
            }
            _ => Vec::new(),
        }
    }
}

impl JailFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Sequence(JailFailureSequence {
                cause: SequenceFailure::Malformed(_) | SequenceFailure::Exhausted,
                ..
            }) => vec!["Restarting the guest resets its sequence in /run.".into()],
            Self::NoParent(_) => {
                vec!["Is the jail root the path where this guest mounts cgroup2?".into()]
            }
            _ => Vec::new(),
        }
    }
}

impl UnavailableFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::MissingBootPin(BinaryRole::Bwrap) => vec![
                "Is bubblewrap installed on this host and on the PATH ral was started with?".into(),
            ],
            Self::MissingBootPin(BinaryRole::Ral) => PinCause::Unavailable.advice(),
            Self::Unsupported(cause) => cause.advice(),
            Self::BwrapProbe(probe) => probe.cause.advice(),
            Self::Kernel(cause) => cause.advice(),
        }
    }
}

impl UnsupportedConfinement {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::WindowsEveryPath(_) => {
                vec!["Grant the folders the program needs instead.".into()]
            }
            Self::WindowsAllowanceExclusion(value) => vec![format!(
                "Allow paths outside '{}', run a program outside it, or drop the exclusion.",
                value.denied
            )],
        }
    }
}

impl BwrapProbeFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Exited(exit)
                if exit.stderr.as_ref().is_some_and(|stderr| {
                    stderr.contains("--ro-bind-fd")
                        && ["Unknown option", "unknown option", "unrecognized option"]
                            .iter()
                            .any(|phrase| stderr.contains(phrase))
                }) =>
            {
                vec!["Is bubblewrap older than 0.8.0?".into()]
            }
            Self::RootOpen(cause) => cause.advice(),
            Self::Lend(cause) => cause.advice(),
            Self::RedirectedDestinationAccepted => vec!["Is bubblewrap older than 0.10.0?".into()],
            _ => Vec::new(),
        }
    }
}

impl KernelFeatureFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::LandlockAbsent => {
                vec!["Is the kernel older than 5.13, or does it have Landlock disabled?".into()]
            }
            Self::LandlockProbe(_) | Self::CloseRangeAbsent => Vec::new(),
        }
    }
}

impl PinFailure {
    pub fn advice(&self) -> Vec<String> {
        self.cause.advice()
    }
}

impl PinCause {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Unavailable => vec!["Is its executable missing or unreadable?".into()],
            Self::ChangedIdentity => vec!["Restart to pick up the new build.".into()],
            _ => Vec::new(),
        }
    }
}

impl ProjectionFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::ChangedSymlink(_) => vec![FROZEN_ALLOWANCE_ADVICE.into()],
            Self::PinnedDirectoryRedirected(_) => {
                vec!["Make it a directory again, or retry once nothing is relinking it.".into()]
            }
            Self::Open(cause) => cause.advice(),
            Self::HiddenWorkingDirectory(_) => {
                vec!["Run it from another directory (`cd` or `within [dir: …]`).".into()]
            }
            Self::ProcExcluded(_) => vec!["Did you mean a path inside /proc?".into()],
            _ => Vec::new(),
        }
    }
}

impl ProjectionOpenFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Linked(_) => vec![FROZEN_ALLOWANCE_ADVICE.into()],
            Self::Failed(_) => Vec::new(),
        }
    }
}

impl ParcelFailure {
    pub fn advice(&self) -> Vec<String> {
        match (&self.step, &self.cause) {
            (ParcelStep::Lend | ParcelStep::Keep, ParcelCause::Io(_)) => {
                vec!["If descriptor allocation failed, check the open-file limit with `sh -c 'ulimit -n'`, and raise it for the session with the rc's `limits: [open-files: N]`".into()]
            }
            (ParcelStep::Lift, ParcelCause::Io(_)) => {
                vec!["is the open-file limit too low to lend descriptors to the confined child? Check it with `sh -c 'ulimit -n'`, and raise it for the session with the rc's `limits: [open-files: N]`".into()]
            }
            (_, ParcelCause::ChangedBytes) => {
                vec!["Did another process open ral's descriptors through /proc?".into()]
            }
            _ => Vec::new(),
        }
    }
}

impl RulesetFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Open(cause) => cause.advice(),
            _ => Vec::new(),
        }
    }
}

impl WarrantFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::Size(_) => vec!["Is the command line that long?".into()],
            Self::Slot(_) | Self::SlotCount(_) | Self::DuplicateSlot(_) | Self::MissingDescriptor(_) | Self::NotReportSocket(_) => vec!["--warrant is how ral starts its own confined children, never a command to run by hand.".into()],
            _ => Vec::new(),
        }
    }
}

impl SeatbeltFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::AlreadyConfined(_) => vec!["Is ral itself running confined?".into()],
            Self::Profile(cause) => cause.advice(),
            _ => Vec::new(),
        }
    }
}

impl DaclFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::NotLocal(_) => vec!["Grant a folder on a local disk.".into()],
            Self::Redirected(value) => match value.target_kind {
                RedirectedTargetKind::MountedVolume => {
                    vec!["Would giving the volume a drive letter and granting that do?".into()]
                }
                RedirectedTargetKind::JunctionOrLink | RedirectedTargetKind::ReparsePoint => {
                    vec![FROZEN_ALLOWANCE_ADVICE.into()]
                }
            },
            Self::Reachable(_) => vec!["Remove that access from it, or drop the exclusion.".into()],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::{Datum, tag};

    #[test]
    fn prepare_app_container_profile_steps_have_distinct_portable_labels_and_hresult_bits() {
        for (step, label) in [
            (
                PrepareStep::AppContainerProfileCreate,
                "app-container-profile-create",
            ),
            (
                PrepareStep::AppContainerProfileDeriveSid,
                "app-container-profile-derive-sid",
            ),
        ] {
            assert_eq!(step.encode(), tag(label, None));
            assert_eq!(PrepareStep::decode(&tag(label, None)).unwrap(), step);
            for hresult in [i32::MIN, -2_147_024_891, -1, 0, i32::MAX] {
                let failure = PrepareFailure {
                    step,
                    subject: PrepareSubject::Capability("ral-profile".into()),
                    cause: PrepareCause::Hresult(hresult),
                };
                let decoded = PrepareFailure::decode(&failure.clone().encode()).unwrap();
                assert_eq!(decoded, failure);
                assert_eq!(decoded.describe(), failure.describe());
            }
        }
        assert!(PrepareStep::decode(&tag("app-container-profile", None)).is_err());
    }

    fn os() -> OsFailure {
        OsFailure {
            kind: super::super::io::IoKind::PermissionDenied,
            raw: Some(13),
            detail: None,
        }
    }

    #[expect(
        clippy::unnecessary_box_returns,
        reason = "Failure fixtures require boxed I/O causes"
    )]
    fn io() -> Box<IoCause> {
        Box::new(IoCause {
            context: Vec::new(),
            leaf: Box::new(super::super::io::IoLeaf::Os(os())),
        })
    }

    fn observers(failure: &LaunchFailure, description: &str, advice: &[&str]) {
        assert_eq!(failure.describe(), description);
        assert_eq!(failure.advice(), advice);
        let decoded = LaunchFailure::decode(&failure.clone().encode()).unwrap();
        assert_eq!(&decoded, failure);
        assert_eq!(decoded.describe(), description);
        assert_eq!(decoded.advice(), advice);
        let error = crate::error::Error::new(super::super::Reason::Launch(Box::new(decoded)));
        assert_eq!(error.to_string(), description);
        assert!(
            error.hints().is_empty(),
            "intrinsic advice must not be stored"
        );
        let report = error.plain();
        assert!(report.contains(description), "{report}");
        let mut rest = report.as_str();
        for entry in advice {
            let (_, remaining) = rest
                .split_once(&format!("hint: {entry}"))
                .expect("ordered advice");
            assert!(!remaining.contains(entry), "advice is printed once");
            rest = remaining;
        }
    }

    #[test]
    fn activated_prepare_steps_describe_the_known_responsibility_after_transport() {
        for (step, subject, operation) in [
            (
                PrepareStep::Stdio,
                PrepareSubject::Program("tool".into()),
                "connect standard streams for program 'tool'",
            ),
            (
                PrepareStep::Stdio,
                PrepareSubject::Stream(StandardStream::Stdin),
                "prepare the child's stdin",
            ),
            (
                PrepareStep::Stdio,
                PrepareSubject::Stream(StandardStream::Stdout),
                "prepare the child's stdout",
            ),
            (
                PrepareStep::Stdio,
                PrepareSubject::Stream(StandardStream::Stderr),
                "prepare the child's stderr",
            ),
            (
                PrepareStep::Stdio,
                PrepareSubject::Child,
                "prepare the child's standard streams",
            ),
            (
                PrepareStep::MergeStderr,
                PrepareSubject::Stream(StandardStream::Stderr),
                "connect the child's stderr to stdout",
            ),
            (
                PrepareStep::CapturePayloadStreams,
                PrepareSubject::Child,
                "preserve the payload's standard streams before starting its envelope",
            ),
            (
                PrepareStep::Argv,
                PrepareSubject::Argument(1usize.into()),
                "prepare argv[1]",
            ),
            (
                PrepareStep::Application,
                PrepareSubject::Program("tool".into()),
                "prepare program 'tool' for launch",
            ),
            (
                PrepareStep::Environment,
                PrepareSubject::EnvironmentKey("\"TOKEN\"".into()),
                "prepare environment key \"TOKEN\"",
            ),
            (
                PrepareStep::Environment,
                PrepareSubject::EnvironmentValue("\"TOKEN\"".into()),
                "prepare the value of environment key \"TOKEN\"",
            ),
            (
                PrepareStep::WorkingDirectory,
                PrepareSubject::Path("/work".into()),
                "prepare working directory '/work'",
            ),
            (
                PrepareStep::AttributeList,
                PrepareSubject::Child,
                "initialize the Windows process attribute list",
            ),
            (
                PrepareStep::HandleList,
                PrepareSubject::Child,
                "install the child's inherited-handle list",
            ),
            (
                PrepareStep::SecurityCapabilities,
                PrepareSubject::Child,
                "install the child's AppContainer security capabilities",
            ),
            (
                PrepareStep::HandleInheritance,
                PrepareSubject::Child,
                "make the admitted handles inheritable by the child",
            ),
            (
                PrepareStep::SessionLimit,
                PrepareSubject::SessionLimit(SessionLimit::OpenFiles),
                "apply the session's open-file limit",
            ),
            (
                PrepareStep::SessionLimit,
                PrepareSubject::SessionLimit(SessionLimit::CoreBytes),
                "apply the session's core-file byte limit",
            ),
            (
                PrepareStep::SessionLimit,
                PrepareSubject::SessionLimit(SessionLimit::StackBytes),
                "apply the session's stack-byte limit",
            ),
            (
                PrepareStep::GrantLimit,
                PrepareSubject::GrantLimit(GrantLimit::Processes),
                "apply the grant's process limit",
            ),
            (
                PrepareStep::GrantLimit,
                PrepareSubject::GrantLimit(GrantLimit::CoreBytes),
                "apply the grant's core-file byte limit",
            ),
            (
                PrepareStep::Group,
                PrepareSubject::Group(42usize.into()),
                "place the child in process group 42",
            ),
            (
                PrepareStep::Group,
                PrepareSubject::Child,
                "establish the child's process-group placement",
            ),
            (
                PrepareStep::SignalReset,
                PrepareSubject::Child,
                "reset the child's signals",
            ),
            (
                PrepareStep::ReadinessMark,
                PrepareSubject::Child,
                "establish the child's exec-readiness evidence",
            ),
            (
                PrepareStep::DetachedFork,
                PrepareSubject::Child,
                "fork the detached survivor",
            ),
            (
                PrepareStep::DetachedSession,
                PrepareSubject::Child,
                "create the detached survivor's session",
            ),
            (
                PrepareStep::DetachedHandshake,
                PrepareSubject::Child,
                "establish the detached launch's PID handshake",
            ),
            (
                PrepareStep::Job,
                PrepareSubject::Child,
                "prepare the child's Windows Job Object",
            ),
            (
                PrepareStep::AssignJob,
                PrepareSubject::Child,
                "place the suspended child in its Windows Job Object",
            ),
            (
                PrepareStep::ResumeThread,
                PrepareSubject::Child,
                "resume the child after assigning its Windows Job Object",
            ),
            (
                PrepareStep::TerminalHandover,
                PrepareSubject::Group(42usize.into()),
                "hand the terminal to process group 42",
            ),
            (
                PrepareStep::AppContainerProfileCreate,
                PrepareSubject::Capability("profile".into()),
                "create AppContainer profile 'profile'",
            ),
            (
                PrepareStep::AppContainerProfileDeriveSid,
                PrepareSubject::Capability("profile".into()),
                "derive the SID of existing AppContainer profile 'profile'",
            ),
            (
                PrepareStep::NetworkCapabilitySids,
                PrepareSubject::Child,
                "derive the network capability SIDs",
            ),
            (
                PrepareStep::FilesystemCapabilitySid,
                PrepareSubject::Capability("files".into()),
                "derive filesystem capability SID 'files'",
            ),
            (
                PrepareStep::SidString,
                PrepareSubject::Capability("files".into()),
                "render the SID for capability 'files'",
            ),
        ] {
            let advice = if step == PrepareStep::AssignJob {
                vec!["Is ral already running in a restrictive Windows Job Object?"]
            } else {
                vec![]
            };
            observers(
                &LaunchFailure::Prepare(PrepareFailure {
                    step,
                    subject,
                    cause: PrepareCause::Io(io()),
                }),
                &format!("ral could not {operation}: permission denied"),
                &advice,
            );
        }
    }

    #[test]
    fn preparation_causes_explain_invalid_inputs_without_schema_labels_or_secret_values() {
        for (cause, description) in [
            (
                PrepareCause::Invalid(PrepareInvalid::Nul),
                "it contains a NUL byte, which cannot cross the process-launch boundary",
            ),
            (
                PrepareCause::Invalid(PrepareInvalid::GroupNoLongerLive(42usize.into())),
                "pipeline group 42 is no longer live",
            ),
            (
                PrepareCause::Invalid(PrepareInvalid::GroupHasNoJob(42usize.into())),
                "pipeline group 42 has no Windows Job Object",
            ),
            (
                PrepareCause::Invalid(PrepareInvalid::NoCapabilitySid("files".into())),
                "Windows returned no capability SID for 'files'",
            ),
            (
                PrepareCause::Hresult(-2_147_024_891),
                "Windows returned HRESULT 0x80070005",
            ),
            (
                PrepareCause::Hresult(i32::MIN),
                "Windows returned HRESULT 0x80000000",
            ),
            (
                PrepareCause::Hresult(-1),
                "Windows returned HRESULT 0xffffffff",
            ),
        ] {
            observers(
                &LaunchFailure::Prepare(PrepareFailure {
                    step: PrepareStep::Environment,
                    subject: PrepareSubject::EnvironmentValue("\"TOKEN\"".into()),
                    cause,
                }),
                &format!(
                    "ral could not prepare the value of environment key \"TOKEN\": {description}"
                ),
                &[],
            );
        }
    }

    #[test]
    fn probe_preparation_has_its_own_tag_and_never_suggests_a_bwrap_version() {
        let mut cause = os();
        cause.detail = Some("Unknown option --ro-bind-fd".into());
        let failure = PrepareFailure {
            step: PrepareStep::ReadinessMark,
            subject: PrepareSubject::Child,
            cause: PrepareCause::Io(Box::new(IoCause {
                context: Vec::new(),
                leaf: Box::new(super::super::io::IoLeaf::Os(cause)),
            })),
        };
        let probe = BwrapProbeFailure::Prepare(failure.clone());
        assert_eq!(
            probe.clone().encode(),
            tag("prepare", Some(failure.encode()))
        );
        assert_eq!(
            BwrapProbeFailure::decode(&probe.clone().encode()).unwrap(),
            probe
        );
        observers(
            &LaunchFailure::Unavailable(UnavailableFailure::BwrapProbe(
                UnavailableFailureBwrapProbe {
                    binary: "/bwrap".into(),
                    cause: probe,
                },
            )),
            "sandbox confinement unavailable: bwrap at '/bwrap' cannot build an envelope on this host: ral could not prepare the bwrap probe: ral could not establish the child's exec-readiness evidence: permission denied",
            &[],
        );
    }

    #[test]
    fn launcher_descriptions_preserve_what_is_and_is_not_known_to_have_started() {
        let decode = JsonInputFailure::Decode(super::super::decode::DecodeFailure::new(
            super::super::decode::DecodeCause::MissingField("child-pid".into()),
        ));
        let decode_text = decode.describe();
        for (cause, description) in [
            (LauncherFailure::Exec(LauncherFailureExec { launcher: LauncherRole::Bwrap, cause: os() }), "cannot start bwrap: permission denied; the command never ran".into()),
            (LauncherFailure::Exec(LauncherFailureExec { launcher: LauncherRole::RalTrampoline, cause: os() }), "cannot start ral's sandbox trampoline: permission denied; the command never ran".into()),
            (LauncherFailure::DiedBeforeReport(LauncherFailureDiedBeforeReport { launcher: LauncherRole::Bwrap, stderr: None }), "bwrap ended before reporting whether the program started; no startup diagnostic was captured".into()),
            (LauncherFailure::DiedBeforeReport(LauncherFailureDiedBeforeReport { launcher: LauncherRole::RalTrampoline, stderr: Some("actual stderr".into()) }), "ral's sandbox trampoline ended before reporting whether the program started: actual stderr".into()),
            (LauncherFailure::ReportRead(io()), "cannot read the trampoline's launch report: permission denied".into()),
            (LauncherFailure::ReportUnknownTag(19), "the trampoline's launch report contains unknown tag 19".into()),
            (LauncherFailure::ReportEndedBeforeStarted, "the trampoline reported the program's end before reporting its start".into()),
            (LauncherFailure::ReportDecode(decode.clone()), format!("cannot decode the trampoline's launch refusal: {decode_text}")),
            (LauncherFailure::ReportEncodingTooLarge(LauncherFailureReportEncodingTooLarge { actual: 65536usize.into(), limit: 65535usize.into() }), "the encoded launch refusal is 65536 bytes, exceeding the report's 65535-byte limit; the original refusal could not be transmitted".into()),
            (LauncherFailure::PayloadGroup(PayloadGroupFailure::Read(io())), "cannot read bwrap's --info-fd record: permission denied".into()),
            (LauncherFailure::PayloadGroup(PayloadGroupFailure::Json(decode)), format!("malformed bwrap --info-fd record: {decode_text}")),
            (LauncherFailure::PayloadGroup(PayloadGroupFailure::InvalidPid(-1)), "bwrap's --info-fd record names invalid child-pid -1".into()),
            (LauncherFailure::ChildPipe(LauncherFailureChildPipe { subject: LaunchImageSubject::File("/tool".into()), cause: os() }), "cannot create the child-setup channel for program '/tool': permission denied".into()),
            (LauncherFailure::ChildFork(LauncherFailureChildFork { subject: LaunchImageSubject::Bundled("rg".into()), cause: os() }), "cannot fork bundled tool 'rg': permission denied".into()),
            (LauncherFailure::ChildSetup(LauncherFailureChildSetup { subject: LaunchImageSubject::File("/tool".into()), cause: os() }), "ral's child setup failed before starting program '/tool': permission denied".into()),
            (LauncherFailure::ChildDiedBeforeSetup(LaunchImageSubject::Bundled("rg".into())), "the child for bundled tool 'rg' died without reporting completion of its setup".into()),
            (LauncherFailure::SpawnBeforeSetup(LauncherFailureSpawnBeforeSetup { subject: LaunchImageSubject::File("/tool".into()), cause: os() }), "ral could not spawn program '/tool' before setup completion was established: permission denied".into()),
            (LauncherFailure::NonNativeExecError(LauncherFailureNonNativeExecError { subject: LaunchImageSubject::File("/tool".into()), cause: io() }), "ral could not construct the execution request for program '/tool': permission denied".into()),
            (LauncherFailure::AnchorReadiness(AnchorReadinessFailure::Read(io())), "cannot read the pipeline anchor's readiness report: permission denied".into()),
            (LauncherFailure::AnchorReadiness(AnchorReadinessFailure::UnexpectedByte(7)), "the pipeline anchor reported readiness byte 7, expected 0".into()),
        ] {
            observers(&LaunchFailure::Launcher(cause), &description, &[]);
        }
    }

    #[test]
    fn detached_receipt_descriptions_keep_the_started_assertion() {
        for (cause, description) in [
            (
                DetachedPidCause::ReaperSilent,
                "the reaper never reported the intermediate process's exit",
            ),
            (
                DetachedPidCause::Reap(io()),
                "cannot reap the intermediate process: permission denied",
            ),
            (
                DetachedPidCause::IntermediateEnded(NativeStatus::Exited(7)),
                "the intermediate process exited with status 7, instead of exiting with status 0",
            ),
            (
                DetachedPidCause::IntermediateEnded(NativeStatus::Signaled(9)),
                "the intermediate process was terminated by signal 9, instead of exiting with status 0",
            ),
            (
                DetachedPidCause::ReceiptRead(io()),
                "cannot read the survivor's PID receipt: permission denied",
            ),
            (
                DetachedPidCause::InvalidPid(-1),
                "the receipt contained -1, which is not a process ID",
            ),
        ] {
            observers(
                &LaunchFailure::DetachedPid(DetachedPidFailure {
                    program: "tool".into(),
                    cause,
                }),
                &format!(
                    "program \"tool\" started, but its survivor PID is unavailable: {description}"
                ),
                &[],
            );
        }
    }

    #[test]
    fn budget_descriptions_explain_why_the_command_is_refused() {
        for (cause, description) in [
            (
                BudgetCountFailure::Kernel(BudgetCountFailureKernel {
                    operation: BudgetProbeOperation::PerUserCeiling,
                    cause: os(),
                }),
                "cannot read kern.maxprocperuid: permission denied",
            ),
            (
                BudgetCountFailure::Kernel(BudgetCountFailureKernel {
                    operation: BudgetProbeOperation::LiveProcesses,
                    cause: os(),
                }),
                "cannot list the current user's processes: permission denied",
            ),
            (
                BudgetCountFailure::InvalidCeiling(-1),
                "kern.maxprocperuid returned invalid process ceiling -1",
            ),
            (
                BudgetCountFailure::CapacityUnaddressable(42usize.into()),
                "a process-list capacity of 42 cannot be addressed",
            ),
            (
                BudgetCountFailure::BufferTooLarge(42usize.into()),
                "the process-list buffer requires 42 bytes, beyond proc_listpids' buffer-size range",
            ),
        ] {
            observers(
                &LaunchFailure::Budget(BudgetFailure::CountProcesses(cause)),
                &format!(
                    "cannot count your processes: {description}; refusing to start the command without a process budget"
                ),
                &[],
            );
        }
        for (cause, description) in [
            (
                BudgetSetFailure::CreateJob(os()),
                "cannot create the budget's Windows Job Object: permission denied",
            ),
            (
                BudgetSetFailure::SetJobLimit(os()),
                "cannot set the Windows Job Object's active-process limit: permission denied",
            ),
            (
                BudgetSetFailure::SetProcessLimit(os()),
                "cannot set the child's process limit: permission denied",
            ),
        ] {
            for (subject, name) in [
                (BudgetSubject::ChildProcesses, "child process"),
                (BudgetSubject::ChildJob, "child's Windows Job Object"),
            ] {
                observers(
                    &LaunchFailure::Budget(BudgetFailure::SetBudget(BudgetFailureSetBudget {
                        subject,
                        limit: 42usize.into(),
                        cause: cause.clone(),
                    })),
                    &format!(
                        "cannot set the {name} budget to 42: {description}; refusing to start the command without that budget"
                    ),
                    &[],
                );
            }
        }
    }

    #[test]
    fn jail_descriptions_and_advice_retain_sequence_and_uncapped_safety_explanations() {
        for (cause, operation) in [
            (
                SequenceFailure::CreateParent(io()),
                "create the parent directory for",
            ),
            (SequenceFailure::Open(io()), "open"),
            (SequenceFailure::Lock(os()), "lock"),
            (SequenceFailure::Read(io()), "read"),
            (SequenceFailure::Write(io()), "write"),
        ] {
            observers(
                &LaunchFailure::Jail(JailFailure::Sequence(JailFailureSequence {
                    path: "/run/seq".into(),
                    cause,
                })),
                &format!(
                    "guest jail: cannot {operation} sequence file '/run/seq': permission denied"
                ),
                &[],
            );
        }
        for (cause, description) in [
            (
                SequenceFailure::Malformed(vec![255, 0]),
                "sequence file '/run/seq' holds [255, 0], not a count of jailed commands",
            ),
            (
                SequenceFailure::Exhausted,
                "every UID reserved for jailed commands has been issued",
            ),
        ] {
            observers(
                &LaunchFailure::Jail(JailFailure::Sequence(JailFailureSequence {
                    path: "/run/seq".into(),
                    cause,
                })),
                &format!("guest jail: {description}"),
                &["Restarting the guest resets its sequence in /run."],
            );
        }
        observers(
            &LaunchFailure::Jail(JailFailure::NoParent("/".into())),
            "guest jail: '/' has no parent, so it is not inside a cgroup2 mount",
            &["Is the jail root the path where this guest mounts cgroup2?"],
        );
        observers(
            &LaunchFailure::Jail(JailFailure::Delegate(JailFailureDelegate {
                parent: "/cg".into(),
                cause: io(),
            })),
            "guest jail: cannot delegate +memory +pids +cpu from '/cg': permission denied; without these controllers the child cgroup cannot enforce its limits, so the command is refused rather than run uncapped",
            &[],
        );
        observers(
            &LaunchFailure::Jail(JailFailure::Create(JailFailureCreate {
                cgroup: "/cg/child".into(),
                cause: io(),
            })),
            "guest jail: cannot create cgroup '/cg/child': permission denied",
            &[],
        );
        for (file, name) in [
            (CgroupLimitFile::MemoryMax, "memory.max"),
            (CgroupLimitFile::PidsMax, "pids.max"),
            (CgroupLimitFile::CpuMax, "cpu.max"),
        ] {
            observers(
                &LaunchFailure::Jail(JailFailure::Limit(JailFailureLimit {
                    cgroup: "/cg/child".into(),
                    file,
                    cause: io(),
                })),
                &format!(
                    "guest jail: cannot write {name} in cgroup '/cg/child': permission denied; refusing to run the command without this limit"
                ),
                &[],
            );
        }
        for (step, operation) in [
            (MembershipStep::OpenProcs, "open cgroup.procs"),
            (
                MembershipStep::WritePid,
                "write the child PID to cgroup.procs",
            ),
            (
                MembershipStep::UnshareNetwork,
                "create an isolated network namespace",
            ),
            (
                MembershipStep::LoopbackSocket,
                "open the loopback configuration socket",
            ),
            (MembershipStep::EnableLoopback, "enable loopback"),
            (MembershipStep::DropGroups, "drop supplementary groups"),
            (MembershipStep::SetGid, "set the jail GID"),
            (MembershipStep::SetUid, "set the jail UID"),
            (MembershipStep::NoNewPrivileges, "enable no_new_privs"),
            (
                MembershipStep::MarkDescriptors,
                "mark inherited descriptors close-on-exec",
            ),
            (MembershipStep::InstallFilter, "install the seccomp filter"),
        ] {
            observers(
                &LaunchFailure::Jail(JailFailure::Membership(JailFailureMembership {
                    cgroup: "/cg/child".into(),
                    step,
                    cause: os(),
                })),
                &format!(
                    "guest jail: cannot {operation} for guest cgroup '/cg/child': permission denied"
                ),
                &[],
            );
        }
    }

    #[test]
    fn handoff_wording_covers_allocation_and_hook_failure_without_guessing_a_syscall() {
        for step in [ParcelStep::Lend, ParcelStep::Keep] {
            let parcel = ParcelFailure {
                role: ParcelRole::Warrant,
                step,
                cause: ParcelCause::Io(io()),
            };
            let description = "cannot prepare a descriptor for the warrant parcel to hand to the confined child: permission denied";
            let advice = [
                "If descriptor allocation failed, check the open-file limit with `sh -c 'ulimit -n'`, and raise it for the session with the rc's `limits: [open-files: N]`",
            ];
            observers(&LaunchFailure::Parcel(parcel.clone()), description, &advice);
            observers(
                &LaunchFailure::Unavailable(UnavailableFailure::BwrapProbe(
                    UnavailableFailureBwrapProbe {
                        binary: "/bwrap".into(),
                        cause: BwrapProbeFailure::Lend(parcel),
                    },
                )),
                &format!(
                    "sandbox confinement unavailable: bwrap at '/bwrap' cannot build an envelope on this host: {description}"
                ),
                &advice,
            );
        }
        observers(
            &LaunchFailure::Parcel(ParcelFailure {
                role: ParcelRole::Warrant,
                step: ParcelStep::Lift,
                cause: ParcelCause::Io(io()),
            }),
            "cannot copy a descriptor for the warrant parcel to fd 98 or above for the confined child: permission denied",
            &[
                "is the open-file limit too low to lend descriptors to the confined child? Check it with `sh -c 'ulimit -n'`, and raise it for the session with the rc's `limits: [open-files: N]`",
            ],
        );
    }

    #[test]
    fn unavailable_descriptions_explain_the_refusal_and_advice_names_the_remedy() {
        for (cause, description, advice) in [
            (
                UnavailableFailure::MissingBootPin(BinaryRole::Bwrap),
                "ral could not find and pin bwrap at startup; a grant confines every external command under bubblewrap, so nothing can start under this grant without it",
                vec!["Is bubblewrap installed on this host and on the PATH ral was started with?"],
            ),
            (
                UnavailableFailure::MissingBootPin(BinaryRole::Ral),
                "ral could not pin its own program at startup, so it cannot vouch that the sandboxed copy is itself; refusing to launch it",
                vec!["Is its executable missing or unreadable?"],
            ),
            (
                UnavailableFailure::Kernel(KernelFeatureFailure::LandlockAbsent),
                "Landlock is unavailable, so this kernel cannot enforce the grant's limits on which programs may run; ral refuses rather than run them unchecked",
                vec!["Is the kernel older than 5.13, or does it have Landlock disabled?"],
            ),
            (
                UnavailableFailure::Kernel(KernelFeatureFailure::LandlockProbe(os())),
                "the Landlock version probe failed: permission denied; this does not establish that Landlock is absent, so ral refuses to launch confined",
                vec![],
            ),
            (
                UnavailableFailure::Kernel(KernelFeatureFailure::CloseRangeAbsent),
                "this kernel has no close_range(2) (Linux 5.9); the handoff cannot be swept, so the program would inherit it; refusing to run",
                vec![],
            ),
        ] {
            observers(
                &LaunchFailure::Unavailable(cause),
                &format!("sandbox confinement unavailable: {description}"),
                &advice,
            );
        }
        for (cause, description) in [
            (
                BwrapProbeFailure::Spawn(os()),
                "it did not start: permission denied",
            ),
            (
                BwrapProbeFailure::Exited(BwrapProbeFailureExited {
                    status: NativeStatus::Exited(1),
                    stderr: None,
                }),
                "it exited with status 1",
            ),
            (
                BwrapProbeFailure::Exited(BwrapProbeFailureExited {
                    status: NativeStatus::Signaled(9),
                    stderr: Some("foreign stderr".into()),
                }),
                "it was terminated by signal 9: foreign stderr",
            ),
            (
                BwrapProbeFailure::RootOpen(ProjectionOpenFailure::Failed(
                    ProjectionOpenFailureFailed {
                        path: "/".into(),
                        cause: io(),
                    },
                )),
                "ral cannot open / to lend bwrap a descriptor: cannot open '/': permission denied",
            ),
            (
                BwrapProbeFailure::RootAbsent,
                "ral cannot open / to lend bwrap a descriptor: no root object was found",
            ),
        ] {
            observers(
                &LaunchFailure::Unavailable(UnavailableFailure::BwrapProbe(
                    UnavailableFailureBwrapProbe {
                        binary: "/bin/bwrap".into(),
                        cause,
                    },
                )),
                &format!(
                    "sandbox confinement unavailable: bwrap at '/bin/bwrap' cannot build an envelope on this host: {description}"
                ),
                &[],
            );
        }
        observers(
            &LaunchFailure::Unavailable(UnavailableFailure::BwrapProbe(
                UnavailableFailureBwrapProbe {
                    binary: "/bin/bwrap".into(),
                    cause: BwrapProbeFailure::RedirectedDestinationAccepted,
                },
            )),
            "sandbox confinement unavailable: bwrap at '/bin/bwrap' cannot build an envelope on this host: it mounts a lent descriptor wherever a symlink at its destination points, rather than refusing it",
            &["Is bubblewrap older than 0.10.0?"],
        );
    }

    #[test]
    fn bwrap_version_advice_inspects_only_exited_foreign_stderr() {
        for (stderr, outdated) in [
            (None, false),
            (Some("Unknown option --ro-bind-fd"), true),
            (Some("unknown option --ro-bind-fd"), true),
            (Some("unrecognized option --ro-bind-fd"), true),
            (Some("Unknown option --something-else"), false),
            (Some("--ro-bind-fd: permission denied"), false),
            (Some("--ro-bind-fd: Bad file descriptor"), false),
        ] {
            for status in [
                NativeStatus::Exited(0),
                NativeStatus::Exited(1),
                NativeStatus::Signaled(9),
            ] {
                let cause = BwrapProbeFailure::Exited(BwrapProbeFailureExited {
                    status,
                    stderr: stderr.map(str::to_owned),
                });
                let expected = if outdated {
                    vec!["Is bubblewrap older than 0.8.0?".to_owned()]
                } else {
                    vec![]
                };
                assert_eq!(cause.advice(), expected);
            }
        }
        observers(
            &LaunchFailure::Unavailable(UnavailableFailure::BwrapProbe(
                UnavailableFailureBwrapProbe {
                    binary: "/bwrap".into(),
                    cause: BwrapProbeFailure::Exited(BwrapProbeFailureExited {
                        status: NativeStatus::Exited(1),
                        stderr: Some("Unknown option --ro-bind-fd".into()),
                    }),
                },
            )),
            "sandbox confinement unavailable: bwrap at '/bwrap' cannot build an envelope on this host: it exited with status 1: Unknown option --ro-bind-fd",
            &["Is bubblewrap older than 0.8.0?"],
        );
        let detail = OsFailure {
            kind: super::super::io::IoKind::Other,
            raw: None,
            detail: Some("Unknown option --ro-bind-fd".into()),
        };
        let local = IoCause {
            context: vec![],
            leaf: Box::new(super::super::io::IoLeaf::Os(detail.clone())),
        };
        for cause in [
            BwrapProbeFailure::Spawn(detail),
            BwrapProbeFailure::RootOpen(ProjectionOpenFailure::Failed(
                ProjectionOpenFailureFailed {
                    path: "/".into(),
                    cause: Box::new(local.clone()),
                },
            )),
            BwrapProbeFailure::Lend(ParcelFailure {
                role: ParcelRole::Trampoline,
                step: ParcelStep::Lend,
                cause: ParcelCause::Io(Box::new(local)),
            }),
        ] {
            assert!(
                cause
                    .advice()
                    .iter()
                    .all(|advice| advice != "Is bubblewrap older than 0.8.0?"),
                "local/OS descriptions cannot imply a bwrap version"
            );
        }
    }

    #[test]
    fn windows_unsupported_axes_explain_what_cannot_be_enforced() {
        for excluding in [false, true] {
            let path = if excluding {
                "every path except some exclusions"
            } else {
                "every path"
            };
            observers(
                &LaunchFailure::Unavailable(UnavailableFailure::Unsupported(
                    UnsupportedConfinement::WindowsEveryPath(
                        UnsupportedConfinementWindowsEveryPath { excluding },
                    ),
                )),
                &format!(
                    "sandbox confinement unavailable: {path}, which Windows cannot enforce: a sandboxed program reaches only folders stamped for it, and ral will not stamp every drive"
                ),
                &["Grant the folders the program needs instead."],
            );
        }
        observers(
            &LaunchFailure::Unavailable(UnavailableFailure::Unsupported(
                UnsupportedConfinement::WindowsAllowanceExclusion(
                    UnsupportedConfinementWindowsAllowanceExclusion {
                        granted: "C:\\allow".into(),
                        denied: "C:\\deny".into(),
                    },
                ),
            )),
            "sandbox confinement unavailable: this command is allowed 'C:\\allow' and 'C:\\deny' is excluded, which Windows cannot enforce together: a sandbox can be kept out of what lies beneath an allowance, but not out of the allowance itself or what holds it",
            &["Allow paths outside 'C:\\deny', run a program outside it, or drop the exclusion."],
        );
    }

    #[test]
    fn pin_descriptions_retain_role_and_optional_spelling() {
        for (cause, description, advice) in [
            (
                PinCause::Unavailable,
                "ral could not pin its own program at startup, so it cannot vouch that the sandboxed copy is itself; refusing to launch it",
                vec!["Is its executable missing or unreadable?"],
            ),
            (
                PinCause::VerificationWalk(io()),
                "cannot verify the pinned ral binary at '/bin/ral': permission denied",
                vec![],
            ),
            (
                PinCause::ChangedIdentity,
                "ral binary at '/bin/ral' changed since startup",
                vec!["Restart to pick up the new build."],
            ),
            (
                PinCause::CurrentNameRead(io()),
                "cannot find where the pinned ral binary '/bin/ral' lives now: permission denied",
                vec![],
            ),
            (
                PinCause::CurrentExecutableRead(io()),
                "cannot resolve ral's own program: permission denied",
                vec![],
            ),
        ] {
            observers(
                &LaunchFailure::Pin(PinFailure {
                    role: BinaryRole::Ral,
                    spelling: Some("/bin/ral".into()),
                    cause,
                }),
                description,
                &advice,
            );
        }
        observers(
            &LaunchFailure::Pin(PinFailure {
                role: BinaryRole::Ral,
                spelling: Some("bin/ral".into()),
                cause: PinCause::RelativePath,
            }),
            "a pin must name an absolute path, but 'bin/ral' is relative",
            &[],
        );
        for (cause, description) in [
            (
                PinCause::VerificationWalk(io()),
                "cannot verify the pinned bwrap binary: permission denied",
            ),
            (
                PinCause::CurrentNameRead(io()),
                "cannot find where the pinned bwrap binary lives now: permission denied",
            ),
            (PinCause::RelativePath, "a pin must name an absolute path"),
        ] {
            observers(
                &LaunchFailure::Pin(PinFailure {
                    role: BinaryRole::Bwrap,
                    spelling: None,
                    cause,
                }),
                description,
                &[],
            );
        }
    }

    #[test]
    fn linked_open_advice_survives_each_launch_composition() {
        let open = || ProjectionOpenFailure::Linked("/grant".into());
        let description =
            "'/grant' was granted by its real path, but it now goes through a symbolic link";
        for (failure, description) in [
            (
                LaunchFailure::Projection(ProjectionFailure::ChangedSymlink(
                    ProjectionFailureChangedSymlink {
                        granted: "/grant".into(),
                    },
                )),
                description.to_owned(),
            ),
            (
                LaunchFailure::Projection(ProjectionFailure::Open(open())),
                description.to_owned(),
            ),
            (
                LaunchFailure::Ruleset(RulesetFailure::Open(open())),
                description.to_owned(),
            ),
            (
                LaunchFailure::Seatbelt(SeatbeltFailure::Profile(ProjectionFailure::Open(open()))),
                format!("cannot build the Seatbelt profile: {description}"),
            ),
            (
                LaunchFailure::Unavailable(UnavailableFailure::BwrapProbe(
                    UnavailableFailureBwrapProbe {
                        binary: "/bwrap".into(),
                        cause: BwrapProbeFailure::RootOpen(open()),
                    },
                )),
                format!(
                    "sandbox confinement unavailable: bwrap at '/bwrap' cannot build an envelope on this host: ral cannot open / to lend bwrap a descriptor: {description}"
                ),
            ),
        ] {
            observers(&failure, &description, &[FROZEN_ALLOWANCE_ADVICE]);
        }
        observers(
            &LaunchFailure::Projection(ProjectionFailure::PinnedDirectoryRedirected(
                ProjectionFailurePinnedDirectoryRedirected {
                    granted: "/grant".into(),
                    reached: "/new".into(),
                },
            )),
            "the directory '/grant' is pinned for this launch, but now reaches '/new' through a symbolic link, which would be shown at its name",
            &["Make it a directory again, or retry once nothing is relinking it."],
        );
    }

    #[test]
    fn projection_descriptions_preserve_the_operation_and_refusal() {
        for (cause, description, advice) in [
            (
                ProjectionFailure::UnrenderablePath("\"/bad\"".into()),
                "sandbox grant path \"/bad\" is not valid Unicode; refusing to render a sandbox rule that could name the wrong path",
                vec![],
            ),
            (
                ProjectionFailure::Open(ProjectionOpenFailure::Failed(
                    ProjectionOpenFailureFailed {
                        path: "/p".into(),
                        cause: io(),
                    },
                )),
                "cannot open '/p': permission denied",
                vec![],
            ),
            (
                ProjectionFailure::Resolve(ProjectionFailureResolve {
                    path: "/p".into(),
                    cause: io(),
                }),
                "cannot resolve '/p': permission denied",
                vec![],
            ),
            (
                ProjectionFailure::List(ProjectionFailureList {
                    path: "/p".into(),
                    cause: io(),
                }),
                "cannot list '/p': permission denied",
                vec![],
            ),
            (
                ProjectionFailure::CopyHandle(ProjectionFailureCopyHandle {
                    path: "/p".into(),
                    cause: io(),
                }),
                "cannot copy the handle on '/p': permission denied",
                vec![],
            ),
            (
                ProjectionFailure::HiddenWorkingDirectory(
                    ProjectionFailureHiddenWorkingDirectory {
                        cwd: "/cwd".into(),
                        exclusion: "/excluded".into(),
                    },
                ),
                "the working directory '/cwd' is excluded by '/excluded', and no confined command can start inside an exclusion",
                vec!["Run it from another directory (`cd` or `within [dir: …]`)."],
            ),
            (
                ProjectionFailure::ProcExcluded(ProjectionFailureProcExcluded {
                    exclusion: "/proc".into(),
                }),
                "the grant denies '/proc', which hides /proc; the envelope needs /proc to start its program",
                vec!["Did you mean a path inside /proc?"],
            ),
        ] {
            observers(&LaunchFailure::Projection(cause), description, &advice);
        }
        for (step, operation) in [
            (CarveStep::OpenDirectory, "open directory"),
            (CarveStep::ListDirectory, "list directory"),
            (CarveStep::LocateEntry, "locate entry"),
        ] {
            observers(
                &LaunchFailure::Projection(ProjectionFailure::Carve(ProjectionFailureCarve {
                    path: "/p".into(),
                    step,
                    cause: io(),
                })),
                &format!("cannot {operation} '/p' to grant around an exclusion: permission denied"),
                &[],
            );
        }
    }

    #[test]
    fn parcel_ruleset_and_filter_descriptions_name_the_actual_object() {
        for (role, step, description) in [
            (
                ParcelRole::ReportSocket,
                ParcelStep::Create,
                "cannot create the confined launch's report socket: permission denied",
            ),
            (
                ParcelRole::InfoSocket,
                ParcelStep::Create,
                "cannot open bwrap's --info-fd socket: permission denied",
            ),
            (
                ParcelRole::Ruleset,
                ParcelStep::Lend,
                "cannot prepare a descriptor for the Landlock ruleset descriptor to hand to the confined child: permission denied",
            ),
        ] {
            let failure = LaunchFailure::Parcel(ParcelFailure {
                role,
                step,
                cause: ParcelCause::Io(io()),
            });
            let advice = failure.advice();
            let advice: Vec<&str> = advice.iter().map(String::as_str).collect();
            observers(&failure, description, &advice);
        }
        let parcel = ParcelFailure {
            role: ParcelRole::Warrant,
            step: ParcelStep::Seal,
            cause: ParcelCause::ChangedBytes,
        };
        observers(
            &LaunchFailure::Parcel(parcel.clone()),
            "the parcel changed before it was sealed",
            &["Did another process open ral's descriptors through /proc?"],
        );
        observers(
            &LaunchFailure::Unavailable(UnavailableFailure::BwrapProbe(
                UnavailableFailureBwrapProbe {
                    binary: "/bwrap".into(),
                    cause: BwrapProbeFailure::Lend(parcel),
                },
            )),
            "sandbox confinement unavailable: bwrap at '/bwrap' cannot build an envelope on this host: the parcel changed before it was sealed",
            &["Did another process open ral's descriptors through /proc?"],
        );
        observers(
            &LaunchFailure::Parcel(ParcelFailure {
                role: ParcelRole::Warrant,
                step: ParcelStep::Write,
                cause: ParcelCause::SocketTooSmall(ParcelCauseSocketTooSmall {
                    bytes: 123_u64.into(),
                    cause: io(),
                }),
            }),
            "a 123-byte parcel does not fit a socket: permission denied",
            &[],
        );
        for step in [ParcelStep::Lend, ParcelStep::Keep, ParcelStep::Lift] {
            let failure = ParcelFailure {
                role: ParcelRole::Warrant,
                step,
                cause: ParcelCause::Io(io()),
            };
            assert!(failure.advice()[0].contains("limits: [open-files: N]"));
            assert!(
                failure
                    .describe()
                    .contains("a descriptor for the warrant parcel")
            );
        }
        for (cause, description) in [
            (
                RulesetFailure::Create(os()),
                "this kernel refused the Landlock ruleset: permission denied",
            ),
            (
                RulesetFailure::Admit(RulesetFailureAdmit {
                    name: "/bin/p".into(),
                    cause: io(),
                }),
                "cannot admit '/bin/p' to the Landlock ruleset: permission denied",
            ),
            (
                RulesetFailure::Restrict(os()),
                "cannot enter the Landlock ruleset: permission denied; refusing to run unconfined",
            ),
            (
                RulesetFailure::MissingFd(98),
                "the launch promised a Landlock ruleset at fd 98 and none arrived; refusing to run unconfined",
            ),
            (
                RulesetFailure::MissingRoot,
                "the envelope has no root to admit to the Landlock ruleset; refusing to run unconfined",
            ),
        ] {
            observers(&LaunchFailure::Ruleset(cause), description, &[]);
        }
        observers(
            &LaunchFailure::Filter(FilterFailure::Arch(FilterFailureArch {
                architecture: "example".into(),
                detail: "foreign detail".into(),
            })),
            "seccomp does not support architecture 'example': foreign detail",
            &[],
        );
        observers(
            &LaunchFailure::Filter(FilterFailure::Compile(FilterFailureCompile {
                detail: "foreign detail".into(),
            })),
            "the seccomp deny-set failed to compile: foreign detail",
            &[],
        );
    }

    #[test]
    fn warrant_validation_describes_why_the_descriptor_or_bytes_are_refused() {
        let invocation = "--warrant is how ral starts its own confined children, never a command to run by hand.";
        observers(
            &LaunchFailure::Warrant(WarrantFailure::Nul(WarrantField::Argument(2_u64.into()))),
            "the warrant's argument 2 holds a NUL byte, which would end it early",
            &[],
        );
        observers(
            &LaunchFailure::Warrant(WarrantFailure::MissingField(WarrantField::BwrapOption(
                3_u64.into(),
            ))),
            "the warrant ends before its bwrap option 3",
            &[],
        );
        observers(
            &LaunchFailure::Warrant(WarrantFailure::MissingDescriptor(
                WarrantFailureMissingDescriptor {
                    field: HandoffField::Stream(StandardStream::Stdin),
                    fd: 100,
                },
            )),
            "no stdin stream descriptor at fd 100",
            &[invocation],
        );
        for (cause, description, advice) in [
            (
                WarrantFailure::MissingTerminator,
                "the warrant does not end in NUL: it was cut short",
                vec![],
            ),
            (WarrantFailure::Magic, "not a ral-warrant/5 warrant", vec![]),
            (
                WarrantFailure::MissingField(WarrantField::Program),
                "the warrant ends before its program",
                vec![],
            ),
            (
                WarrantFailure::Utf8(WarrantFailureUtf8 {
                    field: WarrantField::Program,
                    valid_up_to: 2_u64.into(),
                    error_len: Some(1_u64.into()),
                }),
                "the warrant's program is not UTF-8 at byte 2: invalid 1-byte sequence",
                vec![],
            ),
            (
                WarrantFailure::Utf8(WarrantFailureUtf8 {
                    field: WarrantField::Program,
                    valid_up_to: 2_u64.into(),
                    error_len: None,
                }),
                "the warrant's program is not UTF-8 at byte 2: the sequence is incomplete",
                vec![],
            ),
            (
                WarrantFailure::Confinement("wrong".into()),
                "the warrant's confinement \"wrong\" is not unconfined, landlock@<fd>, or landlock+refer@<fd>",
                vec![],
            ),
            (
                WarrantFailure::Slot(WarrantFailureSlot {
                    field: HandoffField::Warrant,
                    spelling: "97".into(),
                }),
                "\"97\" names no warrant handoff descriptor; expected plain digits naming fd 98 or above",
                vec![invocation],
            ),
            (
                WarrantFailure::SlotCount(WarrantFailureSlotCount {
                    actual: 1_u64.into(),
                }),
                "--warrant descriptor-argument count is 1; expected 2 (warrant and report), or 5 under a Linux envelope",
                vec![invocation],
            ),
            (
                WarrantFailure::DuplicateSlot(98),
                "two handoff owners name fd 98; one would be closed twice",
                vec![invocation],
            ),
            (
                WarrantFailure::MissingDescriptor(WarrantFailureMissingDescriptor {
                    field: HandoffField::Report,
                    fd: 99,
                }),
                "no report descriptor at fd 99",
                vec![invocation],
            ),
            (
                WarrantFailure::NotReportSocket(99),
                "fd 99 is not ral's report socket",
                vec![invocation],
            ),
            (
                WarrantFailure::UnknownImageKind("unknown".into()),
                "the warrant names an unknown kind of program, \"unknown\"",
                vec![],
            ),
            (
                WarrantFailure::InvalidProgram(WarrantFailureInvalidProgram {
                    shown: "sh".into(),
                    cause: io(),
                }),
                "the warrant's file program 'sh' is not a canonical absolute real path: permission denied; refusing PATH lookup",
                vec![],
            ),
            (
                WarrantFailure::UnknownBundledTool("unknown".into()),
                "the warrant names an unknown bundled tool, \"unknown\"",
                vec![],
            ),
            (
                WarrantFailure::ArgumentCountSyntax("+1".into()),
                "the warrant's argument count \"+1\" is not an addressable unsigned integer",
                vec![],
            ),
            (
                WarrantFailure::ArgumentCount(WarrantFailureArgumentCount {
                    declared: 2_u64.into(),
                    actual: 1_u64.into(),
                }),
                "the warrant declares 2 arguments but carries 1",
                vec![],
            ),
            (
                WarrantFailure::Nul(WarrantField::Argv0),
                "the warrant's argv0 holds a NUL byte, which would end it early",
                vec![],
            ),
            (
                WarrantFailure::Noncanonical,
                "the warrant is not in canonical form",
                vec![],
            ),
            (
                WarrantFailure::Size(WarrantFailureSize {
                    actual: 101_u64.into(),
                    limit: 100_u64.into(),
                }),
                "the warrant is 101 bytes, over its 100-byte cap",
                vec!["Is the command line that long?"],
            ),
            (
                WarrantFailure::Read(io()),
                "cannot read the warrant: permission denied",
                vec![],
            ),
            (
                WarrantFailure::SealProbe(os()),
                "cannot inspect the warrant's memfd seals: permission denied",
                vec![],
            ),
            (
                WarrantFailure::MissingSeals,
                "the warrant's memfd lacks required seals; refusing it",
                vec![],
            ),
            (
                WarrantFailure::Rewind(os()),
                "cannot rewind the warrant: permission denied",
                vec![],
            ),
            (
                WarrantFailure::NotSocket,
                "the warrant is not a socket; refusing it",
                vec![],
            ),
            (
                WarrantFailure::SocketStat(os()),
                "cannot stat the warrant: permission denied",
                vec![],
            ),
            (
                WarrantFailure::StreamInstall(WarrantFailureStreamInstall {
                    stream: StandardStream::Stdout,
                    source: 100,
                    cause: os(),
                }),
                "cannot install payload stdout from fd 100: permission denied",
                vec![],
            ),
        ] {
            observers(&LaunchFailure::Warrant(cause), description, &advice);
        }
    }

    #[test]
    fn descriptor_sweep_failures_name_the_syscall_and_refuse_to_run() {
        for (cause, description) in [
            (
                DescriptorSweepFailure::CloseRange(DescriptorSweepFailureCloseRange {
                    first: 3_u64.into(),
                    last: 98_u64.into(),
                    cause: os(),
                }),
                "cannot close inherited descriptors 3 through 98 with close_range(2): permission denied; refusing to run",
            ),
            (
                DescriptorSweepFailure::SetReportCloexec(DescriptorSweepFailureSetReportCloexec {
                    fd: 99,
                    cause: os(),
                }),
                "cannot set the report socket at fd 99 close-on-exec: permission denied; refusing to run",
            ),
            (
                DescriptorSweepFailure::BufferTooLarge(DescriptorSweepFailureBufferTooLarge {
                    bytes: u64::MAX.into(),
                }),
                "cannot size the proc_pidinfo(PROC_PIDLISTFDS) buffer: 18446744073709551615 bytes are not addressable by that API; refusing to run",
            ),
            (
                DescriptorSweepFailure::ListDescriptors(os()),
                "proc_pidinfo(PROC_PIDLISTFDS) failed: permission denied; refusing to run",
            ),
            (
                DescriptorSweepFailure::PartialDescriptorRecord(
                    DescriptorSweepFailurePartialDescriptorRecord {
                        bytes: 7_u64.into(),
                        width: 8_u64.into(),
                    },
                ),
                "proc_pidinfo(PROC_PIDLISTFDS) returned 7 bytes, not a whole number of 8-byte descriptor records; refusing to run",
            ),
            (
                DescriptorSweepFailure::CloseDescriptor(DescriptorSweepFailureCloseDescriptor {
                    fd: 100,
                    cause: os(),
                }),
                "cannot close inherited fd 100: permission denied; refusing to run",
            ),
        ] {
            observers(
                &LaunchFailure::Warrant(WarrantFailure::DescriptorSweep(cause)),
                description,
                &[],
            );
        }
    }

    #[test]
    fn seatbelt_distinguishes_profile_construction_from_entry_and_nested_confinement() {
        for detail in [None, Some("foreign diagnostic".to_owned())] {
            let suffix = detail
                .as_ref()
                .map_or_else(String::new, |detail| format!(": {detail}"));
            observers(
                &LaunchFailure::Seatbelt(SeatbeltFailure::AlreadyConfined(
                    SeatbeltFailureAlreadyConfined {
                        detail: detail.clone(),
                    },
                )),
                &format!(
                    "this process is already inside a macOS sandbox profile and macOS will not let it enter a second, so the launch is refused rather than run under that wider profile{suffix}"
                ),
                &["Is ral itself running confined?"],
            );
            observers(
                &LaunchFailure::Seatbelt(SeatbeltFailure::Entry(SeatbeltFailureEntry {
                    cause: os(),
                    detail,
                })),
                &format!("cannot enter the Seatbelt sandbox: permission denied{suffix}"),
                &[],
            );
        }
        observers(
            &LaunchFailure::Seatbelt(SeatbeltFailure::Nul),
            "the Seatbelt profile contains a NUL byte",
            &[],
        );
    }

    #[test]
    fn dacl_redirected_descriptions_and_advice_never_reveal_a_target() {
        for (target_kind, description, advice) in [
            (
                RedirectedTargetKind::MountedVolume,
                "'C:\\grant' was granted by its real path, but 'C:\\at' is where a volume is mounted; a volume mounted in a folder cannot be granted on Windows because it cannot be told from a junction",
                "Would giving the volume a drive letter and granting that do?",
            ),
            (
                RedirectedTargetKind::JunctionOrLink,
                "'C:\\grant' was granted by its real path, but 'C:\\at' is now a junction or symbolic link",
                FROZEN_ALLOWANCE_ADVICE,
            ),
            (
                RedirectedTargetKind::ReparsePoint,
                "'C:\\grant' was granted by its real path, but 'C:\\at' is now a reparse point",
                FROZEN_ALLOWANCE_ADVICE,
            ),
        ] {
            observers(
                &LaunchFailure::Dacl(DaclFailure::Redirected(DaclFailureRedirected {
                    granted: "C:\\grant".into(),
                    at: "C:\\at".into(),
                    target_kind,
                })),
                &format!("AppContainer confinement failed: {description}"),
                &[advice],
            );
        }
    }

    #[test]
    fn dacl_reachable_is_unavailable_confinement_with_disjunctive_explanation() {
        for (at, inside) in [
            ("C:\\deny", ""),
            (
                "C:\\deny\\file",
                " (it lies inside the excluded 'C:\\deny')",
            ),
        ] {
            observers(
                &LaunchFailure::Dacl(DaclFailure::Reachable(DaclFailureReachable {
                    denied: "C:\\deny".into(),
                    at: at.into(),
                })),
                &format!(
                    "sandbox confinement unavailable: '{at}' is excluded{inside}, but a sandboxed program could still open it: its permissions may admit every Windows sandbox (as ALL APPLICATION PACKAGES does), or it may retain access inherited from an allowed folder (for example, through a hard-linked file), and Windows cannot take such access away"
                ),
                &["Remove that access from it, or drop the exclusion."],
            );
        }
    }

    #[test]
    fn dacl_descriptions_distinguish_operations_ledger_and_sid_failures() {
        for (cause, description, advice) in [
            (
                DaclFailure::NotLocal("C:\\p".into()),
                "'C:\\p' is not on a local fixed volume; network shares, removable drives, and unnamed volumes cannot be granted",
                vec!["Grant a folder on a local disk."],
            ),
            (
                DaclFailure::NotUnicode(r#""C:\\p""#.into()),
                r#"Windows grants name their objects by text, but "C:\\p" is not valid Unicode; refusing an unfaithful spelling rather than approximating it"#,
                vec![],
            ),
            (
                DaclFailure::WriteDacDenied(DaclFailureWriteDacDenied {
                    path: "C:\\p".into(),
                    operation: DaclOperation::SetSecurityInfo,
                    cause: os(),
                }),
                "WRITE_DAC denied on 'C:\\p' during set security info: permission denied",
                vec![],
            ),
            (
                DaclFailure::AccessCheck(DaclFailureAccessCheck {
                    operation: AccessCheckOperation::Check,
                    cause: os(),
                }),
                "cannot verify the sandbox exclusion with AuthzAccessCheck: permission denied",
                vec![],
            ),
            (
                DaclFailure::Win32(DaclFailureWin32 {
                    path: Some("C:\\p".into()),
                    operation: DaclOperation::GetAce,
                    cause: DaclWin32Cause::Io(io()),
                }),
                "get ace failed on 'C:\\p': permission denied",
                vec![],
            ),
            (
                DaclFailure::Win32(DaclFailureWin32 {
                    path: None,
                    operation: DaclOperation::WaitMutex,
                    cause: DaclWin32Cause::UnexpectedWaitResult(5),
                }),
                "wait mutex failed: unexpected wait result: 5",
                vec![],
            ),
            (
                DaclFailure::LedgerIo(LedgerIoFailure::Io(io())),
                "DACL ledger I/O failed: permission denied",
                vec![],
            ),
            (
                DaclFailure::LedgerIo(LedgerIoFailure::RetryExhausted(
                    LedgerIoFailureRetryExhausted {
                        attempts: 10_u64.into(),
                    },
                )),
                "DACL ledger I/O failed: retry exhausted after 10 attempts",
                vec![],
            ),
            (
                DaclFailure::InvalidSid(SidFailure::Empty),
                "the DACL SID is empty",
                vec![],
            ),
            (
                DaclFailure::InvalidSid(SidFailure::TooLong(SidFailureTooLong {
                    bytes: 101_u64.into(),
                    limit: 100_u64.into(),
                })),
                "the DACL SID is 101 bytes, over its 100-byte limit",
                vec![],
            ),
            (
                DaclFailure::InvalidSid(SidFailure::Parse(SidFailureParse {
                    text: "S-bad".into(),
                    cause: os(),
                })),
                "cannot parse Windows SID \"S-bad\": permission denied",
                vec![],
            ),
            (
                DaclFailure::InvalidSid(SidFailure::Invalid("S-bad".into())),
                "the Windows SID parser returned an invalid SID for \"S-bad\"",
                vec![],
            ),
            (
                DaclFailure::MutexTimeout(DaclFailureMutexTimeout {
                    path: "C:\\p".into(),
                    timeout_ms: 1000_u64.into(),
                }),
                "timed out acquiring DACL mutex on 'C:\\p' after 1000 ms",
                vec![],
            ),
        ] {
            observers(
                &LaunchFailure::Dacl(cause),
                &format!("AppContainer confinement failed: {description}"),
                &advice,
            );
        }
        let cause = crate::data::json::from_datum_slice::<LaunchFailure>(b"{").unwrap_err();
        let description = format!(
            "AppContainer confinement failed: cannot decode DACL ledger 'C:\\ledger': {}",
            cause.describe()
        );
        observers(
            &LaunchFailure::Dacl(DaclFailure::LedgerParse(DaclFailureLedgerParse {
                path: "C:\\ledger".into(),
                cause,
            })),
            &description,
            &[],
        );
    }

    #[test]
    fn launch_report_context_is_not_intrinsic_description_or_stored_advice() {
        let failure = LaunchFailure::Pin(PinFailure {
            role: BinaryRole::Ral,
            spelling: Some("/bin/ral".into()),
            cause: PinCause::ChangedIdentity,
        });
        let reason = super::super::Reason::Launch(Box::new(failure));
        let error = crate::error::Error::new(reason.clone()).with_context("bundled tool 'cat'");
        assert_eq!(error.reason, reason);
        assert_eq!(
            error.describe(),
            "ral binary at '/bin/ral' changed since startup"
        );
        assert_eq!(
            error.to_string(),
            "bundled tool 'cat': ral binary at '/bin/ral' changed since startup"
        );
        assert_eq!(error.hints(), []);
        assert_eq!(error.reason.advice(), ["Restart to pick up the new build."]);
        assert!(
            error
                .plain()
                .contains("hint: Restart to pick up the new build.")
        );
        assert!(!format!("{:?}", reason.encode()).contains("bundled tool"));
    }
}
