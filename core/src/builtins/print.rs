//! The value pretty-printer: a rendering utility (not a registered builtin)
//! that renders a [`Value`] to a bracketed, quote-fenced surface form.
//!
//! Two callers, two shapes, tuned through [`PrintParams`]: the REPL wants
//! narrow, `'`-quoted output for a terminal; exarch's tool-result `VALUE`
//! section wants wider, always-`#`-fenced output.

use crate::types::Value;

/// Tuning knobs for [`pretty_print`].
///
/// Two callers, two shapes: the REPL wants
/// narrow, `'`-quoted output for a terminal; exarch's tool-result `VALUE`
/// section wants wider, always-`#`-fenced output because its system prompt
/// only teaches the model the hash-quoted string form.
pub struct PrintParams {
    /// Inline-vs-multiline threshold for a bracketed `List`/`Map`, in chars.
    pub max_width: usize,
    /// Clip leaf strings longer than this many chars; `0` disables clipping.
    pub max_string: usize,
    /// Structural nesting cap on `List`/`Map` bodies; deeper ones collapse to
    /// an `[...N items]` / `[:...N pairs]` marker instead of recursing.
    pub max_depth: usize,
    /// Floor on the `#` fence count around a quoted string. `0` allows the
    /// minimal (possibly unfenced) form; `1` always emits at least one `#`.
    pub min_quote_hashes: usize,
    /// Whether a nested `Bytes` value quote-fences like a `String` (exarch,
    /// whose model-facing surface only speaks quoted strings) or renders as
    /// raw lossy text (the REPL, showing bytes as their readable content).
    pub quote_bytes: bool,
}

pub const REPL_PRINT_PARAMS: PrintParams = PrintParams {
    max_width: 80,
    max_string: 72,
    max_depth: 6,
    min_quote_hashes: 0,
    quote_bytes: false,
};

pub fn pretty_print(val: &Value, indent: usize, params: &PrintParams) -> String {
    match val {
        Value::String(s) => quote_string(s, params),
        Value::Unit => "unit".into(),
        Value::Bool(b) => {
            if *b {
                "true".into()
            } else {
                "false".into()
            }
        }
        Value::Int(n) => n.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Handle(_) => "<handle>".into(),
        Value::Lambda { param, body, .. } => crate::types::fmt_lambda(param, body),
        Value::Block { .. } => "<block>".into(),
        Value::Bytes(b) => {
            let text = String::from_utf8_lossy(b);
            if params.quote_bytes {
                quote_string(&text, params)
            } else {
                text.into_owned()
            }
        }
        Value::List(items) => {
            if items.is_empty() {
                return "[]".into();
            }
            if indent >= params.max_depth {
                return format!("[...{} items]", items.len());
            }
            let parts: Vec<String> = items
                .iter()
                .map(|v| pretty_print(v, indent + 1, params))
                .collect();
            bracketed(&parts, indent, "[", "]", params)
        }
        Value::Map(pairs) => {
            if pairs.is_empty() {
                return "[:]".into();
            }
            if indent >= params.max_depth {
                return format!("[:...{} pairs]", pairs.len());
            }
            let parts: Vec<String> = pairs
                .iter()
                .map(|(k, v)| {
                    let rendered = match v {
                        // Only a map's own values are long-text-shaped enough
                        // (descriptions, file bodies) to be worth eliding —
                        // a list item keeps its string whole.
                        Value::String(s) if params.max_string > 0 => {
                            quote_string(&elide(s, params.max_string), params)
                        }
                        Value::Bytes(b) if params.quote_bytes && params.max_string > 0 => {
                            quote_string(
                                &elide(&String::from_utf8_lossy(b), params.max_string),
                                params,
                            )
                        }
                        _ => pretty_print(v, indent + 1, params),
                    };
                    format!("{k}: {rendered}")
                })
                .collect();
            bracketed(&parts, indent, "[", "]", params)
        }
        Value::Variant { label, payload } => match payload {
            None => format!("`{label}"),
            Some(p) => format!("`{label} {}", pretty_print(p, indent, params)),
        },
    }
}

fn quote_string(body: &str, params: &PrintParams) -> String {
    let level = quote_bump_level(body).max(params.min_quote_hashes);
    let hashes: String = "#".repeat(level);
    format!("{hashes}'{body}'{hashes}")
}

/// Elide the middle of `s` down to a `budget`-char head+tail, leaving an
/// `[…elided N characters…]` marker in between. A run past the head or
/// tail's own newline is cut short there instead, so an embedded newline
/// never survives into the result. Returns `s` unchanged if it already
/// fits (and has no newline to excise).
fn elide(s: &str, budget: usize) -> String {
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
    if elided == 0 {
        return s.to_string();
    }
    format!("{head} […elided {elided} characters…] {tail}")
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

/// Smallest hash-bump level that lets `body` round-trip inside
/// `n*'#' + "'" + body + "'" + n*'#'`.  Zero if the body has no `'`;
/// otherwise one more than the longest run of `#`s following any `'`.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `List`/`Map` nested past `max_depth` collapses to a count marker
    /// instead of unfolding, so a deeply nested value can't blow up output.
    #[test]
    fn pretty_print_elides_past_max_depth() {
        let params = PrintParams {
            max_depth: 1,
            ..REPL_PRINT_PARAMS
        };
        let nested =
            Value::List(vec![Value::List(vec![Value::Int(1), Value::Int(2)].into())].into());
        let out = pretty_print(&nested, 0, &params);
        assert_eq!(out, "[[...2 items]]");
    }

    /// The depth cap only counts `List`/`Map` nesting; a `Variant` wrapper
    /// doesn't consume a depth level on its own.
    #[test]
    fn pretty_print_variant_does_not_consume_depth() {
        let params = PrintParams {
            max_depth: 1,
            ..REPL_PRINT_PARAMS
        };
        let val = Value::List(
            vec![Value::Variant {
                label: "some".into(),
                payload: Some(Box::new(Value::Int(1))),
            }]
            .into(),
        );
        let out = pretty_print(&val, 0, &params);
        assert_eq!(out, "[`some 1]");
    }

    /// A long string as a map value elides its middle to a head+tail with
    /// an `[…elided N characters…]` marker, not a first-line clip.
    #[test]
    fn pretty_print_elides_long_map_string_value() {
        let params = PrintParams {
            max_string: 20,
            ..REPL_PRINT_PARAMS
        };
        let val = Value::Map(
            vec![(
                "note".into(),
                Value::String(
                    "a very long and tiresome sentence that goes on and on and so the play ended"
                        .into(),
                ),
            )]
            .into(),
        );
        let out = pretty_print(&val, 0, &params);
        assert!(
            out.contains("…elided") && out.contains("characters…"),
            "expected an elision marker, got {out:?}"
        );
        assert!(
            out.starts_with("[note: 'a very lon"),
            "keeps the head, got {out:?}"
        );
        assert!(out.ends_with("play ended']"), "keeps the tail, got {out:?}");
    }

    /// A string that's a list item, not a map value, is never elided —
    /// only map values get the truncation treatment.
    #[test]
    fn pretty_print_does_not_elide_list_string_items() {
        let params = PrintParams {
            max_string: 10,
            ..REPL_PRINT_PARAMS
        };
        let long = "a very long and tiresome sentence that goes on and on";
        let val = Value::List(vec![Value::String(long.into())].into());
        let out = pretty_print(&val, 0, &params);
        assert!(out.contains(long), "list items print in full, got {out:?}");
    }
}
