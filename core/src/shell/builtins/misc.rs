//! Small builtins with no cluster of their own: `clear`, `reset`, `fail`,
//! `exit`, `surface`, `warn`, and `ask`.

use crate::data::{FOValue, datum::Datum};
use crate::fact::{FailureCode as _, Reason};
use crate::process::{CommandFailure, Signal, SpawnFailure, WorkerStop};
use crate::shell::runtime::{
    Break, CallSite, Error, Escape, Map, Mooring, Settled, Shell, Status, Value, sig,
};
use crate::site::{Silent, site};
use std::num::NonZeroU8;

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

/// Keep zero's advice; the checker also refuses a literal zero.
fn fail_status_code(status: i64) -> Result<NonZeroU8, Break> {
    if status == 0 {
        return Err(sig(
            "fail requires a nonzero status (use `return` for clean exit)",
        ));
    }
    u8::try_from(status)
        .ok()
        .and_then(NonZeroU8::new)
        .ok_or_else(|| {
            sig(format!(
                "fail: status {status} is outside 1..=255: an exit status is one byte"
            ))
        })
}

/// A record field decoded as `T`; `None` when absent.
fn optional<T: Datum>(m: &Map, key: &str) -> Result<Option<T>, Break> {
    m.get(key)
        .map(|v| {
            FOValue::try_from(&*v)
                .map_err(|_| "not plain data".to_string())
                .and_then(|fo| T::decode(&fo))
                .map_err(|why| sig(format!("fail: `{key}`: {why}")))
        })
        .transpose()
}

/// The failure a record's `reason` names, at the record's `status`.
fn projected(reason: Reason, status: NonZeroU8) -> Result<Status, Break> {
    Ok(match reason {
        Reason::Exited(0) => {
            return Err(sig(
                "fail: `exited 0` is success, not an external failure; use `return`, or drop reason",
            ));
        }
        Reason::Exited(n) => Status::Process(CommandFailure::ExitCode(n)),
        Reason::Signaled(n) if (1..=127).contains(&n) => {
            Status::Process(CommandFailure::Signal(Signal::new(n)))
        }
        Reason::Signaled(n) => {
            return Err(sig(format!(
                "fail: `signaled {n}` names no signal; signals are 1 to 127"
            )));
        }
        Reason::NotFound => Status::Process(CommandFailure::Spawn(SpawnFailure::NotFound)),
        Reason::NotRunnable => {
            Status::Process(CommandFailure::Spawn(SpawnFailure::PermissionDenied {
                found: None,
            }))
        }
        Reason::Cancelled => Status::WorkerStopped(WorkerStop::Cancelled),
        Reason::TimedOut => Status::WorkerStopped(WorkerStop::TimedOut),
        Reason::Raised => Status::Raised(status),
    })
}

/// The checker demands `[status: Int, message: String, ...]` of `fail`, so the
/// shape refusals here answer only a record that reached the call through an
/// unchecked boundary: a decoder's result, or a module's asserted shape.
/// A record `try` handed over re-raises faithfully: `reason`, `cmd` and `site`
/// ride along.
fn reraise(args: &[Value]) -> Result<Error, Break> {
    let Some(Value::Map(m)) = args.first() else {
        return Err(sig(
            "fail expects an error record [status: Int, message: String, ...]",
        ));
    };
    let Some(status) = m.get("status").as_deref().and_then(Value::as_int) else {
        return Err(sig(
            "fail: error record missing or non-integer 'status' field",
        ));
    };
    let status = fail_status_code(status)?;
    let message = m.get("message");
    let Some(Value::String(message)) = message.as_deref() else {
        return Err(sig(
            "fail: this error record has no String `message`; the message is the text the failure carries",
        ));
    };
    let message = message.to_string();
    let reason = m
        .get("reason")
        .map(|v| {
            FOValue::try_from(&*v)
                .ok()
                .and_then(|fo| Reason::decode(&fo).ok())
                .ok_or_else(|| {
                    sig(format!(
                        "fail: reason {} is not a reason; see try's error record",
                        *v
                    ))
                })
        })
        .transpose()?;
    let projected = reason.map_or(Ok(Status::Raised(status)), |r| projected(r, status))?;
    if projected.failure_code() != status {
        let (shown, code) = (
            Value::from_datum(reason.expect("only a reason projects")),
            projected.failure_code(),
        );
        return Err(sig(format!(
            "fail: status {status} disagrees with reason {shown}: that failure has status {code}; \
             set status to {code}, or drop reason"
        )));
    }
    let command = optional::<String>(m, "cmd")?;
    let site = optional::<Option<CallSite>>(m, "site")?.flatten();
    Ok(Error {
        command: command.map(Into::into),
        site: site.map(Box::new),
        ..Error::with_status(message, projected)
    })
}

pub(super) fn builtin_fail(args: &[Value]) -> Break {
    reraise(args).map_or_else(|b| b, Break::Error)
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
    use crate::fact::ErrorRecord;
    use crate::process::CancelCause;

    fn nz(n: u8) -> NonZeroU8 {
        NonZeroU8::new(n).unwrap()
    }

    fn site() -> CallSite {
        CallSite {
            script: "a.ral".into(),
            line: 3,
            col: 4,
        }
    }

    fn reraised(record: &ErrorRecord) -> Error {
        match builtin_fail(&[Value::from_datum(record.clone())]) {
            Break::Error(e) => e,
            other @ Break::Escape(_) => panic!("expected an error, got {other:?}"),
        }
    }

    fn refusal(record: Value) -> String {
        match builtin_fail(&[record]) {
            Break::Error(e) => e.message,
            other @ Break::Escape(_) => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn record_value(status: i64, reason: Value) -> Value {
        Value::map(vec![
            ("status".into(), Value::Int(status)),
            ("message".into(), Value::string("m")),
            ("reason".into(), reason),
        ])
    }

    #[test]
    fn exit_takes_an_int_not_numeric_text() {
        let mut shell = crate::test_helper::core_shell();
        match builtin_exit(&[Value::string("3")], &mut shell) {
            Err(Break::Error(e)) => assert_eq!(e.message, "exit: expected Int, got String"),
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
                error.message,
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
    fn fail_refuses_a_status_outside_one_byte() {
        let record = |status| {
            Value::map(vec![
                ("status".into(), Value::Int(status)),
                ("message".into(), Value::string("x")),
            ])
        };
        for status in [-1, 256] {
            assert_eq!(
                refusal(record(status)),
                format!("fail: status {status} is outside 1..=255: an exit status is one byte")
            );
        }
        assert_eq!(
            refusal(record(0)),
            "fail requires a nonzero status (use `return` for clean exit)"
        );
        assert!(
            matches!(builtin_fail(&[record(255)]), Break::Error(error) if error.failure_code().get() == 255)
        );
    }

    #[test]
    fn fail_of_a_record_is_a_faithful_reraise() {
        let statuses = [
            Status::Raised(nz(9)),
            Status::Process(CommandFailure::ExitCode(3)),
            Status::Process(CommandFailure::Signal(Signal::new(9))),
            Status::Process(CommandFailure::Spawn(SpawnFailure::NotFound)),
            Status::Process(CommandFailure::Spawn(SpawnFailure::PermissionDenied {
                found: None,
            })),
            Status::WorkerStopped(WorkerStop::Cancelled),
            Status::WorkerStopped(WorkerStop::TimedOut),
        ];
        for status in statuses {
            for site in [None, Some(site())] {
                let record = ErrorRecord::new("cmd", &status, "boom", site);
                assert_eq!(reraised(&record).record_at(None), record);
            }
        }
        assert_eq!(
            Status::WorkerStopped(WorkerStop::Cancelled).failure_code(),
            CancelCause::Cancelled.failure_code()
        );
        assert_eq!(
            Status::WorkerStopped(WorkerStop::TimedOut).failure_code(),
            CancelCause::TimedOut.failure_code()
        );
    }

    #[test]
    fn fail_refuses_a_status_that_disagrees_with_its_reason() {
        let msg = refusal(record_value(
            5,
            Value::variant("exited", Some(Value::Int(3))),
        ));
        assert!(
            msg.contains("disagrees with reason") && msg.contains("set status to 3"),
            "{msg}"
        );
    }

    #[test]
    fn fail_refuses_a_zero_external_exit_reason() {
        for status in [1, 255] {
            let Break::Error(error) =
                builtin_fail(&[record_value(status, Value::from_datum(Reason::Exited(0)))])
            else {
                panic!("a zero external exit reason must be refused");
            };
            assert_eq!(
                error.message,
                "fail: `exited 0` is success, not an external failure; use `return`, or drop reason"
            );
            assert_eq!(error.failure_code(), NonZeroU8::MIN);
            assert_eq!(error.record_at(None).reason, Reason::Raised);
        }
    }

    #[test]
    fn fail_preserves_negative_external_exit_reasons() {
        for raw in [-1, i32::MIN, -1_073_741_790] {
            let reason = Reason::Exited(raw);
            let Break::Error(error) = builtin_fail(&[record_value(1, Value::from_datum(reason))])
            else {
                panic!("a negative Windows exit is a failure");
            };
            assert_eq!(error.failure_code(), NonZeroU8::MIN);
            assert_eq!(error.record_at(None).reason, reason);
            let mismatch = refusal(record_value(2, Value::from_datum(reason)));
            assert!(mismatch.contains("disagrees with reason"), "{mismatch}");
            assert!(mismatch.contains("set status to 1"), "{mismatch}");
        }
    }

    #[test]
    fn fail_refuses_a_signal_out_of_range() {
        let msg = refusal(record_value(
            1,
            Value::variant("signaled", Some(Value::Int(200))),
        ));
        assert!(msg.contains("names no signal"), "{msg}");
    }

    #[test]
    fn fail_keeps_a_records_site() {
        let record = ErrorRecord::new("cmd", &Status::Raised(nz(2)), "boom", Some(site()));
        assert_eq!(reraised(&record).site.as_deref(), Some(&site()));
    }

    #[test]
    fn fail_keeps_runtime_as_cmd() {
        let record = ErrorRecord::new("<runtime>", &Status::Raised(nz(2)), "boom", None);
        let error = reraised(&record);
        assert_eq!(error.command.as_deref(), Some("<runtime>"));
        assert_eq!(error.record_at(None).cmd(), "<runtime>");
    }
}
