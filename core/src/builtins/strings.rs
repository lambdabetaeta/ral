use crate::types::*;

use super::util::{arg0_str, as_list, check_arity, sig, sig_hint};
#[cfg(feature = "grep")]
use super::util::regex_err;

#[cfg(not(feature = "grep"))]
const NO_GREP: &str =
    "regex operations require the grep feature — rebuild with --features grep";

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

pub(super) fn builtin_upper(args: &[Value]) -> Result<Value, EvalSignal> {
    Ok(Value::String(arg0_str(args).to_uppercase()))
}

pub(super) fn builtin_lower(args: &[Value]) -> Result<Value, EvalSignal> {
    Ok(Value::String(arg0_str(args).to_lowercase()))
}

pub(super) fn builtin_dedent(args: &[Value]) -> Result<Value, EvalSignal> {
    Ok(Value::String(dedent(&arg0_str(args))))
}

pub(super) fn builtin_join(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "join")?;
    let sep = args[0].to_string();
    let items = as_list(&args[1], "join")?;
    Ok(Value::String(
        items
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(&sep),
    ))
}

pub(super) fn builtin_slice(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 3, "slice")?;
    let s = args[0].to_string();
    let start = as_index(&args[1], "slice start")?;
    let length = as_index(&args[2], "slice length")?;
    Ok(Value::String(s.chars().skip(start).take(length).collect()))
}

pub(super) fn builtin_shell_split(args: &[Value]) -> Result<Value, EvalSignal> {
    let s = arg0_str(args);
    // shlex returns `None` on malformed input (e.g. unterminated quote)
    // without distinguishing the cause; the underlying tokenizer simply
    // halts.  A single message is honest about that.
    let parts = shlex::split(&s)
        .ok_or_else(|| sig("shell-split: malformed input (unterminated quote?)".to_string()))?;
    Ok(Value::List(parts.into_iter().map(Value::String).collect()))
}

pub(super) fn builtin_shell_quote(args: &[Value]) -> Result<Value, EvalSignal> {
    let s = arg0_str(args);
    let quoted = shlex::try_quote(&s).map_err(|e| sig(format!("shell-quote: {e}")))?;
    Ok(Value::String(quoted.into_owned()))
}

#[cfg(feature = "grep")]
fn compile_regex(ctx: &str, pattern: &str) -> Result<regex::Regex, EvalSignal> {
    regex::Regex::new(pattern).map_err(|e| sig(regex_err(ctx, pattern, &e.to_string())))
}

pub(super) fn builtin_replace(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 3, "replace")?;
    #[cfg(feature = "grep")]
    {
        let pattern = args[0].to_string();
        let repl = args[1].to_string();
        let input = args[2].to_string();
        let re = compile_regex("replace", &pattern)?;
        Ok(Value::String(re.replace(&input, repl.as_str()).into_owned()))
    }
    #[cfg(not(feature = "grep"))]
    Err(sig(NO_GREP))
}

pub(super) fn builtin_replace_all(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 3, "replace-all")?;
    #[cfg(feature = "grep")]
    {
        let pattern = args[0].to_string();
        let repl = args[1].to_string();
        let input = args[2].to_string();
        let re = compile_regex("replace-all", &pattern)?;
        Ok(Value::String(
            re.replace_all(&input, repl.as_str()).into_owned(),
        ))
    }
    #[cfg(not(feature = "grep"))]
    Err(sig(NO_GREP))
}

pub(super) fn builtin_split(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "split")?;
    #[cfg(feature = "grep")]
    {
        let pattern = args[0].to_string();
        let input = args[1].to_string();
        let re = compile_regex("split", &pattern)?;
        Ok(Value::List(
            re.split(&input).map(|p| Value::String(p.into())).collect(),
        ))
    }
    #[cfg(not(feature = "grep"))]
    Err(sig(NO_GREP))
}

pub(super) fn builtin_match(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "match")?;
    #[cfg(feature = "grep")]
    {
        let pattern = args[0].to_string();
        let input = args[1].to_string();
        let re = compile_regex("match", &pattern)?;
        let matched = re.is_match(&input);
        shell.set_status_from_bool(matched);
        Ok(Value::Bool(matched))
    }
    #[cfg(not(feature = "grep"))]
    {
        let _ = shell;
        Err(sig(NO_GREP))
    }
}

pub(super) fn builtin_find_match(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "find-match")?;
    #[cfg(feature = "grep")]
    {
        let pattern = args[0].to_string();
        let input = args[1].to_string();
        let re = compile_regex("find-match", &pattern)?;
        match re.find(&input) {
            Some(m) => Ok(Value::String(m.as_str().to_owned())),
            None => Err(sig(format!("find-match: no match for pattern '{pattern}'"))),
        }
    }
    #[cfg(not(feature = "grep"))]
    Err(sig(NO_GREP))
}

pub(super) fn builtin_find_matches(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "find-matches")?;
    #[cfg(feature = "grep")]
    {
        let pattern = args[0].to_string();
        let input = args[1].to_string();
        let re = compile_regex("find-matches", &pattern)?;
        Ok(Value::List(
            re.find_iter(&input)
                .map(|m| Value::String(m.as_str().to_owned()))
                .collect(),
        ))
    }
    #[cfg(not(feature = "grep"))]
    Err(sig(NO_GREP))
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
