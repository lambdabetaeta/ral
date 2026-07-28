//! Builtins: the commands implemented in Rust that run inside the shell process.
//!
//! `builtin_registry!` binds each builtin's names, arity, type rule, doc and
//! runtime body in one entry — so those facets cannot drift apart — and expands
//! them into the [`CORE_BUILTINS`] static every shell's builtin table is seeded
//! from.

use crate::diagnostic;
use crate::types::{
    Binding, Break, BuiltinBody, BuiltinEntry, Env, Error, Escape, Mooring, Settled, Shell, Value,
};
// Variants unprefixed, so an entry reads `ty: Sig(sig::RANGE)`.
use crate::typecheck::builtins::BuiltinTypeRule;
use crate::typecheck::builtins::BuiltinTypeRule::{Scheme, Sig};
use crate::typecheck::builtins::{scheme, sig};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

mod codecs;
mod collections;
pub(crate) mod concurrency;
mod fs;
pub mod help;
mod math;
mod misc;
pub mod modules;
mod predicates;
mod print;
mod shell;
pub mod strings;
pub use codecs::value_to_json;
pub use util::value_to_json_lossy_bytes;
pub mod util;
pub mod uutils;

macro_rules! builtin_arity_from {
    (_) => {
        None
    };
    ($n:literal) => {
        Some($n)
    };
}

/// `Option` equality, spelled out because `PartialEq` is not const.
const fn arity_agrees(declared: Option<usize>, structural: Option<usize>) -> bool {
    match (declared, structural) {
        (Some(a), Some(b)) => a == b,
        (None, None) => true,
        _ => false,
    }
}

/// One entry per builtin, expanded into [`CORE_BUILTINS`].
///
/// `arity:` is authoritative for a `Scheme` rule and is injected into the
/// emitted [`BuiltinTypeRule`]; a `Sig` already implies a structural arity, so
/// there the two are compared during the static's const evaluation and a
/// mismatch is a build error.  `arity: _` means no fixed argv, and `$name`
/// resolves only when the type rule carries an explicit value form.
///
/// `call` must be a non-capturing closure — it is coerced to a fn pointer.
macro_rules! builtin_registry {
    (
        $(
            $(#[$meta:meta])*
            $variant:ident {
                names: [$($name:literal),+ $(,)?],
                arity: $arity:tt,
                ty: $ty:expr,
                doc: $doc:literal,
                call: $call:expr,
            }
        ),+ $(,)?
    ) => {
        // A module, so a variant name like `Map` cannot collide with a type in scope.
        mod __core_thunks {
            #[allow(unused_imports)]
            use super::*;
            $(
                $(#[$meta])*
                #[allow(non_snake_case)]
                pub(super) fn $variant(
                    args: &[Value],
                    mooring: &Mooring,
                    shell: &mut Shell,
                ) -> Settled<Value> {
                    let inner: fn(&[Value], &Mooring, &mut Shell) -> Settled<Value> = $call;
                    inner(args, mooring, shell)
                }
            )+
        }

        /// Every host-implemented name the language ships with.
        ///
        /// Installed into each fresh [`Shell`] by [`Shell::new`]; the checker
        /// seeds from that same table (`Shell::session_schemes`), so there is no
        /// second lookup path to drift from it.
        pub static CORE_BUILTINS: &[BuiltinEntry] = &[
            $(
                $(#[$meta])*
                $(
                    BuiltinEntry {
                        name: Cow::Borrowed($name),
                        type_rule: match $ty {
                            BuiltinTypeRule::Scheme(_, factory) => BuiltinTypeRule::Scheme(builtin_arity_from!($arity), factory),
                            BuiltinTypeRule::Sig(sig) => {
                                assert!(
                                    arity_agrees(builtin_arity_from!($arity), sig.fixed_arity()),
                                    "builtin `arity:` disagrees with its Sig's structural arity — fix the arity: field or the signature",
                                );
                                BuiltinTypeRule::Sig(sig)
                            }
                        },
                        doc: $doc,
                        body: BuiltinBody::Static(__core_thunks::$variant),
                    },
                )+
            )+
        ];
    };
}

builtin_registry! {
    Clear { names: ["clear"], arity: 0, ty: Sig(sig::TERMINAL_CONTROL),
        doc: "clear  — clear screen and scrollback (ESC[H ESC[2J ESC[3J). Shadows external `clear`; use `^clear` for the ncurses binary.",
        call: |args, _mooring, shell| Ok(misc::builtin_clear(args, shell)), },
    Reset { names: ["reset"], arity: 0, ty: Sig(sig::TERMINAL_CONTROL),
        doc: "reset  — emit ESC c (RIS) to reset the terminal. Does not touch stty modes; use `^reset` for the full ncurses terminfo reset.",
        call: |args, _mooring, shell| Ok(misc::builtin_reset(args, shell)), },
    Each { names: ["each"], arity: 2, ty: Scheme(None, scheme::each_op),
        doc: "each <fn> <list>  — call fn on each element for side effects.",
        call: |args, mooring, shell| collections::builtin_each(args, mooring, shell), },
    Map { names: ["map"], arity: 2, ty: Scheme(None, scheme::map_op),
        doc: "map <fn> <list>  — apply fn to each element, return new list.",
        call: |args, mooring, shell| collections::builtin_map(args, mooring, shell), },
    Filter { names: ["filter"], arity: 2, ty: Scheme(None, scheme::filter_op),
        doc: "filter <fn> <list>  — keep elements where fn returns true.",
        call: |args, mooring, shell| collections::builtin_filter(args, mooring, shell), },
    SortList { names: ["sort-list"], arity: 1, ty: Scheme(None, scheme::sort_list),
        doc: "sort-list <list>  — sort a list lexicographically.",
        call: |args, _mooring, _shell| collections::builtin_sort(args), },
    SortListBy { names: ["sort-list-by"], arity: 2, ty: Scheme(None, scheme::sort_list_by),
        doc: "sort-list-by <fn> <list>  — sort by a key function.",
        call: |args, mooring, shell| collections::builtin_sort_by(args, mooring, shell), },
    Fold { names: ["fold"], arity: 3, ty: Scheme(None, scheme::fold_op),
        doc: "fold <fn> <init> <list>  — reduce list left-to-right with fn and accumulator.",
        call: |args, mooring, shell| collections::builtin_fold(args, mooring, shell), },
    Range { names: ["range"], arity: 2, ty: Sig(sig::RANGE),
        doc: "range <start> <end>  — generate a list of integers from start (inclusive) to end (exclusive).",
        call: |args, mooring, _| collections::builtin_range(args, mooring), },
    Fail { names: ["fail"], arity: 1, ty: Sig(sig::FAIL),
        doc: "fail <status>  — exit with error status.",
        call: |args, _mooring, _shell| Err(misc::builtin_fail(args)), },
    Len { names: ["length"], arity: 1, ty: Scheme(None, scheme::length),
        doc: "length <val>  — number of elements in a string, bytes, list, or map.",
        call: |args, _mooring, _shell| strings::builtin_len(args), },
    Upper { names: ["upper"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "upper <s>  — convert a string to uppercase.",
        call: |args, _mooring, _shell| strings::builtin_upper(args), },
    Lower { names: ["lower"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "lower <s>  — convert a string to lowercase.",
        call: |args, _mooring, _shell| strings::builtin_lower(args), },
    Dedent { names: ["dedent"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "dedent <s>  — strip common leading whitespace from every non-empty line.",
        call: |args, _mooring, _shell| strings::builtin_dedent(args), },
    Intercalate { names: ["intercalate"], arity: 2, ty: Scheme(None, scheme::intercalate),
        doc: "intercalate <sep> <items>  — interpose sep between every pair of items, concatenated as one string.",
        call: |args, _mooring, _shell| strings::builtin_join(args), },
    Slice { names: ["slice"], arity: 3, ty: Scheme(None, scheme::slice),
        doc: "slice <s> <start> <count>  — extract a substring by character offset.",
        call: |args, _mooring, _shell| strings::builtin_slice(args), },
    Split { names: ["re-split"], arity: 2, ty: Scheme(None, scheme::re_split),
        doc: "re-split <pattern> <s>  — split a string by a regex pattern.",
        call: |args, _mooring, _shell| strings::builtin_split(args), },
    Match { names: ["re-match"], arity: 2, ty: Scheme(None, scheme::re_match),
        doc: "re-match <pattern> <s>  — true if regex pattern matches anywhere in s.",
        call: |args, _mooring, shell| strings::builtin_match(args, shell), },
    FindMatch { names: ["re-find-match"], arity: 2, ty: Scheme(None, scheme::re_find_match),
        doc: "re-find-match <pattern> <s>  — first regex match, or fail if none.",
        call: |args, _mooring, _shell| strings::builtin_find_match(args), },
    FindMatches { names: ["re-find-matches"], arity: 2, ty: Scheme(None, scheme::re_split),
        doc: "re-find-matches <pattern> <s>  — all non-overlapping regex matches as a list.",
        call: |args, _mooring, _shell| strings::builtin_find_matches(args), },
    Replace { names: ["re-replace"], arity: 3, ty: Scheme(None, scheme::replace_3),
        doc: "re-replace <pattern> <repl> <s>  — replace first regex match; $1 etc. backreferences.",
        call: |args, _mooring, _shell| strings::builtin_replace(args), },
    ReplaceAll { names: ["re-replace-all"], arity: 3, ty: Scheme(None, scheme::replace_3),
        doc: "re-replace-all <pattern> <repl> <s>  — replace every regex match.",
        call: |args, _mooring, _shell| strings::builtin_replace_all(args), },
    StrReplace { names: ["string-replace"], arity: 3, ty: Scheme(None, scheme::replace_3),
        doc: "string-replace <from> <to> <s>  — replace the unique literal occurrence of <from> in <s> with <to>; error on 0 or >1 matches. No regex.",
        call: |args, _mooring, _shell| strings::builtin_string_replace(args), },
    ShellQuote { names: ["shell-quote"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "shell-quote <s>  — quote a string for safe shell-word use.",
        call: |args, _mooring, _shell| strings::builtin_shell_quote(args), },
    ShellSplit { names: ["shell-split"], arity: 1, ty: Scheme(None, scheme::shell_split),
        doc: "shell-split <s>  — split a shell-quoted string into a list of words.",
        call: |args, _mooring, _shell| strings::builtin_shell_split(args), },
    Keys { names: ["keys"], arity: 1, ty: Scheme(None, scheme::keys),
        doc: "keys <map>  — list of map keys.",
        call: |args, _mooring, _shell| predicates::builtin_keys(args), },
    Has { names: ["has"], arity: 2, ty: Scheme(None, scheme::has),
        doc: "has <map> <key>  — true if map contains key.",
        call: |args, _mooring, shell| predicates::builtin_has(args, shell), },
    ResolvePath { names: ["resolve-path"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "resolve-path <path>  — resolve to an absolute path.",
        call: |args, _mooring, shell| fs::builtin_resolve_path(args, shell), },
    AbsolutePath { names: ["absolute-path"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "absolute-path <path>  — absolute form of a path, computed lexically: anchors to the current working directory (honouring `within [dir: …]`), expands `~`/`xdg:`, folds `.`/`..`. Never touches the filesystem: symlinks stay as written and the path need not exist. Lexical counterpart of resolve-path.",
        call: |args, _mooring, shell| fs::builtin_absolute_path(args, shell), },
    Glob { names: ["glob"], arity: 1, ty: Scheme(None, scheme::glob),
        doc: "glob <pattern>  — list paths matching a Unix shell-style glob (`?`, `*`, `**`, `[…]`, `[!…]`; `**` spans directories). Patterns relative to the current working directory return matches relative to the current working directory; sigil-rooted (`~`, `xdg:`) and absolute patterns return absolute matches. Dotfiles are excluded from wildcard matches — match them by fully-literal name, or fall back to `list-dir | filter`.",
        call: |args, _mooring, shell| fs::builtin_glob(args, shell), },
    Exit { names: ["exit", "quit"], arity: 1, ty: Scheme(None, scheme::exit_op),
        doc: "exit [status]  — exit the shell.",
        call: |args, _mooring, shell| misc::builtin_exit(args, shell), },
    Surface { names: ["surface"], arity: 1, ty: Scheme(None, scheme::surface_op),
        doc: "surface <event>  — forward a tagged variant event to the host's structured-event sink; the identity when no host is installed.",
        call: |args, mooring, shell| misc::builtin_surface(args, mooring, shell), },
    FoldLines { names: ["fold-lines"], arity: 2, ty: Scheme(None, scheme::fold_lines),
        doc: "fold-lines <fn> <init>  — fold over stdin lines.",
        call: |args, mooring, shell| collections::builtin_fold_lines(args, mooring, shell), },
    FromBytes { names: ["from-bytes"], arity: 0, ty: Sig(sig::FROM_BYTES),
        doc: "from-bytes  — read raw bytes from the channel (stdin / `< file` / pipe) as Bytes.",
        call: |args, _mooring, shell| codecs::builtin_from_bytes(args, shell), },
    FromString { names: ["from-string"], arity: 0, ty: Sig(sig::FROM_STRING),
        doc: "from-string  — decode UTF-8 bytes from the channel to a String.",
        call: |args, _mooring, shell| codecs::builtin_from_string(args, shell), },
    FromLine { names: ["from-line"], arity: 0, ty: Sig(sig::FROM_STRING),
        doc: "from-line  — decode UTF-8 bytes from the channel, stripping one trailing newline.",
        call: |args, _mooring, shell| codecs::builtin_from_line(args, shell), },
    FromLines { names: ["from-lines"], arity: 0, ty: Sig(sig::FROM_LINES),
        doc: "from-lines  — decode channel bytes to a Step stream of lines (lossy on invalid UTF-8).",
        call: |args, _mooring, shell| codecs::builtin_from_lines(args, shell), },
    FromJson { names: ["from-json"], arity: 0, ty: Sig(sig::FROM_JSON),
        doc: "from-json  — decode JSON bytes from the channel to a value.",
        call: |args, _mooring, shell| codecs::builtin_from_json(args, shell), },
    FromCsv { names: ["from-csv"], arity: 0, ty: Sig(sig::FROM_JSON),
        doc: "from-csv  — decode CSV bytes from the channel to a list of records keyed by the header row (every field a String).",
        call: |args, _mooring, shell| codecs::builtin_from_csv(args, shell), },
    ToBytes { names: ["to-bytes"], arity: 1, ty: Sig(sig::TO_BYTES),
        doc: "to-bytes <value>  — pass Bytes (or list of Ints) through to the byte channel.",
        call: |args, _mooring, shell| codecs::builtin_to_bytes(args, shell), },
    ToString { names: ["to-string"], arity: 1, ty: Sig(sig::TO_ANY_BYTES),
        doc: "to-string <value>  — encode a value's String form to the byte channel.",
        call: |args, _mooring, shell| codecs::builtin_to_string(args, shell), },
    ToLine { names: ["to-line"], arity: 1, ty: Sig(sig::TO_LINE),
        doc: "to-line <value>  — encode value with a trailing newline (inverse of from-line).",
        call: |args, _mooring, shell| codecs::builtin_to_line(args, shell), },
    ToLines { names: ["to-lines"], arity: 1, ty: Sig(sig::TO_LINES),
        doc: "to-lines <list>  — newline-join the list elements to the byte channel.",
        call: |args, _mooring, shell| codecs::builtin_to_lines(args, shell), },
    ToJson { names: ["to-json"], arity: 1, ty: Sig(sig::TO_ANY_BYTES),
        doc: "to-json <value>  — encode a value as JSON bytes.",
        call: |args, _mooring, shell| codecs::builtin_to_json(args, shell), },
    ToCsv { names: ["to-csv"], arity: 1, ty: Sig(sig::TO_ANY_BYTES),
        doc: "to-csv <records>  — encode a list of records as CSV bytes; columns are the first record's keys in sorted order.",
        call: |args, _mooring, shell| codecs::builtin_to_csv(args, shell), },
    Ask { names: ["ask"], arity: 1, ty: Scheme(None, scheme::ask),
        doc: "ask <prompt>  — prompt for interactive input, return string.",
        call: |args, _mooring, _shell| misc::builtin_ask(args).map_err(Break::from), },
    Source { names: ["source"], arity: 1, ty: Scheme(None, scheme::source_op),
        doc: "source <file>  — execute a .ral script file.",
        call: |args, mooring, shell| modules::builtin_source(args, mooring, shell), },
    Use { names: ["use"], arity: 1, ty: Scheme(None, scheme::use_op),
        doc: "use <file>  — load a .ral module, returning its bindings as a map.",
        call: |args, mooring, shell| modules::builtin_use(args, mooring, shell), },
    Cwd { names: ["cwd"], arity: 0, ty: Scheme(None, scheme::pure_string),
        doc: "cwd  — return the current working directory as a String.",
        call: |_args, _mooring, shell| Ok(Value::String(shell.cwd().to_string_lossy().into_owned())), },
    Chdir { names: ["cd"], arity: _, ty: Sig(sig::CHDIR),
        doc: "cd [path]  — change the shell working directory; gated by shell.chdir capability. Empty/missing path means $HOME.",
        call: |args, _mooring, shell| shell::builtin_chdir(args, shell), },
    Alias { names: ["alias"], arity: 2, ty: Sig(sig::ALIAS),
        doc: "alias NAME { |args| BODY }  — install BODY as a handler-frame alias for NAME; replaces any prior alias for the same name. Persists past `within` blocks; remove with `unalias`.",
        call: |args, _mooring, shell| shell::builtin_alias(args, shell), },
    Unalias { names: ["unalias"], arity: 1, ty: Sig(sig::UNALIAS),
        doc: "unalias NAME  — remove the alias for NAME. Errors if none is installed.",
        call: |args, _mooring, shell| shell::builtin_unalias(args, shell), },
    IsEmpty { names: ["is-empty"], arity: 1, ty: Scheme(None, scheme::is_empty),
        doc: "is-empty <val>  — true if list, map, bytes, or string is empty.",
        call: |args, _mooring, shell| predicates::builtin_is_empty(args, shell), },
    Exists { names: ["exists"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "exists <path>  — true if path exists (any type); resolves against the within-scoped cwd.",
        call: |args, _mooring, shell| fs::builtin_exists(args, shell), },
    IsFile { names: ["is-file"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-file <path>  — true if path is a regular file (follows symlinks).",
        call: |args, _mooring, shell| fs::builtin_is_file(args, shell), },
    IsDir { names: ["is-dir"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-dir <path>  — true if path is a directory (follows symlinks).",
        call: |args, _mooring, shell| fs::builtin_is_dir(args, shell), },
    IsLink { names: ["is-link"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-link <path>  — true if path is a symbolic link.",
        call: |args, _mooring, shell| fs::builtin_is_link(args, shell), },
    IsReadable { names: ["is-readable"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-readable <path>  — true if path is readable by the caller.",
        call: |args, _mooring, shell| fs::builtin_is_readable(args, shell), },
    IsWritable { names: ["is-writable"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-writable <path>  — true if path is writable by the caller.",
        call: |args, _mooring, shell| fs::builtin_is_writable(args, shell), },
    Equal { names: ["equal"], arity: 2, ty: Scheme(None, scheme::compare),
        doc: "equal <a> <b>  — true if a and b are equal.",
        call: |args, _mooring, shell| predicates::builtin_equal(args, shell), },
    Lt { names: ["lt"], arity: 2, ty: Scheme(None, scheme::compare),
        doc: "lt <a> <b>  — true if a < b (numeric on Int/Float, lexicographic on String).",
        call: |args, _mooring, shell| predicates::builtin_lt(args, shell), },
    Gt { names: ["gt"], arity: 2, ty: Scheme(None, scheme::compare),
        doc: "gt <a> <b>  — true if a > b (numeric on Int/Float, lexicographic on String).",
        call: |args, _mooring, shell| predicates::builtin_gt(args, shell), },
    ListDir { names: ["list-dir"], arity: 1, ty: Scheme(None, scheme::list_dir),
        doc: "list-dir <path>  — list directory contents as [{name, type, size, mtime}].",
        call: |args, _mooring, shell| fs::builtin_list_dir(args, shell), },
    FileInfo { names: ["file-info"], arity: 1, ty: Scheme(None, scheme::file_info),
        doc: "file-info <path>  — {name, type, size, mtime, atime, btime, readonly, target}; uses lstat (no symlink follow); 0 when a timestamp is unavailable.",
        call: |args, _mooring, shell| fs::builtin_file_info(args, shell), },
    TempDir { names: ["temp-dir"], arity: 0, ty: Scheme(None, scheme::temp_path),
        doc: "temp-dir  — create a temporary directory.",
        call: |args, _mooring, shell| fs::builtin_temp_dir(args, shell), },
    TempFile { names: ["temp-file"], arity: 0, ty: Scheme(None, scheme::temp_path),
        doc: "temp-file  — create a temporary file.",
        call: |args, _mooring, shell| fs::builtin_temp_file(args, shell), },
    ToInt { names: ["int"], arity: 1, ty: Sig(sig::INT_PARSE),
        doc: "int <val>  — parse a value as an integer.",
        call: |args, _mooring, _shell| strings::builtin_to_int(args), },
    ToFloat { names: ["float"], arity: 1, ty: Sig(sig::FLOAT_PARSE),
        doc: "float <val>  — parse a value as a float.",
        call: |args, _mooring, _shell| strings::builtin_to_float(args), },
    Round { names: ["round"], arity: 2, ty: Sig(sig::ROUND),
        doc: "round <x> <places>  — round a Float to <places> decimal places, halves away from zero; always returns a Float (round 3.7 0 is 4.0). Use int for a whole number.",
        call: |args, _mooring, _shell| math::builtin_round(args), },
    Floor { names: ["floor"], arity: 1, ty: Sig(sig::FLOAT_TO_INT),
        doc: "floor <x>  — the greatest Int not exceeding a Float.",
        call: |args, _mooring, _shell| math::builtin_floor(args), },
    Ceil { names: ["ceil"], arity: 1, ty: Sig(sig::FLOAT_TO_INT),
        doc: "ceil <x>  — the least Int not below a Float.",
        call: |args, _mooring, _shell| math::builtin_ceil(args), },
    Trunc { names: ["trunc"], arity: 1, ty: Sig(sig::FLOAT_TO_INT),
        doc: "trunc <x>  — drop a Float's fractional part toward zero, yielding an Int.",
        call: |args, _mooring, _shell| math::builtin_trunc(args), },
    Str { names: ["str"], arity: 1, ty: Sig(sig::STR_PARSE),
        doc: "str <val>  — convert a value to its string representation.",
        call: |args, _mooring, _shell| strings::builtin_to_string(args), },
    Spawn { names: ["spawn"], arity: 1, ty: Scheme(None, scheme::spawn),
        doc: "spawn <thunk>  — spawn a concurrent block on a worker thread, return a handle.",
        call: |args, mooring, shell| concurrency::builtin_spawn(args, mooring, shell), },
    Await { names: ["await"], arity: 1, ty: Scheme(None, scheme::await_op),
        doc: "await <handle>  — wait for a concurrent block to complete, returning {value, stdout, stderr}; re-raises a failed block.",
        call: |args, mooring, shell| concurrency::builtin_await(args, mooring, shell), },
    Poll { names: ["poll"], arity: 1, ty: Scheme(None, scheme::poll),
        doc: "poll <handle>  — non-blocking sample of a concurrent block: `settled with {stdout, stderr, outcome: `ok/`err} once it finishes, or `pending while still running. Never re-raises.",
        call: |args, _mooring, shell| concurrency::builtin_poll(args, shell), },
    Race { names: ["race"], arity: 1, ty: Scheme(None, scheme::race),
        doc: "race <handles>  — wait for the first of several concurrent blocks to finish, returning {value, stdout, stderr}; re-raises a failed winner.",
        call: |args, mooring, shell| concurrency::builtin_race(args, mooring, shell), },
    Cancel { names: ["cancel"], arity: 1, ty: Scheme(None, scheme::cancel_op),
        doc: "cancel <handle>  — cancel a running concurrent block.",
        call: |args, _mooring, shell| concurrency::builtin_cancel(args, shell), },
    // The bundled uutils tools (cat, yes, head, wc, …) are deliberately absent:
    // `runtime::command` runs them either as an in-process `uumain` call or as a
    // `ral --ral-bundled-tool <tool>` child, so they cross the same wait / signal /
    // exit-code boundary as a system binary.  See `builtins::uutils::is_uutils_tool`.

    // `_type` types as an ordinary `α → F α`; the printing is a side effect of its
    // `BuiltinDiagnostic::TypeProbe`, and the runtime body is the identity.
    TypeOf { names: ["_type"], arity: 1, ty: Sig(sig::TYPE_PROBE),
        doc: "_type <val>  — print inferred type at compile time; passthrough at runtime.",
        call: |args, _mooring, _shell| Ok(args.first().cloned().unwrap_or(Value::Unit)), },
    Help { names: ["help"], arity: 0, ty: Sig(sig::HELP),
        doc: "help  — print an overview of builtins, prelude, and library; see also `explain`.",
        call: |args, _mooring, shell| Ok(help::builtin_help(args, shell)), },
    Explain { names: ["explain"], arity: 1, ty: Sig(sig::EXPLAIN),
        doc: "explain <name>  — print documentation for one name: doc, type signature, and source location.",
        call: |args, _mooring, shell| Ok(help::builtin_explain(args, shell)), },
    // The `_ed-*` family rides the REPL's boot surface instead; see
    // `ral::repl::plugin_ed_builtins::ED_BUILTINS`.
    AnsiOk { names: ["_ansi-ok"], arity: 0, ty: Scheme(None, scheme::pure_bool),
        doc: "_ansi-ok  — true if stdout supports ANSI colour (respects NO_COLOR / non-tty).",
        call: |_args, _mooring, _shell| Ok(Value::Bool(crate::ansi::use_ui_color())), },
}

/// A [`BuiltinTable`](crate::types::BuiltinTable) of [`CORE_BUILTINS`] alone —
/// what a checker with no live shell
/// ([`SessionSchemes`](crate::typecheck::SessionSchemes)'s `Default`) types
/// against, absent any host dressing.
pub fn core_builtin_table() -> crate::types::BuiltinTable {
    let mut table = crate::types::BuiltinTable::default();
    table.install_static(CORE_BUILTINS);
    table
}

/// `watch` — a spawn whose output streams live to the caller's stdout,
/// line-framed, kept out of [`CORE_BUILTINS`] for a host's boot surface to carry.
///
/// That sink must outlive the run, so only the interactive and batch ral hosts
/// install it; exarch's active streams are per-call capture buffers, and naming
/// `watch` there is an unknown-name diagnostic at compile time rather than a
/// builtin that resolves and then refuses.  [`SERVICE_BUILTIN`] is the same
/// shape with the hosts swapped.
pub static WATCH_BUILTIN: &[BuiltinEntry] = &[BuiltinEntry {
    name: Cow::Borrowed("watch"),
    type_rule: BuiltinTypeRule::Scheme(Some(2), scheme::watch),
    doc: "watch <label> <thunk>  — spawn a concurrent block whose output streams live to the caller's stdout, line-framed with the given label.",
    body: BuiltinBody::Static(concurrency::builtin_watch),
}];

/// `service` — an ordinary buffered spawn registered under the durable lease
/// class: no idle reap, no absolute backstop, bounded instead by the description
/// its birth requires.
///
/// Availability inverts [`WATCH_BUILTIN`]'s.  Only a host with a lease frame has
/// anything to exempt a worker from, so exarch installs it while the ral hosts —
/// whose spawns already live until cancel or exit — leave it out, making the name
/// a compile-time unknown there rather than a call-time refusal.
pub static SERVICE_BUILTIN: &[BuiltinEntry] = &[BuiltinEntry {
    name: Cow::Borrowed("service"),
    type_rule: BuiltinTypeRule::Scheme(Some(2), scheme::service),
    doc: "service <desc> <thunk>  — birth a durable worker: like spawn, but never idle-reaped and exempt from the 24 h backstop. <desc> is a required, single-line, non-empty description of what it's for — a durable service's only bound is legibility, so the host tracks it by this description rather than a lease. Dies only by `cancel` through its handle, /clear, or process exit. It does not outlive this process: work that must still be running after this process exits needs `detach`.",
    body: BuiltinBody::Static(concurrency::builtin_service),
}];

/// `detach` — a birth this session renounces: the process reparents to init.
///
/// Carried only by a host that also arms a detach policy
/// ([`crate::types::Shell::arm_detach`]) — one act, so absence is an unknown-name
/// diagnostic and never a veto.  Whether
/// a call that does resolve may *spend* the verb is the separate capability
/// question, asked of the live grant stack
/// ([`crate::types::GrantStack::permits_detach`]) and answered as a refusal.
///
/// The other three verbs vary policy over one type; this one changes it — a
/// variadic effect, hence no `$detach`, returning a plain record rather than a
/// [`Value::Handle`] because no eliminator here can reach what it births.  The
/// birth is a double-fork, which Windows has no analogue of.
#[cfg(unix)]
pub static DETACH_BUILTIN: &[BuiltinEntry] = &[BuiltinEntry {
    name: Cow::Borrowed("detach"),
    type_rule: BuiltinTypeRule::Sig(sig::DETACH),
    doc: "detach <desc> <cmd> <args...>  — run a program that keeps running after this session is over. Returns a receipt {pid, desc}: data, not a handle — await, poll, race and cancel do not apply, and nothing in ral can stop it once it is born. It is also mute. Its stdin, stdout and stderr are all /dev/null, and its exit status is unrecoverable, since init reaps it and nothing here can ever wait for it: if it dies at startup — port already in use, bad flag, a missing import — nothing observes that, and a returned pid says only that the program was exec'd, never that it is alive or that it worked. The one way to learn whether it is running is to probe whatever it serves: connect to the port, fetch the URL, read the file it writes. Give it its own logging if you want a record of what it did. <pid> is the name it had at birth, not a capability over it — pids are recycled, so that number may later name something else entirely. Only cwd and env cross into it, from the enclosing `within`; bindings and the audit tree do not, and a head that a handler in scope intercepts runs that handler instead, birthing nothing. A grant you birth it inside confines it for the rest of its life: it keeps the fs, net and exec limits in force at that moment, and nothing later can widen them, since nothing later can name it. A grant may also withhold the verb outright with `detach: false`, in which case the call is refused and no process is born. <desc> is required, single-line and non-empty: once this session is gone it is all that says what the pid was for.",
    body: BuiltinBody::Static(concurrency::builtin_detach),
}];

/// Synthesise the first-class thunk a `$name` reference resolves to: an n-ary
/// lambda over a name-dispatched [`crate::ir::CompKind::Exec`], where `n` is the
/// entry's fixed arity.  `None` for builtins with no value form.
///
/// The body dispatches by [`crate::ir::CommandWord::Name`] rather than calling
/// the entry's thunk, so a `within [handlers: …]` frame installed later still
/// intercepts the reified primitive when the lambda is applied.
pub fn synthesize_builtin_value(entry: &BuiltinEntry) -> Option<Value> {
    use crate::ir::{CommandName, CommandWord, CompKind, Exec, IrPattern, Val};
    use crate::source::Spanned;
    use crate::typecheck::builtins::BuiltinTypeRule;

    let arity = entry.fixed_arity()?;
    match entry.type_rule {
        BuiltinTypeRule::Scheme(..) => {}
        BuiltinTypeRule::Sig(sig) if sig.value.is_some() => {}
        BuiltinTypeRule::Sig(_) => return None,
    }
    let name: &str = entry.name.as_ref();

    let arg_vars: crate::ir::Args = (0..arity)
        .map(|i| {
            Spanned::synthetic(crate::ir::ValListElem::Single(Val::Variable(format!(
                "__b{i}"
            ))))
        })
        .collect();
    let mut body = Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::Name(CommandName::Bare(name.into())),
        args: arg_vars,
        redirects: Vec::new(),
    }));
    // Innermost outward, leaving `__b0` outermost so it binds the first argument.
    for i in (0..arity).rev() {
        body = Spanned::synthetic(CompKind::Lam {
            param: IrPattern::Name(format!("__b{i}")),
            body: Arc::new(body),
        });
    }
    // Arity 0 never wraps, so the bare `Exec` becomes a `Block`, not a `Lambda`.
    let captured = Arc::new(Env::default());
    Some(match body.item {
        CompKind::Lam {
            param,
            body: lam_body,
        } => Value::Lambda {
            param,
            body: lam_body,
            captured,
        },
        _ => Value::Block {
            body: Arc::new(body),
            captured,
        },
    })
}

/// Clone the prelude's top-level bindings into `shell`'s environment.
///
/// The prelude — a ral script baked into the binary — is evaluated once per
/// process; every later shell gets a copy of what that run left behind.
pub fn register(shell: &mut Shell, prelude_comp: &Arc<crate::ir::Comp>) {
    static PRELUDE_BINDINGS: OnceLock<HashMap<String, Binding>> = OnceLock::new();

    let bindings = PRELUDE_BINDINGS.get_or_init(|| {
        let mut prelude_env = Shell::new(crate::io::TerminalState::default());

        // `Val::from_word` already classifies bare `true`/`false` in the
        // elaborator; these bindings exist only so `$true` / `$false` resolve.
        prelude_env
            .mobile
            .scope
            .set("true".into(), Value::Bool(true));
        prelude_env
            .mobile
            .scope
            .set("false".into(), Value::Bool(false));

        if let Err(e) = crate::evaluate(prelude_comp, &Mooring::adrift(), &mut prelude_env) {
            let msg = match &e {
                Break::Error(err) => err.to_string(),
                Break::Escape(Escape::Exit(code)) => format!("exit {code}"),
                #[cfg(unix)]
                Break::Escape(Escape::Stopped { signal, cmd, .. }) => {
                    format!("{cmd}: stopped by signal {}", signal.display())
                }
            };
            diagnostic::cmd_error("prelude", &msg);
        }
        prelude_env.mobile.scope.top_scope().clone()
    });

    for (name, binding) in bindings {
        shell
            .mobile
            .scope
            .set_binding(name.clone(), binding.clone());
    }

    // Keep the prelude alone in scopes[0], so a lookup can tell a prelude
    // binding from a user one.
    shell.mobile.scope.push_scope();
}

pub use print::{PrintParams, REPL_PRINT_PARAMS, pretty_print};

/// Apply a thunk (`Block` or `Lambda`) to `args`, with a run frame already
/// installed: builtins that take function arguments call this, as does the run
/// door's hook arm ([`crate::Shell::run`]), which establishes that frame first.
///
/// # Errors
/// If `val` is neither a `Block` nor a `Lambda`, or the applied thunk fails.
pub fn apply(val: &Value, args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    match val {
        Value::Lambda { .. } | Value::Block { .. } => {
            crate::evaluator::apply(val.clone(), args.to_vec(), mooring, shell)
        }
        _ => Err(Break::Error(
            Error::new(format!("cannot call {} '{}'", val.type_name(), val), 1)
                .with_hint("only Blocks and Lambdas can be called"),
        )),
    }
}
