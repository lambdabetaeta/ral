//! Core's own housekeeping, spoken at the run's ready boundary.
//!
//! A [`Notice`] names a worker the lease chain reaped or a run of idle
//! top-level bindings the ledger pruned — decoded once from its raw
//! `` `notice `` value and rendered as a dim one-liner. It is a fact about
//! the run, never a message to the model.
//!
//! The host-authored services ledger pin lives here too — the same author,
//! core's own housekeeping, but state rather than event.

use super::value::{map_of, str_field};
use super::{Card, Field, FieldVal, Mark, Role, Span};
use ral_core::Value as RalValue;

/// The decoded body of a `` `notice `` surface event core's own engine
/// pushes at a run's ready boundary.
///
/// The notice names a worker the lease chain
/// reaped or a run of idle top-level bindings the ledger pruned.
/// Like
/// [`crate::bus::card::DoneOutcome`], the raw record [`value_to_notice`] decodes once and
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
    Prune {
        names: Vec<String>,
        idle_calls: Vec<u64>,
    },
}

/// Decode a `` `notice `` value into its [`Notice`].
///
/// The shape is
/// `` `notice [kind: `reap|`prune, …fields] `` where `kind`
/// selects the fields read below — exactly the two surface classes core's
/// `emit_ready_boundary_notices` pushes. Anything else — an unrecognised
/// `kind`, a missing field, a value that is not this variant at all —
/// returns `None`; the decoder seam falls through.
pub(crate) fn value_to_notice(v: &RalValue) -> Option<Notice> {
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
        "prune" => {
            let RalValue::List(names) = m.get("names")? else {
                return None;
            };
            let names: Vec<String> = names
                .iter()
                .map(|v| match v {
                    RalValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect::<Option<_>>()?;
            let RalValue::List(idle) = m.get("idle-calls")? else {
                return None;
            };
            let idle_calls: Vec<u64> = idle
                .iter()
                .map(|v| match v {
                    RalValue::Int(i) => {
                        #[allow(
                            clippy::cast_sign_loss,
                            reason = "max(0) floors to a non-negative call count"
                        )]
                        let n = (*i).max(0) as u64;
                        Some(n)
                    }
                    _ => None,
                })
                .collect::<Option<_>>()?;
            if names.len() != idle_calls.len() {
                return None;
            }
            Notice::Prune { names, idle_calls }
        }
        _ => return None,
    })
}

/// Compose a decoded [`Notice`] into its one-line [`crate::bus::card::Card`] — dispatching to
/// the variant's own composer, each a dim one-liner naming what happened.
pub(crate) fn notice_card(notice: &Notice) -> Card {
    match notice {
        Notice::Reap { cmd, cause } => reap_card(cmd, *cause),
        Notice::Prune { names, idle_calls } => bindings_pruned_card(names, idle_calls),
    }
}

/// Compose one policy removal into its one-line [`crate::bus::card::Card`] — the reap's
/// analogue of [`crate::bus::card::done_card`]: a worker the registry removed by policy
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
        Span::new(Role::Warn, "reaped"),
        Span::plain(format!("  {cmd} — {phrase}")),
    ];
    Card(vec![Mark::Text { spans }])
}

/// Compose one prune pass's notice into a [`crate::bus::card::Card`] — `reap_card`'s
/// binding-lease sibling: a dim one-liner naming every pruned name and the
/// idle bound each met, e.g. `pruned 3 idle bindings: rows, tmp, out
/// (unused >= 256 calls)`. The displayed count is the *minimum* idle-call
/// age across `idle_calls` — every pruned name was idle at least that
/// long, so the figure is truthful even when a multi-name prune's
/// individual ages differ.
fn bindings_pruned_card(names: &[String], idle_calls: &[u64]) -> Card {
    let min_idle = idle_calls.iter().min().copied().unwrap_or_default();
    let phrase = format!(
        "pruned {} idle binding{}: {} (unused >= {min_idle} calls)",
        names.len(),
        if names.len() == 1 { "" } else { "s" },
        names.join(", "),
    );
    let spans = vec![Span::new(Role::Muted, phrase)];
    Card(vec![Mark::Text { spans }])
}

// ── `services`: the host-owned durable-service ledger ────────────────────

/// Compose every live durable service into one ledger card — one
/// [`crate::bus::card::Mark::Fields`] row per service, labelled by the id `service-handle`
/// takes and valued by its birth description and age.  Host-authored only
/// ([`Agent::reconcile_service_pins`](crate::agent::Agent::reconcile_service_pins)): a durable service's whole bound is
/// legibility, and this pin is what makes the live set legible.
pub(crate) fn services_pin_card(services: &[crate::agent::ProbedWorker]) -> Card {
    let rows = services
        .iter()
        .map(|entry| Field {
            label: format!("service {}", entry.id),
            value: FieldVal::Inline(vec![Span::plain(format!(
                "{}  (up {}s)",
                entry.cmd, entry.up_secs
            ))]),
        })
        .collect();
    Card(vec![Mark::Fields { rows }])
}
