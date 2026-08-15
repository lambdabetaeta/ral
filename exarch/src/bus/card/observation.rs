//! Card composition over core's one observation vocabulary
//! (`ral_core::types::Observed`): a command settled, a write landed, a
//! redirect read opened, a grep ran, a capability check was denied. Decoding
//! the surfaced `Value` back into an [`Observation`] is core's own
//! `Observation::from_value`, called at `shell_eval.rs`'s `decode_surface`;
//! this module only renders what core already decoded.

use ral_core::Value as RalValue;
use ral_core::serial::FOValue;
use std::borrow::Cow;

use ral_core::types::{
    CommandOrigin, Decision, LeaseClass, Map, Observation, Observed, WorkerId, WriteOutcome,
};

use super::diff::whole_file_hunks;
use super::{Card, Mark, Role, Span};

/// `committed`/`aborted`/`stopped`/`failed`, styled
/// `Role::Ok`/`Role::Warn`/`Role::Warn`/`Role::Bad` in [`write_spans`].
/// Exarch's own labels: core's `WriteOutcome` names its wire tag privately,
/// and the two vocabularies need not agree.
fn write_outcome_label(outcome: WriteOutcome) -> &'static str {
    match outcome {
        WriteOutcome::Committed => "committed",
        WriteOutcome::Aborted => "aborted",
        WriteOutcome::Deferred => "stopped",
        WriteOutcome::Failed => "failed",
    }
}

fn write_outcome_role(outcome: WriteOutcome) -> Role {
    match outcome {
        WriteOutcome::Committed => Role::Ok,
        WriteOutcome::Aborted | WriteOutcome::Deferred => Role::Warn,
        WriteOutcome::Failed => Role::Bad,
    }
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

/// Where a surfaced observation lands on the rail, or `None` for one the rail
/// does not draw: evaluation (a `builtin` command), or a capability check that
/// was not a denial. Core reports every observation it makes; this is where
/// the host says which of them it wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RailPlace {
    /// Joins a coalesced run, tallied under this bucket.
    Grouped(ObservationKind),
    /// Lands alone and ends the run before it.
    Barrier,
    /// Lands alone, tallied under nothing.
    Standalone,
}

pub(crate) fn rail_place(what: &Observed) -> Option<RailPlace> {
    Some(match what {
        Observed::Read { .. } => RailPlace::Grouped(ObservationKind::Read),
        Observed::Grep { .. } => RailPlace::Grouped(ObservationKind::Grep),
        Observed::Command {
            origin: CommandOrigin::External | CommandOrigin::Detached,
            ..
        } => RailPlace::Grouped(ObservationKind::Exec),
        Observed::Write { .. } => RailPlace::Barrier,
        // A denial reads best whole, not dissolved into a tally; a birth
        // stays whole for the same reason — kept even where redundant, so
        // the rail can name what the trail names.
        Observed::Capability {
            decision: Decision::Denied,
            ..
        }
        | Observed::Worker { .. } => RailPlace::Standalone,
        // Desk-fed only: an `Act` never reaches the rail from the engine seam.
        Observed::Command { .. } | Observed::Capability { .. } | Observed::Act { .. } => {
            return None;
        }
    })
}

/// Compose an [`Observed`] into a [`Card`]: one [`Mark::Text`] heading of a
/// muted verb, the path, program, or resource as its [`Role::Path`] subject,
/// and the outcome, status, or decision roled by its level. A write is the one
/// observation that carries a body, and so the one that departs — see
/// [`write_card`].
pub fn observation_card(what: &Observed) -> Card {
    let spans = match what {
        Observed::Read { path } => read_spans(path),
        Observed::Write {
            path,
            outcome,
            new_bytes,
            old_bytes,
            ..
        } => {
            return write_card(path, *outcome, old_bytes.as_deref(), new_bytes.as_deref());
        }
        Observed::Command { argv, status, .. } => {
            let mut spans = vec![Span::plain("$ ")];
            spans.extend(exec_cmd_spans(argv));
            let role = if *status == 0 { Role::Ok } else { Role::Bad };
            spans.push(Span::plain(" → "));
            spans.push(Span::new(role, status.to_string()));
            spans
        }
        Observed::Grep { scope, pattern } => {
            let mut spans = vec![Span::new(Role::Muted, "grep ")];
            spans.extend(grep_spans(scope, pattern));
            spans
        }
        Observed::Capability {
            resource,
            decision,
            fields,
        } => capability_spans(resource, *decision, fields),
        Observed::Worker { id, cmd, class } => worker_spans(*id, cmd, *class),
        Observed::Act { .. } => {
            unreachable!("an `Act` never reaches the rail from the engine seam")
        }
    };
    Card(vec![Mark::Text { spans }])
}

/// A write's card: its [`write_preview`] under a `write <path> <outcome>`
/// heading — save when that preview is a diff, which names the file and shows
/// the change itself.  A heading above it would only say the same twice, so the
/// diff stands as the whole card and an `edit` reads as the edit it is.
fn write_card(path: &str, outcome: WriteOutcome, old: Option<&[u8]>, new: Option<&[u8]>) -> Card {
    let body = if outcome == WriteOutcome::Committed {
        write_preview(path, old, new)
    } else {
        Vec::new()
    };
    if let [Mark::Diff { .. }] = body.as_slice() {
        return Card(body);
    }
    let mut marks = vec![Mark::Text {
        spans: write_spans(path, outcome),
    }];
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
/// mode, which rides the observation only.
fn write_spans(path: &str, outcome: WriteOutcome) -> Vec<Span> {
    vec![
        Span::new(Role::Muted, "write "),
        Span::new(Role::Path, path),
        Span::plain(" "),
        Span::new(write_outcome_role(outcome), write_outcome_label(outcome)),
    ]
}

/// A capability check's heading, `check <resource> <decision> <fields…>`. The
/// decision roles `Role::Bad` when denied — the only decision the rail ever
/// surfaces, per the policy table in `evaluator/audit.rs`'s `observe_stamped`. The
/// trailing fields are core's own `fields` map (`name`/`resolved`/`args` for
/// `exec`, `op`/`path`/`granted` for `fs`) rendered as `key=value` pairs in
/// the map's own order — whatever is present, nothing inferred.
fn capability_spans(resource: &str, decision: Decision, fields: &Map) -> Vec<Span> {
    let mut spans = vec![
        Span::new(Role::Muted, "check "),
        Span::new(Role::Path, resource),
        Span::plain(" "),
        Span::new(
            if decision == Decision::Denied {
                Role::Bad
            } else {
                Role::Ok
            },
            decision.as_str(),
        ),
    ];
    if !fields.is_empty() {
        spans.push(Span::plain(" "));
        spans.push(Span::new(Role::Muted, capability_fields(fields)));
    }
    spans
}

fn lease_class_label(class: LeaseClass) -> &'static str {
    match class {
        LeaseClass::Worker => "worker",
        LeaseClass::Durable => "durable",
    }
}

/// A worker birth's heading, `worker #id cmd class` — a mark of its own,
/// never `spawn`'s `$ cmd → status` row, so a birth reads as what it is
/// rather than as another exec.
fn worker_spans(id: WorkerId, cmd: &str, class: LeaseClass) -> Vec<Span> {
    vec![
        Span::new(Role::Muted, "worker "),
        Span::new(Role::Path, format!("#{}", id.0)),
        Span::plain(" "),
        Span::new(Role::Code, cmd),
        Span::plain(" "),
        Span::new(Role::Muted, lease_class_label(class)),
    ]
}

fn capability_fields(fields: &Map) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
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
// Each reuses the exact `observation_card` span vocabulary, so a run of one
// renders like its own card, modulo the deliberate exec departure below.
// Writes and capability checks never reach here — each lands alone.

/// `read p1, read p2, …`
pub fn reads_card(reads: &[String]) -> Option<Card> {
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
/// Drops the ` → status` tail a lone [`observation_card`] exec row carries: a
/// joined run reads as the *set of commands run*, where per-command statuses
/// would be noise. Nothing is lost — each status still rides its own bus
/// event; only this presentation omits it.
pub fn execs_card(execs: &[Observed]) -> Option<Card> {
    if execs.is_empty() {
        return None;
    }
    let mut spans = vec![Span::plain("$ ")];
    join_spans(&mut spans, execs, |spans, e| {
        if let Observed::Command { argv, .. } = e {
            spans.extend(exec_cmd_spans(argv));
        }
    });
    Some(Card(vec![Mark::Text { spans }]))
}

/// `grep p1 in s1, p2 in s2, …` under one verb.
pub fn greps_card(greps: &[Observed]) -> Option<Card> {
    if greps.is_empty() {
        return None;
    }
    let mut spans = vec![Span::new(Role::Muted, "grep ")];
    join_spans(&mut spans, greps, |spans, e| {
        if let Observed::Grep { scope, pattern } = e {
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

/// The record leg: an observation's total wire form, the payload
/// `Display::Observation` carries — the one display content the protocol
/// records cannot supply, since a write's byte diff and a read's card never
/// enter the model's result string.
///
/// Total, never `Result`: `Observation::to_wire` already scrubs every leaf
/// `FOValue::try_from` rejects, so the conversion cannot fail in practice —
/// only in the sense that a bug in that scrub would be a bug worth a panic.
#[allow(
    dead_code,
    reason = "P4 of dev/docs/plans/260814_one_seam_one_log.md: the commit producer (P2) calls this at its Display::Observation emit site, landing concurrently"
)]
pub(crate) fn observation_wire(event: &Observation) -> FOValue {
    FOValue::try_from(&event.to_wire())
        .expect("Observation::to_wire scrubs every leaf FOValue::try_from rejects")
}

/// The decode leg, inverse of [`observation_wire`]: rebuilds the
/// [`Observation`] a `Display::Observation` or `Display::ObservationGroup`
/// record carried, for a renderer to hand to [`observation_card`].
pub fn observation_from_wire(value: FOValue) -> Option<Observation> {
    Observation::from_value(&RalValue::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::types::AuditIo;

    /// The card's first [`Mark::Text`] flattened — the on-screen line, roling
    /// dropped.
    fn line(card: &Card) -> String {
        let Card(marks) = card;
        match &marks[0] {
            Mark::Text { spans } => spans.iter().map(|s| s.text.as_str()).collect(),
            _ => panic!("expected a text mark"),
        }
    }

    fn command(argv: &[&str], status: i32) -> Observed {
        Observed::Command {
            argv: argv.iter().map(ToString::to_string).collect(),
            status,
            origin: CommandOrigin::External,
            io: AuditIo::default(),
            error: None,
            value: RalValue::Unit,
        }
    }

    #[test]
    fn exec_requotes_only_where_the_shell_would_reparse() {
        let cmd = |argv: &[&str]| -> String {
            let full = line(&observation_card(&command(argv, 0)));
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
    fn capability_card_denies_role_bad_and_shows_its_fields() {
        let card = observation_card(&Observed::Capability {
            resource: "fs".into(),
            decision: Decision::Denied,
            fields: [
                ("op".to_string(), RalValue::String("write".into())),
                ("path".to_string(), RalValue::String("/etc/passwd".into())),
            ]
            .into_iter()
            .collect(),
        });
        assert_eq!(line(&card), "check fs denied op=write path=/etc/passwd");
        let Card(marks) = &card;
        let Mark::Text { spans } = &marks[0] else {
            panic!("expected a text mark")
        };
        let denied = spans
            .iter()
            .find(|s| s.text == "denied")
            .expect("a decision span");
        assert_eq!(denied.role, Some(Role::Bad));
    }
}
