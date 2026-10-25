//! How a worker's completion reads.
//!
//! Core flushes a single `` `done `` value at the end of a background block's
//! deferred buffer; [`value_to_done`] decodes it, [`settled_spans`] words it.

use jiff::fmt::friendly::{Designator, Spacing, SpanPrinter};
use ral_core::data::FOValue;
use ral_core::types::{Done, DoneEvent};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{Role, Span};

/// How long something took, in prose: `47 secs`, `1 min 12 secs`, `1 hr 3 mins`.
/// Whole seconds — a runtime is not a stopwatch reading — and jiff balances
/// the units up to hours on its own.
pub fn elapsed_phrase(elapsed: Duration) -> String {
    static PRINTER: SpanPrinter = SpanPrinter::new()
        .designator(Designator::Short)
        .spacing(Spacing::BetweenUnitsAndDesignators);
    PRINTER.unsigned_duration_to_string(&Duration::from_secs(elapsed.as_secs()))
}

/// A worker's `` `done `` completion, minus the one-line card the
/// view fold rebuilds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum DoneOutcome {
    Ok,
    Err { message: String, status: i64 },
    Exited { status: i64 },
    Stopped { cause: String },
    Panic { message: String },
}

/// Decode a `` `done `` value into the worker's id, title, run time and how
/// it settled, `None` for anything else — and since
/// `shell_eval::surface::decode_surface` tries this branch last, `None` drops
/// the value rather than passing it on.
pub(crate) fn value_to_done(v: &FOValue) -> Option<(u64, String, Duration, DoneOutcome)> {
    let DoneEvent {
        id,
        title,
        elapsed_ms,
        outcome,
    } = DoneEvent::from_surface(v)?;
    let outcome = match outcome {
        Done::Ok => DoneOutcome::Ok,
        Done::Err(rec) => DoneOutcome::Err {
            message: rec.message().to_owned(),
            status: rec.status().into(),
        },
        Done::Exited(n) => DoneOutcome::Exited { status: n.into() },
        Done::Stopped(c) => DoneOutcome::Stopped {
            cause: c.message().to_owned(),
        },
        Done::Panic(message) => DoneOutcome::Panic { message },
    };
    Some((id.0, title, Duration::from_millis(elapsed_ms), outcome))
}

/// How a settled worker reads: its id and title, how long it ran, and muted
/// prose around the outcome.
///
/// The outcome alone carries a level, roled `ok`/`bad` exactly as the
/// `$ cmd → status` exec row roles an exit code — which is what a settled
/// block's is.
pub fn settled_spans(
    id: Option<u64>,
    title: &str,
    elapsed: Option<Duration>,
    outcome: &DoneOutcome,
) -> Vec<Span> {
    let (role, how, message) = match outcome {
        DoneOutcome::Ok => (Role::Ok, "exit 0".to_string(), ""),
        DoneOutcome::Err { message, status } => {
            (Role::Bad, format!("exit {status}"), message.as_str())
        }
        DoneOutcome::Exited { status } => (Role::Bad, format!("exited {status}"), ""),
        DoneOutcome::Stopped { cause } => (Role::Bad, cause.clone(), ""),
        DoneOutcome::Panic { message } => (Role::Bad, "panic".to_string(), message.as_str()),
    };
    let mut spans = vec![
        Span::new(Role::Muted, "errand"),
        Span::new(
            Role::Path,
            id.map_or_else(String::new, |id| format!(" #{id}")),
        ),
        Span::new(Role::Muted, " ("),
        Span::new(Role::Strong, title),
        Span::new(Role::Muted, ") finished"),
    ];
    if let Some(elapsed) = elapsed {
        spans.push(Span::new(
            Role::Muted,
            format!(" after {}", elapsed_phrase(elapsed)),
        ));
    }
    spans.extend([
        Span::new(Role::Muted, " ("),
        Span::new(role, how),
        Span::new(Role::Muted, ")"),
    ]);
    if !message.is_empty() {
        spans.push(Span::new(Role::Muted, format!(": {message}")));
    }
    spans
}

/// [`settled_spans`] flattened, for the two sinks that have no ink to spend:
/// the headless stderr tee and the model's wake-up notice (`surface_notice` in
/// `bus/post.rs`).
pub fn settled_text(
    id: Option<u64>,
    title: &str,
    elapsed: Option<Duration>,
    outcome: &DoneOutcome,
) -> String {
    settled_spans(id, title, elapsed, outcome)
        .iter()
        .map(|s| s.text.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::types::{ErrorRecord, Status};

    fn done(outcome: Done) -> FOValue {
        DoneEvent {
            id: ral_core::types::WorkerId(7),
            title: "building".into(),
            elapsed_ms: 192_000,
            outcome,
        }
        .to_surface()
    }

    #[test]
    fn value_to_done_decodes_each_outcome() {
        let named = |outcome| Some((7, "building".to_string(), Duration::from_secs(192), outcome));
        assert_eq!(value_to_done(&done(Done::Ok)), named(DoneOutcome::Ok));
        let err = ErrorRecord::new("<runtime>", &Status::Raised(2), "boom", None);
        assert_eq!(
            value_to_done(&done(Done::Err(err))),
            named(DoneOutcome::Err {
                message: "boom".into(),
                status: 2,
            })
        );
        assert_eq!(
            value_to_done(&done(Done::Panic("kaput".into()))),
            named(DoneOutcome::Panic {
                message: "kaput".into(),
            })
        );
    }

    #[test]
    fn errand_completion_keeps_its_identity_and_failure() {
        assert_eq!(
            settled_text(
                Some(7),
                "building",
                Some(Duration::from_secs(192)),
                &DoneOutcome::Err {
                    message: "build failed".into(),
                    status: 2,
                }
            ),
            "errand #7 (building) finished after 3 mins 12 secs (exit 2): build failed"
        );
        assert_eq!(
            settled_text(Some(7), "building", None, &DoneOutcome::Ok),
            "errand #7 (building) finished (exit 0)"
        );
    }

    /// The one duration prose every runtime line shares: whole seconds,
    /// balanced up to hours, no unit that would read as zero.
    #[test]
    fn elapsed_reads_as_prose() {
        let phrase = |secs| elapsed_phrase(Duration::from_secs(secs));
        assert_eq!(phrase(0), "0 secs");
        assert_eq!(phrase(1), "1 sec");
        assert_eq!(phrase(47), "47 secs");
        assert_eq!(phrase(72), "1 min 12 secs");
        assert_eq!(phrase(120), "2 mins");
        assert_eq!(phrase(3780), "1 hr 3 mins");
    }

    /// Sub-second precision is noise in a breadcrumb: a run of 12.7s reports
    /// the seconds, not the stopwatch.
    #[test]
    fn elapsed_truncates_below_the_second() {
        assert_eq!(elapsed_phrase(Duration::from_millis(12_700)), "12 secs");
    }
}
