//! How a worker's completion reads.
//!
//! Core flushes a single `` `done `` value at the end of a background block's
//! deferred buffer; [`value_to_done`] decodes it, [`settled_spans`] words it.

use jiff::fmt::friendly::{Designator, Spacing, SpanPrinter};
use ral_core::data::FOValue;
use ral_core::shell::runtime::{Done, DoneEvent};
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

/// Decode a `` `done `` value into the worker's id, title, run time and how
/// it settled, `None` for anything else — and since
/// `shell_eval::surface::decode_surface` tries this branch last, `None` drops
/// the value rather than passing it on.
pub(crate) fn value_to_done(v: &FOValue) -> Option<(u64, String, Duration, Done)> {
    let DoneEvent {
        id,
        title,
        elapsed_ms,
        outcome,
    } = DoneEvent::from_surface(v)?;
    Some((id.0, title, Duration::from_millis(elapsed_ms), outcome))
}

/// How a settled worker reads: its id and title, how long it ran, and muted
/// prose around the outcome.
pub fn settled_spans(
    id: Option<u64>,
    title: &str,
    elapsed: Option<Duration>,
    outcome: &Done,
) -> Vec<Span> {
    let (how, detail) = outcome.parts();
    let role = if matches!(outcome, Done::Ok(())) {
        Role::Ok
    } else {
        Role::Bad
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
        Span::new(role, how.into_owned()),
        Span::new(Role::Muted, ")"),
    ]);
    if let Some(detail) = detail {
        spans.push(Span::new(Role::Muted, format!(": {detail}")));
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
    outcome: &Done,
) -> String {
    settled_spans(id, title, elapsed, outcome)
        .iter()
        .map(|s| s.text.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::process::CancelCause;
    use ral_core::shell::runtime::{ErrorFact, Reason};

    fn done(outcome: Done) -> FOValue {
        DoneEvent {
            id: ral_core::shell::runtime::WorkerId(7),
            title: "building".into(),
            elapsed_ms: 192_000,
            outcome,
        }
        .to_surface()
    }

    #[test]
    fn value_to_done_decodes_each_outcome() {
        let named = |outcome| Some((7, "building".to_string(), Duration::from_secs(192), outcome));
        for outcome in [
            Done::Ok(()),
            Done::Err(
                ErrorFact::new(
                    Reason::raised("boom"),
                    "run".into(),
                    ral_core::test_helper::site(),
                )
                .record(),
            ),
            Done::Panic("kaput".into()),
            Done::Exited(3),
            Done::Stopped(CancelCause::Cancelled),
        ] {
            assert_eq!(value_to_done(&done(outcome.clone())), named(outcome));
        }
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
}
