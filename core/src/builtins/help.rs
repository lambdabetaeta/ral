//! `help` prints the command index; `explain <name>` resolves one user
//! definition, builtin, prelude function, or host-installed library entry to
//! its doc, its type, and where the shell would find it.

use crate::ansi::{self, BOLD, CYAN, DIM, RESET};
use crate::ir::CommandName;
use crate::prelude_manifest::PRELUDE_DOCS;
use crate::runtime::command::CommandIdentity;
use crate::typecheck::{builtin_type_hint, fmt_scheme};
use crate::types::{Binding, Env, HandlerLookup, Shell, Value};
use std::fmt::{self, Write};
use std::path::PathBuf;

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
///
/// A leading `_` marks a name internal: it is kept out of the index and out
/// of the search, though `explain` named it exactly still answers.
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
        (
            "Prelude",
            PRELUDE_DOCS
                .iter()
                .copied()
                .filter(|(name, _)| !name.starts_with('_'))
                .collect(),
        ),
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

/// The lead paragraph of a doc: everything before the first blank line, as
/// `build.rs` already scrapes the prelude's.
fn lead_paragraph(doc: &str) -> &str {
    doc.split_once("\n\n").map_or(doc, |(head, _)| head)
}

/// One `explain` entry: the doc, the type, then a dim line apiece for where
/// the name lives and what that shadows.
fn fmt_entry(
    name: &str,
    doc: Option<&str>,
    ty: Option<&str>,
    tail: &[String],
    Colors {
        cyan, dim, reset, ..
    }: Colors,
) -> String {
    let head = match doc {
        // Continuation lines must land inside the two-space block, not the margin.
        Some(doc) => format!(
            "  {cyan}{name}{reset}{dim}:{reset} {}\n",
            doc.replace('\n', "\n  ")
        ),
        None => format!("  {cyan}{name}{reset}\n"),
    };
    // Every manifest row has a type to print — a value row's polytype, a base
    // frame's argv type — so an em dash means no registry had one.
    let ty = format!("  {dim}{}{reset}\n", ty.unwrap_or("—"));
    let tail = tail.iter().fold(String::new(), |mut lines, line| {
        let _ = writeln!(lines, "  {dim}{line}{reset}");
        lines
    });
    format!("{head}{ty}{tail}\n")
}

/// One row of the `help` index.
fn fmt_line(
    name: &str,
    doc: &str,
    Colors {
        cyan, dim, reset, ..
    }: Colors,
) -> String {
    format!(
        "  {cyan}{name}{reset} {dim}—{reset} {}\n",
        lead_paragraph(doc)
    )
}

pub(super) fn builtin_help(_args: &[Value], _env: &Env, shell: &mut Shell) -> Value {
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

pub(super) fn builtin_explain(args: &[Value], env: &Env, shell: &mut Shell) -> Value {
    let out = match args.first() {
        Some(arg) => explanation(&arg.to_string(), env, shell, Colors::current()),
        None => "explain: expected a name, e.g. `explain map`\n".to_string(),
    };
    let _ = shell.write_stdout(out.as_bytes());
    shell.mobile.control.last_status = 0;
    Value::Unit
}

/// What documents `name` over the frame that would run.  A name nothing binds
/// and no registry documents falls to a search of the whole index; everything
/// else prints the one entry block, an em dash standing in for whichever of
/// the doc and the type no registry holds.
fn explanation(name: &str, env: &Env, shell: &Shell, colors: Colors) -> String {
    let sites = locate_all(name, env, shell);
    let (doc, ty) = documented(name, sites.first(), env, shell);
    if sites.is_empty() && doc.is_none() && ty.is_none() {
        return search(name, shell, colors);
    }
    let mut tail: Vec<String> = sites
        .first()
        .map(|site| format!("{name}: {site}"))
        .into_iter()
        .collect();
    if let [_, shadowed @ ..] = sites.as_slice()
        && !shadowed.is_empty()
    {
        let names: Vec<String> = shadowed.iter().map(Where::to_string).collect();
        tail.push(format!("shadows: {}", names.join(", ")));
    }
    fmt_entry(name, doc.as_deref(), ty.as_deref(), &tail, colors)
}

/// The doc and type held by the registry that owns `name`.
///
/// A local answers alone: it shadows every other resolution at runtime, so no
/// registry below it may speak for it, and its doc can only be the library
/// table's — that is the one registry naming the locals a sourced library
/// installs.
///
/// Every other site sweeps the documented registries, since a frame stacked
/// over a native — a handler, an alias — inherits the native's doc.
fn documented(
    name: &str,
    site: Option<&Where>,
    scope: &Env,
    shell: &Shell,
) -> (Option<String>, Option<String>) {
    let manifest = || builtin_type_hint(&shell.session.builtins, name);
    let library_doc = || shell.session.library_docs.get(name).cloned();

    if matches!(site, Some(Where::Local)) {
        return (library_doc(), scheme_of(scope.session_binding(name)));
    }
    shell
        .lookup_builtin(name)
        .map(|entry| (Some(entry.doc.to_owned()), manifest()))
        .or_else(|| {
            prelude_doc(name).map(|doc| {
                let ty = scheme_of(scope.prelude_binding(name)).or_else(manifest);
                (Some(doc.to_owned()), ty)
            })
        })
        .or_else(|| library_doc().map(|doc| (Some(doc), manifest())))
        .unwrap_or_default()
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

/// Which registry owns a name — the frame that would run, or the file
/// dispatch would exec.  Both halves of `explain` read this one answer: the
/// source line prints it, the doc ladder asks that registry for a doc.
enum Where {
    Local,
    Prelude,
    Alias,
    Handler,
    BaseFrame,
    Builtin,
    Path(PathBuf),
    DeniedByGrant(PathBuf),
}

impl fmt::Display for Where {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("local"),
            Self::Prelude => f.write_str("prelude"),
            Self::Alias => f.write_str("alias"),
            Self::Handler => f.write_str("handler"),
            // A base frame reads as the builtin it is: that it answers through
            // the stack rather than the manifest is what keeps it from
            // reporting as a handler someone installed, and its own doc and
            // argv type say the rest.  The reader needs no third word.
            Self::BaseFrame | Self::Builtin => f.write_str("builtin"),
            Self::Path(path) => write!(f, "{}", path.display()),
            Self::DeniedByGrant(path) => write!(f, "denied by grant ({})", path.display()),
        }
    }
}

/// Every registry holding `name`, in the order the runtime resolves it: the
/// first is what runs, the rest are shadowed.  That tail is the whole answer
/// `which` cannot give — a PATH binary this name will never reach.
///
/// Probes handlers before the manifest — the reverse of
/// `command_call::resolve`'s env-first order — so a handler under a native's
/// name reports as `handler` though only `^name` reaches it.
fn locate_all(name: &str, scope: &Env, shell: &Shell) -> Vec<Where> {
    let mut sites = Vec::new();
    if scope.session_binding(name).is_some() {
        sites.push(Where::Local);
    }
    if scope.prelude_binding(name).is_some() {
        sites.push(Where::Prelude);
    }
    // An alias is a handler frame too, so it must be named before the stack
    // answers generically; a base frame is one the stack carries below every
    // run frame, which is what tells it from a handler stacked over it.
    let stacked = if shell.has_alias(name) {
        Some(Where::Alias)
    } else {
        shell.lookup_handler(name).map(|found| match found {
            HandlerLookup::Frame(..) => Where::Handler,
            HandlerLookup::Base(..) => Where::BaseFrame,
        })
    };
    // A base frame is a manifest row seen through the stack, so naming it off
    // the manifest as well would report one frame as two.
    let seen_as_frame = matches!(stacked, Some(Where::BaseFrame));
    sites.extend(stacked);
    if !seen_as_frame && shell.lookup_builtin(name).is_some() {
        sites.push(Where::Builtin);
    }
    if let Some(path) = shell.locate_command(name) {
        sites.push(if grant_admits(name, shell) {
            Where::Path(path)
        } else {
            Where::DeniedByGrant(path)
        });
    }
    sites
}

/// Whether a grant admits `name` as a command head — dispatch's own admission
/// rule, read a second time here, so `explain` can say `denied by grant`
/// rather than name a file the shell would refuse to exec.
fn grant_admits(name: &str, shell: &Shell) -> bool {
    let head = if name.contains('/') {
        CommandName::Path(name.into())
    } else {
        CommandName::Bare(name.into())
    };
    let id = CommandIdentity::resolve(head, &shell.mobile.context);
    crate::capability::admits_head(&shell.mobile.context, &id)
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
        let env = shell.mobile.scope.clone();
        builtin_help(&[], &env, &mut shell);
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
        let env = shell.mobile.scope.clone();
        builtin_help(&[], &env, &mut shell);
        let help_out =
            String::from_utf8(crate::io::take_buffer(&buf)).expect("help output is UTF-8");
        assert!(
            help_out.contains("Library:") && help_out.contains("frob"),
            "help must list the installed library entry, got:\n{help_out}"
        );

        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_explain(&[Value::String("frob".into())], &env, &mut shell);
        let explain_out =
            String::from_utf8(crate::io::take_buffer(&buf)).expect("explain output is UTF-8");
        assert!(
            explain_out.contains("frob the widget"),
            "explain must resolve the installed library doc, got:\n{explain_out}"
        );
    }

    /// The index prints a doc's lead paragraph; `explain` prints the whole thing.
    #[test]
    fn index_shows_summary_explain_shows_full_doc() {
        let mut shell = Shell::default();
        shell.install_library_docs(vec![(
            "frob".to_string(),
            "frob the widget.\n\nHandle with care: it is load-bearing.".to_string(),
        )]);

        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        let env = shell.mobile.scope.clone();
        builtin_help(&[], &env, &mut shell);
        let help_out =
            String::from_utf8(crate::io::take_buffer(&buf)).expect("help output is UTF-8");
        assert!(
            help_out.contains("frob the widget.") && !help_out.contains("load-bearing"),
            "help's index row must carry only the lead paragraph, got:\n{help_out}"
        );

        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_explain(&[Value::String("frob".into())], &env, &mut shell);
        let explain_out =
            String::from_utf8(crate::io::take_buffer(&buf)).expect("explain output is UTF-8");
        assert!(
            explain_out.contains("frob the widget.") && explain_out.contains("load-bearing"),
            "explain must print the doc in full, got:\n{explain_out}"
        );
    }
}
