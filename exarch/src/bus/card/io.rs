//! The closed set of effects core reports onto the `surface` sink — a read, a
//! write, an exec, a grep — decoded here into a typed [`IoEvent`] and composed
//! into the [`Card`] each renders as. Core's end of the wire contract is
//! `core/src/runtime/command/io_event.rs`, which names no card type.

use ral_core::Value as RalValue;
use serde::Serialize;
use std::borrow::Cow;

use super::diff::whole_file_hunks;
use super::value::{bytes_field, int_field, map_of, str_field, strings_field};
use super::{Card, Mark, Role, Span};

/// How a write reached the file. Rides the recorded event only: the write card
/// names the act and how it settled, never the mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    Write,
    Append,
    Stream,
}

impl WriteMode {
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
/// `failed` to open or rename.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteOutcome {
    Committed,
    Aborted,
    Failed,
}

impl WriteOutcome {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "committed" => Self::Committed,
            "aborted" => Self::Aborted,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

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

/// Whether an exec ran cleanly (`ok`) or not (`bad`). Recorded only: the exec
/// card roles its status span by the status code, not by this tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutcome {
    Ok,
    Bad,
}

impl ExecOutcome {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => Self::Ok,
            "bad" => Self::Bad,
            _ => return None,
        })
    }
}

/// The raw effect record behind a read, write, exec, or grep card.
///
/// [`value_to_io`] decodes it, [`io_card`] renders it, and both ride the bus
/// together as `Kind::Io`, so `transcript.jsonl` keeps the structure the
/// rendered mark tree erases.
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
        // A bounded prefix of what landed, for the card's preview. Skipped so
        // `transcript.jsonl` records a write's shape, not its content.
        #[serde(skip)]
        new_bytes: Option<Vec<u8>>,
        // The target's prior whole content — core supplies it only for an
        // atomic overwrite of an existing file with neither side past its
        // preview cap. Decides diff versus listing; skipped for the same reason.
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

/// The census bucket a surfaced observation counts toward when a coalesced run
/// reduces to its tally (`Tally` in `tui/group.rs`). A write has no bucket: it
/// is a barrier that ends a run, tracked by its card origin instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ObservationKind {
    Read,
    Exec,
    Grep,
}

/// Decode a surfaced `Value` into an [`IoEvent`]: a `Map` whose `io` field names
/// one of `read`/`write`/`exec`/`grep`. Anything else answers `None`, and
/// `decode_surface` in `shell_eval.rs` tries the next decoder.
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

/// Compose an [`IoEvent`] into a [`Card`]: one [`Mark::Text`] heading of a muted
/// verb, the path or program as its [`Role::Path`] subject, and the outcome
/// roled by its level. A committed write appends a [`write_preview`] below.
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

/// An exec's command alone, without the `$ ` prompt or ` → status` tail, so
/// [`execs_card`] can comma-join several under one prompt.
///
/// The surfaced `argv` is post-shell — already word-split, quotes consumed — so
/// each token is re-quoted *only* where the shell would otherwise reparse it,
/// and the rendered line word-splits back to the exact argv rather than to a
/// lie. `try_quote`'s sole error is an interior nul, which no real argv carries.
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

/// A grep's `pattern in scope`, *without* the leading `grep ` verb, so a
/// comma-joined run can carry one shared verb at its head.
fn grep_spans(scope: &str, pattern: &str) -> Vec<Span> {
    vec![
        Span::new(Role::Code, pattern),
        Span::plain(" in "),
        Span::new(Role::Path, scope),
    ]
}

/// A read row, `read <path>` — reused verbatim per entry in [`reads_card`], so a
/// lone read and a grouped one share one shape.
fn read_spans(path: &str) -> Vec<Span> {
    vec![Span::new(Role::Muted, "read "), Span::new(Role::Path, path)]
}

/// A write card's heading, `write <path> <outcome>` — the same line whatever the
/// mode, which rides the event only.
fn write_spans(path: &str, outcome: WriteOutcome) -> Vec<Span> {
    vec![
        Span::new(Role::Muted, "write "),
        Span::new(Role::Path, path),
        Span::plain(" "),
        Span::new(outcome.role(), outcome.label()),
    ]
}

/// Leading lines a write card lists when it cannot diff.
const WRITE_PREVIEW_LINES: usize = 10;

/// Preview a committed write: a whole-file [`Mark::Diff`] when `old` is present
/// and both sides are valid UTF-8, else the head of `new` as a
/// [`Mark::Listing`] — the fallback for a new file, binary content, or a side
/// too large. Every committed write lands here whatever wrote it (a `>`
/// redirect, `edit-hash`, `edit-replace`), so the choice is made in one place.
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
// Each reuses the exact `io_card` span vocabulary, so a run of one renders like
// its own card, modulo the deliberate exec departure below. Writes never reach
// here — a write is a barrier, landed alone.

/// `read p1, read p2, …`
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

/// `$ cmd1, cmd2, …` under one prompt.
///
/// Drops the ` → status` tail a lone [`io_card`] exec row carries: a joined run
/// reads as the *set of commands run*, where per-command statuses would be
/// noise. Nothing is lost — each status still rides its own bus event into
/// `transcript.jsonl`; only this presentation omits it.
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

/// `grep p1 in s1, p2 in s2, …` under one verb.
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

/// The comma-join every observation group shares.
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

    /// The card's first [`Mark::Text`] flattened — the on-screen line, roling
    /// dropped.
    fn line(card: &Card) -> String {
        let Card(marks) = card;
        match &marks[0] {
            Mark::Text { spans } => spans.iter().map(|s| s.text.as_str()).collect(),
            _ => panic!("expected a text mark"),
        }
    }

    #[test]
    fn exec_requotes_only_where_the_shell_would_reparse() {
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
        // Per token, not per line: nothing shell-safe gains quotes it lacked.
        assert_eq!(
            cmd(&["grep", "-n", "why-the-ubuntu-22-fiction", "VM.md"]),
            "grep -n why-the-ubuntu-22-fiction VM.md"
        );
        assert_eq!(cmd(&["ls", "README.md"]), "ls README.md");
        // Whatever quoting shlex picks, the line word-splits back to the argv.
        let tricky = ["echo", "hello world", "*.rs", "it's", ""];
        assert_eq!(
            shlex::split(&cmd(&tricky)).expect("the rendered line re-parses"),
            tricky.map(String::from)
        );
    }

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

    /// The shape `transcript.jsonl` records: tagged by `io`, enums as
    /// `snake_case`. The card it renders is not recorded.
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
