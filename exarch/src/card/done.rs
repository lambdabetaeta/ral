//! How a detached worker's completion reads.
//!
//! Core flushes a single `` `done `` value at the end of a background block's
//! deferred buffer; [`value_to_done`] decodes it, [`settled_spans`] words it.

use ral_core::first_order::FOValue;
use ral_core::types::{Done, DoneEvent};
use serde::{Deserialize, Serialize};

use super::{Role, Span};

/// A detached worker's `` `done `` completion, minus the one-line card the
/// view fold rebuilds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum DoneOutcome {
    Ok,
    Err { message: String, status: i64 },
    Panic { message: String },
}

/// Decode a `` `done `` value into the worker's `cmd` and how it settled,
/// `None` for anything else — and since `decode_surface` in `shell_eval.rs`
/// tries this branch last, `None` drops the value rather than passing it on.
pub(crate) fn value_to_done(v: &FOValue) -> Option<(u64, String, DoneOutcome)> {
    let DoneEvent { id, cmd, outcome } = DoneEvent::from_surface(v)?;
    let outcome = match outcome {
        Done::Ok => DoneOutcome::Ok,
        Done::Err(rec) => DoneOutcome::Err {
            message: rec.message().to_owned(),
            status: rec.status().into(),
        },
        Done::Panic(message) => DoneOutcome::Panic { message },
    };
    Some((id.0, cmd, outcome))
}

/// How a settled worker reads: its name, and muted prose around the outcome.
///
/// The outcome alone carries a level, roled `ok`/`bad` exactly as the
/// `$ cmd → status` exec row roles an exit code — which is what a settled
/// block's is.
pub fn settled_spans(id: Option<u64>, cmd: &str, outcome: &DoneOutcome) -> Vec<Span> {
    let (role, how, message) = match outcome {
        DoneOutcome::Ok => (Role::Ok, "exit 0".to_string(), ""),
        DoneOutcome::Err { message, status } => {
            (Role::Bad, format!("exit {status}"), message.as_str())
        }
        DoneOutcome::Panic { message } => (Role::Bad, "panic".to_string(), message.as_str()),
    };
    let mut spans = vec![
        Span::new(Role::Muted, "errand"),
        Span::new(
            Role::Path,
            id.map_or_else(String::new, |id| format!(" #{id}")),
        ),
        Span::new(Role::Muted, " finished ("),
        Span::new(role, how),
        Span::new(Role::Muted, ") · "),
        Span::new(Role::Strong, cmd.strip_prefix("block at ").unwrap_or(cmd)),
    ];
    if !message.is_empty() {
        spans.push(Span::new(Role::Muted, format!(": {message}")));
    }
    spans
}

/// [`settled_spans`] flattened, for the two sinks that have no ink to spend:
/// the headless stderr tee and the model's wake-up notice (`surface_notice` in
/// `bus/post.rs`).
pub fn settled_text(id: Option<u64>, cmd: &str, outcome: &DoneOutcome) -> String {
    settled_spans(id, cmd, outcome)
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
            id: ral_core::types::WorkerId(7),
            cmd: "block at turn 1, line 1".into(),
            outcome,
        }
        .to_surface()
    }

    #[test]
    fn value_to_done_decodes_each_outcome() {
        let named = |outcome| Some((7, "block at turn 1, line 1".to_string(), outcome));
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
            settled_text(Some(7), "block at turn 1, line 3", &DoneOutcome::Ok),
            "errand #7 finished (exit 0) · turn 1, line 3"
        );
        assert_eq!(
            settled_text(
                Some(7),
                "build release",
                &DoneOutcome::Err {
                    message: "build failed".into(),
                    status: 2,
                }
            ),
            "errand #7 finished (exit 2) · build release: build failed"
        );
    }

    #[test]
    fn older_completion_records_remain_readable() {
        let record: crate::record::Display =
            serde_json::from_str(r#"{"kind":"done","cmd":"build","outcome":{"outcome":"ok"}}"#)
                .expect("completion without an id");
        assert!(matches!(
            record,
            crate::record::Display::Done { id: None, .. }
        ));
        assert_eq!(
            settled_text(None, "build", &DoneOutcome::Ok),
            "errand finished (exit 0) · build"
        );
    }

    #[test]
    fn value_to_done_rejects_non_done_values() {
        assert!(value_to_done(&card_value(vec![])).is_none());
        assert!(value_to_done(&map_value(vec![("kind", s("read"))])).is_none());
        assert!(value_to_done(&s("plain")).is_none());
    }
}
