//! Renders a [`Value`] as bracketed, quote-fenced ral surface syntax — a
//! utility, not a registered builtin. The REPL (`ral/src/repl/exec.rs`) tunes it
//! narrow for a terminal; exarch's tool-result `VALUE` section
//! (`exarch/src/shell_eval.rs`) forces `#` fences and quotes bytes, because the
//! model's prompt teaches only the `#'…'#` string form.

use crate::types::Value;

/// Tuning knobs for [`pretty_print`].
pub struct PrintParams {
    /// Char width above which a bracketed `List`/`Map` breaks across lines; the
    /// leading indent does not count toward it.
    pub max_width: usize,
    /// Elide the middle of map values longer than this; `0` keeps them whole.
    pub max_string: usize,
    /// `List`/`Map` nesting depth past which a body collapses to a count marker.
    pub max_depth: usize,
    /// Floor on the `#` fence count around a quoted string; `0` allows the
    /// minimal, possibly unfenced form.
    pub min_quote_hashes: usize,
    /// Quote-fence a nested `Bytes` like a `String`, rather than emitting its
    /// lossy text raw.
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
        Value::Native { entry, applied } => crate::types::fmt_native(&entry.name, applied),
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
                        // Long text lands in map values (descriptions, file
                        // bodies); a list item keeps its string whole.
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

/// Elide the middle of `s` to a `budget`-char head and tail around an
/// `[…elided N characters…]` marker. Each stops at its nearest newline too, so
/// no embedded newline survives; `s` returns whole only when nothing was cut.
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

#[cfg(test)]
mod tests {
    use super::*;

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
