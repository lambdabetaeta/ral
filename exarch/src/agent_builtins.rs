//! Exarch-owned agent search and edit primitives.
//!
//! These are static host builtins: process-owned Rust atoms registered
//! with `ral-core` before the shell compiles user/model source.  Dynamic
//! plugins remain source/alias/hook loaders; this module only publishes
//! the resident agent surface that core should not own.

use crate::skill;
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
use std::fmt::Write as _;
use std::fs;
use std::io::Write;

mod fff_index;

const AGENT_SOURCE: &str = include_str!("../data/agent.ral");

/// The tag `Frame::Attach` carries to name this module's [`install_on`] as
/// the wire engine child's builtin installer.
///
/// See `core/src/engine.rs`'s installer table and the enquiry-channel ADR's
/// shell-parity item.
pub const INSTALLER_TAG: &str = "exarch-agent";

/// The builtin sets the exarch agent host installs on top of core's
/// `CORE_BUILTINS`.
///
/// These are exarch's own surface ([`EXARCH_BUILTINS`]) and core's
/// host-selected `service` ([`ral_core::builtins::SERVICE_BUILTIN`] — the
/// `watch` mechanism with the hosts swapped, kept out of `CORE_BUILTINS` so
/// that only the agent host, under whose worker lease a durable birth means
/// anything, gains it). This is the one source of truth: [`install_on`]
/// installs these sets and the prompt's builtin index names them, so the two
/// cannot drift.
pub static HOST_BUILTIN_SETS: &[&[BuiltinEntry]] =
    &[EXARCH_BUILTINS, ral_core::builtins::SERVICE_BUILTIN];

/// Register the exarch host builtins process-wide and install them into
/// `shell`. Idempotent. The REPL and batch hosts never call this, so they
/// never gain `service`.
pub fn install_on(shell: &mut ral_core::Shell) {
    for &set in HOST_BUILTIN_SETS {
        ral_core::builtins::register_builtins(set);
        shell.install_builtins(set);
    }
}

/// Source the embedded agent helper library into the live shell.
///
/// # Errors
/// Returns `Err` if sourcing the embedded library raises a ral error
/// (re-surfaced as a signal) or propagates a non-error escape.
pub fn install_agent_library(shell: &mut Shell) -> Settled<Value> {
    ral_core::builtins::modules::evaluate_source(shell, AGENT_SOURCE, "<exarch:agent>").map_err(
        |e| match e {
            Break::Error(err) => sig(format!("exarch agent library: {}", err.message)),
            other @ Break::Escape(_) => other,
        },
    )
}

/// One-line docs for the helper-library functions sourced from
/// `agent.ral`.  These are ral closures, not registered builtins, so
/// `help` cannot find them on its own; the host hands them to
/// [`ral_core::builtins::help::register_library_docs`] at boot so the
/// agent library is as discoverable as the prelude.
pub(crate) fn agent_library_docs() -> Vec<(String, String)> {
    [
        ("view-text-around", "view-text-around PATH LINE PEEK  — show the 2*PEEK+1 lines of PATH centred on LINE, tagged like `view-text`, clamped at the top of the file."),
        ("empty-tasks", "empty-tasks  — an empty task list; clears the pinned gauge.  Canonical initialiser."),
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
/// all-digit token in `edit-hash`'s hash position would otherwise elaborate to
/// `Val::Int` and never compare equal to the recomputed `String` hash.
///
/// Private: the witness is never something the model constructs, only one it
/// copies out of a `view-text` read, so neither this nor the
/// window hash is exposed to ral — `view-text`, `view-text-around`,
/// and `edit-hash` are the whole surface.
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
/// Shared verbatim by `view-text` and `edit-hash`, so a read and the
/// edit that follows it derive identical witnesses from identical content.
fn window_hashes(rows: &[String]) -> Vec<String> {
    let n = rows.len();
    if n == 0 {
        return Vec::new();
    }
    let lh: Vec<String> = rows.iter().map(|line| line_hash(line)).collect();

    // The signature `edit-hash` and `view-text` agree on: two lines share a radius-`r`
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
/// shape the redirect frame would have pushed.  `edit-hash`/`edit-replace` are the
/// exception: they read silently and speak only their `write` event.
fn surface_read(shell: &Shell, path: &str) {
    shell.surface(&Value::map(vec![
        ("io".into(), Value::String("read".into())),
        ("path".into(), Value::String(path.to_string())),
    ]));
}

/// Parse a 1-or-greater bound argument for `view-text`.
fn view_bound(arg: &Value, which: &str) -> Settled<usize> {
    match arg.as_int() {
        Some(n) if n >= 1 => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "guarded n >= 1; exarch is 64-bit so usize == u64"
            )]
            let bound = n as usize;
            Ok(bound)
        }
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
            #[allow(
                clippy::cast_possible_wrap,
                reason = "line index bounded by file length; no i64 wrap"
            )]
            let line = i as i64 + 1;
            result_rows.push(Value::map(vec![
                ("line".into(), Value::Int(line)),
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

/// Whether the cwd-relative `rel` is readable under the live grant — the
/// resolve-then-[`check_fs_read`](Shell::check_fs_read) filter the tree walks
/// apply to skip a denied entry rather than abort the whole listing.
fn readable(shell: &mut Shell, rel: &str) -> bool {
    let rp = shell.resolve(rel);
    shell.check_fs_read(&rp).is_ok()
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
        if !readable(shell, &rel) {
            continue;
        }
        // One read per file: the search runs over these bytes directly.
        let Ok(bytes) = fs::read(abs) else { continue };
        let _ = searcher.search_slice(
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
        );
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
    shell.surface(&Value::map(vec![
        ("io".into(), Value::String("grep".into())),
        ("scope".into(), Value::String(".".into())),
        ("pattern".into(), Value::String(pattern.clone())),
    ]));

    let results = search_tree(shell, &pattern)?
        .into_iter()
        .map(|hit| {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "line number bounded by file length; no i64 wrap"
            )]
            let line = hit.line as i64;
            Value::map(vec![
                ("file".into(), Value::String(hit.file)),
                ("line".into(), Value::Int(line)),
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

/// Backslash letters that read as a C/Python-style escape but are not one:
/// `edit-hash`/`edit-replace` take their replacement text verbatim, with no escaping
/// of their own, so a literal `\n` in a replacement lands as two characters
/// — backslash, n — not a newline.  A model reaching for a familiar escape
/// syntax here almost always meant the real character; `has_suspicious_escapes`
/// flags that so `edit-hash`'s stderr note can ask.
const SUSPECT_ESCAPE_LETTERS: [char; 7] = ['n', 't', 'r', '0', '\\', '\'', '"'];

fn has_suspicious_escapes(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(1))
        .any(|i| bytes[i] == b'\\' && SUSPECT_ESCAPE_LETTERS.contains(&(bytes[i + 1] as char)))
}

/// Note a completed edit on stderr for the model, with a trailing warning if
/// any replacement looks like it carries an unintended escape sequence.
/// Surfaced separately from the `write` io event (which stays the forensic
/// record of the commit) since this is a plain status line, not structured
/// data for the card layer.
fn note_edit(shell: &mut Shell, path: &str, lines: &str, plural: bool, any_escapes: bool) {
    let word = if plural { "lines" } else { "line" };
    let warning = if any_escapes {
        " [WARNING: replacements contain escapes, did you mean to do that?]"
    } else {
        ""
    };
    let _ = writeln!(
        shell.stderr_mut(),
        "[EXARCH] Replaced {word} {lines} of {path}.{warning}"
    );
}

/// `edit-hash PATH EDITS` — apply a batch of `[hash: …, line: …]` records in one
/// read/rebuild/write pass, then surface one write io event carrying the
/// whole-file diff.  The read runs in Rust (not a redirect), so it raises no
/// read card; the write goes through core's atomic write door
/// ([`Shell::atomic_write`]) below the redirect frame, so `edit-hash` owns its
/// surface — one committed `write` event whose old/new snapshots the write card
/// renders as a diff, exactly like a committed `>` over the same file.
///
/// Each `hash` resolves against the file as read, before anything is written, so
/// the edits never interfere (adjacent lines included) and the batch is atomic:
/// it fails, writing nothing, unless every hash picks exactly one line (a stale
/// or now-ambiguous hash means the file moved) and no two records name the same
/// line.  The `line` field is the replacement text, taken verbatim: empty
/// deletes the line; a real newline inside it splits the line into several.
///
/// A commit also notes what changed on stderr (see [`note_edit`]), with a
/// warning if a `line` looks like it carries an unintended `\n`/`\t`-style
/// escape rather than the literal character (see [`has_suspicious_escapes`]).
fn builtin_edit_hash(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "edit-hash")?;
    let path = args[0].to_string();
    let edits = match &args[1] {
        Value::List(items) => items,
        other => {
            return Err(sig(format!(
                "edit-hash: expected a List of [hash: …, line: …] records, got {}",
                other.type_name()
            )));
        }
    };
    if edits.is_empty() {
        return Err(sig(
            "edit-hash: no edits given — pass a list of [hash: …, line: …] records.".to_string(),
        ));
    }

    let body = read_text_file(shell, &path, "edit-hash")?;
    let rows = rows_of(&body);
    let n = rows.len();
    let hashes = window_hashes(&rows);

    // Resolve each record to the unique index of the line its hash names,
    // against the original snapshot.  A stale hash fails here, before the write —
    // the failure messages are user-facing and pinned by tests.
    let mut resolved = Vec::with_capacity(edits.len());
    for e in edits {
        let m = match e {
            Value::Map(m) => m,
            other => {
                return Err(sig(format!(
                    "edit-hash: each edit must be a [hash: …, line: …] record, got {}",
                    other.type_name()
                )));
            }
        };
        let want = match m.get("hash") {
            Some(v) => v.to_string(),
            None => {
                return Err(sig(
                    "edit-hash: each edit needs a `hash` field — the witness from view-text/witnesses."
                        .to_string(),
                ));
            }
        };
        let new = match m.get("line") {
            Some(v) => v.to_string(),
            None => {
                return Err(sig(
                    "edit-hash: each edit needs a `line` field — the replacement text.".to_string(),
                ));
            }
        };
        let idxs: Vec<usize> = (0..n).filter(|&i| hashes[i] == want).collect();
        match idxs.len() {
            0 => {
                return Err(sig(format!(
                    "edit-hash: no line in {path} hashes to {want} — did the file change? Re-read with view-text/view-text-around before editing."
                )));
            }
            1 => resolved.push(ResolvedEdit { at: idxs[0], new }),
            _ => {
                let at: Vec<String> = idxs.iter().map(|i| (i + 1).to_string()).collect();
                let r#where = at.join(", ");
                return Err(sig(format!(
                    "edit-hash: hash {want} matches lines {where} in {path} — re-read; the witness has gone stale."
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
                    "edit-hash: two edits name line {} in {path}.",
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
    shell.atomic_write(&path, final_text.as_bytes())?;
    surface_write(shell, &path, &body, &final_text);

    let mut line_nums: Vec<usize> = resolved.iter().map(|r| r.at + 1).collect();
    line_nums.sort_unstable();
    let lines = line_nums
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let any_escapes = resolved.iter().any(|r| has_suspicious_escapes(&r.new));
    note_edit(shell, &path, &lines, line_nums.len() > 1, any_escapes);

    Ok(Value::Unit)
}

/// The largest either snapshot of an edit may reach before its `write` card
/// falls back to a plain listing instead of a whole-file diff — mirrors core's
/// write-preview cap (`PREVIEW_CAP` in `runtime/command/redirect.rs`), so an
/// `edit-hash`/`edit-replace` write and a committed `>` over the same file make the
/// identical diff-vs-listing choice.
const DIFF_SNAPSHOT_CAP: usize = 64 * 1024;

/// Surface the structural `write` io event an `edit-hash`/`edit-replace` commit raises —
/// the same event a committed `>` redirect emits, so the write card renders
/// `old` vs `new` as a whole-file diff below its `write <path> committed`
/// heading.  Both snapshots ride as `old_bytes`/`new_bytes`; `old_bytes` is
/// withheld when either side exceeds [`DIFF_SNAPSHOT_CAP`], so a large edit
/// falls back to a listing preview rather than an unwieldy diff — the same gate
/// core's `old_snapshot_for_diff` applies to the redirect path.  `new_bytes` is
/// capped to that prefix too, since past the cap it only ever seeds the listing.
fn surface_write(shell: &Shell, path: &str, old: &str, new: &str) {
    let fits = old.len() <= DIFF_SNAPSHOT_CAP && new.len() <= DIFF_SNAPSHOT_CAP;
    let new_prefix = new.as_bytes()[..new.len().min(DIFF_SNAPSHOT_CAP)].to_vec();
    let mut fields = vec![
        ("io".into(), Value::String("write".into())),
        ("path".into(), Value::String(path.to_string())),
        ("mode".into(), Value::String("write".into())),
        ("outcome".into(), Value::String("committed".into())),
        ("new_bytes".into(), Value::Bytes(new_prefix)),
    ];
    if fits {
        fields.push(("old_bytes".into(), Value::Bytes(old.as_bytes().to_vec())));
    }
    shell.surface(&Value::map(fields));
}

/// Read a file as a UTF-8 string for the witness layer, gating the read through
/// the active grant the way a `< path` redirect would.  The shared read door of
/// `view-text`, `witnesses`, and `edit-hash`: in Rust, below the ral line, so it
/// never reaches the redirect frame.  Each caller decides its own surface —
/// `view-text`/`witnesses` raise one read card, `edit-hash`/`edit-replace` read silently
/// and speak only their `write` event.  A non-UTF-8 file is named (the witness
/// layer cannot address it); `tool` puts the calling builtin's name on the error.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:witness-read] The witness layer's read door (view-text/witnesses/edit-hash), in Rust below the ral line so it never reaches the redirect frame. view-text and witnesses surface their own read card; edit-hash/edit-replace read silently and emit only their write event. The grant is still checked, as a `< path` redirect would."
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

/// `edit-replace <path> <from> <to>` — read `path`, replace the one literal
/// occurrence of `from` with `to` via the same match/error logic as
/// `string-replace` (0 or >1 matches errors, leaving the file untouched),
/// and write the result back.  Composed over the same read/write doors as
/// `edit-hash`: the read is silent and the write goes through core's atomic door
/// ([`Shell::atomic_write`]), so it surfaces one committed `write` io event
/// whose old/new snapshots the write card renders as a whole-file diff.  It
/// notes the change on stderr the same way `edit-hash` does, with the line range
/// computed from where the match started (see [`note_edit`]).
fn builtin_edit_replace(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 3, "edit-replace")?;
    let path = args[0].to_string();
    let from = args[1].to_string();
    let to = args[2].to_string();
    let body = read_text_file(shell, &path, "edit-replace")?;
    let replaced = ral_core::builtins::strings::builtin_string_replace(&[
        Value::String(from.clone()),
        Value::String(to.clone()),
        Value::String(body.clone()),
    ])
    .map_err(relabel_string_replace)?;
    let final_text = replaced.to_string();
    shell.atomic_write(&path, final_text.as_bytes())?;
    surface_write(shell, &path, &body, &final_text);

    // `string_replace` above already proved `from` matches exactly once, so
    // the same offset it used is the one match here — safe to relocate for
    // the line-range note without re-validating uniqueness.
    let start = body
        .find(&from)
        .expect("edit-replace: match vanished after string_replace confirmed it");
    let start_line = body[..start].matches('\n').count() + 1;
    let end_line = start_line + from.matches('\n').count();
    let lines = if start_line == end_line {
        start_line.to_string()
    } else {
        format!("{start_line}-{end_line}")
    };
    note_edit(
        shell,
        &path,
        &lines,
        start_line != end_line,
        has_suspicious_escapes(&to),
    );

    Ok(Value::Unit)
}

/// `edit-replace` borrows `string-replace`'s match/error logic, but its
/// diagnostics name that verb; re-label the `string-replace:` prefix to
/// `edit-replace:` so a bad `edit-replace` call surfaces the verb the model
/// actually invoked (core's own message stays correct for its own callers).
fn relabel_string_replace(b: Break) -> Break {
    match b {
        Break::Error(mut e) => {
            if let Some(rest) = e.message.strip_prefix("string-replace:") {
                e.message = format!("edit-replace:{rest}");
            }
            Break::Error(e)
        }
        escape @ Break::Escape(_) => escape,
    }
}

fn builtin_explore_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "explore-dir")?;
    let depth: usize = match &args[0] {
        Value::Int(n) if *n >= 0 => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "guarded n >= 0; exarch is 64-bit so usize == u64"
            )]
            let depth = *n as usize;
            depth
        }
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
                    .unwrap_or_else(|_| entry.path())
                    .to_string_lossy()
                    .into_owned();
                // Honour the grant's deny_paths, skipping a denied entry
                // rather than aborting the whole walk, so one off-limits path
                // doesn't blank the listing — the same policy `_search-files`
                // applies to its hits.
                if !readable(shell, &rel) {
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
    Ok(ral_core::builtins::util::checked_read_path(shell, path)?.into_inner())
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

/// `edit-hash :: Str → [{hash: Str, line: Str}] → F Unit` — `path` then a list
/// of `[hash: …, line: …]` records.  Returns Unit: `edit-hash` writes and
/// surfaces, it does not yield a value.
fn scheme_edit_hash(_u: &mut Unifier) -> Scheme {
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

/// `edit-replace :: Str → Str → Str → F Unit` — `path`, `from`, `to`.  Returns
/// Unit: `edit-replace` writes and surfaces, it does not yield a value.
fn scheme_edit_replace(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(
            Ty::String,
            fun(Ty::String, fun(Ty::String, pure(Ty::Unit))),
        )),
    )
}
const DEFAULT_LIMIT: usize = 50;

/// `fff QUERY` — fuzzy file-name search (frecency-ranked) over the
/// working tree, returning a list of matching paths.
fn builtin_fff(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "fff")?;
    let query = args[0].to_string();
    let cwd = checked_read_path(shell, ".")?;
    let idx = fff_index::index_for(&cwd).map_err(sig)?;
    let paths = fff_index::search_paths(idx, &query, DEFAULT_LIMIT).map_err(sig)?;
    let allowed = paths
        .into_iter()
        .filter(|rel| readable(shell, rel))
        .map(Value::String)
        .collect();
    Ok(Value::list(allowed))
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
    for root in skill::skill_roots(&cwd, &config_dir) {
        let dir = root.join(&name);
        let sk_md = dir.join("SKILL.md");
        let rp = shell.resolve(&sk_md.to_string_lossy());
        if shell.check_fs_read(&rp).is_ok() {
            let Ok(body) = skill::read_skill_body(&dir) else {
                return Settled::Ok(Value::String(format!("could not read skill: {name}")));
            };
            // Surface only once the body is in hand, so the card never claims
            // a load that did not happen.
            shell.surface(&Value::map(vec![
                ("io".into(), Value::String("skill".into())),
                ("name".into(), Value::String(name)),
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
#[allow(
    clippy::unnecessary_wraps,
    reason = "registered as a `BuiltinBody::Static` fn pointer; the `Settled<Value>` return is the shape the builtin table dispatches through, not a choice of this body."
)]
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
            let _ = write!(out, "{}: {}", s.name, s.description);
        }
    }
    #[allow(
        clippy::cast_possible_wrap,
        reason = "skill-list line count bounded; no i64 wrap"
    )]
    let count = out.lines().count() as i64;
    shell.surface(&Value::map(vec![
        ("io".into(), Value::String("skill-list".into())),
        ("count".into(), Value::Int(count)),
    ]));
    Settled::Ok(Value::String(out))
}

/// `service-handle :: ∀α. Int → F (Handle α)` — the same per-call-site α
/// instantiation `race :: [Handle α] → …` already accepts.
fn scheme_service_handle(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    let a = Ty::Var(av);
    scheme(
        &[av],
        &[],
        &[],
        thunk(fun(Ty::Int, pure(Ty::Handle(Box::new(a))))),
    )
}

/// `service-handle <id>` — re-acquire a durable service's live `Handle` by
/// id: looked up among this shell's `LeaseClass::Durable` entries only, and
/// handed back bare so the ordinary eliminators resume — `await
/// (service-handle 3)`, `cancel (service-handle 3)`.
///
/// An id naming an ephemeral `spawn`/`watch` worker is refused exactly
/// like an unknown one: an ephemeral spawn is lease-bounded and
/// rediscovered through its binding, not by id — `decisions/260705_leases-
/// and-budgets` carves out by-id re-acquisition for services alone, not a
/// general control plane over every worker.
///
/// A bare top-level `service-handle N` result cannot cross the host seam —
/// a `Handle` is not ground — by design: it exists to be composed with an
/// eliminator in the same turn.
fn builtin_service_handle(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "service-handle")?;
    let id = match args[0].as_int() {
        Some(n) if n >= 0 => {
            #[allow(clippy::cast_sign_loss, reason = "guarded n >= 0")]
            let id = n as u64;
            ral_core::types::WorkerId(id)
        }
        _ => {
            return Err(sig(format!(
                "service-handle: expected a non-negative Int id, got {}",
                args[0].type_name()
            )));
        }
    };
    match shell.worker_by_id(id) {
        Some(entry) if entry.class == ral_core::types::LeaseClass::Durable => {
            Ok(Value::Handle(entry.handle))
        }
        _ => Err(sig(format!(
            "service-handle: no durable service registered with id {} — an ephemeral \
             spawn/watch worker is not reacquired by id, only by the binding that named it",
            id.0
        ))),
    }
}

pub static EXARCH_BUILTINS: &[BuiltinEntry] = &[
    BuiltinEntry {
        name: Cow::Borrowed("view-text"),
        type_rule: BuiltinTypeRule::Scheme(Some(3), scheme_view_text),
        doc: "view-text <path> <start> <end>  — show the half-open line range [start, end) of PATH, each line tagged `<line-no>\\t<hash>\\t<text>`. Returns a list of records [{line: Int, hash: String, text: String}]. The hash is the witness `edit-hash` checks; copy it, never recompute it. Reads the whole file (the witness depends on file-wide uniqueness).",
        body: BuiltinBody::Static(builtin_view_text),
    },
    BuiltinEntry {
        name: Cow::Borrowed("grep-files"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_grep_files),
        doc: "grep-files <pattern>  — recursively search the cwd (ignore-aware, Rust regex) in one read per matched file, giving [{file, line, text}].",
        body: BuiltinBody::Static(builtin_grep_files),
    },
    BuiltinEntry {
        name: Cow::Borrowed("edit-hash"),
        type_rule: BuiltinTypeRule::Scheme(Some(2), scheme_edit_hash),
        doc: "edit-hash <path> <edits>  — apply a batch of [hash: HASH, line: TEXT] records in one read/write pass: each replaces the line whose witness is HASH with TEXT verbatim (a real newline inside '…' splits the line into several, \\n does not; empty deletes). Atomic — all hashes resolve against the file as read, so edits never interfere; fails writing nothing unless every hash picks exactly one line and no two records name the same one.",
        body: BuiltinBody::Static(builtin_edit_hash),
    },
    BuiltinEntry {
        name: Cow::Borrowed("edit-replace"),
        type_rule: BuiltinTypeRule::Scheme(Some(3), scheme_edit_replace),
        doc: "edit-replace <path> <from> <to>  — read PATH, replace the one literal occurrence of FROM with TO, write the result back. Errors, leaving the file untouched, if FROM matches zero times or more than once.",
        body: BuiltinBody::Static(builtin_edit_replace),
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
    BuiltinEntry {
        name: Cow::Borrowed("service-handle"),
        type_rule: BuiltinTypeRule::Scheme(Some(1), scheme_service_handle),
        doc: "service-handle <id>  — re-acquire a durable service's live Handle by id (durable services only; an ephemeral spawn/watch id is refused). Compose with an eliminator: `await (service-handle 3)`, `cancel (service-handle 3)`.",
        body: BuiltinBody::Static(builtin_service_handle),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn status(b: Break) -> i32 {
        match b {
            Break::Error(e) => e.exit_code(),
            other @ Break::Escape(_) => panic!("expected Break::Error, got {other:?}"),
        }
    }

    #[test]
    fn line_hash_ignores_trailing_whitespace() {
        assert_eq!(line_hash("x"), line_hash("x   "));
    }

    /// Every witness folds in at least ±`MIN_RADIUS` of context (the freshness
    /// floor).  In a file shorter than a full floor window every line's window
    /// clamps to the whole file, so the lines are told apart by their offset
    /// within it, each resolved at the floor radius.
    #[test]
    fn window_hashes_floor_at_min_radius() {
        let rows: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let hashes = window_hashes(&rows);
        assert_eq!(hashes.len(), rows.len());
        // The window clamps to the whole file, so every witness folds in the
        // same body and differs only by the offset prefix, at radius MIN_RADIUS.
        let body: String = rows.iter().map(|r| line_hash(r)).collect();
        for (i, hash) in hashes.iter().enumerate() {
            let expected = line_hash(&format!("{MIN_RADIUS}:{i}:{body}"));
            assert_eq!(*hash, expected, "row {i} at the floor radius");
        }
        let distinct: std::collections::HashSet<&String> = hashes.iter().collect();
        assert_eq!(distinct.len(), rows.len(), "all distinct");
    }

    /// Two identical lines, each deep enough in the interior to share the same
    /// offset within its ±`MIN_RADIUS` window, are told apart only by the context
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
    /// index.  No two lines share a witness, so `edit-hash` never faces ambiguity.
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
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
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
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        shell
            .foreground()
            .cancel(ral_core::process::CancelCause::Interrupt);
        let err = builtin_explore_dir(&[Value::Int(3)], &mut shell)
            .expect_err("a cancelled scope must abort the directory walk");
        assert_eq!(status(err), 130);
    }

    // ── `service-handle` builtin ─────────────────────────────────────────

    /// A worker body that blocks until cancelled: it polls
    /// `process::check` so the spawned thread genuinely stays `Running`
    /// (rather than completing instantly). Named distinctly from
    /// `agent.rs`'s own test-only blocker (`test-clear-block-forever`) so
    /// registering both in the same test binary never collides on name.
    fn builtin_test_block_forever(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
        loop {
            ral_core::process::check(shell)?;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn scheme_test_block_forever(_u: &mut Unifier) -> Scheme {
        scheme(&[], &[], &[], thunk(pure(Ty::Unit)))
    }

    static WORKER_TEST_BUILTINS: &[BuiltinEntry] = &[BuiltinEntry {
        name: Cow::Borrowed("test-block-forever"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_test_block_forever),
        doc: "test-only: block until cancelled.",
        body: BuiltinBody::Static(builtin_test_block_forever),
    }];

    /// Run `src` as one top-level turn with no deferred lease (so nothing
    /// races a reap during the test) and no deferred sink (the tests below
    /// never care where a deferred surface batch would land). Panics on a
    /// static (parse/type) failure or a runtime error — every source this
    /// helper runs is expected to compile and complete cleanly.
    fn run_top_level(shell: &mut Shell, src: &str) {
        use ral_core::transport::{Program, Turn};
        use ral_core::{RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin};
        let req = TurnRequest {
            turn: Turn {
                program: Program::Source(src.to_string()),
                script_name: "<test>".to_string(),
                caps: ral_core::types::Capabilities::root(),
                turn_limit: None,
                deferred_lease: None,
                worker_cap: None,
                io: TurnIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: TurnStdin::Empty,
            },
            surface: None,
            deferred: None,
            desk: None,
            lifecycle: Box::new(()),
        };
        match shell.run_turn(req) {
            TurnReport::Ran { result, .. } => {
                result.expect("worker-registry fixture source must run cleanly");
            }
            TurnReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
        }
    }

    /// A `service`-born worker registers under the durable class with its
    /// birth description standing in for the old generic placeholder —
    /// `class: Durable`, `cmd` the description verbatim.
    #[test]
    fn service_registers_as_durable_with_its_description() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        install_on(&mut shell);
        ral_core::builtins::register_builtins(WORKER_TEST_BUILTINS);
        shell.install_builtins(WORKER_TEST_BUILTINS);
        run_top_level(
            &mut shell,
            r#"service "watch the thing" { test-block-forever }"#,
        );

        let entries = shell.workers();
        assert_eq!(entries.len(), 1, "exactly one registered service");
        assert_eq!(entries[0].class, ral_core::types::LeaseClass::Durable);
        assert_eq!(entries[0].cmd, "watch the thing");

        entries[0]
            .handle
            .cancel
            .cancel(ral_core::process::CancelCause::Explicit);
    }

    /// The whole rediscovery idiom: birth a service without keeping its
    /// binding, learn its id, reacquire its handle with `service-handle`,
    /// and `await` it — the round trip a compaction-erased binding leaves
    /// as the only way back.
    #[test]
    fn service_handle_reacquires_a_durable_service_and_await_round_trips() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        install_on(&mut shell);
        run_top_level(&mut shell, r#"service "answer" { 42 }"#);

        let entry = shell.workers().pop().expect("the service registered");
        assert_eq!(entry.class, ral_core::types::LeaseClass::Durable);

        #[allow(
            clippy::cast_possible_wrap,
            reason = "test WorkerId is small; no i64 wrap"
        )]
        let id = entry.id.0 as i64;
        let handle = match builtin_service_handle(&[Value::Int(id)], &mut shell) {
            Ok(Value::Handle(h)) => h,
            other => panic!("service-handle must return a Handle, got {other:?}"),
        };
        let await_fn = shell
            .lookup_builtin("await")
            .expect("core must register `await`");
        let result = await_fn
            .body
            .call(&[Value::Handle(handle)], &mut shell)
            .expect("await on the reacquired handle must succeed");
        let Value::Map(record) = result else {
            panic!("await must return a record");
        };
        assert_eq!(record.get("value"), Some(&Value::Int(42)));
    }

    /// A settled-but-unclaimed service — nothing has awaited, polled, or
    /// cancelled it — still lingers in the registry (a durable birth arms
    /// no retention-exempting lease of its own), so `service-handle`
    /// resolves it exactly as it would a still-running one; `await` on the
    /// reacquired handle then delivers the cached result, never blocking.
    #[test]
    fn service_handle_reacquires_a_settled_but_unclaimed_service() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        install_on(&mut shell);
        run_top_level(&mut shell, r#"service "answer" { 42 }"#);

        let entry = shell.workers().pop().expect("the service registered");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if *entry.handle.state.lock().unwrap() != ral_core::types::HandleState::Running {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the service must settle within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            shell.worker_count(),
            1,
            "settled but unclaimed, the entry still lingers"
        );

        #[allow(
            clippy::cast_possible_wrap,
            reason = "test WorkerId is small; no i64 wrap"
        )]
        let id = entry.id.0 as i64;
        let handle = match builtin_service_handle(&[Value::Int(id)], &mut shell) {
            Ok(Value::Handle(h)) => h,
            other => panic!("a settled-but-retained service must still resolve, got {other:?}"),
        };
        let await_fn = shell
            .lookup_builtin("await")
            .expect("core must register `await`");
        let result = await_fn
            .body
            .call(&[Value::Handle(handle)], &mut shell)
            .expect("await on a retaken, already-settled handle must deliver the cached result");
        let Value::Map(record) = result else {
            panic!("await must return a record");
        };
        assert_eq!(record.get("value"), Some(&Value::Int(42)));
    }

    /// An id naming no registered worker at all is refused.
    #[test]
    fn service_handle_errors_on_an_unknown_id() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        install_on(&mut shell);
        let err = match builtin_service_handle(&[Value::Int(999_999)], &mut shell) {
            Err(Break::Error(e)) => e,
            other => panic!("an unknown id must error, got {other:?}"),
        };
        assert!(err.message.contains("no durable service"));
    }

    /// `service-handle`'s scope is durable services only: an ephemeral
    /// `spawn`'s id is refused exactly like an unknown one, never handed
    /// back — rediscovering an ordinary worker is the binding-lease
    /// ledger's job, not this verb's.
    #[test]
    fn service_handle_refuses_an_ephemeral_worker_id() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        install_on(&mut shell);
        ral_core::builtins::register_builtins(WORKER_TEST_BUILTINS);
        shell.install_builtins(WORKER_TEST_BUILTINS);
        run_top_level(&mut shell, "spawn { test-block-forever }");

        let entry = shell.workers().pop().expect("the spawn registered");
        assert_eq!(entry.class, ral_core::types::LeaseClass::Worker);

        #[allow(
            clippy::cast_possible_wrap,
            reason = "test WorkerId is small; no i64 wrap"
        )]
        let id = entry.id.0 as i64;
        let err = match builtin_service_handle(&[Value::Int(id)], &mut shell) {
            Err(Break::Error(e)) => e,
            other => panic!("an ephemeral worker's id must be refused, got {other:?}"),
        };
        assert!(err.message.contains("no durable service"));

        entry
            .handle
            .cancel
            .cancel(ral_core::process::CancelCause::Explicit);
    }

    /// Builtin-table hygiene: `service-handle` is exarch's own affordance,
    /// never core's. The REPL installs `CORE_BUILTINS` alone and never
    /// `EXARCH_BUILTINS`, so a bare ral host has no `service-handle` at
    /// all — this pins the half of that story that doesn't require
    /// booting a REPL to check: the name simply isn't in the core table.
    #[test]
    fn service_handle_is_exarch_only_never_a_core_builtin() {
        assert!(
            EXARCH_BUILTINS
                .iter()
                .any(|e| e.name.as_ref() == "service-handle"),
            "service-handle must be registered in EXARCH_BUILTINS"
        );
        assert!(
            !ral_core::builtins::CORE_BUILTINS
                .iter()
                .any(|e| e.name.as_ref() == "service-handle"),
            "service-handle must never be a core builtin"
        );
    }

    /// `service`'s availability mirrors `watch`'s with the hosts swapped:
    /// implemented in core but absent from `CORE_BUILTINS` (and from the
    /// `WATCH_BUILTIN` set the REPL adds), it reaches a shell only through
    /// exarch's `install_on` — so a bare ral host has no `service` name to
    /// resolve, while an agent shell dispatches it.
    #[test]
    fn service_is_installed_by_exarch_and_absent_from_the_repl_sets() {
        assert!(
            ral_core::builtins::SERVICE_BUILTIN
                .iter()
                .any(|e| e.name.as_ref() == "service"),
            "SERVICE_BUILTIN must carry the `service` entry"
        );
        assert!(
            !ral_core::builtins::CORE_BUILTINS
                .iter()
                .any(|e| e.name.as_ref() == "service"),
            "service must never be a core builtin"
        );
        assert!(
            !ral_core::builtins::WATCH_BUILTIN
                .iter()
                .any(|e| e.name.as_ref() == "service"),
            "the REPL's host surface (watch) must not smuggle service in"
        );
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        assert!(
            shell.lookup_builtin("service").is_none(),
            "a bare shell (the REPL's baseline) must not dispatch service"
        );
        install_on(&mut shell);
        assert!(
            shell.lookup_builtin("service").is_some(),
            "exarch's install_on must install service"
        );
    }
}
