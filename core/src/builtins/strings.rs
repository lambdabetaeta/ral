use crate::types::*;

use super::util::{arg0_str, as_list, check_arity, regex_err, sig, sig_hint};

/// Parse a `Value` as a non-negative `usize` index.  Errors descriptively
/// rather than silently coercing junk to zero.
fn as_index(v: &Value, ctx: &str) -> Result<usize, EvalSignal> {
    match v {
        Value::Int(n) if *n >= 0 => Ok(*n as usize),
        Value::Int(n) => Err(sig(format!("{ctx}: index must be non-negative, got {n}"))),
        Value::String(s) => s
            .parse::<usize>()
            .map_err(|_| sig(format!("{ctx}: '{s}' is not a non-negative integer"))),
        other => Err(sig(format!(
            "{ctx}: expected Int, got {}",
            other.type_name()
        ))),
    }
}

pub(super) fn builtin_len(args: &[Value]) -> Result<Value, EvalSignal> {
    let val = args.first().ok_or_else(|| sig("length requires 1 argument"))?;
    let n = match val {
        Value::String(s) => s.chars().count(),
        Value::Bytes(b) => b.len(),
        Value::List(items) => items.len(),
        Value::Map(pairs) => pairs.len(),
        _ => {
            return Err(sig(format!(
                "length: expected String, Bytes, List, or Map, got {}",
                val.type_name()
            )));
        }
    };
    Ok(Value::Int(n as i64))
}

pub(super) fn builtin_str(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let op = args.first().map(|v| v.to_string()).unwrap_or_default();
    let rest = &args[1..];
    match op.as_str() {
        "upper" => Ok(Value::String(arg0_str(rest).to_uppercase())),
        "lower" => Ok(Value::String(arg0_str(rest).to_lowercase())),
        "replace" => {
            check_arity(rest, 3, "replace")?;
            let s = rest[0].to_string();
            let from = rest[1].to_string();
            let to = rest[2].to_string();
            if from.is_empty() {
                return Err(sig("replace: empty pattern"));
            }
            let positions: Vec<usize> =
                s.match_indices(from.as_str()).map(|(i, _)| i).collect();
            match positions.len() {
                0 => Err(sig_hint(
                    "replace: pattern not found",
                    "the file may have changed, or the anchor's whitespace/newlines \
                     differ from the file's — re-read the file and copy the exact bytes",
                )),
                1 => Ok(Value::String(s.replacen(&from, &to, 1))),
                n => Err(sig_hint(
                    format!(
                        "replace: pattern matches {n} times ({}) — ambiguous",
                        line_preview(&s, &positions),
                    ),
                    "widen the anchor with surrounding context (e.g. include the \
                     previous line) so it identifies a single site",
                )),
            }
        }
        "replace-all" => {
            check_arity(rest, 3, "replace-all")?;
            Ok(Value::String(
                rest[0]
                    .to_string()
                    .replace(&rest[1].to_string(), &rest[2].to_string()),
            ))
        }
        "join" => {
            check_arity(rest, 2, "join")?;
            let sep = rest[0].to_string();
            let items = as_list(&rest[1], "join")?;
            Ok(Value::String(
                items
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(&sep),
            ))
        }
        "slice" => {
            check_arity(rest, 3, "slice")?;
            let s = rest[0].to_string();
            let start = as_index(&rest[1], "slice start")?;
            let length = as_index(&rest[2], "slice length")?;
            Ok(Value::String(s.chars().skip(start).take(length).collect()))
        }
        "split" => {
            check_arity(rest, 2, "split")?;
            let pattern = rest[0].to_string();
            let input = rest[1].to_string();
            let re = regex_lite::Regex::new(&pattern)
                .map_err(|e| sig(regex_err("split", &pattern, &e.to_string())))?;
            Ok(Value::List(
                re.split(&input).map(|p| Value::String(p.into())).collect(),
            ))
        }
        "match" => {
            check_arity(rest, 2, "match")?;
            let pattern = rest[0].to_string();
            let input = rest[1].to_string();
            let re = regex_lite::Regex::new(&pattern)
                .map_err(|e| sig(regex_err("match", &pattern, &e.to_string())))?;
            let matched = re.is_match(&input);
            shell.set_status_from_bool(matched);
            Ok(Value::Bool(matched))
        }
        "shell-split" => {
            let s = if rest.is_empty() {
                String::new()
            } else {
                rest[0].to_string()
            };
            let parts = shell_words::split(&s).map_err(|e| sig(format!("shell-split: {e}")))?;
            Ok(Value::List(parts.into_iter().map(Value::String).collect()))
        }
        "shell-quote" => {
            let s = if rest.is_empty() {
                String::new()
            } else {
                rest[0].to_string()
            };
            let quoted = shlex::try_quote(&s).map_err(|e| sig(format!("shell-quote: {e}")))?;
            Ok(Value::String(quoted.into_owned()))
        }
        "dedent" => {
            let s = arg0_str(rest);
            Ok(Value::String(dedent(&s)))
        }
        _ => Err(sig(format!("_str: unknown operation '{op}'"))),
    }
}

/// Render a 1-based line-number list for byte offsets into `s`, capped at
/// five entries with `…` for the rest.  Used in `replace`'s ambiguity
/// error so the caller can widen the anchor at the right site.
fn line_preview(s: &str, positions: &[usize]) -> String {
    const CAP: usize = 5;
    let line_of = |off: usize| s.as_bytes()[..off].iter().filter(|&&b| b == b'\n').count() + 1;
    let head: Vec<String> = positions
        .iter()
        .take(CAP)
        .map(|&p| line_of(p).to_string())
        .collect();
    let suffix = if positions.len() > CAP { ", …" } else { "" };
    format!("lines {}{}", head.join(", "), suffix)
}

/// Strip the common leading whitespace from every non-empty line of `s`.
///
/// The indent level is the minimum number of leading spaces across all lines
/// that contain at least one non-whitespace character.  Blank lines are
/// preserved unchanged (they contribute no indent).  A single trailing
/// newline is preserved if present.
fn dedent(s: &str) -> String {
    let min_indent = s
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    if min_indent == 0 {
        return s.to_owned();
    }

    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            out.push_str(&line[min_indent..]);
            out.push('\n');
        }
    }
    // Preserve trailing newline presence from the original.
    if !s.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

pub(super) fn builtin_convert(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "_convert")?;
    let op = args[0].to_string();
    let val = &args[1];
    match op.as_str() {
        "int" => match val {
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Float(f) if f.fract() == 0.0 => Ok(Value::Int(*f as i64)),
            Value::String(s) => s.parse::<i64>().map(Value::Int).map_err(|_| {
                sig_hint(
                    format!("int: '{s}' is not a valid integer"),
                    "expected a whole-number string, e.g. int '42'",
                )
            }),
            other => Err(sig_hint(
                format!("int: expected String or Int, got {}", other.type_name()),
                "e.g. int '42'",
            )),
        },
        "float" => match val {
            Value::Int(n) => Ok(Value::Float(*n as f64)),
            Value::Float(f) => Ok(Value::Float(*f)),
            Value::String(s) => s.parse::<f64>().map(Value::Float).map_err(|_| {
                sig_hint(
                    format!("float: '{s}' is not a valid number"),
                    "expected a numeric string, e.g. float '3.14'",
                )
            }),
            other => Err(sig_hint(
                format!("float: expected String or Int, got {}", other.type_name()),
                "e.g. float '3.14'",
            )),
        },
        "string" => match val {
            Value::Bytes(_) => Err(sig_hint(
                "str does not accept Bytes",
                "decode bytes first: read-string $bytes",
            )),
            _ => Ok(Value::String(val.to_string())),
        },
        _ => Err(sig(format!("_convert: unknown type '{op}'"))),
    }
}
