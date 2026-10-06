//! How a detached worker's completion reads.
//!
//! Core flushes a single `` `done `` value at the end of a background block's
//! deferred buffer; [`value_to_done`] decodes it, [`settled_spans`] words it.

use ral_core::first_order::FOValue;
use ral_core::types::{Done, DoneEvent};

use super::{Role, Span};
use crate::record::DoneOutcome;

/// Decode a `` `done `` value into the worker's `cmd` and how it settled,
/// `None` for anything else — and since `decode_surface` in `shell_eval.rs`
/// tries this branch last, `None` drops the value rather than passing it on.
pub(crate) fn value_to_done(v: &FOValue) -> Option<(String, DoneOutcome)> {
    let DoneEvent { cmd, outcome } = DoneEvent::from_surface(v)?;
    let outcome = match outcome {
        Done::Ok => DoneOutcome::Ok,
        Done::Err(rec) => DoneOutcome::Err {
            message: rec.message().to_owned(),
            status: rec.status().into(),
        },
        Done::Panic(message) => DoneOutcome::Panic { message },
    };
    Some((cmd, outcome))
}

/// How a settled worker reads: its name, and muted prose around the outcome.
///
/// The outcome alone carries a level, roled `ok`/`bad` exactly as the
/// `$ cmd → status` exec row roles an exit code — which is what a settled
/// block's is.
pub fn settled_spans(cmd: &str, outcome: &DoneOutcome) -> Vec<Span> {
    let (role, how, message) = match outcome {
        DoneOutcome::Ok => (Role::Ok, "exit 0".to_string(), ""),
        DoneOutcome::Err { message, status } => {
            (Role::Bad, format!("exit {status}"), message.as_str())
        }
        DoneOutcome::Panic { message } => (Role::Bad, "panic".to_string(), message.as_str()),
    };
    let mut spans = vec![
        Span::new(Role::Muted, "background "),
        Span::new(Role::Strong, cmd),
        Span::new(Role::Muted, " settled ("),
        Span::new(role, how),
        Span::new(Role::Muted, ")"),
    ];
    if !message.is_empty() {
        spans.push(Span::new(Role::Muted, format!(": {message}")));
    }
    spans
}

/// [`settled_spans`] flattened, for the two sinks that have no ink to spend:
/// the headless stderr tee and the model's wake-up notice (`surface_notice` in
/// `bus/post.rs`).
pub fn settled_text(cmd: &str, outcome: &DoneOutcome) -> String {
    settled_spans(cmd, outcome)
        .iter()
        .map(|s| s.text.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{card_value, map_value, s};
    use super::*;
    use ral_core::types::{ErrorRecord, Status};

    fn done(outcome: Done) -> FOValue {
        DoneEvent {
            cmd: "block at turn 1, line 1".into(),
            outcome,
        }
        .to_surface()
    }

    #[test]
    fn value_to_done_decodes_each_outcome() {
        let named = |outcome| Some(("block at turn 1, line 1".to_string(), outcome));
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
    fn value_to_done_rejects_non_done_values() {
        assert!(value_to_done(&card_value(vec![])).is_none());
        assert!(value_to_done(&map_value(vec![("kind", s("read"))])).is_none());
        assert!(value_to_done(&s("plain")).is_none());
    }
}
