//! `help` prints the language and command index.
//!
//! `explain <name>` resolves one language form, user definition, builtin,
//! prelude function, or host-installed library entry to its documentation and
//! place in the shell.

use crate::ansi::{BOLD, CYAN, DIM, RESET};
use crate::ir::CommandName;
use crate::path::shown;
use crate::prelude_manifest::PRELUDE_DOCS;
use crate::runtime::command::Head;
use crate::terminal;
use crate::typecheck::builtin_type_hint;
use crate::types::{Binding, HandlerLookup, Settled, Shell, Value};
use std::fmt::{self, Write};
use std::path::PathBuf;

/// Syntax that the parser, rather than a binding, provides.
///
/// Keep this vocabulary paired with `syntax::is_keyword` and
/// `LANGUAGE_CONSTANTS`; the test below makes drift loud.
static LANGUAGE_DOCS: &[(&str, &str)] = &[
    (
        "audit",
        "audit { body }  — run a block and return its outcome and a structured trail instead of failing.\n\n\
         The result is `[outcome: `ok value | `err error, trail: observations]`. The trail records commands, reads, writes, workers, host acts, and denied capability checks performed by the body. Each command observation keeps `argv`, `status`, `stdout`, `stderr`, and total byte counts; each stream retains at most 1 MiB.\n\n\
         Use `succeeded $report` for a Boolean and `commands $report` for just the command observations. Inspect a failure with `case $report[outcome] [...]`. Output still flows to the surrounding pipe or redirect while it is recorded.\n\n\
         An uncaught failure stops the body; wrap individual calls in `attempt` when later calls should still run.\n\n\
         Example:\n  let report = audit { grep -c ERROR app.log }\n  case $report[outcome] [\n    `ok: { |_| bytes-to-string !{commands $report}[0][stdout] },\n    `err: { |e|\n      if $[$e[reason] == `exited 1] { 'no matches' } else { fail $e }\n    }\n  ]",
    ),
    (
        "case",
        "case <variant> [`tag: { |payload| body }, ...]  — run the arm matching a variant's tag.\n\n\
         Write every arm at the `case`; the checker uses that fixed set to prove the match exhaustive. Each arm is a one-argument block, or a name holding one. A tag without a payload passes `()`. All arms must return compatible results.\n\n\
         Example:\n  case $result [\n    `ok:  { |value| return $value },\n    `err: { |error| fail $error },\n  ]",
    ),
    (
        "else",
        "else { body }  — supply the fallback branch of an `if`.\n\n\
         It follows the final `if` or `elsif` branch. The body is a block, or a name holding one, and runs only when every preceding condition is false. With `else`, every branch must return a compatible result. See `explain if` for the complete form.",
    ),
    (
        "elsif",
        "elsif <condition> { body }  — add another condition and branch to an `if`.\n\n\
         Conditions are tested in order and must be `Bool`; only the first true branch runs. The body is a block, or a name holding one. See `explain if` for the complete form.",
    ),
    (
        "false",
        "false  — the Boolean false value.\n\n\
         It is a successful value, not a failure: `?` does not react to it and `try` does not catch it. It selects another `if` branch, and a top-level result of `false` gives the process status 1.",
    ),
    (
        "gather",
        "gather { body }  — run a block that owns the workers spawned inside it: nothing it started outlives it.\n\n\
         A `spawn` inside `gather` is the gather's: its node hangs under this expression, so an interrupt or a timeout on the run reaches it. When the block returns, every such worker whose result no one has claimed is awaited in birth order, as `await` would: its output joins the block's, and the first failure, in birth order, is the block's, the rest cancelled and drained. A raise, an exit or a stop cancels and drains them. A worker's body is a gather of its own. So a gather leaves nothing running.\n\n\
         Example:\n  gather {\n    let b = spawn \"build\" { cargo build }\n    let l = spawn \"lint\"  { cargo clippy }\n    return [build: !{await $b}, lint: !{await $l}]\n  }",
    ),
    (
        "grant",
        "grant [capabilities] { body }  — run a block with reduced authority.\n\n\
         A grant may contain `exec`, `fs`, `net`, `editor`, and `shell`. Omitted fields inherit their surrounding authority. A written field restricts it, nested grants intersect, and no grant can restore authority withheld outside it. `grant [] { body }` changes nothing. Work spawned inside inherits the active grant.\n\n\
         The outer brackets are syntax, not a collection literal: write the option names there, with no bound bundle or spread (`grant $caps { ... }` is a parse error). Option values may be computed; a misspelled option is a static error. `[]` is the empty option set; `[:]` names no options either. The capability fields also describe the record an exarch sub-agent's `grant: `restrict` takes.\n\n\
         `exec` is `[allow: targets, deny: targets]`; once present, only allowed targets may run and any deny wins. A target is a bare command (`name git), one executable at a place (`cwd tools/lint), or every executable under a place or selector (`dir `cwd tools, `dir `path, `dir `system, or a package manager such as `dir `rustup or `dir `homebrew). There is no argument filtering.\n\n\
         `fs` is `[read: places, write: places, deny: places]`; `write` also permits reading, and `deny` wins. A present `fs` table grants no unlisted reads or writes. Places are tagged anchors: `absolute /path, `cwd relative, `home relative, `tempdir relative, `gitdir relative, and `xdg-config relative, `xdg-data relative, `xdg-cache relative, `xdg-state relative, or `xdg-bin relative. Relative anchors and symlinks are resolved when the grant begins.\n\n\
         Write `net: `offline` to deny child networking. `editor` is a list drawn from `read, `write, and `tui. `shell` is a list drawn from `chdir and `detach. Lists are sets. An empty written list withholds every verb in that dimension; `shell: [`chdir]` withholds `detach`.\n\n\
         Allow named commands and system executables, exclude `rm` and `.git`, and write only under `build`:\n  grant [\n    exec: [allow: [`name git, `name rg, `dir `system], deny: [`name rm]],\n    fs: [read: [`cwd .], write: [`cwd build], deny: [`cwd .git]],\n    net: `offline,\n    editor: [`read, `write, `tui],\n    shell: [`chdir],\n  ] {\n    rg TODO src\n  }\n\n\
         Compute a path while keeping the option names explicit; `.` names an anchor itself:\n  let subtree = 'src'\n  grant [fs: [read: [`cwd $subtree]], net: `offline] {\n    from-string < \"$subtree/lib.rs\"\n  }",
    ),
    (
        "guard",
        "guard { body } { cleanup }  — run cleanup after body, whether body succeeds or fails.\n\n\
         When cleanup succeeds, `guard` returns the body's value or continues its failure. A cleanup that fails or exits takes precedence over the body's result. Use `attempt { ... }` inside cleanup when cleanup failure should be ignored.\n\n\
         Example:\n  guard { use-resource } { attempt { release-resource } }",
    ),
    (
        "if",
        "if <Bool> { body } [elsif <Bool> { body } ...] [else { body }]  — choose the first true branch.\n\n\
         Conditions are evaluated in order. Branch bodies are blocks, or names holding blocks, and unchosen bodies do not run. With `else`, all branches must return compatible results. Without `else`, the form is for effects and returns `()`. Bind a computed block first, then name it as the body.\n\n\
         Example:\n  if $[$n == 0] { 'zero' }\n  elsif $[$n > 0] { 'positive' }\n  else { 'negative' }",
    ),
    (
        "let",
        "let <pattern> = <command>  — evaluate a command and bind its result for the rest of the surrounding scope.\n\n\
         Use the binding as `$name`; a bare name in command position applies it. External-command output is captured as a `String` unless a final decoder such as `from-lines` or `from-json` returns another value. Patterns may destructure lists and records: `let [head, ...tail] = $items`. Bindings are immutable, but a later binding may shadow an earlier one.\n\n\
         Example:\n  let files = glob #'**/*.rs'#\n  let counts = map { |file| line-count $file } $files",
    ),
    (
        "return",
        "return [value]  — finish successfully with a value; without one, return `()`.\n\n\
         `return` constructs a successful computation; it does not exit the process and does not escape an enclosing function early. A block's result is the final computation it runs, so `return` is most useful when lifting data explicitly or returning unit from an otherwise empty path.\n\n\
         In an rc file, plugin manifest, or capability profile whose terminal result a host checks against a fixed keyset, return a record: `return [edit-mode: 'vi']`. A misspelled key is a type error before the file runs. `return [\"edit-mode\": 'vi']` is a map, and `[]` is a list; neither is a configuration record. A file with nothing to set returns `()`.\n\n\
         Example:\n  let twice = { |n| return $[$n * 2] }",
    ),
    (
        "spawn",
        "spawn \"title\" { body }  — run a block on a worker thread and return its handle at once.\n\n\
         The title names the work to people: it appears in `workers`, in the birth and completion notices, and is required by the exarch host (the interactive shell accepts `spawn { body }`, naming the worker by its line). The worker is owned by the `gather` or the worker it was started inside, else by the session: it lives until it finishes, is cancelled, or the session ends, and a worker finishes only after every worker it started has. `await $h` behaves as if the block had run there; `poll $h` samples it; `cancel $h` stops it and everything it started; `worker <id>` recovers a handle whose binding is gone.\n\n\
         Example:\n  let h = spawn \"sort the log\" { sort huge.log > sorted.log }\n  …\n  await $h",
    ),
    (
        "true",
        "true  — the Boolean true value.\n\n\
         It is a successful value and selects an `if` branch. A top-level result of `true` gives the process status 0.",
    ),
    (
        "try",
        "try { body } { |error| handler }  — recover from a failure raised while running body.\n\n\
         On success, return the body's value without running the handler. On failure, call the handler with a record with fields `cmd`, `status`, `reason`, `message`, and `site`; branch on `reason`, and use `fail $error` to re-raise. `site` is `some` carrying a record with fields `script`, `line`, and `col`, or `none` when no source location is available. Body and handler must return compatible results. Output is not captured: use `audit` when stdout, stderr, or the full trail is needed. Exits and internal control escapes bypass the handler.\n\n\
         Example:\n  try { from-string < config.txt } { |error|\n    warn \"config: $error[message]\"\n    return #'{}'#\n  }",
    ),
    (
        "within",
        "within [options] { body }  — run a block with temporary shell context.\n\n\
         Options are `dir: path`, `env: map`, `handlers: [name: handler, ...]`, and `handler: catch-all`. `dir` changes the logical working directory; `env` overlays variables, with `unset removing one. Named handlers receive the command's argument list. The catch-all receives the unresolved command name and then its argument list. Options compose, and the previous context returns when the body finishes.\n\n\
         The outer brackets are syntax: write option names in place, with computed values such as `within [dir: $d] { ... }`. A bound option bundle or spread is a parse error; a misspelled option is a static error. `[]` and `[:]` both name no options.\n\n\
         Write the `handlers:` table in place too: its names are bound in the body, so a bound table or spread is a parse error. Each entry is a one-argument block or a name holding one. The catch-all `handler:` is ordinary data: a two-argument function, written inline or bound, that intercepts every external command reached by a bare name. `^name` bypasses handlers and runs the binary. A handler is a stub, not a sandbox.\n\n\
         Example:\n  within [\n    dir: project,\n    env: [MODE: 'test', TOKEN: `unset],\n    handlers: [notify: { |args| echo ...$args }],\n  ] {\n    notify building\n    make check\n  }",
    ),
];

static LANGUAGE_GUIDES: &[(&str, &str)] = &[(
    "redirects",
    "Redirects bind a computation's byte streams.\n\n\
         `< file` reads stdin from a file; `<< text` feeds a string, dropping one newline at its very start. `> file` writes stdout atomically, `>> file` appends, and `>~ file` truncates and streams output directly. `2> file` and `2>~ file` truncate and stream stderr, while `2>> file` appends it. `2>&1` sends stderr wherever stdout goes, whichever side of `> file` it is written.\n\n\
         Each stream takes at most one redirect: `cmd > a > b` and `cmd 2> e 2>&1` are parse errors. There is no general fd plumbing: `1<`, `1>`, `0<`, `0>`, `1>&2`, and descriptors past 2 are refused. `echo 1>file` is refused rather than writing an empty file. Use `warn \"message\"` to write one line to stderr and return unit.\n\n\
         Example:\n  sort in.txt > sorted.txt 2> errors.txt\n\n\
         See `explain from-string` and `explain to-string` for reading and writing text values.",
)];

/// A checked `let` installs its generalised scheme beside the value, so a
/// definition's most general type reads straight off its binding — the baked
/// prelude's `Bind` nodes carry theirs too.  `None` for a binding from an
/// unchecked path, which has no scheme to show.
fn scheme_of(binding: Option<&Binding>) -> Option<String> {
    binding?.scheme.as_deref().map(ToString::to_string)
}

fn prelude_doc(name: &str) -> Option<&'static str> {
    PRELUDE_DOCS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|&(_, doc)| doc)
}

fn language_doc(name: &str) -> Option<&'static str> {
    LANGUAGE_DOCS
        .iter()
        .chain(LANGUAGE_GUIDES)
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

/// Every documented registry, in `help`'s print order: language syntax, the
/// builtin manifest, the prelude, then whatever a host installed with
/// [`Runtime::install_library_docs`] — which is empty until one does.
/// `explain`'s fallback search sweeps the same four.
///
/// A leading `_` marks a name internal: it is kept out of the index and out
/// of the search, though `explain` named it exactly still answers.
fn registries(shell: &Shell) -> [(&'static str, Vec<(&str, &str)>); 4] {
    [
        (
            "Language",
            by_name(
                LANGUAGE_DOCS
                    .iter()
                    .chain(LANGUAGE_GUIDES)
                    .copied()
                    .collect(),
            ),
        ),
        (
            "Builtins",
            by_name(
                shell
                    .builtin_names()
                    .filter(|name| !name.starts_with('_'))
                    .filter_map(|name| {
                        shell
                            .lookup_builtin(name)
                            .map(|entry| (name, entry.decl.doc))
                    })
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
                    .runtime
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
        if terminal::ui_color() {
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

/// One `explain` entry: a readable guide, then labelled facts about the name.
fn fmt_entry(
    name: &str,
    doc: Option<&str>,
    ty: Option<&str>,
    tail: &[String],
    Colors {
        bold,
        cyan,
        dim,
        reset,
    }: Colors,
) -> String {
    let mut out = format!("  {bold}{cyan}{name}{reset}\n");
    if let Some(doc) = doc {
        let (usage, description) = doc
            .split_once("  — ")
            .map_or((None, doc), |(usage, description)| {
                (Some(usage), description)
            });
        if let Some(usage) = usage {
            let _ = writeln!(out, "  {dim}Usage:{reset} {usage}");
        }
        let _ = writeln!(out, "  {}", description.replace('\n', "\n  "));
    }
    if doc.is_some() && (ty.is_some() || !tail.is_empty()) {
        out.push('\n');
    }
    if let Some(ty) = ty {
        let _ = writeln!(out, "  {dim}Type:{reset} {ty}");
    }
    for line in tail {
        let _ = writeln!(out, "  {dim}{line}{reset}");
    }
    out.push('\n');
    out
}

/// One row of the `help` index.
fn fmt_line(
    name: &str,
    doc: &str,
    Colors {
        cyan, dim, reset, ..
    }: Colors,
) -> String {
    let summary = lead_paragraph(doc);
    match summary.strip_prefix(name) {
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => {
            format!("  {cyan}{name}{reset}{rest}\n")
        }
        _ => format!("  {cyan}{name}{reset} {dim}—{reset} {summary}\n"),
    }
}

pub(super) fn builtin_help(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
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
         {dim}Use `explain <name>` for the full guide, type when applicable, source, and shadowing details.{reset}\n"
    );

    shell.write_stdout(out.as_bytes())?;
    Ok(Value::Unit)
}

pub(super) fn builtin_explain(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let out = match args.first() {
        Some(arg) => explanation(&arg.to_string(), shell, Colors::current()),
        None => "explain: expected a name, e.g. `explain map`\n".to_string(),
    };
    shell.write_stdout(out.as_bytes())?;
    Ok(Value::Unit)
}

/// What documents `name` over the frame that would run.  A name nothing binds
/// and no registry documents falls to a search of the whole index; everything
/// else prints the one entry block. Optional facts are omitted when no registry
/// holds them, rather than rendered as unexplained placeholders.
fn explanation(name: &str, shell: &Shell, colors: Colors) -> String {
    if let Some(doc) = language_doc(name) {
        return fmt_entry(
            name,
            Some(doc),
            None,
            &["Source: language syntax".to_string()],
            colors,
        );
    }
    let sites = locate_all(name, shell);
    let (doc, ty) = documented(name, sites.first(), shell);
    if sites.is_empty() && doc.is_none() && ty.is_none() {
        return search(name, shell, colors);
    }
    let mut tail: Vec<String> = sites
        .first()
        .map(|site| format!("Source: {site}"))
        .into_iter()
        .collect();
    if let [_, shadowed @ ..] = sites.as_slice()
        && !shadowed.is_empty()
    {
        let names: Vec<String> = shadowed.iter().map(Where::to_string).collect();
        tail.push(format!("Shadows: {}", names.join(", ")));
    }
    fmt_entry(name, doc.as_deref(), ty.as_deref(), &tail, colors)
}

/// The doc and type held by the registry that owns `name`.
///
/// A session binding answers alone: it shadows every other resolution at
/// runtime, so no registry below it may speak for it, and its doc can only be
/// the library table's — that is the one registry naming the bindings a
/// sourced library installs.
///
/// Every other site sweeps the documented registries, since a frame stacked
/// over a native — a handler, an alias — inherits the native's doc.
fn documented(name: &str, site: Option<&Where>, shell: &Shell) -> (Option<String>, Option<String>) {
    let manifest = || builtin_type_hint(shell.session.runtime.builtins.manifest(), name);
    let library_doc = || shell.session.runtime.library_docs.get(name).cloned();

    if matches!(site, Some(Where::Session)) {
        return (library_doc(), scheme_of(shell.env.binding(name)));
    }
    shell
        .lookup_builtin(name)
        .map(|entry| (Some(entry.decl.doc.to_owned()), manifest()))
        .or_else(|| {
            prelude_doc(name).map(|doc| {
                let ty = scheme_of(shell.sig.prelude_binding(name)).or_else(manifest);
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
    Session,
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
            Self::Session => f.write_str("session"),
            Self::Prelude => f.write_str("prelude"),
            Self::Alias => f.write_str("alias"),
            Self::Handler => f.write_str("handler"),
            // A base frame reads as the builtin it is: that it answers through
            // the stack rather than the manifest is what keeps it from
            // reporting as a handler someone installed, and its own doc and
            // argv type say the rest.  The reader needs no third word.
            Self::BaseFrame | Self::Builtin => f.write_str("builtin"),
            Self::Path(path) => write!(f, "{}", shown(path)),
            Self::DeniedByGrant(path) => write!(f, "denied by grant ({})", shown(path)),
        }
    }
}

/// Every registry holding `name`, in the order the runtime resolves it: the
/// first is what runs, the rest are shadowed.  That tail is the whole answer
/// `which` cannot give — a PATH binary this name will never reach.
///
/// Bare-head order (`command_call::resolve`'s): session, prelude, the value
/// half of the manifest (a native — the env hit), then the handler stack, then
/// `PATH`.  A native's own arm is unreachable from any bare head or `^` (A-1′),
/// so it reports as `builtin`, and an arm stacked over it reports separately,
/// under `Shadows:`.
fn locate_all(name: &str, shell: &Shell) -> Vec<Where> {
    let mut sites = Vec::new();
    if shell.env.binding(name).is_some() {
        sites.push(Where::Session);
    }
    if shell.sig.prelude_binding(name).is_some() {
        sites.push(Where::Prelude);
    }
    if shell
        .session
        .runtime
        .builtins
        .manifest()
        .value(name)
        .is_some()
    {
        sites.push(Where::Builtin);
    }
    // An alias is a handler frame too, so it must be named before a bare base
    // frame answers generically; the stack's winner is `BaseFrame` only when
    // nothing shadows it, which is what tells it from an arm stacked over one.
    let stacked = if shell.has_alias(name) {
        Some(Where::Alias)
    } else {
        shell.lookup_handler(name).map(|found| match found {
            HandlerLookup::Frame(..) => Where::Handler,
            HandlerLookup::Base(..) => Where::BaseFrame,
        })
    };
    let seen_as_frame = matches!(stacked, Some(Where::BaseFrame));
    sites.extend(stacked);
    // An arm stacked over a base frame shadows it in the stack's own lookup,
    // which returns only the winner — so the frame underneath is named here,
    // by its own argv-half manifest row, independently of the stack's answer.
    if !seen_as_frame
        && shell
            .session
            .runtime
            .builtins
            .manifest()
            .base_frames()
            .any(|e| e.name == name)
    {
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
    let Ok(program) = Head::resolve(&CommandName::of(name), &shell.context).program else {
        return true;
    };
    shell.context.grants.admits(&program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_docs_cover_every_keyword_and_constant() {
        let mut expected: Vec<&str> = crate::syntax::CONTROL_OPERATORS.into_iter().collect();
        expected.extend(crate::syntax::FLOW_KEYWORDS);
        expected.extend(
            crate::typecheck::builtins::LANGUAGE_CONSTANTS
                .iter()
                .map(|&(name, _)| name),
        );
        expected.sort_unstable();

        let documented: Vec<&str> = LANGUAGE_DOCS.iter().map(|&(name, _)| name).collect();
        assert_eq!(documented, expected);
    }

    /// `build.rs` scrapes these summaries out of `prelude.ral`; the rule worth
    /// pinning is how a wrapped doc joins.
    #[test]
    fn multiline_doc_summary_joins_first_paragraph() {
        let doc = prelude_doc("extension").expect("extension has a doc comment");
        assert!(
            doc.starts_with("The extension of a path's last component"),
            "summary starts at the first line, got {doc:?}"
        );
        assert!(
            doc.contains("`a.tar.gz` has `` `some 'gz'``, while"),
            "the wrapped sentence is joined in full, got {doc:?}"
        );
    }

    /// Library docs hang off the session, not a process-global registry, so
    /// a shell no host dressed shows no `Library:` section at all.
    #[test]
    fn bare_shell_help_has_no_library_section() {
        let mut shell = crate::test_helper::core_shell();
        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_help(&[], &mut shell).expect("a buffer sink cannot fail");
        let out = String::from_utf8(buf.take()).expect("help output is UTF-8");
        assert!(
            !out.contains("Library:"),
            "a bare shell must list no Library section, got:\n{out}"
        );
    }

    /// [`Runtime::install_library_docs`] is the one door onto
    /// `runtime.library_docs`.
    #[test]
    fn installed_library_docs_surface_in_help_and_explain() {
        let mut runtime = crate::test_helper::core_runtime();
        runtime.install_library_docs(vec![("frob".to_string(), "frob the widget".to_string())]);
        let mut shell = crate::Shell::root(runtime.into());

        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_help(&[], &mut shell).expect("a buffer sink cannot fail");
        let help_out = String::from_utf8(buf.take()).expect("help output is UTF-8");
        assert!(
            help_out.contains("Library:") && help_out.contains("frob"),
            "help must list the installed library entry, got:\n{help_out}"
        );

        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_explain(&[Value::string("frob")], &mut shell).expect("a buffer sink cannot fail");
        let explain_out = String::from_utf8(buf.take()).expect("explain output is UTF-8");
        assert!(
            explain_out.contains("frob the widget"),
            "explain must resolve the installed library doc, got:\n{explain_out}"
        );
    }

    /// The index prints a doc's lead paragraph; `explain` prints the whole thing.
    #[test]
    fn index_shows_summary_explain_shows_full_doc() {
        let mut runtime = crate::test_helper::core_runtime();
        runtime.install_library_docs(vec![(
            "frob".to_string(),
            "frob the widget.\n\nHandle with care: it is load-bearing.".to_string(),
        )]);
        let mut shell = crate::Shell::root(runtime.into());

        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_help(&[], &mut shell).expect("a buffer sink cannot fail");
        let help_out = String::from_utf8(buf.take()).expect("help output is UTF-8");
        assert!(
            help_out.contains("frob the widget.") && !help_out.contains("load-bearing"),
            "help's index row must carry only the lead paragraph, got:\n{help_out}"
        );

        let (sink, buf) = crate::io::new_buffer();
        shell.set_stdout(sink);
        builtin_explain(&[Value::string("frob")], &mut shell).expect("a buffer sink cannot fail");
        let explain_out = String::from_utf8(buf.take()).expect("explain output is UTF-8");
        assert!(
            explain_out.contains("frob the widget.") && explain_out.contains("load-bearing"),
            "explain must print the doc in full, got:\n{explain_out}"
        );
    }

    /// A block's own names are not the session's: `explain` answers only for
    /// the latter, from inside a block as at the top.
    #[test]
    fn explain_reflects_on_the_session_not_the_callers_block() {
        let mut shell = crate::test_helper::core_shell();
        shell.set_var("sess_name".into(), Value::Int(1));
        for (source, expected) in [
            ("!{ explain sess_name }", "Source: session"),
            (
                "let f = { |zqx_local| explain zqx_local }\nf 1",
                "explain: zqx_local: not found",
            ),
        ] {
            let (result, bytes, _) = crate::evaluator::with_capture(&mut shell, |shell| {
                crate::evaluator::run_source(source, shell)
            });
            result.expect("explain must run");
            let out = String::from_utf8(bytes).expect("explain output is UTF-8");
            assert!(
                out.contains(expected),
                "`{source}` must print `{expected}`, got:\n{out}"
            );
        }
    }
}
