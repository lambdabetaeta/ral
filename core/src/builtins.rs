//! Builtin command dispatch and registration.
//!
//! Builtins are commands implemented in Rust that run inside the shell
//! process.  Each builtin is registered in a single
//! [`builtin_registry!`] entry that names the builtin, its computation-
//! type hint, its one-line doc, and its runtime dispatch — so adding a
//! new builtin can update only one place and the four facets cannot
//! drift apart.
//!
//! The prelude (a ral script baked into the binary) is evaluated once
//! per process; its top-level bindings are cloned into every fresh
//! environment via [`register`].

use crate::diagnostic;
use crate::types::*;
use std::collections::HashMap;
use std::sync::OnceLock;

mod caps;
mod codecs;
mod collections;
mod concurrency;
mod control;
pub mod editor;
mod fs;
pub mod misc;
mod modules;
mod path;
pub mod plugin;
mod predicates;
mod scope;
mod shell;
mod strings;
pub use util::{value_to_json_audit, value_to_json_pub};
mod util;
pub mod uutils;

/// Computation-type hint for a builtin, consumed by the type checker to
/// determine how the command's return value flows through pipelines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinCompHint {
    /// Returns a first-class value (the common case).
    Value,
    /// Produces raw byte output (stdout-oriented commands like `echo`).
    Bytes,
    /// Result type is determined by the last thunk argument.
    LastThunk,
    /// Decodes byte input into a value (e.g. `from-json`).
    DecodeToValue,
    /// Encodes a value into byte output (e.g. `to-json`).
    EncodeToBytes,
    /// Diverging command: type is ∀α μ ν. F[μ,ν] α — unifies with any branch.
    Never,
}

/// Single source of truth for the runtime side of every builtin.
///
/// Every entry binds four facets at once: the user-visible names, the
/// computation-type hint consumed by the inference engine, the doc
/// string the `help` builtin prints, and a `call` block that produces
/// the runtime result.  The macro emits the `is_builtin` /
/// `builtin_doc` / `builtin_names` / `builtin_comp_hint` / `call`
/// public API directly from these entries, so all five views observe
/// the same registration and no out-of-band match table can drift from
/// the docs/types.
///
/// The `call` block runs inside a function whose locals are named
/// `args: &[Value]` and `shell: &mut Shell`; the lambda-like
/// `|args, shell| body` syntax is decoration only — it makes the
/// signature visible at every call site.  Each block must produce a
/// `Result<Option<Value>, EvalSignal>`.
macro_rules! builtin_registry {
    (
        $(
            $(#[$meta:meta])*
            $variant:ident {
                names: [$($name:literal),+ $(,)?],
                hint: $hint:ident,
                doc: $doc:literal,
                call: $call:expr,
            }
        ),+ $(,)?
    ) => {
        pub fn builtin_comp_hint(name: &str) -> Option<BuiltinCompHint> {
            match name {
                $($(#[$meta])* $($name)|+ => Some(BuiltinCompHint::$hint),)+
                _ => None,
            }
        }

        pub fn builtin_doc(name: &str) -> Option<&'static str> {
            match name {
                $($(#[$meta])* $($name)|+ => Some($doc),)+
                _ => None,
            }
        }

        pub fn builtin_names() -> &'static [&'static str] {
            &[
                $($(#[$meta])* $($name,)+)+
            ]
        }

        /// Call a builtin function.  Returns `Ok(None)` if `name` is not a
        /// registered builtin so callers can fall through to alias /
        /// external dispatch without inspecting an enum.
        ///
        /// Each registry entry's `call` field is a non-capturing closure
        /// `|args, shell| ...`; the match arm invokes it with the function's
        /// own parameters, side-stepping the macro-hygiene gap that otherwise
        /// keeps `$body` from naming locals introduced by the expansion.
        pub fn call(
            name: &str,
            args: &[Value],
            shell: &mut Shell,
        ) -> Result<Option<Value>, EvalSignal> {
            // Bind the closure to a typed local so a non-capturing
            // `|args, shell|` doesn't need per-entry annotations — the
            // `fn(...)` target drives parameter inference.
            type Handler = fn(&[Value], &mut Shell) -> Result<Option<Value>, EvalSignal>;
            match name {
                $(
                    $(#[$meta])*
                    $($name)|+ => {
                        let handler: Handler = $call;
                        handler(args, shell)
                    }
                )+
                _ => Ok(None),
            }
        }
    };
}

builtin_registry! {
    Echo { names: ["echo"], hint: Bytes,
        doc: "echo <args...>  — write arguments to stdout.",
        call: |args, shell| Ok(Some(misc::builtin_echo(args, shell))), },
    Warn { names: ["_warn"], hint: Bytes,
        doc: "_warn <args...>  — write arguments to stderr.",
        call: |args, _shell| Ok(Some(misc::builtin_warn(args))), },
    Each { names: ["_each"], hint: Value,
        doc: "_each <list> <fn>  — call fn on each element for side effects.",
        call: |args, shell| collections::builtin_each(args, shell).map(Some), },
    Map { names: ["map"], hint: Value,
        doc: "map <fn> <list>  — apply fn to each element, return new list.",
        call: |args, shell| collections::builtin_map(args, shell).map(Some), },
    Filter { names: ["filter"], hint: Value,
        doc: "filter <fn> <list>  — keep elements where fn returns true.",
        call: |args, shell| collections::builtin_filter(args, shell).map(Some), },
    SortList { names: ["sort-list"], hint: Value,
        doc: "sort-list <list>  — sort a list lexicographically.",
        call: |args, _shell| collections::builtin_sort(args).map(Some), },
    SortListBy { names: ["sort-list-by"], hint: Value,
        doc: "sort-list-by <fn> <list>  — sort by a key function.",
        call: |args, shell| collections::builtin_sort_by(args, shell).map(Some), },
    Fold { names: ["_fold"], hint: Value,
        doc: "_fold <list> <init> <fn>  — reduce list left-to-right with fn and accumulator.",
        call: |args, shell| collections::builtin_fold(args, shell).map(Some), },
    Try { names: ["_try"], hint: Value,
        doc: "_try <thunk>  — run thunk; return {ok, value, status, cmd, stderr, line, col} record.",
        call: |args, shell| control::builtin_try(args, shell).map(Some), },
    TryWith { names: ["try"], hint: Value,
        doc: "try <body> <handler>  — run body; on failure call handler with error record.",
        call: |args, shell| control::builtin_try_with(args, shell).map(Some), },
    TryApply { names: ["_try-apply"], hint: Value,
        doc: "_try-apply <f> <val>  — apply f to val; on parameter-pattern mismatch return [ok:false,value:unit], else [ok:true,value:result].",
        call: |args, shell| control::builtin_try_apply(args, shell).map(Some), },
    Guard { names: ["guard"], hint: Value,
        doc: "guard <body> <cleanup>  — run body, then cleanup regardless of outcome.",
        call: |args, shell| control::builtin_guard(args, shell).map(Some), },
    Audit { names: ["audit"], hint: Value,
        doc: "audit <thunk>  — run thunk and record its execution tree.",
        call: |args, shell| control::builtin_audit(args, shell).map(Some), },
    Fail { names: ["fail"], hint: Never,
        doc: "fail <status>  — exit with error status.",
        call: |args, _shell| Err(misc::builtin_fail(args)), },
    Len { names: ["length"], hint: Value,
        doc: "length <val>  — number of elements in a string, bytes, list, or map.",
        call: |args, _shell| strings::builtin_len(args).map(Some), },
    Upper { names: ["upper"], hint: Value,
        doc: "upper <s>  — convert a string to uppercase.",
        call: |args, _shell| strings::builtin_upper(args).map(Some), },
    Lower { names: ["lower"], hint: Value,
        doc: "lower <s>  — convert a string to lowercase.",
        call: |args, _shell| strings::builtin_lower(args).map(Some), },
    Dedent { names: ["dedent"], hint: Value,
        doc: "dedent <s>  — strip common leading whitespace from every non-empty line.",
        call: |args, _shell| strings::builtin_dedent(args).map(Some), },
    Intercalate { names: ["intercalate"], hint: Value,
        doc: "intercalate <sep> <items>  — interpose sep between every pair of items, concatenated as one string.",
        call: |args, _shell| strings::builtin_join(args).map(Some), },
    Slice { names: ["slice"], hint: Value,
        doc: "slice <s> <start> <count>  — extract a substring by character offset.",
        call: |args, _shell| strings::builtin_slice(args).map(Some), },
    Split { names: ["split"], hint: Value,
        doc: "split <pattern> <s>  — split a string by a regex pattern.",
        call: |args, _shell| strings::builtin_split(args).map(Some), },
    Match { names: ["match"], hint: Value,
        doc: "match <pattern> <s>  — true if regex pattern matches anywhere in s.",
        call: |args, shell| strings::builtin_match(args, shell).map(Some), },
    FindMatch { names: ["find-match"], hint: Value,
        doc: "find-match <pattern> <s>  — first regex match, or fail if none.",
        call: |args, _shell| strings::builtin_find_match(args).map(Some), },
    FindMatches { names: ["find-matches"], hint: Value,
        doc: "find-matches <pattern> <s>  — all non-overlapping regex matches as a list.",
        call: |args, _shell| strings::builtin_find_matches(args).map(Some), },
    Replace { names: ["replace"], hint: Value,
        doc: "replace <pattern> <repl> <s>  — replace first regex match; $1 etc. backreferences.",
        call: |args, _shell| strings::builtin_replace(args).map(Some), },
    ReplaceAll { names: ["replace-all"], hint: Value,
        doc: "replace-all <pattern> <repl> <s>  — replace every regex match.",
        call: |args, _shell| strings::builtin_replace_all(args).map(Some), },
    ShellQuote { names: ["shell-quote"], hint: Value,
        doc: "shell-quote <s>  — quote a string for safe shell-word use.",
        call: |args, _shell| strings::builtin_shell_quote(args).map(Some), },
    ShellSplit { names: ["shell-split"], hint: Value,
        doc: "shell-split <s>  — split a shell-quoted string into a list of words.",
        call: |args, _shell| strings::builtin_shell_split(args).map(Some), },
    Keys { names: ["keys"], hint: Value,
        doc: "keys <map>  — list of map keys.",
        call: |args, _shell| predicates::builtin_keys(args).map(Some), },
    Has { names: ["has"], hint: Value,
        doc: "has <map> <key>  — true if map contains key.",
        call: |args, shell| predicates::builtin_has(args, shell).map(Some), },
    Path { names: ["_path"], hint: Value,
        doc: "_path <op> <path>  — path ops: stem ext dir base resolve join.",
        call: |args, shell| path::builtin_path(args, shell).map(Some), },
    Glob { names: ["glob"], hint: Value,
        doc: "glob <pattern>  — list paths matching a glob pattern.",
        call: |args, shell| fs::builtin_glob(args, shell).map(Some), },
    Within { names: ["within"], hint: LastThunk,
        doc: "within <opts> <thunk>  — run thunk with scoped shell, dir, and/or effect handlers.\n  Keys: shell (map), dir (path), handlers (map of name→thunk), handler (catch-all thunk).",
        call: |args, shell| scope::builtin_within(args, shell).map(Some), },
    Grant { names: ["grant"], hint: LastThunk,
        doc: "grant <caps> <thunk>  — run thunk with restricted capabilities.",
        call: |args, shell| scope::builtin_grant(args, shell).map(Some), },
    Exit { names: ["exit", "quit"], hint: Value,
        doc: "exit [status]  — exit the shell.",
        call: |args, shell| misc::builtin_exit(args, shell), },
    FoldLines { names: ["fold-lines"], hint: DecodeToValue,
        doc: "fold-lines <fn> <init>  — fold over stdin lines.",
        call: |args, shell| codecs::builtin_fold_lines(args, shell).map(Some), },
    FromBytes { names: ["from-bytes"], hint: DecodeToValue,
        doc: "from-bytes [input]  — read raw bytes from stdin (or arg) as Bytes.",
        call: |args, shell| codecs::builtin_from_bytes(args, shell).map(Some), },
    FromString { names: ["from-string"], hint: DecodeToValue,
        doc: "from-string [input]  — decode UTF-8 bytes from stdin (or arg) to a String.",
        call: |args, shell| codecs::builtin_from_string(args, shell).map(Some), },
    FromLine { names: ["from-line"], hint: DecodeToValue,
        doc: "from-line [input]  — decode UTF-8 bytes, stripping one trailing newline.",
        call: |args, shell| codecs::builtin_from_line(args, shell).map(Some), },
    FromLines { names: ["from-lines"], hint: DecodeToValue,
        doc: "from-lines [input]  — decode bytes to a Step stream of lines (lossy on invalid UTF-8).",
        call: |args, shell| codecs::builtin_from_lines(args, shell).map(Some), },
    FromJson { names: ["from-json"], hint: DecodeToValue,
        doc: "from-json [input]  — decode JSON bytes from stdin (or arg) to a value.",
        call: |args, shell| codecs::builtin_from_json(args, shell).map(Some), },
    ToBytes { names: ["to-bytes"], hint: EncodeToBytes,
        doc: "to-bytes <value>  — pass Bytes (or list of Ints) through to the byte channel.",
        call: |args, shell| codecs::builtin_to_bytes(args, shell).map(Some), },
    ToString { names: ["to-string"], hint: EncodeToBytes,
        doc: "to-string <value>  — encode a value's String form to the byte channel.",
        call: |args, shell| codecs::builtin_to_string(args, shell).map(Some), },
    ToLine { names: ["to-line"], hint: EncodeToBytes,
        doc: "to-line <value>  — encode value with a trailing newline (inverse of from-line).",
        call: |args, shell| codecs::builtin_to_line(args, shell).map(Some), },
    ToLines { names: ["to-lines"], hint: EncodeToBytes,
        doc: "to-lines <list>  — newline-join the list elements to the byte channel.",
        call: |args, shell| codecs::builtin_to_lines(args, shell).map(Some), },
    ToJson { names: ["to-json"], hint: EncodeToBytes,
        doc: "to-json <value>  — encode a value as JSON bytes.",
        call: |args, shell| codecs::builtin_to_json(args, shell).map(Some), },
    Ask { names: ["ask"], hint: Value,
        doc: "ask <prompt>  — prompt for interactive input, return string.",
        call: |args, _shell| misc::builtin_ask(args).map(Some), },
    Source { names: ["source"], hint: Value,
        doc: "source <file>  — execute a .ral script file.",
        call: |args, shell| modules::builtin_source(args, shell).map(Some), },
    Use { names: ["use"], hint: Value,
        doc: "use <file>  — load a .ral module (cached).",
        call: |args, shell| modules::builtin_use(args, shell).map(Some), },
    Which { names: ["which"], hint: Value,
        doc: "which <name>  — find executable in PATH.",
        call: |args, shell| path::builtin_which(args, shell).map(Some), },
    Cwd { names: ["cwd"], hint: Value,
        doc: "cwd  — return the current working directory as a String.",
        call: |_args, shell| Ok(Some(Value::String(shell.resolve_path(".").to_string_lossy().into_owned()))), },
    Chdir { names: ["cd"], hint: Value,
        doc: "cd [path]  — change the shell working directory; gated by shell.chdir capability. Empty/missing path means $HOME.",
        call: |args, shell| shell::builtin_chdir(args, shell).map(Some), },
    Exists { names: ["exists"], hint: Value,
        doc: "exists <path>  — true if path exists.",
        call: |args, shell| fs::builtin_fs_pred(args, |p| p.exists(), shell).map(Some), },
    IsFile { names: ["is-file"], hint: Value,
        doc: "is-file <path>  — true if path is a regular file.",
        call: |args, shell| fs::builtin_fs_pred(args, |p| p.is_file(), shell).map(Some), },
    IsDir { names: ["is-dir"], hint: Value,
        doc: "is-dir <path>  — true if path is a directory.",
        call: |args, shell| fs::builtin_fs_pred(args, |p| p.is_dir(), shell).map(Some), },
    IsLink { names: ["is-link"], hint: Value,
        doc: "is-link <path>  — true if path is a symbolic link.",
        call: |args, shell| fs::builtin_fs_pred(args, |p| p.is_symlink(), shell).map(Some), },
    IsReadable { names: ["is-readable"], hint: Value,
        doc: "is-readable <path>  — true if path is readable.",
        call: |args, shell| fs::builtin_fs_pred(args, |p| p.metadata().map(|_| true).unwrap_or(false), shell).map(Some), },
    IsWritable { names: ["is-writable"], hint: Value,
        doc: "is-writable <path>  — true if path is writable.",
        call: |args, shell| fs::builtin_fs_pred(args, |p| p.metadata().map(|m| !m.permissions().readonly()).unwrap_or(false), shell).map(Some), },
    IsEmpty { names: ["is-empty"], hint: Value,
        doc: "is-empty <val>  — true if list, map, bytes, or string is empty.",
        call: |args, shell| predicates::builtin_is_empty(args, shell).map(Some), },
    Equal { names: ["equal"], hint: Value,
        doc: "equal <a> <b>  — true if a and b are equal.",
        call: |args, shell| predicates::builtin_equal(args, shell).map(Some), },
    Lt { names: ["lt"], hint: Value,
        doc: "lt <a> <b>  — true if a < b (lexicographic or numeric).",
        call: |args, shell| predicates::builtin_lt(args, shell).map(Some), },
    Gt { names: ["gt"], hint: Value,
        doc: "gt <a> <b>  — true if a > b (lexicographic or numeric).",
        call: |args, shell| predicates::builtin_gt(args, shell).map(Some), },
    Fs { names: ["_fs"], hint: Value,
        doc: "_fs <op> ...  — filesystem queries: lines size mtime empty list tempdir tempfile.",
        call: |args, shell| fs::builtin_fs(args, shell).map(Some), },
    Par { names: ["par"], hint: Value,
        doc: "par <fn> <list> <jobs>  — parallel map with a concurrency limit.",
        call: |args, shell| control::builtin_par(args, shell).map(Some), },
    Convert { names: ["_convert"], hint: Value,
        doc: "_convert <type> <val>  — convert value to int, float, or string.",
        call: |args, _shell| strings::builtin_convert(args).map(Some), },
    Spawn { names: ["spawn"], hint: Value,
        doc: "spawn <thunk>  — spawn a concurrent task, return a handle.",
        call: |args, shell| concurrency::builtin_spawn(args, shell).map(Some), },
    Watch { names: ["watch"], hint: Value,
        doc: "watch <label> <thunk>  — spawn a concurrent task whose output streams live to the caller's stdout, line-framed with the given label.",
        call: |args, shell| concurrency::builtin_watch(args, shell).map(Some), },
    Await { names: ["await"], hint: Value,
        doc: "await <handle>  — wait for a task to complete.",
        call: |args, shell| concurrency::builtin_await(args, shell).map(Some), },
    Race { names: ["race"], hint: Value,
        doc: "race <handles>  — wait for the first of several tasks to finish.",
        call: |args, shell| concurrency::builtin_race(args, shell).map(Some), },
    Cancel { names: ["cancel"], hint: Value,
        doc: "cancel <handle>  — cancel a running task.",
        call: |args, shell| concurrency::builtin_cancel(args, shell).map(Some), },
    Disown { names: ["disown"], hint: Value,
        doc: "disown <handle>  — detach a task, letting it run in the background.",
        call: |args, shell| concurrency::builtin_disown(args, shell).map(Some), },
    // Bundled uutils tools (cat, yes, head, wc, ...) are not builtins.
    // `resolve_command` substitutes their resolved exec for a re-exec of
    // ourselves with `--ral-uutils-helper`, so they ride through the same
    // boundary as `/usr/bin/cat` did before bundling — one spawn site, one
    // wait site, one signal/exit-code policy, one broken-pipe rule.  See
    // [`crate::builtins::uutils::is_uutils_tool`].
    GrepFiles { names: ["grep-files"], hint: Value,
        doc: "grep-files <pattern> <files>  — search files, return [{file, line, text}].",
        call: |args, shell| fs::builtin_grep_files_dispatch(args, shell).map(Some), },
    // At runtime _type is a passthrough; the type was already printed at check time.
    TypeOf { names: ["_type"], hint: Value,
        doc: "_type <val>  — print inferred type at compile time; passthrough at runtime.",
        call: |args, _shell| Ok(Some(args.first().cloned().unwrap_or(Value::Unit))), },
    Help { names: ["help"], hint: Bytes,
        doc: "help [--types] [name]  — print documentation; --types adds type signatures.",
        call: |args, shell| Ok(Some(misc::builtin_help(args, shell))), },
    Editor { names: ["_editor"], hint: Value,
        doc: "_editor <op> ...  — line editor interface: get set push tui history parse ghost highlight state.",
        call: |args, shell| editor::builtin_editor(args, shell).map(Some), },
    Plugin { names: ["_plugin"], hint: Value,
        doc: "_plugin <op> ...  — plugin lifecycle: load unload.",
        call: |args, shell| plugin::builtin_plugin(args, shell).map(Some), },
    AnsiOk { names: ["_ansi-ok"], hint: Value,
        doc: "_ansi-ok  — true if stdout supports ANSI colour (respects NO_COLOR / non-tty).",
        call: |_args, _shell| Ok(Some(Value::Bool(crate::ansi::use_ui_color()))), },
}

/// Check if a name is a builtin function.
pub fn is_builtin(name: &str) -> bool {
    builtin_doc(name).is_some()
}

/// Synthesise a first-class thunk for a registered builtin so `$name` can be
/// used as a callable value.  The thunk shape is `U(λx₀…λxₙ. Builtin(name, x⃗))`
/// where `n` is the builtin's typechecker-declared arity, so the resulting
/// value plays the same role as any user-written closure.  Returns `None`
/// for unknown names or builtins without a fixed arity (variadic ones like
/// `echo` are command-only).
pub fn synthesize_builtin_thunk(name: &str) -> Option<Value> {
    use crate::ast::Pattern;
    use crate::ir::{Comp, CompKind, Val};
    use std::sync::Arc;

    if !is_builtin(name) {
        return None;
    }
    let arity = crate::typecheck::builtin_arity(name)?;

    // Body: Builtin(name, [Variable("__b0"), …, Variable("__b{n-1}")]).
    let arg_vars: Vec<Val> = (0..arity)
        .map(|i| Val::Variable(format!("__b{i}")))
        .collect();
    let mut body = Comp::new(CompKind::Builtin {
        name: name.to_string(),
        args: arg_vars,
    });
    // Wrap in nested λ from innermost outward: λ__b{n-1}. … λ__b0. body.
    for i in (0..arity).rev() {
        body = Comp::new(CompKind::Lam {
            param: Pattern::Name(format!("__b{i}")),
            body: Box::new(body),
        });
    }
    Some(Value::Thunk {
        body: Arc::new(body),
        captured: Arc::new(Env::default()),
    })
}

/// Register prelude definitions into the environment.
pub fn register(shell: &mut Shell, prelude_comp: &crate::ir::Comp) {
    static PRELUDE_BINDINGS: OnceLock<HashMap<String, Value>> = OnceLock::new();

    // Evaluate the prelude once per process, then clone the resulting
    // top-level bindings into each fresh environment.
    let bindings = PRELUDE_BINDINGS.get_or_init(|| {
        let mut prelude_env = Shell::new(Default::default());

        // These are also recognized by parse_literal, but kept here for
        // backward compatibility with code that references $true etc.
        prelude_env.set("true".into(), Value::Bool(true));
        prelude_env.set("false".into(), Value::Bool(false));

        let saved_script = prelude_env.location.script.clone();
        prelude_env.location.script = "<prelude>".into();
        if let Err(e) = crate::evaluate(prelude_comp, &mut prelude_env) {
            diagnostic::cmd_error("prelude", &e.to_string());
        }
        prelude_env.location.script = saved_script;
        prelude_env.top_scope().clone()
    });

    for (name, value) in bindings {
        shell.set(name.clone(), value.clone());
    }

    // Push a user scope so that prelude bindings (scopes[0]) can be
    // distinguished from user bindings (scopes[1..]) in the lookup chain.
    shell.push_scope();
}

pub use misc::pretty_print;

/// Apply `val` as a callable (thunk/lambda).  Non-callable values produce
/// a descriptive error.  Used by builtins that accept function arguments.
pub(crate) fn call_value(val: &Value, args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    match val {
        Value::Thunk { .. } => crate::evaluator::call_value_pub(val, args, shell),
        _ => Err(EvalSignal::Error(
            Error::new(format!("cannot call {} '{}'", val.type_name(), val), 1)
                .with_hint("only Blocks and Lambdas can be called"),
        )),
    }
}
