//! Exarch-owned agent search and edit primitives.
//!
//! These are static host builtins: process-owned Rust atoms registered
//! with `ral-core` before the shell compiles user/model source.  Dynamic
//! plugins remain source/alias/hook loaders; this module only publishes
//! the resident agent surface that core should not own.

use crate::bus::{Hunk, Row, Seg};
use crate::skill;
use fff_search::file_picker::FilePicker;
use fff_search::{
    FFFMode, FilePickerOptions, FrecencyTracker, FuzzySearchOptions, PaginationArgs, QueryParser,
    QueryTracker, SharedFilePicker, SharedFrecency, SharedQueryTracker,
};
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
use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

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
        ("view-text-around", "view-text-around PATH LINE PEEK  — show the 2*PEEK+1 lines of PATH centred on LINE, tagged like `view-text`, clamped at the top of the file."),
        ("empty-tasks", "empty-tasks  — an empty task list; canonical initialiser"),
        ("add-task", "add-task $exarch-tasks <desc>  — allocate fresh id, append task, update pinned gauge"),
        ("remove-task", "remove-task $exarch-tasks <id>  — drop task by id, update pinned gauge"),
        ("tag-task", "tag-task $exarch-tasks <id> <tag>  — add a tag to a task"),
        ("untag-task", "untag-task $exarch-tasks <id> <tag>  — remove a tag from a task"),
        ("note-task", "note-task $exarch-tasks <id> <note>  — set notes on a task"),
        ("retag-task", "retag-task $exarch-tasks <id> <tags>  — replace all tags on a task"),
        ("transition", "transition $exarch-tasks <id> <status>  — change status (validated: `open|`doing|`blocked|`done) + update pinned gauge"),
        ("status-counts", "status-counts $exarch-tasks  — record of per-status counts (single fold)"),
        ("render-tasks", "render-tasks $exarch-tasks  — echo each task to stdout"),
        ("save-tasks", "save-tasks $exarch-tasks <path>  — write task list as JSON"),
        ("load-tasks", "load-tasks <path>  — read task list from JSON"),
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
///
/// Private: the witness is never something the model constructs, only one it
/// copies out of a `view-text` read, so neither this nor the
/// window hash is exposed to ral — `view-text`, `view-text-around`,
/// and `edit` are the whole surface.
fn line_hash(line: &str) -> String {
    let stripped = line.trim_end();
    let hex = blake3::hash(stripped.as_bytes()).to_hex();
    format!("h{}", &hex[..6])
}

/// The freshness floor: every witness folds in at least ±`MIN_RADIUS` lines of
/// context, even a line unique on its own, so an edit anywhere within that
/// window invalidates the witness and forces a re-read.  The adaptive-context
/// search starts here and only grows.
const MIN_RADIUS: usize = 5;

/// The cap on how far a line's window grows before it falls back to its
/// absolute index.  A line is addressed by the smallest symmetric window (at
/// least ±[`MIN_RADIUS`]) that makes it unique (see [`window_hashes`]); only a
/// run of identical lines longer than `2 * MAX_RADIUS` exhausts this, and the
/// residual is then named by index — the honest positional floor for content
/// that genuinely repeats.
const MAX_RADIUS: usize = 64;

/// How a line was distinguished from every other: by a window of some radius,
/// or — only inside a long verbatim run — by its absolute index.
enum Witness {
    Window(usize),
    Index,
}

/// The witness for *every* line of `rows`, addressing each by the smallest
/// context that makes it unique.  A line's witness is the [`line_hash`] of the
/// neighbours in the smallest symmetric window that no other line shares —
/// prefixed by that radius and the target's offset within the (clamped) window.
/// The window starts at ±[`MIN_RADIUS`] (the freshness floor: every witness
/// folds in that much context, so an edit nearby invalidates it) and grows only
/// as far as a repetition demands.  The witness carries no line number, so it
/// goes stale on a *local* change, not on every insertion elsewhere; the lone
/// exception is a verbatim run longer than `2 * MAX_RADIUS`, whose interior is
/// named by index.
///
/// Computed by partition refinement, the shape of DFA minimisation: group the
/// lines by their ±[`MIN_RADIUS`] window, then repeatedly split only the still-
/// colliding classes by one more line of context on each side.  A line that
/// becomes a singleton is resolved and never re-examined, so the work is bounded
/// by the total size of collision classes across radii — linear on real files,
/// where almost every line is unique at the floor.
///
/// Shared verbatim by `view-text` and `edit`, so a read and the
/// edit that follows it derive identical witnesses from identical content.
fn window_hashes(rows: &[String]) -> Vec<String> {
    let n = rows.len();
    if n == 0 {
        return Vec::new();
    }
    let lh: Vec<String> = rows.iter().map(|line| line_hash(line)).collect();

    // The signature `edit` and `view-text` agree on: two lines share a radius-`r`
    // witness exactly when their signatures here are equal — the target's offset
    // within its clamped window, then that window's line-hashes in order.
    let signature = |i: usize, r: usize| -> String {
        let lo = i.saturating_sub(r);
        let hi = (i + r + 1).min(n);
        let mut s = format!("{}:", i - lo);
        for h in &lh[lo..hi] {
            s.push_str(h);
        }
        s
    };

    // Group a set of line indices by a key; the partition's building block.
    let group = |members: &[usize], r: usize| -> Vec<Vec<usize>> {
        let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
        for &i in members {
            by_key.entry(signature(i, r)).or_default().push(i);
        }
        by_key.into_values().collect()
    };

    let mut how: Vec<Witness> = (0..n).map(|_| Witness::Index).collect();
    let all: Vec<usize> = (0..n).collect();
    // Start at the freshness floor: lines unique within ±MIN_RADIUS resolve
    // there; only collisions grow past it.
    let mut classes = group(&all, MIN_RADIUS);
    let mut r = MIN_RADIUS;
    while !classes.is_empty() {
        let mut next: Vec<Vec<usize>> = Vec::new();
        for class in classes {
            if class.len() == 1 {
                how[class[0]] = Witness::Window(r);
            } else if r >= MAX_RADIUS {
                // A verbatim run deeper than the cap: name each by index.
                for i in class {
                    how[i] = Witness::Index;
                }
            } else {
                next.extend(group(&class, r + 1));
            }
        }
        classes = next;
        r += 1;
    }

    (0..n)
        .map(|i| match how[i] {
            // Fold the radius in too, so witnesses from different radii cannot
            // collide just because their windows happen to coincide.
            Witness::Window(r) => {
                let lo = i.saturating_sub(r);
                let hi = (i + r + 1).min(n);
                let mut body = format!("{}:{}:", r, i - lo);
                for h in &lh[lo..hi] {
                    body.push_str(h);
                }
                line_hash(&body)
            }
            Witness::Index => line_hash(&format!("idx:{i}")),
        })
        .collect()
}

/// Split a file body into rows that rejoin faithfully: a raw `\n` split keeps
/// the trailing empty a terminal newline produces, so joining with `\n`
/// reproduces the body exactly.  The byte-faithful split (as opposed to the
/// edge-trimming `lines`) is what lets a file's trailing newline survive an
/// edit and the window hashes be computed over its actual line structure.
fn rows_of(body: &str) -> Vec<String> {
    body.split('\n').map(str::to_string).collect()
}

/// Surface the one `{io:"read", path}` card for a whole-file read.  `view-text`
/// and `witnesses` read in Rust below the ral line (no `< path` redirect), so
/// they raise their own read card — one logical surface per read, matching the
/// shape the redirect frame would have pushed.  `edit` is the exception: it
/// reads silently and speaks only its diff.
fn surface_read(shell: &mut Shell, path: &str) {
    shell.surface(Value::map(vec![
        ("io".into(), Value::String("read".into())),
        ("path".into(), Value::String(path.to_string())),
    ]));
}

/// Parse a 1-or-greater bound argument for `view-text`.
fn view_bound(arg: &Value, which: &str) -> Settled<usize> {
    match arg.as_int() {
        Some(n) if n >= 1 => Ok(n as usize),
        _ => Err(sig(format!(
            "view-text: {which} must be an Int >= 1 (range is half-open: end > start), got {}",
            arg.type_name()
        ))),
    }
}

/// `view-text PATH START END` — show the half-open line range `[START, END)` of
/// the file, each line tagged `<line-no>\t<hash>\t<text>`.  Reads the whole file
/// (its witnesses depend on file-wide uniqueness), hashes it, and writes the
/// requested slice; surfaces one read card.
fn builtin_view_text(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 3, "view-text")?;
    let path = args[0].to_string();
    let start = view_bound(&args[1], "start")?;
    let end = view_bound(&args[2], "end")?;

    let body = read_text_file(shell, &path, "view-text")?;
    surface_read(shell, &path);
    let rows = rows_of(&body);
    let hashes = window_hashes(&rows);
    let n = rows.len();
    let lo = start - 1;
    let hi = (end - 1).min(n);

    let mut result_rows = Vec::new();
    if lo < hi {
        for i in lo..hi {
            result_rows.push(Value::map(vec![
                ("line".into(), Value::Int(i as i64 + 1)),
                ("hash".into(), Value::String(hashes[i].clone())),
                ("text".into(), Value::String(rows[i].clone())),
            ]));
        }
    }
    Ok(Value::list(result_rows))
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
/// reading each matched file's bytes exactly once and collecting every matching
/// line — its tree-relative path, 1-based line number, and matched text — in
/// walk order.
///
/// Cancellation polling (`ral_core::process::check`), the per-file deny-path
/// skip, and the `\x00` binary-detection quit all live here, the one search
/// site `grep-files` composes over.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:grep-read] The grep door's per-matched-file read, in Rust below the ral line so it never reaches the redirect frame; the logical search emits exactly one `grep` surface (scope + pattern), not one read card per file."
)]
fn search_tree(shell: &mut Shell, pattern: &str) -> Settled<Vec<SearchHit>> {
    let matcher = RegexMatcherBuilder::new()
        .build(pattern)
        .map_err(|e| sig(regex_err("grep-files", pattern, &e.to_string())))?;
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
        // One read per file: the search runs over these bytes directly.
        let bytes = match fs::read(abs) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if searcher
            .search_slice(
                &matcher,
                &bytes,
                Lossy(|line_num, line| {
                    results.push(SearchHit {
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
    }
    Ok(results)
}

/// `grep-files PATTERN` — search the cwd in one read per matched file (see
/// [`search_tree`]).  Returns `[{file, line, text}]`; emits exactly one `grep`
/// surface naming the scope and pattern.
fn builtin_grep_files(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "grep-files")?;
    let pattern = args[0].to_string();

    // One logical search, one surface — the scope is the cwd the walk roots at.
    shell.surface(Value::map(vec![
        ("io".into(), Value::String("grep".into())),
        ("scope".into(), Value::String(".".into())),
        ("pattern".into(), Value::String(pattern.clone())),
    ]));

    let results = search_tree(shell, &pattern)?
        .into_iter()
        .map(|hit| {
            Value::map(vec![
                ("file".into(), Value::String(hit.file)),
                ("line".into(), Value::Int(hit.line as i64)),
                ("text".into(), Value::String(hit.text)),
            ])
        })
        .collect();
    Ok(Value::list(results))
}

/// One resolved edit: the 0-based index of the line its hash uniquely named
/// against the file as read, and the replacement text taken verbatim.
struct ResolvedEdit {
    at: usize,
    new: String,
}

/// `edit PATH EDITS` — apply a batch of `[hash: …, line: …]` records in one
/// read/rebuild/write pass, then surface one whole-file diff card.  All of it
/// runs in Rust — the read is not a redirect and the write is atomic — so `edit`
/// is a single logical surface emitting only its diff card, never a read or
/// write io card.
///
/// Each `hash` resolves against the file as read, before anything is written, so
/// the edits never interfere (adjacent lines included) and the batch is atomic:
/// it fails, writing nothing, unless every hash picks exactly one line (a stale
/// or now-ambiguous hash means the file moved) and no two records name the same
/// line.  The `line` field is the replacement text, taken verbatim: empty
/// deletes the line; a real newline inside it splits the line into several.
fn builtin_edit(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "edit")?;
    let path = args[0].to_string();
    let edits = match &args[1] {
        Value::List(items) => items,
        other => {
            return Err(sig(format!(
                "edit: expected a List of [hash: …, line: …] records, got {}",
                other.type_name()
            )));
        }
    };
    if edits.is_empty() {
        return Err(sig(
            "edit: no edits given — pass a list of [hash: …, line: …] records.".to_string(),
        ));
    }

    let body = read_text_file(shell, &path, "edit")?;
    let rows = rows_of(&body);
    let n = rows.len();
    let hashes = window_hashes(&rows);

    // Resolve each record to the unique index of the line its hash names,
    // against the original snapshot.  A stale hash fails here, before the write —
    // the failure messages are user-facing and pinned by tests.
    let mut resolved = Vec::with_capacity(edits.len());
    for e in edits.iter() {
        let m = match e {
            Value::Map(m) => m,
            other => {
                return Err(sig(format!(
                    "edit: each edit must be a [hash: …, line: …] record, got {}",
                    other.type_name()
                )));
            }
        };
        let want = match m.get("hash") {
            Some(v) => v.to_string(),
            None => {
                return Err(sig(
                    "edit: each edit needs a `hash` field — the witness from view-text/witnesses."
                        .to_string(),
                ));
            }
        };
        let new = match m.get("line") {
            Some(v) => v.to_string(),
            None => {
                return Err(sig(
                    "edit: each edit needs a `line` field — the replacement text.".to_string(),
                ));
            }
        };
        let idxs: Vec<usize> = (0..n).filter(|&i| hashes[i] == want).collect();
        match idxs.len() {
            0 => {
                return Err(sig(format!(
                    "edit: no line in {path} hashes to {want} — did the file change? Re-read with view-text/view-text-around before editing."
                )));
            }
            1 => resolved.push(ResolvedEdit { at: idxs[0], new }),
            _ => {
                let at: Vec<String> = idxs.iter().map(|i| (i + 1).to_string()).collect();
                let r#where = at.join(", ");
                return Err(sig(format!(
                    "edit: hash {want} matches lines {} in {path} — re-read; the witness has gone stale.",
                    r#where
                )));
            }
        }
    }
    // Two records naming the same line is the analogue of the ral fold's
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
            // The *inline* changes carry, per row, the intra-line word diff
            // `similar` computes against the row's paired line: a run of
            // `(emphasised, text)` segments, where the emphasised runs are the
            // bits that actually differ.  A context row reduces to one
            // unemphasised segment, exactly the old line-level shape.
            for change in diff.iter_inline_changes(op) {
                let mut segs: Vec<Seg> = change
                    .iter_strings_lossy()
                    .map(|(emph, text)| Seg {
                        emph,
                        text: text.into_owned(),
                    })
                    .collect();
                // `from_lines` keeps a trailing `\n` on each row's final
                // segment; strip exactly one so the row carries the bare line,
                // the way `rows_of` splits the file, dropping a segment the
                // strip empties.
                if let Some(last) = segs.last_mut() {
                    if let Some(bare) = last.text.strip_suffix('\n') {
                        last.text = bare.to_string();
                    }
                    if last.text.is_empty() {
                        segs.pop();
                    }
                }
                rows.push(match change.tag() {
                    ChangeTag::Equal => Row::Context(segs),
                    ChangeTag::Delete => Row::Del(segs),
                    ChangeTag::Insert => Row::Add(segs),
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
                    let (tag, segs) = match row {
                        Row::Context(s) => ("context", s),
                        Row::Del(s) => ("del", s),
                        Row::Add(s) => ("add", s),
                    };
                    let seg_values: Vec<Value> = segs
                        .into_iter()
                        .map(|seg| {
                            Value::map(vec![
                                ("emph".into(), Value::Bool(seg.emph)),
                                ("text".into(), Value::String(seg.text)),
                            ])
                        })
                        .collect();
                    Value::map(vec![
                        ("tag".into(), Value::String(tag.into())),
                        ("segs".into(), Value::list(seg_values)),
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

/// Read a file as a UTF-8 string for the witness layer, gating the read through
/// the active grant the way a `< path` redirect would.  The shared read door of
/// `view-text`, `witnesses`, and `edit`: in Rust, below the ral line, so it
/// never reaches the redirect frame.  Each caller decides its own surface —
/// `view-text`/`witnesses` raise one read card, `edit` stays silent and speaks
/// only its diff.  A non-UTF-8 file is named (the witness layer cannot address
/// it); `tool` puts the calling builtin's name on the error.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:witness-read] The witness layer's read door (view-text/witnesses/edit), in Rust below the ral line so it never reaches the redirect frame. view-text and witnesses surface their own read card; edit emits only its diff. The grant is still checked, as a `< path` redirect would."
)]
fn read_text_file(shell: &mut Shell, path: &str, tool: &str) -> Settled<String> {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp)?;
    let bytes =
        fs::read(rp.as_path()).map_err(|e| sig(format!("{tool}: cannot read {path}: {e}")))?;
    String::from_utf8(bytes).map_err(|_| {
        sig(format!(
            "{tool}: '{path}' is not valid UTF-8, so its lines cannot be witnessed."
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
            ])))),
        )),
    )
}

/// `view-text :: Str → Int → Int → F [[line: Int, hash: Str, text: Str]]` — `path`, then the half-open line
/// range. Returns a list of records, one per line in [start, end).
fn scheme_view_text(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(
            Ty::String,
            fun(
                Ty::Int,
                fun(
                    Ty::Int,
                    pure(Ty::List(Box::new(closed_record(&[
                        ("line", Ty::Int),
                        ("hash", Ty::String),
                        ("text", Ty::String),
                    ])))),
                ),
            ),
        )),
    )
}

/// `edit :: Str → [{hash: Str, line: Str}] → F Unit` — `path` then a list of
/// `[hash: …, line: …]` records.  Returns Unit: `edit` writes and surfaces, it
/// does not yield a value.
fn scheme_edit(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(
            Ty::String,
            fun(
                Ty::List(Box::new(closed_record(&[
                    ("hash", Ty::String),
                    ("line", Ty::String),
                ]))),
                pure(Ty::Unit),
            ),
        )),
    )
}
/// How long to wait for the initial filesystem scan before serving
/// (possibly partial) results.  Big trees on slow disks can exceed
/// this; the index keeps populating in the background regardless.
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

const DEFAULT_LIMIT: usize = 50;

/// One indexed tree, kept alive for the process lifetime.  The
/// [`SharedFilePicker`] owns the scan thread and the filesystem watcher;
/// dropping it would tear them down, but we never drop — the registry
/// hands out `&'static` borrows.
struct Index {
    picker: SharedFilePicker,
    queries: SharedQueryTracker,
}

/// Process-global registry: one `Index` per canonical base path.
/// Entries are leaked into `&'static` so the picker outlives any
/// lock guard returned to a caller.
fn registry() -> &'static Mutex<HashMap<PathBuf, &'static Index>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, &'static Index>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn resolve_base(base: &Path) -> ral_core::path::ResolvedPath {
    ral_core::path::Resolver {
        home: String::new(),
        cwd: None,
        mode: ral_core::path::CanonMode::Lenient,
    }
    .resolve(&base.to_string_lossy())
}

/// Get-or-create the index for `base`.  Blocks the caller while the
/// initial scan runs the first time `base` is seen; cheap on every
/// subsequent call.
fn index_for(base: &Path) -> Result<&'static Index, String> {
    let canonical = resolve_base(base)
        .canonicalise_strict()
        .map_err(|e| format!("could not canonicalise {}: {e}", base.display()))?;
    let mut guard = registry().lock().expect("fff registry mutex poisoned");
    if let Some(idx) = guard.get(&canonical) {
        return Ok(idx);
    }
    let idx: &'static Index = Box::leak(Box::new(build_index(&canonical)?));
    guard.insert(canonical, idx);
    Ok(idx)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:fff-db-dir] creates the fff index's temp db dir; cache infra, not turn-time data I/O"
)]
fn build_index(base: &Path) -> Result<Index, String> {
    let db_root = std::env::temp_dir().join(format!(
        "exarch-fff-{}-{:016x}",
        std::process::id(),
        path_hash(base),
    ));
    std::fs::create_dir_all(&db_root).map_err(|e| format!("fff db dir: {e}"))?;

    let frecency = SharedFrecency::default();
    frecency
        .init(FrecencyTracker::open(db_root.join("frecency")).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;

    let queries = SharedQueryTracker::default();
    queries
        .init(QueryTracker::open(db_root.join("queries")).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;

    let picker = SharedFilePicker::default();
    FilePicker::new_with_shared_state(
        picker.clone(),
        frecency,
        FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            mode: FFFMode::Ai,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    picker.wait_for_scan(SCAN_TIMEOUT);
    Ok(Index { picker, queries })
}

fn path_hash(p: &Path) -> u64 {
    let mut h = DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// Run one search against `idx` and return matching paths.
fn search_paths(idx: &Index, query: &str, limit: usize) -> Result<Vec<String>, String> {
    let parser = QueryParser::default();
    let parsed = parser.parse(query);
    let picker_guard = idx
        .picker
        .read()
        .map_err(|e: fff_search::Error| e.to_string())?;
    let picker = picker_guard
        .as_ref()
        .ok_or("fff index handle is empty (scan failed)")?;
    let qt_guard = idx.queries.read().map_err(|e| e.to_string())?;
    let result = picker.fuzzy_search(
        &parsed,
        qt_guard.as_ref(),
        FuzzySearchOptions {
            pagination: PaginationArgs { offset: 0, limit },
            ..Default::default()
        },
    );
    Ok(result
        .items
        .iter()
        .map(|item| item.relative_path(picker))
        .collect())
}

/// `fff QUERY` — fuzzy file-name search (frecency-ranked) over the
/// working tree, returning a list of matching paths.
fn builtin_fff(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "fff")?;
    let query = args[0].to_string();
    let cwd = checked_read_path(shell, ".")?;
    let idx = index_for(&cwd).map_err(sig)?;
    let paths = search_paths(idx, &query, DEFAULT_LIMIT).map_err(sig)?;
    Ok(Value::list(paths.into_iter().map(Value::String).collect()))
}

fn scheme_explore_dir(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::Int, pure(Ty::List(Box::new(Ty::String))))),
    )
}
fn scheme_fff(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::String, pure(Ty::List(Box::new(Ty::String))))),
    )
}

fn scheme_skill(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::String))))
}

/// `skill NAME` — load the full SKILL.md body of a skill (fresh scan at
/// each call — picks up skills added or edited mid-session).
fn builtin_skill(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "skill")?;
    let name = args[0].to_string();
    // A malformed name can never name a discoverable skill, and rejecting it
    // here keeps `root.join(&name)` from escaping the skills root.
    if !skill::valid_skill_name(&name) {
        return Settled::Ok(Value::String(format!("skill not found: {name}")));
    }
    let cwd = shell.cwd();
    let config_dir = crate::bootstrap::xdg_app_dir(ral_core::path::basedir::XdgKind::Config);
    for root in [
        cwd.join(".exarch").join("skills"),
        config_dir.join("skills"),
    ] {
        let dir = root.join(&name);
        let sk_md = dir.join("SKILL.md");
        let rp = shell.resolve(&sk_md.to_string_lossy());
        if shell.check_fs_read(&rp).is_ok() {
            let body = match skill::read_skill_body(&dir) {
                Ok(body) => body,
                Err(_) => {
                    return Settled::Ok(Value::String(format!("could not read skill: {name}")));
                }
            };
            // Surface only once the body is in hand, so the card never claims
            // a load that did not happen.
            shell.surface(Value::map(vec![
                ("io".into(), Value::String("skill".into())),
                ("name".into(), Value::String(name.clone())),
                (
                    "dir".into(),
                    Value::String(dir.to_string_lossy().into_owned()),
                ),
            ]));
            return Settled::Ok(Value::String(format!(
                "// skill root: {}\n\n{}",
                dir.display(),
                body
            )));
        }
    }
    Settled::Ok(Value::String(format!("skill not found: {name}")))
}

fn scheme_skill_list(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(pure(Ty::String)))
}

/// `skill-list` — list all available skills (fresh scan, filtered by
/// the live grant). Returns one `name: description` per line.
fn builtin_skill_list(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let cwd = shell.cwd();
    let config_dir = crate::bootstrap::xdg_app_dir(ral_core::path::basedir::XdgKind::Config);
    let all = skill::discover_all(&cwd, &config_dir);
    let mut out = String::new();
    for (name, dir) in &all {
        let rp = shell.resolve(&dir.join("SKILL.md").to_string_lossy());
        if shell.check_fs_read(&rp).is_ok()
            && let Some(s) = skill::parse_skill(&dir.join("SKILL.md"), name)
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{}: {}", s.name, s.description));
        }
    }
    shell.surface(Value::map(vec![
        ("io".into(), Value::String("skill-list".into())),
        ("count".into(), Value::Int(out.lines().count() as i64)),
    ]));
    Settled::Ok(Value::String(out))
}

pub static EXARCH_BUILTINS: &[BuiltinEntry] = &[
    BuiltinEntry {
        name: Cow::Borrowed("view-text"),
        type_rule: BuiltinTypeRule::Scheme(Some(3), scheme_view_text),
        doc: "view-text <path> <start> <end>  — show the half-open line range [start, end) of PATH, each line tagged `<line-no>\\t<hash>\\t<text>`. Returns a list of records [{line: Int, hash: String, text: String}]. The hash is the witness `edit` checks; copy it, never recompute it. Reads the whole file (the witness depends on file-wide uniqueness) and surfaces one read card.",
        body: BuiltinBody::Static(builtin_view_text),
    },
    BuiltinEntry {
        name: Cow::Borrowed("grep-files"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_grep_files),
        doc: "grep-files <pattern>  — recursively search the cwd (ignore-aware, Rust regex) in one read per matched file, giving [{file, line, text}].",
        body: BuiltinBody::Static(builtin_grep_files),
    },
    BuiltinEntry {
        name: Cow::Borrowed("edit"),
        type_rule: BuiltinTypeRule::Scheme(Some(2), scheme_edit),
        doc: "edit <path> <edits>  — apply a batch of [hash: HASH, line: TEXT] records in one read/write pass: each replaces the line whose witness is HASH with TEXT verbatim (a real newline inside '…' splits the line into several, \\n does not; empty deletes). Atomic — all hashes resolve against the file as read, so edits never interfere; fails writing nothing unless every hash picks exactly one line and no two records name the same one. Surfaces one whole-file diff card.",
        body: BuiltinBody::Static(builtin_edit),
    },
    BuiltinEntry {
        name: Cow::Borrowed("explore-dir"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_explore_dir),
        doc: "explore-dir <n>  — list directory entries up to depth n respecting ignore files.",
        body: BuiltinBody::Static(builtin_explore_dir),
    },
    BuiltinEntry {
        name: Cow::Borrowed("skill-list"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_skill_list),
        doc: "skill-list  — list all available skills (fresh scan, filtered by grant). Returns one `name: description` per line.",
        body: BuiltinBody::Static(builtin_skill_list),
    },
    BuiltinEntry {
        name: Cow::Borrowed("skill"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_skill),
        doc: "skill <name>  — load the full SKILL.md body for the named skill (discovered from .exarch/skills/ and your config). Returns its Markdown instructions, or an error string if not found.",
        body: BuiltinBody::Static(builtin_skill),
    },
    BuiltinEntry {
        name: Cow::Borrowed("fff"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_fff),
        doc: "fff <query>  — fuzzy file-name search (frecency-ranked) over the working tree, returning [String].",
        body: BuiltinBody::Static(builtin_fff),
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

    /// Our wiring of `similar`'s inline changes into [`Row`]s: a changed line
    /// threads through as segments that concatenate back to the original line
    /// (trailing newline stripped) and carry *both* an emphasised and an
    /// unemphasised run, so the emph distinction the renderer needs survives.
    /// *Which* words `similar` flags is its concern, not ours, so we don't
    /// assert the boundary.
    #[test]
    fn whole_file_hunks_threads_inline_segments() {
        let hunks = whole_file_hunks("alpha\nthe quick brown fox\n", "alpha\nthe quick red fox\n");
        let rows: Vec<&Row> = hunks.iter().flat_map(|h| h.rows.iter()).collect();
        let find = |want: fn(&Row) -> bool| *rows.iter().find(|r| want(r)).expect("the row");

        // The shared `alpha` line maps to a context row of one unemphasised
        // segment — our `Equal → Context` mapping.
        let ctx = find(|r| matches!(r, Row::Context(_)));
        assert_eq!(ctx.text(), "alpha");
        assert!(ctx.segs().iter().all(|s| !s.emph));

        // The edited line round-trips on each side, with the `\n` `from_lines`
        // carries stripped, and keeps both an emphasised and an unchanged run.
        for (row, text) in [
            (find(|r| matches!(r, Row::Del(_))), "the quick brown fox"),
            (find(|r| matches!(r, Row::Add(_))), "the quick red fox"),
        ] {
            assert_eq!(row.text(), text);
            assert!(!row.segs().iter().any(|s| s.text.ends_with('\n')));
            assert!(row.segs().iter().any(|s| s.emph), "an emphasised run");
            assert!(row.segs().iter().any(|s| !s.emph), "an unchanged run");
        }
    }

    /// Every witness folds in at least ±MIN_RADIUS of context (the freshness
    /// floor).  In a file shorter than a full floor window every line's window
    /// clamps to the whole file, so the lines are told apart by their offset
    /// within it, each resolved at the floor radius.
    #[test]
    fn window_hashes_floor_at_min_radius() {
        let rows: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let hashes = window_hashes(&rows);
        assert_eq!(hashes.len(), rows.len());
        // The window clamps to the whole file, so every witness folds in the
        // same body and differs only by the offset prefix, at radius MIN_RADIUS.
        let body: String = rows.iter().map(|r| line_hash(r)).collect();
        for i in 0..rows.len() {
            let expected = line_hash(&format!("{MIN_RADIUS}:{i}:{body}"));
            assert_eq!(hashes[i], expected, "row {i} at the floor radius");
        }
        let distinct: std::collections::HashSet<&String> = hashes.iter().collect();
        assert_eq!(distinct.len(), rows.len(), "all distinct");
    }

    /// Two identical lines, each deep enough in the interior to share the same
    /// offset within its ±MIN_RADIUS window, are told apart only by the context
    /// folded into the witness — what a bare line hash could not do.
    #[test]
    fn window_hashes_distinguish_repeated_lines_by_context() {
        let mut rows: Vec<String> = vec!["fn alpha() {".to_string()];
        for k in 0..5 {
            rows.push(format!("    a{k}"));
        }
        let t1 = rows.len();
        rows.push("    target".to_string());
        for k in 0..6 {
            rows.push(format!("    a{k}"));
        }
        rows.push("}".to_string());
        rows.push("fn beta() {".to_string());
        for k in 0..5 {
            rows.push(format!("    b{k}"));
        }
        let t2 = rows.len();
        rows.push("    target".to_string());
        for k in 0..6 {
            rows.push(format!("    b{k}"));
        }
        rows.push("}".to_string());

        let hashes = window_hashes(&rows);
        assert_eq!(
            line_hash(&rows[t1]),
            line_hash(&rows[t2]),
            "same line content"
        );
        assert_ne!(
            hashes[t1], hashes[t2],
            "distinct neighbourhoods must witness distinctly, even at equal offsets"
        );
    }

    /// The property the adaptive-context witness buys over a fixed window: even
    /// a long run of byte-identical lines — where every fixed-radius window
    /// repeats — yields all-distinct witnesses, because each line grows its
    /// context to the run's boundary, and the residual interior folds in its
    /// index.  No two lines share a witness, so `edit` never faces ambiguity.
    #[test]
    fn window_hashes_are_unique_across_a_long_identical_run() {
        let mut rows: Vec<String> = vec!["head".to_string()];
        rows.extend((0..200).map(|_| "dup".to_string()));
        rows.push("tail".to_string());

        let hashes = window_hashes(&rows);
        let distinct: std::collections::HashSet<&String> = hashes.iter().collect();
        assert_eq!(
            distinct.len(),
            rows.len(),
            "every line, even deep in a 200-line identical run, must witness uniquely"
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
