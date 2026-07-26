//! The cluster-C small builtins: terminal control (`clear`, `reset`),
//! failure and exit (`fail`, `exit`), the structured-event `surface`, and
//! the interactive `ask` prompt.

use crate::types::{Break, Error, Escape, Mooring, Settled, Shell, Value, sig};

const CLEAR_SEQ: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";
/// `ESC c` (RIS, Reset to Initial State): resets the terminal but leaves
/// stty modes untouched; `^reset` reaches the full ncurses terminfo reset.
const RESET_SEQ: &[u8] = b"\x1bc";

pub(super) fn builtin_clear(_args: &[Value], shell: &mut Shell) -> Value {
    let _ = shell.write_stdout(CLEAR_SEQ);
    shell.mobile.control.last_status = 0;
    Value::Unit
}

pub(super) fn builtin_reset(_args: &[Value], shell: &mut Shell) -> Value {
    let _ = shell.write_stdout(RESET_SEQ);
    shell.mobile.control.last_status = 0;
    Value::Unit
}

/// Narrow an i64 status to the `i32` an exit code must fit, erroring with
/// `who` named when it doesn't.  The zero-status guard reads the i64
/// directly, so a value that truncated to 0 under `as i32` no longer slips
/// the guard as a "success" failure.
fn status_i32(who: &str, n: i64) -> Result<i32, Break> {
    i32::try_from(n).map_err(|_| sig(format!("{who}: status {n} is outside the exit-code range")))
}

/// Turn a `fail` status into an exit code, or the one rule every `fail` path
/// must honour: the status must be nonzero.  Shared by the bare-int shorthand
/// and the error-record path so a zero status is always named as "wrong
/// rule", never mistaken for a shape complaint.
fn fail_status_code(status: i64) -> Result<i32, Break> {
    if status == 0 {
        return Err(Break::Error(Error::new(
            "fail requires a nonzero status (use `return` for clean exit)",
            1,
        )));
    }
    status_i32("fail", status)
}

pub(super) fn builtin_fail(args: &[Value]) -> Break {
    let m = match args.first() {
        Some(Value::Map(m)) => m,
        // `fail "msg"` / `fail $bytes` — a bare message shorthand for
        // `fail [status: 1, message: "msg"]`.  The checker's `fail` arg
        // is row-polymorphic and does not reject a scalar here, so the
        // runtime honours the scalar rather than erroring on it: a
        // failing pipeline producer (`{ fail "boom" }`) then raises with
        // the author's text instead of a shape complaint that hides it.
        Some(Value::String(s)) => return Break::Error(Error::new(s.clone(), 1)),
        Some(Value::Bytes(b)) => {
            return Break::Error(Error::new(String::from_utf8_lossy(b).into_owned(), 1));
        }
        // `fail $n` — a bare status with no message.
        Some(Value::Int(n)) => {
            return match fail_status_code(*n) {
                Ok(code) => Break::Error(Error::new("explicit failure", code)),
                Err(b) => b,
            };
        }
        _ => {
            return Break::Error(Error::new(
                "fail expects an error record [status: Int, ...]",
                1,
            ));
        }
    };
    let lookup = |k: &str| m.get(k);
    let Some(status) = lookup("status").and_then(Value::as_int) else {
        return Break::Error(Error::new(
            "fail: error record missing or non-integer 'status' field",
            1,
        ));
    };
    let code = match fail_status_code(status) {
        Ok(code) => code,
        Err(b) => return b,
    };
    let message = match lookup("message") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => "explicit failure".to_string(),
    };
    Break::Error(Error::new(message, code))
}

pub(super) fn builtin_exit(args: &[Value], _env: &mut Shell) -> Settled<Value> {
    if args.len() > 1 {
        return Err(sig("exit accepts at most 1 argument"));
    }
    let code = match args.first() {
        None => 0,
        Some(Value::Int(n)) => status_i32("exit", *n)?,
        Some(v) => v
            .to_string()
            .parse::<i32>()
            .map_err(|_| sig("exit: status must be an integer"))?,
    };
    Err(Break::Escape(Escape::Exit(code)))
}

/// `surface <event>` — hand the event value to the host's structured-event
/// sink, if one is installed.  The host decides what the variant's tag means;
/// with no sink (e.g. a bare REPL) this is the identity and returns Unit.
#[allow(
    clippy::unnecessary_wraps,
    reason = "builtin dispatched through the fn-pointer table in builtins.rs; the uniform `Settled<Value>` return is fixed by the registry, not by this body."
)]
pub(super) fn builtin_surface(args: &[Value], mooring: &Mooring, _shell: &Shell) -> Settled<Value> {
    if let Some(event) = args.first() {
        mooring.surface(event);
    }
    Ok(Value::Unit)
}

// Print prompt to the console and read one line from the console.
// Bypasses stdin/stdout redirection so it always talks to the user.
// Errors on EOF (Ctrl+D / Ctrl+Z).
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:ask-tty] `ask` builtin opens the controlling terminal device (/dev/tty or CON) to prompt and read one line direct from the user, bypassing redirection; a terminal-device interaction, not turn-time model data I/O."
)]
pub(super) fn builtin_ask(args: &[Value]) -> Result<Value, Error> {
    let prompt = args
        .first()
        .ok_or_else(|| Error::new("ask requires a prompt string", 1))?;
    #[cfg(unix)]
    const CON_OUT: &str = "/dev/tty";
    #[cfg(unix)]
    const CON_IN: &str = "/dev/tty";
    #[cfg(not(unix))]
    const CON_OUT: &str = "CONOUT$";
    #[cfg(not(unix))]
    const CON_IN: &str = "CONIN$";

    use std::io::{BufRead, Write};
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .open(CON_OUT)
        .map_err(|e| Error::new(format!("ask: {e}"), 1))?;
    write!(out, "{prompt}").ok();
    out.flush().ok();
    drop(out);
    let inp = std::fs::File::open(CON_IN).map_err(|e| Error::new(format!("ask: {e}"), 1))?;
    let mut line = String::new();
    let n = std::io::BufReader::new(inp)
        .read_line(&mut line)
        .map_err(|e| Error::new(format!("ask: {e}"), 1))?;
    if n == 0 {
        return Err(Error::new("ask: EOF", 1));
    }
    let len = crate::io::str_strip_one_terminator(&line).len();
    line.truncate(len);
    Ok(Value::String(line))
}
