//! Builtin command bindings and registration.
//!
//! Builtins are commands implemented in Rust that run inside the shell
//! process.  Each builtin is registered in a single
//! [`builtin_registry!`] entry that names the builtin, its computation-
//! type hint, its fixed arity (if first-class-callable), its one-line
//! doc, and its runtime body — so adding a new builtin can update only
//! one place and the six facets cannot drift apart.  The macro emits a
//! [`CORE_BUILTINS`] static (a `&[BuiltinEntry]`) consumed by each shell's
//! builtin table.
//!
//! The prelude (a ral script baked into the binary) is evaluated once
//! per process; its top-level bindings are cloned into every fresh
//! environment via [`register`].

use crate::diagnostic;
use crate::types::*;
// `BuiltinTypeRule` is brought into scope via `use ... ::*` below so registry
// entries can write `ty: Scheme(None, scheme::length)` or `ty: Sig(sig::RANGE)`
// without the `BuiltinTypeRule::` prefix on every line.
#[allow(unused_imports)]
use crate::typecheck::builtins::BuiltinTypeRule;
#[allow(unused_imports)]
use crate::typecheck::builtins::BuiltinTypeRule::{Scheme, Sig};
#[allow(unused_imports)]
use crate::typecheck::builtins::{scheme, sig};
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

mod codecs;
mod collections;
pub(crate) mod concurrency;
mod fs;
mod math;
pub mod misc;
pub mod modules;
mod predicates;
mod shell;
mod strings;
pub use util::{value_to_json, value_to_json_lossy_bytes};
pub mod util;
pub mod uutils;

// ── builtin_arity_from ───────────────────────────────────────────────────

/// Expand `_` to `None`, or a number literal `n` to `Some(n)`.
macro_rules! builtin_arity_from {
    (_) => {
        None
    };
    ($n:literal) => {
        Some($n)
    };
}

/// True if a declared `arity:` agrees with a signature's structural
/// arity (`Option` equality, usable in const context).
const fn arity_agrees(declared: Option<usize>, structural: Option<usize>) -> bool {
    match (declared, structural) {
        (Some(a), Some(b)) => a == b,
        (None, None) => true,
        _ => false,
    }
}

/// Single source of truth for the runtime side of every builtin.
///
/// Every entry binds six facets at once: the user-visible names, the
/// computation-type hint consumed by the inference engine, a fixed arity
/// (if the builtin is first-class-callable and not variadic), the doc
/// string the `help` builtin prints, the type-checker rule, and a
/// `call` block that produces the runtime result.  The macro emits a
/// single [`BuiltinEntry`] per name into the [`CORE_BUILTINS`] static
/// array, so all facets observe the same registration and no
/// out-of-band match table can drift from the docs/types/arity.
///
/// The `arity:` field is authoritative for `Scheme` rules: the
/// written value is injected into the emitted [`BuiltinTypeRule`].  For
/// `Sig` rules the structural arity follows from the signature's argument
/// policy, so the written `arity:` is checked against it at compile time
/// (the static is const-evaluated, and a mismatch is a const-panic build
/// error) — the two cannot drift apart.
///
/// Entries with `arity: _` are not first-class-callable (variadic or
/// command-only); `$name` is available only if the type signature has an
/// explicit value form.
///
/// The `call` block is a non-capturing closure `|args, shell| body`
/// returning `Settled<Value>`; the macro wraps it into a per-entry
/// adapter function, nested in `__core_thunks` so the variant name
/// cannot collide with a public type in scope.
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
        // Per-entry adapter fns.  Nested in a module so variant names
        // (e.g. `Map`, `Echo`) cannot collide with public types in
        // scope at this module level.
        mod __core_thunks {
            #[allow(unused_imports)]
            use super::*;
            $(
                $(#[$meta])*
                #[allow(non_snake_case)]
                pub(super) fn $variant(
                    args: &[Value],
                    shell: &mut Shell,
                ) -> Settled<Value> {
                    let inner: fn(&[Value], &mut Shell) -> Settled<Value> = $call;
                    inner(args, shell)
                }
            )+
        }

        /// Core builtins: every host-implemented name the language ships
        /// with, expanded from the registry into a single static slice.
        /// Installed into every fresh [`Shell`] by [`Shell::new`] (via
        /// [`Shell::install_builtins`]) and registered globally for
        /// typecheck-time lookups by [`ensure_core_builtins_registered`].
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
        call: |args, shell| Ok(misc::builtin_clear(args, shell)), },
    Reset { names: ["reset"], arity: 0, ty: Sig(sig::TERMINAL_CONTROL),
        doc: "reset  — emit ESC c (RIS) to reset the terminal. Does not touch stty modes; use `^reset` for the full ncurses terminfo reset.",
        call: |args, shell| Ok(misc::builtin_reset(args, shell)), },
    Each { names: ["each"], arity: 2, ty: Scheme(None, scheme::each_op),
        doc: "each <fn> <list>  — call fn on each element for side effects.",
        call: |args, shell| collections::builtin_each(args, shell), },
    Map { names: ["map"], arity: 2, ty: Scheme(None, scheme::map_op),
        doc: "map <fn> <list>  — apply fn to each element, return new list.",
        call: |args, shell| collections::builtin_map(args, shell), },
    Filter { names: ["filter"], arity: 2, ty: Scheme(None, scheme::filter_op),
        doc: "filter <fn> <list>  — keep elements where fn returns true.",
        call: |args, shell| collections::builtin_filter(args, shell), },
    SortList { names: ["sort-list"], arity: 1, ty: Scheme(None, scheme::sort_list),
        doc: "sort-list <list>  — sort a list lexicographically.",
        call: |args, _shell| collections::builtin_sort(args), },
    SortListBy { names: ["sort-list-by"], arity: 2, ty: Scheme(None, scheme::sort_list_by),
        doc: "sort-list-by <fn> <list>  — sort by a key function.",
        call: |args, shell| collections::builtin_sort_by(args, shell), },
    Fold { names: ["fold"], arity: 3, ty: Scheme(None, scheme::fold_op),
        doc: "fold <fn> <init> <list>  — reduce list left-to-right with fn and accumulator.",
        call: |args, shell| collections::builtin_fold(args, shell), },
    Range { names: ["range"], arity: 2, ty: Sig(sig::RANGE),
        doc: "range <start> <end>  — generate a list of integers from start (inclusive) to end (exclusive).",
        call: |args, shell| collections::builtin_range(args, shell), },
    Fail { names: ["fail"], arity: 1, ty: Sig(sig::FAIL),
        doc: "fail <status>  — exit with error status.",
        call: |args, _shell| Err(misc::builtin_fail(args)), },
    Len { names: ["length"], arity: 1, ty: Scheme(None, scheme::length),
        doc: "length <val>  — number of elements in a string, bytes, list, or map.",
        call: |args, _shell| strings::builtin_len(args), },
    Upper { names: ["upper"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "upper <s>  — convert a string to uppercase.",
        call: |args, _shell| strings::builtin_upper(args), },
    Lower { names: ["lower"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "lower <s>  — convert a string to lowercase.",
        call: |args, _shell| strings::builtin_lower(args), },
    Dedent { names: ["dedent"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "dedent <s>  — strip common leading whitespace from every non-empty line.",
        call: |args, _shell| strings::builtin_dedent(args), },
    Intercalate { names: ["intercalate"], arity: 2, ty: Scheme(None, scheme::intercalate),
        doc: "intercalate <sep> <items>  — interpose sep between every pair of items, concatenated as one string.",
        call: |args, _shell| strings::builtin_join(args), },
    Slice { names: ["slice"], arity: 3, ty: Scheme(None, scheme::slice),
        doc: "slice <s> <start> <count>  — extract a substring by character offset.",
        call: |args, _shell| strings::builtin_slice(args), },
    Split { names: ["re-split"], arity: 2, ty: Scheme(None, scheme::re_split),
        doc: "re-split <pattern> <s>  — split a string by a regex pattern.",
        call: |args, _shell| strings::builtin_split(args), },
    Match { names: ["re-match"], arity: 2, ty: Scheme(None, scheme::re_match),
        doc: "re-match <pattern> <s>  — true if regex pattern matches anywhere in s.",
        call: |args, shell| strings::builtin_match(args, shell), },
    FindMatch { names: ["re-find-match"], arity: 2, ty: Scheme(None, scheme::re_find_match),
        doc: "re-find-match <pattern> <s>  — first regex match, or fail if none.",
        call: |args, _shell| strings::builtin_find_match(args), },
    FindMatches { names: ["re-find-matches"], arity: 2, ty: Scheme(None, scheme::re_split),
        doc: "re-find-matches <pattern> <s>  — all non-overlapping regex matches as a list.",
        call: |args, _shell| strings::builtin_find_matches(args), },
    Replace { names: ["re-replace"], arity: 3, ty: Scheme(None, scheme::replace_3),
        doc: "re-replace <pattern> <repl> <s>  — replace first regex match; $1 etc. backreferences.",
        call: |args, _shell| strings::builtin_replace(args), },
    ReplaceAll { names: ["re-replace-all"], arity: 3, ty: Scheme(None, scheme::replace_3),
        doc: "re-replace-all <pattern> <repl> <s>  — replace every regex match.",
        call: |args, _shell| strings::builtin_replace_all(args), },
    StrReplace { names: ["string-replace"], arity: 3, ty: Scheme(None, scheme::replace_3),
        doc: "string-replace <from> <to> <s>  — replace the unique literal occurrence of <from> in <s> with <to>; error on 0 or >1 matches. No regex.",
        call: |args, _shell| strings::builtin_string_replace(args), },
    ShellQuote { names: ["shell-quote"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "shell-quote <s>  — quote a string for safe shell-word use.",
        call: |args, _shell| strings::builtin_shell_quote(args), },
    ShellSplit { names: ["shell-split"], arity: 1, ty: Scheme(None, scheme::shell_split),
        doc: "shell-split <s>  — split a shell-quoted string into a list of words.",
        call: |args, _shell| strings::builtin_shell_split(args), },
    Keys { names: ["keys"], arity: 1, ty: Scheme(None, scheme::keys),
        doc: "keys <map>  — list of map keys.",
        call: |args, _shell| predicates::builtin_keys(args), },
    Has { names: ["has"], arity: 2, ty: Scheme(None, scheme::has),
        doc: "has <map> <key>  — true if map contains key.",
        call: |args, shell| predicates::builtin_has(args, shell), },
    ResolvePath { names: ["resolve-path"], arity: 1, ty: Scheme(None, scheme::str_to_str),
        doc: "resolve-path <path>  — resolve to an absolute path.",
        call: |args, shell| fs::builtin_resolve_path(args, shell), },
    Glob { names: ["glob"], arity: 1, ty: Scheme(None, scheme::glob),
        doc: "glob <pattern>  — list paths matching a Unix shell-style glob (`?`, `*`, `**`, `[…]`, `[!…]`; `**` spans directories). Patterns relative to the current working directory return matches relative to the current working directory; sigil-rooted (`~`, `xdg:`) and absolute patterns return absolute matches. Dotfiles are excluded from wildcard matches — match them by fully-literal name, or fall back to `list-dir | filter`.",
        call: |args, shell| fs::builtin_glob(args, shell), },
    Exit { names: ["exit", "quit"], arity: 1, ty: Scheme(None, scheme::exit_op),
        doc: "exit [status]  — exit the shell.",
        call: |args, shell| misc::builtin_exit(args, shell), },
    Surface { names: ["surface"], arity: 1, ty: Scheme(None, scheme::surface_op),
        doc: "surface <event>  — forward a tagged variant event to the host's structured-event sink; the identity when no host is installed.",
        call: |args, shell| misc::builtin_surface(args, shell), },
    FoldLines { names: ["fold-lines"], arity: 2, ty: Scheme(None, scheme::fold_lines),
        doc: "fold-lines <fn> <init>  — fold over stdin lines.",
        call: |args, shell| codecs::builtin_fold_lines(args, shell), },
    FromBytes { names: ["from-bytes"], arity: _, ty: Sig(sig::FROM_BYTES),
        doc: "from-bytes  — read raw bytes from the channel (stdin / `< file` / pipe) as Bytes.",
        call: |args, shell| codecs::builtin_from_bytes(args, shell), },
    FromString { names: ["from-string"], arity: _, ty: Sig(sig::FROM_STRING),
        doc: "from-string  — decode UTF-8 bytes from the channel to a String.",
        call: |args, shell| codecs::builtin_from_string(args, shell), },
    FromLine { names: ["from-line"], arity: _, ty: Sig(sig::FROM_STRING),
        doc: "from-line  — decode UTF-8 bytes from the channel, stripping one trailing newline.",
        call: |args, shell| codecs::builtin_from_line(args, shell), },
    FromLines { names: ["from-lines"], arity: _, ty: Sig(sig::FROM_LINES),
        doc: "from-lines  — decode channel bytes to a Step stream of lines (lossy on invalid UTF-8).",
        call: |args, shell| codecs::builtin_from_lines(args, shell), },
    FromJson { names: ["from-json"], arity: _, ty: Sig(sig::FROM_JSON),
        doc: "from-json  — decode JSON bytes from the channel to a value.",
        call: |args, shell| codecs::builtin_from_json(args, shell), },
    FromCsv { names: ["from-csv"], arity: _, ty: Sig(sig::FROM_JSON),
        doc: "from-csv  — decode CSV bytes from the channel to a list of records keyed by the header row (every field a String).",
        call: |args, shell| codecs::builtin_from_csv(args, shell), },
    ToBytes { names: ["to-bytes"], arity: 1, ty: Sig(sig::TO_BYTES),
        doc: "to-bytes <value>  — pass Bytes (or list of Ints) through to the byte channel.",
        call: |args, shell| codecs::builtin_to_bytes(args, shell), },
    ToString { names: ["to-string"], arity: 1, ty: Sig(sig::TO_ANY_BYTES),
        doc: "to-string <value>  — encode a value's String form to the byte channel.",
        call: |args, shell| codecs::builtin_to_string(args, shell), },
    ToLine { names: ["to-line"], arity: 1, ty: Sig(sig::TO_ANY_BYTES),
        doc: "to-line <value>  — encode value with a trailing newline (inverse of from-line).",
        call: |args, shell| codecs::builtin_to_line(args, shell), },
    ToLines { names: ["to-lines"], arity: 1, ty: Sig(sig::TO_LINES),
        doc: "to-lines <list>  — newline-join the list elements to the byte channel.",
        call: |args, shell| codecs::builtin_to_lines(args, shell), },
    ToJson { names: ["to-json"], arity: 1, ty: Sig(sig::TO_ANY_BYTES),
        doc: "to-json <value>  — encode a value as JSON bytes.",
        call: |args, shell| codecs::builtin_to_json(args, shell), },
    ToCsv { names: ["to-csv"], arity: 1, ty: Sig(sig::TO_ANY_BYTES),
        doc: "to-csv <records>  — encode a list of records as CSV bytes; columns are the first record's keys in sorted order.",
        call: |args, shell| codecs::builtin_to_csv(args, shell), },
    Ask { names: ["ask"], arity: 1, ty: Scheme(None, scheme::ask),
        doc: "ask <prompt>  — prompt for interactive input, return string.",
        call: |args, _shell| misc::builtin_ask(args).map_err(Break::from), },
    Source { names: ["source"], arity: 1, ty: Scheme(None, scheme::source_op),
        doc: "source <file>  — execute a .ral script file.",
        call: |args, shell| modules::builtin_source(args, shell), },
    Use { names: ["use"], arity: 1, ty: Scheme(None, scheme::use_op),
        doc: "use <file>  — load a .ral module, returning its bindings as a map.",
        call: |args, shell| modules::builtin_use(args, shell), },
    Cwd { names: ["cwd"], arity: 0, ty: Scheme(None, scheme::pure_string),
        doc: "cwd  — return the current working directory as a String.",
        call: |_args, shell| Ok(Value::String(shell.cwd().to_string_lossy().into_owned())), },
    Chdir { names: ["cd"], arity: _, ty: Sig(sig::CHDIR),
        doc: "cd [path]  — change the shell working directory; gated by shell.chdir capability. Empty/missing path means $HOME.",
        call: |args, shell| shell::builtin_chdir(args, shell), },
    Alias { names: ["alias"], arity: 2, ty: Sig(sig::ALIAS),
        doc: "alias NAME { |args| BODY }  — install BODY as a handler-frame alias for NAME; replaces any prior alias for the same name. Persists past `within` blocks; remove with `unalias`.",
        call: |args, shell| shell::builtin_alias(args, shell), },
    Unalias { names: ["unalias"], arity: 1, ty: Sig(sig::UNALIAS),
        doc: "unalias NAME  — remove the alias for NAME. Errors if none is installed.",
        call: |args, shell| shell::builtin_unalias(args, shell), },
    IsEmpty { names: ["is-empty"], arity: 1, ty: Scheme(None, scheme::is_empty),
        doc: "is-empty <val>  — true if list, map, bytes, or string is empty.",
        call: |args, shell| predicates::builtin_is_empty(args, shell), },
    Exists { names: ["exists"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "exists <path>  — true if path exists (any type); resolves against the within-scoped cwd.",
        call: |args, shell| predicates::builtin_exists(args, shell), },
    IsFile { names: ["is-file"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-file <path>  — true if path is a regular file (follows symlinks).",
        call: |args, shell| predicates::builtin_is_file(args, shell), },
    IsDir { names: ["is-dir"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-dir <path>  — true if path is a directory (follows symlinks).",
        call: |args, shell| predicates::builtin_is_dir(args, shell), },
    IsLink { names: ["is-link"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-link <path>  — true if path is a symbolic link.",
        call: |args, shell| predicates::builtin_is_link(args, shell), },
    IsReadable { names: ["is-readable"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-readable <path>  — true if path is readable by the caller.",
        call: |args, shell| predicates::builtin_is_readable(args, shell), },
    IsWritable { names: ["is-writable"], arity: 1, ty: Sig(sig::PATH_BOOL),
        doc: "is-writable <path>  — true if path is writable by the caller.",
        call: |args, shell| predicates::builtin_is_writable(args, shell), },
    Equal { names: ["equal"], arity: 2, ty: Scheme(None, scheme::compare),
        doc: "equal <a> <b>  — true if a and b are equal.",
        call: |args, shell| predicates::builtin_equal(args, shell), },
    Lt { names: ["lt"], arity: 2, ty: Scheme(None, scheme::compare),
        doc: "lt <a> <b>  — true if a < b (numeric on Int/Float, lexicographic on String).",
        call: |args, shell| predicates::builtin_lt(args, shell), },
    Gt { names: ["gt"], arity: 2, ty: Scheme(None, scheme::compare),
        doc: "gt <a> <b>  — true if a > b (numeric on Int/Float, lexicographic on String).",
        call: |args, shell| predicates::builtin_gt(args, shell), },
    ListDir { names: ["list-dir"], arity: 1, ty: Scheme(None, scheme::list_dir),
        doc: "list-dir <path>  — list directory contents as [{name, type, size, mtime}].",
        call: |args, shell| fs::builtin_list_dir(args, shell), },
    FileInfo { names: ["file-info"], arity: 1, ty: Scheme(None, scheme::file_info),
        doc: "file-info <path>  — {name, type, size, mtime, atime, btime, readonly, target}; uses lstat (no symlink follow); 0 when a timestamp is unavailable.",
        call: |args, shell| fs::builtin_file_info(args, shell), },
    TempDir { names: ["temp-dir"], arity: 0, ty: Scheme(None, scheme::temp_path),
        doc: "temp-dir  — create a temporary directory.",
        call: |args, shell| fs::builtin_temp_dir(args, shell), },
    TempFile { names: ["temp-file"], arity: 0, ty: Scheme(None, scheme::temp_path),
        doc: "temp-file  — create a temporary file.",
        call: |args, shell| fs::builtin_temp_file(args, shell), },
    ToInt { names: ["int"], arity: 1, ty: Sig(sig::INT_PARSE),
        doc: "int <val>  — parse a value as an integer.",
        call: |args, _shell| strings::builtin_to_int(args), },
    ToFloat { names: ["float"], arity: 1, ty: Sig(sig::FLOAT_PARSE),
        doc: "float <val>  — parse a value as a float.",
        call: |args, _shell| strings::builtin_to_float(args), },
    Round { names: ["round"], arity: 2, ty: Sig(sig::ROUND),
        doc: "round <x> <places>  — round a Float to <places> decimal places, halves away from zero; always returns a Float (round 3.7 0 is 4.0). Use int for a whole number.",
        call: |args, _shell| math::builtin_round(args), },
    Floor { names: ["floor"], arity: 1, ty: Sig(sig::FLOAT_TO_INT),
        doc: "floor <x>  — the greatest Int not exceeding a Float.",
        call: |args, _shell| math::builtin_floor(args), },
    Ceil { names: ["ceil"], arity: 1, ty: Sig(sig::FLOAT_TO_INT),
        doc: "ceil <x>  — the least Int not below a Float.",
        call: |args, _shell| math::builtin_ceil(args), },
    Trunc { names: ["trunc"], arity: 1, ty: Sig(sig::FLOAT_TO_INT),
        doc: "trunc <x>  — drop a Float's fractional part toward zero, yielding an Int.",
        call: |args, _shell| math::builtin_trunc(args), },
    Str { names: ["str"], arity: 1, ty: Sig(sig::STR_PARSE),
        doc: "str <val>  — convert a value to its string representation.",
        call: |args, _shell| strings::builtin_to_string(args), },
    Spawn { names: ["spawn"], arity: 1, ty: Scheme(None, scheme::spawn),
        doc: "spawn <thunk>  — spawn a concurrent block on a worker thread, return a handle.",
        call: |args, shell| concurrency::builtin_spawn(args, shell), },
    Await { names: ["await"], arity: 1, ty: Scheme(None, scheme::await_op),
        doc: "await <handle>  — wait for a concurrent block to complete, returning {value, stdout, stderr}; re-raises a failed block.",
        call: |args, shell| concurrency::builtin_await(args, shell), },
    Poll { names: ["poll"], arity: 1, ty: Scheme(None, scheme::poll),
        doc: "poll <handle>  — non-blocking sample of a concurrent block: `settled with {stdout, stderr, outcome: `ok/`err} once it finishes, or `pending while still running. Never re-raises.",
        call: |args, shell| concurrency::builtin_poll(args, shell), },
    Race { names: ["race"], arity: 1, ty: Scheme(None, scheme::race),
        doc: "race <handles>  — wait for the first of several concurrent blocks to finish, returning {value, stdout, stderr}; re-raises a failed winner.",
        call: |args, shell| concurrency::builtin_race(args, shell), },
    Cancel { names: ["cancel"], arity: 1, ty: Scheme(None, scheme::cancel_op),
        doc: "cancel <handle>  — cancel a running concurrent block.",
        call: |args, shell| concurrency::builtin_cancel(args, shell), },
    // Bundled uutils tools (cat, yes, head, wc, ...) are not builtins.
    // `runtime::command` routes a bare invocation either through an
    // in-process `uumain` call (the clean-terminal fast path) or by
    // spawning the `ral --ral-bundled-tool <tool>` command image as an
    // ordinary child, so they ride through the same wait / signal /
    // exit-code boundary as a system binary — one spawn site, one
    // broken-pipe rule.  See [`crate::builtins::uutils::is_uutils_tool`].
    // _type's signature carries a probe diagnostic: typing is ordinary
    // `α → F α`, and the inferencer prints the resolved α as a separate
    // side effect. Runtime is a passthrough.
    TypeOf { names: ["_type"], arity: 1, ty: Sig(sig::TYPE_PROBE),
        doc: "_type <val>  — print inferred type at compile time; passthrough at runtime.",
        call: |args, _shell| Ok(args.first().cloned().unwrap_or(Value::Unit)), },
    Help { names: ["help"], arity: 0, ty: Sig(sig::HELP),
        doc: "help  — print an overview of builtins, prelude, and library; see also `explain`.",
        call: |args, shell| Ok(misc::builtin_help(args, shell)), },
    Explain { names: ["explain"], arity: 1, ty: Sig(sig::EXPLAIN),
        doc: "explain <name>  — print documentation for one name: doc, type signature, and source location.",
        call: |args, shell| Ok(misc::builtin_explain(args, shell)), },
    // `_ed-*` builtins (16 entries) are registered by the `ral` crate's
    // host-builtins table at REPL startup; see
    // `ral::repl::plugin_ed_builtins::HOST_BUILTINS`.
    AnsiOk { names: ["_ansi-ok"], arity: 0, ty: Scheme(None, scheme::pure_bool),
        doc: "_ansi-ok  — true if stdout supports ANSI colour (respects NO_COLOR / non-tty).",
        call: |_args, _shell| Ok(Value::Bool(crate::ansi::use_ui_color())), },
}

/// `watch` — the detached-streaming concurrency builtin, kept out of
/// [`CORE_BUILTINS`] and exposed for a host to install on its own.
///
/// A watched worker is line-framed (`concurrency::builtin_watch` over
/// `spawn_child`'s `ChildIoMode::Watch`) and streams live to the caller's
/// stdout as it runs, so it is admissible only where that sink is durable
/// enough to outlive the turn — an interactive or batch ral host, whose
/// stdout is the real terminal or pipe.  An agent host (exarch), whose
/// active streams are per-call capture buffers, does not install it; naming
/// `watch` there is then a compile-time unknown-name diagnostic, not a
/// builtin that resolves and refuses at call time.
///
/// Only this entry is public — the implementation (`builtin_watch`,
/// `spawn_child`, `Sink::LineFramed`) and the type scheme (`scheme::watch`)
/// stay private to core.  This is the one asymmetry the split introduces:
/// the builtin is implemented in core but registered by the host.
pub static WATCH_BUILTIN: &[BuiltinEntry] = &[BuiltinEntry {
    name: Cow::Borrowed("watch"),
    type_rule: BuiltinTypeRule::Scheme(Some(2), scheme::watch),
    doc: "watch <label> <thunk>  — spawn a concurrent block whose output streams live to the caller's stdout, line-framed with the given label.",
    body: BuiltinBody::Static(concurrency::builtin_watch),
}];

// ─── Process-level builtin registry ────────────────────────────────────────
//
// Typecheck-time helpers ([`is_builtin`], [`builtin_doc`], scheme
// lookup) resolve names without a `Shell`, so builtin sets register
// here in addition to being installed into each shell's builtin table.
// Identity is by storage: a static-slice address (idempotent for repeat
// calls from the same boot path) or an `Arc<[…]>` pointer.

/// One registered builtin set — either a process-static slice (the macro
/// emission for [`CORE_BUILTINS`], or a host crate's static array) or a
/// runtime-owned `Arc` whose closures capture host state.
enum BuiltinSet {
    Static(&'static [BuiltinEntry]),
    Captured(Arc<[BuiltinEntry]>),
}

impl BuiltinSet {
    fn entries(&self) -> &[BuiltinEntry] {
        match self {
            BuiltinSet::Static(s) => s,
            BuiltinSet::Captured(a) => a,
        }
    }
}

static REGISTERED_BUILTINS: RwLock<Vec<BuiltinSet>> = RwLock::new(Vec::new());
static CORE_BUILTINS_REGISTER: std::sync::Once = std::sync::Once::new();

/// Register [`CORE_BUILTINS`] into the process-level registry the first
/// time this is called.  Invoked from [`Shell::new`] and from every
/// helper that consults the registry, so consumers don't need to
/// remember a separate init step.
pub fn ensure_core_builtins_registered() {
    CORE_BUILTINS_REGISTER.call_once(|| {
        REGISTERED_BUILTINS
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(BuiltinSet::Static(CORE_BUILTINS));
    });
}

/// Register static host builtins.  Idempotent: re-registering the same
/// `&'static` slice (by pointer identity) is a no-op.  Name collisions
/// with already-registered builtins panic — host crates must own
/// disjoint surfaces.
pub fn register_builtins(entries: &'static [BuiltinEntry]) {
    register_builtins_checked(entries).expect("builtin registration failed");
}

/// Fallible form of [`register_builtins`].
pub fn register_builtins_checked(entries: &'static [BuiltinEntry]) -> Result<(), String> {
    ensure_core_builtins_registered();
    let mut sets = REGISTERED_BUILTINS
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    if sets.iter().any(|h| match h {
        BuiltinSet::Static(s) => std::ptr::eq(*s, entries),
        BuiltinSet::Captured(_) => false,
    }) {
        return Ok(());
    }
    check_builtin_collisions(entries, &sets)?;
    sets.push(BuiltinSet::Static(entries));
    Ok(())
}

/// Register captured builtins (ones whose bodies hold runtime state).
pub fn register_captured_builtins(entries: Arc<[BuiltinEntry]>) {
    ensure_core_builtins_registered();
    let mut sets = REGISTERED_BUILTINS
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    if sets.iter().any(|h| match h {
        BuiltinSet::Captured(a) => Arc::ptr_eq(a, &entries) || same_builtin_names(a, &entries),
        BuiltinSet::Static(_) => false,
    }) {
        return;
    }
    if let Err(e) = check_builtin_collisions(&entries, &sets) {
        panic!("captured builtin registration failed: {e}");
    }
    sets.push(BuiltinSet::Captured(entries));
}

fn same_builtin_names(a: &[BuiltinEntry], b: &[BuiltinEntry]) -> bool {
    a.len() == b.len()
        && a.iter()
            .map(|entry| entry.name.as_ref())
            .all(|name| b.iter().any(|entry| entry.name == name))
}

fn check_builtin_collisions(
    new_entries: &[BuiltinEntry],
    registered: &[BuiltinSet],
) -> Result<(), String> {
    let mut local = HashSet::new();
    for entry in new_entries {
        let name = entry.name.as_ref();
        if !local.insert(name) {
            return Err(format!(
                "builtin `{name}` is registered twice in one builtin set"
            ));
        }
        if registered
            .iter()
            .flat_map(|h| h.entries().iter())
            .any(|existing| existing.name == name)
        {
            return Err(format!(
                "builtin `{name}` conflicts with a registered builtin"
            ));
        }
    }
    Ok(())
}

/// Walk registered builtins newest-first for a per-name entry.  The
/// callback projects out a value that survives the lock release.
fn with_entry<R>(name: &str, f: impl FnOnce(&BuiltinEntry) -> R) -> Option<R> {
    ensure_core_builtins_registered();
    let sets = REGISTERED_BUILTINS
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    sets.iter()
        .rev()
        .flat_map(|h| h.entries().iter())
        .find(|e| e.name == name)
        .map(f)
}

/// True if `name` is registered as a builtin.
pub fn is_builtin(name: &str) -> bool {
    with_entry(name, |_| ()).is_some()
}

/// Doc string for a registered name, or `None` if not installed.
pub fn builtin_doc(name: &str) -> Option<&'static str> {
    with_entry(name, |e| e.doc)
}

/// Fixed value-arg count; `None` for variadic / command-only entries
/// and for unknown names.
pub fn builtin_arity(name: &str) -> Option<usize> {
    with_entry(name, |e| e.fixed_arity())?
}

/// Type-checker rule for a registered name.
pub fn builtin_type_rule(name: &str) -> Option<BuiltinTypeRule> {
    with_entry(name, |e| e.type_rule)
}

/// All registered builtin names, in registration order.  Captured
/// entries must use `Cow::Borrowed` names so this returns
/// `&'static str`.
pub fn builtin_names() -> Vec<&'static str> {
    ensure_core_builtins_registered();
    let sets = REGISTERED_BUILTINS
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    let mut names: Vec<&'static str> = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::new();
    for entry in sets.iter().flat_map(|h| h.entries().iter()) {
        if let Cow::Borrowed(s) = &entry.name
            && seen.insert(*s)
        {
            names.push(*s);
        }
    }
    names
}

/// Synthesise a first-class thunk for a [`BuiltinEntry`] so a
/// `$name` reference resolves to a callable value.  The thunk
/// wraps an n-ary lambda around a name-dispatched
/// [`crate::ir::CompKind::Exec`], where `n` is the entry's fixed
/// arity, so the resulting value plays the same role as any
/// user-written closure.  Returns `None` for builtins without a
/// first-class value form.
///
/// The body uses [`CommandWord::Name`] rather than calling the
/// entry's thunk directly — that way a future `within [handlers:
/// …]` frame still intercepts the reified primitive when the
/// synthesised lambda is later applied.
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

    // Body: Exec(Name(name), [Variable("__b0"), …]) — name-dispatched
    // at call time, so a `within [handlers: …]` frame can intercept a
    // reified builtin the same way it intercepts a bare-command call.
    // Each curried bound arg `__b{i}` becomes a `Single` element with
    // no span — synthetic thunks built here have no surface position
    // to attribute.
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
    // Wrap in nested lambda abstractions from innermost outward: __b{n-1}. … __b0. body.
    for i in (0..arity).rev() {
        body = Spanned::synthetic(CompKind::Lam {
            param: IrPattern::Name(format!("__b{i}")),
            body: Arc::new(body),
        });
    }
    // The outermost wrap is `Lam` whenever arity > 0; lift its param so
    // the produced value is a `Value::Lambda` directly.  Arity 0 yields
    // a `Value::Block` whose body is the bare `Exec`.
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

/// Register prelude definitions into the environment.
pub fn register(shell: &mut Shell, prelude_comp: &Arc<crate::ir::Comp>) {
    static PRELUDE_BINDINGS: OnceLock<HashMap<String, Binding>> = OnceLock::new();

    // Evaluate the prelude once per process, then clone the resulting
    // top-level bindings into each fresh environment.
    let bindings = PRELUDE_BINDINGS.get_or_init(|| {
        let mut prelude_env = Shell::new(Default::default());

        // Bare-word `true`/`false` are already classified by
        // `Val::from_word` in the elaborator, but `$true` / `$false`
        // need an actual binding to resolve — these entries cover that
        // explicit-sigil path.
        prelude_env
            .mobile
            .scope
            .set("true".into(), Value::Bool(true));
        prelude_env
            .mobile
            .scope
            .set("false".into(), Value::Bool(false));

        let saved_script = prelude_env.turn.loc.script.clone();
        prelude_env.turn.loc.script = "<prelude>".into();
        if let Err(e) = crate::evaluate(prelude_comp, &mut prelude_env) {
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
        prelude_env.turn.loc.script = saved_script;
        prelude_env.mobile.scope.top_scope().clone()
    });

    for (name, binding) in bindings {
        shell
            .mobile
            .scope
            .set_binding(name.clone(), binding.clone());
    }

    // Push a user scope so that prelude bindings (scopes[0]) can be
    // distinguished from user bindings (scopes[1..]) in the lookup chain.
    shell.mobile.scope.push_scope();
}

pub use misc::pretty_print;

/// Apply a thunk (`Block` or `Lambda`) `val` to `args` while a turn frame is
/// already installed.  Any other `Value` produces a descriptive error.  Used
/// by builtins that accept function arguments and by the value turn door
/// ([`crate::Shell::run_value_turn`]), which establishes the frame first.
pub fn apply(val: &Value, args: &[Value], shell: &mut Shell) -> Settled<Value> {
    match val {
        Value::Lambda { .. } | Value::Block { .. } => {
            crate::evaluator::apply(val.clone(), args.to_vec(), shell)
        }
        _ => Err(Break::Error(
            Error::new(format!("cannot call {} '{}'", val.type_name(), val), 1)
                .with_hint("only Blocks and Lambdas can be called"),
        )),
    }
}
