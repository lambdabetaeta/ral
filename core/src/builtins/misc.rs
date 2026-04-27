use crate::ansi::{self, BOLD, CYAN, DIM, RESET};
use crate::typecheck::{builtin_type_hint, fmt_scheme};
use crate::types::*;
use std::collections::HashMap;
use std::sync::OnceLock;

use super::util::sig;

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

/// Scan the embedded prelude source for `## doc` / `let name` pairs and return
/// the resulting map, initialised once.
fn prelude_docs() -> &'static HashMap<String, String> {
    static DOCS: OnceLock<HashMap<String, String>> = OnceLock::new();
    DOCS.get_or_init(|| {
        let mut map = HashMap::new();
        let mut pending: Option<&str> = None;
        for line in include_str!("../prelude.ral").lines() {
            let trimmed = line.trim();
            if let Some(doc) = trimmed.strip_prefix("## ") {
                pending = Some(doc);
            } else if let Some(rest) = trimmed.strip_prefix("let ") {
                if let Some(doc) = pending.take()
                    && let Some(fn_name) = rest.split_whitespace().next()
                {
                    map.insert(
                        fn_name.trim_end_matches('=').trim().to_string(),
                        doc.to_string(),
                    );
                }
            } else {
                pending = None;
            }
        }
        map
    })
}

/// Return the doc comment for a prelude function.
pub(super) fn prelude_doc(name: &str) -> Option<String> {
    prelude_docs().get(name).cloned()
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

pub(super) fn builtin_help(args: &[Value], shell: &mut Shell) -> Value {
    let color = ansi::use_ui_color();
    let (bold, cyan, dim, reset) = if color {
        (BOLD, CYAN, DIM, RESET)
    } else {
        ("", "", "", "")
    };

    // Parse --types flag: strip it from args before treating remainder as a name.
    let show_types = args.iter().any(|v| v.to_string() == "--types");
    let name_args: Vec<&Value> = args.iter().filter(|v| v.to_string() != "--types").collect();

    let fmt_entry = |name: &str, doc: &str, type_hint: Option<String>| -> String {
        let mut s = format!(
            "  {cyan}{name}{reset}{dim}:{reset} {doc}\n",
            cyan = cyan,
            name = name,
            reset = reset,
            dim = dim,
            doc = doc
        );
        if show_types && let Some(hint) = type_hint {
            s.push_str(&format!(
                "  {dim}{hint}{reset}\n",
                dim = dim,
                hint = hint,
                reset = reset
            ));
        }
        s.push('\n');
        s
    };

    let out = if name_args.is_empty() {
        let mut s = format!("{bold}Builtins:{reset}\n", bold = bold, reset = reset);
        let mut builtin_names: Vec<&str> = super::builtin_names()
            .iter()
            .copied()
            .filter(|n| !n.starts_with('_'))
            .collect();
        builtin_names.sort_unstable();
        for name in builtin_names {
            if let Some(doc) = super::builtin_doc(name) {
                s.push_str(&fmt_entry(name, doc, builtin_type_hint(name)));
            }
        }
        s.push_str(&format!(
            "{bold}Prelude:{reset}\n",
            bold = bold,
            reset = reset
        ));
        for (name, doc) in prelude_all_docs() {
            s.push_str(&fmt_entry(&name, &doc, prelude_type_hint(&name)));
        }
        s
    } else {
        let name = name_args[0].to_string();
        if let Some(doc) = super::builtin_doc(&name) {
            fmt_entry(&name, doc, builtin_type_hint(&name))
        } else if let Some(doc) = prelude_doc(&name) {
            fmt_entry(&name, &doc, prelude_type_hint(&name))
        } else {
            format!("help: no documentation for '{name}'\n")
        }
    };
    let _ = shell.write_stdout(out.as_bytes());
    shell.control.last_status = 0;
    Value::Unit
}

pub fn pretty_print(val: &Value, indent: usize) -> String {
    match val {
        Value::String(s) => {
            let escaped = s.replace('\'', "''");
            if escaped.len() > 80 || escaped.contains('\n') {
                let first_line = escaped.lines().next().unwrap_or("");
                // Truncate by chars — slicing by byte offset can split a UTF-8
                // multibyte sequence and panic.
                let truncated: String = first_line.chars().take(72).collect();
                format!("'{truncated}...'")
            } else {
                format!("'{escaped}'")
            }
        }
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
        Value::Thunk { .. } => "<block>".into(),
        Value::Bytes(b) => format!("<bytes: {}>", b.len()),
        Value::List(items) => {
            if items.is_empty() {
                return "[]".into();
            }
            if items.iter().all(is_simple) {
                let parts: Vec<String> = items.iter().map(|v| pretty_print(v, 0)).collect();
                return format!("[{}]", parts.join(", "));
            }
            let pad = "  ".repeat(indent + 1);
            let end_pad = "  ".repeat(indent);
            let parts: Vec<String> = items
                .iter()
                .map(|v| format!("{pad}{}", pretty_print(v, indent + 1)))
                .collect();
            format!("[\n{}\n{end_pad}]", parts.join(",\n"))
        }
        Value::Map(pairs) => {
            if pairs.is_empty() {
                return "[:]".into();
            }
            if pairs.iter().all(|(_, v)| is_simple(v)) {
                let parts: Vec<String> = pairs
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", pretty_print(v, 0)))
                    .collect();
                return format!("[{}]", parts.join(", "));
            }
            let pad = "  ".repeat(indent + 1);
            let end_pad = "  ".repeat(indent);
            let parts: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{pad}{k}: {}", pretty_print(v, indent + 1)))
                .collect();
            format!("[\n{}\n{end_pad}]", parts.join(",\n"))
        }
    }
}

fn is_simple(val: &Value) -> bool {
    matches!(
        val,
        Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Handle(_)
            | Value::Thunk { .. }
    ) || matches!(val, Value::String(s) if s.len() < 60)
}

fn format_args(args: &[Value]) -> String {
    args.iter()
        .map(|v| match v {
            Value::Map(_) => pretty_print(v, 0),
            _ => v.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn builtin_echo(args: &[Value], shell: &mut Shell) -> Value {
    let _ = shell.write_stdout(format!("{}\n", format_args(args)).as_bytes());
    shell.control.last_status = 0;
    Value::Unit
}

pub(super) fn builtin_warn(args: &[Value]) -> Value {
    eprintln!("{}", format_args(args));
    Value::Unit
}

pub(super) fn builtin_fail(args: &[Value]) -> EvalSignal {
    let Some(Value::Map(pairs)) = args.first() else {
        return EvalSignal::Error(Error::new(
            "fail expects an error record [status: Int, ...]",
            1,
        ));
    };
    let lookup = |k: &str| pairs.iter().find(|(name, _)| name == k).map(|(_, v)| v);
    let Some(status) = lookup("status").and_then(Value::as_int) else {
        return EvalSignal::Error(Error::new(
            "fail: error record missing or non-integer 'status' field",
            1,
        ));
    };
    if status == 0 {
        return EvalSignal::Error(Error::new(
            "fail requires a nonzero status (use `return` for clean exit)",
            1,
        ));
    }
    let message = match lookup("message") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => "explicit failure".to_string(),
    };
    EvalSignal::Error(Error::new(message, status as i32))
}

pub(super) fn builtin_exit(args: &[Value], _env: &mut Shell) -> Result<Option<Value>, EvalSignal> {
    if args.len() > 1 {
        return Err(sig("exit accepts at most 1 argument"));
    }
    let code = match args.first() {
        None => 0,
        Some(Value::Int(n)) => *n as i32,
        Some(v) => v
            .to_string()
            .parse::<i32>()
            .map_err(|_| sig("exit: status must be an integer"))?,
    };
    Err(EvalSignal::Exit(code))
}

// Print prompt to the console and read one line from the console.
// Bypasses stdin/stdout redirection so it always talks to the user.
// Returns Unit on EOF (Ctrl+D / Ctrl+Z).
pub(super) fn builtin_ask(args: &[Value]) -> Result<Value, EvalSignal> {
    let prompt = args
        .first()
        .ok_or_else(|| sig("ask requires a prompt string"))?;
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
        .map_err(|e| sig(format!("ask: {e}")))?;
    write!(out, "{}", prompt).ok();
    out.flush().ok();
    drop(out);
    let inp = std::fs::File::open(CON_IN).map_err(|e| sig(format!("ask: {e}")))?;
    let mut line = String::new();
    let n = std::io::BufReader::new(inp)
        .read_line(&mut line)
        .map_err(|e| sig(format!("ask: {e}")))?;
    if n == 0 {
        return Err(sig("ask: EOF"));
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Value::String(line))
}
