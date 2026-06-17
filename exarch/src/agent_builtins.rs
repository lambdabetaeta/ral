//! Exarch-owned agent search and edit primitives.
//!
//! These are static host builtins: process-owned Rust atoms registered
//! with `ral-core` before the shell compiles user/model source.  Dynamic
//! plugins remain source/alias/hook loaders; this module only publishes
//! the resident agent surface that core should not own.

use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, SearcherBuilder, sinks::Lossy};
use ignore::WalkBuilder;
use ral_core::builtins::util::{check_arity, regex_err};
use ral_core::typecheck::builtins::{
    BuiltinTypeRule, closed_record, fun, mk_scheme as scheme, pure, thunk,
};
use ral_core::typecheck::{Scheme, Ty, Unifier};
use ral_core::types::{Break, BuiltinBody, BuiltinEntry, Settled, sig};
use ral_core::{Shell, Value};
use std::borrow::Cow;
use std::fs;
use std::io::Write;

const AGENT_SOURCE: &str = include_str!("../data/agent.ral");

/// Register the exarch builtins process-wide and install them into `shell`.
/// Idempotent.
pub fn install_on(shell: &mut ral_core::Shell) {
    ral_core::builtins::register_builtins(EXARCH_BUILTINS);
    shell.install_builtins(EXARCH_BUILTINS);
}

/// Source the embedded agent helper library into the live shell.
pub fn install_agent_library(shell: &mut Shell) -> Settled<Value> {
    ral_core::builtins::modules::evaluate_source(shell, AGENT_SOURCE, "<exarch:agent>").map_err(
        |e| match e {
            Break::Error(err) => sig(format!("exarch agent library: {}", err.message)),
            other => other,
        },
    )
}

/// One-line docs for the helper-library functions sourced from
/// `agent.ral`.  These are ral closures, not registered builtins, so
/// `help` cannot find them on its own; the host hands them to
/// [`ral_core::builtins::misc::register_library_docs`] at boot so the
/// agent library is as discoverable as the prelude.
pub(crate) fn agent_library_docs() -> Vec<(String, String)> {
    [
        ("window-hash", "window-hash ROWS I  — the witness for line I (0-indexed) of the line list ROWS: the hash of the ±3 surrounding lines' line-hashes. What `view` and `grep-files` show and `edit` checks; folding in context distinguishes repeated lines without a position."),
        ("view", "view START END < PATH  — show the half-open line range [START, END), each line tagged `<line-no>\\t<hash>\\t<text>`; the hash is the ±3-context witness `edit` checks."),
        ("view-around", "view-around LINE PEEK < PATH  — show the 2*PEEK+1 lines centred on LINE, tagged like `view`, clamped at the top of the file."),
        ("grep-files", "grep-files PATTERN  — recursively search the cwd (ignore-aware, Rust regex) and stamp each hit with its `window-hash`, giving [{file, line, text, hash}] that feeds straight into `edit`."),
        ("edit", "edit PATH EDITS  — apply a batch of [HASH, NEW-TEXT] pairs in one read/write pass: each replaces the line whose window-hash is HASH (NEW-TEXT is verbatim — a real newline inside '…' splits the line, \\n does not; empty deletes). Atomic — all hashes resolve against the file as read, so edits never interfere; fails writing nothing unless every hash picks exactly one line and no two pairs name the same one."),
    ]
    .into_iter()
    .map(|(n, d)| (n.to_string(), d.to_string()))
    .collect()
}

/// Content hash of a line for witnessed editing: the letter `h` followed
/// by six hex characters of a Blake3 digest, trailing whitespace ignored.
/// The `h` prefix keeps the witness un-lexable as a number — a bare
/// all-digit token in `edit`'s hash position would otherwise elaborate to
/// `Val::Int` and never compare equal to the recomputed `String` hash.
fn line_hash(line: &str) -> String {
    let stripped = line.trim_end();
    let hex = blake3::hash(stripped.as_bytes()).to_hex();
    format!("h{}", &hex[..6])
}

/// Expose `line_hash` to ral.  This is the only irreducibly-Rust part
/// of the read/edit surface: numbering, slicing, and tagging compose in
/// the prelude (`view`), but the Blake3 digest cannot.
fn builtin_line_hash(args: &[Value], _shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "line-hash")?;
    Ok(Value::String(line_hash(&args[0].to_string())))
}

/// The one sanctioned [`WalkBuilder::build`](ignore::WalkBuilder::build) site.
///
/// An `ignore::Walk` runs to completion regardless of cancellation, so
/// the callers below poll [`ral_core::process::check`] at the top of each
/// iteration over the returned walk — surfacing a tool-timeout or Esc
/// cancel as a status-130 `Break` before the next filesystem entry is
/// processed.  Routing every walk through here keeps that contract in one
/// place, backed by the `WalkBuilder::build` clippy ban.
#[allow(clippy::disallowed_methods)]
fn cancellable(builder: &WalkBuilder) -> ignore::Walk {
    builder.build()
}

fn builtin_search_files(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "_search-files")?;
    let pattern = args[0].to_string();
    // The only caller is the `grep-files` prelude helper, so a bad pattern
    // is the agent's `grep-files` call: name that, not the hidden atom.
    let matcher = RegexMatcherBuilder::new()
        .build(&pattern)
        .map_err(|e| sig(regex_err("grep-files", &pattern, &e.to_string())))?;
    let root = checked_read_path(shell, ".")?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();
    let mut results = Vec::new();

    for raw in cancellable(WalkBuilder::new(&root).git_global(false)) {
        ral_core::process::check(shell)?;
        let entry = match raw {
            Ok(e) if e.file_type().is_some_and(|ft| ft.is_file()) => e,
            _ => continue,
        };
        let abs = entry.path();
        let rel = abs
            .strip_prefix(&root)
            .unwrap_or(abs)
            .to_string_lossy()
            .into_owned();
        // Honour the grant's deny_paths, but skip a denied file rather than
        // aborting the whole search, so one off-limits path doesn't blank
        // the results — the same policy `explore-dir` applies to its hits.
        let rp = shell.resolve(&rel);
        if shell.check_fs_read(&rp).is_err() {
            continue;
        }
        let file = match fs::File::open(abs) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if searcher
            .search_reader(
                &matcher,
                file,
                Lossy(|line_num, line| {
                    let text = line.trim_end_matches(['\r', '\n']).to_string();
                    results.push(Value::map(vec![
                        ("file".into(), Value::String(rel.clone())),
                        ("line".into(), Value::Int(line_num as i64)),
                        ("text".into(), Value::String(text)),
                    ]));
                    Ok(true)
                }),
            )
            .is_err()
        {
            continue;
        }
    }
    Ok(Value::list(results))
}

fn builtin_explore_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "explore-dir")?;
    let depth: usize = match &args[0] {
        Value::Int(n) if *n >= 0 => *n as usize,
        Value::Int(n) => {
            return Err(sig(format!(
                "explore-dir: depth must be non-negative, got {n}"
            )));
        }
        other => {
            return Err(sig(format!(
                "explore-dir: expected a non-negative Int for depth, got {}",
                other.type_name()
            )));
        }
    };
    let root = checked_read_path(shell, ".")?;
    let walker = cancellable(
        WalkBuilder::new(&root)
            .max_depth(Some(depth))
            .git_global(false),
    );
    let mut results = Vec::new();

    for result in walker {
        ral_core::process::check(shell)?;
        match result {
            Ok(entry) => {
                if entry.depth() == 0 {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .into_owned();
                // Honour the grant's deny_paths, skipping a denied entry
                // rather than aborting the whole walk, so one off-limits path
                // doesn't blank the listing — the same policy `_search-files`
                // applies to its hits.
                let rp = shell.resolve(&rel);
                if shell.check_fs_read(&rp).is_err() {
                    continue;
                }
                results.push(Value::String(rel));
            }
            Err(e) => {
                let _ = writeln!(shell.stderr_mut(), "explore-dir: {e}");
            }
        }
    }
    Ok(Value::list(results))
}

fn checked_read_path(shell: &mut Shell, path: &str) -> Settled<std::path::PathBuf> {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp)?;
    Ok(rp.into_inner())
}

fn scheme_search_files(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(
            Ty::String,
            pure(Ty::List(Box::new(closed_record(&[
                ("file", Ty::String),
                ("line", Ty::Int),
                ("text", Ty::String),
            ])))),
        )),
    )
}

fn scheme_explore_dir(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::Int, pure(Ty::List(Box::new(Ty::String))))),
    )
}

fn scheme_line_hash(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::String))))
}

pub static EXARCH_BUILTINS: &[BuiltinEntry] = &[
    BuiltinEntry {
        name: Cow::Borrowed("line-hash"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_line_hash),
        doc: "line-hash <s>  — content hash of a line (an `h` tag plus six hex, trailing whitespace ignored); the witness `view` shows and `edit` checks.",
        body: BuiltinBody::Static(builtin_line_hash),
    },
    BuiltinEntry {
        // `_`-prefixed: the engine behind the `grep-files` prelude helper,
        // hidden from `help`.  The agent reaches it only through `grep-files`,
        // which adds the witness the witness-less hits here lack.
        name: Cow::Borrowed("_search-files"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_search_files),
        doc: "_search-files <pattern>  — recursively search cwd (ignore-aware, Rust regex), return [{file, line, text}]; the `grep-files` helper composes over it and stamps each hit with its witness.",
        body: BuiltinBody::Static(builtin_search_files),
    },
    BuiltinEntry {
        name: Cow::Borrowed("explore-dir"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_explore_dir),
        doc: "explore-dir <n>  — list directory entries up to depth n respecting ignore files.",
        body: BuiltinBody::Static(builtin_explore_dir),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn status(b: Break) -> i32 {
        match b {
            Break::Error(e) => e.exit_code(),
            other => panic!("expected Break::Error, got {other:?}"),
        }
    }

    #[test]
    fn line_hash_ignores_trailing_whitespace() {
        assert_eq!(line_hash("x"), line_hash("x   "));
    }

    /// A pre-cancelled scope aborts the search walk at its first poll,
    /// before any filesystem entry is processed, surfacing status 130.
    #[test]
    fn search_files_honours_a_cancelled_scope() {
        let mut shell = Shell::new(Default::default());
        shell
            .foreground()
            .cancel(ral_core::process::CancelCause::Interrupt);
        let err = builtin_search_files(&[Value::String("x".into())], &mut shell)
            .expect_err("a cancelled scope must abort the search walk");
        assert_eq!(status(err), 130);
    }

    /// `explore-dir` likewise aborts at its first poll under a cancelled
    /// scope, surfacing status 130 before listing any entry.
    #[test]
    fn explore_dir_honours_a_cancelled_scope() {
        let mut shell = Shell::new(Default::default());
        shell
            .foreground()
            .cancel(ral_core::process::CancelCause::Interrupt);
        let err = builtin_explore_dir(&[Value::Int(3)], &mut shell)
            .expect_err("a cancelled scope must abort the directory walk");
        assert_eq!(status(err), 130);
    }
}
