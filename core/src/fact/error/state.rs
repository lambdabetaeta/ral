//! Portable state failure facts.

use super::shared::{Count, ValueShape};
use crate::{label, record, variant};
use strum::{IntoStaticStr, VariantArray};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationState {
    pub operation: String,
}
record!(OperationState {
    operation: "operation"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCap {
    pub limit: Count,
}
record!(WorkerCap { limit: "limit" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum AmbientSetting {
    Home,
    User,
    Scratch,
}
label!(AmbientSetting);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCycle {
    pub chain: Vec<String>,
}
record!(ModuleCycle { chain: "chain" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum DepthKind {
    EvalStack,
    NativeReentry,
    Source,
}
label!(DepthKind);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepthFailure {
    pub kind: DepthKind,
    pub limit: Count,
}
record!(DepthFailure {
    kind: "kind",
    limit: "limit"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureLimit {
    pub bytes: Count,
}
record!(CaptureLimit { bytes: "bytes" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHookRef {
    pub plugin: String,
    pub hook: String,
}
record!(PluginHookRef {
    plugin: "plugin",
    hook: "hook"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookRef {
    Session(String),
    Plugin(PluginHookRef),
}
variant!(HookRef { Session(String): "session", Plugin(PluginHookRef): "plugin" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSignature {
    Block,
    Hook(String),
}
variant!(HookSignature { Block: "block", Hook(String): "hook" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookNotFunction {
    pub name: HookRef,
    pub actual: ValueShape,
}
record!(HookNotFunction {
    name: "name",
    actual: "actual"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookArity {
    pub name: HookRef,
    pub expected: Count,
    pub actual: Count,
    pub signature: HookSignature,
}
record!(HookArity {
    name: "name",
    expected: "expected",
    actual: "actual",
    signature: "signature"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookRegistrationFailure {
    NotFunction(HookNotFunction),
    Arity(HookArity),
    AlreadyRegistered(HookRef),
    NoSession,
}
variant!(HookRegistrationFailure { NotFunction(HookNotFunction): "not-function", Arity(HookArity): "arity", AlreadyRegistered(HookRef): "already-registered", NoSession: "no-session" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateFailure {
    WorkerClaimed(OperationState),
    EmptyWait(OperationState),
    WorkerCap(WorkerCap),
    GatherClosed,
    MissingAmbient(AmbientSetting),
    ModuleCycle(ModuleCycle),
    Depth(DepthFailure),
    CaptureLimit(CaptureLimit),
    NoByteInput(OperationState),
    TerminalEof,
    MissingBuiltin(String),
    MissingHook(HookRef),
    HookRegistration(HookRegistrationFailure),
    AliasMissing(String),
    AliasConflict(String),
    NoDesk,
    NoForks,
    NoChildEngines,
    NurseryMissing(Count),
}
variant!(StateFailure { WorkerClaimed(OperationState): "worker-claimed", EmptyWait(OperationState): "empty-wait", WorkerCap(WorkerCap): "worker-cap", GatherClosed: "gather-closed", MissingAmbient(AmbientSetting): "missing-ambient", ModuleCycle(ModuleCycle): "module-cycle", Depth(DepthFailure): "depth", CaptureLimit(CaptureLimit): "capture-limit", NoByteInput(OperationState): "no-byte-input", TerminalEof: "terminal-eof", MissingBuiltin(String): "missing-builtin", MissingHook(HookRef): "missing-hook", HookRegistration(HookRegistrationFailure): "hook-registration", AliasMissing(String): "alias-missing", AliasConflict(String): "alias-conflict", NoDesk: "no-desk", NoForks: "no-forks", NoChildEngines: "no-child-engines", NurseryMissing(Count): "nursery-missing" });

impl OperationState {
    pub fn describe(&self) -> String {
        format!("operation: {}", self.operation)
    }
}

impl WorkerCap {
    pub fn describe(&self) -> String {
        format!("limit: {}", self.limit)
    }
}

impl AmbientSetting {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ModuleCycle {
    pub fn describe(&self) -> String {
        format!("chain: {}", self.chain.join(", "))
    }
}

impl DepthKind {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl DepthFailure {
    pub fn describe(&self) -> String {
        match self.kind {
            DepthKind::EvalStack => format!(
                "recursion limit exceeded ({})",
                crate::text::plural(self.limit.to_u64(), "frame")
            ),
            DepthKind::NativeReentry => format!("native re-entry depth exceeded ({})", self.limit),
            DepthKind::Source => format!(
                "{} depth exceeded its limit {}",
                self.kind.describe(),
                self.limit
            ),
        }
    }
}

impl CaptureLimit {
    pub fn describe(&self) -> String {
        format!(
            "capture exceeded {} and nothing was kept: a prefix is not what the command wrote",
            crate::text::plural(self.bytes.to_u64(), "byte")
        )
    }
}

impl PluginHookRef {
    pub fn describe(&self) -> String {
        format!("{:?} from plugin {:?}", self.hook, self.plugin)
    }
}

impl HookRef {
    pub fn describe(&self) -> String {
        match self {
            Self::Session(value) => format!("{value:?}"),
            Self::Plugin(value) => value.describe(),
        }
    }
}

impl HookSignature {
    pub fn describe(&self) -> String {
        match self {
            Self::Block => "block".to_owned(),
            Self::Hook(value) => format!("{value} hook"),
        }
    }
}

impl HookNotFunction {
    pub fn describe(&self) -> String {
        format!(
            "hook {} must be a function, got {}",
            self.name.describe(),
            self.actual.describe()
        )
    }
}

impl HookArity {
    pub fn describe(&self) -> String {
        format!(
            "cannot register {} as a {}: expected {} parameter{}, got {}",
            self.name.describe(),
            self.signature.describe(),
            self.expected,
            if self.expected.to_u64() == 1 { "" } else { "s" },
            self.actual
        )
    }
}

impl HookRegistrationFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::NotFunction(value) => value.describe(),
            Self::Arity(value) => value.describe(),
            Self::AlreadyRegistered(value) => format!("hook {} is already registered", value.describe()),
            Self::NoSession => "hooks and plugins belong to the session, but this shell runs beside it (an aside hook, a spawned worker, a pipeline stage)".to_owned(),
        }
    }
}

impl StateFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::WorkerClaimed(state) => {
                format!("{}: worker was already claimed", state.operation)
            }
            Self::EmptyWait(state) => format!(
                "{}: cannot wait on an empty set of workers",
                state.operation
            ),
            Self::WorkerCap(cap) => {
                format!(
                    "{} {} still executing or stopping",
                    crate::text::plural(cap.limit.to_u64(), "worker"),
                    if cap.limit.to_u64() == 1 { "is" } else { "are" }
                )
            }
            Self::GatherClosed => "the gather scope is closed".into(),
            Self::MissingAmbient(AmbientSetting::Home) => {
                "HOME is not set: `~` and `home` need a home directory".into()
            }
            Self::MissingAmbient(AmbientSetting::User) => {
                "`user` needs USER (or USERNAME on Windows), and neither is set".into()
            }
            Self::MissingAmbient(setting) => {
                format!("no ambient {} is available", setting.describe())
            }
            Self::ModuleCycle(cycle) => format!("module cycle: {}", cycle.chain.join(" → ")),
            Self::Depth(cause) => cause.describe(),
            Self::CaptureLimit(cause) => cause.describe(),
            Self::NoByteInput(state) => format!(
                "{}: no byte input; standard input is a terminal",
                state.operation
            ),
            Self::TerminalEof => "the terminal reached end of input".into(),
            Self::MissingBuiltin(name) => format!("required builtin {name:?} is unavailable"),
            Self::MissingHook(hook) => format!("hook {} is not installed", hook.describe()),
            Self::HookRegistration(cause) => cause.describe(),
            Self::AliasMissing(name) => format!("alias {name:?} is not installed"),
            Self::AliasConflict(name) => format!("alias {name:?} is already installed"),
            Self::NoDesk => "this host answers no enquiries".into(),
            Self::NoForks => "this host cannot fork an engine".into(),
            Self::NoChildEngines => "this host provides no child engines".into(),
            Self::NurseryMissing(id) => {
                format!("no forked session is parked under nursery id {id}")
            }
        }
    }
}

impl StateFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::NoDesk => vec!["only a foreground run has a desk: call it directly, not in `spawn` or a pipeline stage".into()],
            Self::MissingAmbient(AmbientSetting::User) => vec!["set USER, e.g. `within [env: [USER: \"me\"]] { … }`".into()],
            Self::MissingAmbient(AmbientSetting::Home) => vec!["set HOME, or spell out an explicit path".into()],
            Self::WorkerCap(_) => vec!["wait for one to end (`workers` lists them), or cancel work you no longer need".into()],
            Self::GatherClosed => vec!["the newly born worker was cancelled".into()],
            Self::HookRegistration(HookRegistrationFailure::NoSession) => vec!["did you mean to register it from the rc or the prompt itself?".into()],
            Self::Depth(DepthFailure { kind: DepthKind::EvalStack, .. }) => vec!["usually a runaway recursive function: raise via rc recursion-limit: or --recursion-limit".into()],
            Self::Depth(DepthFailure { kind: DepthKind::NativeReentry, .. }) => vec!["a builtin such as `map` applied a function that itself applied `map`, too deep: restructure with a tail-recursive loop".into()],
            Self::CaptureLimit(_) => vec!["did you mean to write this to a file? `cmd > out.txt` keeps every byte".into()],
            Self::NoByteInput(_) => vec!["pipe bytes or redirect a file".into()],
            Self::NurseryMissing(_) => vec!["was it adopted already, or has the run that forked it ended?".into()],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;

    fn projection(failure: &StateFailure, description: &str, advice: &[&str]) {
        assert_eq!(failure.describe(), description);
        assert_eq!(failure.advice(), advice);
        let decoded = StateFailure::decode(&failure.clone().encode()).unwrap();
        assert_eq!(&decoded, failure);
        assert_eq!(decoded.describe(), description);
        assert_eq!(decoded.advice(), advice);
    }

    #[test]
    fn depth_and_capture_use_the_actual_limit_without_losing_bytes() {
        for (kind, description, advice) in [
            (
                DepthKind::EvalStack,
                "recursion limit exceeded (1 frame)",
                vec![
                    "usually a runaway recursive function: raise via rc recursion-limit: or --recursion-limit",
                ],
            ),
            (
                DepthKind::NativeReentry,
                "native re-entry depth exceeded (1)",
                vec![
                    "a builtin such as `map` applied a function that itself applied `map`, too deep: restructure with a tail-recursive loop",
                ],
            ),
            (
                DepthKind::Source,
                "source depth exceeded its limit 1",
                vec![],
            ),
        ] {
            projection(
                &StateFailure::Depth(DepthFailure {
                    kind,
                    limit: Count::from_u64(1),
                }),
                description,
                &advice,
            );
        }
        for bytes in [0, 1, 1_048_577, 9_223_372_036_854_775_809] {
            projection(
                &StateFailure::CaptureLimit(CaptureLimit {
                    bytes: Count::from_u64(bytes),
                }),
                &format!(
                    "capture exceeded {bytes} {} and nothing was kept: a prefix is not what the command wrote",
                    if bytes == 1 { "byte" } else { "bytes" }
                ),
                &["did you mean to write this to a file? `cmd > out.txt` keeps every byte"],
            );
        }
    }

    #[test]
    fn hook_arity_keeps_the_parameter_noun_and_hook_kind() {
        for (expected, signature, description) in [
            (
                1,
                HookSignature::Hook("post-exec".into()),
                "cannot register \"after\" as a post-exec hook: expected 1 parameter, got 2",
            ),
            (
                0,
                HookSignature::Block,
                "cannot register \"after\" as a block: expected 0 parameters, got 2",
            ),
            (
                9_223_372_036_854_775_809,
                HookSignature::Block,
                "cannot register \"after\" as a block: expected 9223372036854775809 parameters, got 2",
            ),
        ] {
            projection(
                &StateFailure::HookRegistration(HookRegistrationFailure::Arity(HookArity {
                    name: HookRef::Session("after".into()),
                    expected: Count::from_u64(expected),
                    actual: Count::from_u64(2),
                    signature,
                })),
                description,
                &[],
            );
        }
    }

    #[test]
    fn nursery_missing_explains_once_only_adoption_without_losing_the_id() {
        for id in [0, 7, u64::MAX] {
            let failure = StateFailure::NurseryMissing(Count::from_u64(id));
            let description = format!("no forked session is parked under nursery id {id}");
            let advice = "was it adopted already, or has the run that forked it ended?";
            projection(&failure, &description, &[advice]);
            let reason = crate::fact::Reason::State(Box::new(failure));
            assert_eq!(reason.advice(), [advice]);
            let error = crate::error::Error::new(reason);
            assert_eq!(error.hints(), []);
            assert_eq!(error.plain(), format!("{description}\nhint: {advice}"));
        }
    }
}
