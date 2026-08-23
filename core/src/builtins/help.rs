//! `help` prints the command index; `explain <name>` resolves one user
//! definition, builtin, prelude function, or host-installed library entry to
//! its doc, its type, and where the shell would find it.

use crate::ansi::{self, BOLD, CYAN, DIM, RESET};
use crate::prelude_manifest::PRELUDE_DOCS;
use crate::typecheck::{builtin_type_hint, fmt_scheme};
use crate::types::{Binding, Shell, Value};
use std::fmt::Write;

/// A checked `let` installs its generalised scheme beside the value, so a
/// definition's most general type reads straight off its binding — the baked
/// prelude's `Bind` nodes carry theirs too.  `None` for a binding from an
/// unchecked path, which has no scheme to show.
fn scheme_of(binding: Option<&Binding>) -> Option<String> {
    binding?.scheme.as_ref().map(fmt_scheme)
}

fn prelude_doc(name: &str) -> Option<&'static str> {
    PRELUDE_DOCS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|&(_, doc)| doc)
}

/// The documented prelude function names.  An embedding host folds these into
/// its own command index beside [`Shell::builtin_names`].
pub fn prelude_names() -> Vec<&'static str> {
    PRELUDE_DOCS.iter().map(|&(name, _)| name).collect()
}

fn by_name<'a>(mut rows: Vec<(&'a str, &'a str)>) -> Vec<(&'a str, &'a str)> {
    rows.sort_unstable_by_key(|&(name, _)| name);
    rows
}

/// Every documented registry, in `help`'s print order: the builtin manifest,
/// the prelude, then whatever a host installed with
/// [`Shell::install_library_docs`] — which is empty until one does.
/// `explain`'s fallback search sweeps the same three.
fn registries(shell: &Shell) -> [(&'static str, Vec<(&str, &str)>); 3] {
    [
        (
            "Builtins",
            by_name(
                shell
                    .builtin_names()
                    .filter(|name| !name.starts_with('_'))
                    .filter_map(|name| shell.lookup_builtin(name).map(|entry| (name, entry.doc)))
                    .collect(),
            ),
        ),
        // `build.rs` emits these name-sorted.
        ("Prelude", PRELUDE_DOCS.to_vec()),
        (
            "Library",
            by_name(
                shell
                    .session
                    .library_docs
                    .iter()
                    .map(|(name, doc)| (name.as_str(), doc.as_str()))
                    .collect(),
            ),
        ),
    ]
}

/// The palette for one render, every field empty when color is off.
#[derive(Clone, Copy)]
struct Colors {
    bold: &'static str,
    cyan: &'static str,
    dim: &'static str,
    reset: &'static str,
}

impl Colors {
    fn current() -> Self {
        if ansi::use_ui_color() {
            Self {
                bold: BOLD,
                cyan: CYAN,
                dim: DIM,
                reset: RESET,
            }
        } else {
            Self {
                bold: "",
                cyan: "",
                dim: "",
                reset: "",
            }
        }
    }
}

/// One `explain` entry: the doc, the type, and the frame that would run.
fn fmt_entry(
    name: &str,
    doc: Option<&str>,
    ty: Option<&str>,
    source: Option<&str>,
    Colors {
        cyan, dim, reset, ..
    }: Colors,
) -> String {
    let head = match doc {
        Some(doc) => format!("  {cyan}{name}{reset}{dim}:{reset} {doc}\n"),
        None => format!("  {cyan}{name}{reset}\n"),
    };
    // Every manifest row has a type to print — a value row's polytype, a base
    // frame's argv type — so an em dash means no registry had one.
    let ty = format!("  {dim}{}{reset}\n", ty.unwrap_or("—"));
    let source = source.map_or_else(String::new, |src| format!("  {dim}{src}{reset}\n"));
    format!("{head}{ty}{source}\n")
}

/// One row of the `help` index.
fn fmt_line(
    name: &str,
    doc: &str,
    Colors {
        cyan, dim, reset, ..
    }: Colors,
) -> String {
    format!("  {cyan}{name}{reset} {dim}—{reset} {doc}\n")
}

pub(super) fn builtin_help(_args: &[Value], shell: &mut Shell) -> Value {
    let colors = Colors::current();
    let Colors {
        bold, dim, reset, ..
    } = colors;

    let index = registries(shell)
        .into_iter()
        .filter(|(_, rows)| !rows.is_empty())
        .fold(String::new(), |mut index, (heading, rows)| {
            let _ = writeln!(index, "{bold}{heading}:{reset}");
            index.extend(rows.iter().map(|&(name, doc)| fmt_line(name, doc, colors)));
            index
        });
    let out = format!(
        "{index}{dim}──{reset}\n\
         {dim}Use `explain <name>` for the full type signature and source location of any entry.{reset}\n"
    );

    let _ = shell.write_stdout(out.as_bytes());
    shell.mobile.control.last_status = 0;
    Value::Unit
}

pub(super) fn builtin_explain(args: &[Value], shell: &mut Shell) -> Value {
    let out = match args.first() {
        Some(arg) => explanation(&arg.to_string(), shell, Colors::current()),
        None => "explain: expected a name, e.g. `explain map`\n".to_string(),
    };
    let _ = shell.write_stdout(out.as_bytes());
    shell.mobile.control.last_status = 0;
    Value::Unit
}

/// The owning registry's doc and type over the frame that would run; failing
/// that, the bare source line; failing that, a search of every documented name.
fn explanation(name: &str, shell: &Shell, colors: Colors) -> String {
    let source = which_line(name, shell);
    match resolve(name, shell) {
        Some((doc, ty)) => fmt_entry(
            name,
            doc.as_deref(),
            ty.as_deref(),
            source.as_deref(),
            colors,
        ),
        None => source.map_or_else(
            || search(name, shell, colors),
            |src| format!("explain: {src}\n"),
        ),
    }
}

/// The registry that owns `name`, in the order the runtime resolves it, each
/// answering with a doc and a type.
///
/// A local answers first, since it shadows every other resolution at runtime —
/// and its doc can only be the library table's: that is the one registry
/// naming the locals a sourced library installs, so a local shadowing a
/// prelude name has not inherited the prelude's doc.
fn resolve(name: &str, shell: &Shell) -> Option<(Option<String>, Option<String>)> {
    let scope = &shell.mobile.scope;
    let manifest = builtin_type_hint(&shell.session.builtins, name);
    let library_doc = || shell.session.library_docs.get(name).cloned();

    scheme_of(scope.get_local_binding(name))
        .map(|ty| (library_doc(), Some(ty)))
        .or_else(|| {
            shell
                .lookup_builtin(name)
                .map(|entry| (Some(entry.doc.to_owned()), manifest.clone()))
        })
        .or_else(|| {
            prelude_doc(name).map(|doc| {
                let ty = scheme_of(scope.get_prelude_binding(name)).or_else(|| manifest.clone());
                (Some(doc.to_owned()), ty)
            })
        })
        .or_else(|| library_doc().map(|doc| (Some(doc), manifest.clone())))
}

/// The fallback `explain` runs when no registry knows the name: every
/// documented entry whose name matches, as `help`'s one-line rows.
fn search(pattern: &str, shell: &Shell, colors: Colors) -> String {
    let matches = matcher(pattern);
    let hits: String = registries(shell)
        .into_iter()
        .flat_map(|(_, rows)| rows)
        .filter(|&(name, _)| matches(name))
        .map(|(name, doc)| fmt_line(name, doc, colors))
        .collect();
    if hits.is_empty() {
        format!("explain: {pattern}: not found\n")
    } else {
        hits
    }
}

/// A case-insensitive regex over the whole index — compiled once, not per
/// name — degrading to a substring test for a pattern that will not compile.
fn matcher(pattern: &str) -> impl Fn(&str) -> bool {
    let regex = regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .ok();
    let lowered = pattern.to_lowercase();
    move |name| match &regex {
        Some(regex) => regex.is_match(name),
        None => name.to_lowercase().contains(&lowered),
    }
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

    /// `build.rs` scrapes these summaries out of `prelude.ral`; the two rules
    /// worth pinning are how a wrapped doc joins and where one stops.
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
