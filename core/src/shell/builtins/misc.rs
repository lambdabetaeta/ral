//! Small builtins with no cluster of their own: `clear`, `reset`, `fail`,
//! `exit`, `surface`, `warn`, and `ask`.

use crate::data::{FOValue, datum::Datum};
use crate::fact::{ErrorRecord, Failure};
use crate::shell::runtime::{Break, Error, Escape, Mooring, Settled, Shell, Value, sig};
use crate::site::{Silent, site};

const CLEAR_SEQ: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";
/// `ESC c` (RIS): a terminal reset that leaves stty modes untouched.
const RESET_SEQ: &[u8] = b"\x1bc";

pub(super) fn builtin_clear(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    shell.write_stdout(CLEAR_SEQ)?;
    Ok(Value::Unit)
}

pub(super) fn builtin_reset(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    shell.write_stdout(RESET_SEQ)?;
    Ok(Value::Unit)
}

fn exit_status(n: i64) -> Result<u8, Break> {
    u8::try_from(n).map_err(|_| {
        sig(format!(
            "exit: status {n} is outside 0..=255: an exit status is one byte"
        ))
    })
}

fn error_record(args: &[Value], cmd: &str) -> Result<ErrorRecord, Break> {
    let value = args
        .first()
        .ok_or_else(|| sig(format!("{cmd}: expected an error record")))?;
    let data = FOValue::try_from(value).map_err(|_| sig(format!("{cmd}: expected plain data")))?;
    ErrorRecord::decode(&data).map_err(|why| sig(format!("{cmd}: {why}")))
}

fn reraise(args: &[Value]) -> Result<Error, Break> {
    let record = error_record(args, "raise")?;
    match record.reason().failure() {
        Some(Failure::Exited(0)) => {
            return Err(sig(
                "raise: `exited 0` is success, not a failure; use `return`",
            ));
        }
        Some(Failure::Signaled(n)) if !(1..=127).contains(n) => {
            return Err(sig(format!(
                "raise: `signaled {n}` names no signal; signals are 1 to 127"
            )));
        }
        _ => {}
    }
    Ok(Error::from_record(record))
}

pub(super) fn builtin_fail(args: &[Value]) -> Break {
    match args.first().and_then(|v| v.as_str("fail").ok()) {
        Some(message) => Error::raised(message).into(),
        None => sig("fail: expected a String; use `raise $e` to re-raise an error record"),
    }
}

pub(super) fn builtin_raise(args: &[Value]) -> Break {
    reraise(args).map_or_else(|b| b, Break::Error)
}

pub(super) fn builtin_describe(args: &[Value]) -> Settled<Value> {
    Ok(Value::string(
        error_record(args, "describe")?.describe().into_owned(),
    ))
}

pub(super) fn builtin_exit(args: &[Value], _env: &mut Shell) -> Settled<Value> {
    let Value::Int(n) = &args[0] else {
        return Err(sig(format!(
            "exit: expected Int, got {}",
            args[0].type_name()
        )));
    };
    Err(Break::Escape(Escape::Exit(exit_status(*n)?)))
}

/// Hand the event to the host's structured-event sink, or to nothing when none
/// is installed.  [`Mooring::surface`] drops values that are not first-order.
///
/// `pub`: a host declares its own [`BuiltinEntry`](crate::shell::runtime::BuiltinEntry)
/// over this body under its own name; see [`crate::shell::builtins::SURFACE_BUILTIN`].
///
/// # Errors
/// Never returns `Err`; `Settled` only because the registry's fn-pointer
/// table fixes the return type.
#[allow(
    clippy::unnecessary_wraps,
    reason = "builtin dispatched through the fn-pointer table in builtins.rs; the uniform `Settled<Value>` return is fixed by the registry, not by this body."
)]
pub fn builtin_surface(args: &[Value], mooring: &Mooring, _shell: &mut Shell) -> Settled<Value> {
    if let Some(event) = args.first() {
        mooring.surface(event);
    }
    Ok(Value::Unit)
}

/// `surface`'s plain-text sibling: one diagnostic line to the stderr sink,
/// which `2> f` rebinds and a capture keeps apart from the byte channel.
///
/// A builtin rather than fd plumbing — ral has no `1>&2` for a diagnostic to
/// borrow the byte channel through, and a diagnostic never was a payload.
pub(super) fn builtin_warn(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let mut line = args[0].as_str("warn")?.to_owned();
    line.push('\n');
    shell.write_stderr(line.as_bytes())?;
    Ok(Value::Unit)
}

// Opens the controlling terminal rather than using the shell's stdio, so the
// prompt still reaches the user under redirection.
pub(super) fn builtin_ask(args: &[Value]) -> Result<Value, Error> {
    let prompt = args
        .first()
        .ok_or_else(|| Error::new("ask requires a prompt string"))?;
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
    .map_err(|e| Error::new(format!("ask: {e}")))?;
    write!(out, "{prompt}").ok();
    out.flush().ok();
    drop(out);
    let inp = site!(Silent::AskTty, std::fs::File::open(CON_IN))
        .map_err(|e| Error::new(format!("ask: {e}")))?;
    let mut line = String::new();
    let n = std::io::BufReader::new(inp)
        .read_line(&mut line)
        .map_err(|e| Error::new(format!("ask: {e}")))?;
    if n == 0 {
        return Err(Error::new("ask: EOF"));
    }
    line.truncate(crate::io::without_terminator(line.as_bytes()).len());
    Ok(Value::string(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{FailureCode as _, NotRunnable, Reason, Unrunnable};
    use crate::shell::runtime::ExitHints;
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

    fn reraised(record: &ErrorRecord) -> Error {
        match builtin_raise(&[Value::from_datum(record.clone())]) {
            Break::Error(e) => e,
            other @ Break::Escape(_) => panic!("expected an error, got {other:?}"),
        }
    }

    fn refusal(record: Value) -> String {
        match builtin_raise(&[record]) {
            Break::Error(e) => e.describe().into_owned(),
            other @ Break::Escape(_) => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn record_value(reason: Reason) -> Value {
        Value::from_datum(ErrorRecord::new(reason, "tool".into(), site()))
    }

    #[test]
    fn exit_takes_an_int_not_numeric_text() {
        let mut shell = crate::test_helper::core_shell();
        match builtin_exit(&[Value::string("3")], &mut shell) {
            Err(Break::Error(e)) => assert_eq!(e.describe(), "exit: expected Int, got String"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn exit_refuses_a_status_outside_one_byte() {
        let mut shell = crate::test_helper::core_shell();
        for status in [-1, 256] {
            let Err(Break::Error(error)) = builtin_exit(&[Value::Int(status)], &mut shell) else {
                panic!("exit {status} must refuse");
            };
            assert_eq!(
                error.describe(),
                format!("exit: status {status} is outside 0..=255: an exit status is one byte")
            );
        }
        for status in [0, 255] {
            assert!(matches!(
                builtin_exit(&[Value::Int(i64::from(status))], &mut shell),
                Err(Break::Escape(Escape::Exit(code))) if code == status
            ));
        }
    }

    #[test]
    fn fail_takes_text_and_refuses_records() {
        let Break::Error(error) = builtin_fail(&[Value::string("boom")]) else {
            panic!()
        };
        assert_eq!(error.reason, Reason::Raised("boom".into()));
        let Break::Error(error) = builtin_fail(&[record_value(tool(Failure::Exited(3)))]) else {
            panic!()
        };
        assert!(error.describe().contains("use `raise $e`"));
    }

    #[test]
    fn raise_of_a_record_is_a_faithful_reraise() {
        let shell = crate::test_helper::core_shell();
        for reason in [
            Reason::Raised("boom".into()),
            tool(Failure::Exited(3)),
            tool(Failure::Signaled(9)),
            tool(Failure::NotFound),
            tool(Failure::NotRunnable(NotRunnable {
                found: "tool".into(),
                why: Unrunnable::PermissionDenied,
            })),
            Reason::Cancelled,
            Reason::TimedOut,
        ] {
            let record = ErrorRecord::new(reason, "tool".into(), site());
            assert_eq!(reraised(&record).record(&shell), record);
        }
    }

    #[test]
    fn raise_trusts_a_hand_built_records_provenance() {
        let shell = crate::test_helper::core_shell();
        let record = ErrorRecord::new(tool(Failure::Exited(3)), "claimed".into(), site());
        assert_eq!(reraised(&record).record(&shell), record);
        assert_eq!(reraised(&record).failure_code().get(), 3);
    }

    #[test]
    fn raise_refuses_a_zero_external_exit_reason() {
        assert_eq!(
            refusal(record_value(tool(Failure::Exited(0)))),
            "raise: `exited 0` is success, not a failure; use `return`"
        );
    }

    #[test]
    fn raise_preserves_negative_external_exit_reasons() {
        for raw in [-1, i32::MIN, -1_073_741_790] {
            let record = ErrorRecord::new(tool(Failure::Exited(raw)), "tool".into(), site());
            let error = reraised(&record);
            assert_eq!(error.failure_code(), NonZeroU8::MIN);
            assert_eq!(error.reason, tool(Failure::Exited(raw)));
        }
    }

    #[test]
    fn raise_refuses_a_signal_out_of_range() {
        let msg = refusal(record_value(tool(Failure::Signaled(200))));
        assert!(msg.contains("names no signal"), "{msg}");
    }

    #[test]
    fn reraised_exited_record_uses_the_session_table_and_situational_hint_wins() {
        let mut shell = crate::test_helper::core_shell();
        shell.set_exit_hints(ExitHints::from_text("tool 3 check the input"));
        let record = ErrorRecord::new(tool(Failure::Exited(3)), "wrapper".into(), site());
        let error = reraised(&record);
        assert_eq!(error.hints(), []);
        let rendered = error.render(shell.sources(), None, shell.exit_hints());
        assert!(rendered.contains("check the input"), "{rendered}");
        let decorated = error
            .with_hint("check this invocation")
            .with_context("outer")
            .discarded(12);
        let rendered = decorated.render(shell.sources(), None, shell.exit_hints());
        assert!(rendered.contains("check this invocation"), "{rendered}");
        assert!(!rendered.contains("check the input"), "{rendered}");
        let raised = reraised(&decorated.record(&shell));
        assert!(raised.hints().is_empty() && raised.context.is_none());
        assert_eq!(raised.unwound, crate::shell::runtime::Unwound::default());
        assert!(
            raised
                .render(shell.sources(), None, shell.exit_hints())
                .contains("check the input")
        );
    }

    #[cfg(unix)]
    #[test]
    fn reraised_records_recompute_reason_hints_without_a_caret() {
        let record = ErrorRecord::new(tool(Failure::Signaled(9)), "wrapper".into(), site());
        let rendered = reraised(&record).compact(&ExitHints::default());
        assert!(rendered.contains("SIGKILL"));
        assert!(rendered.contains("a.ral:3:4"));
        assert!(!rendered.contains("here"));
    }
}
