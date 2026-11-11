//! String, regex, shell-word, and value-coercion builtins.

use crate::data::Finite;
use crate::error::Error;
use crate::fact::{
    CodecFailure, CodecFailureMatchCount, Expected, IntOrFloat, InvalidCause, InvalidFailure,
    LiteralOrRegex, Number, NumberFailure, NumberFault, NumberRange, Reason, ShapeFailure,
    ValueShape,
};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Settled, Value};
use std::borrow::Cow;

use super::util::regex_failure;
use super::util::{lossy_line_list, read_lines};

/// Decode a `Value` as a `usize` index.
pub(super) fn as_index(v: &Value, operation: &str, argument: usize) -> Settled<usize> {
    let cause = match v {
        Value::Int(n) => match usize::try_from(*n) {
            Ok(index) => return Ok(index),
            Err(_) => InvalidCause::Range(NumberRange {
                value: Number::Int(*n),
                minimum: Number::Int(0),
                maximum: Number::Int(i64::MAX),
            }),
        },
        _ => InvalidCause::Shape(ShapeFailure {
            expected: Expected::Shape(ValueShape::Int),
            found: v.found(),
        }),
    };
    Err(Error::new(Reason::Invalid(Box::new(
        InvalidFailure::new(operation.into(), cause).index(argument.into()),
    )))
    .into())
}

pub(super) fn builtin_len(args: &[Value]) -> Settled<Value> {
    let val = &args[0];
    let n = match val {
        Value::String(s) => s.chars().count(),
        Value::Bytes(b) => b.len(),
        Value::List(items) => items.len(),
        Value::Map(m) => m.len(),
        _ => {
            return Err(val
                .expected(
                    "length",
                    Expected::AnyOf(vec![
                        ValueShape::String,
                        ValueShape::Bytes,
                        ValueShape::List,
                        ValueShape::Map,
                    ]),
                )
                .into());
        }
    };
    #[allow(
        clippy::cast_possible_wrap,
        reason = "n is an in-memory char/byte/element count; a live collection cannot exceed i64::MAX"
    )]
    Ok(Value::Int(n as i64))
}

pub(super) fn builtin_upper(args: &[Value]) -> Settled<Value> {
    Ok(Value::string(args[0].as_str("upper")?.to_uppercase()))
}

pub(super) fn builtin_lower(args: &[Value]) -> Settled<Value> {
    Ok(Value::string(args[0].as_str("lower")?.to_lowercase()))
}

pub(super) fn builtin_dedent(args: &[Value]) -> Settled<Value> {
    Ok(Value::string(dedent(args[0].as_str("dedent")?)))
}

pub(super) fn builtin_join(args: &[Value]) -> Settled<Value> {
    let sep = args[0].as_str("intercalate")?;
    let items = args[1].as_list("intercalate")?;
    Ok(Value::string(
        items
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(sep),
    ))
}

pub(super) fn builtin_slice(args: &[Value]) -> Settled<Value> {
    let s = args[0].as_str("slice")?;
    let start = as_index(&args[1], "slice", 1)?;
    let length = as_index(&args[2], "slice", 2)?;
    Ok(Value::string(
        s.chars().skip(start).take(length).collect::<String>(),
    ))
}

/// `from-lines` over the string's bytes rather than the channel.
pub(super) fn builtin_lines(args: &[Value]) -> Settled<Value> {
    let s = args[0].as_str("lines")?;
    lossy_line_list(read_lines("lines", s.as_bytes()))
}

pub(super) fn builtin_posix_split(args: &[Value]) -> Settled<Value> {
    let s = args[0].as_str("posix-split")?;
    // shlex signals every malformed shape as a bare `None`, so one message covers all.
    let parts = shlex::split(s)
        .ok_or_else(|| invalid("posix-split", InvalidCause::Codec(CodecFailure::PosixSplit)))?;
    Ok(Value::list(parts.into_iter().map(Value::string).collect()))
}

pub(super) fn builtin_posix_quote(args: &[Value]) -> Settled<Value> {
    let s = args[0].as_str("posix-quote")?;
    let quoted = shlex::try_quote(s).map_err(|error| {
        invalid(
            "posix-quote",
            InvalidCause::Codec(match error {
                shlex::QuoteError::Nul => CodecFailure::PosixQuoteNul,
                other => CodecFailure::PosixQuoteUnclassified(other.to_string()),
            }),
        )
    })?;
    Ok(Value::string(quoted))
}

pub(super) fn builtin_ral_quote(args: &[Value]) -> Settled<Value> {
    let s = args[0].as_str("ral-quote")?;
    Ok(Value::string(crate::syntax::quote_word(s).into_owned()))
}

fn compile_regex(ctx: &str, pattern: &str) -> Settled<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| {
        invalid(
            ctx,
            InvalidCause::Codec(CodecFailure::Regex(regex_failure(pattern, &e))),
        )
        .into()
    })
}

/// Compile `args[0]`, hand the regex to `f`.
fn with_regex(
    ctx: &'static str,
    args: &[Value],
    f: impl FnOnce(&regex::Regex, &[Value]) -> Settled<Value>,
) -> Settled<Value> {
    let re = compile_regex(ctx, args[0].as_str(ctx)?)?;
    f(&re, args)
}

/// Errors unless the pattern matches exactly once, like its literal counterpart
/// [`builtin_string_replace`]; [`builtin_replace_all`] is the every-match variant.
pub(super) fn builtin_replace(args: &[Value]) -> Settled<Value> {
    with_regex("re-replace", args, |re, args| {
        let input = args[2].as_str("re-replace")?;
        let matches = re.find_iter(input).count();
        if matches != 1 {
            return Err(
                match_count("re-replace", LiteralOrRegex::Regex, re.as_str(), matches).into(),
            );
        }
        Ok(Value::string(
            re.replace(input, args[1].as_str("re-replace")?),
        ))
    })
}

/// Byte offsets of every occurrence of `from` in `input`, **overlaps included**.
///
/// `"aa"` occurs twice in `"aaa"`, so a needle that overlaps itself cannot pass
/// for unique.  `pub` so exarch's `edit-replace` counts the way this does.
pub fn occurrence_starts(input: &str, from: &str) -> Vec<usize> {
    debug_assert!(
        !from.is_empty(),
        "an empty needle has no occurrence to name"
    );
    // Step by the first char, not one byte: an occurrence begins on a char
    // boundary, so nothing between two overlapping hits is skipped.
    let step = from.chars().next().map_or(1, char::len_utf8);
    let mut starts = Vec::new();
    let mut i = 0;
    while let Some(offset) = input[i..].find(from) {
        let at = i + offset;
        starts.push(at);
        i = at + step;
    }
    starts
}

/// Literal replace of the unique occurrence of `from` in `s` with `to`.
///
/// No regex, so braces, dollars, and backslashes are verbatim.  Counts through
/// [`occurrence_starts`], which exarch's `edit-replace` shares.
///
/// # Errors
/// If given fewer than three arguments, if `from` is empty, or if `from` does not
/// occur in `s` exactly once.
pub(crate) fn builtin_string_replace(args: &[Value]) -> Settled<Value> {
    let from = args[0].as_str("string-replace")?;
    let to = args[1].as_str("string-replace")?;
    let input = args[2].as_str("string-replace")?;
    if from.is_empty() {
        return Err(invalid(
            "string-replace",
            InvalidCause::Codec(CodecFailure::EmptyReplacementPattern),
        )
        .into());
    }
    let matches = occurrence_starts(input, from).len();
    if matches != 1 {
        return Err(match_count("string-replace", LiteralOrRegex::Literal, from, matches).into());
    }
    Ok(Value::string(input.replacen(from, to, 1)))
}

pub(super) fn builtin_replace_all(args: &[Value]) -> Settled<Value> {
    with_regex("re-replace-all", args, |re, args| {
        let input = args[2].as_str("re-replace-all")?;
        Ok(Value::string(
            re.replace_all(input, args[1].as_str("re-replace-all")?),
        ))
    })
}

pub(super) fn builtin_split(args: &[Value]) -> Settled<Value> {
    with_regex("re-split", args, |re, args| {
        let input = args[1].as_str("re-split")?;
        Ok(Value::list(re.split(input).map(Value::string).collect()))
    })
}

pub(super) fn builtin_match(args: &[Value]) -> Settled<Value> {
    with_regex("re-match", args, |re, args| {
        let matched = re.is_match(args[1].as_str("re-match")?);
        Ok(Value::Bool(matched))
    })
}

pub(super) fn builtin_find_match(args: &[Value]) -> Settled<Value> {
    with_regex("re-find-match", args, |re, args| {
        let input = args[1].as_str("re-find-match")?;
        match re.find(input) {
            Some(m) => Ok(Value::string(m.as_str())),
            None => Err(match_count("re-find-match", LiteralOrRegex::Regex, re.as_str(), 0).into()),
        }
    })
}

pub(super) fn builtin_find_matches(args: &[Value]) -> Settled<Value> {
    with_regex("re-find-matches", args, |re, args| {
        let input = args[1].as_str("re-find-matches")?;
        Ok(Value::list(
            re.find_iter(input)
                .map(|m| Value::string(m.as_str()))
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

fn match_count(operation: &str, kind: LiteralOrRegex, pattern: &str, matches: usize) -> Error {
    invalid(
        operation,
        InvalidCause::Codec(CodecFailure::MatchCount(CodecFailureMatchCount {
            kind,
            pattern: pattern.into(),
            matches: matches.into(),
        })),
    )
}

fn number_failure(operation: &str, text: &str, target: IntOrFloat, why: NumberFault) -> Error {
    invalid(
        operation,
        InvalidCause::Number(NumberFailure {
            text: text.into(),
            target,
            why,
        }),
    )
}

pub(super) fn builtin_to_int(args: &[Value]) -> Settled<Value> {
    match &args[0] {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Float(f) if f.get().fract() == 0.0 => f
            .to_i64()
            .map(Value::Int)
            .ok_or_else(|| invalid("int", InvalidCause::IntegerRange(*f)).into()),
        Value::Float(f) => Err(invalid("int", InvalidCause::NonIntegral(*f)).into()),
        Value::String(s) => s.trim().parse::<i64>().map(Value::Int).map_err(|error| {
            let why = match error.kind() {
                std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                    NumberFault::OutOfRange
                }
                _ => NumberFault::Syntax,
            };
            number_failure("int", s, IntOrFloat::Int, why).into()
        }),
        other => Err(other
            .expected(
                "int",
                Expected::AnyOf(vec![ValueShape::String, ValueShape::Int, ValueShape::Float]),
            )
            .into()),
    }
}

pub(super) fn builtin_to_float(args: &[Value]) -> Settled<Value> {
    match &args[0] {
        Value::Int(n) => Ok(Value::Float(Finite::from(*n))),
        Value::Float(f) => Ok(Value::Float(*f)),
        Value::String(s) => match s.parse::<f64>().map(Finite::new) {
            Ok(Some(f)) => Ok(Value::Float(f)),
            Ok(None) => {
                Err(number_failure("float", s, IntOrFloat::Float, NumberFault::NonFinite).into())
            }
            Err(_) => {
                Err(number_failure("float", s, IntOrFloat::Float, NumberFault::Syntax).into())
            }
        },
        other => Err(other
            .expected(
                "float",
                Expected::AnyOf(vec![ValueShape::String, ValueShape::Int, ValueShape::Float]),
            )
            .into()),
    }
}

/// The renderer `echo` lowers through ([`Value`]'s `Display`).  Bytes go lossy
/// here; `from-string` is the faithful decode, and errors on invalid UTF-8.
pub(super) fn builtin_to_string(args: &[Value]) -> Value {
    match &args[0] {
        s @ Value::String(_) => s.clone(),
        v => Value::string(v.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::DataStep;
    use crate::shell::runtime::Break;

    #[test]
    fn length_requires_a_sized_runtime_head() {
        let Err(Break::Error(error)) = builtin_len(&[Value::Unit]) else {
            panic!("length of an unsized value")
        };
        assert_eq!(
            error.reason,
            Value::Unit
                .expected(
                    "length",
                    Expected::AnyOf(vec![
                        ValueShape::String,
                        ValueShape::Bytes,
                        ValueShape::List,
                        ValueShape::Map,
                    ])
                )
                .reason
        );
    }

    fn refusal(result: Settled<Value>) -> InvalidFailure {
        let Err(Break::Error(error)) = result else {
            panic!("expected refusal")
        };
        let Reason::Invalid(failure) = error.reason else {
            panic!("invalid input")
        };
        *failure
    }

    #[test]
    fn slice_keeps_argument_position_separate_from_operation() {
        let failure = refusal(builtin_slice(&[
            Value::string("hello"),
            Value::string("1"),
            Value::Int(3),
        ]));
        assert_eq!(failure.operation, "slice");
        assert_eq!(failure.path, vec![DataStep::Index(1usize.into())]);
        assert!(matches!(
            failure.cause,
            InvalidCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Int),
                ..
            })
        ));
        let negative = refusal(builtin_slice(&[
            Value::string("hello"),
            Value::Int(0),
            Value::Int(-1),
        ]));
        assert_eq!(negative.path, vec![DataStep::Index(2usize.into())]);
        assert!(matches!(negative.cause, InvalidCause::Range(_)));
    }

    #[test]
    fn numeric_conversions_distinguish_syntax_range_fraction_and_nonfinite() {
        for (text, target, fault) in [
            ("garbage", IntOrFloat::Int, NumberFault::Syntax),
            (
                "9223372036854775808",
                IntOrFloat::Int,
                NumberFault::OutOfRange,
            ),
            (
                "-9223372036854775809",
                IntOrFloat::Int,
                NumberFault::OutOfRange,
            ),
            ("garbage", IntOrFloat::Float, NumberFault::Syntax),
            ("NaN", IntOrFloat::Float, NumberFault::NonFinite),
            ("1e999", IntOrFloat::Float, NumberFault::NonFinite),
        ] {
            let result = match target {
                IntOrFloat::Int => builtin_to_int(&[Value::string(text)]),
                IntOrFloat::Float => builtin_to_float(&[Value::string(text)]),
            };
            assert_eq!(
                refusal(result).cause,
                InvalidCause::Number(NumberFailure {
                    text: text.into(),
                    target,
                    why: fault
                })
            );
        }
        let f = Finite::new(3.7).unwrap();
        assert_eq!(
            refusal(builtin_to_int(&[Value::Float(f)])).cause,
            InvalidCause::NonIntegral(f)
        );
        let large = Finite::new(f64::MAX).unwrap();
        assert_eq!(
            refusal(builtin_to_int(&[Value::Float(large)])).cause,
            InvalidCause::IntegerRange(large)
        );
        for builtin in [builtin_to_int, builtin_to_float] {
            assert!(matches!(
                refusal(builtin(&[Value::Bool(true)])).cause,
                InvalidCause::Shape(_)
            ));
        }
    }

    #[test]
    fn codec_refusals_distinguish_quotes_regex_and_literal_matches() {
        assert_eq!(
            refusal(builtin_posix_split(&[Value::string("'")])).cause,
            InvalidCause::Codec(CodecFailure::PosixSplit)
        );
        assert_eq!(
            refusal(builtin_posix_quote(&[Value::string("\0")])).cause,
            InvalidCause::Codec(CodecFailure::PosixQuoteNul)
        );
        let invalid = builtin_match(&[Value::string("["), Value::string("x")]);
        assert!(
            matches!(refusal(invalid).cause, InvalidCause::Codec(CodecFailure::Regex(crate::fact::RegexFailure { pattern, cause: crate::fact::RegexCause::Syntax(_) })) if pattern == "[")
        );
        for (from, input, matches) in [("z", "abc", 0usize), ("aa", "aaa", 2usize)] {
            assert_eq!(
                refusal(builtin_string_replace(&[
                    Value::string(from),
                    Value::string("x"),
                    Value::string(input)
                ]))
                .cause,
                InvalidCause::Codec(CodecFailure::MatchCount(CodecFailureMatchCount {
                    kind: LiteralOrRegex::Literal,
                    pattern: from.into(),
                    matches: matches.into()
                }))
            );
        }
        assert_eq!(
            refusal(builtin_string_replace(&[
                Value::string(""),
                Value::string("x"),
                Value::string("abc")
            ]))
            .cause,
            InvalidCause::Codec(CodecFailure::EmptyReplacementPattern)
        );
        for (input, matches) in [("x", 0usize), ("aa", 2usize)] {
            assert!(
                matches!(refusal(builtin_replace(&[Value::string("a"), Value::string("x"), Value::string(input)])).cause,
                InvalidCause::Codec(CodecFailure::MatchCount(f)) if f.kind == LiteralOrRegex::Regex && f.matches == matches.into())
            );
        }
        assert!(
            matches!(refusal(builtin_find_match(&[Value::string("a"), Value::string("x")])).cause,
            InvalidCause::Codec(CodecFailure::MatchCount(f)) if f.matches == 0usize.into())
        );
    }
}
