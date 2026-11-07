//! The shell's runtime values through [`data::print`](crate::data::print),
//! the one value printer, and the REPL's numbers for it.

use crate::data::print::{PrintParams, Printable, Shape};
use crate::shell::runtime::Value;
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
impl Printable for Value {
    type Child<'a> = Cow<'a, Self>;

    fn shape(&self) -> Shape<'_, Self> {
        match self {
            Self::String(s) => Shape::String(s.as_str()),
            Self::Bytes(b) => Shape::Bytes(b),
            Self::List(items) => Shape::List {
                len: items.len(),
                items: Box::new(items.iter()),
            },
            Self::Map(pairs) => Shape::Map {
                len: pairs.len(),
                entries: Box::new(pairs.iter()),
            },
            Self::Variant { label, payload } => Shape::Variant {
                label,
                payload: payload.as_deref(),
            },
            Self::Handle(_) => Shape::Atom("<handle>".into()),
            other => Shape::Atom(other.to_string().into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::print::pretty_print;

    fn record(fields: &[(&str, Value)]) -> Value {
        Value::Map(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    fn print(v: &Value) -> String {
        pretty_print(v, 0, &REPL_PRINT_PARAMS)
    }

    #[test]
    fn uniform_records_print_as_rows() {
        let row = |id, title: &str| {
            record(&[
                ("id", Value::Int(id)),
                ("title", Value::String(title.into())),
            ])
        };
        assert_eq!(
            print(&Value::list(vec![
                row(7, "build"),
                row(12, "lint the tree")
            ])),
            "[\n  [id: 7,  title: 'build'],\n  [id: 12, title: 'lint the tree']\n]"
        );
    }

    /// Keys that differ, a lone record, and a cell that breaks a line all
    /// keep the ordinary layout.
    #[test]
    fn mixed_records_print_as_before() {
        let a = record(&[("a", Value::Int(1))]);
        let b = record(&[("b", Value::Int(2))]);
        assert_eq!(print(&Value::list(vec![a.clone(), b])), "[[a: 1], [b: 2]]");
        assert_eq!(print(&Value::list(vec![a])), "[[a: 1]]");
        let s = |text: &str| record(&[("s", Value::String(text.into()))]);
        assert_eq!(
            print(&Value::list(vec![s("a\nb"), s("c")])),
            "[\n  [\n    s: 'a\nb'\n  ],\n  [s: 'c']\n]"
        );
    }
}
