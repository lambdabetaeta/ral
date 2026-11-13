//! String, regex, shell-word, and value-coercion builtins.

use crate::data::Finite;
use crate::error::Error;
use crate::fact::{
    CodecFailure, CodecFailureMatchCount, Expected, IntOrFloat, InvalidCause, InvalidFailure,
    LiteralOrRegex, Number, NumberFailure, NumberFault, NumberRange, Reason, ShapeFailure,
    ValueShape,
};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Settled, ValClos, View};
use std::borrow::Cow;

use super::util::regex_failure;
use super::util::{lossy_line_list, read_lines};

/// Decode a `ValClos` as a `usize` index.
pub(super) fn as_index(v: &ValClos, operation: &str, argument: usize) -> Settled<usize> {
    let cause = match v.view() {
        View::Int(n) => match usize::try_from(n) {
            Ok(index) => return Ok(index),
            Err(_) => InvalidCause::Range(NumberRange {
                value: Number::Int(n),
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

pub(super) fn builtin_upper(args: &[ValClos]) -> Settled<ValClos> {
    Ok(ValClos::string(args[0].as_str("upper")?.to_uppercase()))
}

pub(super) fn builtin_lower(args: &[ValClos]) -> Settled<ValClos> {
    Ok(ValClos::string(args[0].as_str("lower")?.to_lowercase()))
}

pub(super) fn builtin_dedent(args: &[ValClos]) -> Settled<ValClos> {
    Ok(ValClos::string(dedent(args[0].as_str("dedent")?)))
}

pub(super) fn builtin_join(args: &[ValClos]) -> Settled<ValClos> {
    let sep = args[0].as_str("intercalate")?;
    let items = args[1].as_list("intercalate")?;
    Ok(ValClos::string(
        items
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(sep),
    ))
}

pub(super) fn builtin_slice(args: &[ValClos]) -> Settled<ValClos> {
    let s = args[0].as_str("slice")?;
    let start = as_index(&args[1], "slice", 1)?;
    let length = as_index(&args[2], "slice", 2)?;
    Ok(ValClos::string(
        s.chars().skip(start).take(length).collect::<String>(),
    ))
}

/// `from-lines` over the string's bytes rather than the channel.
pub(super) fn builtin_lines(args: &[ValClos]) -> Settled<ValClos> {
    let s = args[0].as_str("lines")?;
    lossy_line_list(read_lines("lines", s.as_bytes()))
}

/// Uses Unicode scalar values, matching `slice`'s indexing unit.
pub(super) fn builtin_chars(args: &[ValClos]) -> Settled<ValClos> {
    let s = args[0].as_str("chars")?;
    Ok(ValClos::list(
        s.chars().map(|c| ValClos::string(c.to_string())).collect(),
    ))
}

pub(super) fn builtin_posix_split(args: &[ValClos]) -> Settled<ValClos> {
    let s = args[0].as_str("posix-split")?;
    // shlex signals every malformed shape as a bare `None`, so one message covers all.
    let parts = shlex::split(s)
        .ok_or_else(|| invalid("posix-split", InvalidCause::Codec(CodecFailure::PosixSplit)))?;
    Ok(ValClos::list(
        parts.into_iter().map(ValClos::string).collect(),
    ))
}

pub(super) fn builtin_posix_quote(args: &[ValClos]) -> Settled<ValClos> {
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
    Ok(ValClos::string(quoted))
}

pub(super) fn builtin_ral_quote(args: &[ValClos]) -> Settled<ValClos> {
    let s = args[0].as_str("ral-quote")?;
    Ok(ValClos::string(crate::syntax::quote_word(s).into_owned()))
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
    args: &[ValClos],
    f: impl FnOnce(&regex::Regex, &[ValClos]) -> Settled<ValClos>,
) -> Settled<ValClos> {
    let re = compile_regex(ctx, args[0].as_str(ctx)?)?;
    f(&re, args)
}

/// Errors unless the pattern matches exactly once, like its literal counterpart
/// [`builtin_string_replace`]; [`builtin_replace_all`] is the every-match variant.
pub(super) fn builtin_replace(args: &[ValClos]) -> Settled<ValClos> {
    with_regex("re-replace", args, |re, args| {
        let input = args[2].as_str("re-replace")?;
        let matches = re.find_iter(input).count();
        if matches != 1 {
            return Err(
                match_count("re-replace", LiteralOrRegex::Regex, re.as_str(), matches).into(),
            );
        }
        Ok(ValClos::string(
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
pub(crate) fn builtin_string_replace(args: &[ValClos]) -> Settled<ValClos> {
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
    Ok(ValClos::string(input.replacen(from, to, 1)))
}

pub(super) fn builtin_replace_all(args: &[ValClos]) -> Settled<ValClos> {
    with_regex("re-replace-all", args, |re, args| {
        let input = args[2].as_str("re-replace-all")?;
        Ok(ValClos::string(
            re.replace_all(input, args[1].as_str("re-replace-all")?),
        ))
    })
}

pub(super) fn builtin_split(args: &[ValClos]) -> Settled<ValClos> {
    with_regex("re-split", args, |re, args| {
        let input = args[1].as_str("re-split")?;
        Ok(ValClos::list(
            re.split(input).map(ValClos::string).collect(),
        ))
    })
}

pub(super) fn builtin_match(args: &[ValClos]) -> Settled<ValClos> {
    with_regex("re-match", args, |re, args| {
        let matched = re.is_match(args[1].as_str("re-match")?);
        Ok(ValClos::bool(matched))
    })
}

pub(super) fn builtin_find_match(args: &[ValClos]) -> Settled<ValClos> {
    with_regex("re-find-match", args, |re, args| {
        let input = args[1].as_str("re-find-match")?;
        match re.find(input) {
            Some(m) => Ok(ValClos::string(m.as_str())),
            None => Err(match_count("re-find-match", LiteralOrRegex::Regex, re.as_str(), 0).into()),
        }
    })
}

pub(super) fn builtin_find_matches(args: &[ValClos]) -> Settled<ValClos> {
    with_regex("re-find-matches", args, |re, args| {
        let input = args[1].as_str("re-find-matches")?;
        Ok(ValClos::list(
            re.find_iter(input)
                .map(|m| ValClos::string(m.as_str()))
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

pub(super) fn builtin_to_int(args: &[ValClos]) -> Settled<ValClos> {
    match args[0].view() {
        View::Int(n) => Ok(ValClos::int(n)),
        View::Float(f) if f.get().fract() == 0.0 => f
            .to_i64()
            .map(ValClos::int)
            .ok_or_else(|| invalid("int", InvalidCause::IntegerRange(f)).into()),
        View::Float(f) => Err(invalid("int", InvalidCause::NonIntegral(f)).into()),
        View::String(s) => s.trim().parse::<i64>().map(ValClos::int).map_err(|error| {
            let why = match error.kind() {
                std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                    NumberFault::OutOfRange
                }
                _ => NumberFault::Syntax,
            };
            number_failure("int", s, IntOrFloat::Int, why).into()
        }),
        _ => Err(args[0]
            .expected(
                "int",
                Expected::AnyOf(vec![ValueShape::String, ValueShape::Int, ValueShape::Float]),
            )
            .into()),
    }
}

pub(super) fn builtin_to_float(args: &[ValClos]) -> Settled<ValClos> {
    match args[0].view() {
        View::Int(n) => Ok(ValClos::float(Finite::from(n))),
        View::Float(f) => Ok(ValClos::float(f)),
        View::String(s) => match s.parse::<f64>().map(Finite::new) {
            Ok(Some(f)) => Ok(ValClos::float(f)),
            Ok(None) => {
                Err(number_failure("float", s, IntOrFloat::Float, NumberFault::NonFinite).into())
            }
            Err(_) => {
                Err(number_failure("float", s, IntOrFloat::Float, NumberFault::Syntax).into())
            }
        },
        _ => Err(args[0]
            .expected(
                "float",
                Expected::AnyOf(vec![ValueShape::String, ValueShape::Int, ValueShape::Float]),
            )
            .into()),
    }
}

/// The renderer `echo` lowers through ([`ValClos`]'s `Display`).  Bytes go lossy
/// here; `from-string` is the faithful decode, and errors on invalid UTF-8.
pub(super) fn builtin_to_string(args: &[ValClos]) -> ValClos {
    match args[0].view() {
        View::String(_) => args[0].clone(),
        _ => ValClos::string(args[0].to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::DataStep;
    use crate::shell::runtime::Break;

    fn refusal(result: Settled<ValClos>) -> InvalidFailure {
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
            ValClos::string("hello"),
            ValClos::string("1"),
            ValClos::int(3),
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
            ValClos::string("hello"),
            ValClos::int(0),
            ValClos::int(-1),
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
                IntOrFloat::Int => builtin_to_int(&[ValClos::string(text)]),
                IntOrFloat::Float => builtin_to_float(&[ValClos::string(text)]),
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
            refusal(builtin_to_int(&[ValClos::float(f)])).cause,
            InvalidCause::NonIntegral(f)
        );
        let large = Finite::new(f64::MAX).unwrap();
        assert_eq!(
            refusal(builtin_to_int(&[ValClos::float(large)])).cause,
            InvalidCause::IntegerRange(large)
        );
        for builtin in [builtin_to_int, builtin_to_float] {
            assert!(matches!(
                refusal(builtin(&[ValClos::bool(true)])).cause,
                InvalidCause::Shape(_)
            ));
        }
    }

    #[test]
    fn codec_refusals_distinguish_quotes_regex_and_literal_matches() {
        assert_eq!(
            refusal(builtin_posix_split(&[ValClos::string("'")])).cause,
            InvalidCause::Codec(CodecFailure::PosixSplit)
        );
        assert_eq!(
            refusal(builtin_posix_quote(&[ValClos::string("\0")])).cause,
            InvalidCause::Codec(CodecFailure::PosixQuoteNul)
        );
        let invalid = builtin_match(&[ValClos::string("["), ValClos::string("x")]);
        assert!(
            matches!(refusal(invalid).cause, InvalidCause::Codec(CodecFailure::Regex(crate::fact::RegexFailure { pattern, cause: crate::fact::RegexCause::Syntax(_) })) if pattern == "[")
        );
        for (from, input, matches) in [("z", "abc", 0usize), ("aa", "aaa", 2usize)] {
            assert_eq!(
                refusal(builtin_string_replace(&[
                    ValClos::string(from),
                    ValClos::string("x"),
                    ValClos::string(input)
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
                ValClos::string(""),
                ValClos::string("x"),
                ValClos::string("abc")
            ]))
            .cause,
            InvalidCause::Codec(CodecFailure::EmptyReplacementPattern)
        );
        for (input, matches) in [("x", 0usize), ("aa", 2usize)] {
            assert!(
                matches!(refusal(builtin_replace(&[ValClos::string("a"), ValClos::string("x"), ValClos::string(input)])).cause,
                InvalidCause::Codec(CodecFailure::MatchCount(f)) if f.kind == LiteralOrRegex::Regex && f.matches == matches.into())
            );
        }
        assert!(
            matches!(refusal(builtin_find_match(&[ValClos::string("a"), ValClos::string("x")])).cause,
            InvalidCause::Codec(CodecFailure::MatchCount(f)) if f.matches == 0usize.into())
        );
    }
}
