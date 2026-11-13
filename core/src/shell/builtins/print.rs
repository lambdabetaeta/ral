//! The shell's runtime values through [`data::print`](crate::data::print),
//! the one value printer, and the REPL's numbers for it.

use crate::data::print::{PrintParams, Printable, Shape};
use crate::ir::{VClosRef, View};
use std::borrow::Cow;

pub const REPL_PRINT_PARAMS: PrintParams = PrintParams {
    max_width: 80,
    max_string: 72,
    max_depth: 3,
    min_quote_hashes: 0,
    quote_bytes: false,
    max_bytes: 64 * 1024,
};

/// A leaf that is not data prints as its `Display`, a handle as `<handle>`.
impl Printable for VClosRef<'_> {
    type Child<'a>
        = Cow<'a, Self>
    where
        Self: 'a;

    fn shape(&self) -> Shape<'_, Self> {
        let (labels, terminal) = self.peel_variants();
        if !labels.is_empty() {
            return Shape::Variant {
                labels: labels.into_iter().map(AsRef::as_ref).collect(),
                payload: (!terminal.is_unit()).then_some(Cow::Owned(terminal)),
            };
        }
        match self.view() {
            View::String(s) => Shape::String(s.as_str()),
            View::Bytes(b) => Shape::Bytes(b),
            View::List(items) => Shape::List {
                len: items.len(),
                items: Box::new(items.iter().map(Cow::Owned)),
            },
            View::Map(pairs) => Shape::Map {
                len: pairs.len(),
                entries: Box::new(pairs.iter().map(|(key, value)| (key, Cow::Owned(value)))),
            },
            View::Variant { .. } => unreachable!("consecutive tags were peeled above"),
            View::Handle(_) => Shape::Atom("<handle>".into()),
            _ => Shape::Atom(self.to_string().into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::print::pretty_print;
    use crate::ir::ValClos;

    fn record(fields: &[(&str, ValClos)]) -> ValClos {
        ValClos::map(fields.iter().map(|(k, v)| ((*k).to_string(), v.clone())))
    }

    fn print(v: &ValClos) -> String {
        pretty_print(&VClosRef::from(v), 0, &REPL_PRINT_PARAMS)
    }

    #[test]
    fn uniform_records_print_as_rows() {
        let row = |id, title: &str| {
            record(&[("id", ValClos::int(id)), ("title", ValClos::string(title))])
        };
        assert_eq!(
            print(&ValClos::list(vec![
                row(7, "build"),
                row(12, "lint the tree")
            ])),
            "[\n  [id: 7,  title: 'build'],\n  [id: 12, title: 'lint the tree']\n]"
        );
    }

    #[test]
    fn variant_payloads_print_whether_syntactic_or_built() {
        let mut shell = crate::test_helper::core_shell();
        let literal =
            crate::shell::evaluator::run_source("let u = (); return [`none $u]", &mut shell)
                .expect("a variant over a captured unit");
        assert_eq!(print(&literal), "[`none]");
        assert_eq!(print(&ValClos::variant("none", None)), "`none");
        for (payload, full, shallow) in [
            (Some(ValClos::UNIT), "`some", "`some"),
            (Some(ValClos::int(7)), "`some 7", "`some 7"),
            (
                Some(ValClos::string("a#'b")),
                "`some ##'a#'b'##",
                "`some ##'a#'b'##",
            ),
            (
                Some(ValClos::list(vec![ValClos::int(7)])),
                "`some [7]",
                "`some [...1 items]",
            ),
        ] {
            let value = ValClos::variant("some", payload);
            assert_eq!(print(&value), full);
            let value =
                (0..10_000).fold(value, |payload, _| ValClos::variant("outer", Some(payload)));
            let prefix = "`outer ".repeat(10_000);
            assert_eq!(print(&value), format!("{prefix}{full}"));
            assert_eq!(
                pretty_print(
                    &VClosRef::from(&ValClos::list(vec![value])),
                    0,
                    &PrintParams {
                        max_depth: 0,
                        ..REPL_PRINT_PARAMS
                    }
                ),
                format!("[\n  {prefix}{shallow}\n]")
            );
        }
        let tagged = || {
            ValClos::variant(
                "row",
                Some(record(&[("id", ValClos::int(7)), ("n", ValClos::int(12))])),
            )
        };
        assert_eq!(
            pretty_print(
                &VClosRef::from(&ValClos::list(vec![tagged(), tagged()])),
                0,
                &PrintParams {
                    max_width: 17,
                    ..REPL_PRINT_PARAMS
                }
            ),
            "[\n  `row [id: 7, n: 12],\n  `row [id: 7, n: 12]\n]"
        );
    }

    /// Keys that differ, a lone record, and a cell that breaks a line all
    /// keep the ordinary layout.
    #[test]
    fn mixed_records_print_as_before() {
        let a = record(&[("a", ValClos::int(1))]);
        let b = record(&[("b", ValClos::int(2))]);
        assert_eq!(
            print(&ValClos::list(vec![a.clone(), b])),
            "[[a: 1], [b: 2]]"
        );
        assert_eq!(print(&ValClos::list(vec![a])), "[[a: 1]]");
        let s = |text: &str| record(&[("s", ValClos::string(text))]);
        assert_eq!(
            print(&ValClos::list(vec![s("a\nb"), s("c")])),
            "[\n  [\n    s: 'a\nb'\n  ],\n  [s: 'c']\n]"
        );
    }
}
