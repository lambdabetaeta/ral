//! The structural I/O surface: the closed set of effects core reports onto
//! the `surface` sink — a read, a write, an exec, a grep. `surface` decodes
//! each, once, from a raw ral value into a typed [`IoEvent`], and composes
//! the [`crate::bus::card::Card`] each renders as.

use ral_core::Value as RalValue;
use serde::Serialize;
use std::borrow::Cow;

use super::diff::whole_file_hunks;
use super::value::{bytes_field, int_field, map_of, str_field, strings_field};
use super::{Card, Mark, Role, Span};

/// How a write reached the file: a one-shot `write`, an `append`, or a
/// `stream` of bytes.  Nominal, like a [`crate::bus::card::Role`] — the card maps it to text,
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

    /// The word shown in a write card's outcome span.
    fn label(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }

    /// The nominal role that spans `label` in the outcome span.
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
/// Unlike a [`crate::bus::card::Card`] (which the kit composes in
/// ral and exarch only renders), an `IoEvent` is the raw effect record —
/// `surface` decodes it once ([`value_to_io`]) and composes the matching card
/// ([`io_card`]).  Both ride the bus together ([`crate::bus::Kind::Io`]) so
/// the recorded event keeps the structure the rendered mark tree erases.
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
/// `tui::group`).
///
/// Reads, execs, and greps fold into a run and tally here; a
/// write is a barrier that ends a run, so it is not an observation kind — it is
/// tracked by its card origin instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ObservationKind {
    Read,
    Exec,
    Grep,
}

/// Decode a runtime `Value` core surfaced into a structural [`IoEvent`].
///
/// An io event is a `Map` whose `io` field names one of `read`/`write`/`exec`/
/// `grep` — the contract core emits.  Anything else (a `` `card `` variant, a
/// plain string, a map without a recognised `io` tag) returns `None`; the
/// decoder seam falls through to the card decoder.
pub(crate) fn value_to_io(v: &RalValue) -> Option<IoEvent> {
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

/// Compose an [`IoEvent`] into a [`crate::bus::card::Card`].
///
/// The heading is one [`crate::bus::card::Mark::Text`]
/// of roled spans: a dim verb naming the operation (a nominal category, carried
/// by a word rather than a mirror-orientation glyph) followed by the path or
/// program as the subject — lifted by [`crate::bus::card::Role::Path`]'s hue against the muted
/// label — and the outcome roled by its level.  A committed write appends a
/// [`write_preview`] below that heading: a diff against the prior content
/// when there was one to diff against, otherwise a plain listing of what it
/// wrote.
pub(crate) fn io_card(event: &IoEvent) -> Card {
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
                body.extend(write_preview(
                    path,
                    old_bytes.as_deref(),
                    new_bytes.as_deref(),
                ));
            }
            write_spans(path, *outcome)
        }
        IoEvent::Exec {
            argv,
            outcome: _,
            status,
        } => {
            let mut spans = vec![Span::plain("$ ")];
            spans.extend(exec_cmd_spans(argv));
            let role = if *status == 0 { Role::Ok } else { Role::Bad };
            spans.push(Span::plain(" → "));
            spans.push(Span::new(role, status.to_string()));
            spans
        }
        IoEvent::Grep { scope, pattern } => {
            let mut spans = vec![Span::new(Role::Muted, "grep ")];
            spans.extend(grep_spans(scope, pattern));
            spans
        }
    };
    let mut marks = vec![Mark::Text { spans }];
    marks.extend(body);
    Card(marks)
}

/// The command of an exec, *without* its `$ ` prefix or `→ status` tail —
/// the program as a [`crate::bus::card::Role::Path`] span and each arg as plain ink (a missing
/// command degrades to plain ink).  Shared by [`io_card`] (which frames it
/// with the prompt and status) and [`execs_card`] (which comma-joins
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
            let mut spans = vec![Span::new(Role::Path, quote(prog))];
            for arg in args {
                spans.push(Span::plain(format!(" {}", quote(arg))));
            }
            spans
        }
        None => vec![Span::plain("(no command)")],
    }
}

/// A grep's `pattern in scope` — the pattern as [`crate::bus::card::Role::Code`], the scope as
/// [`crate::bus::card::Role::Path`] — *without* the leading `grep ` verb, so the group head can
/// carry one shared verb over a comma-joined run.
fn grep_spans(scope: &str, pattern: &str) -> Vec<Span> {
    vec![
        Span::new(Role::Code, pattern),
        Span::plain(" in "),
        Span::new(Role::Path, scope),
    ]
}

/// A read row: the muted verb `read`, then the path as the subject.  Reused
/// verbatim per entry in [`reads_card`]'s comma-joined read run, so a lone
/// read and a grouped one share one shape.
fn read_spans(path: &str) -> Vec<Span> {
    vec![Span::new(Role::Muted, "read "), Span::new(Role::Path, path)]
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
        Span::new(Role::Muted, "write "),
        Span::new(Role::Path, path),
        Span::plain(" "),
        Span::new(outcome.role(), outcome.label()),
    ]
}

/// The number of leading lines a write card previews of the file it wrote,
/// when it falls back to a listing rather than a diff.
const WRITE_PREVIEW_LINES: usize = 10;

/// Preview a committed write: a whole-file [`crate::bus::card::Mark::Diff`] against the prior
/// content when `old` is present (core supplies it only for an atomic write
/// that overwrote an existing file with neither side exceeding its read cap)
/// and both sides are valid UTF-8 — the diff is computed here, once, for
/// every committed write [`io_card`] renders, whatever wrote it (a `>`
/// redirect, `edit-hash`, `edit-replace`).  Otherwise, the first
/// [`WRITE_PREVIEW_LINES`] lines of `new` as one [`crate::bus::card::Mark::Listing`], `more` set
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

// ── Observation groups: a run of buffered surfaces of one kind → one card ────
//
// Each helper composes a run of buffered observation surfaces of one kind
// (Read / Exec / Grep) into a single [`Card`], `None` when the run is empty.
// The card is one [`Mark::Text`] reusing the exact `io_card` span vocabulary,
// so hues match; a lone surface (a run of one) renders identically to its
// `io_card`, modulo the deliberate exec departure. Writes never reach here: a
// write is a barrier rendered standalone as its own card.

/// The Read group: `read p1, read p2, …` — each entry the verb + path,
/// comma-joined.
pub(crate) fn reads_card(reads: &[String]) -> Option<Card> {
    if reads.is_empty() {
        return None;
    }
    let mut spans = Vec::new();
    join_spans(&mut spans, reads, |spans, path| {
        spans.extend(read_spans(path));
    });
    Some(Card(vec![Mark::Text { spans }]))
}

/// The Exec group: `$ cmd1, cmd2, …` — one prompt, the commands comma-joined.
///
/// **Drops the `→ status` tail** that single [`io_card`] exec rows carry: a
/// comma-joined run of commands reads as the *set of commands run* (`$ wc -l,
/// grep -rn, git status`), and a per-command status would be per-event noise
/// on that line.  The status is not lost — it rides the bus in each
/// [`crate::bus::Kind::Io`]'s structured event and reaches the transcript via
/// [`crate::agent::transcript::event_record`]; only this grouped *presentation* omits it.
pub(crate) fn execs_card(execs: &[IoEvent]) -> Option<Card> {
    if execs.is_empty() {
        return None;
    }
    let mut spans = vec![Span::plain("$ ")];
    join_spans(&mut spans, execs, |spans, e| {
        if let IoEvent::Exec { argv, .. } = e {
            spans.extend(exec_cmd_spans(argv));
        }
    });
    Some(Card(vec![Mark::Text { spans }]))
}

/// The Grep group: `grep p1 in s1, p2 in s2, …` — one verb, the
/// `pattern in scope` entries comma-joined.
pub(crate) fn greps_card(greps: &[IoEvent]) -> Option<Card> {
    if greps.is_empty() {
        return None;
    }
    let mut spans = vec![Span::new(Role::Muted, "grep ")];
    join_spans(&mut spans, greps, |spans, e| {
        if let IoEvent::Grep { scope, pattern } = e {
            spans.extend(grep_spans(scope, pattern));
        }
    });
    Some(Card(vec![Mark::Text { spans }]))
}

/// Append each of `items` to `spans` via `each`, separating entries with a
/// plain `", "` — the comma-join shared by every observation group.
fn join_spans<T>(spans: &mut Vec<Span>, items: &[T], each: impl Fn(&mut Vec<Span>, &T)) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::plain(", "));
        }
        each(spans, item);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{card_value, io_value, list, s};
    use super::*;

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
}
