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

use crate::ir::{CompKind, Val};
use crate::source::Spanned;
use crate::stream::{DONE_LABEL, HEAD_FIELD, MORE_LABEL, TAIL_FIELD};
use crate::types::*;
use std::sync::Arc;

use super::apply;
use super::util::{
    as_byte_list, as_list, check_arity, decode_utf8_strict, json_to_value, value_to_json,
};

fn read_stdin_bytes(name: &str, shell: &mut Shell) -> Settled<Vec<u8>> {
    use std::io::Read;

    let mut bytes = Vec::new();
    super::util::stdin_reader(name, shell)?
        .read_to_end(&mut bytes)
        .map_err(|e| sig(format!("{name}: {e}")))?;
    Ok(bytes)
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
) -> Settled<Vec<u8>> {
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

pub(super) fn builtin_fold_lines(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "fold-lines")?;
    let func = args[0].clone();
    let mut acc = args[1].clone();
    super::util::for_each_stdin_line("fold-lines", shell, |line, shell| {
        acc = apply(&func, &[acc.clone(), Value::String(line)], shell)?;
        Ok(())
    })?;
    Ok(acc)
}

pub(super) fn builtin_from_bytes(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    Ok(Value::Bytes(input_bytes(args, "from-bytes", true, shell)?))
}

pub(super) fn builtin_from_string(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bytes = input_bytes(args, "from-string", false, shell)?;
    Ok(Value::String(decode_utf8_strict(
        bytes,
        "from-string: input is not valid UTF-8",
        "use from-bytes to keep raw bytes",
    )?))
}

pub(super) fn builtin_from_line(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bytes = input_bytes(args, "from-line", false, shell)?;
    let text = decode_utf8_strict(
        bytes,
        "from-line: input is not valid UTF-8",
        "use from-bytes to keep raw bytes",
    )?;
    Ok(Value::String(
        crate::io::str_strip_one_terminator(&text).to_owned(),
    ))
}

fn stream_cons(head: String, tail: Value) -> Value {
    let mut captured = Env::new();
    captured.set("__stream_tail".into(), tail);
    let body = Arc::new(Spanned::synthetic(CompKind::Return(Val::Variable(
        "__stream_tail".into(),
    ))));
    Value::Variant {
        label: MORE_LABEL.into(),
        payload: Some(Box::new(Value::map(vec![
            (HEAD_FIELD.into(), Value::String(head)),
            (
                TAIL_FIELD.into(),
                Value::Block {
                    body,
                    captured: Arc::new(captured),
                },
            ),
        ]))),
    }
}

pub(super) fn builtin_from_lines(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bytes = input_bytes(args, "from-lines", false, shell)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let mut s = Value::Variant {
        label: DONE_LABEL.into(),
        payload: None,
    };
    for line in text.lines().rev() {
        s = stream_cons(line.to_owned(), s);
    }
    Ok(s)
}

pub(super) fn builtin_from_json(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let bytes = input_bytes(args, "from-json", false, shell)?;
    let text = decode_utf8_strict(
        bytes,
        "from-json: input is not valid UTF-8",
        "use from-bytes to keep raw bytes",
    )?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| sig(format!("from-json: {e}")))?;
    json_to_value(&json)
}

/// Common tail for every `to-X` builtin: write encoded bytes to stdout and
/// return them as `Value::Bytes`.
fn write_encoded(name: &str, bytes: Vec<u8>, shell: &mut Shell) -> Settled<Value> {
    shell
        .write_stdout(&bytes)
        .map_err(|e| sig(format!("{name}: {e}")))?;
    Ok(Value::Bytes(bytes))
}

pub(super) fn builtin_to_bytes(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-bytes")?;
    let bs = as_byte_list(&args[0], "to-bytes")?;
    write_encoded("to-bytes", bs, shell)
}

pub(super) fn builtin_to_string(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-string")?;
    write_encoded("to-string", args[0].to_string().into_bytes(), shell)
}

pub(super) fn builtin_to_line(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-line")?;
    let mut s = args[0].to_string();
    s.push('\n');
    write_encoded("to-line", s.into_bytes(), shell)
}

pub(super) fn builtin_to_lines(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-lines")?;
    let items = as_list(&args[0], "to-lines")?;
    let joined = items
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    write_encoded("to-lines", joined.into_bytes(), shell)
}

pub(super) fn builtin_to_json(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "to-json")?;
    let text = serde_json::to_string(&value_to_json(&args[0])?)
        .map_err(|e| sig(format!("to-json: {e}")))?;
    write_encoded("to-json", text.into_bytes(), shell)
}
