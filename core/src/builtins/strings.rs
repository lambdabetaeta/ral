//! String, regex, shell-word, and value-coercion builtins.

use crate::types::*;
use std::borrow::Cow;

#[cfg(feature = "grep")]
use super::util::regex_err;
use super::util::{arg0_str, as_list, check_arity};

#[cfg(not(feature = "grep"))]
const NO_GREP: &str = "regex operations require the grep feature — rebuild with --features grep";

/// Parse a `Value` as a non-negative `usize` index.  Errors descriptively
/// rather than silently coercing junk to zero.
fn as_index(v: &Value, ctx: &str) -> Settled<usize> {
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

pub(super) fn builtin_len(args: &[Value]) -> Settled<Value> {
    let val = args
        .first()
        .ok_or_else(|| sig("length requires 1 argument"))?;
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
    Ok(Value::Int(n as i64))
}

pub(super) fn builtin_upper(args: &[Value]) -> Settled<Value> {
    Ok(Value::String(arg0_str(args, "upper")?.to_uppercase()))
}

pub(super) fn builtin_lower(args: &[Value]) -> Settled<Value> {
    Ok(Value::String(arg0_str(args, "lower")?.to_lowercase()))
}

pub(super) fn builtin_dedent(args: &[Value]) -> Settled<Value> {
    Ok(Value::String(dedent(&arg0_str(args, "dedent")?)))
}

pub(super) fn builtin_join(args: &[Value]) -> Settled<Value> {
    check_arity(args, 2, "intercalate")?;
    let sep = args[0].to_string();
    let items = as_list(&args[1], "intercalate")?;
    Ok(Value::String(
        items
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(&sep),
    ))
}

pub(super) fn builtin_slice(args: &[Value]) -> Settled<Value> {
    check_arity(args, 3, "slice")?;
    let s = args[0].to_string();
    let start = as_index(&args[1], "slice start")?;
    let length = as_index(&args[2], "slice length")?;
    Ok(Value::String(s.chars().skip(start).take(length).collect()))
}

pub(super) fn builtin_shell_split(args: &[Value]) -> Settled<Value> {
    let s = arg0_str(args, "shell-split")?;
    // shlex returns `None` on malformed input (e.g. unterminated quote)
    // without distinguishing the cause; the underlying tokenizer simply
    // halts.  A single message is honest about that.
    let parts = shlex::split(&s)
        .ok_or_else(|| sig("shell-split: malformed input (unterminated quote?)".to_string()))?;
    Ok(Value::list(parts.into_iter().map(Value::String).collect()))
}

pub(super) fn builtin_shell_quote(args: &[Value]) -> Settled<Value> {
    let s = arg0_str(args, "shell-quote")?;
    let quoted = shlex::try_quote(&s).map_err(|e| sig(format!("shell-quote: {e}")))?;
    Ok(Value::String(quoted.into_owned()))
}

#[cfg(feature = "grep")]
fn compile_regex(ctx: &str, pattern: &str) -> Settled<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| sig(regex_err(ctx, pattern, &e.to_string())))
}

/// The prologue the regex builtins share: arity-check, then — behind the
/// `grep` feature — compile `args[0]` and hand the regex plus the full
/// argument list to `f`.  Without the feature every regex builtin returns
/// the same `NO_GREP` refusal, so the feature gate is defined once here
/// rather than duplicated across the six regex builtins.
fn with_regex(
    ctx: &'static str,
    args: &[Value],
    arity: usize,
    f: impl FnOnce(&regex::Regex, &[Value]) -> Settled<Value>,
) -> Settled<Value> {
    check_arity(args, arity, ctx)?;
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

/// Regex replace of the *unique* match of `pattern` in `input`.  Like its
/// literal counterpart [`builtin_string_replace`], it errors when the pattern
/// matches zero or more than once: a surgical replacement only means anything
/// when the target is unique, and silently editing the first of several (or
/// nothing at all) is a footgun.  [`builtin_replace_all`] is the every-match
/// variant.
pub(super) fn builtin_replace(args: &[Value]) -> Settled<Value> {
    with_regex("re-replace", args, 3, |re, args| {
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

/// Literal-string replace of the unique occurrence of `from` in `s`
/// with `to`.  Errors when `from` is empty, absent, or matches more
/// than once: surgical replacement operations only mean anything when
/// the target is unique, and silently
/// accepting a 0- or many-match request would be a footgun.  No regex
/// involvement — `from` and `to` are taken verbatim, so braces,
/// dollars, and backslashes carry no special meaning.  `pub` (not
/// `pub(super)`) so exarch's `edit-str` builtin can share this exact
/// match/error logic rather than duplicating it.
pub fn builtin_string_replace(args: &[Value]) -> Settled<Value> {
    check_arity(args, 3, "string-replace")?;
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
    with_regex("re-replace-all", args, 3, |re, args| {
        Ok(Value::String(
            re.replace_all(&args[2].to_string(), args[1].to_string().as_str())
                .into_owned(),
        ))
    })
}

pub(super) fn builtin_split(args: &[Value]) -> Settled<Value> {
    with_regex("re-split", args, 2, |re, args| {
        let input = args[1].to_string();
        Ok(Value::list(
            re.split(&input).map(|p| Value::String(p.into())).collect(),
        ))
    })
}

pub(super) fn builtin_match(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    with_regex("re-match", args, 2, |re, args| {
        let matched = re.is_match(&args[1].to_string());
        shell.set_status_from_bool(matched);
        Ok(Value::Bool(matched))
    })
}

pub(super) fn builtin_find_match(args: &[Value]) -> Settled<Value> {
    with_regex("re-find-match", args, 2, |re, args| {
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
    with_regex("re-find-matches", args, 2, |re, args| {
        let input = args[1].to_string();
        Ok(Value::list(
            re.find_iter(&input)
                .map(|m| Value::String(m.as_str().to_owned()))
                .collect(),
        ))
    })
}

/// Number of leading whitespace *characters* on `line`.  Counts characters,
/// not bytes, so a multibyte whitespace (NBSP, ideographic space) advances
/// the indent by one — the byte-count form panicked when a later slice
/// landed mid-character.
fn leading_whitespace_chars(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// Strip the common leading whitespace from every non-blank line of `s`.
///
/// The indent level is the minimum count of leading whitespace characters
/// across all lines that contain at least one non-whitespace character.
/// Blank framing lines around the block fall away, so the common
/// `dedent #'\n  text\n'#` shape does not leave an opener/closer line in
/// the value.  Interior blank lines are preserved unchanged, and an interior
/// `\r` belonging to a CRLF terminator stays put: `s` is split on `\n` and
/// rejoined on `\n`.
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
    // A single content line has no peers to share a common indent with:
    // its own leading whitespace *is* the minimum.  Stripping it would erase
    // all indentation, so leave `min_indent` at zero.
    let min_indent = if start == end { 0 } else { min_indent };

    block
        .iter()
        .enumerate()
        .map(|(offset, line)| {
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
    check_arity(args, 1, "int")?;
    let val = &args[0];
    match val {
        Value::Int(n) => Ok(Value::Int(*n)),
        // `as i64` saturates a float beyond i64 to the nearest bound; an
        // integral magnitude that large is not representable, so refuse it
        // rather than report a silently clamped value.  The half-open
        // bound is `2^63`: i64::MAX (`2^63 - 1`) rounds up to `2^63` as an
        // f64, so the comparison must be strict against `2^63`.
        Value::Float(f) if f.fract() == 0.0 => {
            if *f >= 9223372036854775808.0 || *f < -9223372036854775808.0 {
                Err(sig(format!("int: {f} is outside the integer range")))
            } else {
                Ok(Value::Int(*f as i64))
            }
        }
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
    check_arity(args, 1, "float")?;
    let val = &args[0];
    match val {
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
    }
}

/// `str <val>` renders any value to its `String` form via [`Value`]'s
/// `Display`.  Bytes render as lossy UTF-8 (like `List`/`Map`, which are
/// equally non-round-trippable); faithful byte→text decoding is `from-string`,
/// which errors on invalid UTF-8.  This is the renderer `echo` lowers through.
pub(super) fn builtin_to_string(args: &[Value]) -> Settled<Value> {
    check_arity(args, 1, "str")?;
    Ok(Value::String(args[0].to_string()))
}
