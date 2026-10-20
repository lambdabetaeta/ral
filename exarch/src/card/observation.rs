//! Card composition over core's one observation vocabulary
//! (`ral_core::types::Observed`): a command settled, a redirect read opened,
//! a grep ran, a capability check was denied.  A write lands as a
//! [`Change`](super::Change) instead.  Decoding
//! the surfaced value back into an `Observation` is core's own
//! `Observation::from_surface`, called at `shell_eval::surface::decode_surface`;
//! this module only renders what core already decoded.

use std::borrow::Cow;

use ral_core::fact::{Check, Command, Grep, Read, Worker};
use ral_core::types::{Decision, Observed, WorkerId};
use std::collections::BTreeMap;

use super::{Card, Mark, Role, Span};

/// An observation the rail draws, by where it lands; evaluation, a write (drawn
/// as its change), an allowed check and a desk-fed act have none.
pub(crate) enum Landed {
    /// Folds onto the call above it, which buckets the fact itself.
    Effect(Effect),
    /// Its own bounded block: a denial reads best whole, not dissolved into a tally.
    Surfaced(Check),
    /// A line on the rail, the mirror of the settlement it departs for: a worker's birth.
    Announced(Worker),
}

#[derive(Clone, Debug)]
pub(crate) enum Effect {
    Read(Read),
    Command(Command),
    Grep(Grep),
}

impl Landed {
    pub(crate) fn of(what: &Observed) -> Option<Self> {
        Some(match what {
            Observed::Read(r) => Self::Effect(Effect::Read(r.clone())),
            Observed::Command(c) => Self::Effect(Effect::Command(c.clone())),
            Observed::Grep(g) => Self::Effect(Effect::Grep(g.clone())),
            Observed::Check(c) if c.decision() == Decision::Denied => Self::Surfaced(c.clone()),
            Observed::Worker(w) => Self::Announced(w.clone()),
            Observed::Write(_) | Observed::Check(_) | Observed::Act(_) => return None,
        })
    }
}

/// The one line a [`Landed`] reads as.
///
/// A muted verb, the path, program, or resource as its [`Role::Path`] subject,
/// and the outcome, status, or decision roled by its level.  A card wraps this
/// as its heading; a [`Landed::Announced`] observation is only this, drawn
/// on the rail.
pub(crate) fn observation_spans(what: &Landed) -> Vec<Span> {
    match what {
        Landed::Effect(Effect::Read(r)) => reads_listing(&[&r.path]).spans(),
        Landed::Effect(Effect::Command(c)) => {
            let mut spans = exec_listing(&c.argv).spans();
            let role = if c.status == 0 { Role::Ok } else { Role::Bad };
            spans.push(Span::plain(" → "));
            spans.push(Span::new(role, c.status.to_string()));
            spans
        }
        Landed::Effect(Effect::Grep(g)) => greps_listing(&[g]).spans(),
        Landed::Surfaced(check) => capability_spans(check),
        Landed::Announced(w) => worker_spans(w.id, &w.title),
    }
}

/// Compose a [`Landed`] into a [`Card`]: its [`observation_spans`] line.
pub(crate) fn observation_card(what: &Landed) -> Card {
    Card(vec![Mark::Text {
        spans: observation_spans(what),
    }])
}

/// An effect as a head and the items after it, which a renderer may fold or
/// clip between but never within.  A lone surface and the mirror's grouped
/// rows both read one, so the two cannot drift apart.
pub(crate) struct Listing {
    pub(crate) head: Vec<Span>,
    pub(crate) items: Vec<Vec<Span>>,
    pub(crate) sep: &'static str,
}

impl Listing {
    fn spans(self) -> Vec<Span> {
        let mut spans = self.head;
        for (i, item) in self.items.into_iter().enumerate() {
            spans.push(Span::plain(if i == 0 { " " } else { self.sep }));
            spans.extend(item);
        }
        spans
    }
}

/// `$ prog arg…`, one item per argument.
///
/// The surfaced `argv` is post-shell — already word-split, quotes consumed — so
/// each token is re-quoted *only* where the shell would otherwise reparse it,
/// and the rendered line word-splits back to the exact argv rather than to a
/// lie. `try_quote`'s sole error is an interior nul, which no real argv carries.
pub(crate) fn exec_listing(argv: &[String]) -> Listing {
    let quote = |t: &str| shlex::try_quote(t).map_or_else(|_| t.to_string(), Cow::into_owned);
    let (prog, args) = match argv.split_first() {
        Some((prog, args)) => (Span::new(Role::Path, quote(prog)), args),
        None => (Span::plain("(no command)"), &[][..]),
    };
    Listing {
        head: vec![Span::plain("$ "), prog],
        items: args.iter().map(|a| vec![Span::plain(quote(a))]).collect(),
        sep: " ",
    }
}

/// `read p1, p2, …`
pub(crate) fn reads_listing(paths: &[&str]) -> Listing {
    Listing {
        head: vec![Span::new(Role::Muted, "read")],
        items: paths
            .iter()
            .map(|p| vec![Span::new(Role::Path, *p)])
            .collect(),
        sep: ", ",
    }
}

/// `grep p1 in s1, p2 in s2, …`
pub(crate) fn greps_listing(greps: &[&Grep]) -> Listing {
    let item = |g: &&Grep| {
        vec![
            Span::new(Role::Code, &g.pattern),
            Span::plain(" in "),
            Span::new(Role::Path, &g.scope),
        ]
    };
    Listing {
        head: vec![Span::new(Role::Muted, "grep")],
        items: greps.iter().map(item).collect(),
        sep: ", ",
    }
}

/// A capability check's heading, `check <resource> <decision> <fields…>`. The
/// decision roles `Role::Bad` when denied — the only decision the rail ever
/// surfaces, per [`Landed::of`]: core's door (`Shell::observe_stamped` in
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

/// A worker birth's heading, `errand #id started · title` — a mark of its
/// own, never `spawn`'s `$ cmd → status` row, so a birth reads as what it is
/// rather than as another exec.
fn worker_spans(id: WorkerId, title: &str) -> Vec<Span> {
    vec![
        Span::new(Role::Muted, "errand "),
        Span::new(Role::Path, format!("#{}", id.0)),
        Span::new(Role::Muted, " started · "),
        Span::new(Role::Code, title),
    ]
}

fn capability_fields(fields: &BTreeMap<String, String>) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
