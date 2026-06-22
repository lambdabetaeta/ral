//! Exarch-owned agent search and edit primitives.
//!
//! These are static host builtins: process-owned Rust atoms registered
//! with `ral-core` before the shell compiles user/model source.  Dynamic
//! plugins remain source/alias/hook loaders; this module only publishes
//! the resident agent surface that core should not own.

use crate::bus::{Hunk, Row};
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
        ("view-text", "view-text START END < PATH  — show the half-open line range [START, END), each line tagged `<line-no>\\t<hash>\\t<text>`; the hash is the ±3-context witness `edit` checks."),
        ("view-text-around", "view-text-around LINE PEEK < PATH  — show the 2*PEEK+1 lines centred on LINE, tagged like `view-text`, clamped at the top of the file."),
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
/// the prelude (`view-text`), but the Blake3 digest cannot.
fn builtin_line_hash(args: &[Value], _shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "line-hash")?;
    Ok(Value::String(line_hash(&args[0].to_string())))
}

/// The witness for line `i` (0-indexed) of the line list `rows`: the
/// [`line_hash`] of the ±3 surrounding lines' own `line_hash`es, joined and
/// prefixed with the target's offset within the window.  Folding the context
/// into the hash gives two identical lines distinct witnesses whenever their
/// neighbourhoods differ, so a repeated header, blank, or brace is addressable
/// without a line number; the offset keeps lines distinct in a file short
/// enough that the window saturates to the whole of it.  The window clamps at
/// the ends of the file.
///
/// Shared by `window-hash`, `grep-files`, and `edit`: a search result feeds
/// straight into an `edit` because both stamp the same line against the same
/// row list.
fn window_hash(rows: &[String], i: usize) -> String {
    let n = rows.len();
    let lo = i.saturating_sub(3);
    let hi = (i + 4).min(n);
    let body: String = rows[lo..hi].iter().map(|line| line_hash(line)).collect();
    line_hash(&format!("{}:{}", i - lo, body))
}

/// Split a file body into rows that rejoin faithfully: a raw `\n` split keeps
/// the trailing empty a terminal newline produces, so joining with `\n`
/// reproduces the body exactly.  The byte-faithful split (as opposed to the
/// edge-trimming `lines`) is what lets a file's trailing newline survive an
/// edit and the window-hashes be computed over its actual line structure —
/// the Rust twin of `agent.ral`'s `_rows`.
fn rows_of(body: &str) -> Vec<String> {
    body.split('\n').map(str::to_string).collect()
}

/// Expose [`window_hash`] to ral: `window-hash ROWS I`.  `view-text` (still ral)
/// stamps each line it shows through this, so a read hands back the witness
/// `edit` checks.
fn builtin_window_hash(args: &[Value], _shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "window-hash")?;
    let rows: Vec<String> = match &args[0] {
        Value::List(items) => items.iter().map(|v| v.to_string()).collect(),
        other => {
            return Err(sig(format!(
                "window-hash: expected a List of lines, got {}",
                other.type_name()
            )));
        }
    };
    let i = match args[1].as_int() {
        Some(n) if n >= 0 => n as usize,
        _ => {
            return Err(sig(format!(
                "window-hash: expected a non-negative Int index, got {}",
                args[1].type_name()
            )));
        }
    };
    Ok(Value::String(window_hash(&rows, i)))
}

/// The one sanctioned [`WalkBuilder::build`](ignore::WalkBuilder::build) site.
///
/// An `ignore::Walk` runs to completion regardless of cancellation, so
/// the callers below poll [`ral_core::process::check`] at the top of each
/// iteration over the returned walk — surfacing a tool-timeout or Esc
/// cancel as a status-130 `Break` before the next filesystem entry is
/// processed.  Routing every walk through here keeps that contract in one
/// place, backed by the `WalkBuilder::build` clippy ban.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:grep-walk] The one sanctioned WalkBuilder::build site, rooting the grep door's directory walk; the search emits one `grep` surface for the whole walk and polls check() per entry for cancel."
)]
fn cancellable(builder: &WalkBuilder) -> ignore::Walk {
    builder.build()
}

/// One matching line found by [`search_tree`]: the file's tree-relative path,
/// the 1-based line number, and the matched text (newline-trimmed).
struct SearchHit {
    file: String,
    line: u64,
    text: String,
}

/// Recursively search the cwd for `pattern` (ignore-aware, Rust regex),
/// reading each matched file's bytes exactly once.  For every file the walk
/// visits with at least one match, `per_file` receives the tree-relative path,
/// the file's full bytes, and its hits in line order — so a caller may both
/// report the matches and stamp them against the file's own line structure
/// without a second read.
///
/// Cancellation polling (`ral_core::process::check`), the per-file deny-path
/// skip, and the `\x00` binary-detection quit all live here, the one search
/// site `grep-files` composes over.  `scope` returns the cwd the walk roots at.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:grep-read] The grep door's per-matched-file read, in Rust below the ral line so it never reaches the redirect frame; the logical search emits exactly one `grep` surface (scope + pattern), not one read card per file."
)]
fn search_tree(
    shell: &mut Shell,
    pattern: &str,
    mut per_file: impl FnMut(&str, &[u8], Vec<SearchHit>),
) -> Settled<()> {
    let matcher = RegexMatcherBuilder::new()
        .build(pattern)
        .map_err(|e| sig(regex_err("grep-files", pattern, &e.to_string())))?;
    let root = checked_read_path(shell, ".")?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();

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
        // One read per file: the search runs over these same bytes, and the
        // caller splits them into rows to witness each hit — never a second
        // strict re-read.
        let bytes = match fs::read(abs) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut hits = Vec::new();
        if searcher
            .search_slice(
                &matcher,
                &bytes,
                Lossy(|line_num, line| {
                    hits.push(SearchHit {
                        file: rel.clone(),
                        line: line_num,
                        text: line.trim_end_matches(['\r', '\n']).to_string(),
                    });
                    Ok(true)
                }),
            )
            .is_err()
        {
            continue;
        }
        if !hits.is_empty() {
            per_file(&rel, &bytes, hits);
        }
    }
    Ok(())
}

/// `grep-files PATTERN` — search the cwd and stamp each hit with the witness
/// `edit` checks, in one read per matched file (see [`search_tree`]).  Returns
/// `[{file, line, text, hash}]`; emits exactly one `grep` surface naming the
/// scope and pattern.
fn builtin_grep_files(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "grep-files")?;
    let pattern = args[0].to_string();

    // One logical search, one surface — the scope is the cwd the walk roots at.
    shell.surface(Value::map(vec![
        ("io".into(), Value::String("grep".into())),
        ("scope".into(), Value::String(".".into())),
        ("pattern".into(), Value::String(pattern.clone())),
    ]));

    let mut results = Vec::new();
    search_tree(shell, &pattern, |_rel, bytes, hits| {
        // The scan decodes lossily, so it can match a file that is not valid
        // UTF-8; such a file cannot be split into witnessable rows and `edit`
        // can never touch it, so rather than failing the whole search we flag
        // its hits with an empty-string hash — a value no `window-hash` ever
        // produces, so it resolves to no line and is unmistakably "no witness".
        let rows = std::str::from_utf8(bytes).ok().map(rows_of);
        for hit in hits {
            let hash = match &rows {
                Some(rows) => window_hash(rows, (hit.line - 1) as usize),
                None => String::new(),
            };
            results.push(Value::map(vec![
                ("file".into(), Value::String(hit.file)),
                ("line".into(), Value::Int(hit.line as i64)),
                ("text".into(), Value::String(hit.text)),
                ("hash".into(), Value::String(hash)),
            ]));
        }
    })?;
    Ok(Value::list(results))
}

/// One resolved edit: the 0-based index of the line its hash uniquely named
/// against the file as read, and the replacement text taken verbatim.
struct ResolvedEdit {
    at: usize,
    new: String,
}

/// `edit PATH EDITS` — apply a batch of `[hash, new-text]` pairs in one
/// read/rebuild/write pass, then surface one whole-file diff card.  All of it
/// runs in Rust — the read is not a redirect and the write is atomic — so `edit`
/// is a single logical surface emitting only its diff card, never a read or
/// write io card.
///
/// Every hash resolves against the file as read, before anything is written, so
/// the edits never interfere (adjacent lines included) and the batch is atomic:
/// it fails, writing nothing, unless every hash picks exactly one line (zero
/// means the file moved, more than one means the line and its ±3 context both
/// repeat) and no two pairs name the same line.  An empty `new-text` deletes the
/// line; a real newline inside it splits the line into several.
fn builtin_edit(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "edit")?;
    let path = args[0].to_string();
    let edits = match &args[1] {
        Value::List(items) => items,
        other => {
            return Err(sig(format!(
                "edit: expected a List of [hash, new-text] pairs, got {}",
                other.type_name()
            )));
        }
    };
    if edits.is_empty() {
        return Err(sig(
            "edit: no edits given — pass a list of [hash, new-text] pairs.".to_string(),
        ));
    }

    let body = read_file_string(shell, &path)?;
    let rows = rows_of(&body);
    let n = rows.len();
    let hashes: Vec<String> = (0..n).map(|i| window_hash(&rows, i)).collect();

    // Resolve each pair to the unique index of the line its hash names, against
    // the original snapshot.  A stale or repeated hash fails here, before the
    // write — the failure messages are user-facing and pinned by tests.
    let mut resolved = Vec::with_capacity(edits.len());
    for e in edits.iter() {
        let (want, new) = match e {
            Value::List(pair) if pair.len() >= 2 => (
                pair.get(0).unwrap().to_string(),
                pair.get(1).unwrap().to_string(),
            ),
            other => {
                return Err(sig(format!(
                    "edit: each edit must be a [hash, new-text] pair, got {}",
                    other.type_name()
                )));
            }
        };
        let idxs: Vec<usize> = (0..n).filter(|&i| hashes[i] == want).collect();
        match idxs.len() {
            0 => {
                return Err(sig(format!(
                    "edit: no line in {path} hashes to {want} — did the file change? Re-read with view-text/grep-files before editing."
                )));
            }
            1 => resolved.push(ResolvedEdit { at: idxs[0], new }),
            _ => {
                let at: Vec<String> = idxs.iter().map(|i| (i + 1).to_string()).collect();
                let r#where = at.join(", ");
                return Err(sig(format!(
                    "edit: hash {want} matches lines {} in {path} — the text repeats, so a hash alone cannot choose one.",
                    r#where
                )));
            }
        }
    }
    // Two pairs naming the same line is the analogue of the ral fold's
    // length-`hit` > 1 guard: caught before the write, nothing rebuilt.
    for w in 0..resolved.len() {
        for v in (w + 1)..resolved.len() {
            if resolved[w].at == resolved[v].at {
                return Err(sig(format!(
                    "edit: two edits name line {} in {path}.",
                    resolved[w].at + 1
                )));
            }
        }
    }

    // Rebuild in one pass over the original rows: an untouched row passes
    // through, a named row becomes its replacement (a real newline splits it
    // into several; empty drops the line).
    let mut out: Vec<String> = Vec::with_capacity(n);
    for (i, row) in rows.iter().enumerate() {
        match resolved.iter().find(|r| r.at == i) {
            None => out.push(row.clone()),
            Some(r) if r.new.is_empty() => {}
            Some(r) => out.extend(rows_of(&r.new)),
        }
    }
    let final_text = out.join("\n");
    write_file_atomic(shell, &path, final_text.as_bytes())?;

    // One canonical whole-file diff (original vs final), grouped into hunks by
    // `similar` with ±2 lines of context.  A no-op edit yields no hunks and so
    // surfaces nothing; otherwise the rail draws a single card for the file.
    let hunks = whole_file_hunks(&body, &final_text);
    if !hunks.is_empty() {
        shell.surface(diff_card_value(&path, hunks));
    }
    Ok(Value::Unit)
}

/// Compute the whole-file line-level diff of `old` vs `new`, grouped into
/// hunks with ±2 lines of context (matching the kit's former `peek`).  Each
/// hunk's `start` is the 1-indexed original line of its first row, and its
/// rows are the unified context / deletion / insertion list `similar` yields.
fn whole_file_hunks(old: &str, new: &str) -> Vec<Hunk> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(2) {
        let first = group.first().expect("grouped_ops yields non-empty groups");
        let start = first.old_range().start as u32 + 1;
        let mut rows = Vec::new();
        for op in &group {
            for change in diff.iter_changes(op) {
                // `from_lines` keeps a trailing `\n` on each change value;
                // strip exactly one so the row carries the bare line, the way
                // `rows_of` splits the file.
                let text = change
                    .value()
                    .strip_suffix('\n')
                    .unwrap_or(change.value())
                    .to_string();
                rows.push(match change.tag() {
                    ChangeTag::Equal => Row::Context(text),
                    ChangeTag::Delete => Row::Del(text),
                    ChangeTag::Insert => Row::Add(text),
                });
            }
        }
        hunks.push(Hunk { start, rows });
    }
    hunks
}

/// Build the `` `card [`diff …] `` value `edit` surfaces for the whole-file
/// diff: the `path` and the grouped `hunks`, each hunk a `start` line and a
/// `rows` list of `{ tag, text }` records the card decoder lifts back into
/// [`Row`]s.
fn diff_card_value(path: &str, hunks: Vec<Hunk>) -> Value {
    let hunk_values: Vec<Value> = hunks
        .into_iter()
        .map(|h| {
            let rows: Vec<Value> = h
                .rows
                .into_iter()
                .map(|row| {
                    let (tag, text) = match row {
                        Row::Context(t) => ("context", t),
                        Row::Del(t) => ("del", t),
                        Row::Add(t) => ("add", t),
                    };
                    Value::map(vec![
                        ("tag".into(), Value::String(tag.into())),
                        ("text".into(), Value::String(text)),
                    ])
                })
                .collect();
            Value::map(vec![
                ("start".into(), Value::Int(h.start as i64)),
                ("rows".into(), Value::list(rows)),
            ])
        })
        .collect();
    let diff = Value::Variant {
        label: "diff".into(),
        payload: Some(Box::new(Value::map(vec![
            ("path".into(), Value::String(path.to_string())),
            ("hunks".into(), Value::list(hunk_values)),
        ]))),
    };
    Value::Variant {
        label: "card".into(),
        payload: Some(Box::new(Value::list(vec![diff]))),
    }
}

/// Read a file as a UTF-8 string for witnessed editing, gating the read through
/// the active grant the way a `< path` redirect would.  This is `edit`'s read
/// door: in Rust, so it never reaches the redirect frame and so raises no read
/// io card.  A non-UTF-8 file is named (the witness layer cannot address it).
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:edit-read] `edit`'s read door, in Rust below the ral line so it never reaches the redirect frame: edit is one logical surface that emits only its diff cards, never a separate read card. The grant is still checked, as a `< path` redirect would."
)]
fn read_file_string(shell: &mut Shell, path: &str) -> Settled<String> {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp)?;
    let bytes =
        fs::read(rp.as_path()).map_err(|e| sig(format!("edit: cannot read {path}: {e}")))?;
    String::from_utf8(bytes).map_err(|_| {
        sig(format!(
            "edit: '{path}' is not valid UTF-8, so its lines cannot be witnessed for editing."
        ))
    })
}

/// Write `bytes` to `path` atomically — a temp file in the target's directory,
/// flushed and renamed into place — gating the write through the active grant
/// the way a `> path` redirect would.  This is `edit`'s write door: in Rust, so
/// it raises no write io card.  The rename is atomic on the same filesystem, so
/// a reader never sees a half-written file and a failed write leaves the
/// original untouched.
fn write_file_atomic(shell: &mut Shell, path: &str, bytes: &[u8]) -> Settled<()> {
    let rp = shell.resolve(path);
    shell.check_fs_write(&rp)?;
    let target = rp.into_inner();
    // `resolve` returns a cwd-anchored, collapsed path, so a regular file
    // always has a parent directory to stage the temp file in; the only
    // parent-less path is the filesystem root, which is not an editable file.
    let parent = target.parent().ok_or_else(|| {
        sig(format!(
            "edit: {path} has no parent directory to write into"
        ))
    })?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".ral-edit-")
        .tempfile_in(parent)
        .map_err(|e| sig(format!("edit: cannot stage write to {path}: {e}")))?;
    tmp.write_all(bytes)
        .map_err(|e| sig(format!("edit: cannot write {path}: {e}")))?;
    tmp.flush()
        .map_err(|e| sig(format!("edit: cannot write {path}: {e}")))?;
    tmp.persist(&target)
        .map_err(|e| sig(format!("edit: cannot commit write to {path}: {e}")))?;
    Ok(())
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

fn scheme_grep_files(_u: &mut Unifier) -> Scheme {
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
                ("hash", Ty::String),
            ])))),
        )),
    )
}

/// `window-hash :: [Str] → Int → F Str` — the witness for a line of a row list.
fn scheme_window_hash(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(
            Ty::List(Box::new(Ty::String)),
            fun(Ty::Int, pure(Ty::String)),
        )),
    )
}

/// `edit :: Str → [[Str]] → F Unit` — `path` then a list of `[hash, new-text]`
/// pairs.  Returns Unit: `edit` writes and surfaces, it does not yield a value.
fn scheme_edit(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(
            Ty::String,
            fun(
                Ty::List(Box::new(Ty::List(Box::new(Ty::String)))),
                pure(Ty::Unit),
            ),
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
        doc: "line-hash <s>  — content hash of a line (an `h` tag plus six hex, trailing whitespace ignored); the witness `view-text` shows and `edit` checks.",
        body: BuiltinBody::Static(builtin_line_hash),
    },
    BuiltinEntry {
        name: Cow::Borrowed("window-hash"),
        type_rule: BuiltinTypeRule::Scheme(Some(2), scheme_window_hash),
        doc: "window-hash <rows> <i>  — the witness for line i (0-indexed) of the line list ROWS: the line-hash of the ±3 surrounding lines' line-hashes, prefixed with the target's offset. What `view-text` shows and `edit` checks; folding in context distinguishes repeated lines without a position.",
        body: BuiltinBody::Static(builtin_window_hash),
    },
    BuiltinEntry {
        name: Cow::Borrowed("grep-files"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_grep_files),
        doc: "grep-files <pattern>  — recursively search the cwd (ignore-aware, Rust regex) in one read per matched file and stamp each hit with its window-hash, giving [{file, line, text, hash}] that feeds straight into `edit`.",
        body: BuiltinBody::Static(builtin_grep_files),
    },
    BuiltinEntry {
        name: Cow::Borrowed("edit"),
        type_rule: BuiltinTypeRule::Scheme(Some(2), scheme_edit),
        doc: "edit <path> <edits>  — apply a batch of [hash, new-text] pairs in one read/write pass: each replaces the line whose window-hash is HASH (NEW-TEXT is verbatim — a real newline inside '…' splits the line, \\n does not; empty deletes). Atomic — all hashes resolve against the file as read, so edits never interfere; fails writing nothing unless every hash picks exactly one line and no two pairs name the same one. Surfaces one whole-file diff card.",
        body: BuiltinBody::Static(builtin_edit),
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

    /// The Rust `window_hash` reproduces the retired ral algorithm exactly.
    /// The expected value is a hand-port of `agent.ral`'s `window-hash`: the
    /// `line-hash` of `"<offset>:" ++ concat(line-hash of each ±3 window row)`.
    #[test]
    fn window_hash_matches_the_retired_ral_algorithm() {
        let rows: Vec<String> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for i in 0..rows.len() {
            let lo = i.saturating_sub(3);
            let hi = (i + 4).min(rows.len());
            let body: String = rows[lo..hi].iter().map(|l| line_hash(l)).collect();
            let expected = line_hash(&format!("{}:{}", i - lo, body));
            assert_eq!(window_hash(&rows, i), expected, "row {i}");
        }
    }

    /// The window folds in ±3 lines of context, so two lines with identical
    /// text but different neighbourhoods get distinct witnesses — what a bare
    /// line hash could not do.  Both `target` rows hash the same with
    /// `line_hash`, yet differ under `window_hash`.
    #[test]
    fn window_hash_distinguishes_repeated_lines_by_context() {
        let rows: Vec<String> = [
            "section one:",
            "target",
            "    delete me",
            "",
            "section two:",
            "target",
            "    keep me",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            line_hash(&rows[1]),
            line_hash(&rows[5]),
            "same line content"
        );
        assert_ne!(
            window_hash(&rows, 1),
            window_hash(&rows, 5),
            "distinct neighbourhoods must witness distinctly"
        );
    }

    /// A pre-cancelled scope aborts the search walk at its first poll,
    /// before any filesystem entry is processed, surfacing status 130.  The
    /// `grep-files` builtin now owns that walk (`search_tree`).
    #[test]
    fn search_files_honours_a_cancelled_scope() {
        let mut shell = Shell::new(Default::default());
        shell
            .foreground()
            .cancel(ral_core::process::CancelCause::Interrupt);
        let err = builtin_grep_files(&[Value::String("x".into())], &mut shell)
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
