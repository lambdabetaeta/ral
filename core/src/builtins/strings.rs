//! String, regex, shell-word, and value-coercion builtins.

use crate::types::{Settled, Shell, Value, as_list, sig, sig_hint};
use std::borrow::Cow;

#[cfg(feature = "grep")]
use super::util::regex_err;
use super::util::{arg0_str, f64_to_i64};

#[cfg(not(feature = "grep"))]
const NO_GREP: &str = "regex operations require the grep feature — rebuild with --features grep";

/// Parse a `Value` as a `usize` index; junk errors rather than coercing to zero.
fn as_index(v: &Value, ctx: &str) -> Settled<usize> {
    match v {
        Value::Int(n) => usize::try_from(*n)
            .map_err(|_| sig(format!("{ctx}: index must be non-negative, got {n}"))),
        Value::String(s) => s
            .parse::<usize>()
            .map_err(|_| sig(format!("{ctx}: '{s}' is not a non-negative integer"))),
        other => Err(sig(format!(
            "{ctx}: expected Int, got {}",
            other.type_name()
        ))),
    }
}

pub(super) fn builtin_len(args: &[Value]) -> Settled<Value> {
    let val = &args[0];
    let n = match val {
        Value::String(s) => s.chars().count(),
        Value::Bytes(b) => b.len(),
        Value::List(items) => items.len(),
        Value::Map(m) => m.len(),
        _ => {
            return Err(sig(format!(
                "length: expected String, Bytes, List, or Map, got {}",
                val.type_name()
            )));
        }
    };
    #[allow(
        clippy::cast_possible_wrap,
        reason = "n is an in-memory char/byte/element count; a live collection cannot exceed i64::MAX"
    )]
    Ok(Value::Int(n as i64))
}

pub(super) fn builtin_upper(args: &[Value]) -> Settled<Value> {
    Ok(Value::String(arg0_str(args)?.to_uppercase()))
}

pub(super) fn builtin_lower(args: &[Value]) -> Settled<Value> {
    Ok(Value::String(arg0_str(args)?.to_lowercase()))
}

pub(super) fn builtin_dedent(args: &[Value]) -> Settled<Value> {
    Ok(Value::String(dedent(&arg0_str(args)?)))
}

pub(super) fn builtin_join(args: &[Value]) -> Settled<Value> {
    let sep = args[0].to_string();
    let items = as_list(&args[1], "intercalate")?;
    Ok(Value::String(
        items
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(&sep),
    ))
}

pub(super) fn builtin_slice(args: &[Value]) -> Settled<Value> {
    let s = args[0].to_string();
    let start = as_index(&args[1], "slice start")?;
    let length = as_index(&args[2], "slice length")?;
    Ok(Value::String(s.chars().skip(start).take(length).collect()))
}

pub(super) fn builtin_shell_split(args: &[Value]) -> Settled<Value> {
    let s = arg0_str(args)?;
    // shlex signals every malformed shape as a bare `None`, so one message covers all.
    let parts = shlex::split(&s)
        .ok_or_else(|| sig("shell-split: malformed input (unterminated quote?)".to_string()))?;
    Ok(Value::list(parts.into_iter().map(Value::String).collect()))
}

pub(super) fn builtin_shell_quote(args: &[Value]) -> Settled<Value> {
    let s = arg0_str(args)?;
    let quoted = shlex::try_quote(&s).map_err(|e| sig(format!("shell-quote: {e}")))?;
    Ok(Value::String(quoted.into_owned()))
}

#[cfg(feature = "grep")]
fn compile_regex(ctx: &str, pattern: &str) -> Settled<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| sig(regex_err(ctx, pattern, &e.to_string())))
}

/// Compile `args[0]`, hand the regex to `f`.  The `grep` gate lives here so
/// the six regex builtins need not each repeat the `NO_GREP` refusal.
fn with_regex(
    ctx: &'static str,
    args: &[Value],
    f: impl FnOnce(&regex::Regex, &[Value]) -> Settled<Value>,
) -> Settled<Value> {
    #[cfg(feature = "grep")]
    {
        let re = compile_regex(ctx, &args[0].to_string())?;
        f(&re, args)
    }
    #[cfg(not(feature = "grep"))]
    {
        let _ = f;
        Err(sig(NO_GREP))
    }
}

/// Errors unless the pattern matches exactly once, like its literal counterpart
/// [`builtin_string_replace`]; [`builtin_replace_all`] is the every-match variant.
pub(super) fn builtin_replace(args: &[Value]) -> Settled<Value> {
    with_regex("re-replace", args, |re, args| {
        let input = args[2].to_string();
        match re.find_iter(&input).count() {
            0 => Err(sig(
                "re-replace: pattern not found in input — is the file already updated, \
                 or did the pattern get whitespace-mangled?",
            )),
            1 => Ok(Value::String(
                re.replace(&input, args[1].to_string().as_str())
                    .into_owned(),
            )),
            n => Err(sig(format!(
                "re-replace: pattern matches {n} times — must match exactly once; \
                 widen the pattern with surrounding context to disambiguate, \
                 or use re-replace-all to replace every match"
            ))),
        }
    })
}

/// Literal replace of the unique occurrence of `from` in `s` with `to`; no regex,
/// so braces, dollars, and backslashes are verbatim.  `pub` so exarch's
/// `edit-replace` can share this match/error logic.
///
/// # Errors
/// If given fewer than three arguments, if `from` is empty, or if `from` does not
/// occur in `s` exactly once.
pub fn builtin_string_replace(args: &[Value]) -> Settled<Value> {
    let from = args[0].to_string();
    let to = args[1].to_string();
    let input = args[2].to_string();
    if from.is_empty() {
        return Err(sig("string-replace: 'from' must be non-empty"));
    }
    let count = input.matches(&from).count();
    match count {
        0 => Err(sig(
            "string-replace: 'from' not found in input — is the file already updated, \
             or did the pattern get whitespace-mangled?",
        )),
        1 => Ok(Value::String(input.replacen(&from, &to, 1))),
        n => Err(sig(format!(
            "string-replace: 'from' matches {n} times — must match exactly once; \
             widen the pattern with surrounding context to disambiguate"
        ))),
    }
}

pub(super) fn builtin_replace_all(args: &[Value]) -> Settled<Value> {
    with_regex("re-replace-all", args, |re, args| {
        Ok(Value::String(
            re.replace_all(&args[2].to_string(), args[1].to_string().as_str())
                .into_owned(),
        ))
    })
}

pub(super) fn builtin_split(args: &[Value]) -> Settled<Value> {
    with_regex("re-split", args, |re, args| {
        let input = args[1].to_string();
        Ok(Value::list(
            re.split(&input).map(|p| Value::String(p.into())).collect(),
        ))
    })
}

pub(super) fn builtin_match(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    with_regex("re-match", args, |re, args| {
        let matched = re.is_match(&args[1].to_string());
        shell.set_status_from_bool(matched);
        Ok(Value::Bool(matched))
    })
}

pub(super) fn builtin_find_match(args: &[Value]) -> Settled<Value> {
    with_regex("re-find-match", args, |re, args| {
        let input = args[1].to_string();
        match re.find(&input) {
            Some(m) => Ok(Value::String(m.as_str().to_owned())),
            None => Err(sig(format!(
                "re-find-match: no match for pattern '{}'",
                args[0]
            ))),
        }
    })
}

pub(super) fn builtin_find_matches(args: &[Value]) -> Settled<Value> {
    with_regex("re-find-matches", args, |re, args| {
        let input = args[1].to_string();
        Ok(Value::list(
            re.find_iter(&input)
                .map(|m| Value::String(m.as_str().to_owned()))
                .collect(),
        ))
    })
}

/// Leading whitespace of `line` counted in *characters*, because [`dedent`] strips
/// it with `chars().skip` — a byte count would slice mid-character.
fn leading_whitespace_chars(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// Strip the common leading whitespace — the minimum across non-blank lines — from
/// `s`, dropping the blank framing lines a multi-line literal opens and closes with.
fn dedent(s: &str) -> String {
    let lines = s.split('\n').collect::<Vec<_>>();
    let start = lines.iter().position(|line| !line.trim().is_empty());
    let Some(start) = start else {
        return String::new();
    };
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .expect("start found a non-blank line");
    let block = &lines[start..=end];
    let min_indent = block
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|line| leading_whitespace_chars(line))
        .min()
        .expect("block contains a non-blank line");
    // A lone content line is its own minimum; stripping would erase all its indent.
    let min_indent = if start == end { 0 } else { min_indent };

    block
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            // Only the last line, and only if a `\n` followed it: that `\r` is half
            // of a CRLF terminator the split already ate.  Interior ones stay.
            let line = if start + offset == end && end + 1 < lines.len() {
                line.strip_suffix('\r').unwrap_or(line)
            } else {
                line
            };
            if line.trim().is_empty() || min_indent == 0 {
                Cow::Borrowed(line)
            } else {
                Cow::Owned(line.chars().skip(min_indent).collect::<String>())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn builtin_to_int(args: &[Value]) -> Settled<Value> {
    let val = &args[0];
    match val {
        Value::Int(n) => Ok(Value::Int(*n)),
        // An integral magnitude `i64` cannot hold is refused, not silently clamped.
        Value::Float(f) if f.fract() == 0.0 => f64_to_i64("int", *f).map(Value::Int),
        Value::String(s) => s.trim().parse::<i64>().map(Value::Int).map_err(|_| {
            sig_hint(
                format!("int: '{s}' is not a valid integer"),
                "expected a whole-number string, e.g. int '42'",
            )
        }),
        other => Err(sig_hint(
            format!(
                "int: expected String, Int, or integral Float, got {}",
                other.type_name()
            ),
            "e.g. int '42'",
        )),
    }
}

pub(super) fn builtin_to_float(args: &[Value]) -> Settled<Value> {
    let val = &args[0];
    match val {
        #[allow(
            clippy::cast_precision_loss,
            reason = "Int→Float coercion; loss of low bits beyond 2^53 is the intrinsic, documented semantics of `float`"
        )]
        Value::Int(n) => Ok(Value::Float(*n as f64)),
        Value::Float(f) => Ok(Value::Float(*f)),
        // `f64::from_str` accepts 'NaN' and 'inf', but a Float is finite by
        // construction, so those spellings are refused alongside garbage.
        Value::String(s) => match s.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::Float(f)),
            Ok(_) => Err(sig(format!("float: '{s}' is not a finite number"))),
            Err(_) => Err(sig_hint(
                format!("float: '{s}' is not a valid number"),
                "expected a numeric string, e.g. float '3.14'",
            )),
        },
        other => Err(sig_hint(
            format!("float: expected String or Int, got {}", other.type_name()),
            "e.g. float '3.14'",
        )),
    }
}

/// The renderer `echo` lowers through ([`Value`]'s `Display`).  Bytes go lossy
/// here; `from-string` is the faithful decode, and errors on invalid UTF-8.
pub(super) fn builtin_to_string(args: &[Value]) -> Settled<Value> {
    Ok(Value::String(args[0].to_string()))
}
