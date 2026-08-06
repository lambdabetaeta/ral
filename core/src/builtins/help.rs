//! `help` prints the command index; `explain <name>` resolves one user
//! definition, builtin, prelude function, or host-installed library entry to
//! its doc, its type, and where the shell would find it.

use crate::ansi::{self, BOLD, CYAN, DIM, RESET};
use crate::typecheck::{builtin_type_hint, fmt_scheme};
use crate::types::{Shell, Value};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::OnceLock;

/// The baked prelude's `Bind` nodes carry the checker's harvested schemes,
/// so the shell's own prelude scope is the whole registry.
fn prelude_type_hint(name: &str, shell: &Shell) -> Option<String> {
    let scheme = shell
        .mobile
        .scope
        .get_prelude_binding(name)?
        .scheme
        .as_ref()?;
    Some(fmt_scheme(scheme))
}

/// A checked top-level `let` installs its generalised scheme beside the
/// value, so a user definition's most general type reads straight off the
/// local binding.  `None` for a binding from an unchecked path.
fn local_type_hint(name: &str, shell: &Shell) -> Option<String> {
    let scheme = shell
        .mobile
        .scope
        .get_local_binding(name)?
        .scheme
        .as_ref()?;
    Some(fmt_scheme(scheme))
}

fn sorted_pairs(map: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// Empty until a host calls [`Shell::install_library_docs`].
fn library_all_docs(shell: &Shell) -> Vec<(String, String)> {
    sorted_pairs(&shell.session.library_docs)
}

/// Scrape `## doc` / `let name` pairs from the prelude source — the same file
/// the baked prelude comes from, read a second way, since only the values and
/// schemes survive baking.  A summary is the first paragraph: doc lines join
/// with spaces, a bare `##` closes it, anything else discards it.
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

fn prelude_doc(name: &str) -> Option<String> {
    prelude_docs().get(name).cloned()
}

/// The documented prelude function names, unsorted.  An embedding host folds
/// these into its own command index beside [`Shell::builtin_names`].
pub fn prelude_names() -> Vec<&'static str> {
    prelude_docs().keys().map(String::as_str).collect()
}

fn prelude_all_docs() -> Vec<(String, String)> {
    sorted_pairs(prelude_docs())
}

/// `(bold, cyan, dim, reset)`, all empty when color is off.
fn ui_colors() -> (&'static str, &'static str, &'static str, &'static str) {
    if ansi::use_ui_color() {
        (BOLD, CYAN, DIM, RESET)
    } else {
        ("", "", "", "")
    }
}

fn fmt_entry(
    name: &str,
    doc: Option<&str>,
    type_hint: &str,
    source: Option<&str>,
    (cyan, dim, reset): (&str, &str, &str),
) -> String {
    let mut s = match doc {
        Some(doc) => format!("  {cyan}{name}{reset}{dim}:{reset} {doc}\n"),
        None => format!("  {cyan}{name}{reset}\n"),
    };
    let _ = writeln!(s, "  {dim}{type_hint}{reset}");
    if let Some(src) = source {
        let _ = writeln!(s, "  {dim}{src}{reset}");
    }
    s.push('\n');
    s
}

fn fmt_line(name: &str, doc: &str, (cyan, dim, reset): (&str, &str, &str)) -> String {
    format!("  {cyan}{name}{reset} {dim}—{reset} {doc}\n")
}

pub(super) fn builtin_help(_args: &[Value], shell: &mut Shell) -> Value {
    let (bold, cyan, dim, reset) = ui_colors();
    let line_colors = (cyan, dim, reset);

    let out = {
        let mut s = format!("{bold}Builtins:{reset}\n");
        let mut builtin_names: Vec<&str> = shell
            .builtin_names()
            .filter(|n| !n.starts_with('_'))
            .collect();
        builtin_names.sort_unstable();
        for name in builtin_names {
            if let Some(entry) = shell.lookup_builtin(name) {
                s.push_str(&fmt_line(name, entry.doc, line_colors));
            }
        }
        let _ = writeln!(s, "{bold}Prelude:{reset}");
        for (name, doc) in prelude_all_docs() {
            s.push_str(&fmt_line(&name, &doc, line_colors));
        }
        let library = library_all_docs(shell);
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
    let type_str = type_for(&name, &shell.session.builtins);

    // A local binding shadows every other resolution at runtime, so it
    // answers first here too.
    let out = if let Some(lt) = local_type_hint(&name, shell) {
        fmt_entry(&name, None, &lt, source.as_deref(), colors)
    } else if let Some(entry) = shell.lookup_builtin(&name) {
        fmt_entry(&name, Some(entry.doc), &type_str, source.as_deref(), colors)
    } else if let Some(doc) = prelude_doc(&name) {
        let pt = prelude_type_hint(&name, shell).unwrap_or(type_str);
        fmt_entry(&name, Some(&doc), &pt, source.as_deref(), colors)
    } else if let Some(doc) = shell.session.library_docs.get(&name).cloned() {
        fmt_entry(&name, Some(&doc), &type_str, source.as_deref(), colors)
    } else if let Some(src) = source {
        format!("explain: {src}\n")
    } else {
        let mut hits: Vec<(String, String)> = Vec::new();
        for n in shell.builtin_names() {
            if !n.starts_with('_')
                && name_matches(&name, n)
                && let Some(entry) = shell.lookup_builtin(n)
            {
                hits.push((n.to_string(), entry.doc.to_string()));
            }
        }
        for (n, doc) in prelude_all_docs() {
            if name_matches(&name, &n) {
                hits.push((n, doc));
            }
        }
        for (n, doc) in library_all_docs(shell) {
            if name_matches(&name, &n) {
                hits.push((n, doc));
            }
        }
        if hits.is_empty() {
            format!("explain: {name}: not found\n")
        } else {
            hits.sort_by(|a, b| a.0.cmp(&b.0));
            hits.iter()
                .map(|(n, doc)| fmt_line(n, doc, colors))
                .collect()
        }
    };
    let _ = shell.write_stdout(out.as_bytes());
    shell.mobile.control.last_status = 0;
    Value::Unit
}

/// The fallback search `explain` runs when a name resolves to nothing: a
/// case-insensitive regex, degrading to substring for a pattern that will not
/// compile.
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

/// Case-insensitive substring search; without `grep` there is no regex.
#[cfg(not(feature = "grep"))]
fn name_matches(pattern: &str, name: &str) -> bool {
    name.to_lowercase().contains(&pattern.to_lowercase())
}

/// A command-only builtin has no first-class scheme for `builtin_type_hint`
/// to format, so the fallback builds the `CompTy` its signature describes and
/// renders it through [`fmt_comp_ty_ctx`], the same renderer a type error
/// uses — one notation for both, result mode included.
fn type_for(name: &str, table: &crate::types::BuiltinTable) -> String {
    builtin_type_hint(table, name).unwrap_or_else(|| {
        use crate::typecheck::builtins::{
            BuiltinTypeRule, CompTemplate, lines_step_ty, sig_pipe_spec, ty_of_template,
        };
        use crate::typecheck::{CompTy, FmtCtx, Unifier, fmt_comp_ty_ctx};
        match table.get(name).map(|e| e.type_rule) {
            Some(BuiltinTypeRule::Sig(sig)) => {
                let mut u = Unifier::new();
                let pipe = sig_pipe_spec(&sig.result, &mut u);
                let value = match sig.result {
                    CompTemplate::Pure(t) | CompTemplate::Return { value: t, .. } => {
                        ty_of_template(t, &mut u)
                    }
                    CompTemplate::Never => u.fresh_ty(),
                    CompTemplate::LinesStep => lines_step_ty(&mut u),
                };
                fmt_comp_ty_ctx(&CompTy::Return(pipe, Box::new(value)), &FmtCtx::default())
            }
            _ => "—".into(),
        }
    })
}

/// Where the shell would find `name`.  Probes handlers before the manifest —
/// the reverse of `command_call::resolve`'s env-first order — so a handler
/// under a native's name reports as `handler` though only `^name` reaches it.
fn which_line(name: &str, shell: &Shell) -> Option<String> {
    if shell.mobile.scope.get_local(name).is_some() {
        return Some(format!("{name}: local"));
    }
    if shell.mobile.scope.get_prelude(name).is_some() {
        return Some(format!("{name}: prelude"));
    }
    // An alias is a handler frame too, so it must be named before the
    // handler arm swallows it.
    if shell.has_alias(name) {
        return Some(format!("{name}: alias"));
    }
    if shell.lookup_handler(name).is_some() {
        return Some(format!("{name}: handler"));
    }
    if shell.lookup_builtin(name).is_some() {
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

    #[test]
    fn blank_doc_line_ends_the_summary() {
        let par_doc = prelude_doc("par").expect("par has a doc comment");
        assert_eq!(
            par_doc, "Parallel map over `items` with at most `jobs` concurrent blocks.",
            "only the lead paragraph is the summary, got {par_doc:?}"
        );
    }

    /// Library docs hang off the session, not a process-global registry, so
    /// a shell no host dressed shows no `Library:` section at all.
    #[test]
    fn bare_shell_help_has_no_library_section() {
        let mut shell = Shell::default();
        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_help(&[], &mut shell);
        let out = String::from_utf8(crate::io::take_buffer(&buf)).expect("help output is UTF-8");
        assert!(
            !out.contains("Library:"),
            "a bare shell must list no Library section, got:\n{out}"
        );
    }

    /// [`Shell::install_library_docs`] is the one door onto
    /// `session.library_docs`.
    #[test]
    fn installed_library_docs_surface_in_help_and_explain() {
        let mut shell = Shell::default();
        shell.install_library_docs(vec![("frob".to_string(), "frob the widget".to_string())]);

        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_help(&[], &mut shell);
        let help_out =
            String::from_utf8(crate::io::take_buffer(&buf)).expect("help output is UTF-8");
        assert!(
            help_out.contains("Library:") && help_out.contains("frob"),
            "help must list the installed library entry, got:\n{help_out}"
        );

        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_explain(&[Value::String("frob".into())], &mut shell);
        let explain_out =
            String::from_utf8(crate::io::take_buffer(&buf)).expect("explain output is UTF-8");
        assert!(
            explain_out.contains("frob the widget"),
            "explain must resolve the installed library doc, got:\n{explain_out}"
        );
    }
}
