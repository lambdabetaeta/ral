//! Renders a [`Value`] as bracketed, quote-fenced ral surface syntax — a
//! utility, not a registered builtin. One policy serves both readers: the REPL
//! (`ral/src/repl/exec.rs`) and exarch's tool-result `VALUE` section
//! (`exarch/src/shell_eval.rs`) agree on what to show and differ only in
//! numbers — how wide the reader's window is, which quote form its parser
//! accepts, how many bytes it can absorb.
//!
//! The policy is that **truncation preserves identity**: at the depth limit a
//! record keeps its keys and a list keeps its heads, a long string keeps its
//! head and tail around a marker, and a spent budget names how many children it
//! dropped. A bare count is the rendering of last resort, for the floor beneath
//! the depth limit, where there is no room left to say anything else.

use crate::types::Value;
use std::cell::Cell;

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

pub const REPL_PRINT_PARAMS: PrintParams = PrintParams {
    max_width: 80,
    max_string: 72,
    max_depth: 3,
    min_quote_hashes: 0,
    quote_bytes: false,
    max_bytes: 64 * 1024,
};

pub fn pretty_print(val: &Value, indent: usize, params: &PrintParams) -> String {
    full(val, indent, params, &Cell::new(params.max_bytes))
}

/// The full rendering: containers expand their children, and a string that *is*
/// the value is content rather than a field. Everything else renders as its
/// [`shallow`] form, which the two levels agree on.
fn full(val: &Value, indent: usize, params: &PrintParams, budget: &Cell<usize>) -> String {
    match val {
        // A string that is the whole value prints whole; any string inside a
        // structure is a field, and elides — see the `shallow` arms.
        Value::String(s) if indent == 0 => quote_string(s, params),
        Value::Bytes(b) if indent == 0 && params.quote_bytes => {
            quote_string(&String::from_utf8_lossy(b), params)
        }
        Value::List(items) if !items.is_empty() => {
            let parts = budgeted(items.len(), items.iter(), budget, |v| {
                child(v, indent, params, budget)
            });
            bracketed(&parts, indent, "[", "]", params)
        }
        Value::Map(pairs) if !pairs.is_empty() => {
            let parts = budgeted(pairs.len(), pairs.iter(), budget, |(k, v)| {
                format!("{k}: {}", child(v, indent, params, budget))
            });
            bracketed(&parts, indent, "[", "]", params)
        }
        Value::Variant {
            label,
            payload: Some(p),
        } => format!("`{label} {}", full(p, indent, params, budget)),
        other => shallow(other, params),
    }
}

/// A container's child: full one level deeper, or shallow once `max_depth` is
/// reached. The summary a depth-limited container gives is therefore its
/// children's keys and heads, and only what lies beneath them collapses.
fn child(val: &Value, indent: usize, params: &PrintParams, budget: &Cell<usize>) -> String {
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
fn shallow(val: &Value, params: &PrintParams) -> String {
    match val {
        Value::String(s) => quote_string(&elide(s, params.max_string), params),
        Value::Bytes(b) if params.quote_bytes => quote_string(
            &elide(&String::from_utf8_lossy(b), params.max_string),
            params,
        ),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::List(items) if items.is_empty() => "[]".into(),
        Value::List(items) => format!("[...{} items]", items.len()),
        Value::Map(pairs) if pairs.is_empty() => "[:]".into(),
        Value::Map(pairs) => format!("[:...{} pairs]", pairs.len()),
        Value::Variant { label, payload } => match payload {
            None => format!("`{label}"),
            Some(p) => format!("`{label} {}", shallow(p, params)),
        },
        Value::Unit => "unit".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Handle(_) => "<handle>".into(),
        Value::Lambda { param, body, .. } => crate::types::fmt_lambda(param, body),
        Value::Block { .. } => "<block>".into(),
        Value::Native { entry, applied } => crate::types::fmt_native(&entry.name, applied),
    }
}

/// Render `count` children, stopping at `…N more` once `budget` is spent — but
/// never before the first, since a marker alone tells the reader nothing.
/// Charging a finished part against the budget as it stood *before* rendering
/// unwinds whatever its own children charged, so every byte is paid for once,
/// by the outermost container that owns it.
fn budgeted<T>(
    count: usize,
    items: impl Iterator<Item = T>,
    budget: &Cell<usize>,
    mut render: impl FnMut(T) -> String,
) -> Vec<String> {
    let mut parts = Vec::with_capacity(count);
    for (i, item) in items.enumerate() {
        if i > 0 && budget.get() == 0 {
            parts.push(format!("…{} more", count - i));
            break;
        }
        let before = budget.get();
        let part = render(item);
        budget.set(before.saturating_sub(part.len()));
        parts.push(part);
    }
    parts
}

fn quote_string(body: &str, params: &PrintParams) -> String {
    let level = quote_bump_level(body).max(params.min_quote_hashes);
    let hashes: String = "#".repeat(level);
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

fn bracketed(
    parts: &[String],
    indent: usize,
    open: &str,
    close: &str,
    params: &PrintParams,
) -> String {
    let inline = format!("{open}{}{close}", parts.join(", "));
    if inline.chars().count() <= params.max_width && !inline.contains('\n') {
        return inline;
    }
    let pad = "  ".repeat(indent + 1);
    let end_pad = "  ".repeat(indent);
    format!(
        "{open}\n{pad}{}\n{end_pad}{close}",
        parts.join(&format!(",\n{pad}"))
    )
}

/// Smallest `n` for which `body` round-trips inside `n*'#' + "'" + body + "'" +
/// n*'#'`: zero when `body` has no `'`, else one past the longest run of `#`s
/// following a `'`.
fn quote_bump_level(body: &str) -> usize {
    let bytes = body.as_bytes();
    let mut max_run: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            let mut run = 0;
            while i + 1 + run < bytes.len() && bytes[i + 1 + run] == b'#' {
                run += 1;
            }
            max_run = Some(max_run.map_or(run, |m| m.max(run)));
            i += 1 + run;
        } else {
            i += 1;
        }
    }
    max_run.map_or(0, |m| m + 1)
}
