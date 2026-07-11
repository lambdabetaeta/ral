//! The render document the `surface` builtin carries.
//!
//! A [`Card`] is an ordered stack of Bertin *marks* the kit composes
//! entirely in ral; exarch decodes it once here ([`value_to_card`]) into
//! this closed Rust model and renders it through one generic interpreter
//! (`tui::line::render_card`).  The *set of cards* is open — compose marks
//! in ral, zero Rust per card — while the *set of marks* stays closed and
//! small, so the renderer is total and reflow / disclosure / aggregation /
//! the rendered `user.log` all keep working.
//!
//! The discipline is Bertin's: the kit declares **data and its level of
//! measurement, never its appearance**.  A [`Span`] carries a nominal
//! [`Role`] (identity → hue/shape); a [`Measure`] or [`Mark::Diff`] carries
//! a magnitude (ordered → size/value/grain).  The one binding table lives
//! in the renderer, so the kit *cannot* put magnitude on hue: the encoding
//! is correct by construction.  See
//! `docs/ral-wiki/decisions/260619_surface-carries-documents.md`.

use ral_core::Value as RalValue;
use serde::Serialize;
use std::borrow::Cow;
use std::fmt::Write;

/// The closed nominal role set — the *selective* (identity) channel a
/// [`Span`] may carry.
///
/// The renderer holds the one binding table mapping
/// each role to a hue/shape; the kit names a role, never a colour, so
/// identity can never masquerade as a magnitude.  An unknown role tag
/// renders as plain ink rather than dropping.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Path,
    Code,
    Ok,
    Warn,
    Bad,
    Muted,
    Strong,
}

impl Role {
    /// Parse a nominal role tag; `None` for an unrecognised role.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "path" => Self::Path,
            "code" => Self::Code,
            "ok" => Self::Ok,
            "warn" => Self::Warn,
            "bad" => Self::Bad,
            "muted" => Self::Muted,
            "strong" => Self::Strong,
            _ => return None,
        })
    }
}

/// A run of text optionally carrying a nominal [`Role`].
///
/// A heading is
/// just a `Strong` span; a path is a `Path` span.  A span never carries a
/// magnitude — that is the job of [`Measure`] and [`Mark::Diff`].
#[derive(Clone, Debug, Serialize)]
pub struct Span {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    pub text: String,
}

/// The quantitative mark `[label, value, max?, unit?]`, rendered with the
/// two ordered Bertin variables — size (a bar) and value (lightness).
///
/// A
/// bounded magnitude (`max` present) reads as a proportional fill (the old
/// progress meter); an unbounded one (`max` absent) reads as a `log2` size
/// bar (the old header size-bar).  Both apply the value ramp, so a larger
/// magnitude reads brighter as well as fuller.
#[derive(Clone, Debug, Serialize)]
pub struct Measure {
    pub label: String,
    pub value: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// A value in a [`Mark::Fields`] row's shared value column: a run of inline
/// spans (`text`) or a nested [`Measure`] — the one composability rule
/// (marks nest in a field's value) at the field scale.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldVal {
    Inline(Vec<Span>),
    Measure(Measure),
}

impl FieldVal {
    /// The value's plain text: inline spans concatenated, or a measure's
    /// `value[/max][unit]` readout.  Shared by the [`summary_line`] rail
    /// summary and the headless stderr condenser.
    pub fn plain(&self) -> String {
        match self {
            Self::Inline(spans) => spans.iter().map(|s| s.text.as_str()).collect(),
            Self::Measure(m) => {
                let bound = m.max.map(|mx| format!("/{mx}")).unwrap_or_default();
                format!("{}{bound}{}", m.value, m.unit.as_deref().unwrap_or(""))
            }
        }
    }
}

/// One `(label, value)` row of a [`Mark::Fields`] matrix — Bertin's
/// selective alignment in miniature, every value landing in one shared
/// label column.
#[derive(Clone, Debug, Serialize)]
pub struct Field {
    pub label: String,
    pub value: FieldVal,
}

/// One Bertin mark on the plane.  Closed and small so the renderer is
/// total; stacked openly into a [`Card`] in ral.
///
/// - [`Mark::Text`] — the *qualitative* mark: a run of optionally-roled spans.
/// - [`Mark::Measure`] — the *quantitative* mark (size + value).
/// - [`Mark::Fields`] — the *matrix* mark: an aligned `(label, value)` table.
/// - [`Mark::Diff`] — the *dense composite* mark (size + grain + value + shape).
/// - [`Mark::Listing`] — a *numbered source listing*: the head of a written
///   file, gutter-numbered and syntax-lit but one-sided (no `+`/`-`).
/// - [`Mark::Raw`] — *un-encoded ink*: pre-formed bytes appended verbatim.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "mark", rename_all = "snake_case")]
pub enum Mark {
    Text { spans: Vec<Span> },
    Measure(Measure),
    Fields { rows: Vec<Field> },
    Diff { path: String, hunks: Vec<Hunk> },
    Listing { bytes: Vec<u8>, more: bool },
    Raw { bytes: Vec<u8> },
}

/// An ordered stack of [`Mark`]s rendered top-to-bottom on one scrollback
/// block — the render document `surface` carries, composed in ral.
#[derive(Clone, Debug, Serialize)]
pub struct Card(pub Vec<Mark>);

impl Card {
    /// The card's marks, in plane order.
    pub fn marks(&self) -> &[Mark] {
        &self.0
    }

    /// True when the card carries at least one [`Mark::Diff`] — the only
    /// content that earns graded disclosure (L1 header ↔ L3 full).  A card
    /// of only `text`/`fields`/`measure`/`raw` is chrome-level (L3-only).
    pub fn has_diff(&self) -> bool {
        self.0.iter().any(|m| matches!(m, Mark::Diff { .. }))
    }

    /// The card's magnitude — total changed lines summed across its `diff`
    /// marks, or `None` when it carries no diff.  The rail's value-step and
    /// the matrix's per-agent size readout both read this.
    pub fn magnitude(&self) -> Option<u32> {
        let mut any = false;
        let total = self
            .0
            .iter()
            .filter_map(|m| match m {
                Mark::Diff { hunks, .. } => {
                    any = true;
                    Some(hunk_magnitude(hunks))
                }
                _ => None,
            })
            .sum();
        any.then_some(total)
    }

    /// When the card is exactly one `diff` mark, its `(path, hunks)` — the
    /// aggregation key consecutive same-path diff cards merge on, mirroring
    /// a unified diff's single per-file block.  `None` for any richer card.
    pub fn single_diff(&self) -> Option<(&str, &[Hunk])> {
        match self.0.as_slice() {
            [Mark::Diff { path, hunks }] => Some((path, hunks)),
            _ => None,
        }
    }

    /// Consume a single-`diff` card into its owned `(path, hunks)` for the
    /// patch-aggregation buffer; `Err(self)` hands a richer card back
    /// untouched so the caller can push it as its own block.
    ///
    /// # Errors
    /// Returns `Err(self)` if the card is not exactly one `diff` mark.
    pub fn into_single_diff(self) -> Result<(String, Vec<Hunk>), Self> {
        if self.single_diff().is_some() {
            let Self(mut marks) = self;
            match marks.pop() {
                Some(Mark::Diff { path, hunks }) => Ok((path, hunks)),
                _ => unreachable!("single_diff checked exactly one diff mark"),
            }
        } else {
            Err(self)
        }
    }
}

/// Total changed lines (deletions + additions) across `hunks` — the diff
/// magnitude, shared by [`Card::magnitude`] and the renderer's size-bar.
/// Context rows are unchanged, so they do not count.
pub fn hunk_magnitude(hunks: &[Hunk]) -> u32 {
    #[allow(clippy::cast_possible_truncation, reason="changed-line count cannot approach u32::MAX")]
    let n = hunks
        .iter()
        .flat_map(|h| h.rows.iter())
        .filter(|r| matches!(r, Row::Del(_) | Row::Add(_)))
        .count() as u32;
    n
}

/// One grouped hunk of a whole-file diff, carried by a [`Mark::Diff`].
///
/// A flat unified list of [`Row`]s — context,
/// deletions, and insertions interleaved exactly as `similar`'s grouped ops
/// yield them.
/// `start` is the 1-indexed original line of the hunk's first
/// row; the sink walks the rows from there, advancing an old- and a
/// new-side counter — a `Context` advances both, a `Del` advances the old
/// counter (and keeps its pre-edit number), an `Add` advances the new
/// counter (and takes its post-edit number).
#[derive(Clone, Debug, Serialize)]
pub struct Hunk {
    pub start: u32,
    pub rows: Vec<Row>,
}

/// One run of a diff row's text: a contiguous slice flagged `emph` when it is
/// the part that actually changed against the row's paired line — the
/// intra-line word diff `similar` computes.
///
/// A context row, and the unchanged
/// stretches that surround a change on a del/add row, carry `emph: false`.
#[derive(Clone, Debug, Serialize)]
pub struct Seg {
    pub emph: bool,
    pub text: String,
}

impl Seg {
    /// A whole, unemphasised run — the shape a context row carries and the
    /// default a plainly-constructed del/add row falls back to.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            emph: false,
            text: text.into(),
        }
    }
}

/// One row of a [`Hunk`]'s unified line list: unchanged context, a removed
/// line, or an inserted line.
///
/// Each carries its text as a run of [`Seg`]ments
/// so a del/add can mark the words that changed against its paired line; a
/// context row is a single unemphasised segment.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "tag", content = "segs", rename_all = "snake_case")]
pub enum Row {
    Context(Vec<Seg>),
    Del(Vec<Seg>),
    Add(Vec<Seg>),
}

impl Row {
    /// The row's segments, whatever its kind.
    pub fn segs(&self) -> &[Seg] {
        match self {
            Self::Context(s) | Self::Del(s) | Self::Add(s) => s,
        }
    }

    /// The row's full text — its segments concatenated, dropping the
    /// inline-emphasis distinction (the plain-text/headless rendering).
    pub fn text(&self) -> String {
        self.segs().iter().map(|s| s.text.as_str()).collect()
    }
}

/// Compute the whole-file line-level diff of `old` vs `new`, grouped into
/// hunks with ±2 lines of context.  Each hunk's `start` is the 1-indexed
/// original line of its first row, and its rows are the unified context /
/// deletion / insertion list `similar` yields.  Shared by every diff-card
/// producer: `edit-hash`/`edit-replace` (`agent_builtins.rs`) feed it through a
/// `` `diff `` value the model-facing `surface` builtin forwards; a
/// committed `>` redirect that overwrote an existing file (the write-card
/// preview below) calls it directly, with no `Value` round-trip, since it
/// already sits in the rendering layer.
pub(crate) fn whole_file_hunks(old: &str, new: &str) -> Vec<Hunk> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(2) {
        let first = group.first().expect("grouped_ops yields non-empty groups");
        #[allow(clippy::cast_possible_truncation, reason="diff line index cannot approach u32::MAX")]
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

// ── I/O events: structural shapes core emits onto the `surface` sink ─────────

/// How a write reached the file: a one-shot `write`, an `append`, or a
/// `stream` of bytes.  Nominal, like a [`Role`] — the card maps it to text,
/// never to appearance.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    Write,
    Append,
    Stream,
}

impl WriteMode {
    /// Parse a write-mode tag; `None` for an unrecognised mode.
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "write" => Self::Write,
            "append" => Self::Append,
            "stream" => Self::Stream,
            _ => return None,
        })
    }
}

/// How a write settled: `committed` to disk, `aborted` before commit, or
/// `failed`.  The card roles the outcome span by this (committed→`Ok`,
/// aborted→`Warn`, failed→`Bad`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteOutcome {
    Committed,
    Aborted,
    Failed,
}

impl WriteOutcome {
    /// Parse a write-outcome tag; `None` for an unrecognised outcome.
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "committed" => Self::Committed,
            "aborted" => Self::Aborted,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    /// The word shown in a write card's outcome span, and its nominal role.
    fn label(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }

    fn role(self) -> Role {
        match self {
            Self::Committed => Role::Ok,
            Self::Aborted => Role::Warn,
            Self::Failed => Role::Bad,
        }
    }
}

/// Whether an exec ran cleanly (`ok`) or not (`bad`).
///
/// The exec card pairs
/// this with the numeric status; the status span is roled by the status code
/// (0→`Ok`, nonzero→`Bad`), so the outcome tag is the structural twin of that
/// readout in the recorded event.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutcome {
    Ok,
    Bad,
}

impl ExecOutcome {
    /// Parse an exec-outcome tag; `None` for an unrecognised outcome.
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => Self::Ok,
            "bad" => Self::Bad,
            _ => return None,
        })
    }
}

/// A structural I/O event core surfaces onto the `surface` sink: a read, a
/// write, an exec, or a grep.
///
/// Unlike a [`Card`] (which the kit composes in
/// ral and exarch only renders), an `IoEvent` is the raw effect record —
/// `surface` decodes it once ([`value_to_io`]) and composes the matching card
/// ([`io_card`]).  Both ride the bus together (`Kind::Io`) so the recorded
/// event keeps the structure the rendered mark tree erases.
#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
#[serde(tag = "io", rename_all = "snake_case")]
pub enum IoEvent {
    Read {
        path: String,
    },
    Write {
        path: String,
        mode: WriteMode,
        outcome: WriteOutcome,
        // A bounded prefix of the committed content (the host caps the read),
        // input to the write card's preview only.  `#[serde(skip)]`: it never
        // reaches `events.json` — the forensic log keeps the write's structural
        // shape (path / mode / outcome); the rendered preview lives only in the
        // TUI's `user.log`.
        #[serde(skip)]
        new_bytes: Option<Vec<u8>>,
        // The pre-existing target's whole content, present only when the
        // write was atomic, overwrote an existing file, and neither side
        // exceeded core's read cap — input to the write card's diff-vs-
        // preview choice only.  Same `#[serde(skip)]` reasoning as `new_bytes`.
        #[serde(skip)]
        old_bytes: Option<Vec<u8>>,
    },
    Exec {
        argv: Vec<String>,
        outcome: ExecOutcome,
        status: i64,
    },
    Grep {
        scope: String,
        pattern: String,
    },
}

/// Which `|>` effect a surfaced observation is — the census bucket it counts
/// toward when a coalesced run reduces to its tally (the L0 census in
/// [`super::tui`]).
///
/// Reads, execs, and greps fold into a run and tally here; a
/// write is a barrier that ends a run, so it is not an observation kind — it is
/// tracked by its card origin instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObservationKind {
    Read,
    Exec,
    Grep,
}

/// Decode a runtime `Value` core surfaced into a structural [`IoEvent`].
///
/// An io event is a `Map` whose `io` field names one of `read`/`write`/`exec`/
/// `grep` — the contract core emits.  Anything else (a `` `card `` variant, a
/// plain string, a map without a recognised `io` tag) returns `None`, the same
/// graceful degradation as [`value_to_card`]; the sink then falls through to
/// the card decoder.
pub fn value_to_io(v: &RalValue) -> Option<IoEvent> {
    let m = map_of(v)?;
    Some(match str_field(m, "io")?.as_str() {
        "read" => IoEvent::Read {
            path: str_field(m, "path")?,
        },
        "write" => IoEvent::Write {
            path: str_field(m, "path")?,
            mode: WriteMode::parse(&str_field(m, "mode")?)?,
            outcome: WriteOutcome::parse(&str_field(m, "outcome")?)?,
            new_bytes: bytes_field(m, "new_bytes"),
            old_bytes: bytes_field(m, "old_bytes"),
        },
        "exec" => IoEvent::Exec {
            argv: strings_field(m, "argv"),
            outcome: ExecOutcome::parse(&str_field(m, "outcome")?)?,
            status: int_field(m, "status")?,
        },
        "grep" => IoEvent::Grep {
            scope: str_field(m, "scope")?,
            pattern: str_field(m, "pattern")?,
        },
        _ => return None,
    })
}

/// Compose an [`IoEvent`] into a [`Card`].
///
/// The heading is one [`Mark::Text`]
/// of roled spans: a dim verb naming the operation (a nominal category, carried
/// by a word rather than a mirror-orientation glyph) followed by the path or
/// program as the subject — lifted by [`Role::Path`]'s hue against the muted
/// label — and the outcome roled by its level.  A committed write appends a
/// [`write_preview`] below that heading: a diff against the prior content
/// when there was one to diff against, otherwise a plain listing of what it
/// wrote.
pub fn io_card(event: &IoEvent) -> Card {
    let mut body: Vec<Mark> = Vec::new();
    let spans = match event {
        IoEvent::Read { path } => read_spans(path),
        IoEvent::Write {
            path,
            outcome,
            new_bytes,
            old_bytes,
            ..
        } => {
            if *outcome == WriteOutcome::Committed {
                body.extend(write_preview(path, old_bytes.as_deref(), new_bytes.as_deref()));
            }
            write_spans(path, *outcome)
        }
        IoEvent::Exec {
            argv,
            outcome: _,
            status,
        } => {
            let mut spans = vec![span_plain("$ ")];
            spans.extend(exec_cmd_spans(argv));
            let role = if *status == 0 { Role::Ok } else { Role::Bad };
            spans.push(span_plain(" → "));
            spans.push(span(role, &status.to_string()));
            spans
        }
        IoEvent::Grep { scope, pattern } => {
            let mut spans = vec![span(Role::Muted, "grep ")];
            spans.extend(grep_spans(scope, pattern));
            spans
        }
    };
    let mut marks = vec![Mark::Text { spans }];
    marks.extend(body);
    Card(marks)
}

/// The command of an exec, *without* its `$ ` prefix or `→ status` tail —
/// the program as a [`Role::Path`] span and each arg as plain ink (a missing
/// command degrades to plain ink).  Shared by [`io_card`] (which frames it
/// with the prompt and status) and [`observation_group_card`] (which comma-joins
/// several, dropping the per-event status — see its docs).
///
/// The surfaced `argv` is post-shell — word-split, quotes already consumed —
/// so each token is re-quoted by [`shlex::try_quote`] *only* where the shell
/// would otherwise reparse it.  A clean token rides bare (`ls README.md`); one
/// carrying a space, a glob, or other shell metacharacter is re-wrapped, so
/// the line round-trips back to a runnable command rather than a lie the shell
/// would word-split differently.  `try_quote`'s sole error is an interior nul,
/// which no real argv carries, so that degrades to the raw token.  Tokens longer
/// than 80 chars are truncated with `…` before quoting to keep the rail legible.
fn exec_cmd_spans(argv: &[String]) -> Vec<Span> {
    let quote = |t: &str| {
        const CAP: usize = 80;
        let s = if t.chars().count() > CAP {
            format!("{}…", t.chars().take(CAP - 1).collect::<String>())
        } else {
            t.to_string()
        };
        shlex::try_quote(&s).map_or_else(|_| s.clone(), Cow::into_owned)
    };
    match argv.split_first() {
        Some((prog, args)) => {
            let mut spans = vec![span(Role::Path, &quote(prog))];
            for arg in args {
                spans.push(span_plain(&format!(" {}", quote(arg))));
            }
            spans
        }
        None => vec![span_plain("(no command)")],
    }
}

/// A grep's `pattern in scope` — the pattern as [`Role::Code`], the scope as
/// [`Role::Path`] — *without* the leading `grep ` verb, so the group head can
/// carry one shared verb over a comma-joined run.
fn grep_spans(scope: &str, pattern: &str) -> Vec<Span> {
    vec![
        span(Role::Code, pattern),
        span_plain(" in "),
        span(Role::Path, scope),
    ]
}

/// A read row: the muted verb `read`, then the path as the subject.  Reused
/// verbatim per entry in [`observation_group_card`]'s comma-joined read run, so a lone
/// read and a grouped one share one shape.
fn read_spans(path: &str) -> Vec<Span> {
    vec![span(Role::Muted, "read "), span(Role::Path, path)]
}

/// A write row: the muted verb `write`, the path as the subject, then the
/// outcome roled by how it settled (`committed`→`Ok`, `aborted`→`Warn`,
/// `failed`→`Bad`).  Every write reads the same `write <path> <outcome>`,
/// whatever its mode — the mode rides the recorded event, but the surface
/// names only the act and how it landed.  The heading line of a write card
/// ([`io_card`]); a committed write previews its content below via
/// [`write_preview`].
fn write_spans(path: &str, outcome: WriteOutcome) -> Vec<Span> {
    vec![
        span(Role::Muted, "write "),
        span(Role::Path, path),
        span_plain(" "),
        span(outcome.role(), outcome.label()),
    ]
}

/// The number of leading lines a write card previews of the file it wrote,
/// when it falls back to a listing rather than a diff.
const WRITE_PREVIEW_LINES: usize = 10;

/// Preview a committed write: a whole-file [`Mark::Diff`] against the prior
/// content when `old` is present (core supplies it only for an atomic write
/// that overwrote an existing file with neither side exceeding its read cap)
/// and both sides are valid UTF-8 — the same diff `edit-hash`/`edit-replace` surface
/// explicitly, here computed directly from the two snapshots since this
/// already sits in the rendering layer.  Otherwise, the first
/// [`WRITE_PREVIEW_LINES`] lines of `new` as one [`Mark::Listing`], `more` set
/// when content continues past them — a plain preview of *what was written*,
/// for a new file or content this can't safely diff (binary, or too large on
/// either side).  Absent or empty `new` yields no marks, so the
/// `write <path> <outcome>` heading stands alone (a zero-byte write).
fn write_preview(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> Vec<Mark> {
    let new = match new {
        Some(b) if !b.is_empty() => b,
        _ => return Vec::new(),
    };
    if let Some(old) = old
        && let (Ok(old_text), Ok(new_text)) = (std::str::from_utf8(old), std::str::from_utf8(new))
    {
        let hunks = whole_file_hunks(old_text, new_text);
        if !hunks.is_empty() {
            return vec![Mark::Diff {
                path: path.to_string(),
                hunks,
            }];
        }
    }
    let text = String::from_utf8_lossy(new);
    let mut lines = text.lines();
    let head: Vec<&str> = lines.by_ref().take(WRITE_PREVIEW_LINES).collect();
    if head.is_empty() {
        return Vec::new();
    }
    vec![Mark::Listing {
        bytes: head.join("\n").into_bytes(),
        more: lines.next().is_some(),
    }]
}

/// Compose a run of buffered observation surfaces — even interleaved, grouped
/// by the TUI into per-kind buckets — into one [`Card`] *per non-empty kind*,
/// in a fixed Read → Exec → Grep order.
///
/// Each card is a single [`Mark::Text`]
/// reusing the exact `io_card` span vocabulary, so hues match; a lone surface
/// (a group of one) renders identically to its `io_card`, modulo the deliberate
/// exec departure below — no special case.  Writes never reach here: a write is
/// a barrier rendered standalone as its own card (header + content preview).
///
/// The exec group **drops the `→ status` tail** that single `io_card` exec
/// rows carry: a comma-joined run of commands reads as the *set of commands
/// run* (`$ wc -l, grep -rn, git status`), and a per-command status would be
/// per-event noise on that line.  The status is not lost — it rides the bus
/// in each `Kind::Io`'s structured event and reaches the transcript via
/// `transcript::event_record`; only this grouped *presentation* omits it.
pub fn observation_group_card(reads: &[String], execs: &[IoEvent], greps: &[IoEvent]) -> Vec<Card> {
    let mut cards = Vec::new();
    // Read: `read p1, read p2, …` — each entry the verb + path, comma-joined.
    if !reads.is_empty() {
        let mut spans = Vec::new();
        join_spans(&mut spans, reads, |spans, path| {
            spans.extend(read_spans(path));
        });
        cards.push(Card(vec![Mark::Text { spans }]));
    }
    // Exec: `$ cmd1, cmd2, …` — one prompt, the commands comma-joined, each
    // dropping its status tail (see the doc comment above).
    if !execs.is_empty() {
        let mut spans = vec![span_plain("$ ")];
        join_spans(&mut spans, execs, |spans, e| {
            if let IoEvent::Exec { argv, .. } = e {
                spans.extend(exec_cmd_spans(argv));
            }
        });
        cards.push(Card(vec![Mark::Text { spans }]));
    }
    // Grep: `grep p1 in s1, p2 in s2, …` — one verb, the `pattern in scope`
    // entries comma-joined.
    if !greps.is_empty() {
        let mut spans = vec![span(Role::Muted, "grep ")];
        join_spans(&mut spans, greps, |spans, e| {
            if let IoEvent::Grep { scope, pattern } = e {
                spans.extend(grep_spans(scope, pattern));
            }
        });
        cards.push(Card(vec![Mark::Text { spans }]));
    }
    cards
}

/// Append each of `items` to `spans` via `each`, separating entries with a
/// plain `", "` — the comma-join shared by every [`observation_group_card`] bucket.
fn join_spans<T>(spans: &mut Vec<Span>, items: &[T], each: impl Fn(&mut Vec<Span>, &T)) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            spans.push(span_plain(", "));
        }
        each(spans, item);
    }
}

/// A roled span carrying `text`.
fn span(role: Role, text: &str) -> Span {
    Span {
        role: Some(role),
        text: text.to_string(),
    }
}

/// A roleless (plain-ink) span carrying `text`.
fn span_plain(text: &str) -> Span {
    Span {
        role: None,
        text: text.to_string(),
    }
}

// ── `done`: the completion event a detached worker flushes at the boundary ───

/// How a detached `spawn` worker settled, as the final `` `done `` event core
/// appends to the worker's deferred buffer at completion.
///
/// Like an [`IoEvent`]
/// it is the raw record — [`value_to_done`] decodes it once and [`done_card`]
/// composes the matching one-line card.  Core names the event (`` `ok ``,
/// `` `err ``, `` `panic ``); exarch names its appearance: a fixed-position
/// outcome mark roled by result, never an animation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DoneOutcome {
    /// The worker returned cleanly.
    Ok,
    /// The worker raised — the caught error's `message` and `status`, the same
    /// fields `try`/`poll` surface.
    Err { message: String, status: i64 },
    /// The worker panicked — the panic message.
    Panic { message: String },
}

/// Decode the `` `done `` value a detached worker flushes at completion into
/// its [`DoneOutcome`].
///
/// The shape is `` `done [cmd: "…", outcome: …] `` where
/// `outcome` is the closed `` `ok ``/`` `err ``/`` `panic `` variant core mints;
/// `err` carries the `{cmd, status, message, line, col}` error record.  The
/// `cmd` field is no longer surfaced — the card names the worker generically —
/// so only `outcome` is read.  Anything else returns `None`, the same graceful
/// degradation as [`value_to_io`] and [`value_to_card`]; the decoder seam then
/// drops it.
pub fn value_to_done(v: &RalValue) -> Option<DoneOutcome> {
    let RalValue::Variant { label, payload } = v else {
        return None;
    };
    if label != "done" {
        return None;
    }
    let m = map_of(payload.as_deref()?)?;
    let RalValue::Variant { label, payload } = m.get("outcome")? else {
        return None;
    };
    let outcome = match label.as_str() {
        "ok" => DoneOutcome::Ok,
        "err" => {
            let rec = map_of(payload.as_deref()?)?;
            DoneOutcome::Err {
                message: str_field(rec, "message").unwrap_or_default(),
                status: int_field(rec, "status").unwrap_or(0),
            }
        }
        "panic" => DoneOutcome::Panic {
            message: match payload.as_deref() {
                Some(RalValue::String(s)) => s.clone(),
                _ => String::new(),
            },
        },
        _ => return None,
    };
    Some(outcome)
}

/// Compose a `` `done `` event into a one-line [`Card`] using only the
/// existing [`Mark::Text`] vocabulary.
///
/// The card is an outcome span roled by how it settled — a clean
/// return is `Ok`, a raise is `Bad` carrying the message and status, a panic is
/// `Bad` carrying the message — followed by a plain gloss naming it as a
/// background block.
///
/// The outcome is a fixed-position value mark, not an
/// animation — the worker has already settled when this renders.
pub fn done_card(outcome: &DoneOutcome) -> Card {
    let mut spans = Vec::new();
    match outcome {
        DoneOutcome::Ok => {
            spans.push(span(Role::Ok, "done"));
            spans.push(span_plain("  Background block finished (exit 0)"));
        }
        DoneOutcome::Err { message, status } => {
            spans.push(span(Role::Bad, &format!("failed ({status})")));
            let mut body = String::from("  Background block error");
            if !message.is_empty() {
                let _ = write!(body, ": {message}");
            }
            spans.push(span_plain(&body));
        }
        DoneOutcome::Panic { message } => {
            spans.push(span(Role::Bad, "panicked"));
            let mut body = String::from("  Background block error");
            if !message.is_empty() {
                let _ = write!(body, ": {message}");
            }
            spans.push(span_plain(&body));
        }
    }
    Card(vec![Mark::Text { spans }])
}

// ── `notice`: core's own ready-boundary housekeeping, pushed ─────────────

/// The decoded body of a `` `notice `` surface event core's own engine
/// pushes at a turn's ready boundary
/// (`decisions/260706_enquiry-channel` §4.2).
///
/// The notice names a worker the lease chain
/// reaped, a run of idle top-level bindings the ledger pruned, or a
/// session-scope install past the large-binding threshold.
/// Like
/// [`DoneOutcome`], the raw record [`value_to_notice`] decodes once and
/// [`notice_card`] composes the matching one-line card — core emits the
/// fact, exarch renders it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Notice {
    /// A worker's registry entry was removed by policy — the lease chain's
    /// idle bound or backstop, or the retention sweep expiring a settled
    /// entry's unclaimed result.
    Reap {
        cmd: String,
        cause: ral_core::types::ReapCause,
    },
    /// The binding-lease chain pruned idle top-level names at this
    /// boundary — one notice per boundary, however many names fell.
    /// `idle_calls` rides parallel to `names`, so the card can report the
    /// truthful minimum age across a multi-name prune.
    Prune { names: Vec<String>, idle_calls: Vec<u64> },
    /// A session-scope binding install met the large-binding soft
    /// threshold — a residency nudge, never an eviction.
    LargeBinding { name: String, bytes: u64 },
}

/// Decode a `` `notice `` value into its [`Notice`].
///
/// The shape is
/// `` `notice [kind: `reap|`large-binding, …fields] `` where `kind` selects
/// the fields read below — exactly the two classes core's
/// `emit_ready_boundary_notices` pushes. [`Notice::Prune`] deliberately has
/// no decode arm: the idle-prune notice is host-composed from
/// `prune_idle_bindings`'s polled return (the migration's acknowledged
/// residue) and never rides the surface rail; its arm arrives with the
/// migration that pushes it. Anything else — an unrecognised `kind`, a
/// missing field, a value that is not this variant at all — returns `None`,
/// the same graceful degradation as [`value_to_done`].
pub fn value_to_notice(v: &RalValue) -> Option<Notice> {
    let RalValue::Variant { label, payload } = v else {
        return None;
    };
    if label != "notice" {
        return None;
    }
    let m = map_of(payload.as_deref()?)?;
    let RalValue::Variant { label: kind, .. } = m.get("kind")? else {
        return None;
    };
    Some(match kind.as_str() {
        "reap" => Notice::Reap {
            cmd: str_field(m, "cmd")?,
            cause: match str_field(m, "cause")?.as_str() {
                "idle" => ral_core::types::ReapCause::Idle,
                "backstop" => ral_core::types::ReapCause::Backstop,
                "retention" => ral_core::types::ReapCause::Retention,
                _ => return None,
            },
        },
        "large-binding" => {
            #[allow(clippy::cast_sign_loss, reason="max(0) floors to a non-negative byte size")]
            let bytes = int_field(m, "bytes")?.max(0) as u64;
            Notice::LargeBinding {
                name: str_field(m, "name")?,
                bytes,
            }
        }
        _ => return None,
    })
}

/// Compose a decoded [`Notice`] into its one-line [`Card`] — dispatching to
/// the variant's own composer, each a dim one-liner naming what happened.
pub fn notice_card(notice: &Notice) -> Card {
    match notice {
        Notice::Reap { cmd, cause } => reap_card(cmd, *cause),
        Notice::Prune { names, idle_calls } => bindings_pruned_card(names, idle_calls),
        Notice::LargeBinding { name, bytes } => large_binding_card(name, *bytes),
    }
}

/// Compose one policy removal into its one-line [`Card`] — the reap's
/// analogue of [`done_card`]: a worker the registry removed by policy
/// (the lease chain's two bounds on a running worker, or the retention
/// sweep expiring a settled entry's unclaimed result), so its `cmd` and
/// the bound that fired render as a fixed one-liner. Unlike `done`, the
/// `cmd` is worth keeping — this is the model's (or operator's) only
/// record of *which* worker is gone, since nothing else names it once
/// removed.
fn reap_card(cmd: &str, cause: ral_core::types::ReapCause) -> Card {
    let phrase = match cause {
        ral_core::types::ReapCause::Idle => "idle 1h unobserved",
        ral_core::types::ReapCause::Backstop => "24h backstop",
        ral_core::types::ReapCause::Retention => "finished, result unclaimed",
    };
    let spans = vec![
        span(Role::Warn, "reaped"),
        span_plain(&format!("  {cmd} — {phrase}")),
    ];
    Card(vec![Mark::Text { spans }])
}

/// Compose one prune pass's notice into a [`Card`] — `reap_card`'s
/// binding-lease sibling: a dim one-liner naming every pruned name and the
/// idle bound each met, e.g. `pruned 3 idle bindings: rows, tmp, out
/// (unused >= 256 calls)`. The displayed count is the *minimum* idle-call
/// age across `idle_calls` — every pruned name was idle at least that
/// long, so the figure is truthful even when a multi-name prune's
/// individual ages differ (`decisions/260629_agent-binding-reaping`).
fn bindings_pruned_card(names: &[String], idle_calls: &[u64]) -> Card {
    let min_idle = idle_calls.iter().min().copied().unwrap_or_default();
    let phrase = format!(
        "pruned {} idle binding{}: {} (unused >= {min_idle} calls)",
        names.len(),
        if names.len() == 1 { "" } else { "s" },
        names.join(", "),
    );
    let spans = vec![span(Role::Muted, &phrase)];
    Card(vec![Mark::Text { spans }])
}

/// Compose one large-binding notice into a [`Card`] — a dim one-liner
/// naming the binding, its shallow-size estimate, and the file-path
/// recommendation: writing a large result to disk and binding the path
/// keeps residency shallow, since the binding itself is otherwise
/// completely untouched (`decisions/260629_agent-binding-reaping`,
/// `decisions/260705_leases-and-budgets` §"Shell residency is lexical
/// state plus host leases").
fn large_binding_card(name: &str, bytes: u64) -> Card {
    let spans = vec![span(
        Role::Warn,
        &format!(
            "large binding: `{name}` — ~{bytes} bytes; consider writing it to a file \
             and binding the path instead of the captured bytes"
        ),
    )];
    Card(vec![Mark::Text { spans }])
}

// ── `services`: the host-owned durable-service ledger ────────────────────

/// Compose every live durable service into one ledger card — one
/// [`Mark::Fields`] row per service, labelled by the id `service-handle`
/// takes and valued by its birth description and age.  Host-authored only
/// (`Agent::reconcile_service_pins`): a durable service's whole bound is
/// legibility, and this pin is what makes the live set legible.
pub(crate) fn services_pin_card(services: &[crate::agent::ProbedWorker]) -> Card {
    let rows = services
        .iter()
        .map(|entry| Field {
            label: format!("service {}", entry.id),
            value: FieldVal::Inline(vec![span_plain(&format!(
                "{}  (up {}s)",
                entry.cmd, entry.up_secs
            ))]),
        })
        .collect();
    Card(vec![Mark::Fields { rows }])
}

/// A [`Card`] as a compact one-line summary — the session-layer digest the
/// nudge facility shows when reminding the model of its pinned state, where the
/// TUI's framed rendering is out of reach.
///
/// Text marks concatenate their span
/// runs (whitespace collapsed); a measure reads `label value/maxunit`; a fields
/// matrix reads `label value` pairs; a diff names its path; raw ink is its
/// bytes, lossily.  Marks join with a space.
pub fn summary_line(card: &Card) -> String {
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut parts: Vec<String> = Vec::new();
    for mark in card.marks() {
        let part = match mark {
            Mark::Text { spans } => collapse(
                &spans
                    .iter()
                    .map(|s| {
                        if s.role == Some(Role::Strong) {
                            format!("{}: ", s.text)
                        } else {
                            s.text.clone()
                        }
                    })
                    .collect::<String>(),
            ),
            Mark::Measure(m) => {
                let bound = m.max.map(|mx| format!("/{mx}")).unwrap_or_default();
                format!(
                    "{} {}{bound}{}",
                    m.label,
                    m.value,
                    m.unit.as_deref().unwrap_or("")
                )
            }
            Mark::Fields { rows } => rows
                .iter()
                .map(|f| format!("{} {}", f.label, f.value.plain()))
                .collect::<Vec<_>>()
                .join(", "),
            Mark::Diff { path, .. } => format!("diff {path}"),
            Mark::Listing { bytes, .. } | Mark::Raw { bytes } => {
                collapse(&String::from_utf8_lossy(bytes))
            }
        };
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts.join(" ")
}

// ── Decode: runtime `Value` → `Card` ────────────────────────────────────────

/// Decode the value a ral kit handed to `surface` into a [`Card`].
///
/// The canonical shape is `` `card [mark, mark, …] `` — a variant whose
/// payload is a *list* of mark variants, each carrying a record payload.
/// A bare known mark surfaced unwrapped (`` `diff […] ``) is lifted into a
/// one-mark card for the model's convenience.  Anything else returns
/// `None` and is dropped.
///
/// Decoding never fails *within* a recognised card: an unknown mark label
/// or role degrades to plain `text` rather than dropping the whole card,
/// because a card is a deliberate user-facing act, not a sentinel that
/// might be malformed.
pub fn value_to_card(v: &RalValue) -> Option<Card> {
    let RalValue::Variant { label, payload } = v else {
        return None;
    };
    if label == "card" {
        let marks = match payload.as_deref() {
            Some(RalValue::List(items)) => items.iter().map(decode_mark).collect(),
            // A `card` with a non-list payload is still a deliberate
            // surface; render whatever single mark it holds, or nothing.
            Some(other) => vec![decode_mark(other)],
            None => Vec::new(),
        };
        Some(Card(marks))
    } else if is_mark_label(label) {
        Some(Card(vec![decode_mark(v)]))
    } else {
        None
    }
}

/// Decode a `` `pin ``/`` `unpin `` *disposition wrapper* into its register key
/// and optional body card.
///
/// The shape is `` `pin [key: "…", body: `card […]] ``
/// — a render document keyed to a register slot — or `` `unpin [key: "…"] `` to
/// drop the slot.  The `body` is decoded by the **unchanged** [`value_to_card`],
/// so the wrapper carries only *placement*; an absent — or empty — body is the
/// same as `` `unpin ``, so a pin with nothing left to show drops the slot.
/// Anything else returns `None`, the same graceful degradation as
/// [`value_to_card`]; the decoder seam then drops it.
pub fn value_to_pin(v: &RalValue) -> Option<(String, Option<Card>)> {
    let RalValue::Variant { label, payload } = v else {
        return None;
    };
    match label.as_str() {
        "pin" => {
            let m = map_of(payload.as_deref()?)?;
            let key = str_field(m, "key")?;
            let body = m
                .get("body")
                .and_then(value_to_card)
                .filter(|c| !c.marks().is_empty());
            Some((key, body))
        }
        "unpin" => Some((str_field(map_of(payload.as_deref()?)?, "key")?, None)),
        _ => None,
    }
}

/// The closed mark vocabulary, by tag — also the set lifted into a one-mark
/// card when surfaced unwrapped.
fn is_mark_label(label: &str) -> bool {
    matches!(label, "text" | "measure" | "fields" | "diff" | "raw")
}

/// Decode one mark.  Total: an unrecognised or malformed mark becomes a
/// plain `text` span carrying the value's display, never a drop or panic.
fn decode_mark(v: &RalValue) -> Mark {
    let RalValue::Variant { label, payload } = v else {
        return plain_text(&v.to_string());
    };
    let rec = match payload.as_deref() {
        Some(RalValue::Map(m)) => Some(m),
        _ => None,
    };
    match label.as_str() {
        "text" => Mark::Text {
            spans: rec.map(decode_spans).unwrap_or_default(),
        },
        "measure" => rec
            .and_then(decode_measure)
            .map_or_else(|| plain_text(label), Mark::Measure),
        "fields" => Mark::Fields {
            rows: rec.map(decode_rows).unwrap_or_default(),
        },
        "diff" => rec
            .and_then(decode_diff)
            .unwrap_or_else(|| plain_text(label)),
        "raw" => Mark::Raw {
            bytes: rec.map(decode_raw_bytes).unwrap_or_default(),
        },
        _ => plain_text(&v.to_string()),
    }
}

/// A one-span plain-text mark — the degradation target for an unknown or
/// malformed mark.
fn plain_text(text: &str) -> Mark {
    Mark::Text {
        spans: vec![Span {
            role: None,
            text: text.to_string(),
        }],
    }
}

/// Decode the `spans` field of a `text` mark (or a `text`-valued field).
fn decode_spans(m: &ral_core::types::Map) -> Vec<Span> {
    match m.get("spans") {
        Some(RalValue::List(items)) => items.iter().map(decode_span).collect(),
        _ => Vec::new(),
    }
}

/// Decode one span record `[role?, text]`.  A bare string is a roleless
/// span; anything stranger falls back to the value's display.
fn decode_span(v: &RalValue) -> Span {
    match v {
        RalValue::Map(m) => Span {
            role: str_field(m, "role").as_deref().and_then(Role::parse),
            text: str_field(m, "text").unwrap_or_default(),
        },
        RalValue::String(s) => Span {
            role: None,
            text: s.clone(),
        },
        other => Span {
            role: None,
            text: other.to_string(),
        },
    }
}

/// Decode a `measure` record; `None` (→ plain-text fallback) when the
/// magnitude `value` is absent or not an integer.
fn decode_measure(m: &ral_core::types::Map) -> Option<Measure> {
    Some(Measure {
        label: str_field(m, "label").unwrap_or_default(),
        value: count_field(m, "value")?,
        max: count_field(m, "max"),
        unit: str_field(m, "unit"),
    })
}

/// Decode the `rows` field of a `fields` mark into aligned `(label, value)`
/// fields.
fn decode_rows(m: &ral_core::types::Map) -> Vec<Field> {
    match m.get("rows") {
        Some(RalValue::List(items)) => items.iter().map(decode_field).collect(),
        _ => Vec::new(),
    }
}

/// Decode one `[label: …, value: …]` row record.  Rows are records, not
/// positional pairs, because ral types a list homogeneously — a `String`
/// label and a variant value could not share one positional list.  The
/// value column nests marks: a bare string is roleless inline text, a
/// `text` mark its spans, a `measure` mark a nested measure; anything else
/// renders as its display.
fn decode_field(v: &RalValue) -> Field {
    let Some(m) = map_of(v) else {
        return Field {
            label: v.to_string(),
            value: FieldVal::Inline(Vec::new()),
        };
    };
    let label = str_field(m, "label").unwrap_or_default();
    let value = match m.get("value") {
        None => FieldVal::Inline(Vec::new()),
        Some(RalValue::Variant { label, payload }) if label == "text" => {
            let spans = match payload.as_deref() {
                Some(RalValue::Map(m)) => decode_spans(m),
                _ => Vec::new(),
            };
            FieldVal::Inline(spans)
        }
        Some(RalValue::Variant { label, payload }) if label == "measure" => {
            match payload.as_deref().and_then(map_of).and_then(decode_measure) {
                Some(measure) => FieldVal::Measure(measure),
                None => FieldVal::Inline(Vec::new()),
            }
        }
        Some(other) => FieldVal::Inline(vec![decode_span(other)]),
    };
    Field { label, value }
}

/// Decode a `diff` record: its `path` and a `hunks` list of hunk records,
/// the whole-file shape `edit` emits.  A missing `hunks` lifts to an empty
/// vec so a bare diff still renders; `None` (→ plain-text fallback) only
/// when there is no `path`.
fn decode_diff(m: &ral_core::types::Map) -> Option<Mark> {
    let path = str_field(m, "path")?;
    let hunks = match m.get("hunks") {
        Some(RalValue::List(items)) => items.iter().filter_map(map_of).map(decode_hunk).collect(),
        _ => Vec::new(),
    };
    Some(Mark::Diff { path, hunks })
}

/// Decode one hunk record: a `start` line (defaulting to 1) and its `rows`
/// list of `{ tag, text }` records.  A missing `rows` defaults to empty, so
/// a partially-formed hunk still renders rather than dropping.
fn decode_hunk(m: &ral_core::types::Map) -> Hunk {
    let rows = match m.get("rows") {
        Some(RalValue::List(items)) => items.iter().filter_map(map_of).map(decode_row).collect(),
        _ => Vec::new(),
    };
    Hunk {
        start: count_field(m, "start").unwrap_or(1),
        rows,
    }
}

/// Decode one row record: its `tag` (`context` / `del` / `add`) and its
/// `segs` list.  An unrecognized or missing tag degrades to context — the row
/// is never dropped or panicked on, so the whole diff still renders.
fn decode_row(m: &ral_core::types::Map) -> Row {
    let segs = match m.get("segs") {
        Some(RalValue::List(items)) => items.iter().filter_map(map_of).map(decode_seg).collect(),
        _ => Vec::new(),
    };
    match str_field(m, "tag").as_deref() {
        Some("del") => Row::Del(segs),
        Some("add") => Row::Add(segs),
        _ => Row::Context(segs),
    }
}

/// Decode one segment record: its `emph` flag (defaulting to unemphasised)
/// and `text`.
fn decode_seg(m: &ral_core::types::Map) -> Seg {
    Seg {
        emph: matches!(m.get("emph"), Some(RalValue::Bool(true))),
        text: str_field(m, "text").unwrap_or_default(),
    }
}

/// Decode a `raw` mark's `bytes`: a `Bytes` value verbatim, a string's
/// UTF-8, or a list of integers as bytes.
fn decode_raw_bytes(m: &ral_core::types::Map) -> Vec<u8> {
    match m.get("bytes") {
        Some(RalValue::Bytes(b)) => b.clone(),
        Some(RalValue::String(s)) => s.clone().into_bytes(),
        Some(RalValue::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                RalValue::Int(n) => u8::try_from(*n).ok(),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `&Value` → `&Map` when it is one.
fn map_of(v: &RalValue) -> Option<&ral_core::types::Map> {
    match v {
        RalValue::Map(m) => Some(m),
        _ => None,
    }
}

/// A string-typed field of a record.
fn str_field(m: &ral_core::types::Map, field: &str) -> Option<String> {
    match m.get(field) {
        Some(RalValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// An optional bytes-typed field of a record.
fn bytes_field(m: &ral_core::types::Map, field: &str) -> Option<Vec<u8>> {
    match m.get(field) {
        Some(RalValue::Bytes(b)) => Some(b.clone()),
        _ => None,
    }
}

/// An integer-typed field clamped into `u32` (negatives floor to 0).
fn count_field(m: &ral_core::types::Map, field: &str) -> Option<u32> {
    match m.get(field) {
        Some(RalValue::Int(n)) => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason="value pre-clamped to [0, u32::MAX]")]
            let clamped = (*n).clamp(0, i64::from(u32::MAX)) as u32;
            Some(clamped)
        }
        _ => None,
    }
}

/// An integer-typed field, unclamped — an exec status carries the full signed
/// range a process exit can name (negatives for signal-coded exits).
fn int_field(m: &ral_core::types::Map, field: &str) -> Option<i64> {
    match m.get(field) {
        Some(RalValue::Int(n)) => Some(*n),
        _ => None,
    }
}

/// A list-of-strings field; non-string elements render as their display so a
/// partially-formed field (an `argv`, a row list) stays faithful, and a
/// missing or non-list field is empty.
fn strings_field(m: &ral_core::types::Map, field: &str) -> Vec<String> {
    match m.get(field) {
        Some(RalValue::List(items)) => items
            .iter()
            .map(|v| match v {
                RalValue::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Build a `` `card [marks…] `` runtime value the way the kit does.
    fn card_value(marks: Vec<RalValue>) -> RalValue {
        RalValue::Variant {
            label: "card".into(),
            payload: Some(Box::new(RalValue::list(marks))),
        }
    }
    fn mark(label: &str, fields: Vec<(&str, RalValue)>) -> RalValue {
        RalValue::Variant {
            label: label.into(),
            payload: Some(Box::new(RalValue::map(
                fields.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            ))),
        }
    }
    fn s(text: &str) -> RalValue {
        RalValue::String(text.into())
    }
    fn list(items: Vec<RalValue>) -> RalValue {
        RalValue::list(items)
    }
    /// A diff-row record: a `tag` and a one-segment `segs` list carrying
    /// `text` (unemphasised) — the shape [`decode_row`] lifts back into a
    /// [`Row`].
    fn seg_row(tag: &str, text: &str) -> RalValue {
        RalValue::map(vec![
            ("tag".into(), s(tag)),
            (
                "segs".into(),
                list(vec![RalValue::map(vec![("text".into(), s(text))])]),
            ),
        ])
    }

    /// A full card with one of every mark decodes structurally, in order.
    #[test]
    fn decodes_every_mark() {
        let v = card_value(vec![
            mark(
                "text",
                vec![(
                    "spans",
                    list(vec![RalValue::map(vec![
                        ("role".into(), s("strong")),
                        ("text".into(), s("edited ")),
                    ])]),
                )],
            ),
            mark(
                "diff",
                vec![
                    ("path", s("a.rs")),
                    (
                        "hunks",
                        list(vec![RalValue::map(vec![
                            ("start".into(), RalValue::Int(7)),
                            (
                                "rows".into(),
                                list(vec![seg_row("del", "x"), seg_row("add", "y")]),
                            ),
                        ])]),
                    ),
                ],
            ),
            mark(
                "fields",
                vec![(
                    "rows",
                    list(vec![RalValue::map(vec![
                        ("label".into(), s("tests")),
                        ("value".into(), s("42 passed")),
                    ])]),
                )],
            ),
            mark(
                "measure",
                vec![
                    ("label", s("crates")),
                    ("value", RalValue::Int(7)),
                    ("max", RalValue::Int(12)),
                ],
            ),
            mark("raw", vec![("bytes", s("hi"))]),
        ]);
        let Card(marks) = value_to_card(&v).expect("a card decodes");
        assert_eq!(marks.len(), 5);
        assert!(matches!(&marks[0], Mark::Text { spans } if spans[0].role == Some(Role::Strong)));
        assert!(matches!(&marks[1], Mark::Diff { path, hunks }
            if path == "a.rs" && hunks[0].start == 7
                && matches!(hunks[0].rows.as_slice(), [Row::Del(_), Row::Add(_)])
                && hunks[0].rows.iter().map(Row::text).eq(["x", "y"].map(String::from))));
        assert!(matches!(&marks[2], Mark::Fields { rows } if rows[0].label == "tests"));
        assert!(matches!(&marks[3], Mark::Measure(m) if m.value == 7 && m.max == Some(12)));
        assert!(matches!(&marks[4], Mark::Raw { bytes } if bytes == b"hi"));
    }

    /// A non-`card` variant is dropped (→ `None`); a *bare known mark* is
    /// lifted into a one-mark card for convenience.
    #[test]
    fn drops_non_card_but_lifts_bare_mark() {
        assert!(value_to_card(&RalValue::String("nope".into())).is_none());
        assert!(
            value_to_card(&RalValue::Variant {
                label: "bogus".into(),
                payload: Some(Box::new(RalValue::map(vec![]))),
            })
            .is_none(),
            "an unknown top-level variant is not a card"
        );
        let bare = mark(
            "diff",
            vec![("path", s("a.rs")), ("start", RalValue::Int(1))],
        );
        let Card(marks) = value_to_card(&bare).expect("a bare diff lifts");
        assert_eq!(marks.len(), 1);
        assert!(matches!(&marks[0], Mark::Diff { .. }));
    }

    /// An unknown *mark* inside a card degrades to plain text, never a drop
    /// or a panic — the whole card still renders.
    #[test]
    fn unknown_mark_degrades_to_plain_text() {
        let v = card_value(vec![
            mark("text", vec![("spans", list(vec![]))]),
            mark("wormhole", vec![("x", RalValue::Int(1))]),
        ]);
        let Card(marks) = value_to_card(&v).expect("card decodes");
        assert_eq!(marks.len(), 2);
        assert!(matches!(&marks[1], Mark::Text { .. }), "unknown → text");
    }

    /// A card serialises to a structured mark tree — with each mark internally
    /// tagged and a `raw` mark carrying its bytes.  Only `raw` is opaque, and
    /// honestly so.
    #[test]
    fn serialises_to_a_structured_mark_tree() {
        let card = Card(vec![
            Mark::Text {
                spans: vec![Span {
                    role: Some(Role::Ok),
                    text: "done".into(),
                }],
            },
            Mark::Measure(Measure {
                label: "tasks".into(),
                value: 3,
                max: Some(12),
                unit: None,
            }),
            Mark::Raw {
                bytes: vec![0xff, b'h'],
            },
        ]);
        let v = serde_json::to_value(&card).expect("a card serialises");
        let marks = v.as_array().expect("a card is a JSON array of marks");
        assert_eq!(marks[0]["mark"], "text");
        assert_eq!(marks[0]["spans"][0]["role"], "ok");
        assert_eq!(marks[1]["mark"], "measure");
        assert_eq!(marks[1]["value"], 3);
        assert_eq!(marks[1]["max"], 12);
        assert_eq!(marks[2]["mark"], "raw");
        assert_eq!(marks[2]["bytes"], serde_json::json!([255, 104]));
    }

    /// Build a `Value::Map` of `(field, value)` pairs the way core does.
    fn io_value(fields: Vec<(&str, RalValue)>) -> RalValue {
        RalValue::map(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Each of the four io shapes decodes into its typed [`IoEvent`].
    #[test]
    fn value_to_io_decodes_each_shape() {
        assert_eq!(
            value_to_io(&io_value(vec![("io", s("read")), ("path", s("a.rs"))])),
            Some(IoEvent::Read {
                path: "a.rs".into()
            })
        );
        assert_eq!(
            value_to_io(&io_value(vec![
                ("io", s("write")),
                ("path", s("b.rs")),
                ("mode", s("append")),
                ("outcome", s("committed")),
            ])),
            Some(IoEvent::Write {
                path: "b.rs".into(),
                mode: WriteMode::Append,
                outcome: WriteOutcome::Committed,
                new_bytes: None,
                old_bytes: None,
            })
        );
        assert_eq!(
            value_to_io(&io_value(vec![
                ("io", s("exec")),
                ("argv", list(vec![s("git"), s("status"), s("-s")])),
                ("outcome", s("bad")),
                ("status", RalValue::Int(128)),
            ])),
            Some(IoEvent::Exec {
                argv: vec!["git".into(), "status".into(), "-s".into()],
                outcome: ExecOutcome::Bad,
                status: 128,
            })
        );
        assert_eq!(
            value_to_io(&io_value(vec![
                ("io", s("grep")),
                ("scope", s("src/")),
                ("pattern", s("TODO")),
            ])),
            Some(IoEvent::Grep {
                scope: "src/".into(),
                pattern: "TODO".into(),
            })
        );
    }

    /// The flattened text of a card's first [`Mark::Text`], spans joined —
    /// the on-screen line without its roling, for asserting exec rendering.
    fn line(card: &Card) -> String {
        let Card(marks) = card;
        match &marks[0] {
            Mark::Text { spans } => spans.iter().map(|s| s.text.as_str()).collect(),
            _ => panic!("expected a text mark"),
        }
    }

    /// A surfaced exec re-quotes each post-shell argv token *only* where the
    /// shell would reparse it: a clean token rides bare, a space or glob is
    /// single-quoted, and an embedded quote takes the `'\''` idiom — so the
    /// rendered `$` line is always a runnable command.
    #[test]
    fn exec_requotes_only_where_the_shell_would_reparse() {
        // The rendered command, without the `$ ` prompt and ` → status` tail.
        let cmd = |argv: &[&str]| -> String {
            let full = line(&io_card(&IoEvent::Exec {
                argv: argv.iter().map(ToString::to_string).collect(),
                outcome: ExecOutcome::Ok,
                status: 0,
            }));
            full.strip_prefix("$ ")
                .and_then(|s| s.strip_suffix(" → 0"))
                .expect("the `$ … → status` frame")
                .to_string()
        };
        // A clean argv rides bare — we re-quote per token rather than wrap
        // everything, so nothing shell-safe gains quotes it didn't need.
        assert_eq!(
            cmd(&["grep", "-n", "why-the-ubuntu-22-fiction", "VM.md"]),
            "grep -n why-the-ubuntu-22-fiction VM.md"
        );
        assert_eq!(cmd(&["ls", "README.md"]), "ls README.md");
        // A metacharacter-laden argv round-trips: whatever quoting `shlex`
        // chooses, our space-joined line word-splits back to the exact argv —
        // i.e. the rendered command is faithful and runnable.
        let tricky = ["echo", "hello world", "*.rs", "it's", ""];
        assert_eq!(
            shlex::split(&cmd(&tricky)).expect("the rendered line re-parses"),
            tricky.map(String::from)
        );
    }

    /// A non-io value is not an io event: a `` `card `` variant, a plain
    /// string, and a map without a recognised `io` tag all return `None`,
    /// so the sink falls through to the card decoder.
    #[test]
    fn value_to_io_rejects_non_io_values() {
        assert!(
            value_to_io(&card_value(vec![])).is_none(),
            "a card is not io"
        );
        assert!(value_to_io(&s("plain")).is_none(), "a string is not io");
        assert!(
            value_to_io(&io_value(vec![("io", s("teleport"))])).is_none(),
            "an unknown io tag is not an io event"
        );
        assert!(
            value_to_io(&io_value(vec![("path", s("a.rs"))])).is_none(),
            "a map without an io field is not an io event"
        );
    }

    /// An `IoEvent` serialises structurally — tagged by its `io` field, with
    /// the mode/outcome enums as `snake_case` strings — so the raw effect is
    /// recorded in `transcript.jsonl` (the card it renders is not).
    #[test]
    fn io_event_serialises_structurally() {
        let v = serde_json::to_value(IoEvent::Write {
            path: "b.rs".into(),
            mode: WriteMode::Append,
            outcome: WriteOutcome::Failed,
            new_bytes: None,
            old_bytes: None,
        })
        .expect("an io event serialises");
        assert_eq!(v["io"], "write");
        assert_eq!(v["path"], "b.rs");
        assert_eq!(v["mode"], "append");
        assert_eq!(v["outcome"], "failed");

        let v = serde_json::to_value(IoEvent::Exec {
            argv: vec!["git".into(), "log".into()],
            outcome: ExecOutcome::Ok,
            status: 0,
        })
        .expect("an exec event serialises");
        assert_eq!(v["io"], "exec");
        assert_eq!(v["argv"], serde_json::json!(["git", "log"]));
        assert_eq!(v["outcome"], "ok");
        assert_eq!(v["status"], 0);
    }

    /// Build the `` `done `` value a detached worker flushes — `cmd` plus a
    /// closed `` `ok ``/`` `err ``/`` `panic `` outcome — the way core mints it.
    fn done_value(cmd: &str, outcome: RalValue) -> RalValue {
        RalValue::Variant {
            label: "done".into(),
            payload: Some(Box::new(RalValue::map(vec![
                ("cmd".into(), s(cmd)),
                ("outcome".into(), outcome),
            ]))),
        }
    }
    fn variant(label: &str, payload: RalValue) -> RalValue {
        RalValue::Variant {
            label: label.into(),
            payload: Some(Box::new(payload)),
        }
    }

    /// The three outcome classes decode into their typed [`DoneOutcome`]: a
    /// clean `` `ok ``, an `` `err `` carrying the error record's message and
    /// status, and a `` `panic `` carrying its message.
    #[test]
    fn value_to_done_decodes_each_outcome() {
        assert_eq!(
            value_to_done(&done_value("<block>", variant("ok", RalValue::Unit))),
            Some(DoneOutcome::Ok)
        );
        let err = variant(
            "err",
            RalValue::map(vec![
                ("cmd".into(), s("<runtime>")),
                ("status".into(), RalValue::Int(2)),
                ("message".into(), s("boom")),
                ("line".into(), RalValue::Int(3)),
                ("col".into(), RalValue::Int(1)),
            ]),
        );
        assert_eq!(
            value_to_done(&done_value("<block>", err)),
            Some(DoneOutcome::Err {
                message: "boom".into(),
                status: 2,
            })
        );
        assert_eq!(
            value_to_done(&done_value("<block>", variant("panic", s("kaput")))),
            Some(DoneOutcome::Panic {
                message: "kaput".into(),
            })
        );
    }

    /// A non-`done` value is not a done event: a `` `card `` variant, an io
    /// `Map`, and a plain string all return `None` so the decoder seam drops
    /// them onto the next branch.
    #[test]
    fn value_to_done_rejects_non_done_values() {
        assert!(value_to_done(&card_value(vec![])).is_none());
        assert!(value_to_done(&io_value(vec![("io", s("read"))])).is_none());
        assert!(value_to_done(&s("plain")).is_none());
    }

    /// `single_diff` keys aggregation: exactly one diff mark yields its
    /// path + hunks; a richer card does not.
    #[test]
    fn single_diff_keys_aggregation() {
        let one = Card(vec![Mark::Diff {
            path: "a.rs".into(),
            hunks: vec![Hunk {
                start: 1,
                rows: vec![
                    Row::Del(vec![Seg::plain("x")]),
                    Row::Add(vec![Seg::plain("y")]),
                    Row::Add(vec![Seg::plain("z")]),
                ],
            }],
        }]);
        assert_eq!(one.single_diff().map(|(p, _)| p), Some("a.rs"));
        assert_eq!(one.magnitude(), Some(3));
        assert!(one.has_diff());
        let rich = Card(vec![
            Mark::Text { spans: vec![] },
            Mark::Diff {
                path: "a.rs".into(),
                hunks: vec![],
            },
        ]);
        assert!(rich.single_diff().is_none());
        assert!(rich.has_diff());
        let plain = Card(vec![Mark::Text { spans: vec![] }]);
        assert_eq!(plain.magnitude(), None);
        assert!(!plain.has_diff());
    }
}
