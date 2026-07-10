//! The `help` / `explain` subsystem.
//!
//! `help` prints the at-a-glance command index; `explain <name>` resolves
//! one builtin, prelude function, or host-registered library entry to its
//! doc, type signature, and source location.

use crate::ansi::{self, BOLD, CYAN, DIM, RESET};
use crate::typecheck::{builtin_type_hint, fmt_scheme};
use crate::types::{Shell, Value};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::OnceLock;

/// Register prelude type hints from the baked schemes so that `builtin_help`
/// can display them without needing access to the baked binary.
///
/// # Panics
/// Panics if the prelude type hints have already been registered.
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
///
/// # Panics
/// Panics if the host library docs have already been registered.
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

/// Collect a `name → doc` map into a vector of pairs sorted by name.
fn sorted_pairs(map: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// Return all host-registered library names with their doc strings, sorted
/// alphabetically.
fn library_all_docs() -> Vec<(String, String)> {
    LIBRARY_DOCS.get().map_or_else(Vec::new, sorted_pairs)
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
fn prelude_doc(name: &str) -> Option<String> {
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
fn prelude_all_docs() -> Vec<(String, String)> {
    sorted_pairs(prelude_docs())
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
}
