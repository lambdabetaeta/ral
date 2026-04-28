//! Builtin command dispatch and registration.
//!
//! Builtins are commands implemented in Rust that run inside the shell
//! process.  Each builtin is registered in the `builtin_registry!` macro,
//! which generates the name-to-function dispatch table, per-command
//! documentation strings, and computation-type hints consumed by the
//! type checker.
//!
//! The prelude (a ral script baked into the binary) is evaluated once
//! per process; its top-level bindings are cloned into every fresh
//! environment via [`register`].

use crate::diagnostic;
use crate::types::*;
use std::collections::HashMap;
use std::sync::OnceLock;

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
mod uutils;

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

macro_rules! builtin_registry {
    (
        $(
            $(#[$meta:meta])*
            $variant:ident {
                names: [$($name:literal),+ $(,)?],
                hint: $hint:ident,
                doc: $doc:literal,
            }
        ),+ $(,)?
    ) => {
        /// Builtin dispatch key.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum BuiltinName {
            $($(#[$meta])* $variant,)+
        }

        fn builtin_by_name(name: &str) -> Option<BuiltinName> {
            match name {
                $($(#[$meta])* $($name)|+ => Some(BuiltinName::$variant),)+
                _ => None,
            }
        }

        pub fn builtin_comp_hint(name: &str) -> Option<BuiltinCompHint> {
            let kind = builtin_by_name(name)?;
            Some(match kind {
                $($(#[$meta])* BuiltinName::$variant => BuiltinCompHint::$hint,)+
            })
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
    };
}

builtin_registry! {
    Echo { names: ["echo"], hint: Bytes,
        doc: "echo <args...>  — write arguments to stdout.", },
    Warn { names: ["_warn"], hint: Bytes,
        doc: "_warn <args...>  — write arguments to stderr.", },
    Each { names: ["_each"], hint: Value,
        doc: "_each <list> <fn>  — call fn on each element for side effects.", },
    Map { names: ["map"], hint: Value,
        doc: "map <fn> <list>  — apply fn to each element, return new list.", },
    Filter { names: ["filter"], hint: Value,
        doc: "filter <fn> <list>  — keep elements where fn returns true.", },
    SortList { names: ["sort-list"], hint: Value,
        doc: "sort-list <list>  — sort a list lexicographically.", },
    SortListBy { names: ["sort-list-by"], hint: Value,
        doc: "sort-list-by <fn> <list>  — sort by a key function.", },
    Fold { names: ["_fold"], hint: Value,
        doc: "_fold <list> <init> <fn>  — reduce list left-to-right with fn and accumulator.", },
    Try { names: ["_try"], hint: Value,
        doc: "_try <thunk>  — run thunk; return {ok, value, status, cmd, stderr, line, col} record.", },
    TryWith { names: ["try"], hint: Value,
        doc: "try <body> <handler>  — run body; on failure call handler with error record.", },
    TryApply { names: ["_try-apply"], hint: Value,
        doc: "_try-apply <f> <val>  — apply f to val; on parameter-pattern mismatch return [ok:false,value:unit], else [ok:true,value:result].", },
    Guard { names: ["guard"], hint: Value,
        doc: "guard <body> <cleanup>  — run body, then cleanup regardless of outcome.", },
    Audit { names: ["audit"], hint: Value,
        doc: "audit <thunk>  — run thunk and record its execution tree.", },
    Fail { names: ["fail"], hint: Never,
        doc: "fail <status>  — exit with error status.", },
    Len { names: ["length"], hint: Value,
        doc: "length <val>  — number of elements in a string, bytes, list, or map.", },
    Upper { names: ["upper"], hint: Value,
        doc: "upper <s>  — convert a string to uppercase.", },
    Lower { names: ["lower"], hint: Value,
        doc: "lower <s>  — convert a string to lowercase.", },
    Dedent { names: ["dedent"], hint: Value,
        doc: "dedent <s>  — strip common leading whitespace from every non-empty line.", },
    Intercalate { names: ["intercalate"], hint: Value,
        doc: "intercalate <sep> <items>  — interpose sep between every pair of items, concatenated as one string.", },
    Slice { names: ["slice"], hint: Value,
        doc: "slice <s> <start> <count>  — extract a substring by character offset.", },
    Split { names: ["split"], hint: Value,
        doc: "split <pattern> <s>  — split a string by a regex pattern.", },
    Match { names: ["match"], hint: Value,
        doc: "match <pattern> <s>  — true if regex pattern matches anywhere in s.", },
    FindMatch { names: ["find-match"], hint: Value,
        doc: "find-match <pattern> <s>  — first regex match, or fail if none.", },
    FindMatches { names: ["find-matches"], hint: Value,
        doc: "find-matches <pattern> <s>  — all non-overlapping regex matches as a list.", },
    Replace { names: ["replace"], hint: Value,
        doc: "replace <pattern> <repl> <s>  — replace first regex match; $1 etc. backreferences.", },
    ReplaceAll { names: ["replace-all"], hint: Value,
        doc: "replace-all <pattern> <repl> <s>  — replace every regex match.", },
    ShellQuote { names: ["shell-quote"], hint: Value,
        doc: "shell-quote <s>  — quote a string for safe shell-word use.", },
    ShellSplit { names: ["shell-split"], hint: Value,
        doc: "shell-split <s>  — split a shell-quoted string into a list of words.", },
    Keys { names: ["keys"], hint: Value,
        doc: "keys <map>  — list of map keys.", },
    Has { names: ["has"], hint: Value,
        doc: "has <map> <key>  — true if map contains key.", },
    Path { names: ["_path"], hint: Value,
        doc: "_path <op> <path>  — path ops: stem ext dir base resolve join.", },
    Glob { names: ["glob"], hint: Value,
        doc: "glob <pattern>  — list paths matching a glob pattern.", },
    Within { names: ["within"], hint: LastThunk,
        doc: "within <opts> <thunk>  — run thunk with scoped shell, dir, and/or effect handlers.\n  Keys: shell (map), dir (path), handlers (map of name→thunk), handler (catch-all thunk).", },
    Grant { names: ["grant"], hint: LastThunk,
        doc: "grant <caps> <thunk>  — run thunk with restricted capabilities.", },
    Exit { names: ["exit", "quit"], hint: Value,
        doc: "exit [status]  — exit the shell.", },
    FoldLines { names: ["fold-lines"], hint: DecodeToValue,
        doc: "fold-lines <fn> <init>  — fold over stdin lines.", },
    FromBytes { names: ["from-bytes"], hint: DecodeToValue,
        doc: "from-bytes [input]  — read raw bytes from stdin (or arg) as Bytes.", },
    FromString { names: ["from-string"], hint: DecodeToValue,
        doc: "from-string [input]  — decode UTF-8 bytes from stdin (or arg) to a String.", },
    FromLine { names: ["from-line"], hint: DecodeToValue,
        doc: "from-line [input]  — decode UTF-8 bytes, stripping one trailing newline.", },
    FromLines { names: ["from-lines"], hint: DecodeToValue,
        doc: "from-lines [input]  — decode bytes to a list of lines (lossy on invalid UTF-8).", },
    FromJson { names: ["from-json"], hint: DecodeToValue,
        doc: "from-json [input]  — decode JSON bytes from stdin (or arg) to a value.", },
    ToBytes { names: ["to-bytes"], hint: EncodeToBytes,
        doc: "to-bytes <value>  — pass Bytes (or list of Ints) through to the byte channel.", },
    ToString { names: ["to-string"], hint: EncodeToBytes,
        doc: "to-string <value>  — encode a value's String form to the byte channel.", },
    ToLine { names: ["to-line"], hint: EncodeToBytes,
        doc: "to-line <value>  — encode value with a trailing newline (inverse of from-line).", },
    ToLines { names: ["to-lines"], hint: EncodeToBytes,
        doc: "to-lines <list>  — newline-join the list elements to the byte channel.", },
    ToJson { names: ["to-json"], hint: EncodeToBytes,
        doc: "to-json <value>  — encode a value as JSON bytes.", },
    Ask { names: ["ask"], hint: Value,
        doc: "ask <prompt>  — prompt for interactive input, return string.", },
    Source { names: ["source"], hint: Value,
        doc: "source <file>  — execute a .ral script file.", },
    Use { names: ["use"], hint: Value,
        doc: "use <file>  — load a .ral module (cached).", },
    Which { names: ["which"], hint: Value,
        doc: "which <name>  — find executable in PATH.", },
    Cwd { names: ["cwd"], hint: Value,
        doc: "cwd  — return the current working directory as a String.", },
    Chdir { names: ["cd"], hint: Value,
        doc: "cd [path]  — change the shell working directory; gated by shell.chdir capability. Empty/missing path means $HOME.", },
    Exists { names: ["exists"], hint: Value,
        doc: "exists <path>  — true if path exists.", },
    IsFile { names: ["is-file"], hint: Value,
        doc: "is-file <path>  — true if path is a regular file.", },
    IsDir { names: ["is-dir"], hint: Value,
        doc: "is-dir <path>  — true if path is a directory.", },
    IsLink { names: ["is-link"], hint: Value,
        doc: "is-link <path>  — true if path is a symbolic link.", },
    IsReadable { names: ["is-readable"], hint: Value,
        doc: "is-readable <path>  — true if path is readable.", },
    IsWritable { names: ["is-writable"], hint: Value,
        doc: "is-writable <path>  — true if path is writable.", },
    IsEmpty { names: ["is-empty"], hint: Value,
        doc: "is-empty <val>  — true if list, map, bytes, or string is empty.", },
    Equal { names: ["equal"], hint: Value,
        doc: "equal <a> <b>  — true if a and b are equal.", },
    Lt { names: ["lt"], hint: Value,
        doc: "lt <a> <b>  — true if a < b (lexicographic or numeric).", },
    Gt { names: ["gt"], hint: Value,
        doc: "gt <a> <b>  — true if a > b (lexicographic or numeric).", },
    Fs { names: ["_fs"], hint: Value,
        doc: "_fs <op> ...  — filesystem ops: read write copy rename remove mkdir list size lines mtime tempdir tempfile.", },
    WriteJson { names: ["write-json"], hint: EncodeToBytes,
        doc: "write-json <path> <data>  — write data as pretty-printed JSON to a file.", },
    Par { names: ["par"], hint: Value,
        doc: "par <fn> <list> <jobs>  — parallel map with a concurrency limit.", },
    Convert { names: ["_convert"], hint: Value,
        doc: "_convert <type> <val>  — convert value to int, float, or string.", },
    Diff { names: ["diff"], hint: Value,
        doc: "diff <a> <b>  — diff two maps, or run system diff on two files.", },
    Spawn { names: ["spawn"], hint: Value,
        doc: "spawn <thunk>  — spawn a concurrent task, return a handle.", },
    Watch { names: ["watch"], hint: Value,
        doc: "watch <label> <thunk>  — spawn a concurrent task whose output streams live to the caller's stdout, line-framed with the given label.", },
    Await { names: ["await"], hint: Value,
        doc: "await <handle>  — wait for a task to complete.", },
    Race { names: ["race"], hint: Value,
        doc: "race <handles>  — wait for the first of several tasks to finish.", },
    Cancel { names: ["cancel"], hint: Value,
        doc: "cancel <handle>  — cancel a running task.", },
    Disown { names: ["disown"], hint: Value,
        doc: "disown <handle>  — detach a task, letting it run in the background.", },
    #[cfg(feature = "coreutils")]
    Uutils {
        names: [
            "ls", "cat", "wc", "head", "tail", "cp", "cut", "mkdir", "mv", "rm", "seq",
            "sort", "tee", "touch", "tr", "uniq", "yes", "basename", "comm", "date", "df",
            "dirname", "du", "env", "join", "ln", "paste", "printf", "sleep", "arch", "b2sum",
            "base32", "base64", "basenc", "cksum", "csplit", "dd", "dir", "dircolors", "expand",
            "expr", "factor", "fmt", "fold", "hostname", "link", "md5sum", "mktemp", "nl", "nproc",
            "numfmt", "od", "pr", "printenv", "ptx", "pwd", "readlink", "realpath", "rmdir", "sha1sum",
            "sha224sum", "sha256sum", "sha384sum", "sha512sum", "shred", "shuf", "sum", "sync", "tac",
            "test", "truncate", "tsort", "uname", "unexpand", "unlink", "vdir", "whoami"
        ],
        hint: Bytes,
        doc: "Coreutils-compatible command (see man pages).",
    },
    #[cfg(feature = "diffutils")]
    UuCmp { names: ["cmp"], hint: Bytes,
        doc: "Compare two files byte by byte (coreutils cmp).", },
    GrepFiles { names: ["grep-files"], hint: Value,
        doc: "grep-files <pattern> <files>  — search files, return [{file, line, text}].", },
    TypeOf { names: ["_type"], hint: Value,
        doc: "_type <val>  — print inferred type at compile time; passthrough at runtime.", },
    Help { names: ["help"], hint: Bytes,
        doc: "help [--types] [name]  — print documentation; --types adds type signatures.", },
    Editor { names: ["_editor"], hint: Value,
        doc: "_editor <op> ...  — line editor interface: get set push tui history parse ghost highlight state.", },
    Plugin { names: ["_plugin"], hint: Value,
        doc: "_plugin <op> ...  — plugin lifecycle: load unload.", },
    AnsiOk { names: ["_ansi-ok"], hint: Value,
        doc: "_ansi-ok  — true if stdout supports ANSI colour (respects NO_COLOR / non-tty).", },
}

/// Check if a name is a builtin function.
pub fn is_builtin(name: &str) -> bool {
    builtin_by_name(name).is_some()
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

/// Call a builtin function. Returns None if not a builtin.
pub fn call(name: &str, args: &[Value], shell: &mut Shell) -> Result<Option<Value>, EvalSignal> {
    let Some(kind) = builtin_by_name(name) else {
        return Ok(None);
    };

    match kind {
        BuiltinName::Echo => Ok(Some(misc::builtin_echo(args, shell))),
        BuiltinName::Warn => Ok(Some(misc::builtin_warn(args))),
        BuiltinName::Each => Ok(Some(collections::builtin_each(args, shell)?)),
        BuiltinName::Map => Ok(Some(collections::builtin_map(args, shell)?)),
        BuiltinName::Filter => Ok(Some(collections::builtin_filter(args, shell)?)),
        BuiltinName::SortList => Ok(Some(collections::builtin_sort(args)?)),
        BuiltinName::SortListBy => Ok(Some(collections::builtin_sort_by(args, shell)?)),
        BuiltinName::Fold => Ok(Some(collections::builtin_fold(args, shell)?)),
        BuiltinName::Try => Ok(Some(control::builtin_try(args, shell)?)),
        BuiltinName::TryWith => Ok(Some(control::builtin_try_with(args, shell)?)),
        BuiltinName::TryApply => Ok(Some(control::builtin_try_apply(args, shell)?)),
        BuiltinName::Guard => Ok(Some(control::builtin_guard(args, shell)?)),
        BuiltinName::Audit => Ok(Some(control::builtin_audit(args, shell)?)),
        BuiltinName::Fail => Err(misc::builtin_fail(args)),
        BuiltinName::Len => Ok(Some(strings::builtin_len(args)?)),
        BuiltinName::Upper => Ok(Some(strings::builtin_upper(args)?)),
        BuiltinName::Lower => Ok(Some(strings::builtin_lower(args)?)),
        BuiltinName::Dedent => Ok(Some(strings::builtin_dedent(args)?)),
        BuiltinName::Intercalate => Ok(Some(strings::builtin_join(args)?)),
        BuiltinName::Slice => Ok(Some(strings::builtin_slice(args)?)),
        BuiltinName::Split => Ok(Some(strings::builtin_split(args)?)),
        BuiltinName::Match => Ok(Some(strings::builtin_match(args, shell)?)),
        BuiltinName::FindMatch => Ok(Some(strings::builtin_find_match(args)?)),
        BuiltinName::FindMatches => Ok(Some(strings::builtin_find_matches(args)?)),
        BuiltinName::Replace => Ok(Some(strings::builtin_replace(args)?)),
        BuiltinName::ReplaceAll => Ok(Some(strings::builtin_replace_all(args)?)),
        BuiltinName::ShellQuote => Ok(Some(strings::builtin_shell_quote(args)?)),
        BuiltinName::ShellSplit => Ok(Some(strings::builtin_shell_split(args)?)),
        BuiltinName::Keys => Ok(Some(predicates::builtin_keys(args)?)),
        BuiltinName::Has => Ok(Some(predicates::builtin_has(args, shell)?)),
        BuiltinName::Path => Ok(Some(path::builtin_path(args, shell)?)),
        BuiltinName::Glob => Ok(Some(fs::builtin_glob(args, shell)?)),
        BuiltinName::Within => Ok(Some(scope::builtin_within(args, shell)?)),
        BuiltinName::Grant => Ok(Some(scope::builtin_grant(args, shell)?)),
        BuiltinName::Exit => misc::builtin_exit(args, shell),
        BuiltinName::FoldLines => Ok(Some(codecs::builtin_fold_lines(args, shell)?)),
        BuiltinName::FromBytes => Ok(Some(codecs::builtin_from_bytes(args, shell)?)),
        BuiltinName::FromString => Ok(Some(codecs::builtin_from_string(args, shell)?)),
        BuiltinName::FromLine => Ok(Some(codecs::builtin_from_line(args, shell)?)),
        BuiltinName::FromLines => Ok(Some(codecs::builtin_from_lines(args, shell)?)),
        BuiltinName::FromJson => Ok(Some(codecs::builtin_from_json(args, shell)?)),
        BuiltinName::ToBytes => Ok(Some(codecs::builtin_to_bytes(args, shell)?)),
        BuiltinName::ToString => Ok(Some(codecs::builtin_to_string(args, shell)?)),
        BuiltinName::ToLine => Ok(Some(codecs::builtin_to_line(args, shell)?)),
        BuiltinName::ToLines => Ok(Some(codecs::builtin_to_lines(args, shell)?)),
        BuiltinName::ToJson => Ok(Some(codecs::builtin_to_json(args, shell)?)),
        BuiltinName::Ask => Ok(Some(misc::builtin_ask(args)?)),
        BuiltinName::Source => Ok(Some(modules::builtin_source(args, shell)?)),
        BuiltinName::Use => Ok(Some(modules::builtin_use(args, shell)?)),
        BuiltinName::Which => Ok(Some(path::builtin_which(args, shell)?)),
        BuiltinName::Cwd => {
            let dir = shell.resolve_path(".");
            Ok(Some(Value::String(dir.to_string_lossy().into_owned())))
        }
        BuiltinName::Chdir => Ok(Some(shell::builtin_chdir(args, shell)?)),
        BuiltinName::Exists => Ok(Some(fs::builtin_fs_pred(args, |p| p.exists(), shell)?)),
        BuiltinName::IsFile => Ok(Some(fs::builtin_fs_pred(args, |p| p.is_file(), shell)?)),
        BuiltinName::IsDir => Ok(Some(fs::builtin_fs_pred(args, |p| p.is_dir(), shell)?)),
        BuiltinName::IsLink => Ok(Some(fs::builtin_fs_pred(args, |p| p.is_symlink(), shell)?)),
        BuiltinName::IsReadable => Ok(Some(fs::builtin_fs_pred(
            args,
            |p| p.metadata().map(|_| true).unwrap_or(false),
            shell,
        )?)),
        BuiltinName::IsWritable => Ok(Some(fs::builtin_fs_pred(
            args,
            |p| {
                p.metadata()
                    .map(|m| !m.permissions().readonly())
                    .unwrap_or(false)
            },
            shell,
        )?)),
        BuiltinName::IsEmpty => Ok(Some(predicates::builtin_is_empty(args, shell)?)),
        BuiltinName::Equal => Ok(Some(predicates::builtin_equal(args, shell)?)),
        BuiltinName::Lt => Ok(Some(predicates::builtin_lt(args, shell)?)),
        BuiltinName::Gt => Ok(Some(predicates::builtin_gt(args, shell)?)),
        BuiltinName::Fs => Ok(Some(fs::builtin_fs(args, shell)?)),
        BuiltinName::WriteJson => Ok(Some(fs::builtin_write_json(args, shell)?)),
        BuiltinName::Par => Ok(Some(control::builtin_par(args, shell)?)),
        BuiltinName::Convert => Ok(Some(strings::builtin_convert(args)?)),
        BuiltinName::Diff => {
            #[cfg(feature = "diffutils")]
            if args.iter().any(|v| !matches!(v, Value::Map(_))) {
                return Ok(Some(uutils::uu_diff(args, shell)?));
            }
            if args.len() < 2 {
                return Err(util::sig("diff requires 2 arguments"));
            }
            let (a, b) = (
                util::as_map(&args[0], "diff")?,
                util::as_map(&args[1], "diff")?,
            );
            Ok(Some(Value::Map(
                a.into_iter()
                    .filter(|(k, _)| !b.iter().any(|(bk, _)| bk == k))
                    .collect(),
            )))
        }
        BuiltinName::Spawn => Ok(Some(concurrency::builtin_spawn(args, shell)?)),
        BuiltinName::Watch => Ok(Some(concurrency::builtin_watch(args, shell)?)),
        BuiltinName::Await => Ok(Some(concurrency::builtin_await(args, shell)?)),
        BuiltinName::Race => Ok(Some(concurrency::builtin_race(args, shell)?)),
        BuiltinName::Cancel => Ok(Some(concurrency::builtin_cancel(args, shell)?)),
        BuiltinName::Disown => Ok(Some(concurrency::builtin_disown(args, shell)?)),
        #[cfg(feature = "coreutils")]
        BuiltinName::Uutils => Ok(Some(uutils::uutils(name, args, shell)?)),
        #[cfg(feature = "diffutils")]
        BuiltinName::UuCmp => Ok(Some(uutils::uu_cmp(args, shell)?)),
        BuiltinName::GrepFiles => {
            #[cfg(feature = "grep")]
            return Ok(Some(fs::builtin_grep_files(args, shell)?));
            #[cfg(not(feature = "grep"))]
            return Err(util::sig(
                "grep-files: grep feature not compiled in — rebuild with --features grep",
            ));
        }
        // At runtime _type is a passthrough; the type was already printed at check time.
        BuiltinName::TypeOf => Ok(Some(args.first().cloned().unwrap_or(Value::Unit))),
        BuiltinName::Help => Ok(Some(misc::builtin_help(args, shell))),
        BuiltinName::Editor => Ok(Some(editor::builtin_editor(args, shell)?)),
        BuiltinName::Plugin => Ok(Some(plugin::builtin_plugin(args, shell)?)),
        BuiltinName::AnsiOk => Ok(Some(Value::Bool(crate::ansi::use_ui_color()))),
    }
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
