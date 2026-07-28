//! Exarch's agent search-and-edit builtins: static Rust atoms registered with
//! `ral-core` before any user or model source compiles — the resident agent
//! surface core itself should not own.

use crate::shell_eval::skill;
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, SearcherBuilder, sinks::Lossy};
use ignore::WalkBuilder;
use ral_core::builtins::util::{check_arity, regex_err};
use ral_core::typecheck::builtins::{
    BuiltinTypeRule, closed_record, fun, mk_scheme as scheme, pure, thunk,
};
use ral_core::typecheck::{Scheme, Ty, Unifier};
use ral_core::types::{Break, BuiltinBody, BuiltinEntry, Mooring, Settled, sig};
use ral_core::{HostSurface, Shell, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write;

mod fff_index;
mod harness;

const AGENT_SOURCE: &str = include_str!("../../data/agent.ral");

/// The boot-recipe tag a `Frame::Attach` names to select [`host_surface`] as the
/// wire engine child's builtin surface; matched against the installer table in
/// `core/src/engine.rs`.
pub const INSTALLER_TAG: &str = "exarch-agent";

/// The agent host's builtin surface over core's `CORE_BUILTINS`: exarch's own
/// sets plus core's [`ral_core::builtins::SERVICE_BUILTIN`], which core withholds
/// from `CORE_BUILTINS` so `service` reaches only a host under whose worker lease
/// a durable birth means anything.  The prompt's builtin index ([`crate::prompt`])
/// reads the booted shell's names back off this, so the two cannot drift.
pub fn host_surface() -> HostSurface {
    HostSurface {
        statics: vec![
            EXARCH_BUILTINS,
            harness::HARNESS_BUILTINS,
            ral_core::builtins::SERVICE_BUILTIN,
        ],
        captured: Vec::new(),
    }
}

/// Source the embedded agent helper library into `shell`, installing its one-line
/// docs ([`agent_library_docs`]) in the same act, so `help` can never list a
/// helper the shell lacks nor miss one it has.
///
/// # Errors
/// If sourcing raises a ral error (re-surfaced as a signal) or escapes.
pub fn install_agent_library(mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let result = ral_core::builtins::modules::evaluate_source(
        mooring,
        shell,
        AGENT_SOURCE,
        "<exarch:agent>",
    )
    .map_err(|e| match e {
        Break::Error(err) => sig(format!("exarch agent library: {}", err.message)),
        other @ Break::Escape(_) => other,
    })?;
    shell.install_library_docs(agent_library_docs());
    Ok(result)
}

/// One-line docs for `agent.ral`'s helpers.  They are ral closures, not
/// registered builtins, so `help` cannot find them unaided;
/// [`install_agent_library`] plants these in the sourcing shell's session.
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

/// A line's content hash, trailing whitespace ignored: `h` plus six hex of a
/// Blake3 digest.  The `h` keeps a witness un-lexable as a number — an all-digit
/// token in `edit-hash`'s hash position would elaborate to `Val::Int` and never
/// compare equal to the recomputed `String`.
fn line_hash(line: &str) -> String {
    let stripped = line.trim_end();
    let hex = blake3::hash(stripped.as_bytes()).to_hex();
    format!("h{}", &hex[..6])
}

/// The freshness floor: every witness folds in at least ±`MIN_RADIUS` lines of
/// context, even one unique on its own, so an edit anywhere nearby invalidates
/// it and forces a re-read.
const MIN_RADIUS: usize = 5;

/// The cap on window growth.  Only a run of identical lines longer than
/// `2 * MAX_RADIUS` exhausts it; that residual is named by index instead — the
/// honest positional floor for content that genuinely repeats.
const MAX_RADIUS: usize = 64;

/// How a line is told apart from every other: by a window of some radius, or —
/// only inside a long verbatim run — by its absolute index.
enum Witness {
    Window(usize),
    Index,
}

/// A witness for every line of `rows`: the [`line_hash`]es of the smallest
/// symmetric window — at least ±[`MIN_RADIUS`], at most ±[`MAX_RADIUS`] — that no
/// other line shares, folded together with that radius and the target's offset in
/// the (clamped) window.  Carrying no line number, a witness goes stale on a
/// *local* change, not on every insertion elsewhere.  `view-text` and `edit-hash`
/// both derive theirs here, so a read and the edit that follows it agree.
///
/// Computed by partition refinement, the shape of DFA minimisation: group at the
/// floor radius, then split only the still-colliding classes by one more line of
/// context each side, so a singleton is resolved once and never revisited.
fn window_hashes(rows: &[String]) -> Vec<String> {
    let n = rows.len();
    if n == 0 {
        return Vec::new();
    }
    let lh: Vec<String> = rows.iter().map(|line| line_hash(line)).collect();

    // Two lines collide at radius `r` exactly when these agree: the target's
    // offset within its clamped window, then that window's hashes in order.
    let signature = |i: usize, r: usize| -> String {
        let lo = i.saturating_sub(r);
        let hi = (i + r + 1).min(n);
        let mut s = format!("{}:", i - lo);
        for h in &lh[lo..hi] {
            s.push_str(h);
        }
        s
    };

    let group = |members: &[usize], r: usize| -> Vec<Vec<usize>> {
        let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
        for &i in members {
            by_key.entry(signature(i, r)).or_default().push(i);
        }
        by_key.into_values().collect()
    };

    let mut how: Vec<Witness> = (0..n).map(|_| Witness::Index).collect();
    let all: Vec<usize> = (0..n).collect();
    let mut classes = group(&all, MIN_RADIUS);
    let mut r = MIN_RADIUS;
    while !classes.is_empty() {
        let mut next: Vec<Vec<usize>> = Vec::new();
        for class in classes {
            if class.len() == 1 {
                how[class[0]] = Witness::Window(r);
            } else if r >= MAX_RADIUS {
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
            // The radius is folded in too, so witnesses resolved at different
            // radii cannot collide when their windows happen to coincide.
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

/// Split on raw `\n`, keeping the empty tail a terminal newline leaves, so
/// `join("\n")` reproduces the body byte for byte — what lets a file's trailing
/// newline survive an edit, where the edge-trimming `lines` would eat it.
fn rows_of(body: &str) -> Vec<String> {
    body.split('\n').map(str::to_string).collect()
}

/// Raise the one `{io:"read", path}` card for a whole-file read: `view-text` reads
/// in Rust below the ral line, so no redirect frame speaks for it.
fn surface_read(mooring: &Mooring, path: &str) {
    mooring.surface(&Value::map(vec![
        ("io".into(), Value::String("read".into())),
        ("path".into(), Value::String(path.to_string())),
    ]));
}

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

/// `view-text PATH START END` — the half-open line range `[START, END)`, each row
/// carrying its witness.  Reads and hashes the whole file even for a small
/// slice, since a witness depends on file-wide uniqueness.
fn builtin_view_text(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 3, "view-text")?;
    let path = args[0].to_string();
    let start = view_bound(&args[1], "start")?;
    let end = view_bound(&args[2], "end")?;

    let body = read_text_file(shell, &path, "view-text")?;
    surface_read(mooring, &path);
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

/// The one sanctioned `WalkBuilder::build` site, backed by a clippy ban: an
/// `ignore::Walk` runs to completion regardless of cancellation, so every caller
/// must poll [`ral_core::process::check`] atop each iteration to surface a
/// timeout or Esc as a status-130 `Break` before the next entry.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:grep-walk] The one sanctioned WalkBuilder::build site, rooting the grep door's directory walk; the search emits one `grep` surface for the whole walk and polls check() per entry for cancel."
)]
fn cancellable(builder: &WalkBuilder) -> ignore::Walk {
    builder.build()
}

/// One matching line from [`search_tree`], its path relative to the walk root.
struct SearchHit {
    file: String,
    line: u64,
    text: String,
}

/// Whether cwd-relative `rel` survives the live grant.  Both tree walks filter on
/// this to skip a denied entry rather than abort, so one off-limits path cannot
/// blank a whole listing.
fn readable(shell: &mut Shell, rel: &str) -> bool {
    let rp = shell.resolve(rel);
    shell.check_fs_read(&rp).is_ok()
}

/// Recursively search the cwd for `pattern` (ignore-aware, Rust regex), reading
/// each file's bytes once.  The cancellation poll, the per-file deny skip, and
/// the binary-detection quit all live here, the one site `grep-files` composes over.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:grep-read] The grep door's per-matched-file read, in Rust below the ral line so it never reaches the redirect frame; the logical search emits exactly one `grep` surface (scope + pattern), not one read card per file."
)]
fn search_tree(mooring: &Mooring, shell: &mut Shell, pattern: &str) -> Settled<Vec<SearchHit>> {
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
        ral_core::process::check(mooring)?;
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
        if !readable(shell, &rel) {
            continue;
        }
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

/// `grep-files PATTERN` — [`search_tree`] over the cwd, emitting exactly one
/// `grep` surface for the whole walk rather than a card per file read.
fn builtin_grep_files(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "grep-files")?;
    let pattern = args[0].to_string();

    mooring.surface(&Value::map(vec![
        ("io".into(), Value::String("grep".into())),
        ("scope".into(), Value::String(".".into())),
        ("pattern".into(), Value::String(pattern.clone())),
    ]));

    let results = search_tree(mooring, shell, &pattern)?
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

/// A hash resolved against the file as read: the 0-based line it uniquely named.
struct ResolvedEdit {
    at: usize,
    new: String,
}

/// Backslash letters that read as a C-style escape but are not one here:
/// replacement text is verbatim, so `\n` lands as two characters.  A model
/// reaching for the familiar syntax nearly always meant the real one.
const SUSPECT_ESCAPE_LETTERS: [char; 7] = ['n', 't', 'r', '0', '\\', '\'', '"'];

fn has_suspicious_escapes(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(1))
        .any(|i| bytes[i] == b'\\' && SUSPECT_ESCAPE_LETTERS.contains(&(bytes[i + 1] as char)))
}

/// Note a completed edit on stderr, kept apart from the `write` io event, which
/// stays the structured record of the commit.
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
/// read/rebuild/write pass.  Every hash resolves against the file as read, before
/// anything is written, so the edits cannot interfere (adjacent lines included)
/// and the batch is atomic: nothing is written unless each hash picks exactly one
/// line and no two records name the same one.
///
/// The read raises no card; the write goes through [`Shell::atomic_write`] below
/// the redirect frame, so `edit-hash` owns its surface — one committed `write`
/// event the card layer renders as a whole-file diff, as a committed `>` would.
fn builtin_edit_hash(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
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

    // Resolved against the original snapshot, so a stale or now-ambiguous hash
    // fails here, before anything is written.
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
                    "edit-hash: each edit needs a `hash` field — the witness from view-text/view-text-around."
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
    // Two records on one line: also caught before the write, nothing rebuilt.
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

    // Verbatim: an empty replacement drops the line, a real newline splits it.
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
    surface_write(mooring, &path, &body, &final_text);

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

/// The largest either snapshot of an edit may reach before its `write` card falls
/// back to a plain listing — mirrors `PREVIEW_CAP` in
/// `core/src/runtime/command/redirect.rs`, so an edit and a committed `>` over the
/// same file make the identical diff-vs-listing choice.
const DIFF_SNAPSHOT_CAP: usize = 64 * 1024;

/// Raise the `write` io event an edit commits — the very event a committed `>`
/// emits, so the card renders `old` against `new` as a whole-file diff.
/// `old_bytes` is withheld once either side exceeds [`DIFF_SNAPSHOT_CAP`], the
/// gate core's `old_snapshot_for_diff` applies to the redirect path.
fn surface_write(mooring: &Mooring, path: &str, old: &str, new: &str) {
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
    mooring.surface(&Value::map(fields));
}

/// The witness layer's shared read door, gating on the live grant as a `< path`
/// redirect would but staying in Rust, below the redirect frame — so each caller
/// owns its own surface.  `tool` names the calling builtin in the error.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:witness-read] The witness layer's read door (view-text/edit-hash/edit-replace), in Rust below the ral line so it never reaches the redirect frame. view-text surfaces its own read card; edit-hash/edit-replace read silently and emit only their write event. The grant is still checked, as a `< path` redirect would."
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

/// `edit-replace PATH FROM TO` — replace the one literal occurrence of `FROM`,
/// borrowing `string-replace`'s match/error logic, so 0 or >1 matches errors and
/// leaves the file untouched.  Composed over the same doors as `edit-hash`: a
/// silent read, then [`Shell::atomic_write`], surfacing one committed `write`.
fn builtin_edit_replace(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
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
    surface_write(mooring, &path, &body, &final_text);

    // `string_replace` above already proved `from` matches exactly once, so this
    // relocation for the line-range note needs no second uniqueness check.
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

/// Re-label a borrowed `string-replace:` diagnostic, so the model reads back the
/// verb it actually called.
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

/// `explore-dir DEPTH` — list the cwd's tree (ignore-aware) to `DEPTH`, through
/// the one sanctioned walk site ([`cancellable`]).
fn builtin_explore_dir(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
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
        ral_core::process::check(mooring)?;
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

/// `view-text :: Str → Int → Int → F [[line: Int, hash: Str, text: Str]]`
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

/// `edit-hash :: Str → [[hash: Str, line: Str]] → F Unit`
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

/// `edit-replace :: Str → Str → Str → F Unit`
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

/// `fff QUERY` — frecency-ranked fuzzy file-name search over the working tree.
fn builtin_fff(args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
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

/// `skill NAME` — load a skill's full body, rescanning at each call so a skill
/// added or edited mid-session is found.
fn builtin_skill(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "skill")?;
    let name = args[0].to_string();
    // Rejecting it here is what keeps `root.join(&name)` inside the skills root.
    if !skill::valid_skill_name(&name) {
        return Settled::Ok(Value::String(format!("skill not found: {name}")));
    }
    let cwd = shell.cwd();
    let config_dir = crate::bootstrap::EXARCH.xdg_dir(ral_core::path::basedir::XdgKind::Config);
    for root in skill::skill_roots(&cwd, &config_dir) {
        let dir = root.join(&name);
        let sk_md = dir.join("SKILL.md");
        let rp = shell.resolve(&sk_md.to_string_lossy());
        if shell.check_fs_read(&rp).is_ok() {
            let Ok(body) = skill::read_skill_body(&dir) else {
                return Settled::Ok(Value::String(format!("could not read skill: {name}")));
            };
            // Only once the body is in hand, so the card never claims a load
            // that did not happen.
            mooring.surface(&Value::map(vec![
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

/// `skill-list` — every discoverable skill, one `name: description` per line,
/// fresh-scanned and filtered by the live grant.
#[allow(
    clippy::unnecessary_wraps,
    reason = "registered as a `BuiltinBody::Static` fn pointer; the `Settled<Value>` return is the shape the builtin table dispatches through, not a choice of this body."
)]
fn builtin_skill_list(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let cwd = shell.cwd();
    let config_dir = crate::bootstrap::EXARCH.xdg_dir(ral_core::path::basedir::XdgKind::Config);
    let all = skill::discover_all(&cwd, &config_dir);
    let mut out = String::new();
    for (name, dir) in &all {
        let sk_md = dir.join("SKILL.md");
        let rp = shell.resolve(&sk_md.to_string_lossy());
        if shell.check_fs_read(&rp).is_ok()
            && let Some(s) = skill::parse_skill(dir, name)
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
    mooring.surface(&Value::map(vec![
        ("io".into(), Value::String("skill-list".into())),
        ("count".into(), Value::Int(count)),
    ]));
    Settled::Ok(Value::String(out))
}

/// `service-handle :: ∀α. Int → F (Handle α)` — the per-call-site α instantiation
/// `race :: [Handle α] → …` already accepts.
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

/// `service-handle ID` — re-acquire a durable service's live `Handle`, looked up
/// among this shell's `LeaseClass::Durable` entries alone and handed back bare so
/// the ordinary eliminators resume.  An ephemeral `spawn`/`watch` id is refused
/// like an unknown one: those are lease-bounded and rediscovered through their
/// binding, so by-id re-acquisition stays carved out for services rather than
/// becoming a control plane over every worker.
fn builtin_service_handle(args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
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

    /// Dress a bare test shell with exarch's host surface.
    fn dress(shell: &mut Shell) {
        let surface = host_surface();
        for set in surface.statics {
            shell.install_builtins(set);
        }
        for set in surface.captured {
            shell.install_captured_builtins(set);
        }
    }

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

    /// In a file shorter than a full floor window, every window clamps to the
    /// whole file, so lines are told apart by offset alone, all at the floor.
    #[test]
    fn window_hashes_floor_at_min_radius() {
        let rows: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let hashes = window_hashes(&rows);
        assert_eq!(hashes.len(), rows.len());
        let body: String = rows.iter().map(|r| line_hash(r)).collect();
        for (i, hash) in hashes.iter().enumerate() {
            let expected = line_hash(&format!("{MIN_RADIUS}:{i}:{body}"));
            assert_eq!(*hash, expected, "row {i} at the floor radius");
        }
        let distinct: std::collections::HashSet<&String> = hashes.iter().collect();
        assert_eq!(distinct.len(), rows.len(), "all distinct");
    }

    /// Two identical lines sharing an offset within their floor windows are
    /// separated by folded context alone — what a bare line hash cannot do.
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

    /// What adaptive context buys over a fixed window: even a run where every
    /// fixed-radius window repeats still witnesses distinctly, the interior
    /// falling back to its index.
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

    /// A pre-cancelled scope aborts `search_tree`'s walk at its first poll,
    /// before any filesystem entry is touched.
    #[test]
    fn search_files_honours_a_cancelled_scope() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let m = Mooring::adrift();
        m.cancel.cancel(ral_core::process::CancelCause::Interrupt);
        let err = builtin_grep_files(&[Value::String("x".into())], &m, &mut shell)
            .expect_err("a cancelled scope must abort the search walk");
        assert_eq!(status(err), 130);
    }

    #[test]
    fn explore_dir_honours_a_cancelled_scope() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let m = Mooring::adrift();
        m.cancel.cancel(ral_core::process::CancelCause::Interrupt);
        let err = builtin_explore_dir(&[Value::Int(3)], &m, &mut shell)
            .expect_err("a cancelled scope must abort the directory walk");
        assert_eq!(status(err), 130);
    }

    // ── `service-handle` builtin ─────────────────────────────────────────

    /// A worker body that blocks until cancelled, polling `process::check` so the
    /// thread genuinely stays `Running` rather than settling instantly.  Named
    /// apart from `test-clear-block-forever` in `exarch/src/agent/testkit.rs` so
    /// registering both in one test binary cannot collide.
    fn builtin_test_block_forever(
        _args: &[Value],
        mooring: &Mooring,
        _shell: &mut Shell,
    ) -> Settled<Value> {
        loop {
            ral_core::process::check(mooring)?;
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

    /// Run `src` as one top-level run, deliberately without a deferred lease so
    /// nothing races a reap mid-test.  Panics on any failure.
    fn run_top_level(shell: &mut Shell, src: &str) {
        use ral_core::transport::{Program, Run};
        use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};
        let req = RunRequest {
            run: Run {
                program: Program::Source(src.to_string()),
                script_name: "<test>".to_string(),
                caps: ral_core::types::Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: RunStdin::Empty,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        };
        match shell.run(req) {
            RunReport::Ran { result, .. } => {
                result.expect("worker-registry fixture source must run cleanly");
            }
            RunReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
        }
    }

    /// Dressed, `service` resolves to its own `Handle`-returning scheme rather
    /// than falling through to an external command, so feeding it to `cancel`
    /// typechecks.  The negative half of this is
    /// `service_is_external_on_a_bare_core_table` in `core/tests/typecheck.rs`.
    #[test]
    fn service_typechecks_on_an_exarch_dressed_shell() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        dress(&mut shell);
        match ral_core::compile_and_typecheck(
            r#"let h = service "birth" { return 1 }; cancel $h"#,
            shell.session_schemes(),
            ral_core::source::FileId::DUMMY,
            "",
        ) {
            ral_core::CompileOutcome::Compiled(_) => {}
            ral_core::CompileOutcome::Parse(e) => panic!("expected a clean parse, got: {e}"),
            ral_core::CompileOutcome::Types(errs) => panic!(
                "expected `service`'s Handle to satisfy `cancel` on an exarch-dressed shell, got: {:?}",
                errs.iter()
                    .map(|e| e.kind.render_message())
                    .collect::<Vec<_>>()
            ),
        }
    }

    #[test]
    fn service_registers_as_durable_with_its_description() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        dress(&mut shell);
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

    /// The whole rediscovery idiom: birth a service without keeping its binding,
    /// reacquire by id, `await` — the only way back once compaction erases the
    /// binding that named it.
    #[test]
    fn service_handle_reacquires_a_durable_service_and_await_round_trips() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        dress(&mut shell);
        run_top_level(&mut shell, r#"service "answer" { 42 }"#);

        let entry = shell.workers().pop().expect("the service registered");
        assert_eq!(entry.class, ral_core::types::LeaseClass::Durable);

        #[allow(
            clippy::cast_possible_wrap,
            reason = "test WorkerId is small; no i64 wrap"
        )]
        let id = entry.id.0 as i64;
        let m = Mooring::adrift();
        let handle = match builtin_service_handle(&[Value::Int(id)], &m, &mut shell) {
            Ok(Value::Handle(h)) => h,
            other => panic!("service-handle must return a Handle, got {other:?}"),
        };
        let await_fn = shell
            .lookup_builtin("await")
            .expect("core must register `await`");
        let result = await_fn
            .body
            .call(&[Value::Handle(handle)], &m, &mut shell)
            .expect("await on the reacquired handle must succeed");
        let Value::Map(record) = result else {
            panic!("await must return a record");
        };
        assert_eq!(record.get("value"), Some(&Value::Int(42)));
    }

    /// A settled service nothing has claimed still lingers, a durable birth
    /// arming no retention-exempting lease of its own, so `service-handle`
    /// resolves it as it would a running one.
    #[test]
    fn service_handle_reacquires_a_settled_but_unclaimed_service() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        dress(&mut shell);
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
        let m = Mooring::adrift();
        let handle = match builtin_service_handle(&[Value::Int(id)], &m, &mut shell) {
            Ok(Value::Handle(h)) => h,
            other => panic!("a settled-but-retained service must still resolve, got {other:?}"),
        };
        let await_fn = shell
            .lookup_builtin("await")
            .expect("core must register `await`");
        let result = await_fn
            .body
            .call(&[Value::Handle(handle)], &m, &mut shell)
            .expect("await on a retaken, already-settled handle must deliver the cached result");
        let Value::Map(record) = result else {
            panic!("await must return a record");
        };
        assert_eq!(record.get("value"), Some(&Value::Int(42)));
    }

    #[test]
    fn service_handle_errors_on_an_unknown_id() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        dress(&mut shell);
        let err =
            match builtin_service_handle(&[Value::Int(999_999)], &Mooring::adrift(), &mut shell) {
                Err(Break::Error(e)) => e,
                other => panic!("an unknown id must error, got {other:?}"),
            };
        assert!(err.message.contains("no durable service"));
    }

    /// An ephemeral `spawn`'s id is refused exactly like an unknown one:
    /// rediscovering an ordinary worker is the binding-lease ledger's job.
    #[test]
    fn service_handle_refuses_an_ephemeral_worker_id() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        dress(&mut shell);
        shell.install_builtins(WORKER_TEST_BUILTINS);
        run_top_level(&mut shell, "spawn { test-block-forever }");

        let entry = shell.workers().pop().expect("the spawn registered");
        assert_eq!(entry.class, ral_core::types::LeaseClass::Worker);

        #[allow(
            clippy::cast_possible_wrap,
            reason = "test WorkerId is small; no i64 wrap"
        )]
        let id = entry.id.0 as i64;
        let err = match builtin_service_handle(&[Value::Int(id)], &Mooring::adrift(), &mut shell) {
            Err(Break::Error(e)) => e,
            other => panic!("an ephemeral worker's id must be refused, got {other:?}"),
        };
        assert!(err.message.contains("no durable service"));

        entry
            .handle
            .cancel
            .cancel(ral_core::process::CancelCause::Explicit);
    }

    /// `service-handle` is exarch's own affordance, never core's — so the REPL,
    /// which installs `CORE_BUILTINS` alone, cannot reach it.
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
    /// implemented in core but absent from both `CORE_BUILTINS` and the
    /// `WATCH_BUILTIN` set the REPL adds, it reaches a shell only through
    /// exarch's host surface.
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
        dress(&mut shell);
        assert!(
            shell.lookup_builtin("service").is_some(),
            "exarch's host surface must install service"
        );
    }

    /// `service`'s story inverted: the host surface is a bare fn pointer, so a
    /// `detach` it carried would arrive armed on every shell dressed through it,
    /// child shells that never asked for a budget included.  `boot_root_shell`
    /// installs it per boot instead, in the same act that arms its policy.
    #[test]
    fn detach_is_absent_from_the_host_surface() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        dress(&mut shell);
        assert!(
            shell.lookup_builtin("detach").is_none(),
            "the host surface must not carry `detach`: it cannot see the capabilities that \
             decide whether a survivor is possible at all"
        );
    }

    /// Name and budget arrive together, so a shell that can name the verb can
    /// always spend it.
    #[cfg(unix)]
    #[test]
    fn a_boot_granted_detach_gains_the_verb_and_its_budget() {
        let scratch = crate::bootstrap::Scratch::for_test(crate::bootstrap::EXARCH, "detach-armed")
            .expect("scratch dir");
        let shell = crate::agent::seat::boot_root_shell(
            &scratch,
            std::env::current_dir().expect("test process has a cwd"),
            true,
        );
        assert!(
            shell.lookup_builtin("detach").is_some(),
            "a boot granted detach must install the verb"
        );
        assert_eq!(
            shell
                .detach_policy()
                .expect("installing the name and arming the budget is one act")
                .budget,
            crate::shell_eval::DETACH_BIRTH_BUDGET
        );
    }

    /// A boot denied the verb leaves `detach` an ordinary unknown command, never
    /// a builtin that resolves and refuses: what a *sandbox* does to a call is a
    /// grant's business, asked of the live stack, so it is never read off a boot.
    #[cfg(unix)]
    #[test]
    fn a_boot_denied_detach_leaves_it_an_unknown_name() {
        let scratch =
            crate::bootstrap::Scratch::for_test(crate::bootstrap::EXARCH, "detach-denied")
                .expect("scratch dir");
        let shell = crate::agent::seat::boot_root_shell(
            &scratch,
            std::env::current_dir().expect("test process has a cwd"),
            false,
        );
        assert!(
            shell.lookup_builtin("detach").is_none(),
            "a boot denied the verb must leave the name absent, so calling it reads as an \
             unknown command rather than a permission denial"
        );
        assert!(shell.detach_policy().is_none(), "and no policy is armed");
    }

    /// `/clear` reboots through the same `boot_root_shell`, so the fresh shell
    /// re-gains name and budget together; there is no second install site.
    #[cfg(unix)]
    #[test]
    fn clear_reboots_a_shell_that_still_carries_detach() {
        use crate::agent::event::AgentLog;
        use crate::agent::seat::{Seat, boot_root_shell};

        let dir = std::env::temp_dir().join(format!("exarch-detach-clear-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = AgentLog::root(&dir, 0, "test-model", "test", 0).expect("session log");
        let scratch = std::sync::Arc::new(
            crate::bootstrap::Scratch::for_test(crate::bootstrap::EXARCH, "detach-clear")
                .expect("scratch dir"),
        );
        let cwd = std::env::current_dir().expect("test process has a cwd");
        let mut seat = Seat::identity(
            boot_root_shell(&scratch, cwd.clone(), true),
            scratch,
            cwd,
            true,
            &log,
        );

        seat.clear(&log);
        let engine = seat.shell_mut();
        assert!(
            engine.shell.lookup_builtin("detach").is_some(),
            "the shell `/clear` boots must carry the verb its predecessor had"
        );
        assert_eq!(
            engine
                .shell
                .detach_policy()
                .expect("and the budget armed in the same act")
                .budget,
            crate::shell_eval::DETACH_BIRTH_BUDGET
        );
        drop(engine);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Sourcing the closures and installing their docs is one act, so `help`
    /// names the helpers on exactly the shells that have them.
    #[test]
    fn help_lists_a_library_section_only_on_a_shell_that_sourced_it() {
        use ral_core::transport::{Program, Run};
        use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};

        let run_help = |shell: &mut Shell| -> String {
            let req = RunRequest {
                run: Run {
                    program: Program::Source("help".to_string()),
                    script_name: "<test>".to_string(),
                    caps: ral_core::types::Capabilities::root(),
                    wall: None,
                    deferred_lease: None,
                    worker_cap: None,
                    io: RunIo::Capture,
                    terminal: RequestedTerminalAccess::Denied,
                    stdin: RunStdin::Empty,
                },
                surface: None,
                deferred: None,
                desk: None,
                nursery: None,
                lifecycle: Box::new(()),
            };
            match shell.run(req) {
                RunReport::Ran {
                    result, captured, ..
                } => {
                    result.expect("`help` must run cleanly");
                    String::from_utf8(captured.expect("Capture io yields captured bytes").stdout)
                        .expect("help output is UTF-8")
                }
                RunReport::Static { .. } => panic!("`help` must compile"),
            }
        };

        let mut bare = Shell::new(ral_core::io::TerminalState::default());
        let bare_out = run_help(&mut bare);
        assert!(
            !bare_out.contains("Library:"),
            "a shell that never sourced the agent library must list no Library section, got:\n{bare_out}"
        );

        let mut dressed = Shell::new(ral_core::io::TerminalState::default());
        dress(&mut dressed);
        install_agent_library(&Mooring::adrift(), &mut dressed).expect("embedded agent library");
        let dressed_out = run_help(&mut dressed);
        assert!(
            dressed_out.contains("Library:"),
            "an exarch-dressed shell must list a Library section, got:\n{dressed_out}"
        );
        assert!(
            dressed_out.contains("view-text-around"),
            "the Library section must name the sourced helpers, got:\n{dressed_out}"
        );
    }
}
