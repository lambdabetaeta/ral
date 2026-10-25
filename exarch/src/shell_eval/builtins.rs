//! Exarch's agent search-and-edit builtins: static Rust atoms registered with
//! `ral-core` before any user or model source compiles — the resident agent
//! surface core itself should not own.

use crate::card::{Diff, encode_edit};
use crate::skill::{self, Judge as _};
use grep::matcher::LineTerminator;
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, SearcherBuilder, sinks::Lossy};
use ral_core::fact::{Grep, Read};
use ral_core::path::tree::walk::Walk;
use ral_core::path::{LeafMode, LexicalPath, Reading};
use ral_core::shell::builtins::util::regex_err;
use ral_core::shell::runtime::{Break, BuiltinBody, BuiltinEntry, Mooring, Observed, Settled, sig};
use ral_core::text::rank;
use ral_core::ty::{Scheme, Ty, closed_record};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{fun, mk_scheme as scheme, pure, thunk};
use ral_core::{HostSurface, Shell, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read as _;

pub(crate) mod harness;

/// The boot-recipe tag a `Frame::Attach` names to select [`host_surface`] as the
/// wire engine child's builtin surface; matched against the installer table in
/// `core/src/engine.rs`.
pub const INSTALLER_TAG: &str = "exarch-agent";

/// The agent host's builtin surface over core's `CORE_BUILTINS`: exarch's own
/// sets.  The prompt's builtin index ([`crate::prompt`]) reads the booted
/// shell's names back off this, so the two cannot drift.
pub fn host_surface() -> HostSurface {
    HostSurface {
        statics: vec![EXARCH_BUILTINS, harness::HARNESS_BUILTINS],
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

/// A text file's line ending.  `Crlf` only when every `\n` follows a `\r`; a
/// mixed file is `Lf`, its stray `\r`s kept as text.  So `encode` inverts
/// `decode` byte for byte, and an edit rewrites no line it did not touch.
#[derive(Clone, Copy)]
enum Eol {
    Lf,
    Crlf,
}

impl Eol {
    fn of(bytes: &[u8]) -> Self {
        let mut scan = EolScan::default();
        scan.feed(bytes);
        scan.eol()
    }

    const fn terminator(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }

    fn decode(self, body: String) -> String {
        match self {
            Self::Lf => body,
            Self::Crlf => body.replace("\r\n", "\n"),
        }
    }

    fn encode(self, text: &str) -> Cow<'_, str> {
        match self {
            Self::Lf => Cow::Borrowed(text),
            Self::Crlf => Cow::Owned(text.replace('\n', "\r\n")),
        }
    }
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

    let (_, body) = read_text_file(shell, path, tool)?;
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

/// [`Eol::of`] as a fold, for bytes that arrive in chunks.
#[derive(Default)]
struct EolScan {
    lf: bool,
    bare_lf: bool,
    last: Option<u8>,
}

impl EolScan {
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                self.lf = true;
                self.bare_lf |= self.last != Some(b'\r');
            }
            self.last = Some(b);
        }
    }

    const fn eol(&self) -> Eol {
        if self.lf && !self.bare_lf {
            Eol::Crlf
        } else {
            Eol::Lf
        }
    }
}

/// The longest line a search buffers; past it the file is skipped.
const LONGEST_LINE: usize = 16 << 20;

/// A line past [`LONGEST_LINE`], by its 1-based number.
#[derive(Debug)]
struct LongLine(u64);

impl std::fmt::Display for LongLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "line {} is longer than {} MiB; is the file minified or generated?",
            self.0,
            LONGEST_LINE >> 20
        )
    }
}

impl std::error::Error for LongLine {}

/// The line a stream is in, and how far into it.
struct LineRun {
    line: u64,
    run: usize,
}

impl Default for LineRun {
    fn default() -> Self {
        Self { line: 1, run: 0 }
    }
}

impl LineRun {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), LongLine> {
        let mut segments = bytes.split(|&b| b == b'\n');
        self.run += segments.next().map_or(0, <[u8]>::len);
        for segment in segments {
            self.check()?;
            self.line += 1;
            self.run = segment.len();
        }
        self.check()
    }

    const fn check(&self) -> Result<(), LongLine> {
        if self.run > LONGEST_LINE {
            Err(LongLine(self.line))
        } else {
            Ok(())
        }
    }
}

/// A file as [`search_tree`] streams it: each read first polls the shell's
/// scope, so a stop lands mid-file, and the bytes feed its [`EolScan`] and
/// [`LineRun`], whose limit is what bounds the searcher's buffer.
struct Scanned<'a> {
    file: std::fs::File,
    shell: &'a Shell,
    eol: EolScan,
    run: LineRun,
}

impl<'a> Scanned<'a> {
    fn new(file: std::fs::File, shell: &'a Shell) -> Self {
        Self {
            file,
            shell,
            eol: EolScan::default(),
            run: LineRun::default(),
        }
    }
}

impl std::io::Read for Scanned<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.shell.poll().is_err() {
            return Err(std::io::Error::other("stopped"));
        }
        let n = self.file.read(buf)?;
        self.eol.feed(&buf[..n]);
        self.run.feed(&buf[..n]).map_err(std::io::Error::other)?;
        Ok(n)
    }
}

/// One matching line from [`search_tree`], its file named as `view-text` takes it.
struct SearchHit {
    file: String,
    line: u64,
    text: String,
}

/// The builtin a search runs as: `hidden` is `grep-hidden`.
const fn grep_name(hidden: bool) -> &'static str {
    if hidden { "grep-hidden" } else { "grep-files" }
}

/// Search `path`, a file or a directory walked recursively (Rust regex),
/// streaming each file once.  Beneath `path` the walk skips hidden and
/// ignored entries, or with `hidden` only `.git`, and never leaves `path`'s
/// filesystem; `path` itself is always searched.  A file is read through the
/// guard on the object it names, binary detection quits at NUL, and a file
/// whose read fails midway is noted on stderr and skipped; this is the one
/// site both grep builtins compose over.  A hit names its file as its
/// admitted [`Walk`] does.
fn search_tree(
    shell: &mut Shell,
    hidden: bool,
    pattern: &str,
    path: &str,
) -> Settled<Vec<SearchHit>> {
    let name = grep_name(hidden);
    let matcher = RegexMatcherBuilder::new()
        .crlf(true)
        .build(pattern)
        .map_err(|e| sig(regex_err(name, pattern, &e.to_string())))?;
    let root = walk_root(shell, name, path)?;
    let tree = if hidden {
        Walk::all(&root).filter_entry(|p| p.as_path().file_name() != Some(".git".as_ref()))
    } else {
        Walk::respecting_ignores(&root)
    };
    let mut searcher = SearcherBuilder::new()
        .line_terminator(LineTerminator::crlf())
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();

    let mut results = Vec::new();
    let mut walk = shell.admitted_walk(tree.same_file_system(), name, path);
    while let Some(entry) = walk.next()? {
        if !entry.is_file() {
            continue;
        }
        let Some(file) = shell
            .locate_if_admitted::<Reading>(&entry.shown)
            .and_then(|located| located.read().ok())
        else {
            continue;
        };
        let mut scanned = Scanned::new(file, shell);
        let mut lines = Vec::new();
        let searched = searcher.search_reader(
            &matcher,
            &mut scanned,
            Lossy(|line, text| {
                lines.push((line, text.to_string()));
                Ok(true)
            }),
        );
        let eol = scanned.eol.eol();
        shell.poll()?;
        if let Err(e) = searched {
            let shown = ral_core::path::shown(entry.shown.as_path());
            let _ = shell.write_stderr(format!("{name}: {shown}: skipped: {e}\n").as_bytes());
            continue;
        }
        if !lines.is_empty() {
            // Cut by the file's own ending, so a hit is the row `view-text` shows.
            let term = eol.terminator();
            let file = walk.name(&entry)?;
            results.extend(lines.into_iter().map(|(line, mut text)| {
                if text.ends_with(term) {
                    text.truncate(text.len() - term.len());
                }
                SearchHit {
                    file: file.clone(),
                    line,
                    text,
                }
            }));
        }
    }
    Ok(results)
}

fn builtin_grep_files(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    grep(false, args, mooring, shell)
}

fn builtin_grep_hidden(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    grep(true, args, mooring, shell)
}

/// `grep-files`/`grep-hidden PATTERN PATH` — [`search_tree`] over `PATH`,
/// emitting exactly one `grep` surface for the whole walk rather than a card
/// per file read.
fn grep(hidden: bool, args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let pattern = args[0].as_str(grep_name(hidden))?;
    let path = args[1].as_str(grep_name(hidden))?;

    let grep = Observed::Grep(Grep {
        scope: path.to_string(),
        pattern: pattern.to_string(),
    });
    mooring.surface_data(&shell.observation(grep).to_surface());

    let results = search_tree(shell, hidden, pattern, path)?
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

/// Backslash letters that read as a C-style escape.  Only a FROM that failed
/// to match is checked: a TO is often source code whose escapes are meant.
const SUSPECT_ESCAPE_LETTERS: [char; 7] = ['n', 't', 'r', '0', '\\', '\'', '"'];

fn has_suspicious_escapes(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(1))
        .any(|i| bytes[i] == b'\\' && SUSPECT_ESCAPE_LETTERS.contains(&(bytes[i + 1] as char)))
}

/// Note a completed edit on stderr, kept apart from the `write` observation,
/// which stays the structured record of the commit.
fn note_edit(shell: &mut Shell, path: &str, lines: &str, plural: bool) {
    let word = if plural { "lines" } else { "line" };
    let _ = shell.write_stderr(format!("[EXARCH] Replaced {word} {lines} of {path}.\n").as_bytes());
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

    let (eol, body) = read_text_file(shell, path, "edit-hash")?;
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
    shell.atomic_write(path, eol.encode(&final_text).as_bytes())?;
    surface_edit(mooring, path, &body, &final_text);

    let mut line_nums: Vec<usize> = resolved.iter().map(|r| r.at + 1).collect();
    line_nums.sort_unstable();
    let lines = line_nums
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    note_edit(shell, path, &lines, line_nums.len() > 1);

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

/// The witness layer's shared read helper, gating on the live grant as a `< path`
/// redirect would but staying in Rust, below the redirect frame — so each caller
/// owns its own surface.  `tool` names the calling builtin in the error.  The
/// text comes back decoded through its [`Eol`], which the writer re-encodes.
fn read_text_file(shell: &mut Shell, path: &str, tool: &str) -> Settled<(Eol, String)> {
    let rp = shell.resolve(path);
    let located = shell.locate::<Reading>(&rp)?;
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
    let eol = Eol::of(&bytes);
    let body = String::from_utf8(bytes).map_err(|_| {
        sig(format!(
            "{tool}: '{path}' is not valid UTF-8; these tools read and edit text only."
        ))
    })?;
    Ok((eol, eol.decode(body)))
}

/// `edit-replace PATH FROM TO` — replace the one literal occurrence of `FROM`, so
/// 0 or >1 matches errors and leaves the file untouched.  Composed over the same
/// helpers as `edit-hash`: a silent read, then [`Shell::atomic_write`], surfacing
/// one whole-file diff.
fn builtin_edit_replace(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let path = args[0].as_str("edit-replace")?;
    let from = args[1].as_str("edit-replace")?;
    let to = args[2].as_str("edit-replace")?;
    if from.is_empty() {
        return Err(sig("edit-replace: FROM must be non-empty."));
    }
    let (eol, body) = read_text_file(shell, path, "edit-replace")?;
    let starts = ral_core::shell::builtins::strings::occurrence_starts(&body, from);
    let &[start] = starts.as_slice() else {
        return Err(no_unique_match(&body, from, path, &starts));
    };

    let final_text = body.replacen(from, to, 1);
    shell.atomic_write(path, eol.encode(&final_text).as_bytes())?;
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
    note_edit(shell, path, &lines, start_line != end_line);

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

/// `explore-dir DEPTH PATH` — list PATH's tree (ignore-aware) to `DEPTH`.
fn builtin_explore_dir(args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
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
    let path = args[1].as_str("explore-dir")?;
    let root = walk_root(shell, "explore-dir", path)?;
    let tree = Walk::respecting_ignores(&root).max_depth(Some(depth));
    let mut walk = shell.admitted_walk(tree, "explore-dir", path);
    let mut entries = Vec::new();
    while let Some(entry) = walk.next()? {
        if entry.depth > 0 {
            entries.push(Value::string(walk.name(&entry)?));
        }
    }
    Ok(Value::list(entries))
}

/// `find-files`' cut, a literal so the help text can quote it.
macro_rules! find_limit {
    () => {
        50
    };
}
const FIND_LIMIT: usize = find_limit!();

/// `find-files QUERY PATH` — the files under PATH, walked as `grep-files`
/// walks, ranked by path against QUERY.  The grant filters before the ranking
/// cuts, so a denied match neither takes a slot nor shows in the count.
fn builtin_find_files(args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let query = args[0].as_str("find-files")?;
    let path = args[1].as_str("find-files")?;
    let root = walk_root(shell, "find-files", path)?;
    let mut walk = shell.admitted_walk(Walk::respecting_ignores(&root), "find-files", path);
    let mut files = Vec::new();
    while let Some(entry) = walk.next()? {
        if entry.is_file() {
            files.push(walk.name(&entry)?);
        }
    }
    let best = rank(query, files).into_iter().take(FIND_LIMIT);
    Ok(Value::list(best.map(Value::string).collect()))
}

/// The grant-checked root of a walk; a missing one is an error, not an empty walk.
fn walk_root(shell: &mut Shell, who: &str, path: &str) -> Settled<LexicalPath> {
    let root = shell.resolve(path);
    if shell
        .locate_existing::<Reading>(&root, LeafMode::Follow)?
        .is_none()
    {
        return Err(sig(format!(
            "{who}: {path} does not exist; the path is a file or directory, relative to the cwd or absolute."
        )));
    }
    Ok(root)
}

fn scheme_grep_files(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        thunk(fun(
            Ty::String,
            fun(
                Ty::String,
                pure(Ty::list(closed_record(&[
                    ("file", Ty::String),
                    ("line", Ty::Int),
                    ("text", Ty::String),
                ]))),
            ),
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
fn scheme_explore_dir(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        thunk(fun(Ty::Int, fun(Ty::String, pure(Ty::list(Ty::String))))),
    )
}
fn scheme_find_files(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        thunk(fun(Ty::String, fun(Ty::String, pure(Ty::list(Ty::String))))),
    )
}

fn scheme_skill(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(fun(Ty::String, pure(Ty::String))))
}

/// `skill NAME` — load a skill's full body, rescanning at each call so a skill
/// added or edited mid-session is found.
fn builtin_skill(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let name = args[0].as_str("skill")?;
    // Rejecting it here is what keeps `root.join(&name)` inside the skills root.
    if !skill::valid_skill_name(name) {
        return Err(sig(format!(
            "skill: '{name}' is not a skill name; names are lowercase letters, digits and single hyphens"
        )));
    }
    let cwd = shell.cwd();
    let config_dir = crate::app::EXARCH.xdg_dir(ral_core::path::XdgKind::Config);
    for root in skill::skill_roots(&cwd, config_dir.as_deref()) {
        let dir = root.join(name);
        let sk_md = dir.join("SKILL.md");
        // A root that lacks the skill, or may not be read, is not a miss for
        // the name: walk to the next.
        let Some(file) = shell.file(&shell.anchor(&sk_md)) else {
            continue;
        };
        let found = skill::Found {
            name: name.to_owned(),
            dir,
            file,
        };
        let body = skill::read_skill_body(&found).map_err(|why| {
            sig(format!(
                "skill: could not load '{name}' from {}: {why}",
                ral_core::path::shown(&sk_md)
            ))
        })?;
        // Only once the body is in hand, so the card never claims a load
        // that did not happen.
        let dir = ral_core::path::shown(&found.dir);
        mooring.surface(&Value::map(vec![
            ("io".into(), Value::string("skill")),
            ("name".into(), Value::string(name)),
            ("dir".into(), Value::string(dir.clone())),
        ]));
        return Ok(Value::string(format!("// skill root: {dir}\n\n{body}")));
    }
    Err(sig(format!(
        "skill: no skill named '{name}'; did you mean one that `skill-list` shows?"
    )))
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
    let config_dir = crate::app::EXARCH.xdg_dir(ral_core::path::XdgKind::Config);
    let (mut entries, mut errors) = (Vec::new(), Vec::new());
    for found in skill::discover_all(&cwd, config_dir.as_deref(), shell) {
        match skill::parse_skill(&found) {
            Ok(s) => entries.push(format!("{}: {}", s.name, s.description)),
            Err(why) => errors.push(format!(
                "⚠ skipped {}: {why}",
                ral_core::path::shown(found.dir.join("SKILL.md"))
            )),
        }
    }
    mooring.surface(&Value::map(vec![
        ("io".into(), Value::string("skill-list")),
        (
            "count".into(),
            Value::Int(i64::try_from(entries.len()).unwrap_or(i64::MAX)),
        ),
    ]));
    let sections: Vec<_> = [entries, errors]
        .iter()
        .filter(|v| !v.is_empty())
        .map(|v| v.join("\n"))
        .collect();
    Settled::Ok(Value::string(sections.join("\n\n")))
}

// A named array, not a promoted temporary: rustc refuses promotion once an
// entry carries `BuiltinEntry`'s interior-mutable arity cache.
static EXARCH_BUILTINS_ARR: [BuiltinEntry; 11] = [
    BuiltinEntry::new(
        Cow::Borrowed("exarch-surface"),
        ral_core::typecheck::builtins::scheme::surface_op,
        "exarch-surface <event>  — forward a tagged variant onto the rail: `card renders a card there, `pin`/`unpin` write straight to your register (exarch-pins is the tag-based way to reach those same slots), and any other tag is dropped, unrendered.",
        BuiltinBody::Static(ral_core::shell::builtins::builtin_surface),
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
        "grep-files <pattern> <path>  — search PATH, a file or a directory walked recursively (Rust regex), giving [{file, line, text}]; `file` is named as view-text takes it. The walk skips hidden and ignored entries beneath PATH, never PATH itself: name one to search it. It stays on PATH's filesystem, so `/proc` and other mounts beneath PATH are not searched.",
        BuiltinBody::Static(builtin_grep_files),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("grep-hidden"),
        scheme_grep_files,
        "grep-hidden <pattern> <path>  — `grep-files`, whose walk enters every entry beneath PATH, hidden (`.env`, `.github`) and gitignored alike, except `.git`.",
        BuiltinBody::Static(builtin_grep_hidden),
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
        "explore-dir <n> <path>  — the entries beneath PATH up to depth n, as [String], each named as view-text takes it. The walk skips hidden and ignored entries beneath PATH, as grep-files does.",
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
        "skill <name>  — load the full SKILL.md body for the named skill (discovered from .exarch/skills/ and your config). Returns its Markdown instructions; fails on a name no skill bears.",
        BuiltinBody::Static(builtin_skill),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("find-files"),
        scheme_find_files,
        concat!(
            "find-files <query> <path>  — the files under PATH, a file or a directory, fuzzy-ranked by path against QUERY, best first, at most ",
            find_limit!(),
            ", as [String]; each is named as view-text takes it. The walk skips hidden and ignored entries beneath PATH, as grep-files does."
        ),
        BuiltinBody::Static(builtin_find_files),
    ),
];
pub static EXARCH_BUILTINS: &[BuiltinEntry] = &EXARCH_BUILTINS_ARR;

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
