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
use crate::syntax::lexer::fence_admits;
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

/// One level of a value, its children unread.
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
        label: &'a str,
        payload: Option<&'a V>,
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
            Self::Variant { label, payload } => Shape::Variant {
                label,
                payload: payload.as_deref(),
            },
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
                format!("{k}: {}", child(&*v, indent, params, budget))
            });
            parts.extend(more);
            bracketed(&parts, indent, params)
        }
        Shape::Variant {
            label,
            payload: Some(p),
        } => format!("`{label} {}", full(p, indent, params, budget)),
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
    let records = (items.iter())
        .map(|v| match v.shape() {
            Shape::Map { len, entries } if len > 0 => Some(entries.collect::<Vec<_>>()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let first = records[0].iter().map(|(k, _)| k);
    if (records.iter()).any(|r| !r.iter().map(|(k, _)| k).eq(first.clone())) {
        return None;
    }
    let before = budget.get();
    let (rows, more) = budgeted(
        records.len(),
        records.iter(),
        budget,
        |cells: &Vec<String>| cells.iter().map(String::len).sum(),
        |record| {
            (record.iter())
                .map(|(k, v)| format!("{k}: {}", child(&**v, indent + 1, params, budget)))
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
/// to its cardinality. Recursion runs only through `Variant` payloads, which
/// carry no container of their own, so no chain of records can outrun the depth
/// bound.
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
        Shape::Variant { label, payload } => match payload {
            None => format!("`{label}"),
            Some(p) => format!("`{label} {}", shallow(p, params)),
        },
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
