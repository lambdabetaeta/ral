//! Small builtins with no cluster of their own: `clear`, `reset`, `fail`,
//! `exit`, `surface`, and `ask`.

use crate::types::{Break, Error, Escape, Mooring, Settled, Shell, Value, sig};

const CLEAR_SEQ: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";
/// `ESC c` (RIS): a terminal reset that leaves stty modes untouched.
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

fn status_i32(who: &str, n: i64) -> Result<i32, Break> {
    i32::try_from(n).map_err(|_| sig(format!("{who}: status {n} is outside the exit-code range")))
}

/// The nonzero check reads the i64 before narrowing, so a status that would
/// truncate to 0 cannot pass as a clean exit.  The literal `fail [status: 0]`
/// is caught earlier by [`crate::typecheck::builtins::fail_status_is_zero_literal`].
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
        // In command position `fail` takes one `Any` (`sig::FAIL`), so a scalar
        // reaches here: raise the author's text, not a shape complaint burying it.
        Some(Value::String(s)) => return Break::Error(Error::new(s.clone(), 1)),
        Some(Value::Bytes(b)) => {
            return Break::Error(Error::new(String::from_utf8_lossy(b).into_owned(), 1));
        }
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

/// Hand the event to the host's structured-event sink, or to nothing when none
/// is installed.  [`Mooring::surface`] drops values that are not first-order.
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

// Opens the controlling terminal rather than using the shell's stdio, so the
// prompt still reaches the user under redirection.
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
