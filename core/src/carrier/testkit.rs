//! Engines for core's own tests, booted as every engine is.

use super::IdentityTransport;
use crate::engine::EngineInstaller;
use crate::error::Error;
use crate::io::terminal::TerminalState;
use crate::path::EnginePath;
use crate::protocol::{Attach, EngineSpec, Report, Root, Run};
use crate::shell::runtime::{Runtime, Shell};
use crate::source::SourceOrigin;

/// An installer that never makes children, so it states the one policy a host
/// with no grant lexicon can: no seeded child.
pub(crate) const fn installer(
    tag: &'static str,
    runtime: fn(&EngineSpec) -> Result<Runtime, Error>,
    root: fn(&Root, &mut Shell) -> Result<(), Error>,
) -> EngineInstaller {
    EngineInstaller {
        tag,
        runtime,
        root,
        narrow: |_, _| {
            Err(Error::new(crate::fact::Reason::State(Box::new(
                crate::fact::StateFailure::NoChildEngines,
            ))))
        },
    }
}

/// A runtime with no prelude and no surface.
pub(crate) static BARE: [EngineInstaller; 1] = [installer(
    "bare",
    |_| crate::HostSurface::default().runtime(TerminalState::default()),
    |_, _| Ok(()),
)];

/// An attach to `installers`' first recipe, seated in the temp dir.
pub(crate) fn attach(installers: &[EngineInstaller]) -> Attach {
    let temp = std::env::temp_dir();
    Attach::new(
        installers[0].tag,
        EnginePath::Host(temp.clone()),
        EnginePath::Host(temp),
    )
}

/// Boot `attach` in this process.
pub(crate) fn boot_at(
    installers: &'static [EngineInstaller],
    attach: &Attach,
) -> IdentityTransport {
    IdentityTransport::boot(installers, attach).expect("a test recipe boots")
}

/// Boot `installers`' first recipe, seated in the temp dir.
pub(crate) fn boot(installers: &'static [EngineInstaller]) -> IdentityTransport {
    boot_at(installers, &attach(installers))
}

/// Dispatch `src` under the mute host.
pub(crate) fn eval(transport: &IdentityTransport, src: &str) -> Report {
    super::dispatch_to_report(
        transport,
        Run::captured(src, SourceOrigin::named("<test>")),
        std::sync::Arc::new(()),
        None,
    )
    .expect("an identity transport answers synchronously")
}

pub(crate) fn reason_families() -> Vec<crate::fact::Reason> {
    use crate::fact::*;
    vec![
        Reason::process("missing", Failure::NotFound),
        Reason::process(
            "denied",
            Failure::not_runnable("denied", Unrunnable::PermissionDenied),
        ),
        Reason::TimedOut,
        Reason::Cancelled,
        Reason::raised("authored failure"),
        Reason::Denied(Box::new(Denial::exec_run(
            "git".into(),
            Withholding::MissingAllowance(SingleOrNested::Nested),
        ))),
        Reason::Io(Box::new(IoFailure {
            operation: IoOperation::Read,
            subject: IoSubject::Path(crate::text::quote_native(std::ffi::OsStr::new("input"))),
            cause: IoCause::new(IoLeaf::Torn),
        })),
        Reason::Invalid(Box::new(InvalidFailure {
            operation: "test".into(),
            path: vec![DataStep::Index(u64::MAX.into())],
            cause: InvalidCause::Empty,
        })),
        Reason::Evaluation(Box::new(EvalFailure::MissingBinding("binding".into()))),
        Reason::State(Box::new(StateFailure::CaptureLimit(CaptureLimit {
            bytes: u64::MAX.into(),
        }))),
        Reason::Compile(Box::new(CompileFailure {
            source: "source.ral".into(),
            phase: CompilePhase::Types(TypeErrorCount {
                count: (i64::MAX as u64 + 1).into(),
            }),
        })),
        Reason::Launch(Box::new(LaunchFailure::BatchImage("build.cmd".into()))),
        Reason::Protocol(Box::new(ProtocolFailure::UnknownInstaller("recipe".into()))),
        Reason::Host(Box::new(HostFailure::Ral(RalFailure::Boot(
            BootFailure::AlreadyBooted,
        )))),
        Reason::Invariant(Box::new(InvariantFailure::HandleInSeed)),
        Reason::Host(Box::new(HostFailure::Ral(RalFailure::Keybinding(
            KeybindingFailure::Notation(KeybindingFailureNotation {
                key: "shift-alt-ß".into(),
                cause: KeyNotationFailure::ShiftedCharacter(KeyNotationFailureShiftedCharacter {
                    chord: "shift-alt-ß".into(),
                    character: "ß".into(),
                    control: false,
                    alt: true,
                }),
            }),
        )))),
        Reason::Invalid(Box::new(InvalidFailure {
            operation: "load-plugin".into(),
            path: vec![DataStep::Field("zoxide".into())],
            cause: InvalidCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Map),
                found: Found::Int(7),
            }),
        })),
        Reason::Host(Box::new(HostFailure::Ral(RalFailure::Editor(
            EditorFailure::Command(EditorFailureCommand {
                variable: EditorVariable::Visual,
                cause: EditorCommandCause::Unbalanced,
            }),
        )))),
        Reason::State(Box::new(StateFailure::NurseryMissing(u64::MAX.into()))),
    ]
}

/// Report-only host state which neither carrier may import into the engine.
pub(crate) fn host_error(reason: crate::fact::Reason) -> Error {
    use crate::source::{FileId, Source, SourceOrigin, Span};
    let span = Span::new(FileId::DUMMY, 0, 1);
    let mut error = Error::new(reason)
        .with_context("inner")
        .with_context("outer")
        .with_hints(["first", "second", "first", ""])
        .with_witness(Some(span))
        .with_rejection(crate::diagnostic::Rejection {
            source: Source::new(SourceOrigin::named("host.ral"), "x".into()),
            reports: vec![],
            status: std::num::NonZeroU8::MIN,
            nested: None,
        })
        .abandoning_tail()
        .discarded(17);
    error.prov = Some(crate::error::Prov::Live {
        name: "host".into(),
        span,
    });
    error
}

pub(crate) fn assert_normalized(original: &Error, received: &Error) {
    use crate::fact::FailureCode as _;
    assert_eq!(received.reason, original.reason);
    assert_eq!(received.describe(), original.describe());
    assert_eq!(received.reason.advice(), original.reason.advice());
    let mut presentation =
        Error::new(original.reason.clone()).with_hints(original.hints().iter().cloned());
    if let Some(context) = original.context() {
        presentation = presentation.with_context(context);
    }
    assert_eq!(received.plain(), presentation.plain());
    assert_eq!(received.failure_code(), original.failure_code());
    assert_eq!(received.context(), original.context());
    assert_eq!(received.hints(), original.hints());
    assert!(received.prov.is_none());
    assert!(received.witness().is_none());
    assert!(received.rejection().is_none());
    assert_eq!(received.unwound(), crate::error::Unwound::default());
}

/// A real engine-side enquiry door, for provenance/report fixtures.
pub(crate) fn enquiring_runtime(spec: &EngineSpec) -> Result<Runtime, Error> {
    use crate::shell::runtime::{BuiltinBody, BuiltinEntry, Value};
    use crate::typecheck::builtins::{pure, thunk};
    let mut runtime = crate::HostSurface::default().runtime(spec.terminal)?;
    let entry = BuiltinEntry::new(
        std::borrow::Cow::Borrowed("ask-host"),
        |_| thunk(pure(crate::ty::Ty::unit())),
        "test-only: ask the host",
        BuiltinBody::Captured(std::sync::Arc::new(|_, mooring, shell| {
            shell.enquire(mooring, crate::data::FOValue::Unit)?;
            Ok(Value::Unit)
        })),
    );
    runtime.install_captured_builtins(&vec![entry].into());
    Ok(runtime)
}

pub(crate) fn assert_enquiry_report(report: &Report, reason: &crate::fact::Reason) {
    use crate::fact::FailureCode as _;
    let Report::Ran(crate::protocol::Ran {
        ending: crate::protocol::Ending::Err(record),
        rendered,
        ..
    }) = report
    else {
        panic!("an enquiry refusal is an evaluated error report: {report:?}");
    };
    assert_eq!(record.reason(), reason);
    assert_eq!(record.reason().describe(), reason.describe());
    assert_eq!(record.reason().advice(), reason.advice());
    assert!(rendered.contains(reason.describe().as_ref()));
    let mut remaining = rendered.as_str();
    for advice in reason.advice() {
        let at = remaining.find(&advice).expect("ordered intrinsic advice");
        remaining = &remaining[at + advice.len()..];
    }
    assert_eq!(record.reason().failure_code(), reason.failure_code());
    assert_eq!(record.frame(), "ask-host");
    assert_eq!(record.site().script.as_ref(), "engine.ral");
    assert_eq!((record.site().line, record.site().col), (1, 1));
    assert!(rendered.contains("outer: inner"));
    let first = rendered.find("first").expect("first stored hint");
    let second = rendered[first + 5..]
        .find("second")
        .expect("second stored hint")
        + first
        + 5;
    assert!(rendered[second + 6..].contains("first"));
    assert!(!rendered.contains("host.ral"));
}
