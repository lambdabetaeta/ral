//! Card composition over core's one observation vocabulary
//! (`ral_core::types::Observed`): a command settled, a redirect read opened,
//! a grep ran, a capability check was denied.  A write lands as a
//! [`Change`](super::Change) instead.  Decoding
//! the surfaced value back into an `Observation` is core's own
//! `Observation::from_surface`, called at `shell_eval.rs`'s `decode_surface`;
//! this module only renders what core already decoded.

use std::borrow::Cow;

use ral_core::types::{Check, Decision, LeaseClass, Observed, WorkerId};
use std::collections::BTreeMap;

use super::{Card, Mark, Role, Span};

/// Where an observation the rail draws lands, or `None` for one it does not
/// draw: evaluation (a `builtin` command), or a capability check that was not
/// a denial. Core reports every observation it makes; this is where the host
/// says which of them it wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Landing {
    /// Folds onto the call above it, which buckets the fact itself.
    Effect,
    /// Its own bounded block — a denial, a surfaced kit card, a notice.
    Surfaced,
    /// A line on the rail rather than a card: a worker's birth.
    Announced,
}

pub(crate) fn landing(what: &Observed) -> Option<Landing> {
    Some(match what {
        Observed::Read(_) | Observed::Grep(_) | Observed::Command(_) => Landing::Effect,
        // A denial reads best whole, not dissolved into a tally.
        Observed::Check(check) if check.decision() == Decision::Denied => Landing::Surfaced,
        // A birth is the departure a settlement is the arrival of, and reads
        // as that mirror.
        Observed::Worker(_) => Landing::Announced,
        // A write lands as the change it made, decoded before it gets here; a
        // flagged check is the trail's alone; an `Act` is desk-fed and never
        // reaches the rail from the engine seam.
        Observed::Write(_) | Observed::Check(_) | Observed::Act(_) => {
            return None;
        }
    })
}

/// The one line an [`Observed`] reads as.
///
/// A muted verb, the path, program, or resource as its [`Role::Path`] subject,
/// and the outcome, status, or decision roled by its level.  A card wraps this
/// as its heading; a [`Landing::Announced`] observation is only this, drawn
/// on the rail.
pub fn observation_spans(what: &Observed) -> Vec<Span> {
    match what {
        Observed::Read(r) => read_spans(&r.path),
        Observed::Write(w) => super::change::heading(&w.path, w.outcome),
        Observed::Command(c) => {
            let mut spans = vec![Span::plain("$ ")];
            spans.extend(exec_cmd_spans(&c.argv));
            let role = if c.status == 0 { Role::Ok } else { Role::Bad };
            spans.push(Span::plain(" → "));
            spans.push(Span::new(role, c.status.to_string()));
            spans
        }
        Observed::Grep(g) => {
            let mut spans = vec![Span::new(Role::Muted, "grep ")];
            spans.extend(grep_spans(&g.scope, &g.pattern));
            spans
        }
        Observed::Check(check) => capability_spans(check),
        Observed::Worker(w) => worker_spans(w.id, &w.cmd, w.class),
        Observed::Act(_) => {
            unreachable!("an `Act` never reaches the rail from the engine seam")
        }
    }
}

/// Compose an [`Observed`] into a [`Card`]: its [`observation_spans`] line.
pub fn observation_card(what: &Observed) -> Card {
    Card(vec![Mark::Text {
        spans: observation_spans(what),
    }])
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

/// A capability check's heading, `check <resource> <decision> <fields…>`. The
/// decision roles `Role::Bad` when denied — the only decision the rail ever
/// surfaces, per [`landing`]: core's door (`Shell::observe_stamped` in
/// `core/src/types/audit/door.rs`) broadcasts every fact and leaves the rail's
/// policy to the host. The
/// trailing fields are core's own `fields` map (`name`/`resolved`/`args` for
/// `exec`, `op`/`path` for `fs`, `prefix` for `deputy`) rendered as
/// `key=value` pairs in the map's own order — whatever is present, nothing
/// inferred.
fn capability_spans(check: &Check) -> Vec<Span> {
    let (decision, fields) = (check.decision(), &check.fields);
    let mut spans = vec![
        Span::new(Role::Muted, "check "),
        Span::new(Role::Path, <&str>::from(check.resource)),
        Span::plain(" "),
        Span::new(
            if decision == Decision::Denied {
                Role::Bad
            } else {
                Role::Ok
            },
            <&str>::from(decision),
        ),
    ];
    if !fields.is_empty() {
        spans.push(Span::plain(" "));
        spans.push(Span::new(Role::Muted, capability_fields(fields)));
    }
    spans
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
        Span::new(Role::Muted, <&str>::from(class)),
    ]
}

fn capability_fields(fields: &BTreeMap<String, String>) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Observation groups: a call's effects of one kind → one card ─────────────
//
// Each reuses the exact `observation_card` span vocabulary, so a run of one
// renders like its own card, modulo the deliberate exec departure below.
// Capability checks never reach here: a denial lands alone.

/// `read p1, read p2, …`
pub(crate) fn reads_card(reads: &[&str]) -> Option<Card> {
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
pub(crate) fn execs_card(execs: &[&Observed]) -> Option<Card> {
    if execs.is_empty() {
        return None;
    }
    let mut spans = vec![Span::plain("$ ")];
    join_spans(&mut spans, execs, |spans, e| {
        if let Observed::Command(c) = *e {
            spans.extend(exec_cmd_spans(&c.argv));
        }
    });
    Some(Card(vec![Mark::Text { spans }]))
}

/// `grep p1 in s1, p2 in s2, …` under one verb.
pub(crate) fn greps_card(greps: &[&Observed]) -> Option<Card> {
    if greps.is_empty() {
        return None;
    }
    let mut spans = vec![Span::new(Role::Muted, "grep ")];
    join_spans(&mut spans, greps, |spans, e| {
        if let Observed::Grep(g) = *e {
            spans.extend(grep_spans(&g.scope, &g.pattern));
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
mod tests;
