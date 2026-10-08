//! The numeral grammar, in one place: the lexer scans by it and
//! [`WordLiteral::classify`](super::ast::WordLiteral::classify) decides by it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    Int,
    Float,
}

fn digits(b: &[u8]) -> usize {
    b.iter().take_while(|c| c.is_ascii_digit()).count()
}

/// Longest numeral prefix of `s`: its byte length and shape.
///   Int   = sign? digits
///   Float = sign? (digits . digits? | . digits) (e sign? digits)?
/// A float wants a point: `1e6` has prefix `1`, an Int.
pub(crate) fn prefix(s: &str) -> Option<(usize, Shape)> {
    let b = s.as_bytes();
    let sign = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let int = digits(&b[sign..]);
    let at = sign + int;
    if b.get(at) != Some(&b'.') {
        return (int > 0).then_some((at, Shape::Int));
    }
    let frac = digits(&b[at + 1..]);
    if int + frac == 0 {
        return None;
    }
    let mut end = at + 1 + frac;
    if matches!(b.get(end), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(b.get(end + 1), Some(b'+' | b'-')));
        let exp = digits(&b[end + 1 + sign..]);
        if exp > 0 {
            end += 1 + sign + exp;
        }
    }
    Some((end, Shape::Float))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::ast::WordLiteral;

    #[test]
    fn prefix_reads_the_longest_numeral() {
        use Shape::{Float, Int};
        for (s, want) in [
            ("5", Some((1, Int))),
            ("007", Some((3, Int))),
            ("-0", Some((2, Int))),
            ("1abc", Some((1, Int))),
            ("1e6", Some((1, Int))),
            ("2.", Some((2, Float))),
            (".5", Some((2, Float))),
            ("+.5", Some((3, Float))),
            ("1.50", Some((4, Float))),
            ("1.5e+3-.5", Some((6, Float))),
            ("1.5e", Some((3, Float))),
            ("1.5e+", Some((3, Float))),
            ("1.0e300", Some((7, Float))),
            ("3.10.1", Some((4, Float))),
            (".", None),
            ("-", None),
            (".e5", None),
            ("v1", None),
            ("", None),
        ] {
            assert_eq!(prefix(s), want, "{s:?}");
        }
    }

    #[test]
    fn classify_accepts_what_the_grammar_covers() {
        for s in [
            "5", "007", "+5", "-0", ".5", "2.", "1.50", "3.10", "1.0e300",
        ] {
            assert!(
                matches!(
                    WordLiteral::classify(s),
                    Some(WordLiteral::Int(_) | WordLiteral::Float(_))
                ),
                "{s:?} is a numeral"
            );
        }
    }

    #[test]
    fn classify_declines_the_rest() {
        for s in [
            "1e6",
            "1_000",
            "0x10",
            "v1.50",
            "3.10.1",
            "007a",
            "9223372036854775808",
            "1.0e999",
        ] {
            assert!(WordLiteral::classify(s).is_none(), "{s:?} stays a word");
        }
    }
}
