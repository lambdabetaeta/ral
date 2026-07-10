use crate::ansi::{self, BOLD, CYAN, DIM, RESET};
use crate::typecheck::{builtin_type_hint, fmt_scheme};
use crate::types::{Value, Shell, Break, sig, Error, Settled, Escape};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::OnceLock;

/// Register prelude type hints from the baked schemes so that `builtin_help`
/// can display them without needing access to the baked binary.
pub fn register_prelude_type_hints(schemes: &[(String, crate::typecheck::Scheme)]) {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let map: HashMap<String, String> = schemes
            .iter()
            .map(|(name, scheme)| (name.clone(), fmt_scheme(scheme)))
            .collect();
        PRELUDE_TYPE_HINTS
            .set(map)
            .expect("prelude type hints already set");
    });
}

static PRELUDE_TYPE_HINTS: OnceLock<HashMap<String, String>> = OnceLock::new();

fn prelude_type_hint(name: &str) -> Option<String> {
    PRELUDE_TYPE_HINTS.get()?.get(name).cloned()
}

/// Register extra `name -> doc` entries from an embedding host so that
/// `builtin_help` can list and look them up alongside the builtins and
/// prelude.
pub fn register_library_docs(entries: Vec<(String, String)>) {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        LIBRARY_DOCS
            .set(entries.into_iter().collect())
            .expect("library docs already set");
    });
}

static LIBRARY_DOCS: OnceLock<HashMap<String, String>> = OnceLock::new();

fn library_doc(name: &str) -> Option<String> {
    LIBRARY_DOCS.get()?.get(name).cloned()
}

/// Return all host-registered library names with their doc strings, sorted
/// alphabetically.
fn library_all_docs() -> Vec<(String, String)> {
    let Some(map) = LIBRARY_DOCS.get() else {
        return Vec::new();
    };
    let mut v: Vec<_> = map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// Scan the embedded prelude source for `## doc` / `let name` pairs and return
/// the resulting map, initialised once.  A function's summary is the first
/// paragraph of its doc comment: consecutive `## ` lines are joined with single
/// spaces, and a blank `##` line closes the summary so trailing detail
/// paragraphs are excluded.
fn prelude_docs() -> &'static HashMap<String, String> {
    static DOCS: OnceLock<HashMap<String, String>> = OnceLock::new();
    DOCS.get_or_init(|| {
        let mut map = HashMap::new();
        let mut pending: Option<String> = None;
        let mut closed = false;
        for line in include_str!("../prelude.ral").lines() {
            let trimmed = line.trim();
            if trimmed == "##" {
                if pending.is_some() {
                    closed = true;
                }
            } else if let Some(doc) = trimmed.strip_prefix("## ") {
                if !closed {
                    match pending.as_mut() {
                        Some(s) => {
                            s.push(' ');
                            s.push_str(doc);
                        }
                        None => pending = Some(doc.to_string()),
                    }
                }
            } else if let Some(rest) = trimmed.strip_prefix("let ") {
                if let Some(doc) = pending.take()
                    && let Some(fn_name) = rest.split_whitespace().next()
                {
                    map.insert(fn_name.trim_end_matches('=').trim().to_string(), doc);
                }
                closed = false;
            } else {
                pending = None;
                closed = false;
            }
        }
        map
    })
}

/// Return the doc comment for a prelude function.
pub(super) fn prelude_doc(name: &str) -> Option<String> {
    prelude_docs().get(name).cloned()
}

/// The documented prelude function names (the keys of [`prelude_docs`]),
/// unsorted.
///
/// An embedding host folds these into its own at-a-glance command
/// index beside the builtins; `explain <name>` then resolves each through
/// [`prelude_doc`].
pub fn prelude_names() -> Vec<&'static str> {
    prelude_docs().keys().map(String::as_str).collect()
}

/// Return all prelude names with their doc strings, sorted alphabetically.
pub(super) fn prelude_all_docs() -> Vec<(String, String)> {
    let mut v: Vec<_> = prelude_docs()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The ANSI color tuple `(bold, cyan, dim, reset)` used by `help`/`explain`
/// output, collapsed to empty strings when color is disabled.
fn ui_colors() -> (&'static str, &'static str, &'static str, &'static str) {
    if ansi::use_ui_color() {
        (BOLD, CYAN, DIM, RESET)
    } else {
        ("", "", "", "")
    }
}

/// Format a full entry: name, one-line doc, type hint, and optional source
/// location.  Shared by `help` and `explain`.
fn fmt_entry(
    name: &str,
    doc: &str,
    type_hint: &str,
    source: Option<&str>,
    (cyan, dim, reset): (&str, &str, &str),
) -> String {
    let mut s = format!("  {cyan}{name}{reset}{dim}:{reset} {doc}\n");
    let _ = writeln!(s, "  {dim}{type_hint}{reset}");
    if let Some(src) = source {
        let _ = writeln!(s, "  {dim}{src}{reset}");
    }
    s.push('\n');
    s
}

/// Format a one-line `name — doc` entry.  Shared by `help` and `explain`.
fn fmt_line(name: &str, doc: &str, (cyan, dim, reset): (&str, &str, &str)) -> String {
    format!("  {cyan}{name}{reset} {dim}—{reset} {doc}\n")
}

pub(super) fn builtin_help(_args: &[Value], shell: &mut Shell) -> Value {
    let (bold, cyan, dim, reset) = ui_colors();
    let line_colors = (cyan, dim, reset);

    let out = {
        let mut s = format!("{bold}Builtins:{reset}\n");
        let mut builtin_names: Vec<&str> = super::builtin_names()
            .iter()
            .copied()
            .filter(|n| !n.starts_with('_'))
            .collect();
        builtin_names.sort_unstable();
        for name in builtin_names {
            if let Some(doc) = super::builtin_doc(name) {
                s.push_str(&fmt_line(name, doc, line_colors));
            }
        }
        let _ = writeln!(s, "{bold}Prelude:{reset}");
        for (name, doc) in prelude_all_docs() {
            s.push_str(&fmt_line(&name, &doc, line_colors));
        }
        let library = library_all_docs();
        if !library.is_empty() {
            let _ = writeln!(s, "{bold}Library:{reset}");
            for (name, doc) in library {
                s.push_str(&fmt_line(&name, &doc, line_colors));
            }
        }
        let _ = writeln!(s, "{dim}──{reset}");
        let _ = writeln!(
            s,
            "{dim}Use `explain <name>` for the full type signature and source location of any entry.{reset}"
        );
        s
    };
    let _ = shell.write_stdout(out.as_bytes());
    shell.mobile.control.last_status = 0;
    Value::Unit
}
pub(super) fn builtin_explain(args: &[Value], shell: &mut Shell) -> Value {
    let (_bold, cyan, dim, reset) = ui_colors();
    let colors = (cyan, dim, reset);

    if args.is_empty() {
        let _ = shell.write_stdout(b"explain: expected a name, e.g. `explain map`\n");
        shell.mobile.control.last_status = 0;
        return Value::Unit;
    }

    let name = args[0].to_string();
    let source = which_line(&name, shell);
    let type_str = type_for(&name);

    let out = if let Some(doc) = super::builtin_doc(&name) {
        fmt_entry(&name, doc, &type_str, source.as_deref(), colors)
    } else if let Some(doc) = prelude_doc(&name) {
        let pt = prelude_type_hint(&name).unwrap_or(type_str);
        fmt_entry(&name, &doc, &pt, source.as_deref(), colors)
    } else if let Some(doc) = library_doc(&name) {
        fmt_entry(&name, &doc, &type_str, source.as_deref(), colors)
    } else if let Some(src) = source {
        format!("explain: {src}\n")
    } else {
        let mut hits: Vec<(String, String)> = Vec::new();
        for n in super::builtin_names() {
            if !n.starts_with('_')
                && name_matches(&name, n)
                && let Some(doc) = super::builtin_doc(n)
            {
                hits.push((n.to_string(), doc.to_string()));
            }
        }
        for (n, doc) in prelude_all_docs() {
            if name_matches(&name, &n) {
                hits.push((n, doc));
            }
        }
        for (n, doc) in library_all_docs() {
            if name_matches(&name, &n) {
                hits.push((n, doc));
            }
        }
        if hits.is_empty() {
            format!("explain: {name}: not found\n")
        } else {
            hits.sort_by(|a, b| a.0.cmp(&b.0));
            hits.iter().map(|(n, doc)| fmt_line(n, doc, colors)).collect()
        }
    };
    let _ = shell.write_stdout(out.as_bytes());
    shell.mobile.control.last_status = 0;
    Value::Unit
}

/// Test whether `name` matches the search `pattern`, case-insensitively.
#[cfg(feature = "grep")]
fn name_matches(pattern: &str, name: &str) -> bool {
    match grep::regex::RegexMatcherBuilder::new()
        .case_insensitive(true)
        .build(pattern)
    {
        Ok(matcher) => {
            use grep::matcher::Matcher;
            matcher.is_match(name.as_bytes()).unwrap_or(false)
        }
        Err(_) => name.to_lowercase().contains(&pattern.to_lowercase()),
    }
}

/// Test whether `name` matches the search `pattern`, case-insensitively.
#[cfg(not(feature = "grep"))]
fn name_matches(pattern: &str, name: &str) -> bool {
    name.to_lowercase().contains(&pattern.to_lowercase())
}

/// Return a type string for a builtin, falling back to its type rule.
fn type_for(name: &str) -> String {
    builtin_type_hint(name).unwrap_or_else(|| {
        use crate::typecheck::builtins::{BuiltinTypeRule, CompTemplate, ModeTemplate};
        match crate::builtins::builtin_type_rule(name) {
            Some(BuiltinTypeRule::Sig(sig)) => match sig.result {
                CompTemplate::Return {
                    input: ModeTemplate::None,
                    output: ModeTemplate::Bytes,
                    ..
                } => "F[none, bytes]".into(),
                CompTemplate::Return {
                    input: ModeTemplate::Bytes,
                    output: ModeTemplate::None,
                    ..
                }
                | CompTemplate::LinesStep => "F[bytes, none] Value".into(),
                CompTemplate::Return {
                    input: ModeTemplate::Bytes,
                    output: ModeTemplate::Bytes,
                    ..
                } => "F[bytes, bytes]".into(),
                CompTemplate::Pure(_)
                | CompTemplate::Return {
                    input: ModeTemplate::None,
                    output: ModeTemplate::None,
                    ..
                }
                | CompTemplate::Return { .. } => "F[none, none] Value".into(),
                CompTemplate::Never => "∀ types. F[I, O] Type".into(),
            },
            _ => "—".into(),
        }
    })
}

/// Resolve `name` to a one-line description of where the shell would find it.
fn which_line(name: &str, shell: &Shell) -> Option<String> {
    if shell.mobile.scope.get_local(name).is_some() {
        return Some(format!("{name}: local"));
    }
    if shell.mobile.scope.get_prelude(name).is_some() {
        return Some(format!("{name}: prelude"));
    }
    // Handler-stack arrivals (aliases and active `within` frames) report
    // before the builtin/external resolution so the user sees what
    // actually fires, not what would have fired without them.
    if shell.has_alias(name) {
        return Some(format!("{name}: alias"));
    }
    if shell.lookup_handler(name).is_some() {
        return Some(format!("{name}: handler"));
    }
    if crate::builtins::is_builtin(name) {
        return Some(format!("{name}: builtin"));
    }
    let path = shell.locate_command(name)?;
    let exec_name = if name.contains('/') {
        crate::ir::CommandName::Path(name.into())
    } else {
        crate::ir::CommandName::Bare(name.into())
    };
    let id = crate::runtime::command::CommandIdentity::resolve(exec_name, &shell.mobile.context);
    let admitted = crate::capability::admits_head(&shell.mobile.context, &id);
    if admitted {
        Some(format!("{name}: {}", path.to_string_lossy()))
    } else {
        Some(format!("{name}: denied by grant ({})", path.display()))
    }
}

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
                            quote_string(&elide(&String::from_utf8_lossy(b), params.max_string), params)
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
    let head: String = s.chars().take_while(|&c| c != '\n').take(head_budget).collect();
    let tail: String = {
        let rev: String = s.chars().rev().take_while(|&c| c != '\n').take(tail_budget).collect();
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

fn bracketed(parts: &[String], indent: usize, open: &str, close: &str, params: &PrintParams) -> String {
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
const CLEAR_SEQ: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";
// touch stty modes the way ncurses `reset` does; `^reset` reaches the real
const RESET_SEQ: &[u8] = b"\x1bc";
pub(super) fn builtin_clear(_args: &[Value], shell: &mut Shell) -> Value {
    let _ = shell.write_stdout(CLEAR_SEQ);
    shell.mobile.control.last_status = 0;
    Value::Unit
}
pub(super) fn builtin_reset(_args: &[Value], shell: &mut Shell) -> Value {
    let _ = shell.write_stdout(RESET_SEQ);
    shell.mobile.control.last_status = 0;
    Value::Unit
}

/// Narrow an i64 status to the `i32` an exit code must fit, erroring with
/// `who` named when it doesn't.  The zero-status guard reads the i64
/// directly, so a value that truncated to 0 under `as i32` no longer slips
/// the guard as a "success" failure.
fn status_i32(who: &str, n: i64) -> Result<i32, Break> {
    i32::try_from(n).map_err(|_| sig(format!("{who}: status {n} is outside the exit-code range")))
}

/// Turn a `fail` status into an exit code, or the one rule every `fail` path
/// must honour: the status must be nonzero.  Shared by the bare-int shorthand
/// and the error-record path so a zero status is always named as "wrong
/// rule", never mistaken for a shape complaint.
fn fail_status_code(status: i64) -> Result<i32, Break> {
    if status == 0 {
        return Err(Break::Error(Error::new(
            "fail requires a nonzero status (use `return` for clean exit)",
            1,
        )));
    }
    status_i32("fail", status)
}

pub(super) fn builtin_fail(args: &[Value]) -> Break {
    let m = match args.first() {
        Some(Value::Map(m)) => m,
        // `fail "msg"` / `fail $bytes` — a bare message shorthand for
        // `fail [status: 1, message: "msg"]`.  The checker's `fail` arg
        // is row-polymorphic and does not reject a scalar here, so the
        // runtime honours the scalar rather than erroring on it: a
        // failing pipeline producer (`{ fail "boom" }`) then raises with
        // the author's text instead of a shape complaint that hides it.
        Some(Value::String(s)) => return Break::Error(Error::new(s.clone(), 1)),
        Some(Value::Bytes(b)) => {
            return Break::Error(Error::new(String::from_utf8_lossy(b).into_owned(), 1));
        }
        // `fail $n` — a bare status with no message.
        Some(Value::Int(n)) => {
            return match fail_status_code(*n) {
                Ok(code) => Break::Error(Error::new("explicit failure", code)),
                Err(b) => b,
            };
        }
        _ => {
            return Break::Error(Error::new(
                "fail expects an error record [status: Int, ...]",
                1,
            ));
        }
    };
    let lookup = |k: &str| m.get(k);
    let Some(status) = lookup("status").and_then(Value::as_int) else {
        return Break::Error(Error::new(
            "fail: error record missing or non-integer 'status' field",
            1,
        ));
    };
    let code = match fail_status_code(status) {
        Ok(code) => code,
        Err(b) => return b,
    };
    let message = match lookup("message") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => "explicit failure".to_string(),
    };
    Break::Error(Error::new(message, code))
}

pub(super) fn builtin_exit(args: &[Value], _env: &mut Shell) -> Settled<Value> {
    if args.len() > 1 {
        return Err(sig("exit accepts at most 1 argument"));
    }
    let code = match args.first() {
        None => 0,
        Some(Value::Int(n)) => status_i32("exit", *n)?,
        Some(v) => v
            .to_string()
            .parse::<i32>()
            .map_err(|_| sig("exit: status must be an integer"))?,
    };
    Err(Break::Escape(Escape::Exit(code)))
}

/// `surface <event>` — hand the event value to the host's structured-event
/// sink, if one is installed.  The host decides what the variant's tag means;
/// with no sink (e.g. a bare REPL) this is the identity and returns Unit.
pub(super) fn builtin_surface(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    if let Some(event) = args.first() {
        shell.surface(event.clone());
    }
    Ok(Value::Unit)
}

// Print prompt to the console and read one line from the console.
// Bypasses stdin/stdout redirection so it always talks to the user.
// Errors on EOF (Ctrl+D / Ctrl+Z).
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:ask-tty] `ask` builtin opens the controlling terminal device (/dev/tty or CON) to prompt and read one line direct from the user, bypassing redirection; a terminal-device interaction, not turn-time model data I/O."
)]
pub(super) fn builtin_ask(args: &[Value]) -> Result<Value, Error> {
    let prompt = args
        .first()
        .ok_or_else(|| Error::new("ask requires a prompt string", 1))?;
    #[cfg(unix)]
    const CON_OUT: &str = "/dev/tty";
    #[cfg(unix)]
    const CON_IN: &str = "/dev/tty";
    #[cfg(not(unix))]
    const CON_OUT: &str = "CONOUT$";
    #[cfg(not(unix))]
    const CON_IN: &str = "CONIN$";

    use std::io::{BufRead, Write};
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .open(CON_OUT)
        .map_err(|e| Error::new(format!("ask: {e}"), 1))?;
    write!(out, "{prompt}").ok();
    out.flush().ok();
    drop(out);
    let inp = std::fs::File::open(CON_IN).map_err(|e| Error::new(format!("ask: {e}"), 1))?;
    let mut line = String::new();
    let n = std::io::BufReader::new(inp)
        .read_line(&mut line)
        .map_err(|e| Error::new(format!("ask: {e}"), 1))?;
    if n == 0 {
        return Err(Error::new("ask: EOF", 1));
    }
    let len = crate::io::str_strip_one_terminator(&line).len();
    line.truncate(len);
    Ok(Value::String(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary sentence that wraps across several `## ` lines is joined
    /// into one line, not truncated to its last physical line.
    #[test]
    fn multiline_doc_summary_joins_first_paragraph() {
        let lines_doc = prelude_doc("lines").expect("lines has a doc comment");
        assert!(
            lines_doc.starts_with("Split a string into lines"),
            "summary starts at the first line, got {lines_doc:?}"
        );
        assert!(
            lines_doc.contains("matching `from-lines` and external capture"),
            "the wrapped sentence is joined in full, got {lines_doc:?}"
        );
    }

    /// A blank `##` line closes the summary paragraph, so a function whose
    /// doc has trailing detail paragraphs lists only its lead sentence.
    #[test]
    fn blank_doc_line_ends_the_summary() {
        let par_doc = prelude_doc("par").expect("par has a doc comment");
        assert_eq!(
            par_doc, "Parallel map over `items` with at most `jobs` concurrent blocks.",
            "only the lead paragraph is the summary, got {par_doc:?}"
        );
    }

    /// A `List`/`Map` nested past `max_depth` collapses to a count marker
    /// instead of unfolding, so a deeply nested value can't blow up output.
    #[test]
    fn pretty_print_elides_past_max_depth() {
        let params = PrintParams {
            max_depth: 1,
            ..REPL_PRINT_PARAMS
        };
        let nested = Value::List(vec![Value::List(vec![Value::Int(1), Value::Int(2)].into())].into());
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
                Value::String("a very long and tiresome sentence that goes on and on and so the play ended".into()),
            )]
            .into(),
        );
        let out = pretty_print(&val, 0, &params);
        assert!(
            out.contains("…elided") && out.contains("characters…"),
            "expected an elision marker, got {out:?}"
        );
        assert!(out.starts_with("[note: 'a very lon"), "keeps the head, got {out:?}");
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
