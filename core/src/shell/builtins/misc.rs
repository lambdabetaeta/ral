//! Small builtins with no cluster of their own: `clear`, `reset`, `fail`,
//! `exit`, `surface`, `warn`, and `ask`.

use crate::data::{
    FOValue,
    datum::{self, Datum},
};
use crate::error::{Error, Prov};
use crate::fact::{
    ArityFailure, CountRule, Expected, InvalidCause, IoOperation, IoSubject, Number, NumberRange,
    Reason, StateFailure, ValueShape,
};
use crate::fact::{ErrorFact, ErrorRecord};
use crate::ir::{ValClos, View};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Break, Escape, Mooring, Settled, Shell};
use crate::site::{Silent, site};

const CLEAR_SEQ: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";
/// `ESC c` (RIS): a terminal reset that leaves stty modes untouched.
const RESET_SEQ: &[u8] = b"\x1bc";

pub(super) fn builtin_clear(_args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    shell.write_stdout(CLEAR_SEQ)?;
    Ok(ValClos::UNIT)
}

pub(super) fn builtin_reset(_args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    shell.write_stdout(RESET_SEQ)?;
    Ok(ValClos::UNIT)
}

fn exit_status(n: i64) -> Result<u8, Error> {
    u8::try_from(n).map_err(|_| {
        invalid(
            "exit",
            InvalidCause::Range(NumberRange {
                value: Number::Int(n),
                minimum: Number::Int(0),
                maximum: Number::Int(255),
            }),
        )
    })
}

fn raise(args: &[ValClos]) -> Result<Break, Error> {
    let value = args.first().ok_or_else(|| {
        invalid(
            "raise",
            InvalidCause::Arity(ArityFailure {
                expected: CountRule::Exactly(1usize.into()),
                got: args.len().into(),
            }),
        )
    })?;
    let ErrorFact {
        frame,
        reason,
        site,
    } = claimed_fact(value)?;
    Ok(Break::from(Error::new(reason)).claimed_by(|| Prov::Resolved(Box::new((frame, site)))))
}

/// A record is plain data: whatever its origin, raising it is the program's
/// own claim.
fn claimed_fact(value: &ValClos) -> Result<ErrorFact, Error> {
    let data = FOValue::try_from(value)
        .map_err(|error| invalid("raise", InvalidCause::NonData(error.into())))?;
    if matches!(datum::field::<i64>(&data, "status"), Ok(0)) {
        return Err(invalid("raise", InvalidCause::ZeroStatus));
    }
    ErrorRecord::decode(&data)
        .map(ErrorRecord::claim)
        .map_err(|why| invalid("raise", InvalidCause::Decode(why)))
}

pub(super) fn builtin_fail(args: &[ValClos]) -> Break {
    match args.first() {
        Some(value) => match value.view() {
            View::String(message) => Error::new(Reason::raised(message.to_string())).into(),
            _ => value
                .expected("fail", Expected::Shape(ValueShape::String))
                .into(),
        },
        None => invalid(
            "fail",
            InvalidCause::Arity(ArityFailure {
                expected: CountRule::Exactly(1usize.into()),
                got: 0usize.into(),
            }),
        )
        .into(),
    }
}

pub(super) fn builtin_raise(args: &[ValClos]) -> Break {
    raise(args).unwrap_or_else(Break::from)
}

pub(super) fn builtin_exit(args: &[ValClos], _env: &mut Shell) -> Settled<ValClos> {
    let View::Int(n) = args[0].view() else {
        return Err(args[0]
            .expected("exit", Expected::Shape(ValueShape::Int))
            .into());
    };
    Err(Break::Escape(Escape::Exit(exit_status(n)?)))
}

/// Hand the event to the host's structured-event sink, or to nothing when none
/// is installed.  [`Mooring::surface`] drops values that are not first-order.
///
/// `pub`: a host declares its own [`BuiltinEntry`](crate::builtin::BuiltinEntry)
/// over this body under its own name; see [`crate::shell::builtins::SURFACE_BUILTIN`].
///
/// # Errors
/// Never returns `Err`; `Settled` only because the registry's fn-pointer
/// table fixes the return type.
#[allow(
    clippy::unnecessary_wraps,
    reason = "builtin dispatched through the fn-pointer table in builtins.rs; the uniform `Settled<ValClos>` return is fixed by the registry, not by this body."
)]
pub fn builtin_surface(
    args: &[ValClos],
    mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<ValClos> {
    if let Some(event) = args.first() {
        mooring.surface(event);
    }
    Ok(ValClos::UNIT)
}

/// `surface`'s plain-text sibling: one diagnostic line to the stderr sink,
/// which `2> f` rebinds and a capture keeps apart from the byte channel.
///
/// A builtin rather than fd plumbing — ral has no `1>&2` for a diagnostic to
/// borrow the byte channel through, and a diagnostic never was a payload.
pub(super) fn builtin_warn(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    let mut line = args[0].as_str("warn")?.to_owned();
    line.push('\n');
    shell.write_stderr(line.as_bytes())?;
    Ok(ValClos::UNIT)
}

// Opens the controlling terminal rather than using the shell's stdio, so the
// prompt still reaches the user under redirection.
pub(super) fn builtin_ask(args: &[ValClos]) -> Result<ValClos, Error> {
    let prompt = args.first().ok_or_else(|| {
        invalid(
            "ask",
            InvalidCause::Arity(ArityFailure {
                expected: CountRule::Exactly(1usize.into()),
                got: args.len().into(),
            }),
        )
    })?;
    #[cfg(unix)]
    const CON_OUT: &str = "/dev/tty";
    #[cfg(unix)]
    const CON_IN: &str = "/dev/tty";
    #[cfg(not(unix))]
    const CON_OUT: &str = "CONOUT$";
    #[cfg(not(unix))]
    const CON_IN: &str = "CONIN$";

    use std::io::{BufRead, Write};
    let mut out = site!(
        Silent::AskTty,
        std::fs::OpenOptions::new().write(true) => open(CON_OUT)
    )
    .map_err(|e| Error::io(IoOperation::Open, IoSubject::Terminal, &e).with_context("ask"))?;
    write!(out, "{prompt}").ok();
    out.flush().ok();
    drop(out);
    let inp = site!(Silent::AskTty, std::fs::File::open(CON_IN))
        .map_err(|e| Error::io(IoOperation::Open, IoSubject::Terminal, &e).with_context("ask"))?;
    let mut line = String::new();
    let n = std::io::BufReader::new(inp)
        .read_line(&mut line)
        .map_err(|e| Error::io(IoOperation::Read, IoSubject::Terminal, &e).with_context("ask"))?;
    if n == 0 {
        return Err(Error::new(Reason::State(Box::new(
            StateFailure::TerminalEof,
        ))));
    }
    line.truncate(crate::io::without_terminator(line.as_bytes()).len());
    Ok(ValClos::string(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{Claim, Failure, FailureCode as _, NotRunnable, Reason, Unrunnable};
    use crate::source::CallSite;
    use std::num::NonZeroU8;

    fn site() -> CallSite {
        CallSite {
            script: "a.ral".into(),
            line: 3,
            col: 4,
        }
    }

    fn tool(failure: Failure) -> Reason {
        Reason::process("tool", failure)
    }

    fn caught(reason: Reason, frame: &str) -> ErrorFact {
        ErrorFact::new(reason, frame.into(), site())
    }

    fn hand_built(message: &str, status: u8, frame: &str) -> ErrorRecord {
        ErrorRecord {
            frame: frame.into(),
            message: message.into(),
            status: NonZeroU8::new(status).unwrap(),
            site: site(),
        }
    }

    fn claim(message: &str, status: u8) -> Reason {
        Reason::Raised(Box::new(Claim {
            message: message.into(),
            status: NonZeroU8::new(status).unwrap(),
        }))
    }

    fn with_status(record: &ValClos, status: i64) -> ValClos {
        let View::Map(map) = record.view() else {
            panic!("a record is a map")
        };
        ValClos::map(map.iter().map(|(key, value)| {
            let value = if key == "status" {
                ValClos::int(status)
            } else {
                value.into_owned()
            };
            (key.to_string(), value)
        }))
    }

    fn raise_record(record: ValClos) -> Error {
        match builtin_raise(&[record]) {
            Break::Error(e) => e,
            other @ Break::Escape(_) => panic!("expected an error, got {other:?}"),
        }
    }

    fn refusal(args: &[ValClos]) -> InvalidCause {
        match builtin_raise(args) {
            Break::Error(Error {
                reason: Reason::Invalid(failure),
                ..
            }) => failure.cause,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn exit_takes_an_int_not_numeric_text() {
        let mut shell = crate::test_helper::core_shell();
        match builtin_exit(&[ValClos::string("3")], &mut shell) {
            Err(Break::Error(e)) => assert!(
                matches!(e.reason, Reason::Invalid(failure) if matches!(failure.cause, InvalidCause::Shape(_)))
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn exit_refuses_a_status_outside_one_byte() {
        let mut shell = crate::test_helper::core_shell();
        for status in [-1, 256] {
            let Err(Break::Error(error)) = builtin_exit(&[ValClos::int(status)], &mut shell) else {
                panic!("exit {status} must refuse");
            };
            assert!(
                matches!(error.reason, Reason::Invalid(failure) if failure.cause == InvalidCause::Range(NumberRange {
                    value: Number::Int(status), minimum: Number::Int(0), maximum: Number::Int(255),
                }))
            );
        }
        for status in [0, 255] {
            assert!(matches!(
                builtin_exit(&[ValClos::int(i64::from(status))], &mut shell),
                Err(Break::Escape(Escape::Exit(code))) if code == status
            ));
        }
    }

    #[test]
    fn fail_takes_text_and_refuses_records() {
        let Break::Error(error) = builtin_fail(&[ValClos::string("boom")]) else {
            panic!()
        };
        assert_eq!(error.reason, Reason::raised("boom"));
        let Break::Error(error) = builtin_fail(&[ValClos::from_datum(
            caught(tool(Failure::Exited(3)), "tool").record(),
        )]) else {
            panic!()
        };
        assert!(
            matches!(error.reason, Reason::Invalid(failure) if matches!(failure.cause, InvalidCause::Shape(_)))
        );
    }

    #[test]
    fn raise_of_any_failure_record_keeps_its_frame_message_status_and_site() {
        use crate::fact::{
            BootFailure, CompileFailure, CompilePhase, Denial, HostFailure, LaunchFailure,
            ProtocolFailure, RalFailure, SingleOrNested, Withholding,
        };
        let shell = crate::test_helper::core_shell();
        for reason in [
            Reason::raised("boom"),
            tool(Failure::Exited(3)),
            tool(Failure::Signaled(9)),
            tool(Failure::NotFound),
            tool(Failure::NotRunnable(NotRunnable {
                found: "tool".into(),
                why: Unrunnable::PermissionDenied,
            })),
            Reason::Invalid(Box::new(crate::fact::InvalidFailure::new(
                "int".into(),
                InvalidCause::Number(crate::fact::NumberFailure {
                    text: "x".into(),
                    target: crate::fact::IntOrFloat::Int,
                    why: crate::fact::NumberFault::Syntax,
                }),
            ))),
            Reason::Evaluation(Box::new(crate::fact::EvalFailure::MissingBinding(
                "x".into(),
            ))),
            Reason::State(Box::new(StateFailure::NoDesk)),
            Reason::Denied(Box::new(Denial::exec_run(
                "tool".into(),
                Withholding::MissingAllowance(SingleOrNested::Nested),
            ))),
            Error::io(
                IoOperation::Read,
                IoSubject::Path(crate::text::quote_native(std::ffi::OsStr::new("input"))),
                &std::io::ErrorKind::PermissionDenied.into(),
            )
            .reason,
            Reason::Compile(Box::new(CompileFailure {
                source: "module.ral".into(),
                phase: CompilePhase::Parse,
            })),
            Reason::Launch(Box::new(LaunchFailure::BatchImage("build.cmd".into()))),
            Reason::Protocol(Box::new(ProtocolFailure::RootMissingScratch)),
            Reason::Host(Box::new(HostFailure::Ral(RalFailure::Boot(
                BootFailure::AlreadyBooted,
            )))),
            Reason::Invariant(Box::new(
                crate::fact::InvariantFailure::AppliedUnforcedValue(ValueShape::Int),
            )),
            Reason::Cancelled,
            Reason::TimedOut,
        ] {
            let record = caught(reason, "tool").record();
            let error = raise_record(ValClos::from_datum(record.clone()));
            assert_eq!(error.fact(&shell), record.claim());
        }
    }

    #[test]
    fn raise_of_a_hand_built_record_is_its_claim() {
        let shell = crate::test_helper::core_shell();
        let record = hand_built("nope", 3, "claimed");
        let error = raise_record(ValClos::from_datum(record.clone()));
        assert_eq!(error.reason, claim("nope", 3));
        assert_eq!(error.failure_code().get(), 3);
        assert_eq!(error.fact(&shell), record.claim());
    }

    #[test]
    fn a_caught_record_is_raised_as_a_claim_of_its_message() {
        let shell = crate::test_helper::core_shell();
        let fact = caught(tool(Failure::Exited(3)), "wrapper");
        let error = raise_record(ValClos::from_datum(fact.record()));
        assert_eq!(error.reason, claim(&fact.describe(), 3));
        assert_eq!(error.fact(&shell).frame(), "wrapper");
    }

    #[test]
    fn a_modified_record_is_raised_with_its_new_status() {
        let fact = caught(tool(Failure::Exited(3)), "wrapper");
        let error = raise_record(with_status(&ValClos::from_datum(fact.record()), 5));
        assert_eq!(error.reason, claim(&fact.describe(), 5));
    }

    #[test]
    fn raise_refuses_status_zero_with_advice_to_return() {
        let record = ValClos::from_datum(hand_built("nope", 1, "tool"));
        assert_eq!(
            refusal(&[with_status(&record, 0)]),
            InvalidCause::ZeroStatus
        );
    }

    #[test]
    fn raise_refuses_a_status_above_one_byte() {
        let record = ValClos::from_datum(hand_built("nope", 1, "tool"));
        assert!(matches!(
            refusal(&[with_status(&record, 256)]),
            InvalidCause::Decode(_)
        ));
    }

    #[test]
    fn raise_of_a_negative_external_exit_keeps_the_status_its_record_reports() {
        for raw in [-1, i32::MIN, -1_073_741_790] {
            let record = caught(tool(Failure::Exited(raw)), "tool").record();
            let error = raise_record(ValClos::from_datum(record.clone()));
            assert_eq!(error.failure_code(), record.status);
        }
    }

    #[test]
    fn raise_decode_and_nondata_failures_do_not_become_raised_text() {
        assert!(matches!(refusal(&[]), InvalidCause::Arity(_)));
        let block = crate::ir::block_over(&crate::ir::Env::new());
        assert!(matches!(
            refusal(&[ValClos::list(vec![block])]),
            InvalidCause::NonData(crate::fact::NonData { nested: true, .. })
        ));
        assert!(matches!(refusal(&[ValClos::UNIT]), InvalidCause::Decode(_)));
        assert!(matches!(
            builtin_fail(&[]),
            Break::Error(Error {
                reason: Reason::Invalid(_),
                ..
            })
        ));
        assert!(matches!(
            builtin_ask(&[]).unwrap_err().reason,
            Reason::Invalid(_)
        ));
    }
    #[test]
    fn raised_admission_record_carries_no_report_only_witness() {
        use crate::fact::{
            AdmissionCause, AdmissionFailure, AdmissionUses, Expected, Found, InvalidFailure,
            ValueShape,
        };
        use crate::source::{Source, SourceDb, SourceFile, SourceOrigin, Span};
        let shell = crate::test_helper::core_shell();
        let mut db = SourceDb::default();
        let file = db.register(Source::from_text(
            SourceOrigin::File(SourceFile::new("admission.ral").unwrap()),
            "bind\nuse\n",
        ));
        let reason = Reason::Invalid(Box::new(InvalidFailure::new(
            "ask".into(),
            InvalidCause::Admission(AdmissionFailure {
                pointer: String::new(),
                cause: AdmissionCause::Uses(AdmissionUses {
                    found: Found::Map,
                    expected: Expected::Shape(ValueShape::Variant),
                }),
            }),
        )));
        let error = Error::new(reason).with_witness(Some(Span::new(file, 5, 8)));
        assert!(
            error
                .render(&db, Some(file), shell.exit_hints())
                .contains(", use at \"admission.ral\":2:1")
        );
        let record = error.fact(&shell).record();
        let raised = raise_record(ValClos::from_datum(record.clone()));
        assert_eq!(raised.fact(&shell), record.claim());
        assert!(raised.witness().is_none());
        assert_eq!(raised.hints(), []);
        assert!(
            !raised
                .render(&db, Some(file), shell.exit_hints())
                .contains(", use at ")
        );
    }
}
