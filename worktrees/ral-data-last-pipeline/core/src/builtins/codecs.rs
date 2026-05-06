//! Byte-channel codecs.
//!
//! Each `from-X` / `to-X` is its own builtin so that `from-json < file` can
//! dispatch directly through the `Exec` arm — no Thunk indirection, the
//! typechecker sees the actual return type, and a misspelled name fails at
//! command lookup rather than as a runtime "unknown codec" string.
//!
//! `from-X` accepts 0 or 1 argument: zero means read stdin (used with `<file`
//! or pipeline input); one means decode the supplied Bytes/String.  `to-X`
//! always takes a single value, writes its encoded form to stdout, and
//! returns Bytes.  The cached-tty gate fires only when stdin is genuinely
//! unset — see `read_stdin_bytes`.

use crate::types::*;

use super::call_value;
use super::util::{as_byte_list, as_list, check_arity, json_to_value, sig, sig_hint, value_to_json};

fn read_stdin_bytes(name: &str, shell: &mut Shell) -> Result<Vec<u8>, EvalSignal> {
    use std::io::Read;

    let mut bytes = Vec::new();
    if let Some(mut reader) = shell.io.stdin.take_reader() {
        reader
            .read_to_end(&mut bytes)
            .map_err(|e| sig(format!("{name}: {e}")))?;
    } else {
        if shell.io.terminal.startup_stdin_tty {
            return Err(sig(format!(
                "{name}: no input (pipe bytes or pass a value as argument)"
            )));
        }
        std::io::stdin()
            .lock()
            .read_to_end(&mut bytes)
            .map_err(|e| sig(format!("{name}: {e}")))?;
    }
    Ok(bytes)
}

fn decode_utf8(bytes: Vec<u8>, name: &str) -> Result<String, EvalSignal> {
    String::from_utf8(bytes).map_err(|e| {
        sig_hint(
            format!("{name}: input is not valid UTF-8: {e}"),
            "use from-bytes to keep raw bytes",
        )
    })
}

/// Source bytes for a `from-X` builtin.  Zero args → stdin; one arg of Bytes
/// passes through; one arg of any other type is rendered to its String form.
/// `from-bytes` is stricter: a non-Bytes argument is an error rather than a
/// silent stringify, since the whole point of the codec is to assert "these
/// are raw bytes already".
fn input_bytes(
    args: &[Value],
    name: &str,
    require_bytes_arg: bool,
    shell: &mut Shell,
) -> Result<Vec<u8>, EvalSignal> {
    match args {
        [] => read_stdin_bytes(name, shell),
        [Value::Bytes(b)] => Ok(b.clone()),
        [v] => {
            if require_bytes_arg {
                Err(sig_hint(
                    format!("{name}: expected Bytes, got {}", v.type_name()),
                    "use from-string for UTF-8 validation, or from-bytes to read raw bytes",
                ))
            } else {
                Ok(v.to_string().into_bytes())
            }
        }
        _ => Err(sig(format!("{name}: too many arguments (expected 0 or 1)"))),
    }
}

pub(super) fn builtin_fold_lines(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "fold-lines")?;
    shell.control.in_tail_position = false;
    let func = &args[0];
    let mut acc = args[1].clone();
    use std::io::BufRead;
    if let Some(reader) = shell.io.stdin.take_reader() {
        for line in std::io::BufReader::new(reader).lines() {
            let line = line.map_err(|e| sig(format!("fold-lines: {e}")))?;
            acc = call_value(func, &[acc, Value::String(line)], shell)?;
        }
    } else if shell.io.terminal.startup_stdin_tty {
        return Err(sig(
            "fold-lines: no input (pipe a value or use in pipeline)",
        ));
    } else {
        for line in std::io::stdin().lock().lines() {
            let line = line.map_err(|e| sig(format!("fold-lines: {e}")))?;
            acc = call_value(func, &[acc, Value::String(line)], shell)?;
        }
    }
    Ok(acc)
}

pub(super) fn builtin_from_bytes(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    Ok(Value::Bytes(input_bytes(args, "from-bytes", true, shell)?))
}

pub(super) fn builtin_from_string(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let bytes = input_bytes(args, "from-string", false, shell)?;
    Ok(Value::String(decode_utf8(bytes, "from-string")?))
}

pub(super) fn builtin_from_line(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let bytes = input_bytes(args, "from-line", false, shell)?;
    let text = decode_utf8(bytes, "from-line")?;
    let stripped = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    Ok(Value::String(stripped.to_owned()))
}

pub(super) fn builtin_from_lines(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let bytes = input_bytes(args, "from-lines", false, shell)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(Value::List(
        text.lines()
            .map(|line| Value::String(line.to_string()))
            .collect(),
    ))
}

pub(super) fn builtin_from_json(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let bytes = input_bytes(args, "from-json", false, shell)?;
    let text = decode_utf8(bytes, "from-json")?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| sig(format!("from-json: {e}")))?;
    Ok(json_to_value(&json))
}

/// Common tail for every `to-X` builtin: write encoded bytes to stdout and
/// return them as `Value::Bytes`.
fn write_encoded(name: &str, bytes: Vec<u8>, shell: &mut Shell) -> Result<Value, EvalSignal> {
    shell
        .write_stdout(&bytes)
        .map_err(|e| sig(format!("{name}: {e}")))?;
    Ok(Value::Bytes(bytes))
}

pub(super) fn builtin_to_bytes(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "to-bytes")?;
    let bs = as_byte_list(&args[0], "to-bytes")?;
    write_encoded("to-bytes", bs, shell)
}

pub(super) fn builtin_to_string(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "to-string")?;
    write_encoded("to-string", args[0].to_string().into_bytes(), shell)
}

pub(super) fn builtin_to_line(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "to-line")?;
    let mut s = args[0].to_string();
    s.push('\n');
    write_encoded("to-line", s.into_bytes(), shell)
}

pub(super) fn builtin_to_lines(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "to-lines")?;
    let items = as_list(&args[0], "to-lines")?;
    let joined = items
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    write_encoded("to-lines", joined.into_bytes(), shell)
}

pub(super) fn builtin_to_json(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "to-json")?;
    let text = serde_json::to_string(&value_to_json(&args[0]))
        .map_err(|e| sig(format!("to-json: {e}")))?;
    write_encoded("to-json", text.into_bytes(), shell)
}
