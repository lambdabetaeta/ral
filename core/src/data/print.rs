//! A value as bracketed, quote-fenced ral surface syntax.
//!
//! One printer reads first-order data and the shell's runtime values alike,
//! through a [`Printable`] view. One policy serves every reader (the REPL,
//! exarch's tool-result `VALUE` section, a grant shown to its agent); they
//! differ only in numbers — how wide the reader's window is, which quote form
//! its parser accepts, how many bytes it can absorb.
//!
//! The policy is that **truncation preserves identity**: at the depth limit a
//! record keeps its keys and a list keeps its heads, a long string keeps its
//! head and tail around a marker, and a spent budget names how many children it
//! dropped. A bare count is the rendering of last resort, for the floor beneath
//! the depth limit, where there is no room left to say anything else.
//!
//! **Rows.** A list of two or more records sharing one key sequence prints one
//! record per line, each `label: value` cell padded to its column's widest, so
//! the records read as a table and still paste back as the same list.  A cell
//! that would break across lines leaves the list to the ordinary layout.

use super::FOValue;
use crate::syntax::lexer::{fence_admits, is_bidi_control, is_ident};
use crate::syntax::quote::quote_word;
use std::borrow::Cow;
use std::cell::Cell;
use std::ops::Deref;

/// Tuning knobs for [`pretty_print`].
pub struct PrintParams {
    /// Char width above which a bracketed `List`/`Map` breaks across lines; the
    /// leading indent does not count toward it.
    pub max_width: usize,
    /// Elide the middle of a string held *inside* a structure past this many
    /// **characters** — a measure of how much text a reader takes in, not of a
    /// wire; `0` keeps them whole.
    pub max_string: usize,
    /// `List`/`Map` nesting depth past which a container renders its children
    /// in shallow form: keys and heads, and counts below them.
    pub max_depth: usize,
    /// Floor on the `#` fence count around a quoted string; `0` allows the
    /// minimal, possibly unfenced form.
    pub min_quote_hashes: usize,
    /// Quote-fence a nested `Bytes` like a `String`, rather than emitting its
    /// lossy text raw.
    pub quote_bytes: bool,
    /// **Byte** budget for rendered children: once it is spent, each container
    /// closes with `…N more` instead of rendering the rest. Bytes because the
    /// caps downstream are byte caps and `String::len` already counts them,
    /// where `max_string` counts characters. It bounds rendered *content*, not
    /// the finished string: [`bracketed`] chooses its layout once its children
    /// exist, so indentation is padded on behind the budget's back.
    pub max_bytes: usize,
}

/// What the printer reads of a value.
pub trait Printable {
    /// A child as its container hands it out, borrowed or built.
    type Child<'a>: Deref<Target = Self>
    where
        Self: 'a;

    fn shape(&self) -> Shape<'_, Self>;
}

/// A value's head, grouping consecutive tags before their nonvariant terminal.
pub enum Shape<'a, V: Printable + ?Sized + 'a> {
    String(&'a str),
    Bytes(&'a [u8]),
    List {
        len: usize,
        items: Box<dyn Iterator<Item = V::Child<'a>> + 'a>,
    },
    Map {
        len: usize,
        entries: Box<dyn Iterator<Item = (&'a str, V::Child<'a>)> + 'a>,
    },
    Variant {
        /// Nonempty consecutive tags; the payload, if any, is not a variant.
        labels: Vec<&'a str>,
        payload: Option<V::Child<'a>>,
    },
    /// A scalar, or a leaf that is not data, as its text.
    Atom(Cow<'a, str>),
}

impl Printable for FOValue {
    type Child<'a> = &'a Self;

    fn shape(&self) -> Shape<'_, Self> {
        match self {
            Self::Unit => Shape::Atom("()".into()),
            Self::Bool { value } => Shape::Atom(value.to_string().into()),
            Self::Int { value } => Shape::Atom(value.to_string().into()),
            Self::Float { value } => Shape::Atom(value.to_string().into()),
            Self::String { value } => Shape::String(value),
            Self::Bytes { value } => Shape::Bytes(value),
            Self::List { items } => Shape::List {
                len: items.len(),
                items: Box::new(items.iter()),
            },
            Self::Map { entries } => Shape::Map {
                len: entries.len(),
                entries: Box::new(entries.iter().map(|(k, v)| (k.as_str(), v))),
            },
            Self::Variant { label, payload } => {
                let mut labels = vec![label.as_str()];
                let mut payload = payload.as_deref();
                while let Some(Self::Variant {
                    label,
                    payload: next,
                }) = payload
                {
                    labels.push(label.as_str());
                    payload = next.as_deref();
                }
                Shape::Variant { labels, payload }
            }
            #[allow(
                clippy::uninhabited_references,
                reason = "no value of `NoExt` exists to be read"
            )]
            Self::Ext(x) => match *x {},
        }
    }
}

pub fn pretty_print<V: Printable + ?Sized>(val: &V, indent: usize, params: &PrintParams) -> String {
    full(val, indent, params, &Cell::new(params.max_bytes))
}

/// The full rendering: containers expand their children, and a string that *is*
/// the value is content rather than a field. Everything else renders as its
/// [`shallow`] form, which the two levels agree on.
fn full<V: Printable + ?Sized>(
    val: &V,
    indent: usize,
    params: &PrintParams,
    budget: &Cell<usize>,
) -> String {
    match val.shape() {
        // A string that is the whole value prints whole; any string inside a
        // structure is a field, and elides — see the `shallow` arms.
        Shape::String(s) if indent == 0 => quote_string(s, params),
        Shape::Bytes(b) if indent == 0 && params.quote_bytes => {
            quote_string(&String::from_utf8_lossy(b), params)
        }
        Shape::List { len, items } if len > 0 => {
            if len < 2 || indent >= params.max_depth {
                return list(len, items, indent, params, budget);
            }
            let items: Vec<_> = items.collect();
            rows::<V>(&items, indent, params, budget)
                .unwrap_or_else(|| list(len, items.iter().map(|v| &**v), indent, params, budget))
        }
        Shape::Map { len, entries } if len > 0 => {
            let (mut parts, more) = budgeted(len, entries, budget, String::len, |(k, v)| {
                format!(
                    "{}: {}",
                    label(k, params),
                    child(&*v, indent, params, budget)
                )
            });
            parts.extend(more);
            bracketed(&parts, indent, params)
        }
        Shape::Variant { labels, payload } => {
            let mut text = String::new();
            for label in labels {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push('`');
                text.push_str(label);
            }
            if let Some(p) = payload {
                text.push(' ');
                text.push_str(&full(&*p, indent, params, budget));
            }
            text
        }
        _ => shallow(val, params),
    }
}

/// A list's children, bracketed, within `budget`.
fn list<V: Printable + ?Sized, C: Deref<Target = V>>(
    len: usize,
    items: impl Iterator<Item = C>,
    indent: usize,
    params: &PrintParams,
    budget: &Cell<usize>,
) -> String {
    let (mut parts, more) = budgeted(len, items, budget, String::len, |v| {
        child(&*v, indent, params, budget)
    });
    parts.extend(more);
    bracketed(&parts, indent, params)
}

/// The rows layout (see the module doc): `None` unless `items` are records
/// with one key sequence, rendered in full at this depth, and every cell stays
/// on one line.  A refused layout refunds what it charged.
fn rows<V: Printable + ?Sized>(
    items: &[V::Child<'_>],
    indent: usize,
    params: &PrintParams,
    budget: &Cell<usize>,
) -> Option<String> {
    if !matches!(items.first()?.shape(), Shape::Map { len, .. } if len > 0) {
        return None;
    }
    // Recreate entry views for comparison; retaining every row's cells here
    // would allocate for rows the rendering budget will never reach.
    if items
        .iter()
        .skip(1)
        .any(|v| match (items[0].shape(), v.shape()) {
            (Shape::Map { entries: first, .. }, Shape::Map { entries, .. }) => {
                !entries.map(|(k, _)| k).eq(first.map(|(k, _)| k))
            }
            _ => true,
        })
    {
        return None;
    }
    let before = budget.get();
    let (rows, more) = budgeted(
        items.len(),
        items.iter(),
        budget,
        |cells: &Vec<String>| cells.iter().map(String::len).sum(),
        |record| {
            let Shape::Map { entries, .. } = record.shape() else {
                unreachable!("rows have already been checked to be records");
            };
            entries
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        label(k, params),
                        child(&*v, indent + 1, params, budget)
                    )
                })
                .collect()
        },
    );
    if rows.iter().flatten().any(|cell| cell.contains('\n')) {
        budget.set(before);
        return None;
    }
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|j| rows.iter().map(|r| r[j].chars().count()).max().unwrap_or(0))
        .collect();
    let mut lines: Vec<String> = rows
        .iter()
        .map(|cells| {
            let (last, init) = cells.split_last().expect("a record has a key");
            let padded: Vec<String> = init
                .iter()
                .zip(&widths)
                .map(|(cell, &w)| format!("{:<w$}", format!("{cell},"), w = w + 1))
                .chain([last.clone()])
                .collect();
            format!("[{}]", padded.join(" "))
        })
        .collect();
    lines.extend(more);
    Some(stacked(&lines, indent))
}

/// A container's child: full one level deeper, or shallow once `max_depth` is
/// reached. The summary a depth-limited container gives is therefore its
/// children's keys and heads, and only what lies beneath them collapses.
fn child<V: Printable + ?Sized>(
    val: &V,
    indent: usize,
    params: &PrintParams,
    budget: &Cell<usize>,
) -> String {
    if indent >= params.max_depth {
        shallow(val, params)
    } else {
        full(val, indent + 1, params, budget)
    }
}

/// The floor: scalars verbatim, a string cut to its head and tail, a container
/// to its cardinality. Grouped tags render their nonvariant terminal once.
fn shallow<V: Printable + ?Sized>(val: &V, params: &PrintParams) -> String {
    match val.shape() {
        Shape::String(s) => quote_string(&elide(s, params.max_string), params),
        Shape::Bytes(b) if params.quote_bytes => quote_string(
            &elide(&String::from_utf8_lossy(b), params.max_string),
            params,
        ),
        Shape::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Shape::List { len: 0, .. } => "[]".into(),
        Shape::List { len, .. } => format!("[...{len} items]"),
        Shape::Map { len: 0, .. } => "[:]".into(),
        Shape::Map { len, .. } => format!("[:...{len} pairs]"),
        Shape::Variant { labels, payload } => {
            let mut text = String::new();
            for label in labels {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push('`');
                text.push_str(label);
            }
            if let Some(p) = payload {
                text.push(' ');
                text.push_str(&shallow(&*p, params));
            }
            text
        }
        Shape::Atom(text) => text.into_owned(),
    }
}

/// Render `count` children, stopping once `budget` is spent — but never before
/// the first, since a marker alone tells the reader nothing — and return them
/// with the `…N more` marker for the rest, if any.  Charging a finished part's
/// `size` against the budget as it stood *before* rendering unwinds whatever
/// its own children charged, so every byte is paid for once, by the outermost
/// container that owns it.
fn budgeted<T, P>(
    count: usize,
    items: impl Iterator<Item = T>,
    budget: &Cell<usize>,
    size: impl Fn(&P) -> usize,
    mut render: impl FnMut(T) -> P,
) -> (Vec<P>, Option<String>) {
    let mut parts = Vec::with_capacity(count);
    for (i, item) in items.enumerate() {
        if i > 0 && budget.get() == 0 {
            return (parts, Some(format!("…{} more", count - i)));
        }
        let before = budget.get();
        let part = render(item);
        budget.set(before.saturating_sub(size(&part)));
        parts.push(part);
    }
    (parts, None)
}

/// Labels use the parser's identifier alphabet, not the broader bare-word
/// grammar. Quoted keys stay whole, regardless of the string elision limit.
fn label<'a>(key: &'a str, params: &PrintParams) -> Cow<'a, str> {
    if is_ident(key) {
        Cow::Borrowed(key)
    } else if key.chars().any(is_bidi_control) {
        // Raw quotes cannot carry bidi controls; the shared word quoter
        // escapes them in a non-interpolating double-quoted string.
        quote_word(key)
    } else {
        Cow::Owned(quote_string(key, params))
    }
}

fn quote_string(body: &str, params: &PrintParams) -> String {
    let mut level = params.min_quote_hashes;
    while !fence_admits(body, level) {
        level += 1;
    }
    let hashes = "#".repeat(level);
    format!("{hashes}'{body}'{hashes}")
}

/// Elide the middle of `s` to a `budget`-char head and tail around an
/// `[…elided N characters…]` marker. Each stops at its nearest newline too, so a
/// long multi-line string keeps its first and last physical line. A cut that
/// does not shorten is not a cut: `s` returns whole when the marker would cost
/// more than the text it hides — the marker being 25 characters wide, nothing
/// within 26 characters of `budget` is touched at all — and when `budget` is
/// `0`.
fn elide(s: &str, budget: usize) -> String {
    if budget == 0 {
        return s.to_string();
    }
    let total = s.chars().count();
    let head_budget = budget / 2;
    let tail_budget = budget - head_budget;
    let head: String = s
        .chars()
        .take_while(|&c| c != '\n')
        .take(head_budget)
        .collect();
    let tail: String = {
        let rev: String = s
            .chars()
            .rev()
            .take_while(|&c| c != '\n')
            .take(tail_budget)
            .collect();
        rev.chars().rev().collect()
    };
    let elided = total
        .saturating_sub(head.chars().count())
        .saturating_sub(tail.chars().count());
    let marked = format!("{head} […elided {elided} characters…] {tail}");
    if marked.chars().count() < total {
        marked
    } else {
        s.to_string()
    }
}

fn bracketed(parts: &[String], indent: usize, params: &PrintParams) -> String {
    let inline = format!("[{}]", parts.join(", "));
    if inline.chars().count() <= params.max_width && !inline.contains('\n') {
        return inline;
    }
    stacked(parts, indent)
}

/// One part per line, a level deeper than the brackets.
fn stacked(parts: &[String], indent: usize) -> String {
    let pad = "  ".repeat(indent + 1);
    let end_pad = "  ".repeat(indent);
    format!("[\n{pad}{}\n{end_pad}]", parts.join(&format!(",\n{pad}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::ast::{Ast, ListElem, RecordEntry};
    use crate::syntax::parser::parse;
    use std::rc::Rc;

    fn params() -> PrintParams {
        PrintParams {
            max_width: 80,
            max_string: 4,
            max_depth: 3,
            min_quote_hashes: 0,
            quote_bytes: false,
            max_bytes: 64 * 1024,
        }
    }

    fn record(key: &str, value: i64) -> FOValue {
        FOValue::Map {
            entries: vec![(key.to_owned(), FOValue::Int { value })],
        }
    }

    fn printed_ast(value: &FOValue, params: &PrintParams) -> Ast {
        let text = pretty_print(value, 0, params);
        let mut program = parse(&format!("return {text}")).expect("printed value must parse");
        let Ast::Return(Some(value)) = program.remove(0).item else {
            panic!("expected a returned value");
        };
        *value.item
    }

    fn record_keys(ast: &Ast) -> Vec<&str> {
        let Ast::Record(fields) = ast else {
            panic!("expected a record, got {ast:?}");
        };
        fields
            .iter()
            .map(|field| match field {
                RecordEntry::Field { key, .. } => key.item.as_str(),
                RecordEntry::Spread(_) => panic!("printed records cannot spread"),
            })
            .collect()
    }

    #[test]
    fn variants_keep_optional_payloads_in_full_and_shallow_forms() {
        for (payload, full, shallow_text) in [
            (None, "`tag", "`tag"),
            (Some(FOValue::Unit), "`tag ()", "`tag ()"),
            (
                Some(FOValue::String {
                    value: "a#'b".into(),
                }),
                "`tag ##'a#'b'##",
                "`tag ##'a#'b'##",
            ),
            (
                Some(FOValue::List {
                    items: vec![FOValue::Int { value: 7 }],
                }),
                "`tag [7]",
                "`tag [...1 items]",
            ),
        ] {
            let value = FOValue::Variant {
                label: "tag".into(),
                payload: payload.map(Box::new),
            };
            assert_eq!(pretty_print(&value, 0, &params()), full);
            assert_eq!(shallow(&value, &params()), shallow_text);
            let mut value = (0..10_000).fold(value, |payload, _| FOValue::Variant {
                label: "outer".into(),
                payload: Some(Box::new(payload)),
            });
            let prefix = "`outer ".repeat(10_000);
            assert_eq!(
                pretty_print(&value, 0, &params()),
                format!("{prefix}{full}")
            );
            assert_eq!(
                shallow(&value, &params()),
                format!("{prefix}{shallow_text}")
            );
            while let FOValue::Variant {
                payload: Some(payload),
                ..
            } = value
            {
                value = *payload;
            }
        }
    }

    #[test]
    fn record_and_table_labels_paste_back_without_elision() {
        let long_key = format!("unsafe key {} tail", "x".repeat(100));
        for key in [
            "",
            "x: 1, y",
            "a,b",
            "a.b",
            "path/to/key",
            "1",
            "-1",
            "true",
            "false",
            "if",
            "a-b_c",
            "_",
            "é",
            "#comment",
            "...",
            "$key",
            "~/key",
            "it's",
            "a#'b",
            "a'#b#'c",
            "line\nbreak",
            "~$!\u{202e}",
            &long_key,
        ] {
            for floor in [0, 2] {
                let params = PrintParams {
                    min_quote_hashes: floor,
                    ..params()
                };
                assert_eq!(record_keys(&printed_ast(&record(key, 1), &params)), [key]);
                let table = FOValue::List {
                    items: vec![record(key, 1), record(key, 12)],
                };
                let Ast::List(rows) = printed_ast(&table, &params) else {
                    panic!("expected a list");
                };
                assert_eq!(rows.len(), 2);
                for row in rows {
                    let ListElem::Single(row) = row else {
                        panic!("printed lists cannot spread");
                    };
                    assert_eq!(record_keys(&row.item), [key]);
                }
            }
        }
    }

    #[test]
    fn unsafe_labels_keep_record_and_padded_table_layouts() {
        let row = |n| FOValue::Map {
            entries: vec![
                ("x: 1, y".into(), FOValue::Int { value: n }),
                ("z".into(), FOValue::Int { value: 2 }),
            ],
        };
        assert_eq!(pretty_print(&row(1), 0, &params()), "['x: 1, y': 1, z: 2]");
        assert_eq!(
            pretty_print(
                &FOValue::List {
                    items: vec![row(1), row(12)]
                },
                0,
                &params()
            ),
            "[\n  ['x: 1, y': 1,  z: 2],\n  ['x: 1, y': 12, z: 2]\n]"
        );
        assert_eq!(
            label(
                "a.b",
                &PrintParams {
                    min_quote_hashes: 2,
                    ..params()
                }
            ),
            "##'a.b'##"
        );
    }

    #[derive(Default)]
    struct EntryCounts {
        live: Cell<usize>,
        peak: Cell<usize>,
        rendered: Cell<usize>,
    }

    struct EntryProbe {
        cells: usize,
        counts: Rc<EntryCounts>,
    }

    struct EntryView(EntryProbe);

    impl Deref for EntryView {
        type Target = EntryProbe;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Drop for EntryView {
        fn drop(&mut self) {
            if self.cells == 0 {
                self.counts.live.set(self.counts.live.get() - 1);
            }
        }
    }

    impl Printable for EntryProbe {
        type Child<'a> = EntryView;

        fn shape(&self) -> Shape<'_, Self> {
            if self.cells == 0 {
                self.counts.rendered.set(self.counts.rendered.get() + 1);
                return Shape::Atom("1".into());
            }
            Shape::Map {
                len: self.cells,
                entries: Box::new(
                    ["a", "b", "c", "d"]
                        .into_iter()
                        .take(self.cells)
                        .map(|key| {
                            self.counts.live.set(self.counts.live.get() + 1);
                            self.counts
                                .peak
                                .set(self.counts.peak.get().max(self.counts.live.get()));
                            (
                                key,
                                EntryView(Self {
                                    cells: 0,
                                    counts: Rc::clone(&self.counts),
                                }),
                            )
                        }),
                ),
            }
        }
    }

    #[test]
    fn rows_budget_does_not_retain_omitted_cell_views() {
        let counts = Rc::new(EntryCounts::default());
        let items: Vec<_> = (0..128)
            .map(|_| {
                EntryView(EntryProbe {
                    cells: 4,
                    counts: Rc::clone(&counts),
                })
            })
            .collect();
        for bytes in [0, 1] {
            counts.peak.set(0);
            counts.rendered.set(0);
            let params = PrintParams {
                max_bytes: bytes,
                max_depth: 1,
                ..params()
            };
            let budget = Cell::new(params.max_bytes);
            assert_eq!(
                rows::<EntryProbe>(&items, 0, &params, &budget).expect("uniform records form rows"),
                "[\n  [a: 1, b: 1, c: 1, d: 1],\n  …127 more\n]"
            );
            assert_eq!(counts.live.get(), 0);
            assert!(
                counts.peak.get() <= 2,
                "discovery must drop cell views as it compares keys"
            );
            assert_eq!(
                counts.rendered.get(),
                4,
                "only the mandatory first row renders"
            );
            assert_eq!(budget.get(), 0);
        }
    }

    #[test]
    fn raw_strings_round_trip_at_the_smallest_admitted_fence() {
        use crate::syntax::lexer::{Lexeme, Token, lex};

        let base = PrintParams {
            max_width: 80,
            max_string: 0,
            max_depth: 3,
            min_quote_hashes: 0,
            quote_bytes: false,
            max_bytes: 64 * 1024,
        };
        for body in ["", "plain", "it's", "a#'b", "a'#b#'c", "#'", "'#"] {
            for floor in 0..=4 {
                let params = PrintParams {
                    min_quote_hashes: floor,
                    ..base
                };
                let spelling = quote_string(body, &params);
                let level = spelling.bytes().take_while(|&b| b == b'#').count();
                assert!(level >= floor);
                assert!((floor..level).all(|n| !fence_admits(body, n)));
                let tokens = lex(&spelling).expect("printed string must lex");
                assert!(
                    matches!(tokens.as_slice(), [Lexeme { token: Token::SingleQuoted(s), .. }, Lexeme { token: Token::Eof, .. }] if s == body),
                    "{spelling:?}: {tokens:?}"
                );
            }
        }
        assert_eq!(quote_string("a#'b", &base), "##'a#'b'##");
    }
}
