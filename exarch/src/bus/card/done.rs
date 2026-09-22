//! How a detached worker's completion reads.
//!
//! Core flushes a single `` `done `` value at the end of a background block's
//! deferred buffer; [`value_to_done`] decodes it, [`settled_spans`] words it.

use ral_core::serial::FOValue;

use super::value::{int_field, record, str_field};
use super::{Role, Span};
use crate::record::DoneOutcome;

/// Decode a `` `done `` value into the worker's `cmd` and how it settled,
/// `None` for anything else — and since `decode_surface` in `shell_eval.rs`
/// tries this branch last, `None` drops the value rather than passing it on.
pub(crate) fn value_to_done(v: &FOValue) -> Option<(String, DoneOutcome)> {
    let FOValue::Variant { label, payload } = v else {
        return None;
    };
    if label != "done" {
        return None;
    }
    let m = record(payload.as_deref()?)?;
    let FOValue::Variant { label, payload } = m.field("outcome")? else {
        return None;
    };
    let outcome = match label.as_str() {
        "ok" => DoneOutcome::Ok,
        "err" => {
            let rec = record(payload.as_deref()?)?;
            DoneOutcome::Err {
                message: str_field(rec, "message").unwrap_or_default(),
                status: int_field(rec, "status").unwrap_or(0),
            }
        }
        "panic" => DoneOutcome::Panic {
            message: payload
                .as_deref()
                .and_then(FOValue::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        _ => return None,
    };
    Some((str_field(m, "cmd")?, outcome))
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
    use super::super::testkit::{card_value, int, map_value, s, variant};
    use super::*;

    /// Mirrors core's `done_event`: `cmd` plus a closed outcome variant.
    fn done_value(cmd: &str, outcome: FOValue) -> FOValue {
        variant(
            "done",
            map_value(vec![("cmd", s(cmd)), ("outcome", outcome)]),
        )
    }

    #[test]
    fn value_to_done_decodes_each_outcome() {
        let named = |outcome| Some(("block at turn 1, line 1".to_string(), outcome));
        assert_eq!(
            value_to_done(&done_value(
                "block at turn 1, line 1",
                variant("ok", FOValue::Unit)
            )),
            named(DoneOutcome::Ok)
        );
        let err = variant(
            "err",
            map_value(vec![
                ("cmd", s("<runtime>")),
                ("status", int(2)),
                ("message", s("boom")),
                (
                    "site",
                    FOValue::Variant {
                        label: "none".into(),
                        payload: None,
                    },
                ),
            ]),
        );
        assert_eq!(
            value_to_done(&done_value("block at turn 1, line 1", err)),
            named(DoneOutcome::Err {
                message: "boom".into(),
                status: 2,
            })
        );
        assert_eq!(
            value_to_done(&done_value(
                "block at turn 1, line 1",
                variant("panic", s("kaput"))
            )),
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
