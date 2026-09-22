//! Core's own housekeeping at the run's ready boundary: a worker the lease
//! chain reaped, or idle top-level bindings the ledger pruned, decoded from
//! `` `notice `` and composed into a one-liner.

use super::value::{record, str_field};
use super::{Card, Mark, Role, Span};
use ral_core::serial::FOValue;

/// The decoded body of a `` `notice `` event, minted by core's
/// `Shell::emit_ready_boundary_notices` once per settled run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Notice {
    Reap {
        cmd: String,
        cause: ral_core::types::ReapCause,
    },
    /// One notice covers a whole boundary's prune; `idle_calls` rides
    /// parallel to `names`.
    Prune {
        names: Vec<String>,
        idle_calls: Vec<u64>,
    },
}

/// Decode a `` `notice `` value into its [`Notice`].
///
/// Anything unrecognised — a foreign `kind`, a missing field, another variant
/// entirely — answers `None`, and `shell_eval::decode_surface` tries the next
/// shape rather than dropping the value.
pub(crate) fn value_to_notice(v: &FOValue) -> Option<Notice> {
    let FOValue::Variant { label, payload } = v else {
        return None;
    };
    if label != "notice" {
        return None;
    }
    let m = record(payload.as_deref()?)?;
    let FOValue::Variant { label: kind, .. } = m.field("kind")? else {
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
            let names: Vec<String> = m
                .field("names")?
                .as_list()?
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<_>>()?;
            let idle_calls: Vec<u64> = m
                .field("idle-calls")?
                .as_list()?
                .iter()
                .map(|v| {
                    #[allow(
                        clippy::cast_sign_loss,
                        reason = "max(0) floors to a non-negative call count"
                    )]
                    let n = v.as_int()?.max(0) as u64;
                    Some(n)
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

/// Compose a decoded [`Notice`] into its one-line [`Card`].
pub fn notice_card(notice: &Notice) -> Card {
    match notice {
        Notice::Reap { cmd, cause } => reap_card(cmd, *cause),
        Notice::Prune { names, idle_calls } => bindings_pruned_card(names, idle_calls),
    }
}

/// One policy removal as a warned one-liner.  Unlike `settled_spans` it names the
/// `cmd`: once the worker's registry entry is gone, nothing else says which one
/// was.
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

/// Every pruned name on one muted line.  The age quoted is the *minimum*
/// across `idle_calls`, which stays truthful when a multi-name prune's
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
