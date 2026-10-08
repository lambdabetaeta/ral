//! Exarch's agent search-and-edit builtins: static Rust atoms registered with
//! `ral-core` before any user or model source compiles — the resident agent
//! surface core itself should not own.

use crate::card::{Diff, encode_edit};
use crate::skill;
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, SearcherBuilder, sinks::Lossy};
use ignore::WalkBuilder;
use ral_core::builtins::util::regex_err;
use ral_core::capability::FsOp;
use ral_core::fact::{Grep, Read};
use ral_core::ty::Site;
use ral_core::ty::{Scheme, Ty, closed_record};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{fun, mk_plain_scheme, mk_scheme as scheme, pure, thunk};
use ral_core::types::{Break, BuiltinBody, BuiltinEntry, Mooring, Observed, Settled, sig};
use ral_core::{HostSurface, Shell, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Read as _, Write};
use std::sync::Arc;

mod fff_index;
#[cfg(target_os = "linux")]
mod guest_port;
pub(crate) mod harness;

/// The boot-recipe tag a `Frame::Attach` names to select [`host_surface`] as the
/// wire engine child's builtin surface; matched against the installer table in
/// `core/src/engine.rs`.
pub const INSTALLER_TAG: &str = "exarch-agent";

/// The agent host's builtin surface over core's `CORE_BUILTINS`: exarch's own
/// sets plus core's [`ral_core::builtins::SERVICE_BUILTIN`].
///
/// Core withholds it from `CORE_BUILTINS` so `service` reaches only a host
/// under whose worker lease a durable birth means anything.  The prompt's
/// builtin index ([`crate::prompt`]) reads the booted shell's names back off
/// this, so the two cannot drift.
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
/// *local* change, not on every insertion elsewhere.  `view-hash` and `edit-hash`
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
/// `join("\n")` reproduces the body byte for byte: what lets a file's trailing
/// newline survive an edit, where the edge-trimming `lines` would eat it.
fn rows_of(body: &str) -> Vec<String> {
    body.split('\n').map(str::to_string).collect()
}

/// Raise the one read observation for a whole-file read: the readers read in
/// Rust below the ral line, so no redirect frame speaks for them.
fn surface_read(shell: &Shell, mooring: &Mooring, path: &str) {
    let read = Observed::Read(Read {
        path: path.to_string(),
    });
    mooring.surface_data(&shell.observation(read).to_surface());
}

fn view_bound(arg: &Value, which: &str, tool: &str) -> Settled<usize> {
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
            "{tool}: {which} must be an Int >= 1, got {}",
            arg.type_name()
        ))),
    }
}

/// The lines of `PATH` and the half-open slice of them `[START, END)` names,
/// clamped to the file — what both readers show, differing only in what each
/// row carries.
fn view_range(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
    tool: &str,
) -> Settled<(Vec<String>, std::ops::Range<usize>)> {
    let path = args[0].as_str(tool)?;
    let start = view_bound(&args[1], "start", tool)?;
    let end = view_bound(&args[2], "end", tool)?;
    if end <= start {
        return Err(sig(format!(
            "{tool}: end must be greater than start (the range [start, end) is half-open), got start={start}, end={end}"
        )));
    }

    let body = read_text_file(shell, path, tool)?;
    surface_read(shell, mooring, path);
    let rows = rows_of(&body);
    let hi = (end - 1).min(rows.len());
    Ok((rows, start - 1..hi))
}

/// A row index as the 1-based line number the model reads.
fn line_no(i: usize) -> Value {
    #[allow(
        clippy::cast_possible_wrap,
        reason = "line index bounded by file length; no i64 wrap"
    )]
    let line = i as i64 + 1;
    Value::Int(line)
}

/// `view-text PATH START END` — the half-open line range `[START, END)` as
/// `[line, text]` rows.  The text is the whole address here: `edit-replace`
/// matches it verbatim, so a row is copied as it stands.
fn builtin_view_text(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let (rows, range) = view_range(args, mooring, shell, "view-text")?;
    Ok(Value::list(
        range
            .map(|i| {
                Value::map(vec![
                    ("line".into(), line_no(i)),
                    ("text".into(), Value::string(rows[i].clone())),
                ])
            })
            .collect(),
    ))
}

/// `view-hash PATH START END` — the same range with each row's witness, the
/// handle `edit-hash` checks.  Hashes the whole file even for a small slice,
/// since a witness depends on file-wide uniqueness.
fn builtin_view_hash(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let (rows, range) = view_range(args, mooring, shell, "view-hash")?;
    let hashes = window_hashes(&rows);
    Ok(Value::list(
        range
            .map(|i| {
                Value::map(vec![
                    ("line".into(), line_no(i)),
                    ("hash".into(), Value::string(hashes[i].clone())),
                    ("text".into(), Value::string(rows[i].clone())),
                ])
            })
            .collect(),
    ))
}

/// The one sanctioned `WalkBuilder::build` site, backed by a clippy ban: an
/// `ignore::Walk` runs to completion regardless of cancellation, so every caller
/// must poll [`Mooring::check`] atop each iteration to surface a
/// timeout or Esc as a cancellation `Break` before the next entry.
#[allow(
    clippy::disallowed_methods,
    reason = "[surface:grep-walk] The one sanctioned WalkBuilder::build site, rooting the grep site's directory walk; the search emits one `grep` surface for the whole walk and polls check() per entry for cancel."
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
        mooring.check()?;
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
        let rp = shell.resolve(&rel);
        let Some(located) = shell.locate_if_admitted(&rp, &FsOp::Read) else {
            continue;
        };
        let Ok(mut file) = located.read() else {
            continue;
        };
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            continue;
        }
        searcher
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
            .map_err(|e| sig(format!("grep-files: {rel}: {e}")))?;
    }
    Ok(results)
}

/// `grep-files PATTERN` — [`search_tree`] over the cwd, emitting exactly one
/// `grep` surface for the whole walk rather than a card per file read.
fn builtin_grep_files(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let pattern = args[0].as_str("grep-files")?;

    let grep = Observed::Grep(Grep {
        scope: ".".to_string(),
        pattern: pattern.to_string(),
    });
    mooring.surface_data(&shell.observation(grep).to_surface());

    let results = search_tree(mooring, shell, pattern)?
        .into_iter()
        .map(|hit| {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "line number bounded by file length; no i64 wrap"
            )]
            let line = hit.line as i64;
            Value::map(vec![
                ("file".into(), Value::string(hit.file)),
                ("line".into(), Value::Int(line)),
                ("text".into(), Value::string(hit.text)),
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

/// Note a completed edit on stderr, kept apart from the `write` observation,
/// which stays the structured record of the commit.
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
/// the redirect frame, which observes nothing. So `edit-hash` owns its surface
/// entirely, and speaks it as one whole-file diff card ([`surface_edit`]).
fn builtin_edit_hash(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let path = args[0].as_str("edit-hash")?;
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
            "edit-hash: no edits given; pass a list of [hash: …, line: …] records.".to_string(),
        ));
    }

    let body = read_text_file(shell, path, "edit-hash")?;
    let rows = rows_of(&body);
    let n = rows.len();
    let hashes = window_hashes(&rows);

    // Resolved against the original snapshot, so a stale or now-ambiguous hash
    // fails here, before anything is written.
    let mut resolved = Vec::with_capacity(edits.len());
    for e in edits {
        let e = e.into_owned();
        let m = match e {
            Value::Map(m) => m,
            other => {
                return Err(sig(format!(
                    "edit-hash: each edit must be a [hash: …, line: …] record, got {}",
                    other.type_name()
                )));
            }
        };
        let want = match m.get("hash").map(std::borrow::Cow::into_owned) {
            Some(v) => v.as_str("edit-hash")?.to_string(),
            None => {
                return Err(sig(
                    "edit-hash: each edit needs a `hash` field; the witness from view-hash/view-hash-around."
                        .to_string(),
                ));
            }
        };
        let new = match m.get("line").map(std::borrow::Cow::into_owned) {
            Some(v) => v.as_str("edit-hash")?.to_string(),
            None => {
                return Err(sig(
                    "edit-hash: each edit needs a `line` field; the replacement text.".to_string(),
                ));
            }
        };
        let idxs: Vec<usize> = (0..n).filter(|&i| hashes[i] == want).collect();
        match idxs.len() {
            0 => {
                return Err(sig(format!(
                    "edit-hash: no line in {path} hashes to {want}; did the file change? Re-read with view-hash/view-hash-around before editing."
                )));
            }
            1 => resolved.push(ResolvedEdit { at: idxs[0], new }),
            _ => {
                let at: Vec<String> = idxs.iter().map(|i| (i + 1).to_string()).collect();
                let r#where = at.join(", ");
                return Err(sig(format!(
                    "edit-hash: hash {want} matches lines {where} in {path}; re-read; the witness has gone stale."
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
    shell.atomic_write(path, final_text.as_bytes())?;
    surface_edit(mooring, path, &body, &final_text);

    let mut line_nums: Vec<usize> = resolved.iter().map(|r| r.at + 1).collect();
    line_nums.sort_unstable();
    let lines = line_nums
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let any_escapes = resolved.iter().any(|r| has_suspicious_escapes(&r.new));
    note_edit(shell, path, &lines, line_nums.len() > 1, any_escapes);

    Ok(Value::Unit)
}

/// Surface what an edit changed: the diff of the two texts the builtin
/// already holds, so no file is too large to read as the edit it was, and
/// only the diff — cut at the source — crosses the surface.
///
/// The read sinks silently and `atomic_write` observes nothing, so this is the
/// whole of what an edit says, and nothing when it changed no line.
fn surface_edit(mooring: &Mooring, path: &str, old: &str, new: &str) {
    let diff = Diff::between(old, new);
    if !diff.hunks.is_empty() {
        mooring.surface_data(&encode_edit(path, &diff));
    }
}

/// The witness layer's shared read door, gating on the live grant as a `< path`
/// redirect would but staying in Rust, below the redirect frame — so each caller
/// owns its own surface.  `tool` names the calling builtin in the error.
fn read_text_file(shell: &mut Shell, path: &str, tool: &str) -> Settled<String> {
    let rp = shell.resolve(path);
    let located = shell.locate(&rp, &FsOp::Read)?;
    let mut file = located.read().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            sig(format!(
                "{tool}: {path} does not exist; {tool} never creates a file; \
                 `to-string BODY > path` writes a new one."
            ))
        } else {
            sig(format!("{tool}: cannot read {path}: {e}"))
        }
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| sig(format!("{tool}: cannot read {path}: {e}")))?;
    String::from_utf8(bytes).map_err(|_| {
        sig(format!(
            "{tool}: '{path}' is not valid UTF-8; these tools read and edit text only."
        ))
    })
}

/// `edit-replace PATH FROM TO` — replace the one literal occurrence of `FROM`, so
/// 0 or >1 matches errors and leaves the file untouched.  Composed over the same
/// doors as `edit-hash`: a silent read, then [`Shell::atomic_write`], surfacing
/// one whole-file diff.
fn builtin_edit_replace(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let path = args[0].as_str("edit-replace")?;
    let from = args[1].as_str("edit-replace")?;
    let to = args[2].as_str("edit-replace")?;
    if from.is_empty() {
        return Err(sig("edit-replace: FROM must be non-empty."));
    }
    let body = read_text_file(shell, path, "edit-replace")?;
    let starts = ral_core::builtins::strings::occurrence_starts(&body, from);
    let &[start] = starts.as_slice() else {
        return Err(no_unique_match(&body, from, path, &starts));
    };

    let final_text = body.replacen(from, to, 1);
    shell.atomic_write(path, final_text.as_bytes())?;
    surface_edit(mooring, path, &body, &final_text);

    let start_line = line_of(&body, start);
    // A FROM ending in a newline claims that terminator but no content on the
    // line after it, so the range stops short of it.
    let end_line = start_line + from.matches('\n').count() - usize::from(from.ends_with('\n'));
    let lines = if start_line == end_line {
        start_line.to_string()
    } else {
        format!("{start_line}-{end_line}")
    };
    note_edit(
        shell,
        path,
        &lines,
        start_line != end_line,
        has_suspicious_escapes(to),
    );

    Ok(Value::Unit)
}

/// The 1-based line that byte offset `at` falls on.
fn line_of(body: &str, at: usize) -> usize {
    body[..at].matches('\n').count() + 1
}

/// Why `FROM` named no single occurrence.  A miss is nearly always a mangled
/// `FROM`, so each arm names the mangling it can prove; several matches name the
/// lines, which is where the model has to widen.
fn no_unique_match(body: &str, from: &str, path: &str, starts: &[usize]) -> Break {
    if starts.len() > 1 {
        // Ascending offsets, so several matches on one line collapse next to
        // each other: name the line once rather than once per match.
        let mut lines: Vec<String> = Vec::new();
        for &at in starts {
            let line = line_of(body, at).to_string();
            if lines.last() != Some(&line) {
                lines.push(line);
            }
        }
        return sig(format!(
            "edit-replace: FROM matches {} times in {path}, on {}: {}; must match exactly \
             once. Widen FROM with a neighbouring line to make it unique, or change every \
             occurrence by composing string-replace / re-replace-all over from-string and \
             to-string.",
            starts.len(),
            ral_core::text::plural(lines.len(), "line"),
            lines.join(", ")
        ));
    }
    if has_suspicious_escapes(from) {
        return sig(format!(
            "edit-replace: FROM was not found in {path}, and FROM carries a literal backslash \
             escape: ral strings are verbatim, so \\n is a backslash and an n. Write real \
             newlines inside a raw #'…'# string."
        ));
    }
    if let Some(line) = unindented_match(body, from) {
        return sig(format!(
            "edit-replace: FROM was not found in {path}, but line {line} matches it apart from \
             leading whitespace: copy the exact text, indentation included, from \
             view-text-around."
        ));
    }
    sig(format!(
        "edit-replace: FROM was not found in {path}; is the file already what you intended? \
         Re-read it with view-text-around before editing."
    ))
}

/// The 1-based line equal to `from`'s first non-empty line once both are trimmed,
/// but indented differently — the slip a `FROM` copied by eye makes.  Equal
/// indentation is no near-miss: something further down `from` differs, and
/// blaming whitespace would misdirect.
fn unindented_match(body: &str, from: &str) -> Option<usize> {
    let raw = from.lines().find(|l| !l.trim().is_empty())?;
    let needle = raw.trim();
    body.lines()
        .position(|l| l.trim() == needle && l != raw)
        .map(|i| i + 1)
}

/// `explore-dir DEPTH` — list the cwd's tree (ignore-aware) to `DEPTH`, through
/// the one sanctioned walk site ([`cancellable`]).
fn builtin_explore_dir(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
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
        mooring.check()?;
        match result {
            Ok(entry) => {
                if entry.depth() == 0 {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or_else(|_| entry.path())
                    .to_string_lossy();
                if !readable(shell, &rel) {
                    continue;
                }
                results.push(Value::string(rel));
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
        thunk(fun(
            Ty::String,
            pure(Ty::list(closed_record(&[
                ("file", Ty::String),
                ("line", Ty::Int),
                ("text", Ty::String),
            ]))),
        )),
    )
}

/// `view-text :: Str → Int → Int → F [[line: Int, text: Str]]`
fn scheme_view_text(_u: &mut Unifier) -> Scheme {
    scheme_view_range(&[("line", Ty::Int), ("text", Ty::String)])
}

/// `view-hash :: Str → Int → Int → F [[line: Int, hash: Str, text: Str]]`
fn scheme_view_hash(_u: &mut Unifier) -> Scheme {
    scheme_view_range(&[
        ("line", Ty::Int),
        ("hash", Ty::String),
        ("text", Ty::String),
    ])
}

/// `Str → Int → Int → F [row]` — both readers' shape, over the row each carries.
fn scheme_view_range(row: &[(&str, Ty)]) -> Scheme {
    scheme(
        &[],
        &[],
        thunk(fun(
            Ty::String,
            fun(Ty::Int, fun(Ty::Int, pure(Ty::list(closed_record(row))))),
        )),
    )
}

/// `edit-hash :: Str → [[hash: Str, line: Str]] → F Unit`
fn scheme_edit_hash(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        thunk(fun(
            Ty::String,
            fun(
                Ty::list(closed_record(&[("hash", Ty::String), ("line", Ty::String)])),
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
        thunk(fun(
            Ty::String,
            fun(Ty::String, fun(Ty::String, pure(Ty::Unit))),
        )),
    )
}
const DEFAULT_LIMIT: usize = 50;

/// `fff QUERY` — frecency-ranked fuzzy file-name search over the working tree.
fn builtin_fff(args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let query = args[0].as_str("fff")?;
    let cwd = checked_read_path(shell, ".")?;
    let idx = fff_index::index_for(&cwd).map_err(sig)?;
    let paths = fff_index::search_paths(idx, query, DEFAULT_LIMIT).map_err(sig)?;
    let allowed = paths
        .into_iter()
        .filter(|rel| readable(shell, rel))
        .map(Value::string)
        .collect();
    Ok(Value::list(allowed))
}

fn scheme_explore_dir(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(fun(Ty::Int, pure(Ty::list(Ty::String)))))
}
fn scheme_fff(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(fun(Ty::String, pure(Ty::list(Ty::String)))))
}

fn scheme_skill(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(fun(Ty::String, pure(Ty::String))))
}

/// `skill NAME` — load a skill's full body, rescanning at each call so a skill
/// added or edited mid-session is found.
#[allow(
    clippy::unnecessary_wraps,
    reason = "installed as a `BuiltinBody::Static` fn pointer, whose signature fixes the `Settled` return; a skill that cannot be read answers with a message rather than raising."
)]
fn builtin_skill(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let name = args[0].as_str("skill")?;
    // Rejecting it here is what keeps `root.join(&name)` inside the skills root.
    if !skill::valid_skill_name(name) {
        return Settled::Ok(Value::string(format!("skill not found: {name}")));
    }
    let cwd = shell.cwd();
    let config_dir = crate::app::EXARCH.xdg_dir(ral_core::host::XdgKind::Config);
    for root in skill::skill_roots(&cwd, &config_dir) {
        let dir = root.join(name);
        let sk_md = dir.join("SKILL.md");
        let rp = shell.resolve(&sk_md.to_string_lossy());
        if shell.check_fs_read(&rp).is_ok() {
            // `check_fs_read` is a prefix guard, not an existence test: a path
            // under a readable prefix can still be a skill this root lacks.
            // Missing here is not a miss for the name — walk to the next root.
            if !sk_md.is_file() {
                continue;
            }
            let body = match skill::read_skill_body(&dir) {
                Ok(body) => body,
                Err(why) => {
                    return Settled::Ok(Value::string(format!(
                        "could not read skill {name}: {why}"
                    )));
                }
            };
            // Only once the body is in hand, so the card never claims a load
            // that did not happen.
            mooring.surface(&Value::map(vec![
                ("io".into(), Value::string("skill")),
                ("name".into(), Value::string(name)),
                ("dir".into(), Value::string(dir.to_string_lossy())),
            ]));
            return Settled::Ok(Value::string(format!(
                "// skill root: {}\n\n{}",
                dir.display(),
                body
            )));
        }
    }
    Settled::Ok(Value::string(format!("skill not found: {name}")))
}

fn scheme_skill_list(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(pure(Ty::String)))
}

/// `skill-list` — every discoverable skill, one `name: description` per line,
/// fresh-scanned and filtered by the live grant.
#[allow(
    clippy::unnecessary_wraps,
    reason = "registered as a `BuiltinBody::Static` fn pointer; the `Settled<Value>` return is the shape the builtin table dispatches through, not a choice of this body."
)]
fn builtin_skill_list(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let cwd = shell.cwd();
    let config_dir = crate::app::EXARCH.xdg_dir(ral_core::host::XdgKind::Config);
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
        ("io".into(), Value::string("skill-list")),
        ("count".into(), Value::Int(count)),
    ]));
    Settled::Ok(Value::string(out))
}

/// `service-handle :: ∀α. Int → F (Handle α)` — the per-call-site α instantiation
/// `race :: [Handle α] → …` already accepts.
fn scheme_service_handle(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    let a = Ty::Var(av);
    mk_plain_scheme(
        &[av],
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
fn builtin_service_handle(
    args: &[Value],
    site: &Arc<Site>,
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
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
            let mut handle = entry.handle;
            handle.site = Some(Arc::clone(site));
            Ok(Value::Handle(Box::new(handle)))
        }
        _ => Err(sig(format!(
            "service-handle: no durable service registered with id {}; an ephemeral \
             spawn/watch worker is not reacquired by id, only by the binding that named it",
            id.0
        ))),
    }
}

// A named array, not a promoted temporary: rustc refuses promotion once an
// entry carries `BuiltinEntry`'s interior-mutable arity cache.
static EXARCH_BUILTINS_ARR: [BuiltinEntry; 11] = [
    BuiltinEntry::new(
        Cow::Borrowed("exarch-surface"),
        ral_core::typecheck::builtins::scheme::surface_op,
        "exarch-surface <event>  — forward a tagged variant onto the rail: `card renders a card there, `pin`/`unpin` write straight to your register (exarch-pins is the tag-based way to reach those same slots), and any other tag is dropped, unrendered.",
        BuiltinBody::Static(ral_core::builtins::builtin_surface),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("view-text"),
        scheme_view_text,
        "view-text <path> <start> <end>  — show the half-open line range [start, end) of PATH as one record per line, [{line: Int, text: String}]. The text is verbatim, which is what `edit-replace` matches on: copy a row as it stands, indentation included.",
        BuiltinBody::Static(builtin_view_text),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("view-hash"),
        scheme_view_hash,
        "view-hash <path> <start> <end>  — the same range as `view-text`, each record carrying [{line: Int, hash: String, text: String}]. The hash is the witness `edit-hash` checks; copy it, never recompute it. Reads the whole file (the witness depends on file-wide uniqueness).",
        BuiltinBody::Static(builtin_view_hash),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("grep-files"),
        scheme_grep_files,
        "grep-files <pattern>  — recursively search the cwd (ignore-aware, Rust regex) in one read per matched file, giving [{file, line, text}].",
        BuiltinBody::Static(builtin_grep_files),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("edit-hash"),
        scheme_edit_hash,
        "edit-hash <path> <edits>  — apply a batch of [hash: HASH, line: TEXT] records in one read/write pass: each replaces the line whose witness is HASH with TEXT verbatim (a real newline inside '…' splits the line into several, \\n does not; empty deletes). Atomic; all hashes resolve against the file as read, so edits never interfere; fails writing nothing unless every hash picks exactly one line and no two records name the same one.",
        BuiltinBody::Static(builtin_edit_hash),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("edit-replace"),
        scheme_edit_replace,
        "edit-replace <path> <from> <to>  — read PATH, replace the one literal occurrence of FROM with TO, write the result back; errors leaving the file untouched on zero or several matches, naming the count and the lines. FROM and TO are verbatim: \\n is a backslash and an n, so write real newlines inside a raw #'…'# string, which may span lines; so may FROM. It never creates a file (`to-string BODY > path` does). For a target that repeats, compose string-replace / re-replace-all over from-string and to-string instead.",
        BuiltinBody::Static(builtin_edit_replace),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("explore-dir"),
        scheme_explore_dir,
        "explore-dir <n>  — list directory entries up to depth n respecting ignore files.",
        BuiltinBody::Static(builtin_explore_dir),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("skill-list"),
        scheme_skill_list,
        "skill-list  — list all available skills (fresh scan, filtered by grant). Returns one `name: description` per line.",
        BuiltinBody::Static(builtin_skill_list),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("skill"),
        scheme_skill,
        "skill <name>  — load the full SKILL.md body for the named skill (discovered from .exarch/skills/ and your config). Returns its Markdown instructions, or an error string if not found.",
        BuiltinBody::Static(builtin_skill),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("fff"),
        scheme_fff,
        "fff <query>  — fuzzy file-name search (frecency-ranked) over the working tree, returning [String].",
        BuiltinBody::Static(builtin_fff),
    ),
    BuiltinEntry::boundary(
        Cow::Borrowed("service-handle"),
        scheme_service_handle,
        "service-handle <id>  — re-acquire a durable service's live Handle by id (durable services only; an ephemeral spawn/watch id is refused). Compose with an eliminator: `await (service-handle 3)`, `cancel (service-handle 3)`.",
        builtin_service_handle,
    ),
];
pub static EXARCH_BUILTINS: &[BuiltinEntry] = &EXARCH_BUILTINS_ARR;

#[cfg(test)]
mod tests;
