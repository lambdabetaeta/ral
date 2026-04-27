use crate::types::*;

use super::call_value;
use super::util::{as_byte_list, as_list, check_arity, json_to_value, sig, sig_hint, value_to_json};

fn read_stdin_bytes(shell: &mut Shell) -> Result<Vec<u8>, EvalSignal> {
    use std::io::Read;

    let mut bytes = Vec::new();
    if let Some(reader) = shell.io.stdin.take_pipe() {
        (&reader)
            .read_to_end(&mut bytes)
            .map_err(|e| sig(format!("_decode: {e}")))?;
    } else {
        if shell.io.terminal.stdin_tty {
            return Err(sig(
                "_decode: no input (pipe bytes or use a from-X command)",
            ));
        }
        std::io::stdin()
            .lock()
            .read_to_end(&mut bytes)
            .map_err(|e| sig(format!("_decode: {e}")))?;
    }
    Ok(bytes)
}

fn decode_utf8(bytes: Vec<u8>, codec: &str) -> Result<String, EvalSignal> {
    String::from_utf8(bytes).map_err(|e| {
        sig_hint(
            format!("_decode {codec}: input is not valid UTF-8: {e}"),
            "use from-bytes to keep raw bytes",
        )
    })
}

pub(super) fn builtin_fold_lines(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "lines")?;
    shell.control.in_tail_position = false;
    let func = &args[0];
    let mut acc = args[1].clone();
    use std::io::BufRead;
    if let Some(reader) = shell.io.stdin.take_pipe() {
        for line in std::io::BufReader::new(reader).lines() {
            let line = line.map_err(|e| sig(format!("lines: {e}")))?;
            acc = call_value(func, &[acc, Value::String(line)], shell)?;
        }
    } else if shell.io.terminal.stdin_tty {
        return Err(sig("lines: no input (pipe a value or use in pipeline)"));
    } else {
        for line in std::io::stdin().lock().lines() {
            let line = line.map_err(|e| sig(format!("lines: {e}")))?;
            acc = call_value(func, &[acc, Value::String(line)], shell)?;
        }
    }
    Ok(acc)
}

pub(super) fn builtin_decode(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.is_empty() || args.len() > 2 {
        return Err(sig("_decode requires codec and optional input"));
    }
    let codec = args[0].to_string();
    let input_bytes = if args.len() == 2 {
        match &args[1] {
            Value::Bytes(b) => b.clone(),
            v => {
                if codec == "bytes" {
                    return Err(sig_hint(
                        "_decode bytes: expected Bytes, got String",
                        "use from-string for UTF-8 validation, or from-bytes to read raw bytes",
                    ));
                }
                v.to_string().into_bytes()
            }
        }
    } else {
        read_stdin_bytes(shell)?
    };

    match codec.as_str() {
        "bytes" => Ok(Value::Bytes(input_bytes)),
        "string" => Ok(Value::String(decode_utf8(input_bytes, "string")?)),
        "line" => {
            let text = decode_utf8(input_bytes, "line")?;
            let stripped = text
                .strip_suffix("\r\n")
                .or_else(|| text.strip_suffix('\n'))
                .unwrap_or(&text);
            Ok(Value::String(stripped.to_owned()))
        }
        "lines" => {
            let text = String::from_utf8_lossy(&input_bytes).into_owned();
            Ok(Value::List(
                text.lines()
                    .map(|line| Value::String(line.to_string()))
                    .collect(),
            ))
        }
        "json" => {
            let text = decode_utf8(input_bytes, "json")?;
            let json: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| sig(format!("_decode json: {e}")))?;
            Ok(json_to_value(&json))
        }
        _ => Err(sig(
            "_decode: unknown codec (expected bytes|string|line|lines|json)",
        )),
    }
}

pub(super) fn builtin_encode(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.is_empty() || args.len() > 2 {
        return Err(sig("_encode requires codec and value"));
    }
    let codec = args[0].to_string();
    let value = args
        .get(1)
        .ok_or_else(|| sig("_encode: no value (use to-json, to-lines, etc.)"))?
        .clone();

    let out = match codec.as_str() {
        "bytes" => {
            let bs = as_byte_list(&value, "to-bytes")?;
            shell.write_stdout(&bs)
                .map_err(|e| sig(format!("to-bytes: {e}")))?;
            return Ok(Value::Bytes(bs));
        }
        "string" => value.to_string(),
        "line" => {
            // Inverse of `_decode line`: append a single trailing newline.
            let mut s = value.to_string();
            s.push('\n');
            s
        }
        "lines" => {
            let items = as_list(&value, "to-lines")?;
            items
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        }
        "json" => serde_json::to_string(&value_to_json(&value))
            .map_err(|e| sig(format!("to-json: {e}")))?,
        _ => {
            return Err(sig(
                "_encode: unknown codec (expected bytes|string|line|lines|json)",
            ));
        }
    };

    let bytes = out.into_bytes();
    shell.write_stdout(&bytes)
        .map_err(|e| sig(format!("_encode: {e}")))?;
    Ok(Value::Bytes(bytes))
}
